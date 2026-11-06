//! The bounded projection of the `create_workflow` display graph (a pure function on the Rust side).
//!
//! This file is a line-by-line port of
//! `apps/nc-code-cli/packages/core/src/tool/handlers/create-workflow-graph-bounds.ts`
//! (`boundCausalityGraph` / `boundGraphText` / `boundNamePattern`), `create-workflow-graph-fold.ts` in the same directory
//! (`foldPhaseEdges` and its contraction reduction) and
//! the `boundGraphOfAnalysis` entry condition of `workflow-analysis-display.ts`.
//!
//! The input is the JSON of the compile kernel `analyze` (the `causality` / `flow` / `handoff` projections),
//! the output is the JSON shape of the contract `CreateWorkflowCausalityGraph`. The trimming order matches TS exactly:
//! first steps (truncate in source order), then converge onto the lanes they reference, then filter participants/handoffs/edges by the surviving steps,
//! and last the phase vocabulary (all-or-nothing) with its phase-edge folding. Referential integrity outranks how much is retained.
//!
//! Pure function: no I/O, no clock, no randomness. String ceilings are counted in **UTF-16 code units**, see
//! [`bound_graph_text`]; Rust's `String`/JSON parser cannot carry a lone surrogate, so a cut in the middle of a surrogate
//! pair drops one code unit — the same outcome as TS discarding half a character.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

// Consistent with the CREATE_WORKFLOW_GRAPH_MAX_* of `packages/contracts/src/tools/create-workflow.ts`.
const MAX_STEPS: usize = 64;
const MAX_LANES: usize = 32;
const MAX_PARTICIPANTS: usize = 64;
const MAX_HANDOFFS: usize = 256;
const MAX_HANDOFF_TYPES: usize = 8;
const MAX_PHASES: usize = 32;
const MAX_PHASE_EDGES: usize = 128;
const MAX_ID_CHARS: usize = 64;
const MAX_NAME_CHARS: usize = 128;

// The reserved phase id and terminal node ids in the `dynamic-workflow` analysis projection.
const UNPHASED: &str = "unphased";
const FLOW_ENTRY: &str = "entry";
const FLOW_SINK: &str = "sink";
const FLOW_ABORT: &str = "abort";

/// The Rust form of `boundGraphOfAnalysis`: returns `None` when `analysis.causality` is absent,
/// otherwise the bounded display graph in the contract shape. An absent `flow` / `handoff` is handled as TS's `undefined`.
pub fn bound_causality_graph(analysis: &Value) -> Option<Value> {
    let graph = analysis.get("causality").filter(|value| !value.is_null())?;
    let flow = analysis.get("flow").filter(|value| !value.is_null());
    let handoff = analysis.get("handoff").filter(|value| !value.is_null());
    Some(bound_graph(graph, flow, handoff))
}

