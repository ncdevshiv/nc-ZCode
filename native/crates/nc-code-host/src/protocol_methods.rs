//! nc-code-protocol (non-v4) query/control methods: `session/debug`, `session/compact`,
//! `session/goal`, `workspace/generateText` and cancellation.
//!
//! These methods are implemented directly by the CLI's `nc-code-protocol` server in TS (not the v4 command surface):
//! They return **domain results** (debug snapshot, target summary, generated text), the session snapshot is still provided by the caller
//! (`nc-code-services-agent`) is completed with the same strict snapshot constructor. This module only performs protocol layer verification.
//! Status writing (the only write entry is still in `Session`) and real failure, no forged return value.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::goal::{GoalRecord, MAX_GOAL_OBJECTIVE_CHARS};
use crate::rpc::{RpcError, CODE_INVALID_PARAMS};
use crate::session::Session;
use crate::store::now_ms;
use crate::HostServer;

/// The status revision of `assertExpectedRevision` does not match (TS ProtocolRequestError -32009).
pub const PROTOCOL_REVISION_MISMATCH: i64 = -32009;
/// Rejected during operation (TS ProtocolRequestError -32010).
pub const PROTOCOL_ACTIVE_TURN: i64 = -32010;

impl HostServer {
    fn protocol_session(&self, params: &Value) -> Result<Arc<Session>, RpcError> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RpcError::new(CODE_INVALID_PARAMS, "params.sessionId: required string")
            })?;
        self.session(session_id)
            .ok_or_else(|| super::session_not_found(session_id))
    }

    fn assert_revision(session: &Session, params: &Value) -> Result<(), RpcError> {
        let Some(expected) = params.get("expectedRevision").and_then(Value::as_i64) else {
            return Ok(());
        };
        let actual = session.revision();
        if expected == actual {
            return Ok(());
        }
        Err(RpcError::new(
            PROTOCOL_REVISION_MISMATCH,
            "Session state revision mismatch",
        )
        .with_data(json!({"actualRevision": actual, "expectedRevision": expected})))
    }

    fn emit_post_frames(&self, frames: Vec<String>) {
        for line in frames {
            let _ = self.outbox.send(line);
        }
    }

    /// `session/debug`: Debug ledger snapshot of this session (rounds/networkEntries/cache).
    pub(crate) fn protocol_session_debug(&self, params: &Value) -> Result<Value, RpcError> {
        let session = self.protocol_session(params)?;
        Ok(session.debug_snapshot())
    }

    /// `session/compact`: manual compression. Behavioral alignment TS:
    /// - There is already compact in transit → `already_running` (repeated requests are discarded and not queued);
    /// - Other turns in progress → -32010;
    /// - Otherwise submit the compact command and return `accepted` immediately (compression runs in the background, and the final state is expressed by the state stream).
    pub(crate) async fn protocol_session_compact(
        self: &Arc<Self>,
        params: &Value,
    ) -> Result<Value, RpcError> {
        let session = self.protocol_session(params)?;
        Self::assert_revision(&session, params)?;
        let input_id = params
            .get("inputId")
            .and_then(Value::as_str)
            .map(str::to_string);
        if session.is_compacting() {
            let mut compact = json!({"state": "already_running"});
            if let Some(input_id) = &input_id {
                compact["inputId"] = json!(input_id);
            }
            return Ok(json!({"compact": compact}));
        }
        if session.has_execution_in_flight() {
            return Err(RpcError::new(
                PROTOCOL_ACTIVE_TURN,
                "Cannot compact while a prompt is running",
            ));
        }
        let mut payload = json!({});
        if let Some(instructions) = params.get("instructions").and_then(Value::as_str) {
            payload["instructions"] = json!(instructions);
        }
        if let Some(input_id) = &input_id {
            payload["inputId"] = json!(input_id);
        }
        let command_id = input_id.clone().unwrap_or_else(|| self.next_id("compact"));
        let envelope = json!({
            "commandId": command_id,
            "clientId": "zcode-host",
            "sessionId": session.id,
            "type": "compact",
            "payload": payload,
            "issuedAt": now_ms(),
        });
        let outcome = self.command(&envelope).await?;
        let status = outcome
            .response
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("failed");
        if status != "accepted" && status != "duplicate" {
            return Err(RpcError::new(
                CODE_INVALID_PARAMS,
                format!("compact was not admitted: {}", outcome.response),
            ));
        }
        self.emit_post_frames(outcome.post_frames);
        let mut compact = json!({"state": "accepted"});
        if let Some(input_id) = &input_id {
            compact["inputId"] = json!(input_id);
        }
        Ok(json!({"compact": compact}))
    }

    /// `session/goal`: Goal control (show/set/replace/pause/resume/clear).
    ///
    /// Write the same executor as the v4 command (`sendGoalCommand`/`resumeGoal`), do not build the second one
    /// Status writing path; `pause` is the control point (can directly interrupt the referee in progress); `clear` is the protocol layer
    /// Direct writing - v4 command interface does not have clear, but the clear button of the target panel is clicked here.
    pub(crate) async fn protocol_session_goal(
        self: &Arc<Self>,
        params: &Value,
    ) -> Result<Value, RpcError> {
        let session = self.protocol_session(params)?;
        Self::assert_revision(&session, params)?;
        let action = params
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.action: required string"))?;
        if action != "pause" && session.has_execution_in_flight() {
            return Err(RpcError::new(
                PROTOCOL_ACTIVE_TURN,
                "Cannot manage goals while a prompt is running",
            ));
        }
        let plan_mode = session.collaboration_mode() == "plan";
        match action {
            "show" => Ok(json!({
                "response": goal_summary(session.goal().as_ref()),
                "startedTurn": false,
            })),
            "pause" => {
                let had_goal = session.goal().is_some();
                let envelope = json!({
                    "commandId": self.next_id("pause-goal"),
                    "clientId": "zcode-host",
                    "sessionId": session.id,
                    "type": "pauseGoal",
                    "payload": {},
                    "issuedAt": now_ms(),
                    "baseRevision": session.revision(),
                });
                let outcome = self.command(&envelope).await?;
                self.emit_post_frames(outcome.post_frames);
                Ok(json!({
                    "response": if had_goal { "" } else { "No goal to pause." },
                    "startedTurn": false,
                }))
            }
            "resume" => {
                let goal = session.goal();
                let Some(goal) = goal else {
                    return Ok(json!({
                        "response": "No goal to resume.",
                        "startedTurn": false,
                    }));
                };
                let response =
                    format_goal_changed("Goal resumed", &goal.objective, goal.time_used_seconds);
                if plan_mode {
                    // TS: The plan mode only switches status and does not continue running (with instructions).                    session.update_goal(|record| record.set_status(GoalStatus::Active));
                    self.persist_meta(&session);
                    self.emit_post_frames(session.project_goal_update());
                    return Ok(json!({
                        "response": append_plan_note(response),
                        "startedTurn": false,
                    }));
                }
                let envelope = json!({
                    "commandId": self.next_id("resume-goal"),
                    "clientId": "zcode-host",
                    "sessionId": session.id,
                    "type": "resumeGoal",
                    "payload": {},
                    "issuedAt": now_ms(),
                });
                let outcome = self.command(&envelope).await?;
                self.emit_post_frames(outcome.post_frames);
                let accepted = outcome
                    .response
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status == "accepted" || status == "duplicate");
                if !accepted {
                    return Err(RpcError::new(
                        PROTOCOL_ACTIVE_TURN,
                        format!("resumeGoal was not admitted: {}", outcome.response),
                    ));
                }
                Ok(json!({
                    "response": append_plan_note_if(plan_mode, response),
                    "startedTurn": true,
                }))
            }
            "clear" => {
                let had_goal = session.goal().is_some();
                if had_goal {
                    session.set_goal(None);
                    self.persist_meta(&session);
                    self.emit_post_frames(session.project_goal_update());
                }
                Ok(json!({
                    "response": if had_goal { "Goal cleared." } else { "No goal to clear." },
                    "startedTurn": false,
                }))
            }
            "set" | "replace" => {
                let objective = params
                    .get("objective")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if objective.is_empty() {
                    return Ok(json!({
                        "response": if action == "replace" {
                            "Usage: /goal replace <objective>"
                        } else {
                            "Usage: /goal <objective>"
                        },
                        "startedTurn": false,
                    }));
                }
                if objective.chars().count() > MAX_GOAL_OBJECTIVE_CHARS {
                    return Err(RpcError::new(
                        CODE_INVALID_PARAMS,
                        format!(
                            "goal objective must be at most {MAX_GOAL_OBJECTIVE_CHARS} characters"
                        ),
                    ));
                }
                let display_text = format!("/goal {objective}");
                let envelope = json!({
                    "commandId": params
                        .get("inputId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| self.next_id("goal")),
                    "clientId": "zcode-host",
                    "sessionId": session.id,
                "type": "sendGoalCommand",
                    "payload": {"text": objective, "displayText": display_text},
                    "issuedAt": now_ms(),
                });
                let outcome = self.command(&envelope).await?;
                self.emit_post_frames(outcome.post_frames);
                let status = outcome
                    .response
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed");
                if status != "accepted" && status != "duplicate" {
                    return Err(RpcError::new(
                        CODE_INVALID_PARAMS,
                        format!("sendGoalCommand was not admitted: {}", outcome.response),
                    ));
                }
                Ok(json!({
                    "response": append_plan_note_if(
                        plan_mode,
                        format_goal_changed("Goal active", &objective, 0),
                    ),
                    "startedTurn": !plan_mode,
                }))
            }
            other => Err(RpcError::new(
                CODE_INVALID_PARAMS,
                format!("unknown goal action: {other}"),
            )),
        }
    }

    /// `session/subagents`: Lists session-derived subagent runs (active + ended, cursor paginated).
    ///
    /// Two sources of fact, merged when reading: in-process registration table (run initiated by this process) and sub-session directory
    /// `subagent-run.json` (can still be listed after restarting). Sorting and Cursor Semantic Alignment TS
    /// `projectSessionSubagents` / `paginateEndedSubagents`: endedAt descending order,
    /// Also sorted in descending order by childSessionId; the cursor is an opaque string of `endedAt:childSessionId`.
    pub(crate) fn protocol_session_subagents(&self, params: &Value) -> Result<Value, RpcError> {
        let session = self.protocol_session(params)?;
        let ended_limit = params
            .get("endedLimit")
            .and_then(Value::as_u64)
            .map(|limit| limit as usize)
            .unwrap_or(20)
            .clamp(1, 100);
        let cursor = match params.get("endedCursor").and_then(Value::as_str) {
            Some(cursor) => Some(decode_subagent_cursor(cursor)?),
            None => None,
        };
        let runs = self.subagent_runs_for(&session.id);
        let mut running: Vec<Value> = Vec::new();
        let mut ended: Vec<crate::subagent::SubagentRun> = Vec::new();
        for run in runs {
            if run.ended_at.is_none() {
                let mut entry = json!({
                    "childSessionId": run.child_session_id,
                    "subagentType": run.subagent_type,
                    "title": run.title,
                    "startedAt": run.started_at,
                    "status": "running",
                });
                if !run.agent_id.is_empty() {
                    entry["agentId"] = json!(run.agent_id);
                }
                if let Some(tool_call_id) = &run.tool_call_id {
                    entry["toolCallId"] = json!(tool_call_id);
                }
                running.push(entry);
            } else {
                ended.push(run);
            }
        }
        running.sort_by(|left, right| {
            let left_start = left.get("startedAt").and_then(Value::as_u64).unwrap_or(0);
            let right_start = right.get("startedAt").and_then(Value::as_u64).unwrap_or(0);
            right_start.cmp(&left_start).then_with(|| {
                let left_id = left
                    .get("childSessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let right_id = right
                    .get("childSessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                right_id.cmp(left_id)
            })
        });
        ended.sort_by(|left, right| {
            let left_end = left.ended_at.unwrap_or(0);
            let right_end = right.ended_at.unwrap_or(0);
            right_end
                .cmp(&left_end)
                .then_with(|| right.child_session_id.cmp(&left.child_session_id))
        });
        let total = ended.len();
        let start = match &cursor {
            Some((cursor_end, cursor_id)) => ended.iter().position(|run| {
                let ended_at = run.ended_at.unwrap_or(0);
                ended_at < *cursor_end
                    || (ended_at == *cursor_end && run.child_session_id < *cursor_id)
            }),
            None => Some(0),
        };
        let (items, next_cursor) = match start {
            Some(start) => {
                let items: Vec<Value> = ended
                    .iter()
                    .skip(start)
                    .take(ended_limit)
                    .map(subagent_ended_json)
                    .collect();
                let next_cursor = if start + items.len() < total {
                    ended.get(start + items.len().saturating_sub(1)).map(|run| {
                        format!("{}:{}", run.ended_at.unwrap_or(0), run.child_session_id)
                    })
                } else {
                    None
                };
                (items, next_cursor)
            }
            None => (Vec::new(), None),
        };
        let child_session_ids: Vec<String> = running
            .iter()
            .filter_map(|entry| entry.get("childSessionId").and_then(Value::as_str))
            .map(str::to_string)
            .chain(ended.iter().map(|run| run.child_session_id.clone()))
            .collect();
        let mut ended_value = json!({"total": total, "items": items});
        if let Some(next_cursor) = next_cursor {
            ended_value["nextCursor"] = json!(next_cursor);
        }
        Ok(json!({
            "revision": session.revision(),
            "childSessionIds": child_session_ids,
            "running": running,
            "ended": ended_value,
        }))
    }

    /// `workspace/generateText`: A session-less model call (title/description, etc.).
    pub(crate) async fn protocol_generate_text(
        self: &Arc<Self>,
        params: &Value,
    ) -> Result<Value, RpcError> {
        let selection = params.get("selection").cloned().ok_or_else(|| {
            RpcError::new(CODE_INVALID_PARAMS, "params.selection: required object")
        })?;
        let provider = self
            .provider
            .as_ref()
            .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "Provider not configured"))?;
        let model_id = selection
            .get("modelId")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "selection.modelId: required"))?;
        if !provider
            .model_catalog()
            .iter()
            .any(|model| model == model_id)
        {
            return Err(RpcError::new(
                CODE_INVALID_PARAMS,
                format!("model is not configured for this host: {model_id}"),
            ));
        }
        if let (Some(requested), Some(configured)) = (
            selection.get("providerId").and_then(Value::as_str),
            provider.provider_id.as_deref(),
        ) {
            if requested != configured {
                return Err(RpcError::new(
                    CODE_INVALID_PARAMS,
                    format!("provider is not configured for this host: {requested}"),
                ));
            }
        }
        let messages = generate_messages(params)?;
        let tools = generate_tools(params)?;
        let max_tokens = params
            .get("maxOutputTokens")
            .and_then(Value::as_u64)
            .map(|tokens| tokens as u32)
            .unwrap_or(provider.max_tokens);
        let max_tokens = max_tokens.max(1);

        let operation_id = params
            .get("operationId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let cancel_flag = Arc::new(AtomicBool::new(false));
        if let Some(operation_id) = &operation_id {
            self.generate_operations
                .lock()
                .map_err(|_| {
                    RpcError::new(crate::rpc::CODE_INTERNAL, "generate ops lock poisoned")
                })?
                .insert(operation_id.clone(), Arc::clone(&cancel_flag));
        }
        let is_cancelled: Arc<dyn Fn() -> bool + Send + Sync> = {
            let flag = Arc::clone(&cancel_flag);
            Arc::new(move || flag.load(Ordering::SeqCst))
        };
        let context = self.turn_deps.context.clone();
        let result = crate::single_shot::generate(
            provider,
            &context,
            &messages,
            &tools,
            max_tokens,
            is_cancelled,
        )
        .await;
        if let Some(operation_id) = &operation_id {
            if let Ok(mut ops) = self.generate_operations.lock() {
                ops.remove(operation_id);
            }
        }
        let result = result.map_err(|error| {
            let reason = serde_json::to_value(error.code)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "model_request_failed".to_string());
            RpcError::new(CODE_INVALID_PARAMS, error.message.clone())
                .with_data(json!({"reasonCode": reason}))
        })?;
        let mut response = json!({
            "text": result.text,
            "selection": selection,
        });
        if let Some(finish_reason) = result.finish_reason {
            response["finishReason"] = json!(finish_reason);
        }
        if let Some(usage) = usage_json(&result.usage) {
            response["usage"] = usage;
        }
        if !result.tool_call_list.is_empty() {
            response["toolCalls"] = json!(result
                .tool_call_list
                .iter()
                .map(|call| json!({"id": call.id, "name": call.name, "input": call.input}))
                .collect::<Vec<_>>());
        }
        Ok(response)
    }

    /// `workspace/cancelGenerateText`: Cancel generation in progress by operationId.
    /// Returns `cancelled: false` to indicate that there is no matching in-flight operation (ended/never registered) - answer truthfully.
    pub(crate) fn protocol_cancel_generate_text(&self, params: &Value) -> Result<Value, RpcError> {
        let operation_id = params
            .get("operationId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RpcError::new(CODE_INVALID_PARAMS, "params.operationId: required string")
            })?;
        let flag = self
            .generate_operations
            .lock()
            .map_err(|_| RpcError::new(crate::rpc::CODE_INTERNAL, "generate ops lock poisoned"))?
            .remove(operation_id);
        let canceled = match flag {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        };
        Ok(json!({"operationId": operation_id, "cancelled": canceled}))
    }
}

