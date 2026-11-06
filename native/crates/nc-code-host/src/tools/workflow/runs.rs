//! The run observation surface and the saved manifest: ListWorkflowRuns / GetWorkflowRun / ResumeWorkflowRun /
//! ResolveWorkflowQuestion / ListSavedWorkflows.
//!
//! This group of tools is deliberately **thin**: labels, status composition, timestamps and artifact merging are cooked by the run service; here only the four things
//! the port cannot do — normalise an unknown runId / unknown qid into a structured failure, read the clock exactly once per call
//! (`generatedAt` is the only ruler for every "how long ago"), project journal events into the roster and the log tail,
//! and hand the server's refusal wording to the model verbatim (the discriminator key and the wording are maintained separately; the two will eventually say different things).

use std::collections::BTreeMap;

use nc_code_workflow::types::{
    NodeRecord, NodeRecordStatus, RunEvent, RunStatus, RunStopReason, StoredEvent,
};
use serde_json::{json, Value};

use crate::tools::workflow::support::{
    self, describe_script_path, format_relative_age, serialize_artifact,
};
use crate::workflow_service::{
    LabelSource, QuestionRefusal, ResolveQuestionResult, RunListItem, RunSnapshot,
    WorkflowServiceError,
};

use super::support::{capability_unavailable, invalid_input};
use super::{
    WorkflowToolPort, GET_WORKFLOW_RUN_TOOL_NAME, LIST_SAVED_WORKFLOWS_TOOL_NAME,
    LIST_WORKFLOW_RUNS_TOOL_NAME, RESOLVE_WORKFLOW_QUESTION_TOOL_NAME,
    RESUME_WORKFLOW_RUN_TOOL_NAME,
};
use crate::tools::{
    require_string, AsyncTool, Tool, ToolContext, ToolContract, ToolError, SCOPE_NONE,
};

const OBSERVATION_TIMEOUT_MS: u64 = 10_000;
const RESUME_TIMEOUT_MS: u64 = 15_000;
const ASK_TIMEOUT_MS: u64 = 15_000;
const OBSERVATION_MODEL_BYTES: usize = 24_000;
/// Follow TaskOutput: an artifact is exactly the kind of payload that can grow large enough to need an artifact.
const GET_WORKFLOW_RUN_MODEL_BYTES: usize = 400_000;
/// The number of log-tail entries (the same value as TS port `DEFAULT_LOG_TAIL_LIMIT`).
const LOG_TAIL_LIMIT: usize = 20;
/// How many event pages one detail read may flip through at most (1000 per page). The roster / log tail are both projected from this stream; for a very
/// long run that exceeds it only the oldest observation events are dropped, while the run rows and node rows stay complete.
const MAX_EVENT_PAGES: usize = 10;
/// The ceiling for the roster / phase table (the same value as TS `GET_WORKFLOW_RUN_ROSTER_LIMITS`).
const MAX_SUBAGENTS: usize = 64;
const SUMMARY_MAX_CHARS: usize = 400;

const WORKFLOW_RUN_LIFECYCLE_STATUSES: [&str; 5] =
    ["completed", "errored", "pending", "running", "stopped"];

fn lifecycle_status(row: &RunListItem) -> &'static str {
    match row.run_status {
        Some(RunStatus::Completed) => "completed",
        Some(RunStatus::Errored) => "errored",
        Some(RunStatus::Stopped) => "stopped",
        // The snapshot vocabulary folds both pending / running into "still running"; TS's pending only lives in the microsecond window
        // between createRun and the first write, and on the read surface it is indistinguishable from running.
        _ => "running",
    }
}

fn stop_reason_str(reason: Option<RunStopReason>) -> Option<&'static str> {
    reason.map(|reason| reason.as_str())
}

fn owned_by_session(row: &RunListItem, session_id: &str) -> bool {
    row.parent_session_id.as_deref() == Some(session_id)
}

/// The introspection capability is absent (the journal could not be opened): this and "this project never ran a workflow" must be said as two different things.
fn introspection_failure(reason: String) -> ToolError {
    capability_unavailable(format!(
        "workflow_introspection_unavailable: this session cannot read workflow runs — {reason}. This is a capability gap, not an empty project."
    ))
}

/// This session cannot prove it is still alive: the journal says it is running but this service has no engine for it. The status is never rewritten.
fn possibly_interrupted(row: &RunListItem) -> bool {
    lifecycle_status(row) == "running" && !row.live
}

// ————————————————————————————————————————————————————————————————
// ListWorkflowRuns
// ————————————————————————————————————————————————————————————————

const LIST_WORKFLOW_RUNS_DESCRIPTION: &str = "Lists this project's dynamic-workflow runs (the session's working directory is the project key), most recently updated first. Includes runs started by other sessions. Each row gives the run ID, label, lifecycle status, whether this session started it, tokens spent and timestamps. Pass a run ID to GetWorkflowRun for detail. Any run — completed, stopped, errored, or still running — can be revised with AmendWorkflow; a stopped run can be resumed with ResumeWorkflowRun.";

fn list_workflow_runs_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 20, "description": "Maximum number of runs to return (1-50, default 20). Most recently updated runs come first."},
            "statuses": {
                "type": "array",
                "items": {"type": "string", "enum": WORKFLOW_RUN_LIFECYCLE_STATUSES},
                "description": "Optional status filter. Omit to see every run in this project."
            }
        },
        "additionalProperties": false
    })
}

fn list_workflow_runs_contract() -> ToolContract {
    ToolContract {
        name: String::new(),
        description: LIST_WORKFLOW_RUNS_DESCRIPTION.to_string(),
        input_schema: list_workflow_runs_schema(),
        read_only: true,
        destructive: false,
        concurrent_safe: true,
        side_effect_scope: SCOPE_NONE,
        timeout_ms: OBSERVATION_TIMEOUT_MS,
        max_output_bytes: OBSERVATION_MODEL_BYTES,
        max_model_bytes: OBSERVATION_MODEL_BYTES,
        preview_direction: "head",
    }
}

pub struct ListWorkflowRunsTool;

