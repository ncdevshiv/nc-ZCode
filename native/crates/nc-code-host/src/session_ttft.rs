//! Local first-token facts (the source of truth behind the `v4/telemetry/local-ttft` notification).
//!
//! Aligned with `LocalTtftRecorder` in TS `bootstrap/src/nc-code-protocol-v4/local-ttft.ts`
//! and `localTtftFactsSchema` in `packages/shared/src/localTtft.ts`:
//!
//! - Only commands carrying a `ttft` context (`{version:1, observationId}`) are observed —
//!   the renderer uses it to align CLI-side stages with its own click clock; a command without context produces no facts.
//! - Every state advance (admission / execution start / model request / first output / terminal) produces one
//!   **incremental checkpoint** (`revision` moves forward monotonically), matching TS's checkpoint semantics.
//! - All timestamps come from this process's real clock (receipt, admission, execution, request, first content) and the
//!   provider's attempt traces; there is not one estimated stage.
//! - Facts are **in-process**: TS's observation is likewise never written to disk, and a restart starts from an empty ledger.
//!
//! Stages that are not ported (`context` / `hooks` / `persistence` / `compaction` / `mcp` /
//! `tools` / `request_assembly` preparation detail) need per-stage timing inside the turn; these details
//! have no measurement point in this host, so they are **not written** (the schema allows omission) and never invented.

use std::collections::HashMap;

use nc_code_provider::executor::{AttemptTrace, AttemptTraceState};
use serde_json::{json, Value};

/// Fact timestamps are uniformly unsigned milliseconds (the schema's `nonnegative`).
fn now_ms() -> u64 {
    crate::store::now_ms().max(0) as u64
}

/// `LOCAL_TTFT_TTL_MS` / `LOCAL_TTFT_MAX_PENDING` (shared/localTtft.ts:40-41).
pub const TTL_MS: u64 = 300_000;
pub const MAX_PENDING: usize = 128;
/// `LOCAL_TTFT_MAX_DETAILS`.
pub const MAX_DETAILS: usize = 64;

const OUTPUT_KIND_TEXT: &str = "text";
const OUTPUT_KIND_REASONING: &str = "reasoning";
const OUTPUT_KIND_TOOL: &str = "tool";

#[derive(Debug, Clone)]
struct TtftRecord {
    observation_id: String,
    command_id: String,
    session_id: Option<String>,
    turn_id: Option<String>,
    request_id: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    send_mode: &'static str,
    received_at: u64,
    admitted_at: Option<u64>,
    execution_at: Option<u64>,
    request_at: Option<u64>,
    output_at: Option<u64>,
    output_kind: Option<&'static str>,
    terminal: Option<&'static str>,
    revision: u64,
    details: Vec<Value>,
    truncated: bool,
}

/// The trace input of one model request (shares the same provider facts with `session_debug`).
pub struct TtftRequest<'a> {
    pub request_id: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub traces: &'a [AttemptTrace],
}

#[derive(Debug, Default)]
pub struct TtftRecorder {
    instance_id: String,
    records: HashMap<String, TtftRecord>,
}

impl TtftRecorder {
    pub fn new(instance_id: String) -> Self {
        Self {
            instance_id,
            records: HashMap::new(),
        }
    }