/// Message shape (`zcodeWorkspaceModelMessageSchema`) for `workspace/generateText`.
fn generate_messages(params: &Value) -> Result<Vec<nc_code_provider::request::Message>, RpcError> {
    use nc_code_provider::request::{Message, MessageRole, ReasoningBlock, ToolCallRequest};
    let mut messages = Vec::new();
    if let Some(prompt) = params.get("prompt").and_then(Value::as_str) {
        messages.push(Message::text(MessageRole::User, prompt));
    }
    if let Some(items) = params.get("messages").and_then(Value::as_array) {
        for item in items {
            let role = item.get("role").and_then(Value::as_str).unwrap_or_default();
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match role {
                "system" => messages.push(Message::text(MessageRole::System, content)),
                "user" => messages.push(Message::text(MessageRole::User, content)),
                "assistant" => {
                    let mut message = Message::text(MessageRole::Assistant, content);
                    if let Some(calls) = item.get("toolCalls").and_then(Value::as_array) {
                        message.tool_calls = calls
                            .iter()
                            .map(|call| ToolCallRequest {
                                id: call
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                name: call
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                input: call.get("input").cloned().unwrap_or(Value::Null),
                            })
                            .collect();
                    }
                    messages.push(message);
                }
                "tool" => {
                    let mut message = Message::text(MessageRole::Tool, content);
                    message.tool_call_id = item
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    messages.push(message);
                }
                other => {
                    return Err(RpcError::new(
                        CODE_INVALID_PARAMS,
                        format!("unsupported message role: {other}"),
                    ))
                }
            }
        }
    }
    if messages.is_empty() {
        return Err(RpcError::new(
            CODE_INVALID_PARAMS,
            "prompt or messages is required",
        ));
    }
    let _ = ReasoningBlock::default();
    Ok(messages)
}

