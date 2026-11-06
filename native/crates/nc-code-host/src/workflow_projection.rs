//! The pure reduction of `workflowRuns`: one dwf progress event → the new `workflowRuns` state.
//!
//! This file is an entry-by-entry port of `packages/shared/src/nc-code-protocol-v4/workflow-runs-reducer.ts`
//! (including the `workflow-runs-{artifacts,concurrency,lineage,node-progress,phases,started}.ts` it split out,
//! and the summary functions of `workflow-artifact.ts` and `workflow-artifacts.ts`). Port rules:
//!
//! - Pure functions: no I/O, no clock, no randomness, no runtime TypeScript.
//! - `None` corresponds to TS's `null`: an invalid event (missing `runId` / `eventType`), or the same event replayed with
//!   completely identical content. The caller withholds `state.updated` accordingly, and `revision` only rises when something really changed.
//! - String lengths are counted in **UTF-16 code units** (JS's `.length` / `slice`), not Rust characters or bytes.
//!   The only corner that cannot be replicated verbatim: JS's `slice` may cut a surrogate pair into a lone surrogate, while Rust's `String`
//!   (like the JSON parser) cannot carry a lone surrogate, so a cut in the middle of a surrogate pair drops one code unit — see
//!   [`js_slice`].
//! - Object key order matches TS's object spread (`serde_json`'s `preserve_order`), but semantics are decided by JSON value equality:
//!   the idempotence check compares values, not byte-for-byte strings.
//!
//! Three structural facts from engine Boundary C (the TS file header verbatim):
//!   1. A node's phase is exactly the event the engine emits: queued/dispatched/executing/waiting/repairing/nudged/settled.
//!   2. A resume's completion hit takes the short circuit and emits `node-settled` directly, without `node-queued`, so `kind` may be absent.
//!   3. `nodesUsed` is counted by the first phase transition of `node-dispatched`; replaying the same instance does not count again.

use serde_json::{Map, Number, Value};

type Obj = Map<String, Value>;

// ── WORKFLOW_RUNS_LIMITS (workflow-runs.ts) ──
const MAX_RUNS: usize = 8;
const MAX_ACTORS: usize = 256;
const MAX_NODES: usize = 256;
const MAX_REPORTS: usize = 64;
const MAX_REPORT_PREVIEW_LENGTH: usize = 2_048;
const MAX_ERROR_LENGTH: usize = 2_048;
const MAX_PENDING_QUESTIONS: usize = 32;
const MAX_QUESTION_LENGTH: usize = 2_048;
const MAX_CONCURRENCY_KEY_LENGTH: usize = 256;
const MAX_SUBAGENT_MODEL_LENGTH: usize = 256;
const MAX_CONCURRENCY_CEILING: f64 = 1_024.0;
const MAX_ACTOR_NAME_LENGTH: usize = 128;
const MAX_ARTIFACTS: usize = 32;
const MAX_PHASES: usize = 32;
const MAX_PHASE_NAME_LENGTH: usize = 128;
const MAX_INSTRUCTIONS_HEAD_LENGTH: usize = 240;
const MAX_LAST_TOOL_NAME_LENGTH: usize = 64;
const MAX_LAST_TOOL_TARGET_LENGTH: usize = 120;

// ── WORKFLOW_ARTIFACT_LIMITS (workflow-artifacts.ts) ──
const MAX_ARTIFACT_ID_LENGTH: usize = 64;
const MAX_ARTIFACT_TITLE_LENGTH: usize = 120;
const MAX_ARTIFACT_VERSIONS: f64 = 16.0;
const MAX_ARTIFACT_CONTENT_TYPE_LENGTH: usize = 128;

/// Node event → phase. An explicit table rather than string slicing, isomorphic to TS's `NODE_EVENT_PHASE`.
fn node_event_phase(event_type: &str) -> Option<&'static str> {
    match event_type {
        "node-queued" => Some("queued"),
        "node-dispatched" => Some("dispatched"),
        "node-executing" => Some("executing"),
        "node-waiting" => Some("waiting"),
        "node-repairing" => Some("repairing"),
        "node-nudged" => Some("nudged"),
        "node-settled" => Some("settled"),
        _ => None,
    }
}