impl Tool for ListWorkflowRunsTool {
    fn contract(&self) -> ToolContract {
        // Writing the tool name at this position is load-bearing: `check-tool-parity` reads it out of the `impl` body.
        ToolContract {
            name: LIST_WORKFLOW_RUNS_TOOL_NAME.to_string(),
            ..list_workflow_runs_contract()
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let Some(port) = ctx.workflow.as_deref() else {
            return Err(support::workflow_introspection_unavailable());
        };
        let limit = match input.get("limit") {
            None | Some(Value::Null) => 20usize,
            Some(Value::Number(number)) => number
                .as_u64()
                .map(|value| usize::try_from(value).unwrap_or(20).clamp(1, 50))
                .ok_or_else(|| invalid_input("limit: must be an integer"))?,
            Some(_) => return Err(invalid_input("limit: must be an integer")),
        };
        let statuses: Option<Vec<&str>> = match input.get("statuses") {
            None | Some(Value::Null) => None,
            Some(Value::Array(values)) => {
                let mut filter = Vec::new();
                for value in values {
                    let raw = value
                        .as_str()
                        .ok_or_else(|| invalid_input("statuses: must be an array of strings"))?;
                    if !WORKFLOW_RUN_LIFECYCLE_STATUSES.contains(&raw) {
                        return Err(invalid_input(format!(
                            "statuses: unknown lifecycle status {raw}"
                        )));
                    }
                    filter.push(raw);
                }
                Some(filter)
            }
            Some(_) => return Err(invalid_input("statuses: must be an array of strings")),
        };

        // Fetch one extra row to detect truncation (`len == limit` would report it wrongly). With a status filter the window is opened to the ceiling: filtering happens
        // within the page, so a too-narrow window would report "there are matches but all on the next page" as none.
        let fetch = if statuses.is_some() { 51 } else { limit + 1 };
        let rows = port
            .list_runs(None, Some(fetch))
            .map_err(introspection_failure)?;
        let raw_len = rows.len();
        let filtered: Vec<&RunListItem> = rows
            .iter()
            .filter(|row| match &statuses {
                None => true,
                Some(statuses) => statuses.contains(&lifecycle_status(row)),
            })
            .collect();
        let truncated = raw_len > limit || filtered.len() > limit;
        let runs: Vec<Value> = filtered
            .iter()
            .take(limit)
            .map(|row| {
                let mut object = serde_json::Map::new();
                object.insert("runId".into(), json!(row.run_id));
                object.insert("label".into(), json!(row.label));
                object.insert(
                    "labelSource".into(),
                    json!(match row.label_source {
                        LabelSource::Name => "name",
                        LabelSource::Script => "script",
                    }),
                );
                object.insert("status".into(), json!(lifecycle_status(row)));
                if let Some(reason) = stop_reason_str(row.stop_reason) {
                    object.insert("stopReason".into(), json!(reason));
                }
                if let Some(resumed_from) = &row.resumed_from {
                    object.insert("resumedFrom".into(), json!(resumed_from));
                }
                if let Some(superseded_by) = &row.superseded_by {
                    object.insert("supersededBy".into(), json!(superseded_by));
                }
                object.insert(
                    "ownedByThisSession".into(),
                    json!(owned_by_session(row, &ctx.session_id)),
                );
                if possibly_interrupted(row) {
                    object.insert("possiblyInterrupted".into(), json!(true));
                }
                object.insert("createdAt".into(), json!(row.created_at));
                object.insert("updatedAt".into(), json!(row.updated_at));
                object.insert("spentTokens".into(), json!(row.spent_tokens));
                Value::Object(object)
            })
            .collect();
        let mut output = serde_json::Map::new();
        output.insert("runs".into(), Value::Array(runs));
        if truncated {
            output.insert("truncated".into(), json!(true));
        }
        Ok(Value::Object(output).to_string())
    }
    /// The list card's display: the row fields are identical field by field to `ListWorkflowRunsRunSchema` (runId/label/
    /// labelSource/status/…/spentTokens), and the output is already cut by limit 50, so it is passed straight through;
    /// `truncated` only enters the payload when real truncation happened (the schema marks it optional).
    fn result_display_for(&self, _input: &Value, output_text: &str) -> Option<Value> {
        let output: Value = serde_json::from_str(output_text).ok()?;
        let runs = output.get("runs")?.as_array()?;
        let mut display = json!({"kind": "list_workflow_runs", "runs": runs});
        if output.get("truncated").and_then(Value::as_bool) == Some(true) {
            display["truncated"] = json!(true);
        }
        Some(display)
    }
}

// ————————————————————————————————————————————————————————————————
// GetWorkflowRun
// ————————————————————————————————————————————————————————————————

const GET_WORKFLOW_RUN_DESCRIPTION: &str = "Returns the current state of one dynamic-workflow run: progress, token usage and the tail of its log() narration while it runs; the final result once it completed; the structured failure if it errored or was stopped. This is an instant snapshot and never waits (use TaskOutput to block on a run this session started). The `artifacts` section lists what the run published for the user — refer to one by its title rather than pasting its contents. A stopped run (other than a superseded one) can be continued with ResumeWorkflowRun; any run can be revised with AmendWorkflow.";

fn get_workflow_run_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {"type": "string", "minLength": 1, "description": "The workflow run ID to inspect (as returned by CreateWorkflow or ListWorkflowRuns)"}
        },
        "required": ["run_id"],
        "additionalProperties": false
    })
}

fn get_workflow_run_contract() -> ToolContract {
    ToolContract {
        name: String::new(),
        description: GET_WORKFLOW_RUN_DESCRIPTION.to_string(),
        input_schema: get_workflow_run_schema(),
        read_only: true,
        destructive: false,
        concurrent_safe: true,
        side_effect_scope: SCOPE_NONE,
        timeout_ms: OBSERVATION_TIMEOUT_MS,
        max_output_bytes: GET_WORKFLOW_RUN_MODEL_BYTES,
        max_model_bytes: GET_WORKFLOW_RUN_MODEL_BYTES,
        preview_direction: "head",
    }
}

/// One projection of the event stream: the roster and the log tail both come from here (the events are scanned only once).
#[derive(Default)]
struct EventProjection {
    /// `(siteId, ordinal)` → (name, phase), ordered by first appearance (ActorCreated).
    actors: BTreeMap<(String, u32), (Option<String>, Option<String>)>,
    logs: Vec<Value>,
    cached_steps: u64,
    /// The node instance's stat (`siteId@ordinal`) → queue/progress/wait facts.
    asks: BTreeMap<String, AskFacts>,
    /// Each actor's last progress moment.
    progress_at: BTreeMap<(String, u32), f64>,
    first_at: Option<f64>,
    last_at: Option<f64>,
}

#[derive(Default, Clone)]
struct AskFacts {
    actor: Option<(String, u32)>,
    site_id: Option<String>,
    ordinal: Option<u32>,
    actor_seq: Option<u64>,
    instructions_head: Option<String>,
    started_at: Option<f64>,
    turn: Option<u64>,
    tool_calls: Option<u64>,
    last_tool: Option<Value>,
    wait: Option<Value>,
}

fn actor_key(site_id: &str, ordinal: u32) -> (String, u32) {
    (site_id.to_string(), ordinal)
}

fn instance_key(site_id: &str, ordinal: u32) -> String {
    format!("{site_id}@{ordinal}")
}