fn generate_tools(
    params: &Value,
) -> Result<Vec<nc_code_provider::request::ToolDefinition>, RpcError> {
    let mut tools = Vec::new();
    if let Some(items) = params.get("tools").and_then(Value::as_array) {
        for item in items {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "tool.name: required string"))?;
            let mut definition = nc_code_provider::request::ToolDefinition {
                name: name.to_string(),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                input_schema: item
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                strict: false,
            };
            if !definition.input_schema.is_object() {
                return Err(RpcError::new(
                    CODE_INVALID_PARAMS,
                    "tool.inputSchema must be an object",
                ));
            }
            definition.strict = false;
            tools.push(definition);
        }
    }
    Ok(tools)
}

fn usage_json(usage: &nc_code_provider::ModelUsage) -> Option<Value> {
    let mut object = serde_json::Map::new();
    for (key, value) in [
        ("inputTokens", usage.input_tokens),
        ("outputTokens", usage.output_tokens),
        ("totalTokens", usage.total_tokens),
        ("cacheReadTokens", usage.cache_read_tokens),
        ("cacheWriteTokens", usage.cache_write_tokens),
        ("reasoningTokens", usage.reasoning_tokens),
    ] {
        if let Some(value) = value {
            object.insert(key.to_string(), json!(value));
        }
    }
    if let Some(server_tool_use) = &usage.server_tool_use {
        let mut server = serde_json::Map::new();
        if let Some(value) = server_tool_use.web_search_requests {
            server.insert("webSearchRequests".to_string(), json!(value));
        }
        if let Some(value) = server_tool_use.web_fetch_requests {
            server.insert("webFetchRequests".to_string(), json!(value));
        }
        object.insert("serverToolUse".to_string(), Value::Object(server));
    }
    if object.is_empty() {
        return None;
    }
    Some(Value::Object(object))
}

