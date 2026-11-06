//! The renderers: the walk's facts written out as the `causality` / `flow` / `handoff` triple.
//!
//! The shapes mirror the TypeScript analyzer's output field by field, because the host's
//! `bound_causality_graph` is written against that shape and serves both languages from one copy
//! of the caps, the truncation order and the all-or-nothing phase vocabulary.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use super::*;
use crate::script::ir::SiteIndex;
use crate::script::plan::Plan;

impl Projection<'_> {
    /// The cards one step appears on: a phase × lane cross product, expanded over a family's
    /// literal members.
    fn cards_of(&self, step: &Step) -> Vec<CardKey> {
        let phases: Vec<String> = if step.phases.is_empty() {
            vec![UNPHASED.to_string()]
        } else {
            step.phases.iter().map(|index| phase_id(*index)).collect()
        };
        let lane = self.lanes.iter().find(|lane| lane.id == step.lane);
        let members: Vec<Option<(usize, usize)>> = match lane {
            Some(lane) if !lane.families.is_empty() => match lane.cardinality {
                Some(count) if count > 1 => (0..count).map(|index| Some((index, count))).collect(),
                Some(_) => vec![None],
                None => vec![None],
            },
            _ => vec![None],
        };
        let mut out = Vec::new();
        for phase in &phases {
            for member in &members {
                out.push(CardKey {
                    phase: phase.clone(),
                    lane: step.lane.clone(),
                    member: *member,
                });
            }
        }
        out
    }

    fn uses_lane(&self, lane: &str) -> bool {
        self.steps.iter().any(|step| step.lane == lane)
    }

    fn many_card(&self, lane_id: &str) -> bool {
        self.lanes.iter().any(|lane| {
            lane.id == lane_id && !lane.families.is_empty() && lane.cardinality.is_none()
        })
    }

    /// The phase quotient from the marker trace.
    fn flow_edges(&self) -> Vec<(String, String, &'static str)> {
        let mut edges: Vec<(String, String, &'static str)> = Vec::new();
        let mut previous: Option<String> = None;
        for entry in &self.trace {
            match entry {
                Trace::Exit => {
                    if let Some(from) = &previous {
                        push_unique(&mut edges, from.clone(), FLOW_SINK.to_string(), "jump");
                    }
                }
                Trace::Mark(index, inside_loop) => {
                    let id = phase_id(*index);
                    match &previous {
                        None => push_unique(&mut edges, FLOW_ENTRY.to_string(), id.clone(), "next"),
                        Some(from) => {
                            if from != &id {
                                push_unique(&mut edges, from.clone(), id.clone(), "next");
                            }
                            if *inside_loop {
                                push_unique(&mut edges, id.clone(), from.clone(), "loop");
                            }
                        }
                    }
                    previous = Some(id);
                }
            }
        }
        for phase in &self.phases_that_can_reject {
            push_unique(
                &mut edges,
                phase_id(*phase),
                FLOW_ABORT.to_string(),
                "throw",
            );
        }
        edges
    }
}

fn phase_id(index: usize) -> String {
    format!("phase#{}", index + 1)
}