fn bound_graph(graph: &Value, flow: Option<&Value>, handoff: Option<&Value>) -> Value {
    let all_steps = value_array(graph, "steps");
    let mut truncated = all_steps.len() > MAX_STEPS;
    let head_steps = &all_steps[..all_steps.len().min(MAX_STEPS)];

    // 1. Lanes: only the ones the surviving steps stand in, in the graph's lane order
    //    (workspace first, actors in creation order).
    let mut wanted_lanes: HashSet<&str> = HashSet::new();
    for step in head_steps {
        wanted_lanes.insert(value_str(step, "lane"));
        for lane in value_array(step, "lanes") {
            if let Some(lane) = lane.as_str() {
                wanted_lanes.insert(lane);
            }
        }
    }
    let lane_list: Vec<&Value> = value_array(graph, "lanes")
        .iter()
        .filter(|lane| wanted_lanes.contains(value_str(lane, "id")))
        .collect();
    truncated |= lane_list.len() > MAX_LANES;
    let kept_lanes: Vec<&Value> = lane_list.iter().take(MAX_LANES).copied().collect();
    let lane_ids: HashSet<&str> = kept_lanes
        .iter()
        .map(|lane| value_str(lane, "id"))
        .collect();

    // 2. A step whose own lane got dropped has nowhere to sit; a may-set narrows instead.
    let steps: Vec<&Value> = head_steps
        .iter()
        .filter(|step| lane_ids.contains(value_str(step, "lane")))
        .collect();
    truncated |= steps.len() < head_steps.len();
    let step_ids: HashSet<&str> = steps.iter().map(|step| value_str(step, "id")).collect();

    // 3. Participants and hand-offs. A card's steps narrow to the surviving ones and a
    //    card left with none goes; a dropped card's steps stay in `steps`.
    let handoff_participants = handoff
        .map(|handoff| value_array(handoff, "participants"))
        .unwrap_or(&[]);
    let mut participant_list: Vec<Value> = Vec::new();
    for participant in handoff_participants {
        if !lane_ids.contains(value_str(participant, "lane")) {
            continue;
        }
        let member_steps: Vec<&Value> = value_array(participant, "steps")
            .iter()
            .filter(|id| id.as_str().is_some_and(|id| step_ids.contains(id)))
            .collect();
        if member_steps.is_empty() {
            continue;
        }
        let mut card = Map::new();
        card.insert(
            "id".to_string(),
            Value::String(bound_graph_text(value_str(participant, "id"), MAX_ID_CHARS)),
        );
        card.insert(
            "phase".to_string(),
            Value::String(bound_graph_text(
                value_str(participant, "phase"),
                MAX_ID_CHARS,
            )),
        );
        card.insert(
            "lane".to_string(),
            Value::String(bound_graph_text(
                value_str(participant, "lane"),
                MAX_ID_CHARS,
            )),
        );
        let bounded_steps: Vec<Value> = member_steps
            .iter()
            .take(MAX_STEPS)
            .map(|id| {
                Value::String(bound_graph_text(
                    id.as_str().unwrap_or_default(),
                    MAX_ID_CHARS,
                ))
            })
            .collect();
        card.insert("steps".to_string(), Value::Array(bounded_steps));
        if let Some(member) = participant.get("member").and_then(Value::as_object) {
            card.insert("member".to_string(), Value::Object(member.clone()));
        }
        if participant.get("many").and_then(Value::as_bool) == Some(true) {
            card.insert("many".to_string(), Value::Bool(true));
        }
        participant_list.push(Value::Object(card));
    }
    truncated |= participant_list.len() < handoff_participants.len();
    truncated |= participant_list.len() > MAX_PARTICIPANTS;
    let participants: Vec<&Value> = participant_list.iter().take(MAX_PARTICIPANTS).collect();
    let participant_ids: HashSet<&str> = participants
        .iter()
        .map(|participant| value_str(participant, "id"))
        .collect();

    let handoff_edges = handoff
        .map(|handoff| value_array(handoff, "handoffs"))
        .unwrap_or(&[]);
    let mut handoff_list: Vec<Value> = Vec::new();
    for edge in handoff_edges {
        // TS filters the raw edges with the **already truncated** participant ids: a card that collides on a name after truncation is still judged dropped by its original id.
        let from = value_str(edge, "from");
        let to = value_str(edge, "to");
        if !participant_ids.contains(from) || !participant_ids.contains(to) {
            continue;
        }
        let types: Vec<Value> = value_array(edge, "types")
            .iter()
            .take(MAX_HANDOFF_TYPES)
            .map(|kind| bound_graph_text(kind.as_str().unwrap_or_default(), MAX_NAME_CHARS))
            .filter(|kind| !kind.is_empty())
            .map(Value::String)
            .collect();
        let mut out = Map::new();
        out.insert(
            "from".to_string(),
            Value::String(bound_graph_text(from, MAX_ID_CHARS)),
        );
        out.insert(
            "to".to_string(),
            Value::String(bound_graph_text(to, MAX_ID_CHARS)),
        );
        if edge.get("back").and_then(Value::as_bool) == Some(true) {
            out.insert("back".to_string(), Value::Bool(true));
        }
        if !types.is_empty() {
            out.insert("types".to_string(), Value::Array(types));
        }
        handoff_list.push(Value::Object(out));
    }
    truncated |= handoff_list.len() < handoff_edges.len();
    truncated |= handoff_list.len() > MAX_HANDOFFS;
    let handoffs: Vec<Value> = handoff_list.iter().take(MAX_HANDOFFS).cloned().collect();

    let sink: Vec<Value> = graph
        .get("sink")
        .and_then(|sink| sink.get("fedBy"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|id| id.as_str().is_some_and(|id| step_ids.contains(id)))
        .take(MAX_STEPS)
        .map(|id| {
            Value::String(bound_graph_text(
                id.as_str().unwrap_or_default(),
                MAX_ID_CHARS,
            ))
        })
        .collect();

    // 4. Phases: the vocabulary is all-or-nothing and comes from the CONTROL-FLOW
    //    projection. Edges touching `entry` / `abort` have no node to land on; edges into
    //    `sink` become `exits`; self-loops say nothing at quotient granularity.
    let declared_phases = flow
        .and_then(|flow| flow.get("phases"))
        .and_then(Value::as_array);
    let phase_ids: HashSet<&str> = declared_phases
        .map(|phases| phases.iter().map(|phase| value_str(phase, "id")).collect())
        .unwrap_or_default();
    let mut raw_phase_edges: Vec<RawEdge> = Vec::new();
    let mut exit_set: HashSet<&str> = HashSet::new();
    for edge in flow
        .and_then(|flow| flow.get("phaseEdges"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let from = value_str(edge, "from");
        let to = value_str(edge, "to");
        if from == FLOW_ENTRY || from == FLOW_ABORT || to == FLOW_ABORT {
            continue;
        }
        if !phase_ids.contains(from) {
            continue;
        }
        if to == FLOW_SINK {
            exit_set.insert(from);
            continue;
        }
        if !phase_ids.contains(to) {
            continue;
        }
        raw_phase_edges.push(RawEdge {
            back: value_str(edge, "kind") == "loop",
            from: from.to_string(),
            to: to.to_string(),
        });
    }
    let folded_phase_edges = fold_phase_edges(&raw_phase_edges);
    let phase_edges: Vec<Value> = folded_phase_edges
        .iter()
        .map(|edge| {
            let mut out = Map::new();
            out.insert(
                "from".to_string(),
                Value::String(bound_graph_text(&edge.from, MAX_ID_CHARS)),
            );
            out.insert(
                "to".to_string(),
                Value::String(bound_graph_text(&edge.to, MAX_ID_CHARS)),
            );
            if edge.back {
                out.insert("back".to_string(), Value::Bool(true));
            }
            Value::Object(out)
        })
        .collect();
    // The edge ceiling governs what is actually emitted, so it is applied **after** the reduction.
    let phase_vocabulary_dropped = declared_phases
        .is_some_and(|phases| phases.len() > MAX_PHASES || phase_edges.len() > MAX_PHASE_EDGES);
    truncated |= phase_vocabulary_dropped;
    let emit_phases = declared_phases.is_some() && !phase_vocabulary_dropped;

    let bound_phases: Vec<Value> = declared_phases
        .map(|phases| {
            phases
                .iter()
                .map(|phase| bound_phase(phase, &phase_ids))
                .collect()
        })
        .unwrap_or_default();
    let exits: Vec<Value> = declared_phases
        .map(|phases| {
            phases
                .iter()
                .filter(|phase| exit_set.contains(value_str(phase, "id")))
                .map(|phase| Value::String(bound_graph_text(value_str(phase, "id"), MAX_ID_CHARS)))
                .collect()
        })
        .unwrap_or_default();

    let bound_lanes: Vec<Value> = kept_lanes.iter().map(|lane| bound_lane(lane)).collect();

    let bound_steps: Vec<Value> = steps
        .iter()
        .map(|step| bound_step(step, &lane_ids, &phase_ids, emit_phases))
        .collect();

    // A card's phase follows the same rule as a step's `phase`: vocabulary downgraded, or pointing at a phase that is not listed → folded into
    // `unphased`. The card id does not change — it is an opaque key.
    let bound_participants: Vec<Value> = participants
        .iter()
        .map(|participant| {
            let phase_ok = emit_phases
                && participant
                    .get("phase")
                    .and_then(Value::as_str)
                    .is_some_and(|phase| phase_ids.contains(phase));
            if phase_ok {
                (*participant).clone()
            } else {
                let mut card = participant.as_object().cloned().unwrap_or_default();
                card.insert("phase".to_string(), Value::String(UNPHASED.to_string()));
                Value::Object(card)
            }
        })
        .collect();

    let mut out = Map::new();
    out.insert("steps".to_string(), Value::Array(bound_steps));
    out.insert("lanes".to_string(), Value::Array(bound_lanes));
    out.insert("participants".to_string(), Value::Array(bound_participants));
    out.insert("handoffs".to_string(), Value::Array(handoffs));
    if emit_phases {
        out.insert("phases".to_string(), Value::Array(bound_phases));
        out.insert("phaseEdges".to_string(), Value::Array(phase_edges));
        out.insert("exits".to_string(), Value::Array(exits));
    }
    if !sink.is_empty() {
        out.insert("sink".to_string(), Value::Array(sink));
    }
    if truncated {
        out.insert("truncated".to_string(), Value::Bool(true));
    }
    Value::Object(out)
}

fn bound_lane(lane: &Value) -> Value {
    let mut out = Map::new();
    out.insert(
        "id".to_string(),
        Value::String(bound_graph_text(value_str(lane, "id"), MAX_ID_CHARS)),
    );
    // An empty name (such as agent("")) would violate the contract's min(1), so it is handled as "unnamed".
    if let Some(name) = lane.get("name").and_then(Value::as_str) {
        if !name.is_empty() {
            out.insert(
                "name".to_string(),
                Value::String(bound_graph_text(name, MAX_NAME_CHARS)),
            );
        }
    }
    if let Some(pattern) = bound_name_pattern(lane.get("namePattern")) {
        out.insert("namePattern".to_string(), pattern);
    }
    insert_loc(&mut out, lane);
    Value::Object(out)
}

fn bound_step(
    step: &Value,
    lane_ids: &HashSet<&str>,
    phase_ids: &HashSet<&str>,
    emit_phases: bool,
) -> Value {
    let id = value_str(step, "id");
    let mut out = Map::new();
    out.insert(
        "id".to_string(),
        Value::String(bound_graph_text(id, MAX_ID_CHARS)),
    );
    if let Some(kind) = step.get("kind") {
        out.insert("kind".to_string(), kind.clone());
    }
    // An ask's label comes from a script literal and may be empty; the contract requires min(1), so fall back to the step id.
    let label = match step.get("label").and_then(Value::as_str) {
        Some(label) if !label.is_empty() => bound_graph_text(label, MAX_NAME_CHARS),
        _ => id.to_string(),
    };
    out.insert("label".to_string(), Value::String(label));
    if let Some(pattern) = bound_name_pattern(step.get("labelPattern")) {
        out.insert("labelPattern".to_string(), pattern);
    }
    insert_loc(&mut out, step);
    out.insert(
        "lane".to_string(),
        Value::String(bound_graph_text(value_str(step, "lane"), MAX_ID_CHARS)),
    );
    let lanes: Vec<Value> = value_array(step, "lanes")
        .iter()
        .filter(|lane| lane.as_str().is_some_and(|lane| lane_ids.contains(lane)))
        .cloned()
        .collect();
    if lanes.len() > 1 {
        out.insert("lanes".to_string(), Value::Array(lanes));
    }
    if let Some(source) = step.get("source").and_then(Value::as_str) {
        out.insert(
            "source".to_string(),
            Value::String(bound_graph_text(source, MAX_ID_CHARS)),
        );
    }
    // When the whole vocabulary is downgraded, `phase` must disappear with it; a `phase` pointing at a phase that is not listed disappears the same way.
    if emit_phases {
        if let Some(phase) = step.get("phase").and_then(Value::as_str) {
            if phase_ids.contains(phase) {
                out.insert(
                    "phase".to_string(),
                    Value::String(bound_graph_text(phase, MAX_ID_CHARS)),
                );
            }
        }
    }
    if let Some(repeat) = step.get("repeat") {
        out.insert("repeat".to_string(), repeat.clone());
    }
    Value::Object(out)
}

fn bound_phase(phase: &Value, phase_ids: &HashSet<&str>) -> Value {
    let id = value_str(phase, "id");
    let mut out = Map::new();
    out.insert(
        "id".to_string(),
        Value::String(bound_graph_text(id, MAX_ID_CHARS)),
    );
    // The synthetic phase `unphased` has no name; an empty name likewise counts as "unnamed".
    if let Some(name) = phase.get("name").and_then(Value::as_str) {
        if !name.is_empty() {
            out.insert(
                "name".to_string(),
                Value::String(bound_graph_text(name, MAX_NAME_CHARS)),
            );
        }
    }
    insert_loc(&mut out, phase);
    // `alongside`: the other phases still running when this phase is entered. Pointing at a phase not listed, or a self-reference, is dropped;
    // deduplicated keeping order, with the same ceiling as the phase table.
    let mut alongside: Vec<Value> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for other in value_array(phase, "alongside") {
        let Some(other) = other.as_str() else {
            continue;
        };
        if other == id || !phase_ids.contains(other) || !seen.insert(other) {
            continue;
        }
        if alongside.len() >= MAX_PHASES {
            break;
        }
        alongside.push(Value::String(bound_graph_text(other, MAX_ID_CHARS)));
    }
    if !alongside.is_empty() {
        out.insert("alongside".to_string(), Value::Array(alongside));
    }
    Value::Object(out)
}

/// `boundNamePattern`: both affixes go through the same surrogate-safe truncation; when truncation leaves nothing at all the whole
/// field is absent, instead of emitting an empty object.
fn bound_name_pattern(pattern: Option<&Value>) -> Option<Value> {
    let pattern = pattern?;
    let head = match pattern.get("head").and_then(Value::as_str) {
        Some(head) if !head.is_empty() => bound_graph_text(head, MAX_NAME_CHARS),
        _ => String::new(),
    };
    let tail = match pattern.get("tail").and_then(Value::as_str) {
        Some(tail) if !tail.is_empty() => bound_graph_text(tail, MAX_NAME_CHARS),
        _ => String::new(),
    };
    if head.is_empty() && tail.is_empty() {
        return None;
    }
    let mut out = Map::new();
    if !head.is_empty() {
        out.insert("head".to_string(), Value::String(head));
    }
    if !tail.is_empty() {
        out.insert("tail".to_string(), Value::String(tail));
    }
    Some(Value::Object(out))
}

/// JS `String.prototype.slice(0, maxChars)` plus dropping a trailing lone high surrogate.
///
/// `maxChars` is a number of UTF-16 code units. Rust accumulates by char, and a char straddling the boundary (a surrogate pair)
/// never enters the result at all — exactly JS slicing out the high surrogate and then dropping it via `0xd800..=0xdbff`.
fn bound_graph_text(value: &str, max_chars: usize) -> String {
    let mut bounded = String::new();
    let mut used = 0usize;
    for character in value.chars() {
        let width = character.len_utf16();
        if used + width > max_chars {
            break;
        }
        used += width;
        bounded.push(character);
    }
    bounded
}

fn insert_loc(out: &mut Map<String, Value>, value: &Value) {
    let Some(loc) = value.get("loc").and_then(Value::as_object) else {
        return;
    };
    if let Some(line) = loc.get("line") {
        out.insert("line".to_string(), line.clone());
    }
    if let Some(column) = loc.get("column") {
        out.insert("column".to_string(), column.clone());
    }
}

fn value_array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn value_str<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

// ── Phase-edge folding (create-workflow-graph-fold.ts) ─────────────────────────────

/// One edge before folding: the kind has already collapsed into "is this a back edge or not".
#[derive(Clone, Debug)]
struct RawEdge {
    from: String,
    to: String,
    back: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
// The full `OrderKind` type lattice is kept: `foldPhaseEdges` only ever feeds `seq` forward edges and `carryOf: "seq"`,
// but `reduceOrdering`'s `JUSTIFIED_BY` table is ported as it is, and `data` / `control` / `fifo` are part of that
// table rather than inputs of this module.
#[allow(dead_code)]
enum OrderKind {
    Data,
    Control,
    Fifo,
    Seq,
    Carry,
}

/// The input edge of the reduction. `carry_of` is the underlying kind of a carry edge; this module only produces `seq` forward edges and
/// back edges with `carryOf: "seq"`, but the general fields are kept so the code corresponds line-by-line to `reduceOrdering`.
#[derive(Clone)]
struct ReducibleEdge {
    from: String,
    to: String,
    kind: OrderKind,
    carry_of: Option<OrderKind>,
}

/// The same ordered pair folds into one entry (first-seen order), `back` is the conjunction of the folded members; self loops are dropped.
fn fold_pairs(raw: &[RawEdge]) -> Vec<RawEdge> {
    let mut order: Vec<String> = Vec::new();
    let mut by_key: HashMap<String, RawEdge> = HashMap::new();
    for edge in raw {
        if edge.from == edge.to {
            continue;
        }
        let key = format!("{} {}", edge.from, edge.to);
        match by_key.get_mut(&key) {
            Some(seen) => seen.back = seen.back && edge.back,
            None => {
                order.push(key.clone());
                by_key.insert(key, edge.clone());
            }
        }
    }
    order
        .iter()
        .map(|key| by_key.get(key).expect("folded edge").clone())
        .collect()
}

/// Each node → the representative of the strongly connected component it belongs to (the first node appearing in the component). Only forward edges are considered: a back edge says
/// "next round", and counting it into the component would contract the whole loop body into a single point.
fn components_of(folded: &[RawEdge]) -> HashMap<String, String> {
    let mut nodes: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut next: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in folded {
        for id in [edge.from.as_str(), edge.to.as_str()] {
            if seen.insert(id) {
                nodes.push(id);
            }
        }
        if edge.back {
            continue;
        }
        next.entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
    }

    let mut reach: HashMap<&str, HashSet<&str>> = HashMap::new();
    for &start in &nodes {
        let mut reached: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = next.get(start).cloned().unwrap_or_default();
        while let Some(node) = stack.pop() {
            if !reached.insert(node) {
                continue;
            }
            if let Some(neighbors) = next.get(node) {
                stack.extend(neighbors.iter().copied());
            }
        }
        reach.insert(start, reached);
    }

    let mut component_of: HashMap<String, String> = HashMap::new();
    for &node in &nodes {
        if component_of.contains_key(node) {
            continue;
        }
        component_of.insert(node.to_string(), node.to_string());
        let Some(reached) = reach.get(node) else {
            continue;
        };
        for &other in reached {
            if component_of.contains_key(other) {
                continue;
            }
            if reach.get(other).is_some_and(|back| back.contains(node)) {
                component_of.insert(other.to_string(), node.to_string());
            }
        }
    }
    component_of
}

fn component<'a>(component_of: &'a HashMap<String, String>, id: &'a str) -> &'a str {
    component_of.get(id).map(String::as_str).unwrap_or(id)
}

fn edge_key(from: &str, to: &str, back: bool) -> String {
    format!("{from} {to} {back}")
}

/// The folding and reduction of phase edges: fold the same pair → contract the forward edges → dedupe cross-component edges by (component pair, kind) →
/// run `reduceOrdering` on the contracted DAG → expand back into the original edges by surviving component pairs. The output keeps input order.
fn fold_phase_edges(raw: &[RawEdge]) -> Vec<RawEdge> {
    let folded = fold_pairs(raw);
    let component_of = components_of(&folded);

    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut condensed: Vec<ReducibleEdge> = Vec::new();
    for edge in &folded {
        let from = component(&component_of, &edge.from);
        let to = component(&component_of, &edge.to);
        if from == to {
            continue;
        }
        let key = edge_key(from, to, edge.back);
        if !seen_keys.insert(key) {
            continue;
        }
        condensed.push(if edge.back {
            ReducibleEdge {
                from: from.to_string(),
                to: to.to_string(),
                kind: OrderKind::Carry,
                carry_of: Some(OrderKind::Seq),
            }
        } else {
            ReducibleEdge {
                from: from.to_string(),
                to: to.to_string(),
                kind: OrderKind::Seq,
                carry_of: None,
            }
        });
    }

    let kept: HashSet<String> = reduce_ordering(&condensed)
        .iter()
        .map(|edge| edge_key(&edge.from, &edge.to, edge.kind == OrderKind::Carry))
        .collect();
    folded
        .into_iter()
        .filter(|edge| {
            let from = component(&component_of, &edge.from);
            let to = component(&component_of, &edge.to);
            from == to || kept.contains(&edge_key(from, to, edge.back))
        })
        .collect()
}

/// `reduceOrdering` of `causality-reduce.ts`: first the forward edges, deciding implication edge by edge against the surviving set, then carry edges
/// judged redundant by a "forward* → exactly one carry hop → forward*" witness. This module only feeds `seq` / `carryOf: seq`.
fn reduce_ordering(edges: &[ReducibleEdge]) -> Vec<ReducibleEdge> {
    let mut dropped = vec![false; edges.len()];
    let forward: Vec<usize> = edges
        .iter()
        .enumerate()
        .filter(|(_, edge)| edge.kind != OrderKind::Carry)
        .map(|(index, _)| index)
        .collect();
    let carries: Vec<usize> = edges
        .iter()
        .enumerate()
        .filter(|(_, edge)| edge.kind == OrderKind::Carry)
        .map(|(index, _)| index)
        .collect();

    let mut outgoing: HashMap<&str, Vec<usize>> = HashMap::new();
    for &index in &forward {
        outgoing
            .entry(edges[index].from.as_str())
            .or_default()
            .push(index);
    }
    for &index in &forward {
        let edge = &edges[index];
        let Some(allowed) = justified_by(edge.kind) else {
            continue;
        };
        if edge.from == edge.to {
            continue;
        }
        if reaches(edges, &outgoing, &dropped, &edge.from, &edge.to, allowed) {
            dropped[index] = true;
        }
    }

    let mut carry_outgoing: HashMap<&str, Vec<usize>> = HashMap::new();
    for &index in &carries {
        carry_outgoing
            .entry(edges[index].from.as_str())
            .or_default()
            .push(index);
    }
    for &index in &carries {
        let underlying = edges[index].carry_of.unwrap_or(OrderKind::Data);
        let Some(allowed) = justified_by(underlying) else {
            continue;
        };
        if carry_witness(edges, &outgoing, &carry_outgoing, &dropped, index, allowed) {
            dropped[index] = true;
        }
    }

    edges
        .iter()
        .enumerate()
        .filter(|(index, _)| !dropped[*index])
        .map(|(_, edge)| edge.clone())
        .collect()
}

#[derive(Clone, Copy)]
enum AllowedKinds {
    Hard,
    Fifo,
    Seq,
}

/// `JUSTIFIED_BY`: which kinds of path can witness a given edge.
fn justified_by(kind: OrderKind) -> Option<AllowedKinds> {
    match kind {
        OrderKind::Data | OrderKind::Control => Some(AllowedKinds::Hard),
        OrderKind::Fifo => Some(AllowedKinds::Fifo),
        OrderKind::Seq => Some(AllowedKinds::Seq),
        OrderKind::Carry => None,
    }
}

fn allowed_contains(allowed: AllowedKinds, kind: OrderKind) -> bool {
    match allowed {
        AllowedKinds::Hard => matches!(kind, OrderKind::Data | OrderKind::Control),
        AllowedKinds::Fifo => {
            matches!(kind, OrderKind::Data | OrderKind::Control | OrderKind::Fifo)
        }
        AllowedKinds::Seq => matches!(
            kind,
            OrderKind::Data | OrderKind::Control | OrderKind::Fifo | OrderKind::Seq
        ),
    }
}

/// Whether `to` is reachable from `from` via surviving forward edges of an allowed kind (length at least 2).
fn reaches(
    edges: &[ReducibleEdge],
    outgoing: &HashMap<&str, Vec<usize>>,
    dropped: &[bool],
    from: &str,
    to: &str,
    allowed: AllowedKinds,
) -> bool {
    let mut seen: HashSet<String> = HashSet::from([from.to_string()]);
    let mut stack: Vec<String> = Vec::new();
    for &index in outgoing.get(from).map(Vec::as_slice).unwrap_or(&[]) {
        let edge = &edges[index];
        if edge.to == to || !allowed_contains(allowed, edge.kind) || dropped[index] {
            continue;
        }
        if seen.insert(edge.to.clone()) {
            stack.push(edge.to.clone());
        }
    }
    while let Some(node) = stack.pop() {
        for &index in outgoing
            .get(node.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let edge = &edges[index];
            if !allowed_contains(allowed, edge.kind) || dropped[index] {
                continue;
            }
            if edge.to == to {
                return true;
            }
            if seen.insert(edge.to.clone()) {
                stack.push(edge.to.clone());
            }
        }
    }
    false
}

/// A "forward* → exactly one carry hop → forward*" witness (the carry hop judged by its underlying kind),
/// which does not use the candidate itself.
fn carry_witness(
    edges: &[ReducibleEdge],
    outgoing: &HashMap<&str, Vec<usize>>,
    carry_outgoing: &HashMap<&str, Vec<usize>>,
    dropped: &[bool],
    candidate: usize,
    allowed: AllowedKinds,
) -> bool {
    let from = edges[candidate].from.clone();
    let to = edges[candidate].to.clone();
    let mut seen: HashSet<(String, u8)> = HashSet::from([(from.clone(), 0)]);
    let mut stack: Vec<(String, u8)> = vec![(from, 0)];
    while let Some((node, spent)) = stack.pop() {
        for &index in outgoing
            .get(node.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let edge = &edges[index];
            if !allowed_contains(allowed, edge.kind) || dropped[index] {
                continue;
            }
            if spent == 1 && edge.to == to {
                return true;
            }
            if seen.insert((edge.to.clone(), spent)) {
                stack.push((edge.to.clone(), spent));
            }
        }
        if spent == 1 {
            continue;
        }
        for &index in carry_outgoing
            .get(node.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if index == candidate || dropped[index] {
                continue;
            }
            let underlying = edges[index].carry_of.unwrap_or(OrderKind::Data);
            if !allowed_contains(allowed, underlying) {
                continue;
            }
            if edges[index].to == to {
                return true;
            }
            if seen.insert((edges[index].to.clone(), 1)) {
                stack.push((edges[index].to.clone(), 1));
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bound_text_counts_utf16_units() {
        assert_eq!(bound_graph_text("abcdef", 3), "abc");
        assert_eq!(bound_graph_text("abcdef", 6), "abcdef");
        assert_eq!(bound_graph_text("abcdef", 0), "");
        assert_eq!(bound_graph_text("", 128), "");
        // A surrogate pair is 2 code units: when the ceiling cuts inside it, the whole character disappears (TS drops half a character).
        assert_eq!(bound_graph_text("ab\u{1F600}", 3), "ab");
        assert_eq!(bound_graph_text("ab\u{1F600}", 4), "ab\u{1F600}");
        assert_eq!(bound_graph_text("\u{1F600}", 1), "");
        assert_eq!(bound_graph_text("\u{1F600}\u{1F600}", 3), "\u{1F600}");
        // BMP characters (CJK included) each count as 1 code unit.
        assert_eq!(bound_graph_text("Researcher Researcher", 3), "Res");
        assert_eq!(bound_graph_text(&"a".repeat(129), 128), "a".repeat(128));
    }

    #[test]
    fn name_pattern_drops_when_both_affixes_empty() {
        assert_eq!(bound_name_pattern(None), None);
        assert_eq!(bound_name_pattern(Some(&json!({}))), None);
        assert_eq!(
            bound_name_pattern(Some(&json!({"head": "", "tail": ""}))),
            None
        );
        assert_eq!(
            bound_name_pattern(Some(&json!({"head": "abc", "tail": ""}))),
            Some(json!({"head": "abc"}))
        );
        assert_eq!(
            bound_name_pattern(Some(&json!({"head": "", "tail": "xyz"}))),
            Some(json!({"tail": "xyz"}))
        );
        assert_eq!(
            bound_name_pattern(Some(&json!({"head": "\u{1F600}", "tail": "t"}))),
            Some(json!({"head": "\u{1F600}", "tail": "t"})),
            "Each affix is individually capped at 128 symbols; individual proxy pairs remain intact"
        );
        let long = format!("{}\u{1F600}", "h".repeat(127));
        assert_eq!(
            bound_name_pattern(Some(&json!({"head": long}))),
            Some(json!({"head": "h".repeat(127)}))
        );
    }

    /// The cap is the display contract's, and it has to apply to the native projection too: an NWS
    /// script with more awaits than the card can draw arrives bounded and flagged, exactly like a
    /// TypeScript one.
    #[test]
    fn an_nws_graph_is_capped_by_the_same_rules_as_a_typescript_one() {
        let mut script = String::from(
            "workflow \"wide\" {
  let a = agent(\"a\");
",
        );
        for round in 1..=70 {
            script.push_str(&format!(
                " let x{round} = await a.ask(\"question {round}\");
"
            ));
        }
        script.push_str(
            "}
",
        );
        let plan = nc_code_workflow::script::plan::Plan::compile(&script).expect("compiles");
        let analysis = nc_code_workflow::script::graph::project_analysis(&plan);
        assert_eq!(
            analysis["causality"]["steps"]
                .as_array()
                .expect("steps")
                .len(),
            70,
            "the projection describes the script; bounding happens here"
        );
        let graph = bound_causality_graph(&analysis).expect("graph");
        assert_eq!(
            graph["steps"].as_array().expect("steps").len(),
            MAX_STEPS,
            "the same step cap, in source order"
        );
        assert_eq!(graph["truncated"], json!(true));
        assert_eq!(graph["steps"][0]["id"], json!("ask#1"));
        // The lane convergence is visible here: this script never reads the workspace, so the
        // workspace lane has no surviving step standing in it and leaves the payload — the same rule
        // the TypeScript path is tested against.
        let lanes = graph["lanes"].as_array().expect("lanes");
        assert_eq!(lanes.len(), 1, "{lanes:?}");
        assert_eq!(lanes[0]["id"], json!("actor#1"));
    }

    /// A script that declares phases must land the same phase vocabulary on both surfaces, or the
    /// preview card and the sidebar track disagree about what stage a step belongs to.
    #[test]
    fn an_nws_graph_keeps_its_phase_vocabulary_whole() {
        let plan = nc_code_workflow::script::plan::Plan::compile(
            r#"workflow "phased" {
  let a = agent("a");
  phase "plan";
  let x = await a.ask("plan it");
  phase "check";
  let y = await a.ask("check it");
  return y;
}
"#,
        )
        .expect("compiles");
        let graph =
            bound_causality_graph(&nc_code_workflow::script::graph::project_analysis(&plan))
                .expect("graph");
        let names: Vec<&str> = graph["phases"]
            .as_array()
            .expect("phases")
            .iter()
            .map(|phase| phase["name"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["plan", "check"]);
        assert_eq!(graph["steps"][0]["phase"], json!("phase#1"));
        assert_eq!(graph["steps"][1]["phase"], json!("phase#2"));
        // `entry` and `abort` have no node to land on and `sink` becomes the `exits` array: those are
        // the bounding pass's own rules, and the NWS quotient has to satisfy them rather than
        // restate them.
        let edges: Vec<String> = graph["phaseEdges"]
            .as_array()
            .expect("phase edges")
            .iter()
            .map(|edge| {
                format!(
                    "{}>{}",
                    edge["from"].as_str().unwrap_or_default(),
                    edge["to"].as_str().unwrap_or_default()
                )
            })
            .collect();
        assert_eq!(edges, vec!["phase#1>phase#2".to_string()], "{edges:?}");
        assert_eq!(
            graph["exits"],
            json!(["phase#2"]),
            "the return is an exit, not an edge"
        );
    }

    #[test]
    fn absent_causality_is_none() {
        assert_eq!(bound_causality_graph(&json!({"ok": true})), None);
        assert_eq!(
            bound_causality_graph(&json!({"ok": false, "diagnostics": []})),
            None
        );
        assert_eq!(bound_causality_graph(&json!({"causality": null})), None);
    }

    #[test]
    fn empty_graph_emits_only_required_arrays() {
        let graph = json!({
            "steps": [],
            "lanes": [],
            "regions": [],
            "edges": [],
        });
        assert_eq!(
            bound_causality_graph(&json!({"causality": graph})),
            Some(json!({
                "steps": [],
                "lanes": [],
                "participants": [],
                "handoffs": [],
            }))
        );
    }

    fn step(id: &str, lane: &str) -> Value {
        json!({
            "id": id,
            "kind": "ask",
            "label": "a",
            "loc": {"line": 1, "column": 2},
            "lane": lane,
            "region": "root",
            "certainty": "always",
        })
    }

    #[test]
    fn steps_truncate_in_source_order_and_flag_truncation() {
        let steps: Vec<Value> = (1..=65)
            .map(|index| step(&format!("ask#{index}"), "actor#1"))
            .collect();
        let graph = json!({
            "steps": steps,
            "lanes": [{"id": "actor#1"}],
            "edges": [],
        });
        let out = bound_causality_graph(&json!({"causality": graph})).expect("graph");
        let out_steps = out["steps"].as_array().expect("steps");
        assert_eq!(out_steps.len(), MAX_STEPS);
        assert_eq!(out_steps[0]["id"], json!("ask#1"));
        assert_eq!(out_steps[63]["id"], json!("ask#64"));
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(out["lanes"].as_array().expect("lanes").len(), 1);
    }

    #[test]
    fn dropped_lane_drops_its_steps_and_cards() {
        let mut steps = Vec::new();
        let mut lanes = Vec::new();
        for index in 1..=33 {
            let lane = format!("actor#{index}");
            lanes.push(json!({"id": lane}));
            steps.push(step(&format!("ask#{index}"), &format!("actor#{index}")));
        }
        let graph = json!({
            "steps": steps,
            "lanes": lanes,
            "edges": [],
            "sink": {"fedBy": ["ask#33"]},
        });
        let handoff = json!({
            "participants": [
                {"id": "unphased:actor#33", "phase": "unphased", "lane": "actor#33", "steps": ["ask#33"]},
                {"id": "unphased:actor#1", "phase": "unphased", "lane": "actor#1", "steps": ["ask#1"]},
            ],
            "handoffs": [{"from": "unphased:actor#33", "to": "unphased:actor#1"}],
        });
        let out =
            bound_causality_graph(&json!({"causality": graph, "handoff": handoff})).expect("graph");
        assert_eq!(out["lanes"].as_array().expect("lanes").len(), MAX_LANES);
        assert_eq!(out["steps"].as_array().expect("steps").len(), MAX_LANES);
        assert_eq!(
            out["participants"],
            json!([{"id": "unphased:actor#1", "phase": "unphased", "lane": "actor#1", "steps": ["ask#1"]}])
        );
        assert_eq!(out["handoffs"], json!([]));
        assert!(
            out.get("sink").is_none(),
            "a sink whose only feeder was truncated disappears"
        );
        assert_eq!(out["truncated"], json!(true));
    }

    #[test]
    fn may_set_lanes_narrow_and_source_is_kept() {
        let graph = json!({
            "steps": [{
                "id": "ask#1~actor#1",
                "kind": "ask",
                "label": "chosen",
                "loc": {"line": 5, "column": 14},
                "lane": "actor#1",
                "lanes": ["actor#1", "actor#2", "actor#9"],
                "source": "ask#1",
                "region": "root",
                "certainty": "maybe",
            }],
            "lanes": [{"id": "actor#1"}, {"id": "actor#2"}],
            "edges": [],
        });
        let out = bound_causality_graph(&json!({"causality": graph})).expect("graph");
        assert_eq!(
            out["steps"][0]["lanes"],
            json!(["actor#1", "actor#2"]),
            "the dropped lane leaves the may-set, and two survive so the field stays"
        );
        assert_eq!(out["steps"][0]["source"], json!("ask#1"));
    }

    #[test]
    fn empty_label_falls_back_to_raw_step_id() {
        let graph = json!({
            "steps": [{
                "id": "ask#1",
                "kind": "ask",
                "label": "",
                "loc": {"line": 1, "column": 1},
                "lane": "actor#1",
                "region": "root",
                "certainty": "always",
            }],
            "lanes": [{"id": "actor#1", "name": ""}, {"id": "actor#2", "name": "unused"}],
            "edges": [],
        });
        let out = bound_causality_graph(&json!({"causality": graph})).expect("graph");
        assert_eq!(out["steps"][0]["label"], json!("ask#1"));
        assert_eq!(
            out["lanes"],
            json!([{"id": "actor#1"}]),
            "a lane no surviving step stands in leaves the payload"
        );
    }

    #[test]
    fn may_set_lane_list_disappears_when_one_candidate_survives() {
        let graph = json!({
            "steps": [{
                "id": "ask#1~actor#1",
                "kind": "ask",
                "label": "chosen",
                "loc": {"line": 5, "column": 14},
                "lane": "actor#1",
                "lanes": ["actor#1", "actor#9"],
                "source": "ask#1",
                "region": "root",
                "certainty": "maybe",
            }],
            "lanes": [{"id": "actor#1"}],
            "edges": [],
        });
        let out = bound_causality_graph(&json!({"causality": graph})).expect("graph");
        assert!(
            out["steps"][0].get("lanes").is_none(),
            "one surviving candidate is not a may-set"
        );
    }

    #[test]
    fn handoff_types_are_bounded_and_empties_dropped() {
        let graph = json!({
            "steps": [step("ask#1", "actor#1"), step("ask#2", "actor#2")],
            "lanes": [{"id": "actor#1"}, {"id": "actor#2"}],
            "edges": [],
        });
        let handoff = json!({
            "participants": [
                {"id": "unphased:actor#1", "phase": "unphased", "lane": "actor#1", "steps": ["ask#1"], "member": {"index": 0, "of": 2}},
                {"id": "unphased:actor#2", "phase": "unphased", "lane": "actor#2", "steps": ["ask#2"], "many": true},
            ],
            "handoffs": [{
                "from": "unphased:actor#1",
                "to": "unphased:actor#2",
                "back": true,
                "types": ["", "string", "string", "a", "b", "c", "d", "e", "f", "g"],
            }],
        });
        let out =
            bound_causality_graph(&json!({"causality": graph, "handoff": handoff})).expect("graph");
        assert_eq!(
            out["participants"],
            json!([
                {"id": "unphased:actor#1", "phase": "unphased", "lane": "actor#1", "steps": ["ask#1"], "member": {"index": 0, "of": 2}},
                {"id": "unphased:actor#2", "phase": "unphased", "lane": "actor#2", "steps": ["ask#2"], "many": true},
            ])
        );
        assert_eq!(
            out["handoffs"],
            json!([{
                "from": "unphased:actor#1",
                "to": "unphased:actor#2",
                "back": true,
                "types": ["string", "string", "a", "b", "c", "d", "e"],
            }]),
            "slice to 8 happens before the empty type is dropped"
        );
    }

    #[test]
    fn participant_and_handoff_caps_truncate() {
        let graph = json!({
            "steps": [step("ask#1", "actor#1")],
            "lanes": [{"id": "actor#1"}],
            "edges": [],
        });
        let participants: Vec<Value> = (0..=MAX_PARTICIPANTS)
            .map(|index| {
                json!({
                    "id": format!("unphased:actor#1[{index}]"),
                    "phase": "unphased",
                    "lane": "actor#1",
                    "steps": ["ask#1"],
                })
            })
            .collect();
        let mut handoffs = Vec::new();
        for index in 0..=MAX_HANDOFFS {
            handoffs.push(json!({
                "from": format!("unphased:actor#1[{}]", index % 2),
                "to": format!("unphased:actor#1[{}]", (index + 1) % 2),
            }));
        }
        let handoff = json!({"participants": participants, "handoffs": handoffs});
        let out =
            bound_causality_graph(&json!({"causality": graph, "handoff": handoff})).expect("graph");
        assert_eq!(
            out["participants"].as_array().expect("participants").len(),
            MAX_PARTICIPANTS
        );
        assert_eq!(
            out["handoffs"].as_array().expect("handoffs").len(),
            MAX_HANDOFFS
        );
        assert_eq!(out["truncated"], json!(true));
    }

    #[test]
    fn phase_vocabulary_is_all_or_nothing() {
        let graph = json!({
            "steps": [step("ask#1", "actor#1")],
            "lanes": [{"id": "actor#1"}],
            "edges": [],
        });
        let phases: Vec<Value> = (1..=33)
            .map(|index| json!({"id": format!("phase#{index}"), "name": format!("p{index}")}))
            .collect();
        let flow = json!({
            "phases": phases,
            "phaseEdges": [
                {"from": "entry", "to": "phase#1", "kind": "next"},
                {"from": "phase#1", "to": "phase#2", "kind": "next"},
                {"from": "phase#2", "to": "sink", "kind": "jump"},
            ],
        });
        let handoff = json!({
            "participants": [{
                "id": "phase#1:actor#1",
                "phase": "phase#1",
                "lane": "actor#1",
                "steps": ["ask#1"],
            }],
            "handoffs": [],
        });
        let measured = bound_causality_graph(&json!({
            "causality": graph,
            "flow": flow,
            "handoff": handoff,
        }))
        .expect("graph");
        assert!(measured.get("phases").is_none());
        assert!(measured.get("phaseEdges").is_none());
        assert!(measured.get("exits").is_none());
        assert!(measured["steps"][0].get("phase").is_none());
        assert_eq!(measured["participants"][0]["phase"], json!("unphased"));
        assert_eq!(measured["truncated"], json!(true));
    }

    #[test]
    fn handoff_ids_are_matched_against_bounded_card_ids() {
        let long_from = "p".repeat(MAX_ID_CHARS + 6);
        let long_to = format!("{}x", "p".repeat(MAX_ID_CHARS + 6));
        let graph = json!({
            "steps": [step("ask#1", "actor#1"), step("ask#2", "actor#2")],
            "lanes": [{"id": "actor#1"}, {"id": "actor#2"}],
            "edges": [],
        });
        let handoff = json!({
            "participants": [
                {
                    "id": long_from.clone(),
                    "phase": "unphased",
                    "lane": "actor#1",
                    "steps": ["ask#1"],
                },
                {
                    "id": long_to.clone(),
                    "phase": "unphased",
                    "lane": "actor#2",
                    "steps": ["ask#2"],
                },
            ],
            "handoffs": [{"from": long_from, "to": long_to}],
        });
        let out =
            bound_causality_graph(&json!({"causality": graph, "handoff": handoff})).expect("graph");
        assert_eq!(
            out["handoffs"],
            json!([]),
            "edges are filtered by the bounded card ids, before any edge id is bounded"
        );
        assert_eq!(
            out["participants"][0]["id"],
            json!("p".repeat(MAX_ID_CHARS))
        );
        assert_eq!(
            out["participants"][1]["id"],
            json!("p".repeat(MAX_ID_CHARS))
        );
    }

    #[test]
    fn phase_edges_fold_and_reduce() {
        let graph = json!({
            "steps": [step("ask#1", "actor#1")],
            "lanes": [{"id": "actor#1"}],
            "edges": [],
        });
        let flow = json!({
            "phases": [
                {"id": "a", "name": "A"},
                {"id": "b", "name": "B"},
                {"id": "c", "name": "C"},
            ],
            "phaseEdges": [
                {"from": "entry", "to": "a", "kind": "next"},
                {"from": "a", "to": "b", "kind": "next"},
                {"from": "a", "to": "b", "kind": "loop"},
                {"from": "b", "to": "a", "kind": "next"},
                {"from": "b", "to": "c", "kind": "next"},
                {"from": "a", "to": "c", "kind": "next"},
                {"from": "a", "to": "sink", "kind": "jump"},
                {"from": "b", "to": "abort", "kind": "throw"},
                {"from": "a", "to": "a", "kind": "next"},
                {"from": "a", "to": "missing", "kind": "next"},
            ],
        });
        let out = bound_causality_graph(&json!({"causality": graph, "flow": flow})).expect("graph");
        assert_eq!(
            out["phaseEdges"],
            json!([
                {"from": "a", "to": "b"},
                {"from": "b", "to": "a"},
                {"from": "b", "to": "c"},
                {"from": "a", "to": "c"},
            ]),
            "self-loop dropped, entry/abort edges dropped, a->b back merged away"
        );
        assert_eq!(out["exits"], json!(["a"]));
        assert_eq!(out["phases"].as_array().expect("phases").len(), 3);
    }
}