    /// A command was received carrying a ttft context. `false` = capacity is full, this command is not observed
    /// (TS's `receive` refuses the same way, and the caller marks `ttftExcluded` on the ack accordingly).
    pub fn receive(
        &mut self,
        command_id: &str,
        session_id: Option<&str>,
        context: Option<&Value>,
        busy: bool,
    ) -> bool {
        self.prune();
        if self.records.contains_key(command_id) {
            return true;
        }
        let (observation_id, version_ok) = match context {
            Some(context) => (
                context
                    .get("observationId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                context.get("version").and_then(Value::as_u64) == Some(1),
            ),
            None => (String::new(), false),
        };
        if !version_ok || observation_id.is_empty() {
            return true; // No alignable context: not observed, and not an error.
        }
        if self.records.len() >= MAX_PENDING {
            return false;
        }
        self.records.insert(
            command_id.to_string(),
            TtftRecord {
                observation_id,
                command_id: command_id.to_string(),
                session_id: session_id.map(str::to_string),
                turn_id: None,
                request_id: None,
                provider: None,
                model: None,
                send_mode: if busy { "queued" } else { "idle" },
                received_at: now_ms(),
                admitted_at: None,
                execution_at: None,
                request_at: None,
                output_at: None,
                output_kind: None,
                terminal: None,
                revision: 0,
                details: Vec::new(),
                truncated: false,
            },
        );
        true
    }

    pub fn mark_admitted(&mut self, command_id: &str) -> Option<Value> {
        {
            let record = self.records.get_mut(command_id)?;
            record.admitted_at.get_or_insert_with(now_ms);
        }
        self.checkpoint(command_id)
    }

    pub fn mark_execution(&mut self, command_id: &str, turn_id: &str) -> Option<Value> {
        {
            let record = self.records.get_mut(command_id)?;
            record.turn_id = Some(turn_id.to_string());
            record.execution_at.get_or_insert_with(now_ms);
        }
        self.checkpoint(command_id)
    }

    /// Model request start: records the first request's id/provider/model and its request instant, and writes the attempt detail.
    pub fn mark_request(&mut self, command_id: &str, request: &TtftRequest<'_>) -> Option<Value> {
        {
            let record = self.records.get_mut(command_id)?;
            if record.request_at.is_none() {
                // The real start instant of the first physical request (from the provider trace), not "the moment of this call".
                record.request_at = Some(
                    request
                        .traces
                        .first()
                        .map(|trace| trace.started_at_ms)
                        .unwrap_or_else(now_ms),
                );
                record.request_id = Some(request.request_id.to_string());
                record.provider = Some(request.provider.to_string());
                record.model = Some(request.model.to_string());
            }
            apply_traces(record, request);
        }
        self.checkpoint(command_id)
    }

    /// The first content output (text/reasoning/tool call). Only the first one is recorded.
    pub fn mark_first_output(&mut self, command_id: &str, kind: &str) -> Option<Value> {
        {
            let record = self.records.get_mut(command_id)?;
            if record.output_at.is_some() {
                return None;
            }
            record.output_at = Some(now_ms());
            record.output_kind = Some(match kind {
                OUTPUT_KIND_REASONING => OUTPUT_KIND_REASONING,
                OUTPUT_KIND_TOOL => OUTPUT_KIND_TOOL,
                _ => OUTPUT_KIND_TEXT,
            });
        }
        self.checkpoint(command_id)
    }

    pub fn mark_terminal(&mut self, command_id: &str, terminal: &'static str) -> Option<Value> {
        {
            let record = self.records.get_mut(command_id)?;
            record.terminal = Some(terminal);
        }
        self.checkpoint(command_id)
    }

    /// One checkpoint: the revision advances + a fact snapshot. No record (unobserved/expired) → None.
    fn checkpoint(&mut self, command_id: &str) -> Option<Value> {
        let instance_id = self.instance_id.clone();
        let record = self.records.get_mut(command_id)?;
        record.revision = record.revision.saturating_add(1);
        Some(facts(record, &instance_id))
    }

    fn prune(&mut self) {
        let cutoff = now_ms().saturating_sub(TTL_MS);
        self.records
            .retain(|_, record| record.received_at >= cutoff);
    }
}

/// The `v4/telemetry/local-ttft` notification line (stdio and the in-process runtime share the same shape).
pub fn notification_line(facts: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "method": "v4/telemetry/local-ttft",
        "params": facts,
    })
    .to_string()
}

/// Fold the attempt traces into `details` (attempt / retry_wait), keeping the real time windows.
fn apply_traces(record: &mut TtftRecord, request: &TtftRequest<'_>) {
    for trace in request.traces {
        let id = format!("attempt:{}#{}", request.request_id, trace.attempt);
        let outcome = match trace.state {
            AttemptTraceState::Completed => Some("completed"),
            AttemptTraceState::Failed => Some("failed"),
            AttemptTraceState::Cancelled => Some("cancelled"),
            AttemptTraceState::Started => None,
        };
        let mut detail = json!({
            "id": id,
            "stage": "attempt",
            "start": trace.started_at_ms,
            "requestId": request.request_id,
            "role": "response",
            "source": "cli",
        });
        if let Some(ended) = trace.ended_at_ms {
            detail["end"] = json!(ended);
        }
        if let Some(outcome) = outcome {
            // An attempt during which first output already happened is tagged first_output (same vocabulary as TS).
            let first_output = record.output_at.is_some_and(|output| {
                output >= trace.started_at_ms
                    && trace.ended_at_ms.is_some_and(|ended| output <= ended)
                    && trace.first_content_at_ms.is_some()
            });
            detail["outcome"] = json!(if first_output {
                "first_output"
            } else {
                outcome
            });
        }
        record_detail(record, detail);
        if let (Some(ended), Some(delay)) = (trace.ended_at_ms, trace.retry_delay_ms) {
            let retry = json!({
                "id": format!("retry:{}#{}", request.request_id, trace.attempt),
                "stage": "retry_wait",
                "start": ended,
                "end": ended.saturating_add(delay),
                "outcome": "completed",
                "requestId": request.request_id,
                "role": "response",
                "source": "cli",
            });
            record_detail(record, retry);
        }
    }
}

fn record_detail(record: &mut TtftRecord, detail: Value) {
    if let Some(existing) = record.details.iter_mut().find(|existing| {
        existing.get("id").and_then(Value::as_str) == detail.get("id").and_then(Value::as_str)
    }) {
        *existing = detail;
        return;
    }
    if record.details.len() >= MAX_DETAILS {
        record.truncated = true;
        return;
    }
    record.details.push(detail);
}

