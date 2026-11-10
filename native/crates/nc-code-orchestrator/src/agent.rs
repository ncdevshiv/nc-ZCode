//! Agent runtime: one agent's bounded turn loop over the real provider executor.
//!
//! ```text
//! run(agent) --> context package --> model stream --> tool calls
//!              --> permission-checked tool execution (worktree-scoped, audited)
//!              --> tool results back into the transcript --> next round ...
//!              --> submit_result | max_steps | deadline | cancellation
//! ```
//!
//! Everything observable (rounds, tool calls, usage, progress markers) lands in the
//! store and the event stream; nothing here invents success.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use nc_code_provider::request::ToolCallRequest;

use crate::db::now_ms;
use crate::error::{NcagError, Result};
use crate::events::{self, EventHub};
use crate::provider::{self, ChatMessage, ModelReply, ModelTarget};
use crate::tools::{self, PermissionSet, ToolContext};
use crate::types::AgentSubmission;
use crate::Store;

/// Hard ceiling of model rounds per task attempt (infinite repair loops are forbidden).
pub const MAX_ROUNDS: usize = 24;

#[derive(Debug, Clone)]
pub struct AgentRunConfig {
    pub target: ModelTarget,
    pub workspace: std::path::PathBuf,
    pub allowed_commands: Vec<String>,
    pub permissions: Vec<String>,
    pub task_deadline_ms: u64,
    pub round_timeout_ms: u64,
    pub stall_after_ms: i64,
    /// A structured marker that ends the run without `submit_result` (the reviewer's
    /// `<review_verdict>` block). `None` for implementation agents, which must submit.
    pub finish_marker: Option<String>,
    /// Round ceiling for this run (defaults to `MAX_ROUNDS`). Review runs use a smaller
    /// ceiling so a reviewer cannot spend the whole budget on tool calls.
    pub max_rounds: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct AgentRunOutcome {
    pub submission: Option<AgentSubmission>,
    pub rounds: usize,
    pub text_tail: String,
    pub cancelled: bool,
    pub timed_out: bool,
    pub help_request: Option<Value>,
    pub usage_input: u64,
    pub usage_output: u64,
    pub cost_usd: f64,
    pub transcript: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub agent_id: String,
    pub task_id: String,
    pub project_id: String,
    pub role: String,
    pub persona: String,
    pub tool_specs: Vec<tools::ToolDefinitionSpec>,
}

/// The cancellation flag is shared with the API (`POST /agents/{id}/cancel`).
pub struct AgentRuntime {
    store: Arc<Store>,
    hub: Arc<EventHub>,
}

impl AgentRuntime {
    pub fn new(store: Arc<Store>, hub: Arc<EventHub>) -> Self {
        Self { store, hub }
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Run one agent to a terminal outcome. The system prompt is built by the caller
    /// (`Orchestrator::context_package`) so every profile sees the same policy text.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        spec: &AgentSpec,
        config: &AgentRunConfig,
        system: &[String],
        kickoff: &str,
        cancel: Arc<AtomicBool>,
    ) -> Result<AgentRunOutcome> {
        let ctx = ToolContext {
            workspace: config.workspace.clone(),
            permissions: PermissionSet::new(config.permissions.clone()),
            allowed_commands: config.allowed_commands.clone(),
            agent_id: spec.agent_id.clone(),
            task_id: spec.task_id.clone(),
        };
        let tool_defs = tools::provider_definitions();
        let mut transcript: Vec<ChatMessage> = vec![ChatMessage::user(kickoff)];
        let mut transcript_json: Vec<Value> = vec![json!({ "role": "user", "text": kickoff })];
        let mut submission: Option<AgentSubmission> = None;
        let mut help_request: Option<Value> = None;
        let mut cancelled = false;
        let mut timed_out = false;
        let mut usage_input = 0u64;
        let mut usage_output = 0u64;
        let mut cost_usd = 0.0f64;
        let mut text_tail = String::new();
        let mut rounds = 0usize;
        let mut empty_rounds = 0usize;
        let round_limit = config.max_rounds.unwrap_or(MAX_ROUNDS).clamp(1, MAX_ROUNDS);
        let mut final_round_prompted = false;
        let started = Instant::now();
        let deadline = Duration::from_millis(config.task_deadline_ms);
        let progress: Arc<dyn Fn(&str) + Send + Sync> = {
            let store = Arc::clone(&self.store);
            let hub = Arc::clone(&self.hub);
            let project = spec.project_id.clone();
            let task = spec.task_id.clone();
            let agent = spec.agent_id.clone();
            Arc::new(move |marker: &str| {
                let _ = store.heartbeat(&agent, Some(marker));
                let _ = events::emit(
                    &store,
                    &hub,
                    Some(&project),
                    Some(&task),
                    Some(&agent),
                    "agent.progress",
                    json!({ "marker": marker }),
                );
            })
        };
        while rounds < round_limit {
            rounds += 1;
            // Marker runs (reviewers) get one explicit last round: the verdict must be
            // emitted now instead of more tool calls.
            if rounds == round_limit && !final_round_prompted {
                final_round_prompted = true;
                if config.finish_marker.is_some() {
                    // Marker runs (reviewers) get one explicit last round: the verdict must
                    // be emitted now instead of more tool calls.
                    transcript.push(ChatMessage::user(
                        "This is the final round. Do not call any tool. Emit the required structured block now, based on what you have already read.",
                    ));
                    transcript_json
                        .push(json!({ "role": "user", "text": "final round: emit the verdict" }));
                } else {
                    // Task agents hit the same wall live: a model that keeps inspecting
                    // evidence reaches the round ceiling without submit_result and the task
                    // is failed even though the work was done. Ask for the submission in the
                    // last round, and do not accept further tool calls there.
                    transcript.push(ChatMessage::user(
                        "This is the final round. Do not call any more tools. Call submit_result now with your summary, files_changed, commands_run and limitations.",
                    ));
                    transcript_json
                        .push(json!({ "role": "user", "text": "final round: call submit_result" }));
                }
            }
            if cancel.load(Ordering::SeqCst) {
                cancelled = true;
                break;
            }
            if started.elapsed() > deadline {
                timed_out = true;
                break;
            }
            self.store
                .heartbeat(&spec.agent_id, Some(&format!("round {rounds}")))?;
            let reply = match provider::generate(
                &config.target,
                system,
                &transcript,
                &tool_defs,
                config.round_timeout_ms,
            )
            .await
            {
                Ok(reply) => reply,
                Err(error) => {
                    self.store.set_agent_error(
                        &spec.agent_id,
                        &error.message,
                        crate::types::AgentStatus::Failed,
                    )?;
                    let _ = events::emit(
                        &self.store,
                        &self.hub,
                        Some(&spec.project_id),
                        Some(&spec.task_id),
                        Some(&spec.agent_id),
                        "agent.model_failure",
                        json!({ "code": error.code, "message": error.message }),
                    );
                    return Err(error);
                }
            };
            let round_cost = reply.cost_usd;
            usage_input += reply.usage.input_tokens.unwrap_or(0);
            usage_output += reply.usage.output_tokens.unwrap_or(0);
            cost_usd += round_cost;
            let _ = self.store.add_agent_usage(
                &spec.agent_id,
                reply.usage.input_tokens.unwrap_or(0),
                reply.usage.output_tokens.unwrap_or(0),
                round_cost,
            );
            let _ = self.store.record_usage(&crate::types::UsageRow {
                project_id: spec.project_id.clone(),
                task_id: Some(spec.task_id.clone()),
                agent_id: Some(spec.agent_id.clone()),
                provider: reply.provider.clone(),
                model: reply.model.clone(),
                input_tokens: reply.usage.input_tokens.unwrap_or(0),
                output_tokens: reply.usage.output_tokens.unwrap_or(0),
                cost_usd: round_cost,
                created_at: now_ms(),
            });
            if !reply.text.is_empty() {
                text_tail = reply.text.clone();
                transcript_json.push(json!({ "role": "assistant", "text": reply.text }));
            }
            // Persist the assistant turn including tool calls so the transcript is honest.
            let ModelReply {
                text,
                tool_calls,
                usage,
                finish_reason,
                ..
            } = reply;
            let _ = events::emit(
                &self.store,
                &self.hub,
                Some(&spec.project_id),
                Some(&spec.task_id),
                Some(&spec.agent_id),
                "agent.round",
                json!({
                    "round": rounds,
                    "finishReason": finish_reason,
                    "toolCalls": tool_calls.len(),
                    "inputTokens": usage.input_tokens,
                    "outputTokens": usage.output_tokens,
                }),
            );
            if tool_calls.is_empty() {
                if text.is_empty() {
                    // A reasoning-only round (no text, no tool call) is a real provider
                    // outcome. Give the model two explicit repair prompts, then fail with
                    // the exact reason instead of looping.
                    empty_rounds += 1;
                    if empty_rounds > 2 {
                        let error = NcagError::provider(format!(
                            "model returned an empty completion {empty_rounds} times (no text, no tool call)"
                        ));
                        self.store.set_agent_error(
                            &spec.agent_id,
                            &error.message,
                            crate::types::AgentStatus::Failed,
                        )?;
                        return Err(error);
                    }
                    transcript.push(ChatMessage::user(
                        "Your last reply contained neither text nor a tool call. \
                         Continue the task now: either call a tool, or call submit_result with your summary.",
                    ));
                    transcript_json
                        .push(json!({ "role": "user", "text": "empty completion repair" }));
                    continue;
                }
                if let Some(marker) = &config.finish_marker {
                    if text.contains(marker.as_str()) {
                        text_tail = text.clone();
                        transcript_json.push(json!({ "role": "assistant", "text": text }));
                        break;
                    }
                }
                // No tools and no submission: the agent must still submit; give the model
                // one explicit instruction instead of accepting prose as a result.
                transcript.push(ChatMessage::assistant(text));
                transcript.push(ChatMessage::user(
                    "You did not call submit_result. Finish by calling submit_result with your summary, \
                     files_changed, commands_run and limitations.",
                ));
                transcript_json.push(json!({ "role": "user", "text": "missing submit_result" }));
                continue;
            }
            transcript.push(self.assistant_with_calls(&text, &tool_calls));
            transcript_json.push(json!({
                "role": "assistant",
                "text": text,
                "toolCalls": tool_calls.iter().map(|call| json!({ "id": call.id, "name": call.name })).collect::<Vec<_>>(),
            }));
            for call in &tool_calls {
                let outcome = tools::invoke(&ctx, &call.name, &call.input, Some(&progress));
                let (model_text, tool_error) = match &outcome {
                    Ok(outcome) => (outcome.text.clone(), false),
                    Err(error) => (format!("[{}] {}", error.code, error.message), true),
                };
                let _ = events::emit(
                    &self.store,
                    &self.hub,
                    Some(&spec.project_id),
                    Some(&spec.task_id),
                    Some(&spec.agent_id),
                    "agent.tool",
                    json!({
                        "tool": call.name,
                        "callId": call.id,
                        "ok": !tool_error,
                        "input": summarize_tool_input(&call.name, &call.input),
                        "output": crate::tools::truncate_text(&model_text, 1200),
                    }),
                );
                let _ = self.store.append_audit(
                    Some(&spec.project_id),
                    &spec.agent_id,
                    "tool.executed",
                    &json!({
                        "taskId": spec.task_id,
                        "tool": call.name,
                        "ok": !tool_error,
                    }),
                    now_ms(),
                );
                transcript.push(ChatMessage::tool_result(
                    call.id.clone(),
                    model_text.clone(),
                ));
                transcript_json.push(json!({
                    "role": "tool",
                    "toolCallId": call.id,
                    "name": call.name,
                    "text": crate::tools::truncate_text(&model_text, 4000),
                }));
                if let Ok(outcome) = &outcome {
                    if let Some(result) = &outcome.submission {
                        submission = Some(result.clone());
                    }
                    if let Some(help) = &outcome.help_request {
                        help_request = Some(help.clone());
                    }
                }
            }
            if submission.is_some() {
                break;
            }
        }
        if submission.is_none() && !cancelled && !timed_out && config.finish_marker.is_none() {
            // Rounds exhausted without a submission: a failure with evidence, never invented.
            let error = NcagError::new(
                "max_rounds_exhausted",
                format!("agent reached {round_limit} model rounds without submit_result"),
            );
            self.store.set_agent_error(
                &spec.agent_id,
                &error.message,
                crate::types::AgentStatus::Failed,
            )?;
            let _ = events::emit(
                &self.store,
                &self.hub,
                Some(&spec.project_id),
                Some(&spec.task_id),
                Some(&spec.agent_id),
                "agent.stuck",
                json!({ "reason": "max_rounds", "rounds": rounds }),
            );
            return Err(error);
        }
        Ok(AgentRunOutcome {
            submission,
            rounds,
            text_tail,
            cancelled,
            timed_out,
            help_request,
            usage_input,
            usage_output,
            cost_usd,
            transcript: transcript_json,
        })
    }