fn project_events(events: &[StoredEvent]) -> EventProjection {
    let mut projection = EventProjection::default();
    for stored in events {
        if let Some(at) = stored.time_created {
            projection.first_at.get_or_insert(at);
            projection.last_at = Some(at);
        }
        let at = stored.time_created;
        match &stored.event {
            RunEvent::ActorCreated { actor, name, .. } => {
                let key = actor_key(&actor.site_id, actor.ordinal);
                let entry = projection.actors.entry(key).or_default();
                if name.is_some() {
                    entry.0 = name.clone();
                }
            }
            RunEvent::NodeQueued {
                instance,
                actor,
                actor_seq,
                phase_name,
                instructions_head,
                ..
            } => {
                if let Some(actor) = actor {
                    let key = actor_key(&actor.site_id, actor.ordinal);
                    if let Some(phase_name) = phase_name {
                        let entry = projection.actors.entry(key).or_default();
                        if entry.1.is_none() {
                            entry.1 = Some(phase_name.clone());
                        }
                    }
                }
                let facts = projection
                    .asks
                    .entry(instance_key(&instance.site_id, instance.ordinal))
                    .or_default();
                facts.actor = actor
                    .as_ref()
                    .map(|actor| actor_key(&actor.site_id, actor.ordinal));
                facts.site_id = Some(instance.site_id.clone());
                facts.ordinal = Some(instance.ordinal);
                facts.actor_seq = *actor_seq;
                facts.instructions_head = instructions_head.clone();
                facts.started_at = at;
            }
            RunEvent::NodeWaiting {
                instance,
                cause,
                reason,
                retry_after_ms,
                ..
            } => {
                let facts = projection
                    .asks
                    .entry(instance_key(&instance.site_id, instance.ordinal))
                    .or_default();
                let mut wait = serde_json::Map::new();
                wait.insert(
                    "cause".into(),
                    json!(match cause {
                        nc_code_workflow::types::AskWaitCause::Slot => "slot",
                        nc_code_workflow::types::AskWaitCause::Backoff => "backoff",
                    }),
                );
                if let Some(reason) = reason {
                    wait.insert("reason".into(), json!(reason));
                }
                if let Some(retry_after_ms) = retry_after_ms {
                    wait.insert("retryAfterMs".into(), json!(retry_after_ms));
                }
                if let Some(at) = at {
                    wait.insert("since".into(), json!(at));
                }
                facts.wait = Some(Value::Object(wait));
                if let Some(actor) = facts.actor.clone() {
                    if let Some(at) = at {
                        projection.progress_at.insert(actor, at);
                    }
                }
            }
            RunEvent::NodeExecuting { instance } => {
                if let Some(facts) = projection
                    .asks
                    .get(&instance_key(&instance.site_id, instance.ordinal))
                {
                    if let Some(actor) = facts.actor.clone() {
                        if let Some(at) = at {
                            projection.progress_at.insert(actor, at);
                        }
                    }
                }
            }
            RunEvent::NodeProgress {
                instance,
                turn,
                tool_calls,
                last_tool,
            } => {
                let facts = projection
                    .asks
                    .entry(instance_key(&instance.site_id, instance.ordinal))
                    .or_default();
                facts.turn = Some(*turn);
                facts.tool_calls = Some(*tool_calls);
                facts.last_tool = last_tool
                    .as_ref()
                    .map(|tool| json!({"name": tool.name, "target": tool.target, "at": at}));
                if let Some(actor) = facts.actor.clone() {
                    if let Some(at) = at {
                        projection.progress_at.insert(actor, at);
                    }
                }
            }
            RunEvent::NodeSettled {
                instance, cached, ..
            } => {
                if *cached == Some(true) {
                    projection.cached_steps += 1;
                }
                if let Some(facts) = projection
                    .asks
                    .get(&instance_key(&instance.site_id, instance.ordinal))
                {
                    if let Some(actor) = facts.actor.clone() {
                        if let Some(at) = at {
                            projection.progress_at.insert(actor, at);
                        }
                    }
                }
            }
            RunEvent::Log { message } => {
                projection.logs.push(json!({
                    "sequence": stored.sequence,
                    "message": message,
                    "at": stored.time_created,
                }));
            }
            _ => {}
        }
    }
    if projection.logs.len() > LOG_TAIL_LIMIT {
        let skip = projection.logs.len() - LOG_TAIL_LIMIT;
        projection.logs.drain(..skip);
    }
    projection
}

/// All the facts of one detail read (shared by the Get tool and its summary, so the two do not each compute their own version).
struct RunDetail {
    run_id: String,
    label: String,
    label_source: &'static str,
    status: &'static str,
    stop_reason: Option<&'static str>,
    resumed_from: Option<String>,
    superseded_by: Option<String>,
    max_concurrency: Option<u32>,
    subagent_model: Option<String>,
    script_path: Option<String>,
    owned_by_this_session: bool,
    possibly_interrupted: bool,
    created_at: f64,
    updated_at: f64,
    generated_at: f64,
    usage: Usage,
    actors: Vec<Value>,
    log_tail: Vec<Value>,
    subagents: Vec<Value>,
    subagents_truncated: bool,
    /// The phase roster (from the session's `workflowRuns` reducer state; None when no phase is declared/entered).
    phases: Option<Vec<Value>>,
    health: Value,
    result: Option<String>,
    error: Option<Value>,
    pending_questions: Vec<Value>,
    artifacts: Vec<Value>,
    /// The summary projection of health.pendingQuestionsKnown (`questionsClause` reads it).
    pending_questions_known: bool,
}

struct Usage {
    spent_tokens: u64,
    observed: u32,
    running: u32,
    completed: u32,
    failed: u32,
}

fn node_key(node: &NodeRecord) -> Option<(String, u32)> {
    match (&node.actor_site_id, node.actor_ordinal) {
        (Some(site), Some(ordinal)) => Some((site.clone(), ordinal)),
        _ => None,
    }
}