fn push_unique(
    edges: &mut Vec<(String, String, &'static str)>,
    from: String,
    to: String,
    kind: &'static str,
) {
    if edges
        .iter()
        .any(|edge| edge.0 == from && edge.1 == to && edge.2 == kind)
    {
        return;
    }
    edges.push((from, to, kind));
}

// ————————————————————————————————————————————————————————————————————————————————
// The analysis triple
// ————————————————————————————————————————————————————————————————————————————————

/// The JSON the host's `bound_causality_graph` consumes: `causality`, `flow`, `handoff`.
pub fn project_analysis(plan: &Plan) -> Value {
    let mut projection = Projection::new(plan);
    projection.run(&plan.body, State::root());
    json!({
        "causality": causality_of(&projection),
        "flow": flow_of(&projection),
        "handoff": handoff_of(&projection),
    })
}

fn causality_of(projection: &Projection<'_>) -> Value {
    let plan = projection.plan;
    let steps: Vec<Value> = projection
        .steps
        .iter()
        .flat_map(|step| {
            let phases: Vec<Option<usize>> = if step.phases.is_empty() {
                vec![None]
            } else {
                step.phases.iter().map(|index| Some(*index)).collect()
            };
            phases.into_iter().map(move |phase| (step, phase))
        })
        .map(|(step, phase)| {
            let mut out = Map::new();
            out.insert("id".to_string(), json!(plan.sites[step.site].id));
            out.insert("kind".to_string(), json!(step.kind));
            out.insert("label".to_string(), json!(step.label));
            if let Some(pattern) = &step.label_pattern {
                out.insert("labelPattern".to_string(), pattern.clone());
            }
            out.insert(
                "loc".to_string(),
                json!({"line": step.span.line, "column": step.span.column}),
            );
            out.insert("lane".to_string(), json!(step.lane));
            if step.lanes.len() > 1 {
                out.insert("lanes".to_string(), json!(step.lanes));
            }
            out.insert("region".to_string(), json!(step.region));
            out.insert("certainty".to_string(), json!(step.certainty));
            if let Some(index) = phase {
                out.insert("phase".to_string(), json!(phase_id(index)));
            }
            if let Some(repeat) = step.repeat {
                out.insert("repeat".to_string(), json!(repeat));
            }
            Value::Object(out)
        })
        .collect();

    let regions: Vec<Value> = projection
        .regions
        .iter()
        .map(|region| {
            let mut out = Map::new();
            out.insert("id".to_string(), json!(region.id));
            out.insert("kind".to_string(), json!(region.kind.as_str()));
            if let Some(parent) = &region.parent {
                out.insert("parent".to_string(), json!(parent));
            }
            if let Some(span) = region.span {
                out.insert(
                    "loc".to_string(),
                    json!({"line": span.line, "column": span.column}),
                );
            }
            Value::Object(out)
        })
        .collect();

    let mut lanes: Vec<Value> = Vec::new();
    if projection.uses_lane(WORKSPACE_LANE) {
        lanes.push(json!({"id": WORKSPACE_LANE}));
    }
    for lane in &projection.lanes {
        let mut out = Map::new();
        out.insert("id".to_string(), json!(lane.id));
        if let Some(name) = &lane.name {
            out.insert("name".to_string(), json!(name));
        }
        if let Some(pattern) = &lane.name_pattern {
            out.insert("namePattern".to_string(), pattern.clone());
        }
        out.insert(
            "loc".to_string(),
            json!({"line": lane.span.line, "column": lane.span.column}),
        );
        if !lane.families.is_empty() {
            out.insert("families".to_string(), json!(lane.families));
        }
        lanes.push(Value::Object(out));
    }
    if projection.uses_lane(UNKNOWN_LANE) {
        lanes.push(json!({"id": UNKNOWN_LANE}));
    }

    let edges: Vec<Value> = projection
        .edges
        .iter()
        .map(|edge| {
            json!({
                "from": plan.sites[edge.from].id,
                "to": plan.sites[edge.to].id,
                "kind": edge.kind,
                "certainty": edge.certainty,
            })
        })
        .collect();

    let mut out = Map::new();
    out.insert("steps".to_string(), Value::Array(steps));
    out.insert("regions".to_string(), Value::Array(regions));
    out.insert("lanes".to_string(), Value::Array(lanes));
    out.insert("edges".to_string(), Value::Array(edges));
    if !projection.sink.is_empty() {
        out.insert(
            "sink".to_string(),
            json!({"fedBy": projection.sink.iter().map(|site| json!(plan.sites[*site].id.clone())).collect::<Vec<_>>()}),
        );
    }
    if !plan.phase_names.is_empty() {
        out.insert("phases".to_string(), phase_table(projection));
        out.insert(
            "phaseEdges".to_string(),
            Value::Array(
                projection
                    .flow_edges()
                    .into_iter()
                    .map(|(from, to, kind)| json!({"from": from, "to": to, "kind": kind}))
                    .collect(),
            ),
        );
    }
    Value::Object(out)
}

fn phase_table(projection: &Projection<'_>) -> Value {
    let plan = projection.plan;
    let mut table: Vec<Value> = Vec::new();
    if projection.steps.iter().any(|step| step.phases.is_empty()) {
        table.push(json!({"id": UNPHASED}));
    }
    for (index, name) in plan.phase_names.iter().enumerate() {
        let mut entry = Map::new();
        entry.insert("id".to_string(), json!(phase_id(index)));
        entry.insert("name".to_string(), json!(name));
        let alongside: Vec<Value> = plan
            .phase_alongside
            .get(index)
            .map(|row| {
                row.iter()
                    .map(|other| json!(phase_id(*other as usize)))
                    .collect()
            })
            .unwrap_or_default();
        if !alongside.is_empty() {
            entry.insert("alongside".to_string(), Value::Array(alongside));
        }
        table.push(Value::Object(entry));
    }
    Value::Array(table)
}

fn flow_of(projection: &Projection<'_>) -> Value {
    let plan = projection.plan;
    if plan.phase_names.is_empty() {
        return json!({});
    }
    let edges: Vec<Value> = projection
        .flow_edges()
        .into_iter()
        .map(|(from, to, kind)| json!({"from": from, "to": to, "kind": kind}))
        .collect();
    let phases: Vec<Value> = projection
        .plan
        .phase_names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let mut entry = Map::new();
            entry.insert("id".to_string(), json!(phase_id(index)));
            entry.insert("name".to_string(), json!(name));
            let alongside: Vec<Value> = plan
                .phase_alongside
                .get(index)
                .map(|row| {
                    row.iter()
                        .map(|other| json!(phase_id(*other as usize)))
                        .collect()
                })
                .unwrap_or_default();
            if !alongside.is_empty() {
                entry.insert("alongside".to_string(), Value::Array(alongside));
            }
            Value::Object(entry)
        })
        .collect();
    json!({"phases": phases, "phaseEdges": edges})
}