/// `localTtftFactsSchema` (strict: only keys that hold a fact are written).
fn facts(record: &TtftRecord, instance_id: &str) -> Value {
    let mut facts = json!({
        "version": 1,
        "observationId": record.observation_id,
        "instanceId": instance_id,
        "commandId": record.command_id,
        "receivedAt": record.received_at,
        "sendMode": record.send_mode,
        "revision": record.revision,
    });
    if let Some(session_id) = &record.session_id {
        facts["sessionId"] = json!(session_id);
    }
    if let Some(turn_id) = &record.turn_id {
        facts["turnId"] = json!(turn_id);
    }
    if let Some(request_id) = &record.request_id {
        facts["requestId"] = json!(request_id);
    }
    if let Some(provider) = &record.provider {
        facts["provider"] = json!(provider);
    }
    if let Some(model) = &record.model {
        facts["model"] = json!(model);
    }
    if let Some(admitted_at) = record.admitted_at {
        facts["admittedAt"] = json!(admitted_at);
    }
    if let Some(execution_at) = record.execution_at {
        facts["executionAt"] = json!(execution_at);
    }
    if let Some(request_at) = record.request_at {
        facts["requestAt"] = json!(request_at);
    }
    if let Some(output_at) = record.output_at {
        facts["outputAt"] = json!(output_at);
    }
    if let Some(kind) = record.output_kind {
        facts["outputKind"] = json!(kind);
    }
    if let Some(terminal) = record.terminal {
        facts["terminal"] = json!(terminal);
    }
    if !record.details.is_empty() {
        facts["details"] = json!(record.details);
    }
    if record.truncated {
        facts["truncated"] = json!(true);
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_provider::executor::AttemptTraceState;

    fn context() -> Value {
        json!({"version": 1, "observationId": "7f3c1e5a-1e4b-4f0a-9c0e-2b6f1a8d9c11"})
    }

    fn trace(state: AttemptTraceState) -> AttemptTrace {
        AttemptTrace {
            attempt: 1,
            started_at_ms: 10,
            status: Some(200),
            request_headers: Vec::new(),
            response_headers: Vec::new(),
            first_content_at_ms: Some(30),
            ended_at_ms: Some(60),
            state,
            failure_kind: None,
            failure_message: None,
            stalled_timeout_ms: None,
            retry_delay_ms: None,
        }
    }

    #[test]
    fn commands_without_ttft_context_are_not_observed() {
        let mut recorder = TtftRecorder::new("host-1".into());
        assert!(recorder.receive("c-1", Some("s-1"), None, false));
        assert!(recorder.mark_admitted("c-1").is_none());
        // An invalid context (version not 1) is likewise not observed, and is not an error either.
        let invalid = json!({"version": 2, "observationId": "x"});
        assert!(recorder.receive("c-2", None, Some(&invalid), false));
        assert!(recorder.mark_admitted("c-2").is_none());
    }

    #[test]
    fn checkpoints_carry_the_measured_stages() {
        let mut recorder = TtftRecorder::new("host-1".into());
        assert!(recorder.receive("c-1", Some("s-1"), Some(&context()), false));
        let admitted = recorder.mark_admitted("c-1").expect("admitted checkpoint");
        assert_eq!(admitted["version"], json!(1));
        assert_eq!(admitted["commandId"], json!("c-1"));
        assert_eq!(
            admitted["observationId"],
            json!("7f3c1e5a-1e4b-4f0a-9c0e-2b6f1a8d9c11")
        );
        assert_eq!(admitted["sendMode"], json!("idle"));
        assert!(admitted.get("admittedAt").is_some());
        assert!(admitted.get("executionAt").is_none());

        let execution = recorder
            .mark_execution("c-1", "turn-1")
            .expect("execution checkpoint");
        assert_eq!(execution["turnId"], json!("turn-1"));
        let request = recorder
            .mark_request(
                "c-1",
                &TtftRequest {
                    request_id: "req-1",
                    provider: "provider-a",
                    model: "model-a",
                    traces: &[trace(AttemptTraceState::Completed)],
                },
            )
            .expect("request checkpoint");
        assert_eq!(request["requestAt"], json!(10));
        assert_eq!(request["requestId"], json!("req-1"));
        assert_eq!(request["provider"], json!("provider-a"));
        let details = request["details"].as_array().expect("details");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["stage"], json!("attempt"));
        assert_eq!(details[0]["start"], json!(10));
        assert_eq!(details[0]["end"], json!(60));

        let output = recorder
            .mark_first_output("c-1", "text")
            .expect("output checkpoint");
        assert_eq!(output["outputKind"], json!("text"));
        assert!(output["outputAt"].as_u64().unwrap() > 0);
        assert!(
            recorder.mark_first_output("c-1", "tool").is_none(),
            "first output is recorded once"
        );

        let terminal = recorder
            .mark_terminal("c-1", "completed")
            .expect("terminal checkpoint");
        assert_eq!(terminal["terminal"], json!("completed"));
        assert_eq!(terminal["revision"].as_u64(), Some(5));
    }

    #[test]
    fn capacity_refuses_new_records_instead_of_evicting_live_ones() {
        let mut recorder = TtftRecorder::new("host-1".into());
        for index in 0..MAX_PENDING {
            assert!(recorder.receive(&format!("c-{index}"), None, Some(&context()), false));
        }
        assert!(
            !recorder.receive("c-overflow", None, Some(&context()), false),
            "capacity must be reported to the caller"
        );
    }
}