fn build_subagents(
    snapshot: &RunSnapshot,
    nodes: &[NodeRecord],
    projection: &EventProjection,
    live: bool,
) -> (Vec<Value>, bool) {
    // The identity table's order: the minting order of ActorCreated, then actors that only appear in node rows are appended (old journals).
    let mut order: Vec<(String, u32)> = projection.actors.keys().cloned().collect();
    for node in nodes {
        if let Some(key) = node_key(node) {
            if !order.contains(&key) {
                order.push(key);
            }
        }
    }
    let mut roster = Vec::new();
    for key in &order {
        let mut settled = 0u64;
        let mut failed = 0u64;
        let mut running = 0u64;
        let mut tokens = 0u64;
        let phase_name = projection
            .actors
            .get(key)
            .and_then(|(_, phase)| phase.clone());
        let mut current_ask = None;
        let mut wait = None;
        for node in nodes {
            if node_key(node).as_ref() != Some(key) {
                continue;
            }
            match node.status {
                NodeRecordStatus::Completed => settled += 1,
                NodeRecordStatus::Failed => failed += 1,
                NodeRecordStatus::Running => running += 1,
            }
            if let Some(stats) = &node.stats {
                tokens += stats.tokens;
            }
            if node.status == NodeRecordStatus::Running {
                if let Some(facts) = projection
                    .asks
                    .get(&instance_key(&node.site_id, node.ordinal))
                {
                    let mut ask = serde_json::Map::new();
                    ask.insert("siteId".into(), json!(node.site_id));
                    ask.insert("ordinal".into(), json!(node.ordinal));
                    if let Some(actor_seq) = facts.actor_seq {
                        ask.insert("actorSeq".into(), json!(actor_seq));
                    }
                    if let Some(instructions_head) = &facts.instructions_head {
                        ask.insert("instructionsHead".into(), json!(instructions_head));
                    }
                    if let Some(started_at) = facts.started_at {
                        ask.insert("startedAt".into(), json!(started_at));
                    }
                    if let Some(turn) = facts.turn {
                        ask.insert("turn".into(), json!(turn));
                    }
                    if let Some(tool_calls) = facts.tool_calls {
                        ask.insert("toolCalls".into(), json!(tool_calls));
                    }
                    if let Some(last_tool) = &facts.last_tool {
                        ask.insert("lastTool".into(), last_tool.clone());
                    }
                    current_ask = Some(Value::Object(ask));
                    wait = facts.wait.clone();
                }
            }
        }
        let parked_on = snapshot
            .pending_questions
            .iter()
            .find(|question| question.actor == instance_key(&key.0, key.1))
            .map(|question| question.qid.clone());
        let state = if parked_on.is_some() {
            "parked"
        } else if live {
            if running > 0 {
                "executing"
            } else {
                "idle"
            }
        } else if running > 0 {
            "unfinished"
        } else if failed > 0 {
            "failed"
        } else {
            "done"
        };
        let mut entry = serde_json::Map::new();
        entry.insert("siteId".into(), json!(key.0));
        entry.insert("ordinal".into(), json!(key.1));
        if let Some(name) = projection
            .actors
            .get(key)
            .and_then(|(name, _)| name.clone())
        {
            entry.insert("name".into(), json!(name));
        }
        entry.insert("state".into(), json!(state));
        if let Some(phase_name) = phase_name {
            entry.insert("phaseName".into(), json!(phase_name));
        }
        if let Some(current_ask) = current_ask {
            entry.insert("currentAsk".into(), current_ask);
        }
        if let Some(wait) = wait.filter(|wait| !wait.is_null()) {
            entry.insert("wait".into(), wait);
        }
        if let Some(parked_on) = parked_on {
            entry.insert("parkedOn".into(), json!(parked_on));
        }
        entry.insert("stepsSettled".into(), json!(settled));
        entry.insert("stepsFailed".into(), json!(failed));
        entry.insert("tokens".into(), json!(tokens));
        if let Some(last_progress) = projection.progress_at.get(key) {
            entry.insert("lastProgressAt".into(), json!(last_progress));
        }
        roster.push(Value::Object(entry));
    }
    let truncated = roster.len() > MAX_SUBAGENTS;
    roster.truncate(MAX_SUBAGENTS);
    (roster, truncated)
}

/// The phase roster: projected from the session's `workflowRuns` reducer state (the same state as the UI's phase timeline).
///
/// The state words match TS `buildPhaseViews`: `ahead` (declared, never entered), `current` (the live run's current
/// phase), `done` (walked through with nothing left unfinished), `unfinished` (a terminal run still has unsettled nodes other than world-reads).
/// The moments (enteredAt/exitedAt) are not in the reducer state, so they stay absent — never substituted with 0.
fn build_phases(state: Option<&Value>, run_id: &str, terminal: bool) -> Option<Vec<Value>> {
    let run = state?
        .get("runs")?
        .as_array()?
        .iter()
        .find(|run| run.get("runId").and_then(Value::as_str) == Some(run_id))?;
    let declared = run
        .get("phaseNames")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entered = run
        .get("phases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if declared.is_empty() && entered.is_empty() {
        return None;
    }
    let mut names: Vec<String> = declared
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let mut seen: std::collections::HashSet<String> = names.iter().cloned().collect();
    for phase in &entered {
        if let Some(name) = phase.get("name").and_then(Value::as_str) {
            if seen.insert(name.to_string()) {
                names.push(name.to_string());
            }
        }
    }
    let current = run.get("currentPhase").and_then(Value::as_str);
    let nodes = run
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let rows = names
        .iter()
        .map(|name| {
            let rounds = entered
                .iter()
                .find(|phase| phase.get("name").and_then(Value::as_str) == Some(name.as_str()))
                .and_then(|phase| phase.get("rounds").and_then(Value::as_u64))
                .unwrap_or(0);
            let owned: Vec<&Value> = nodes
                .iter()
                .filter(|node| node.get("phaseName").and_then(Value::as_str) == Some(name))
                .collect();
            let settled = owned
                .iter()
                .filter(|node| node.get("phase").and_then(Value::as_str) == Some("settled"))
                .count();
            let running = owned.len() - settled;
            let state = if rounds == 0 {
                "ahead"
            } else if terminal {
                let leftover = owned.iter().any(|node| {
                    node.get("phase").and_then(Value::as_str) != Some("settled")
                        && node.get("kind").and_then(Value::as_str) != Some("world-read")
                });
                if leftover {
                    "unfinished"
                } else {
                    "done"
                }
            } else if current == Some(name.as_str()) {
                "current"
            } else {
                "done"
            };
            json!({
                "name": name,
                "state": state,
                "rounds": rounds,
                "nodesSettled": settled,
                "nodesRunning": running,
            })
        })
        .collect();
    Some(rows)
}