fn handoff_of(projection: &Projection<'_>) -> Value {
    let plan = projection.plan;
    let mut order: Vec<CardKey> = Vec::new();
    let mut cards: BTreeMap<CardKey, Vec<SiteIndex>> = BTreeMap::new();
    let mut site_cards: BTreeMap<SiteIndex, Vec<String>> = BTreeMap::new();
    for step in &projection.steps {
        for key in projection.cards_of(step) {
            let id = key.id();
            if !order.contains(&key) {
                order.push(key.clone());
            }
            cards.entry(key).or_default().push(step.site);
            site_cards.entry(step.site).or_default().push(id.clone());
        }
    }

    let participants: Vec<Value> = order
        .iter()
        .map(|key| {
            let mut card = Map::new();
            card.insert("id".to_string(), json!(key.id()));
            card.insert("phase".to_string(), json!(key.phase));
            card.insert("lane".to_string(), json!(key.lane));
            let steps: Vec<Value> = cards[key]
                .iter()
                .map(|site| json!(plan.sites[*site].id.clone()))
                .collect();
            card.insert("steps".to_string(), Value::Array(steps));
            if let Some((index, of)) = key.member {
                card.insert("member".to_string(), json!({"index": index, "of": of}));
            } else if projection.many_card(&key.lane) {
                card.insert("many".to_string(), json!(true));
            }
            Value::Object(card)
        })
        .collect();

    let mut handoff_order: Vec<(String, String)> = Vec::new();
    let mut types: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut back_only: BTreeMap<(String, String), bool> = BTreeMap::new();
    for edge in &projection.edges {
        let froms = site_cards.get(&edge.from).cloned().unwrap_or_default();
        let tos = site_cards.get(&edge.to).cloned().unwrap_or_default();
        for from in &froms {
            for to in &tos {
                if from == to {
                    continue;
                }
                let key = (from.clone(), to.clone());
                if !handoff_order.contains(&key) {
                    handoff_order.push(key.clone());
                    back_only.insert(key.clone(), true);
                }
                if edge.kind == "data" {
                    let product = product_of(plan, edge.from);
                    let entry = types.entry(key.clone()).or_default();
                    if !entry.contains(&product) {
                        entry.push(product);
                    }
                }
                // `back` is the conjunction over contributing edges: one forward edge and the
                // handoff is not a loop-carried one.
                if !(edge.kind == "control" && edge.certainty == MAYBE) {
                    back_only.insert(key, false);
                }
            }
        }
    }

    let handoffs: Vec<Value> = handoff_order
        .into_iter()
        .map(|key| {
            let mut out = Map::new();
            out.insert("from".to_string(), json!(key.0));
            out.insert("to".to_string(), json!(key.1));
            if back_only.get(&key).copied().unwrap_or(false) {
                out.insert("back".to_string(), json!(true));
            }
            if let Some(kinds) = types.get(&key) {
                if !kinds.is_empty() {
                    out.insert(
                        "types".to_string(),
                        Value::Array(kinds.iter().map(|kind| json!(kind)).collect()),
                    );
                }
            }
            Value::Object(out)
        })
        .collect();

    json!({"participants": participants, "handoffs": handoffs})
}