/// `formatGoalSummary`: Plain text summary of the goal panel (gives usage hints when there is no goal).
fn goal_summary(goal: Option<&GoalRecord>) -> String {
    match goal {
        None => "No goal is set. Use /goal <objective> to set one.".to_string(),
        Some(goal) => format_goal_changed(
            &format!("Goal {}", goal.status.as_str()),
            &goal.objective,
            goal.time_used_seconds,
        ),
    }
}

/// `formatGoalChanged`: title + target text + elapsed time.
/// The durable target of this host does not have a token budget (the v4 command does not have a budget), so there is no output.
/// `Usage: N tokens / budget` line of TS - do not write a number that has never been measured.
fn format_goal_changed(title: &str, objective: &str, time_used_seconds: i64) -> String {
    let mut lines = vec![title.to_string(), format!("Objective: {objective}")];
    if time_used_seconds > 0 {
        lines.push(format!("Time: {time_used_seconds} seconds"));
    }
    lines.join("\n")
}

const PLAN_MODE_GOAL_CONTINUATION_SKIPPED_MESSAGE: &str =
    "The goal has been recorded in Plan mode, but will not continue automatically.";

fn append_plan_note(response: String) -> String {
    format!("{response}\n\n{PLAN_MODE_GOAL_CONTINUATION_SKIPPED_MESSAGE}")
}