fn read_run_detail(
    port: &dyn WorkflowToolPort,
    run_id: &str,
    session_id: &str,
    cwd: &std::path::Path,
    ceiling: Option<u32>,
) -> Result<RunDetail, ToolError> {
    let Some(snapshot) = port.get_run(run_id).map_err(introspection_failure)? else {
        return Err(support::run_not_found(run_id));
    };
    let live = port.is_live(run_id);
    let row = port.run_row(run_id).map_err(introspection_failure)?;
    let nodes = port.run_nodes(run_id).map_err(introspection_failure)?;
    // Flip event pages to the end (bounded): the roster and the log tail are both projected from this stream.
    let mut events: Vec<StoredEvent> = Vec::new();
    let mut after: Option<u64> = None;
    for _ in 0..MAX_EVENT_PAGES {
        let page = port
            .list_events(run_id, after, Some(1000))
            .map_err(introspection_failure)?;
        let has_more = page.has_more;
        after = page.next_sequence.or(after);
        events.extend(page.events);
        if !has_more {
            break;
        }
    }
    let projection = project_events(&events);
    let generated_at = crate::store::now_ms() as f64;
    let status = match &snapshot.run_status {
        Some(RunStatus::Completed) => "completed",
        Some(RunStatus::Errored) => "errored",
        Some(RunStatus::Stopped) => "stopped",
        _ => "running",
    };
    let stop_reason = snapshot.stop_reason.map(|reason| reason.as_str());
    let (label, label_source, created_at, updated_at) = match &row {
        Some(row) => (
            row.label.clone(),
            match row.label_source {
                LabelSource::Name => "name",
                LabelSource::Script => "script",
            },
            row.created_at,
            row.updated_at,
        ),
        None => (
            run_id.to_string(),
            "script",
            projection.first_at.unwrap_or(generated_at),
            projection.last_at.unwrap_or(generated_at),
        ),
    };
    let owned_by_this_session = snapshot.parent_session_id.as_deref() == Some(session_id);
    let possibly_interrupted = status == "running" && !live;
    let terminal = matches!(status, "completed" | "errored" | "stopped");
    let pending_questions_known = live || terminal;
    let usage = Usage {
        spent_tokens: snapshot.spent_tokens,
        observed: snapshot.nodes.observed(),
        running: snapshot.nodes.running,
        completed: snapshot.nodes.completed,
        failed: snapshot.nodes.failed,
    };
    let (subagents, subagents_truncated) = build_subagents(&snapshot, &nodes, &projection, live);
    let phases = build_phases(
        port.workflow_runs_state(session_id).as_ref(),
        run_id,
        terminal,
    );
    let actors: Vec<Value> = projection
        .actors
        .iter()
        .map(|((site_id, ordinal), (name, _))| {
            json!({"siteId": site_id, "ordinal": ordinal, "name": name})
        })
        .collect();
    let pending_questions: Vec<Value> = snapshot
        .pending_questions
        .iter()
        .map(|question| {
            let mut entry = serde_json::Map::new();
            entry.insert("qid".into(), json!(question.qid));
            entry.insert("actor".into(), json!(question.actor));
            if let Some(actor_name) = &question.actor_name {
                entry.insert("actorName".into(), json!(actor_name));
            }
            entry.insert("question".into(), json!(question.question));
            if let Some(context) = &question.context {
                entry.insert("context".into(), json!(context));
            }
            entry.insert("askedAt".into(), json!(question.asked_at));
            Value::Object(entry)
        })
        .collect();
    let artifacts: Vec<Value> = snapshot
        .artifacts
        .iter()
        .map(|artifact| {
            let latest = artifact.versions.last();
            let mut object = serde_json::Map::new();
            object.insert("id".into(), json!(artifact.id));
            object.insert("kind".into(), json!(artifact.kind.as_str()));
            if let Some(title) = &artifact.title {
                object.insert("title".into(), json!(title));
            }
            object.insert("version".into(), json!(artifact.version));
            if let Some(content_type) = &artifact.content_type {
                object.insert("contentType".into(), json!(content_type));
            }
            if let Some(bytes) = latest.and_then(|version| version.bytes) {
                object.insert("bytes".into(), json!(bytes));
            }
            if let Some(source_path) = &artifact.source_path {
                object.insert("sourcePath".into(), json!(source_path));
            }
            object.insert("itemCount".into(), json!(artifact.item_count));
            if artifact.primary == Some(true) {
                object.insert("primary".into(), json!(true));
            }
            Value::Object(object)
        })
        .collect();
    let leftover_running = if terminal && usage.running > 0 {
        Some(usage.running)
    } else {
        None
    };
    // The health cross-section: this host's journal read surface produces no concurrency-pressure / stall criteria (that is a set of governor state),
    // so those fields are **absent entirely** rather than written as 0 — 0 is an assertion, absence is what "could not be determined" means.
    let mut health = serde_json::Map::new();
    if let Some(last_at) = projection.last_at {
        health.insert("lastProgressAt".into(), json!(last_at));
    }
    health.insert("consecutiveFailures".into(), json!(0));
    health.insert("cachedSteps".into(), json!(projection.cached_steps));
    if let Some(leftover_running) = leftover_running {
        health.insert("leftoverRunning".into(), json!(leftover_running));
    }
    health.insert(
        "pendingQuestionsKnown".into(),
        json!(pending_questions_known),
    );
    let health = Value::Object(health);
    let error = snapshot.failure.as_ref().map(|failure| {
        let mut object = serde_json::Map::new();
        object.insert("code".into(), json!(failure.code.as_str()));
        object.insert("message".into(), json!(failure.message));
        if let Some(provider_stop) = &failure.provider_stop {
            object.insert(
                "providerStop".into(),
                serde_json::to_value(provider_stop).unwrap_or(Value::Null),
            );
        }
        Value::Object(object)
    });
    let max_concurrency = match ceiling {
        Some(ceiling) if snapshot.max_concurrency < ceiling => Some(snapshot.max_concurrency),
        Some(_) => None,
        None => Some(snapshot.max_concurrency),
    };
    Ok(RunDetail {
        run_id: run_id.to_string(),
        label,
        label_source,
        status,
        stop_reason,
        resumed_from: snapshot.resumed_from.clone(),
        superseded_by: snapshot.superseded_by.clone(),
        max_concurrency,
        subagent_model: snapshot.subagent_model.clone(),
        script_path: snapshot
            .script_path
            .as_deref()
            .map(|path| describe_script_path(std::path::Path::new(path), Some(cwd))),
        owned_by_this_session,
        possibly_interrupted,
        created_at,
        updated_at,
        generated_at,
        usage,
        actors,
        log_tail: projection.logs.clone(),
        subagents,
        subagents_truncated,
        phases,
        health,
        result: serialize_artifact(snapshot.output.as_ref()),
        error,
        pending_questions,
        artifacts,
        pending_questions_known,
    })
}