    fn assistant_with_calls(
        &self,
        text: &str,
        calls: &[nc_code_provider::ModelToolCall],
    ) -> ChatMessage {
        ChatMessage {
            role: nc_code_provider::request::MessageRole::Assistant,
            text: text.to_string(),
            tool_call_id: None,
            tool_calls: calls
                .iter()
                .map(|call| ToolCallRequest {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                })
                .collect(),
            reasoning: Vec::new(),
        }
    }
}

/// The tool arguments recorded in `agent.tool` events. The full input is kept (bounded)
/// so stuck detection can tell "the same call three times" from "three different calls".
fn summarize_tool_input(tool: &str, input: &Value) -> Value {
    let compact = serde_json::to_string(input).unwrap_or_default();
    let bounded: String = compact.chars().take(600).collect();
    match tool {
        "run_command" => {
            json!({ "command": input.get("command").and_then(Value::as_str).unwrap_or("") })
        }
        "read_file" | "write_file" | "edit_file" | "list_dir" => {
            json!({ "path": input.get("path").and_then(Value::as_str).unwrap_or("") })
        }
        _ => json!(bounded),
    }
}

/// Retry policy: bounded attempts with exponential backoff metadata for the scheduler.
pub fn backoff_ms(attempt: u32, base_ms: u64) -> u64 {
    base_ms.saturating_mul(1u64 << attempt.min(6))
}

/// Observable behaviour of one agent, derived from real events and the worktree.
#[derive(Debug, Clone, Default)]
pub struct ActivityWindow {
    pub tool_calls: Vec<(String, usize)>,
    pub failed_commands: Vec<String>,
    pub new_files: usize,
    pub rounds: usize,
}