fn append_plan_note_if(plan_mode: bool, response: String) -> String {
    if plan_mode {
        append_plan_note(response)
    } else {
        response
    }
}

fn subagent_ended_json(run: &crate::subagent::SubagentRun) -> Value {
    let mut entry = json!({
        "childSessionId": run.child_session_id,
        "subagentType": run.subagent_type,
        "title": run.title,
        "endedAt": run.ended_at.unwrap_or(0),
        "status": match run.status.as_str() {"success" | "failed" | "cancelled" | "lost" => run.status.clone(),
            // Unknown status (old record/damaged): press lost to report, no guess will be successful.
            _ => "lost".to_string(),
        },
    });
    if !run.agent_id.is_empty() {
        entry["agentId"] = json!(run.agent_id);
    }
    if let Some(tool_call_id) = &run.tool_call_id {
        entry["toolCallId"] = json!(tool_call_id);
    }
    if let Some(summary) = &run.summary {
        entry["summary"] = json!(summary);
    }
    entry["startedAt"] = json!(run.started_at);
    entry
}

fn decode_subagent_cursor(cursor: &str) -> Result<(u64, String), RpcError> {
    let Some((ended_at, child_id)) = cursor.split_once(':') else {
        return Err(RpcError::new(CODE_INVALID_PARAMS, "invalid endedCursor"));
    };
    let ended_at = ended_at
        .parse::<u64>()
        .map_err(|_| RpcError::new(CODE_INVALID_PARAMS, "invalid endedCursor"))?;
    if child_id.is_empty() {
        return Err(RpcError::new(CODE_INVALID_PARAMS, "invalid endedCursor"));
    }
    Ok((ended_at, child_id.to_string()))
}