/// The one-sentence situation: assembled deterministically (the same snapshot always yields the same sentence), capped at 400 characters, and when exceeded
/// whole sentences are dropped from the tail. It reads the structured fields above, so the facts come first and this sentence second.
fn build_summary(detail: &RunDetail) -> String {
    let now = detail.generated_at;
    let terminal = matches!(detail.status, "completed" | "errored" | "stopped");
    let elapsed = format_duration(detail.updated_at - detail.created_at);
    let status_clause = if !terminal {
        format!("Running for {}", format_duration(now - detail.created_at))
    } else {
        match detail.status {
            "completed" => format!("Completed in {elapsed}"),
            "errored" => format!("Errored after {elapsed}"),
            _ => {
                let reason = detail.stop_reason.unwrap_or("stopped");
                let ended = format_relative_age(now, Some(detail.updated_at))
                    .map(|age| format!(" {age}"))
                    .unwrap_or_default();
                format!("Stopped ({reason}){ended} after {elapsed}")
            }
        }
    };
    let settled = u64::from(detail.usage.completed) + u64::from(detail.usage.failed);
    let steps_clause = if terminal {
        let failed = if detail.usage.failed > 0 {
            format!(", {} failed", detail.usage.failed)
        } else {
            String::new()
        };
        format!(
            "{settled} step{} settled{failed}, {} tokens",
            if settled == 1 { "" } else { "s" },
            format_count(detail.usage.spent_tokens)
        )
    } else {
        let running = if detail.usage.running > 0 {
            format!(", {} running", detail.usage.running)
        } else {
            String::new()
        };
        format!(
            "{settled} of {} dispatched steps settled{running}",
            detail.usage.observed
        )
    };

    let mut sentences = vec![format!("{status_clause}."), format!("{steps_clause}.")];
    let optional: Vec<String> = [
        detail.error.as_ref().and_then(|error| {
            if detail.status == "errored" || detail.stop_reason == Some("provider") {
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .map(|code| format!("Failure: {code}."))
            } else {
                None
            }
        }),
        if !detail.pending_questions_known {
            Some("Pending questions are unknown from this session.".to_string())
        } else if detail.pending_questions.is_empty() {
            None
        } else {
            Some(format!(
                "{} question{} awaiting your answer.",
                detail.pending_questions.len(),
                if detail.pending_questions.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ))
        },
        if terminal {
            None
        } else {
            format_relative_age(
                now,
                detail.health.get("lastProgressAt").and_then(Value::as_f64),
            )
            .map(|age| format!("Last progress {age}."))
        },
        if detail.status != "completed" {
            None
        } else {
            detail
                .artifacts
                .iter()
                .find(|artifact| artifact.get("primary") == Some(&json!(true)))
                .map(|artifact| {
                    let title = artifact
                        .get("title")
                        .and_then(Value::as_str)
                        .or_else(|| artifact.get("id").and_then(Value::as_str))
                        .unwrap_or("artifact");
                    let kind = artifact
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("file");
                    format!("Deliverable: {title} ({kind}, primary).")
                })
        },
        if detail.owned_by_this_session {
            None
        } else {
            Some("Owned by another session.".to_string())
        },
    ]
    .into_iter()
    .flatten()
    .collect();
    sentences.extend(optional);
    while sentences.len() > 2 && sentences.join(" ").chars().count() > SUMMARY_MAX_CHARS {
        sentences.pop();
    }
    let text = sentences.join(" ");
    if text.chars().count() <= SUMMARY_MAX_CHARS {
        return text;
    }
    let mut clipped: String = text.chars().take(SUMMARY_MAX_CHARS - 1).collect();
    clipped.push('…');
    clipped
}

fn format_duration(ms: f64) -> String {
    let total = if ms.is_finite() && ms > 0.0 { ms } else { 0.0 };
    let second = 1_000.0;
    let minute = 60_000.0;
    let hour = 3_600_000.0;
    let day = 86_400_000.0;
    if total < minute {
        return format!("{}s", (total / second).floor() as u64);
    }
    if total < hour {
        return format!(
            "{}m {:02}s",
            (total / minute).floor() as u64,
            ((total % minute) / second).floor() as u64
        );
    }
    if total < day {
        return format!(
            "{}h {:02}m",
            (total / hour).floor() as u64,
            ((total % hour) / minute).floor() as u64
        );
    }
    format!(
        "{}d {}h",
        (total / day).floor() as u64,
        ((total % day) / hour).floor() as u64
    )
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(character);
    }
    out
}

pub struct GetWorkflowRunTool;

impl Tool for GetWorkflowRunTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: GET_WORKFLOW_RUN_TOOL_NAME.to_string(),
            ..get_workflow_run_contract()
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let Some(port) = ctx.workflow.as_deref() else {
            return Err(support::workflow_introspection_unavailable());
        };
        let run_id = require_string(input, "run_id")?;
        let detail = read_run_detail(
            port,
            run_id,
            &ctx.session_id,
            ctx.workspace_root.as_path(),
            port.concurrency_ceiling(),
        )?;
        let summary = build_summary(&detail);
        let mut object = serde_json::Map::new();
        object.insert("runId".into(), json!(detail.run_id));
        object.insert("label".into(), json!(detail.label));
        object.insert("labelSource".into(), json!(detail.label_source));
        object.insert("status".into(), json!(detail.status));
        if let Some(reason) = detail.stop_reason {
            object.insert("stopReason".into(), json!(reason));
        }
        if let Some(resumed_from) = detail.resumed_from {
            object.insert("resumedFrom".into(), json!(resumed_from));
        }
        if let Some(superseded_by) = detail.superseded_by {
            object.insert("supersededBy".into(), json!(superseded_by));
        }
        object.insert(
            "ownedByThisSession".into(),
            json!(detail.owned_by_this_session),
        );
        if detail.possibly_interrupted {
            object.insert("possiblyInterrupted".into(), json!(true));
        }
        object.insert("createdAt".into(), json!(detail.created_at));
        object.insert("updatedAt".into(), json!(detail.updated_at));
        object.insert("summary".into(), json!(summary));
        object.insert("generatedAt".into(), json!(detail.generated_at));
        object.insert(
            "usage".into(),
            json!({
                "spentTokens": detail.usage.spent_tokens,
                "nodesObserved": detail.usage.observed,
                "nodesRunning": detail.usage.running,
                "nodesCompleted": detail.usage.completed,
                "nodesFailed": detail.usage.failed,
            }),
        );
        if let Some(max_concurrency) = detail.max_concurrency {
            object.insert("maxConcurrency".into(), json!(max_concurrency));
        }
        if let Some(subagent_model) = detail.subagent_model {
            object.insert("subagentModel".into(), json!(subagent_model));
        }
        if let Some(script_path) = detail.script_path {
            object.insert("scriptPath".into(), json!(script_path));
        }
        object.insert("actors".into(), Value::Array(detail.actors));
        object.insert("logTail".into(), Value::Array(detail.log_tail));
        object.insert("subagents".into(), Value::Array(detail.subagents));
        if detail.subagents_truncated {
            object.insert("subagentsTruncated".into(), json!(true));
        }
        if let Some(phases) = detail.phases.filter(|phases| !phases.is_empty()) {
            object.insert("phases".into(), Value::Array(phases));
        }
        object.insert("health".into(), detail.health);
        if let Some(result) = detail.result {
            object.insert("result".into(), json!(result));
        }
        if let Some(error) = detail.error {
            object.insert("error".into(), error);
        }
        if !detail.pending_questions.is_empty() {
            object.insert(
                "pendingQuestions".into(),
                Value::Array(detail.pending_questions),
            );
        }
        if !detail.artifacts.is_empty() {
            object.insert("artifacts".into(), Value::Array(detail.artifacts));
        }
        Ok(Value::Object(object).to_string())
    }

    fn result_display_for(&self, _input: &Value, output_text: &str) -> Option<Value> {
        let output: Value = serde_json::from_str(output_text).ok()?;
        crate::tools::workflow::display::get_workflow_run_display(&output)
    }
}

// ————————————————————————————————————————————————————————————————
// ResumeWorkflowRun
// ————————————————————————————————————————————————————————————————