/// Reduce one progress event into the `workflowRuns` state key.
///
/// Returning `None` means no semantic change (an invalid event, or the same event replayed with identical content). An absent `previous` is
/// equivalent to the empty state `{ revision: 0, runs: [] }`.
pub fn reduce_workflow_runs(previous: Option<&Value>, envelope: &Value) -> Option<Value> {
    let env = envelope.as_object()?;
    let run_id = match env.get("runId") {
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        _ => return None,
    };
    let event_type = match env.get("eventType") {
        Some(Value::String(text)) => text.clone(),
        _ => return None,
    };
    // typeof envelope.sequence === "number" ? envelope.sequence : 0
    let sequence_value = match env.get("sequence") {
        Some(value @ Value::Number(_)) => value.clone(),
        _ => Value::from(0),
    };
    let sequence = sequence_value.as_f64().unwrap_or(0.0);
    let empty_payload = Obj::new();
    let payload = env
        .get("payload")
        .and_then(Value::as_object)
        .unwrap_or(&empty_payload);

    let prior = previous.and_then(Value::as_object);
    let revision = prior
        .and_then(|state| state.get("revision"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let mut runs: Vec<Value> = prior
        .and_then(|state| state.get("runs"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let existing_index = runs
        .iter()
        .position(|run| matches!(run.get("runId"), Some(Value::String(text)) if text == &run_id));
    let existing = existing_index.and_then(|index| runs.get(index).cloned());

    let base = match &existing {
        Some(run) => run.as_object().cloned().unwrap_or_default(),
        None => {
            let mut fresh = Obj::new();
            fresh.insert("runId".into(), Value::String(run_id));
            if let Some(tool_call_id) = env.get("toolCallId").filter(|value| truthy(value)) {
                fresh.insert("toolCallId".into(), tool_call_id.clone());
            }
            fresh.insert("status".into(), Value::String("pending".into()));
            fresh.insert("usage".into(), usage_object(0.0, 0.0));
            fresh.insert("actors".into(), Value::Array(Vec::new()));
            fresh.insert("nodes".into(), Value::Array(Vec::new()));
            fresh.insert("lastEventSequence".into(), sequence_value);
            fresh
        }
    };

    // Monotonic: a late/replayed event never pulls the watermark back.
    let base_sequence = base
        .get("lastEventSequence")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let derived = Derived {
        actor_session_id: env.get("actorSessionId").cloned(),
        tool_call_id: env.get("toolCallId").cloned(),
        sequence: base_sequence.max(sequence),
    };

    let next = apply_workflow_run_event(base, &event_type, payload, &derived);

    // Idempotent: no semantic change produces no delta (replaying the same event does not raise revision).
    if let (Some(index), Some(prior_run)) = (existing_index, existing.as_ref()) {
        if prior_run == &Value::Object(next.clone()) {
            return None;
        }
        let mut runs_out = runs.clone();
        runs_out[index] = Value::Object(next);
        runs = runs_out;
    } else {
        runs.push(Value::Object(next));
    }

    // The most recent ~8, evicting the oldest.
    if runs.len() > MAX_RUNS {
        runs.drain(0..runs.len() - MAX_RUNS);
    }

    let mut state = Obj::new();
    state.insert("revision".into(), number_value(revision + 1.0));
    state.insert("runs".into(), Value::Array(runs));
    Some(Value::Object(state))
}

/// The derived fields of the event envelope that hang outside the payload.
struct Derived {
    actor_session_id: Option<Value>,
    tool_call_id: Option<Value>,
    sequence: f64,
}

fn apply_workflow_run_event(base: Obj, event_type: &str, payload: &Obj, derived: &Derived) -> Obj {
    let mut run = base;
    if let Some(tool_call_id) = derived.tool_call_id.as_ref() {
        let base_has_tool_call = run.get("toolCallId").is_some_and(truthy);
        if truthy(tool_call_id) && !base_has_tool_call {
            run.insert("toolCallId".into(), tool_call_id.clone());
        }
    }
    run.insert("lastEventSequence".into(), number_value(derived.sequence));

    match event_type {
        "run-started" => reduce_run_started(run, payload),
        "actor-created" => reduce_actor_created(run, payload, derived),
        "node-queued" | "node-dispatched" | "node-executing" | "node-waiting"
        | "node-repairing" | "node-nudged" | "node-settled" => {
            reduce_node_event(run, payload, event_type)
        }
        "node-progress" => reduce_node_progress(run, payload),
        "report" => reduce_report(run, payload),
        "artifact-published" => reduce_artifact_published(run, payload),
        // A failed publish claims no id / kind / version: there is nothing to upsert.
        "artifact-failed" => run,
        "usage-updated" => reduce_usage_updated(run, payload),
        "escalation-raised" => reduce_escalation_raised(run, payload),
        "escalation-resolved" => reduce_escalation_resolved(run, payload),
        "concurrency-changed" => reduce_concurrency_changed(run, payload),
        "phase-entered" => reduce_phase_entered(run, payload),
        "run-launched" => reduce_run_launched(run, payload),
        "run-settled" => reduce_run_settled(run, payload),
        // log / compaction / unknown events are not a step and have no site id: only the watermark rises.
        _ => run,
    }
}

// ── run-started (workflow-runs-started.ts) ──

fn reduce_run_started(mut run: Obj, payload: &Obj) -> Obj {
    // The previous life's settlement residue and parked questions: once a resume is running they no longer belong to this life.
    for key in [
        "error",
        "resultPreview",
        "pendingQuestions",
        "resumable",
        "stopReason",
    ] {
        run.remove(key);
    }
    // The lineage pointer arrives with run-started; the resume branch carries the same value, so just move it.
    if let Some(resumed_from) = read_run_id_field(payload.get("resumedFrom")) {
        run.insert("resumedFrom".into(), Value::String(resumed_from));
    }
    // Subagent model: fall back to the known value when it cannot be read (older CLI does not send this key).
    if let Some(subagent_model) = read_subagent_model(payload.get("subagentModel")) {
        run.insert("subagentModel".into(), Value::String(subagent_model));
    }
    run.insert("status".into(), Value::String("running".into()));
    run.insert("usage".into(), usage_object(0.0, 0.0));
    reduce_run_started_concurrency(run, payload)
}

fn reduce_run_started_concurrency(mut run: Obj, payload: &Obj) -> Obj {
    let limit = payload
        .get("caps")
        .and_then(Value::as_object)
        .and_then(|caps| positive_integer(caps.get("maxConcurrency")));
    let read_ceiling = positive_integer(payload.get("concurrencyCeiling"));
    let ceiling = read_ceiling.filter(|value| *value <= MAX_CONCURRENCY_CEILING);

    // The ceiling itself is recorded separately; keep the known value when it cannot be read.
    if let Some(ceiling) = ceiling {
        let known = run.get("concurrencyCeiling").and_then(Value::as_f64);
        if known != Some(ceiling) {
            run.insert("concurrencyCeiling".into(), number_value(ceiling));
        }
    }

    let (Some(limit), Some(ceiling)) = (limit, ceiling) else {
        return run;
    };
    // A run that lives at the ceiling is byte-for-byte identical to before, with not one extra key.
    if limit >= ceiling {
        return run;
    }

    let existing = run.get("concurrency").and_then(Value::as_object).cloned();
    let mut concurrency = match &existing {
        // With no shared-bucket reading, the cap starts at the ceiling.
        Some(current) => current.clone(),
        None => {
            let mut fresh = Obj::new();
            fresh.insert("cap".into(), number_value(ceiling));
            fresh
        }
    };
    let existing_ceiling = existing
        .as_ref()
        .and_then(|current| current.get("ceiling"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    concurrency.insert(
        "ceiling".into(),
        number_value(existing_ceiling.max(ceiling)),
    );
    concurrency.insert("limit".into(), number_value(limit));
    run.insert("concurrency".into(), Value::Object(concurrency));
    run
}

/// The subagent model on the payload (a canonical string). Over the limit, the whole value is dropped rather than truncated.
fn read_subagent_model(value: Option<&Value>) -> Option<String> {
    let text = js_trim(value?.as_str()?);
    let length = js_len(text);
    if length == 0 || length > MAX_SUBAGENT_MODEL_LENGTH {
        None
    } else {
        Some(text.to_string())
    }
}

// ── actor-created ──

fn reduce_actor_created(mut run: Obj, payload: &Obj, derived: &Derived) -> Obj {
    let Some(actor_ref) = instance_ref(payload.get("actor")) else {
        return run;
    };
    let name = non_empty_str(payload.get("name")).map(|text| js_slice(text, MAX_ACTOR_NAME_LENGTH));
    let phase_name =
        non_empty_str(payload.get("phaseName")).map(|text| js_slice(text, MAX_PHASE_NAME_LENGTH));

    let mut actor = Obj::new();
    actor.insert("siteId".into(), Value::String(actor_ref.site_id));
    actor.insert("ordinal".into(), actor_ref.ordinal);
    if let Some(name) = name {
        actor.insert("name".into(), Value::String(name));
    }
    if let Some(session_id) = derived
        .actor_session_id
        .as_ref()
        .filter(|value| truthy(value))
    {
        actor.insert("sessionId".into(), session_id.clone());
    }
    if let Some(phase_name) = phase_name {
        actor.insert("phaseName".into(), Value::String(phase_name));
    }
    // A placeholder value: the state derivation right after recomputes it from the node and run terminal states.
    actor.insert("status".into(), Value::String("waiting".into()));

    let mut actors = clone_array(run.get("actors"));
    let truncated = upsert_by_instance(&mut actors, Value::Object(actor), MAX_ACTORS);
    run.insert("actors".into(), Value::Array(actors));
    if truncated || is_truncated(&run) {
        run.insert("truncated".into(), Value::Bool(true));
    }
    with_derived_actor_statuses(run)
}

// ── node-* ──

fn reduce_node_event(mut run: Obj, payload: &Obj, event_type: &str) -> Obj {
    let Some(node_ref) = instance_ref(payload.get("instance")) else {
        return run;
    };
    let Some(phase) = node_event_phase(event_type) else {
        return run;
    };
    let nodes = clone_array(run.get("nodes"));
    let previous_index = nodes
        .iter()
        .position(|node| matches_instance(node, &node_ref.site_id, &node_ref.ordinal));
    let previous_node = previous_index.and_then(|index| nodes.get(index).cloned());

    let kind = match payload.get("kind") {
        Some(Value::String(text)) if text == "ask" || text == "world-read" => {
            Some(Value::String(text.clone()))
        }
        _ => previous_node
            .as_ref()
            .and_then(|node| node.get("kind"))
            .cloned(),
    };
    // The birth phase name is stamped only on the birth event; everything else carries it forward.
    let phase_name = non_empty_str(payload.get("phaseName"))
        .map(|text| js_slice(text, MAX_PHASE_NAME_LENGTH))
        .map(Value::String)
        .or_else(|| {
            previous_node
                .as_ref()
                .and_then(|node| node.get("phaseName"))
                .cloned()
        });

    let mut node = Obj::new();
    node.insert("siteId".into(), Value::String(node_ref.site_id));
    node.insert("ordinal".into(), node_ref.ordinal);
    if let Some(kind) = kind {
        node.insert("kind".into(), kind);
    }
    node.insert("phase".into(), Value::String(phase.into()));
    if let Some(Value::String(outcome)) = payload.get("outcome") {
        if outcome == "ok" || outcome == "failed" || outcome == "cancelled" {
            node.insert("outcome".into(), Value::String(outcome.clone()));
        }
    }
    if payload.get("cached") == Some(&Value::Bool(true)) {
        node.insert("cached".into(), Value::Bool(true));
    }
    match instance_ref(payload.get("actor")) {
        Some(actor_ref) => {
            node.insert("actorSiteId".into(), Value::String(actor_ref.site_id));
            node.insert("actorOrdinal".into(), actor_ref.ordinal);
        }
        None => {
            if let Some(previous) = previous_node.as_ref() {
                if let Some(site_id) = previous.get("actorSiteId") {
                    node.insert("actorSiteId".into(), site_id.clone());
                    if let Some(ordinal) = previous.get("actorOrdinal") {
                        node.insert("actorOrdinal".into(), ordinal.clone());
                    }
                }
            }
        }
    }
    if let Some(phase_name) = phase_name {
        node.insert("phaseName".into(), phase_name);
    }
    for (key, value) in carry_node_progress(event_type, payload, previous_node.as_ref()) {
        node.insert(key, value);
    }

    let mut updated_nodes = nodes;
    let truncated = upsert_by_instance(&mut updated_nodes, Value::Object(node), MAX_NODES);

    // An instance's **first** dispatch counts one step; replaying the same node-dispatched does not count again.
    let first_dispatch = event_type == "node-dispatched"
        && previous_node
            .as_ref()
            .is_none_or(|node| node.get("phase").and_then(Value::as_str) == Some("queued"));
    if first_dispatch {
        let mut usage = run
            .get("usage")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let nodes_used = usage
            .get("nodesUsed")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        usage.insert("nodesUsed".into(), number_value(nodes_used + 1.0));
        run.insert("usage".into(), Value::Object(usage));
    }

    run.insert("nodes".into(), Value::Array(updated_nodes));
    if truncated || is_truncated(&run) {
        run.insert("truncated".into(), Value::Bool(true));
    }
    with_derived_actor_statuses(run)
}

/// The values of `instructionsHead` / `turn` / `toolCalls` / `lastTool` on lifecycle events.
fn carry_node_progress(
    event_type: &str,
    payload: &Obj,
    previous_node: Option<&Value>,
) -> Vec<(String, Value)> {
    let requeued = event_type == "node-queued";
    let born = requeued
        || (event_type == "node-settled" && payload.get("cached") == Some(&Value::Bool(true)));
    let carried = if born { None } else { previous_node };

    let head = bounded_text(
        payload.get("instructionsHead"),
        MAX_INSTRUCTIONS_HEAD_LENGTH,
    );
    let previous_head = previous_node
        .and_then(|node| node.get("instructionsHead"))
        .cloned();
    let instructions_head = if requeued {
        head.map(Value::String)
    } else {
        head.map(Value::String).or(previous_head)
    };

    let mut fields = Vec::new();
    if let Some(value) = instructions_head {
        fields.push(("instructionsHead".to_string(), value));
    }
    if let Some(value) = carried.and_then(|node| node.get("turn")) {
        fields.push(("turn".to_string(), value.clone()));
    }
    if let Some(value) = carried.and_then(|node| node.get("toolCalls")) {
        fields.push(("toolCalls".to_string(), value.clone()));
    }
    if let Some(value) = carried.and_then(|node| node.get("lastTool")) {
        fields.push(("lastTool".to_string(), value.clone()));
    }
    fields
}

// ── node-progress（workflow-runs-node-progress.ts）──

fn reduce_node_progress(mut run: Obj, payload: &Obj) -> Obj {
    let Some(node_ref) = instance_ref(payload.get("instance")) else {
        return run;
    };
    let nodes = clone_array(run.get("nodes"));
    let Some(index) = nodes
        .iter()
        .position(|node| matches_instance(node, &node_ref.site_id, &node_ref.ordinal))
    else {
        return run;
    };
    let mut node = nodes[index].as_object().cloned().unwrap_or_default();

    let turn = positive_integer(payload.get("turn"))
        .map(number_value)
        .or_else(|| node.get("turn").cloned());
    let tool_calls = non_negative_integer(payload.get("toolCalls"))
        .map(number_value)
        .or_else(|| node.get("toolCalls").cloned());
    let last_tool = read_last_tool(payload.get("lastTool"))
        .map(Value::Object)
        .or_else(|| node.get("lastTool").cloned());

    if let Some(turn) = turn {
        node.insert("turn".into(), turn);
    }
    if let Some(tool_calls) = tool_calls {
        node.insert("toolCalls".into(), tool_calls);
    }
    if let Some(last_tool) = last_tool {
        node.insert("lastTool".into(), last_tool);
    }

    let mut updated = nodes;
    updated[index] = Value::Object(node);
    run.insert("nodes".into(), Value::Array(updated));
    run
}

/// The tool name is the very reason this record exists; when it cannot be read the whole record is not written.
fn read_last_tool(value: Option<&Value>) -> Option<Obj> {
    let record = value?.as_object()?;
    let name = bounded_text(record.get("name"), MAX_LAST_TOOL_NAME_LENGTH)?;
    let mut tool = Obj::new();
    tool.insert("name".into(), Value::String(name));
    if let Some(target) = bounded_text(record.get("target"), MAX_LAST_TOOL_TARGET_LENGTH) {
        tool.insert("target".into(), Value::String(target));
    }
    Some(tool)
}

// ── report ──

fn reduce_report(mut run: Obj, payload: &Obj) -> Obj {
    let Some(report_ref) = instance_ref(payload.get("instance")) else {
        return run;
    };
    let artifact_id = non_empty_str(payload.get("artifactId")).map(str::to_string);
    let preview = workflow_report_preview(payload.get("item"));

    let mut report = Obj::new();
    report.insert("siteId".into(), Value::String(report_ref.site_id.clone()));
    report.insert("ordinal".into(), report_ref.ordinal.clone());
    report.insert("preview".into(), Value::String(preview));
    if let Some(artifact_id) = artifact_id.as_ref() {
        report.insert("artifactId".into(), Value::String(artifact_id.clone()));
    }

    let existing_reports = clone_array(run.get("reports"));
    // The dedupe key for counting is the same one the reports table uses: (siteId, ordinal), and it must be decided **before** the upsert.
    let first_sighting = !existing_reports
        .iter()
        .any(|item| matches_instance(item, &report_ref.site_id, &report_ref.ordinal));
    let mut reports = existing_reports;
    let truncated = upsert_by_instance(&mut reports, Value::Object(report), MAX_REPORTS);
    run.insert("reports".into(), Value::Array(reports));

    if first_sighting {
        if let Some(artifact_id) = artifact_id {
            count_tagged_report(&mut run, &artifact_id);
        }
    }
    if truncated || is_truncated(&run) {
        run.insert("truncated".into(), Value::Bool(true));
    }
    run
}

/// A labelled report that lands on an artifact: add one to that id's `itemCount`.
fn count_tagged_report(run: &mut Obj, artifact_id: &str) {
    let Some(Value::Array(artifacts)) = run.get_mut("artifacts") else {
        return;
    };
    let Some(index) = artifacts
        .iter()
        .position(|item| item.get("id").and_then(Value::as_str) == Some(artifact_id))
    else {
        return;
    };
    if let Some(record) = artifacts[index].as_object_mut() {
        let count = record
            .get("itemCount")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        record.insert("itemCount".into(), number_value(count + 1.0));
    }
}

/// The display preview of one report entry: a string verbatim, anything else pretty-printed JSON.
fn workflow_report_preview(item: Option<&Value>) -> String {
    let text = serialize_workflow_artifact(item).unwrap_or_default();
    if js_len(&text) <= MAX_REPORT_PREVIEW_LENGTH {
        text
    } else {
        let mut out = js_slice(&text, MAX_REPORT_PREVIEW_LENGTH - 1);
        out.push('…');
        out
    }
}

/// `serializeWorkflowArtifact`（workflow-artifact.ts）。
fn serialize_workflow_artifact(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Value::String(text) = value {
        return Some(text.clone());
    }
    match serde_json::to_string_pretty(value) {
        Ok(text) => Some(text),
        Err(_) => Some(js_string(Some(value))),
    }
}

// ── artifact-published（workflow-runs-artifacts.ts）──

fn reduce_artifact_published(mut run: Obj, payload: &Obj) -> Obj {
    let Some(summary) = workflow_artifact_summary(payload.get("artifact")) else {
        return run;
    };
    let mut artifacts = clone_array(run.get("artifacts"));
    let truncated = upsert_by_artifact_id(&mut artifacts, Value::Object(summary), MAX_ARTIFACTS);
    run.insert("artifacts".into(), Value::Array(artifacts));
    if truncated || is_truncated(&run) {
        run.insert("truncated".into(), Value::Bool(true));
    }
    run
}

/// `ArtifactVersionRecord` → the **newest version**'s metadata inside the snapshot.
fn workflow_artifact_summary(value: Option<&Value>) -> Option<Obj> {
    let record = value?.as_object()?;
    let id = non_empty_str(record.get("id"))?;
    let kind = non_empty_str(record.get("kind"))?;
    if !matches!(
        kind,
        "file" | "markdown" | "chart" | "table" | "metrics" | "board"
    ) {
        return None;
    }
    let version = record.get("version")?;
    if !is_js_integer(version) {
        return None;
    }
    let version = version.as_f64()?;
    if version < 1.0 {
        return None;
    }
    let title = non_empty_str(record.get("title"));
    let content_type = non_empty_str(record.get("contentType"));
    let bytes = record.get("bytes");

    let mut summary = Obj::new();
    summary.insert(
        "id".into(),
        Value::String(js_slice(id, MAX_ARTIFACT_ID_LENGTH)),
    );
    summary.insert("kind".into(), Value::String(kind.to_string()));
    if let Some(title) = title {
        summary.insert(
            "title".into(),
            Value::String(js_slice(title, MAX_ARTIFACT_TITLE_LENGTH)),
        );
    }
    summary.insert(
        "version".into(),
        number_value(version.min(MAX_ARTIFACT_VERSIONS)),
    );
    if let Some(content_type) = content_type {
        summary.insert(
            "contentType".into(),
            Value::String(js_slice(content_type, MAX_ARTIFACT_CONTENT_TYPE_LENGTH)),
        );
    }
    if let Some(bytes) = bytes {
        if is_js_integer(bytes) {
            if let Some(bytes) = bytes.as_f64() {
                if bytes >= 0.0 {
                    summary.insert("bytes".into(), number_value(bytes));
                }
            }
        }
    }
    if record.get("primary") == Some(&Value::Bool(true)) {
        summary.insert("primary".into(), Value::Bool(true));
    }
    Some(summary)
}

// ── usage-updated ──

fn reduce_usage_updated(mut run: Obj, payload: &Obj) -> Obj {
    // The event carries the already-spent totals directly; absent means keep the known value.
    let Some(spent_tokens) = payload.get("spentTokens").filter(|value| value.is_number()) else {
        return run;
    };
    let mut usage = run
        .get("usage")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    usage.insert("spentTokens".into(), spent_tokens.clone());
    run.insert("usage".into(), Value::Object(usage));
    run
}

// ── escalation ──

fn reduce_escalation_raised(mut run: Obj, payload: &Obj) -> Obj {
    let Some(qid) = non_empty_str(payload.get("qid")) else {
        return run;
    };
    let Some(question) = non_empty_str(payload.get("question")) else {
        return run;
    };
    // An unreadable actor ref does **not** discard the whole entry: the identity is the qid, the actor is only an attribute.
    let actor_ref = instance_ref(payload.get("actor"));
    let actor_name =
        non_empty_str(payload.get("actorName")).map(|text| js_slice(text, MAX_ACTOR_NAME_LENGTH));
    let context = non_empty_str(payload.get("context"));
    let asked_at = payload.get("askedAt").filter(|value| value.is_number());

    let mut pending = Obj::new();
    pending.insert("qid".into(), Value::String(qid.to_string()));
    if let Some(actor_ref) = actor_ref {
        pending.insert("actorSiteId".into(), Value::String(actor_ref.site_id));
        pending.insert("actorOrdinal".into(), actor_ref.ordinal);
    }
    if let Some(actor_name) = actor_name {
        pending.insert("actorName".into(), Value::String(actor_name));
    }
    pending.insert(
        "question".into(),
        Value::String(bounded_question_text(question)),
    );
    if let Some(context) = context {
        pending.insert(
            "context".into(),
            Value::String(bounded_question_text(context)),
        );
    }
    if let Some(asked_at) = asked_at {
        pending.insert("askedAt".into(), asked_at.clone());
    }

    let mut pending_questions = clone_array(run.get("pendingQuestions"));
    let truncated = upsert_by_qid(
        &mut pending_questions,
        Value::Object(pending),
        MAX_PENDING_QUESTIONS,
    );
    run.insert("pendingQuestions".into(), Value::Array(pending_questions));
    if truncated || is_truncated(&run) {
        run.insert("truncated".into(), Value::Bool(true));
    }
    run
}

fn reduce_escalation_resolved(mut run: Obj, payload: &Obj) -> Obj {
    let Some(qid) = non_empty_str(payload.get("qid")) else {
        return run;
    };
    let Some(Value::Array(current)) = run.get("pendingQuestions") else {
        return run;
    };
    let remaining: Vec<Value> = current
        .iter()
        .filter(|pending| pending.get("qid").and_then(Value::as_str) != Some(qid))
        .cloned()
        .collect();
    if remaining.len() == current.len() {
        return run;
    }
    // Zero entries ⇒ the whole key is removed (rather than leaving an empty array behind).
    if remaining.is_empty() {
        run.remove("pendingQuestions");
    } else {
        run.insert("pendingQuestions".into(), Value::Array(remaining));
    }
    run
}

// ── concurrency-changed（workflow-runs-concurrency.ts）──

fn reduce_concurrency_changed(mut run: Obj, payload: &Obj) -> Obj {
    let Some(next) = positive_integer(payload.get("next")) else {
        return run;
    };
    let previous = positive_integer(payload.get("previous")).unwrap_or(next);
    let existing_ceiling = run
        .get("concurrency")
        .and_then(Value::as_object)
        .and_then(|concurrency| concurrency.get("ceiling"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let ceiling = existing_ceiling.max(previous).max(next);
    let key = non_empty_str(payload.get("key"));
    let limit = run
        .get("concurrency")
        .and_then(Value::as_object)
        .and_then(|concurrency| concurrency.get("limit"))
        .cloned();
    let cooldown = if payload.get("reason").and_then(Value::as_str) == Some("idle_reset") {
        None
    } else {
        non_negative_integer(payload.get("cooldownMs"))
    };

    let mut concurrency = Obj::new();
    if let Some(key) = key {
        if js_len(key) <= MAX_CONCURRENCY_KEY_LENGTH {
            concurrency.insert("key".into(), Value::String(key.to_string()));
        }
    }
    concurrency.insert("cap".into(), number_value(next));
    concurrency.insert("ceiling".into(), number_value(ceiling));
    if let Some(limit) = limit {
        concurrency.insert("limit".into(), limit);
    }
    if let Some(cooldown) = cooldown {
        concurrency.insert("cooldownMs".into(), number_value(cooldown));
    }
    run.insert("concurrency".into(), Value::Object(concurrency));
    run
}

// ── phase-entered / run-launched（workflow-runs-phases.ts）──

fn reduce_phase_entered(mut run: Obj, payload: &Obj) -> Obj {
    let Some(raw) = payload.get("name").and_then(Value::as_str) else {
        return run;
    };
    let name = js_slice(raw, MAX_PHASE_NAME_LENGTH);
    if name.is_empty() {
        return run;
    }
    let ordinal = positive_integer(payload.get("ordinal")).unwrap_or(1.0);

    let existing = clone_array(run.get("phases"));
    let index = existing
        .iter()
        .position(|phase| phase.get("name").and_then(Value::as_str) == Some(name.as_str()));
    let mut phases = existing;
    let mut truncated = is_truncated(&run);
    if let Some(index) = index {
        let current_rounds = phases[index]
            .get("rounds")
            .and_then(Value::as_f64)
            .unwrap_or(f64::NEG_INFINITY);
        // rounds takes the max instead of +1: the replayed prefix of a resume must not double it.
        if current_rounds < ordinal {
            if let Some(record) = phases[index].as_object_mut() {
                record.insert("rounds".into(), number_value(ordinal));
            }
        }
    } else if phases.len() >= MAX_PHASES {
        // Touching the ceiling: refuse new entries, still update the ones already present.
        truncated = true;
    } else {
        let mut phase = Obj::new();
        phase.insert("name".into(), Value::String(name.clone()));
        phase.insert("rounds".into(), number_value(ordinal));
        phases.push(Value::Object(phase));
    }

    run.insert("phases".into(), Value::Array(phases));
    run.insert("currentPhase".into(), Value::String(name));
    if truncated {
        run.insert("truncated".into(), Value::Bool(true));
    }
    run
}

fn reduce_run_launched(mut run: Obj, payload: &Obj) -> Obj {
    let Some(raw_names) = payload.get("phaseNames").and_then(Value::as_array) else {
        return run;
    };
    let mut phase_names: Vec<String> = Vec::new();
    for raw in raw_names {
        let Some(text) = raw.as_str() else {
            continue;
        };
        let name = js_slice(text, MAX_PHASE_NAME_LENGTH);
        if name.is_empty() {
            continue;
        }
        phase_names.push(name);
        if phase_names.len() >= MAX_PHASES {
            break;
        }
    }
    if phase_names.is_empty() {
        return run;
    }
    let alongside = read_phase_alongside(payload.get("phaseAlongside"), phase_names.len());
    run.insert(
        "phaseNames".into(),
        Value::Array(phase_names.into_iter().map(Value::String).collect()),
    );
    if let Some(alongside) = alongside {
        run.insert("phaseAlongside".into(), alongside);
    }
    run
}

/// The "running at the same time" table: the index points at the accepted `phaseNames` entry.
fn read_phase_alongside(raw: Option<&Value>, count: usize) -> Option<Value> {
    let raw = raw?.as_array()?;
    let mut out: Vec<Value> = Vec::with_capacity(count);
    let mut any = false;
    for index in 0..count {
        let mut indexes: Vec<f64> = Vec::new();
        if let Some(Value::Array(entry)) = raw.get(index) {
            for value in entry {
                let Some(number) = value.as_f64() else {
                    continue;
                };
                if number.fract() != 0.0 {
                    continue;
                }
                if number < 0.0 || number >= count as f64 || number == index as f64 {
                    continue;
                }
                if indexes.contains(&number) {
                    continue;
                }
                indexes.push(number);
                if indexes.len() >= MAX_PHASES {
                    break;
                }
            }
        }
        if !indexes.is_empty() {
            any = true;
        }
        out.push(Value::Array(
            indexes.into_iter().map(number_value).collect(),
        ));
    }
    if any {
        Some(Value::Array(out))
    } else {
        None
    }
}

// ── run-settled ──

fn reduce_run_settled(mut run: Obj, payload: &Obj) -> Obj {
    let status = payload.get("status").cloned();
    let message = payload
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string);

    // The terminal state clears parked questions and the cooldown: by definition a terminal run has nobody listening and dispatches nothing more.
    run = without_pending_questions_all(run);
    run = without_cooldown(run);

    let carried_stop_reason = if status.as_ref().and_then(Value::as_str) == Some("stopped") {
        read_workflow_run_stop_reason(payload.get("stopReason"))
    } else {
        None
    };
    let superseded_by = if carried_stop_reason == Some("superseded") {
        read_run_id_field(payload.get("supersededBy"))
    } else {
        None
    };

    let terminal = matches!(
        status.as_ref().and_then(Value::as_str),
        Some("completed" | "errored" | "stopped")
    );
    // An unrecognized word lands in the terminal state as errored; the text is kept, and when absent it says which word it was.
    let settled_message = if terminal {
        message
    } else {
        message.or_else(|| {
            Some(format!(
                "run settled with an unrecognized status: {}",
                js_string(status.as_ref())
            ))
        })
    };

    run.insert(
        "status".into(),
        if terminal {
            status.unwrap_or(Value::String("errored".into()))
        } else {
            Value::String("errored".into())
        },
    );
    if let Some(stop_reason) = carried_stop_reason {
        run.insert("stopReason".into(), Value::String(stop_reason.to_string()));
    }
    if let Some(superseded_by) = superseded_by {
        run.insert("supersededBy".into(), Value::String(superseded_by));
    }
    if let Some(settled_message) = settled_message {
        run.insert(
            "error".into(),
            Value::String(js_slice(&settled_message, MAX_ERROR_LENGTH)),
        );
    }
    if payload.get("resumable") == Some(&Value::Bool(true)) {
        run.insert("resumable".into(), Value::Bool(true));
    }
    with_derived_actor_statuses(run)
}

fn without_pending_questions_all(mut run: Obj) -> Obj {
    let length = run
        .get("pendingQuestions")
        .and_then(Value::as_array)
        .map(Vec::len);
    // When `remaining.length === current.length` it returns unchanged: an empty table still keeps the key.
    if matches!(length, Some(length) if length > 0) {
        run.remove("pendingQuestions");
    }
    run
}

fn without_cooldown(mut run: Obj) -> Obj {
    if let Some(Value::Object(concurrency)) = run.get_mut("concurrency") {
        if concurrency.contains_key("cooldownMs") {
            concurrency.remove("cooldownMs");
        }
    }
    run
}

// ── The derived actor tri-state ──

fn with_derived_actor_statuses(mut run: Obj) -> Obj {
    let nodes = clone_array(run.get("nodes"));
    let mut executing: Vec<(String, f64)> = Vec::new();
    let mut live: Vec<(String, f64)> = Vec::new();
    let mut owned: Vec<(String, f64)> = Vec::new();
    for node in &nodes {
        let (Some(site_id), Some(ordinal)) = (
            node.get("actorSiteId").and_then(Value::as_str),
            node.get("actorOrdinal").and_then(Value::as_f64),
        ) else {
            continue;
        };
        let key = (site_id.to_string(), ordinal);
        push_unique(&mut owned, &key);
        match node.get("phase").and_then(Value::as_str) {
            Some("executing" | "repairing" | "nudged") => push_unique(&mut executing, &key),
            Some("queued" | "dispatched" | "waiting") => push_unique(&mut live, &key),
            _ => {}
        }
    }

    let run_live = matches!(
        run.get("status").and_then(Value::as_str),
        Some("pending" | "running")
    );
    if let Some(Value::Array(actors)) = run.get_mut("actors") {
        for actor in actors.iter_mut() {
            let (Some(site_id), Some(ordinal)) = (
                actor
                    .get("siteId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                actor.get("ordinal").and_then(Value::as_f64),
            ) else {
                continue;
            };
            let key = (site_id, ordinal);
            // The terminal state overrides everything: in a terminal run nobody is still running or waiting.
            let status = if !run_live {
                "completed"
            } else if contains(&executing, &key) {
                "running"
            } else if contains(&live, &key) || !contains(&owned, &key) {
                "waiting"
            } else {
                "completed"
            };
            if actor.get("status").and_then(Value::as_str) != Some(status) {
                if let Some(record) = actor.as_object_mut() {
                    record.insert("status".into(), Value::String(status.to_string()));
                }
            }
        }
    }
    run
}

fn push_unique(list: &mut Vec<(String, f64)>, key: &(String, f64)) {
    if !contains(list, key) {
        list.push(key.clone());
    }
}

fn contains(list: &[(String, f64)], key: &(String, f64)) -> bool {
    list.iter().any(|item| item.0 == key.0 && item.1 == key.1)
}

// ── Bounded upsert (three kinds of key) ──

/// Upsert by (siteId, ordinal). At the ceiling it **refuses new entries and still accepts updates to existing ones**.
fn upsert_by_instance(list: &mut Vec<Value>, entry: Value, limit: usize) -> bool {
    let site_id = entry
        .get("siteId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let ordinal = entry.get("ordinal").cloned();
    if let (Some(site_id), Some(ordinal)) = (site_id, ordinal) {
        if let Some(index) = list
            .iter()
            .position(|item| matches_instance(item, &site_id, &ordinal))
        {
            list[index] = entry;
            return false;
        }
    }
    if list.len() >= limit {
        return true;
    }
    list.push(entry);
    false
}

/// Upsert by `qid`: an escalation has no site-instance identity, so qid is its key.
fn upsert_by_qid(list: &mut Vec<Value>, entry: Value, limit: usize) -> bool {
    let qid = entry.get("qid").cloned();
    if let Some(qid) = qid {
        if let Some(index) = list.iter().position(|item| item.get("qid") == Some(&qid)) {
            list[index] = entry;
            return false;
        }
    }
    if list.len() >= limit {
        return true;
    }
    list.push(entry);
    false
}

/// Upsert by the **artifact id**, preserving the existing entry's `itemCount` (a refresh signal that must not be reset to zero).
fn upsert_by_artifact_id(list: &mut Vec<Value>, entry: Value, limit: usize) -> bool {
    let id = entry.get("id").cloned();
    if let Some(id) = id {
        if let Some(index) = list.iter().position(|item| item.get("id") == Some(&id)) {
            let previous_item_count = list[index].get("itemCount").cloned();
            let mut merged = entry;
            if let Some(item_count) = previous_item_count {
                if let Some(record) = merged.as_object_mut() {
                    record.insert("itemCount".into(), item_count);
                }
            }
            list[index] = merged;
            return false;
        }
    }
    if list.len() >= limit {
        return true;
    }
    list.push(entry);
    false
}

// ── Narrow readers ──

struct InstanceRef {
    site_id: String,
    ordinal: Value,
}

/// Isomorphic to the engine's `InstanceRef` / `ActorRef`: site id × ordinal. Missing either one makes location impossible.
fn instance_ref(value: Option<&Value>) -> Option<InstanceRef> {
    let record = value?.as_object()?;
    let site_id = non_empty_str(record.get("siteId"))?.to_string();
    let ordinal = record.get("ordinal")?;
    if !ordinal.is_number() {
        return None;
    }
    Some(InstanceRef {
        site_id,
        ordinal: ordinal.clone(),
    })
}

fn matches_instance(item: &Value, site_id: &str, ordinal: &Value) -> bool {
    let Some(record) = item.as_object() else {
        return false;
    };
    let site_matches = matches!(record.get("siteId"), Some(Value::String(text)) if text == site_id);
    let ordinal_matches = match (
        record.get("ordinal").and_then(Value::as_f64),
        ordinal.as_f64(),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    };
    site_matches && ordinal_matches
}

/// The run id field on the payload: only a non-empty string counts as present.
fn read_run_id_field(value: Option<&Value>) -> Option<String> {
    non_empty_str(value).map(str::to_string)
}

const STOP_REASONS: [&str; 5] = ["user", "model", "provider", "interrupted", "superseded"];

/// The stop reason on the payload: anything outside the whitelist reads as absent.
fn read_workflow_run_stop_reason(value: Option<&Value>) -> Option<&'static str> {
    let text = value?.as_str()?;
    STOP_REASONS.iter().copied().find(|reason| *reason == text)
}

// ── Small JS-semantics helpers ──

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn is_truncated(run: &Obj) -> bool {
    run.get("truncated").is_some_and(truthy)
}

fn clone_array(value: Option<&Value>) -> Vec<Value> {
    value.and_then(Value::as_array).cloned().unwrap_or_default()
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Some(text.as_str()),
        _ => None,
    }
}

fn is_js_integer(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|number| number.is_finite() && number.fract() == 0.0)
}

/// `Number.isInteger(value) && value > 0`。
fn positive_integer(value: Option<&Value>) -> Option<f64> {
    let number = value?.as_f64()?;
    if number.is_finite() && number.fract() == 0.0 && number > 0.0 {
        Some(number)
    } else {
        None
    }
}

/// `Number.isInteger(value) && value >= 0`。
fn non_negative_integer(value: Option<&Value>) -> Option<f64> {
    let number = value?.as_f64()?;
    if number.is_finite() && number.fract() == 0.0 && number >= 0.0 {
        Some(number)
    } else {
        None
    }
}

/// JS number → JSON number: an integer is written as an integer (`JSON.stringify(1.0) === "1"`).
fn number_value(number: f64) -> Value {
    if number.is_finite()
        && number.fract() == 0.0
        && number >= i64::MIN as f64
        && number <= i64::MAX as f64
    {
        Value::from(number as i64)
    } else {
        Number::from_f64(number).map_or(Value::Null, Value::Number)
    }
}

fn usage_object(spent_tokens: f64, nodes_used: f64) -> Value {
    let mut usage = Obj::new();
    usage.insert("spentTokens".into(), number_value(spent_tokens));
    usage.insert("nodesUsed".into(), number_value(nodes_used));
    Value::Object(usage)
}

/// JS string length: the number of UTF-16 code units (`.length`).
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// JS `slice(0, limit)`: cut by UTF-16 code units.
///
/// When the cut lands in the middle of a surrogate pair, TS leaves a lone surrogate behind (`JSON.stringify` emits a `\udXXX` escape),
/// while neither Rust's `String` nor the JSON parser can carry a lone surrogate. So this cuts one code unit short: the result is always
/// valid UTF-8 with a length at most limit, costing only one code unit less than TS on this pathological input.
fn js_slice(text: &str, limit: usize) -> String {
    if js_len(text) <= limit {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let width = character.len_utf16();
        if used + width > limit {
            break;
        }
        out.push(character);
        used += width;
    }
    out
}

/// The display ceiling: the ellipsis makes the truncation visible.
fn bounded_question_text(text: &str) -> String {
    if js_len(text) <= MAX_QUESTION_LENGTH {
        text.to_string()
    } else {
        let mut out = js_slice(text, MAX_QUESTION_LENGTH - 1);
        out.push('…');
        out
    }
}

/// Trim to the wire ceiling, with an empty string treated as absent (truncate directly, no ellipsis added).
fn bounded_text(value: Option<&Value>, limit: usize) -> Option<String> {
    let text = js_slice(value?.as_str()?, limit);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The whitespace set of JS `String.prototype.trim` (Unicode WhiteSpace + LineTerminator, including U+FEFF,
/// excluding the U+0085 that Rust's `trim` does strip).
fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    // The Unicode Zs run U+2000..U+200A is checked separately: putting it in an or-pattern makes rustfmt lay it out unreadably.
    if ('\u{2000}'..='\u{200A}').contains(&character) {
        return true;
    }
    matches!(
        character,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// JS `String(value)`, used only for that "unrecognized status" text of `run-settled`.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Number(number)) => match number.as_f64() {
            Some(number) if number.fract() == 0.0 && number.abs() < 1e21 => {
                format!("{}", number as i64)
            }
            Some(number) => format!("{number}"),
            None => number.to_string(),
        },
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture_cases() -> Vec<Value> {
        let raw = include_str!("../tests/fixtures/workflow_runs_fixtures.json");
        serde_json::from_str::<Value>(raw)
            .expect("fixture JSON parses")
            .as_array()
            .expect("fixture is an array")
            .clone()
    }

    #[test]
    fn fixtures_match_typescript_reducer() {
        let cases = fixture_cases();
        assert!(
            cases.len() >= 250,
            "expected the full generated corpus, got {} cases",
            cases.len()
        );
        for case in &cases {
            let name = case
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unnamed>");
            let previous = case.get("previous").filter(|value| !value.is_null());
            let expected = case.get("expected").filter(|value| !value.is_null());
            let envelope = case.get("envelope").expect("envelope present");
            let actual = reduce_workflow_runs(previous, envelope);
            match (&actual, expected) {
                (None, None) => {}
                (Some(actual), Some(expected)) => assert_eq!(
                    actual, expected,
                    "case {name} diverged from the TypeScript reducer"
                ),
                (actual, expected) => panic!("case {name}: expected {expected:?}, got {actual:?}"),
            }
        }
    }

    #[test]
    fn invalid_envelopes_return_none() {
        let state = json!({ "revision": 3, "runs": [] });
        for envelope in [
            json!({}),
            json!({ "eventType": "run-started" }),
            json!({ "runId": "", "eventType": "run-started" }),
            json!({ "runId": 7, "eventType": "run-started" }),
            json!({ "runId": "r1" }),
            json!({ "runId": "r1", "eventType": 5 }),
            json!({ "runId": "r1", "eventType": null }),
            json!("not an object"),
            json!([1, 2]),
            Value::Null,
        ] {
            assert_eq!(reduce_workflow_runs(Some(&state), &envelope), None);
            assert_eq!(reduce_workflow_runs(None, &envelope), None);
        }
    }

    #[test]
    fn utf16_truncation_counts_code_units() {
        // Unit level: ASCII and astral characters are both counted in UTF-16 code units.
        assert_eq!(js_len("abc"), 3);
        assert_eq!(js_len("😀"), 2);
        assert_eq!(js_slice("abcdef", 4), "abcd");
        assert_eq!(js_slice("😀😀", 2), "😀");
        assert_eq!(js_slice("😀😀", 3), "😀");

        // The cut lands inside a surrogate pair: TS would leave a lone surrogate, Rust cuts one code unit short (see the js_slice doc).
        let split = format!("a{}", "😀".repeat(64));
        assert_eq!(js_len(&split), 129);
        let truncated = js_slice(&split, MAX_ACTOR_NAME_LENGTH);
        assert_eq!(truncated, format!("a{}", "😀".repeat(63)));
        assert_eq!(js_len(&truncated), 127);

        // End to end: the actor name is trimmed to 128 UTF-16 code units (100 emoji = 200 code units).
        let envelope = json!({
            "runId": "r1",
            "eventType": "actor-created",
            "sequence": 1,
            "payload": {
                "actor": { "siteId": "agent#1", "ordinal": 0 },
                "name": "😀".repeat(100),
            },
        });
        let state = reduce_workflow_runs(None, &envelope).expect("state produced");
        let name = state["runs"][0]["actors"][0]["name"].as_str().unwrap();
        assert_eq!(name, "😀".repeat(64));
        assert_eq!(js_len(name), 128);
    }

    #[test]
    fn js_trim_matches_javascript_whitespace() {
        // JS's WhiteSpace contains U+FEFF and not U+0085; Rust's `str::trim` is exactly the opposite.
        assert_eq!(js_trim("\u{FEFF} value \u{00A0}"), "value");
        assert_eq!(js_trim("\u{0085}value\u{0085}"), "\u{0085}value\u{0085}");
        assert_eq!(js_trim(" \t\n value \r\n"), "value");
        assert_eq!(js_trim("a b"), "a b");
        assert_eq!(js_trim("\u{2000}\u{200A}\u{3000}"), "");
    }

    #[test]
    fn unknown_events_only_advance_the_watermark() {
        let started = reduce_workflow_runs(
            None,
            &json!({ "runId": "r1", "eventType": "run-started", "sequence": 4, "payload": {} }),
        )
        .unwrap();
        let advanced = reduce_workflow_runs(
            Some(&started),
            &json!({ "runId": "r1", "eventType": "log", "sequence": 9, "payload": { "line": "hi" } }),
        )
        .expect("watermark moved");
        assert_eq!(advanced["revision"], json!(2));
        assert_eq!(advanced["runs"][0]["lastEventSequence"], json!(9));
        // Replaying the same event: the watermark does not move and there is no semantic change.
        assert_eq!(
            reduce_workflow_runs(
                Some(&advanced),
                &json!({ "runId": "r1", "eventType": "log", "sequence": 9, "payload": { "line": "hi" } }),
            ),
            None
        );
    }
}