const RESUME_WORKFLOW_RUN_DESCRIPTION: &str = "Resumes a dynamic-workflow run whose status is `stopped` — the user cancelled it, you stopped it with TaskStop, a provider-side error stopped it, or the process that owned it exited (`interrupted`). The run continues under the same run ID: finished steps are replayed from the journal without spending tokens, unfinished steps are dispatched again. A `superseded` run is NOT resumable (its successor is the live run), and an `errored` run needs a corrected script via AmendWorkflow. The resumed run is backgrounded: you will be notified when it completes; do not poll it.";

fn resume_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {"type": "string", "minLength": 1, "description": "The workflow run ID to resume — a cancelled run or one that failed with code `Interrupted`, as seen with GetWorkflowRun or ListWorkflowRuns"}
        },
        "required": ["run_id"],
        "additionalProperties": false
    })
}

fn resume_failure(error: &WorkflowServiceError, run_id: &str) -> ToolError {
    match error {
        WorkflowServiceError::RunNotFound { .. } => support::run_not_found(run_id),
        WorkflowServiceError::NotResumable { .. } => invalid_input(
            "workflow_run_not_resumable: this run is not in the resumable set — only a `stopped` run can be resumed (any stop reason except `superseded`). An `errored` run needs a corrected script submitted with AmendWorkflow; a completed run has nothing to resume. Check the status with GetWorkflowRun.",
        ),
        WorkflowServiceError::Superseded { .. } => invalid_input(format!(
            "workflow_run_superseded: run {run_id} was stopped by an AmendWorkflow and superseded; its unfinished work belongs to the successor run (see GetWorkflowRun's supersededBy). Read or amend the successor instead of resuming this run."
        )),
        WorkflowServiceError::AlreadyRunning { .. } => invalid_input(
            "workflow_run_already_running: this run is already in flight in this session's workflow runtime. Wait for its completion notification instead of resuming it again.",
        ),
        WorkflowServiceError::MissingMetadata { .. } => invalid_input(
            "workflow_run_script_missing: this run's journal record has no stored script text (it predates script persistence), so there is nothing to re-run. Start a fresh run with CreateWorkflow instead.",
        ),
        WorkflowServiceError::ScriptMismatch { .. } => invalid_input(format!(
            "workflow_run_script_mismatch: the stored script hash for run {run_id} no longer matches the stored script text — the journal record was modified by an outside force. Start a fresh run with CreateWorkflow instead."
        )),
        WorkflowServiceError::CompileFailed { diagnostics } => {
            let detail = diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            invalid_input(format!(
                "workflow_run_compile_failed: the stored script of run {run_id} no longer compiles against the current workflow facade, so it cannot be replayed as-is. Rewrite it for the current facade and submit it with AmendWorkflow, which keeps the finished work of this run.\n{detail}"
            ))
        }
        other => capability_unavailable(format!(
            "workflow_resume_failed: the workflow runtime could not resume run {run_id}: {other}"
        )),
    }
}

pub struct ResumeWorkflowRunTool;

#[async_trait::async_trait]
impl AsyncTool for ResumeWorkflowRunTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: RESUME_WORKFLOW_RUN_TOOL_NAME.to_string(),
            description: RESUME_WORKFLOW_RUN_DESCRIPTION.to_string(),
            input_schema: resume_schema(),
            // Resuming = restarting the execution subprocess and the actor sessions, so the read-only declaration does not hold.
            read_only: false,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: SCOPE_NONE,
            timeout_ms: RESUME_TIMEOUT_MS,
            max_output_bytes: OBSERVATION_MODEL_BYTES,
            max_model_bytes: OBSERVATION_MODEL_BYTES,
            preview_direction: "head",
        }
    }

    /// TS `needsApproval: false`: pinned to the already approved same script, no dialog.
    fn always_allowed_by_name(&self) -> bool {
        true
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let Some(port) = ctx.workflow.as_deref() else {
            return Err(capability_unavailable(
                "workflow_resume_unavailable: this session cannot resume workflow runs — workflow execution is not available here. This is a capability gap, not a status problem.",
            ));
        };
        let run_id = require_string(input, "run_id")?;
        let resumed = port
            .resume(run_id)
            .await
            .map_err(|error| resume_failure(&error, run_id))?;
        Ok(json!({
            "ok": true,
            "runId": resumed.run_id,
            "response": format!(
                "The workflow run {} has been resumed and is running in the background. It is still running — you will be notified with the final output when it completes. Do not wait for it or poll it with TaskOutput; continue with other work unless the user asked you to wait.",
                resumed.run_id
            ),
            "status": "backgrounded",
            "backgroundTaskId": resumed.run_id,
        })
        .to_string())
    }

    fn result_display_for(&self, _input: &Value, output_text: &str) -> Option<Value> {
        let output: Value = serde_json::from_str(output_text).ok()?;
        crate::tools::workflow::display::resume_workflow_run_display(&output)
    }
}

// ————————————————————————————————————————————————————————————————
// ResolveWorkflowQuestion
// ————————————————————————————————————————————————————————————————

const RESOLVE_WORKFLOW_QUESTION_DESCRIPTION: &str = "Answers a blocking question that a subagent escalated from inside a RUNNING dynamic-workflow run. Takes question_id (looks like `dwfq-...`) from the escalation notification, or from GetWorkflowRun's pending questions if that notification was lost. The run keeps running the whole time: only the subagent that asked is parked, and your answer becomes that call's result verbatim. Answer directly and actionably; nothing answers on your behalf and the subagent waits indefinitely.";

fn resolve_question_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "question_id": {"type": "string", "minLength": 1, "description": "The question ID from the escalation notification, or from GetWorkflowRun's pending questions (looks like `dwfq-...`)"},
            "answer": {"type": "string", "minLength": 1, "description": "The answer text, delivered verbatim to the subagent that asked"}
        },
        "required": ["question_id", "answer"],
        "additionalProperties": false
    })
}

pub struct ResolveWorkflowQuestionTool;

impl Tool for ResolveWorkflowQuestionTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: RESOLVE_WORKFLOW_QUESTION_TOOL_NAME.to_string(),
            description: RESOLVE_WORKFLOW_QUESTION_DESCRIPTION.to_string(),
            input_schema: resolve_question_schema(),
            // Answering makes a parked actor continue working with this text — not read-only.
            read_only: false,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: SCOPE_NONE,
            timeout_ms: ASK_TIMEOUT_MS,
            max_output_bytes: OBSERVATION_MODEL_BYTES,
            max_model_bytes: OBSERVATION_MODEL_BYTES,
            preview_direction: "head",
        }
    }

    /// TS `needsApproval: false`: answering only delivers a piece of text into an already approved run.
    fn always_allowed_by_name(&self) -> bool {
        true
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let Some(port) = ctx.workflow.as_deref() else {
            return Err(capability_unavailable(
                "workflow_question_answering_unavailable: this session cannot answer workflow escalations — workflow execution is not available here. This is a capability gap, not a bad question ID.",
            ));
        };
        let question_id = require_string(input, "question_id")?;
        let answer = require_string(input, "answer")?;
        match port.resolve_question(question_id, answer) {
            ResolveQuestionResult::Resolved { qid } => Ok(json!({
                "ok": true,
                "qid": qid,
                "response": format!(
                    "Answer delivered for question {qid}. The subagent that asked has resumed its turn with your answer; the run keeps going as before."
                ),
            })
            .to_string()),
            // The server's refusal wording is surfaced verbatim (it is already written as the model's next step); only the stable error code is picked here.
            ResolveQuestionResult::Refused { reason, message, .. } => Err(match reason {
                QuestionRefusal::UnknownQuestion | QuestionRefusal::AlreadyResolved => {
                    invalid_input(message)
                }
                QuestionRefusal::RunNotInFlight => capability_unavailable(message),
            }),
        }
    }
}

// ————————————————————————————————————————————————————————————————
// ListSavedWorkflows
// ————————————————————————————————————————————————————————————————

const LIST_SAVED_WORKFLOWS_DESCRIPTION: &str = "Lists the dynamic workflows saved in this project (`.zcode/workflows/`, keyed on the session's working directory) and the global archive (`~/.zcode/workflows`, available from every project). These are workflow DEFINITIONS you can run — for past runs use ListWorkflowRuns instead. Run one by passing its name to CreateWorkflow as `saved: { name, args }`. `invalid` names saved files that could not be read (usually a hand-edited metadata block).";

fn list_saved_workflows_schema() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

pub struct ListSavedWorkflowsTool;

impl Tool for ListSavedWorkflowsTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: LIST_SAVED_WORKFLOWS_TOOL_NAME.to_string(),
            description: LIST_SAVED_WORKFLOWS_DESCRIPTION.to_string(),
            input_schema: list_saved_workflows_schema(),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: SCOPE_NONE,
            timeout_ms: OBSERVATION_TIMEOUT_MS,
            max_output_bytes: OBSERVATION_MODEL_BYTES,
            max_model_bytes: OBSERVATION_MODEL_BYTES,
            preview_direction: "head",
        }
    }

    fn execute(&self, _input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        // cwd always comes from this session's working directory: the model has no right to scan disks across projects (the precondition of `sideEffectScope: none`).
        let list = crate::workflow_service::saved_workflows::list_saved_workflows(
            ctx.workspace_root.as_path(),
            None,
            None,
        );
        let workflows: Vec<Value> = list
            .entries
            .iter()
            .map(|entry| serde_json::to_value(entry).unwrap_or(Value::Null))
            .collect();
        let mut output = serde_json::Map::new();
        output.insert("workflows".into(), Value::Array(workflows));
        if !list.invalid.is_empty() {
            output.insert(
                "invalid".into(),
                serde_json::to_value(&list.invalid).unwrap_or(Value::Null),
            );
        }
        Ok(Value::Object(output).to_string())
    }

    fn result_display_for(&self, _input: &Value, output_text: &str) -> Option<Value> {
        let output: Value = serde_json::from_str(output_text).ok()?;
        crate::tools::workflow::display::saved_workflow_list_display(&output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow_service::SnapshotStatus;

    fn row(status: Option<RunStatus>, stop: Option<RunStopReason>, live: bool) -> RunListItem {
        RunListItem {
            run_id: "dwfrun-1".into(),
            label: "nightly".into(),
            label_source: LabelSource::Name,
            status: match status {
                Some(RunStatus::Completed) => SnapshotStatus::Completed,
                Some(RunStatus::Errored) => SnapshotStatus::Failed,
                Some(RunStatus::Stopped) => SnapshotStatus::Cancelled,
                _ => SnapshotStatus::Running,
            },
            run_status: status,
            stop_reason: stop,
            resumed_from: None,
            superseded_by: None,
            parent_session_id: Some("session-1".into()),
            tool_call_id: None,
            spent_tokens: 0,
            resumable: false,
            failure_code: None,
            failure_message: None,
            live,
            created_at: 1.0,
            updated_at: 2.0,
        }
    }

    /// The lifecycle vocabulary is the model surface's discriminator: the three terminal words are surfaced as they are, every non-terminal one is read as running.
    #[test]
    fn lifecycle_words_follow_the_run_status_not_the_snapshot_vocabulary() {
        assert_eq!(
            lifecycle_status(&row(Some(RunStatus::Completed), None, false)),
            "completed"
        );
        assert_eq!(
            lifecycle_status(&row(Some(RunStatus::Errored), None, false)),
            "errored"
        );
        assert_eq!(
            lifecycle_status(&row(
                Some(RunStatus::Stopped),
                Some(RunStopReason::Interrupted),
                false
            )),
            "stopped"
        );
        assert_eq!(lifecycle_status(&row(None, None, true)), "running");
        assert_eq!(
            lifecycle_status(&row(Some(RunStatus::Pending), None, true)),
            "running"
        );
    }

    /// `possiblyInterrupted` is an annotation, not a verdict: it is present only when running + this service has no engine for it.
    #[test]
    fn possibly_interrupted_marks_only_live_unknowns() {
        assert!(possibly_interrupted(&row(None, None, false)));
        assert!(!possibly_interrupted(&row(None, None, true)));
        assert!(!possibly_interrupted(&row(
            Some(RunStatus::Stopped),
            None,
            false
        )));
    }

    #[test]
    fn duration_and_counts_are_locale_independent() {
        assert_eq!(format_duration(40_000.0), "40s");
        assert_eq!(format_duration(310_000.0), "5m 10s");
        assert_eq!(format_duration(8_100_000.0), "2h 15m");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_count(999), "999");
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use crate::tools::Tool;

    #[test]
    fn list_display_passes_the_page_and_only_marks_real_truncation() {
        let output = json!({
            "runs": [{
                "runId": "dwfrun-1",
                "label": "linear",
                "labelSource": "name",
                "status": "completed",
                "ownedByThisSession": true,
                "createdAt": 1,
                "updatedAt": 2,
                "spentTokens": 35
            }]
        })
        .to_string();
        let display = ListWorkflowRunsTool
            .result_display_for(&json!({}), &output)
            .expect("display");
        assert_eq!(display["kind"], json!("list_workflow_runs"));
        assert_eq!(display["runs"][0]["runId"], json!("dwfrun-1"));
        assert!(display.get("truncated").is_none());

        let truncated = json!({"runs": [], "truncated": true}).to_string();
        assert_eq!(
            ListWorkflowRunsTool
                .result_display_for(&json!({}), &truncated)
                .expect("display")["truncated"],
            json!(true)
        );
        assert!(ListWorkflowRunsTool
            .result_display_for(&json!({}), "not json")
            .is_none());
    }
}
