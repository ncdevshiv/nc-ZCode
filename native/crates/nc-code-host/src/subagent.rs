//! Subagent (Task tool) runner and the session-level run registry.
//!
//! Aligned with the **foreground** contract of TS `core/src/tool/handlers/agent.ts`: one Task call runs a full
//! turn inside a child session and returns the subagent's final answer as the tool result. The run facts land in
//! the child session's own data directory (`sessions/<childId>/subagent-run.json`), and `session/subagents`
//! reads both the in-memory registry and the disk, so finished subagents are still listed after a restart.
//!
//! Boundaries with TS (`specs/disabled-features.md` §3.6, recorded):
//! - **Both foreground and background are supported**: `run_in_background: true` creates the child session and then runs the full turn in the background,
//!   returning the TS `async_launched` shape (agentId/backgroundTaskId/outputFile) right away; on
//!   completion it writes `subagent-output.txt`, settles the parent session's background entry and sends one completion notification.
//! - **Named profiles** (`crate::subagent_profiles`): the built-in `general-purpose` and the read-only `Explore`,
//!   plus user/project Markdown; the tool allow/deny lists, model, body, `injectAgentsMd` and `maxTurns`
//!   narrow the child turn per profile (recursion is still hard-forbidden by "the child registry has no Task tool"). Unsupported
//!   frontmatter fields are rejected by name at load time, so a run never silently degrades into a weaker agent.
//! - A subagent has no interaction client: non-read-only tools run under the parent session's current authorization (the Task call
//!   itself has already passed the permission gate); the built-in Explore is the exception — it runs with read-only permissions, and a non-read-only call is rejected at the gate.
//! - Of `running/waiting/blocked` only `running` has a real source; when no interaction/background state
//!   is observable, waiting/blocked are never fabricated.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::session::Session;
use crate::store::now_ms;
use crate::subagent_profiles::{self, AgentProfile};
use crate::tools::{AsyncTool, ToolContext, ToolContract, ToolError, ToolErrorCode};
use crate::turn::{self, TurnDeps, TurnState};
use crate::HostServer;

/// The child subagent session's `task_type` (the basis for excluding it from task lists and silently restoring it on restart).
pub const SUBAGENT_TASK_TYPE: &str = "subagent_child";
/// The subagent tool name. TS's **canonical name** is `Agent`; in TS `Task` is a
/// `providerVisible:false` backwards-compatible alias (plugin prompts often say Task). So what is registered here is
/// `Agent`, and the registry adds a separate `Task → Agent` alias so the tool surface the model sees matches TS.
pub const SUBAGENT_TOOL_NAME: &str = "Agent";
/// TS `TASK_TOOL_NAME`: used for resolution only, never appears in the provider tool list.
pub const SUBAGENT_TOOL_ALIAS: &str = "Task";
/// The agent type this host supports (the child session reuses the parent session's model and tool surface).
pub const SUPPORTED_SUBAGENT_TYPE: &str = "general-purpose";

/// One subagent run record (a superset of `zcodeSessionEndedSubagentSchema`; status is
/// `running` while live and success/failed/cancelled once ended).
#[derive(Debug, Clone)]
pub struct SubagentRun {
    pub child_session_id: String,
    pub agent_id: String,
    pub tool_call_id: Option<String>,
    pub subagent_type: String,
    pub title: String,
    pub summary: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub status: String,
}

impl SubagentRun {
    fn to_json(&self) -> Value {
        let mut value = json!({
            "childSessionId": self.child_session_id,
            "agentId": self.agent_id,
            "subagentType": self.subagent_type,
            "title": self.title,
            "startedAt": self.started_at,
            "status": self.status,
        });
        if let Some(tool_call_id) = &self.tool_call_id {
            value["toolCallId"] = json!(tool_call_id);
        }
        if let Some(summary) = &self.summary {
            value["summary"] = json!(summary);
        }
        if let Some(ended_at) = self.ended_at {
            value["endedAt"] = json!(ended_at);
        }
        value
    }
}

/// The in-process run registry: holds only the runs this process started (disk records fill the gap after a restart).
#[derive(Debug, Default)]
pub struct SubagentRegistry {
    runs: HashMap<String, Vec<SubagentRun>>,
}

impl SubagentRegistry {
    pub fn start(&mut self, parent_id: &str, run: SubagentRun) {
        self.runs
            .entry(parent_id.to_string())
            .or_default()
            .push(run);
    }

    pub fn finish(&mut self, parent_id: &str, child_id: &str, status: &str, ended_at: u64) {
        if let Some(runs) = self.runs.get_mut(parent_id) {
            if let Some(run) = runs.iter_mut().find(|run| run.child_session_id == child_id) {
                run.status = status.to_string();
                run.ended_at = Some(ended_at);
            }
        }
    }

    pub fn list(&self, parent_id: &str) -> Vec<SubagentRun> {
        self.runs.get(parent_id).cloned().unwrap_or_default()
    }
}

/// The subagent runner: the Task tool holds it; it uses the host's session factory to create the child session and run a full turn.
pub struct SubagentRunner {
    host: Arc<HostServer>,
    parent: Arc<Session>,
}

impl SubagentRunner {
    pub fn new(host: Arc<HostServer>, parent: Arc<Session>) -> Self {
        Self { host, parent }
    }

    /// Executes one foreground subagent run and returns the tool result in the `AgentCompletedOutput` shape.
    pub async fn run(
        &self,
        input: &Value,
        tool_call_id: Option<&str>,
    ) -> Result<String, ToolError> {
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| {
                ToolError::new(ToolErrorCode::InvalidInput, "description: required string")
            })?
            .to_string();
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| ToolError::new(ToolErrorCode::InvalidInput, "prompt: required string"))?
            .to_string();
        let run_in_background = input
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let subagent_type = input
            .get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or(SUPPORTED_SUBAGENT_TYPE);
        let profile = self.host.subagent_profile(subagent_type).ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::InvalidInput,
                format!(
                    "agent_unknown_type: unsupported subagent_type {subagent_type}; \
                     available: {}",
                    self.host.subagent_profile_names().join(", ")
                ),
            )
        })?;
        let mut provider = self.host.provider.clone().ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "agent_subagent_unavailable: Provider not configured",
            )
        })?;
        // The profile's model choice changes only this one child run: the parent session's model is untouched, so the child
        // runtime's budget/context policy matches the model it actually requests.
        let model_override = profile
            .model
            .as_deref()
            .map(subagent_profiles::split_canonical_model);
        if let Some((_, model, _)) = &model_override {
            provider.model = model.clone();
        }
        let mut seed = crate::sessions::SessionSeed::from_parent(&self.parent);
        seed.title = description.clone();
        seed.title_source = "custom".to_string();
        seed.parent_session_id = Some(self.parent.id.clone());
        seed.task_type = Some(SUBAGENT_TASK_TYPE.to_string());
        let child = self
            .host
            .open_session(seed)
            .map_err(|error| ToolError::new(ToolErrorCode::ExecutionFailed, error.message))?;
        if let Some((provider_id, model, level)) = &model_override {
            child.set_model_config(
                provider_id.as_deref().unwrap_or(provider.kind.as_str()),
                model,
                level.as_deref().unwrap_or(""),
            );
        }
        // The child session inherits the authorization of this Task call: reaching here means the parent session already allowed the Task call
        // (read-only passthrough or user approval/full-access). Explore is the exception — it is a read-only agent, so non-read-only tools must
        // be rejected at the permission gate (TS gives Explore its own read-only permission config).
        child.set_full_access_flag(!profile.is_builtin_explore());

        let started_at = now_ms().max(0) as u64;
        let run = SubagentRun {
            child_session_id: child.id.clone(),
            agent_id: child.id.clone(),
            tool_call_id: tool_call_id.map(str::to_string),
            subagent_type: subagent_type.to_string(),
            title: description.clone(),
            summary: None,
            started_at,
            ended_at: None,
            status: "running".to_string(),
        };
        self.host
            .record_subagent_start(&self.parent.id, run.clone());

        let deps = self.child_deps(&profile);
        if run_in_background {
            // A background subagent enters the **parent session's** background registry (TS `RuntimeTaskType=local_agent`).
            // work_id and agentId hold the same value — TaskOutput/TaskStop locate the task with it; the cancel flag points straight at
            // the child session's cancel switch, so once set the child turn stops at its own checkpoints. Output is written to
            // `subagent-output.txt`: TaskOutput reads it, sharing one read surface with bash tasks.
            let output_path = self.host.session_dir(&child.id).join("subagent-output.txt");
            self.parent.background.record_agent(
                &child.id,
                &description,
                output_path.clone(),
                tool_call_id,
                Arc::clone(&child.cancel),
                started_at as i64,
            );
            let host = Arc::clone(&self.host);
            let parent_id = self.parent.id.clone();
            let registry = Arc::clone(&self.parent.background);
            let child_id = child.id.clone();
            let background_child = Arc::clone(&child);
            let turn_id = self.host.next_id("subagent-turn");
            let source_command_id = format!("subagent-{turn_id}");
            let reported_prompt = prompt.clone();
            let reported_output_path = output_path.to_string_lossy().to_string();
            tokio::spawn(async move {
                let report = turn::run_subagent_turn(
                    background_child,
                    Some(&provider),
                    &deps,
                    &prompt,
                    &turn_id,
                    &source_command_id,
                )
                .await;
                let status = match report.state {
                    TurnState::Completed => "success",
                    TurnState::Failed => "failed",
                    TurnState::Interrupted => "cancelled",
                };
                let _ = std::fs::write(&output_path, &report.final_text);
                let settled = match status {
                    "success" => crate::background::BG_COMPLETED,
                    "cancelled" => crate::background::BG_CANCELLED,
                    _ => crate::background::BG_FAILED,
                };
                host.record_subagent_finish(&parent_id, &child_id, status, now_ms().max(0) as u64);
                // An entry already set to cancelled by TaskStop: settle only advances from running, so this is a noop.
                registry.settle_agent(&child_id, settled);
                // Completion notification (the Rust face of TS runTaskNotificationBatch): one task is notified exactly once.
                registry.notify_settled(&child_id);
            });
            return Ok(json!({
                "status": "async_launched",
                "isAsync": true,
                "agentId": child.id,
                "agentType": subagent_type,
                "description": description,
                "prompt": reported_prompt,
                "childSessionId": child.id,
                "backgroundTaskId": child.id,
                "outputFile": reported_output_path,
                "canReadOutputFile": true,
            })
            .to_string());
        }
        let turn_id = self.host.next_id("subagent-turn");
        let source_command_id = format!("subagent-{turn_id}");
        let child_turn = turn::run_subagent_turn(
            Arc::clone(&child),
            Some(&provider),
            &deps,
            &prompt,
            &turn_id,
            &source_command_id,
        );
        tokio::pin!(child_turn);
        let mut cancel_poll = tokio::time::interval(std::time::Duration::from_millis(100));
        let report = loop {
            tokio::select! {
                report = &mut child_turn => break report,
                _ = cancel_poll.tick() => {
                    // When the parent turn is stopped/cancelled, the subagent must stop at the same checkpoint (it cannot keep running in the background).
                    if self.parent.cancel.load(Ordering::SeqCst) {
                        child.request_cancel();
                    }
                }
            }
        };

        let status = match report.state {
            TurnState::Completed => "success",
            TurnState::Failed => "failed",
            TurnState::Interrupted => "cancelled",
        };
        let ended_at = now_ms().max(0) as u64;
        self.host
            .record_subagent_finish(&self.parent.id, &child.id, status, ended_at);
        if status != "success" {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!(
                    "agent_child_runtime_failed: subagent session {} ended {status}",
                    child.id
                ),
            ));
        }
        let rows = child.rows_snapshot();
        let tool_use_count = rows
            .iter()
            .filter(|row| row.get("kind").and_then(Value::as_str) == Some("toolCall"))
            .count();
        let usage = child.usage_report();
        let total_tokens = total_tokens(&usage);
        let output = json!({
            "status": "completed",
            "agentId": child.id,
            "agentType": subagent_type,
            "description": description,
            "prompt": prompt,
            "content": [{"type": "text", "text": report.final_text}],
            "totalToolUseCount": tool_use_count,
            "totalDurationMs": ended_at.saturating_sub(started_at),
            "totalTokens": total_tokens,
            "usage": usage,
        });
        Ok(output.to_string())
    }

    /// `SendMessage`: deliver one message to a subagent derived from this session.
    ///
    /// - **While running**: enqueue it into the child session's input queue (the child turn picks it up at its own drain point, the same
    ///   channel as queued user input); it does not interrupt the model step in flight.
    /// - **Once ended**: **resume** the child session in the background with this message (same semantics as TS `resumeTerminalAgentInBackground`),
    ///   re-register the background entry, and on completion write the output file the same way.
    /// - The target must be among **this session's** subagents (`subagent_runs_for(parent)`): blind cross-session
    ///   sends by id are not allowed.
    pub async fn send_message(&self, input: &Value) -> Result<String, ToolError> {
        let to = input
            .get("to")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::new(ToolErrorCode::InvalidInput, "to: required agent id"))?
            .to_string();
        let message = input
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ToolError::new(ToolErrorCode::InvalidInput, "message: required string")
            })?;
        let runs = self.host.subagent_runs_for(&self.parent.id);
        let Some(run) = runs
            .iter()
            .find(|run| run.agent_id == to || run.child_session_id == to)
            .cloned()
        else {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("No agent found with ID: {to}"),
            ));
        };
        let child = self
            .host
            .load_session(&run.child_session_id)
            .ok_or_else(|| {
                ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    format!("agent_session_unavailable: {}", run.child_session_id),
                )
            })?;
        if run.ended_at.is_none() {
            // Running: the same channel as queued user input (the child turn decides when to drain).
            let envelope = nc_code_core::protocol::CommandEnvelope {
                command_type: "sendText".to_string(),
                command_id: self.host.next_id("send-message"),
                client_id: "send_message".to_string(),
                session_id: Some(child.id.clone()),
                base_revision: None,
                base_log_epoch: None,
                payload: json!({"text": message}),
                issued_at: now_ms() as f64,
                ttft: None,
            };
            child.enqueue_input(
                envelope,
                message,
                "send_message".to_string(),
                now_ms(),
                None,
            );
            return Ok(json!({
                "success": true,
                "message": format!("Queued the message for running agent {to}."),
            })
            .to_string());
        }

        // Ended: resume in the background (re-register the run record and the background entry).
        // The resume reuses the profile the child session originally had; if that profile was later deleted it falls back to the built-in general-purpose
        // (the child session itself still exists, and its tool surface is the full set actually available to it).
        let profile = self
            .host
            .subagent_profile(&run.subagent_type)
            .unwrap_or_else(subagent_profiles::builtin_general_purpose);
        let mut provider = self.host.provider.clone().ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "agent_subagent_unavailable: Provider not configured",
            )
        })?;
        if let Some(canonical) = &profile.model {
            let (provider_id, model, level) = subagent_profiles::split_canonical_model(canonical);
            provider.model = model.clone();
            child.set_model_config(
                provider_id.as_deref().unwrap_or(provider.kind.as_str()),
                &model,
                level.as_deref().unwrap_or(""),
            );
        }
        child.set_full_access_flag(!profile.is_builtin_explore());
        let started_at = now_ms().max(0) as u64;
        self.host.record_subagent_start(
            &self.parent.id,
            SubagentRun {
                child_session_id: child.id.clone(),
                agent_id: run.agent_id.clone(),
                tool_call_id: run.tool_call_id.clone(),
                subagent_type: run.subagent_type.clone(),
                title: run.title.clone(),
                summary: None,
                started_at,
                ended_at: None,
                status: "running".to_string(),
            },
        );
        let output_path = self.host.session_dir(&child.id).join("subagent-output.txt");
        self.parent.background.remove(&run.agent_id);
        self.parent.background.record_agent(
            &run.agent_id,
            &run.title,
            output_path.clone(),
            run.tool_call_id.as_deref(),
            Arc::clone(&child.cancel),
            started_at as i64,
        );
        let host = Arc::clone(&self.host);
        let parent_id = self.parent.id.clone();
        let registry = Arc::clone(&self.parent.background);
        let agent_id = run.agent_id.clone();
        let deps = self.child_deps(&profile);
        let turn_id = self.host.next_id("subagent-turn");
        let source_command_id = format!("subagent-{turn_id}");
        let child_for_turn = Arc::clone(&child);
        tokio::spawn(async move {
            let report = turn::run_subagent_turn(
                child_for_turn,
                Some(&provider),
                &deps,
                &message,
                &turn_id,
                &source_command_id,
            )
            .await;
            let status = match report.state {
                TurnState::Completed => "success",
                TurnState::Failed => "failed",
                TurnState::Interrupted => "cancelled",
            };
            let _ = std::fs::write(&output_path, &report.final_text);
            let settled = match status {
                "success" => crate::background::BG_COMPLETED,
                "cancelled" => crate::background::BG_CANCELLED,
                _ => crate::background::BG_FAILED,
            };
            host.record_subagent_finish(&parent_id, &agent_id, status, now_ms().max(0) as u64);
            registry.settle_agent(&agent_id, settled);
            registry.notify_settled(&agent_id);
        });
        Ok(json!({
            "success": true,
            "message": format!("Resumed agent {to} in the background."),
        })
        .to_string())
    }

    /// The child session's turn dependencies: the parent session's context + a tool registry narrowed per profile (recursion hard-forbidden).
    fn child_deps(&self, profile: &AgentProfile) -> TurnDeps {
        let disallowed = profile.tool_disallowlist();
        let mut excluded: Vec<&str> = vec![SUBAGENT_TOOL_NAME];
        excluded.extend(disallowed.iter().map(String::as_str));
        let base = self.host.turn_deps.registry.without(&excluded);
        let registry = match profile.tool_allowlist() {
            Some(allowed) => base.only(&allowed),
            None => base,
        };
        let mut context = self.host.turn_deps.context.clone();
        if !profile.inject_agents_md {
            // TS `injectAgentsMd:false`: the workspace instruction-file hint is not brought into the child context (the body
            // was never injected either, only this "present: …" hint).
            context
                .system_blocks
                .retain(|block| !block.starts_with("Workspace instruction files present:"));
            context.instruction_files.clear();
        }
        if !profile.system_prompt.trim().is_empty() {
            context.system_blocks.push(profile.system_prompt.clone());
            context.estimated_context_bytes = context.system_blocks.iter().map(String::len).sum();
        }
        let deps = TurnDeps {
            registry: Arc::new(registry),
            context,
            // A custom agent inherits the parent's permissions; the built-in Explore is a read-only agent (non-read-only tools are rejected by the permission gate).
            full_access: !profile.is_builtin_explore(),
            provider_config: self.host.turn_deps.provider_config.clone(),
            host: std::sync::OnceLock::new(),
            max_tool_rounds: Some(
                profile
                    .max_turns
                    .unwrap_or(subagent_profiles::DEFAULT_MAX_TURNS),
            ),
        };
        let _ = deps.host.set(Arc::downgrade(&self.host));
        deps
    }
}

/// The `Task` tool (async: it must run the child session's full turn inside the same runtime).
pub struct TaskTool {
    profiles: Arc<Vec<AgentProfile>>,
}

impl TaskTool {
    pub fn new(profiles: Arc<Vec<AgentProfile>>) -> Self {
        Self { profiles }
    }
}

impl Default for TaskTool {
    fn default() -> Self {
        Self::new(Arc::new(vec![
            subagent_profiles::builtin_general_purpose(),
            subagent_profiles::builtin_explore(),
        ]))
    }
}

#[async_trait]
impl AsyncTool for TaskTool {
    fn contract(&self) -> ToolContract {
        let available = self
            .profiles
            .iter()
            .map(|profile| format!("- {}: {}", profile.name, profile.description))
            .collect::<Vec<String>>()
            .join("\n");
        ToolContract {
            name: SUBAGENT_TOOL_NAME.to_string(),
            description: format!(
                "Launch a subagent to handle a complex, multi-step task. The subagent runs in \
                 its own session with the toolset and model of the profile named by \
                 subagent_type; its final message is returned as the tool result. Available \
                 subagent types:\n{available}"
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "A short (3-5 word) description of the task"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task for the agent to perform"
                    },
                    "subagent_type": {
                        "type": "string",
                        "description": "The type of specialized agent to use (general-purpose)"
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "Run the agent asynchronously; poll TaskOutput with the returned agentId"
                    }
                },
                "required": ["description", "prompt"]
            }),
            read_only: false,
            destructive: true,
            concurrent_safe: false,
            side_effect_scope: crate::tools::SCOPE_WORKSPACE,
            timeout_ms: 3_600_000,
            max_output_bytes: 120_000,
            // TS Agent/Task: inline=meta=120_000, so the body budget takes min(...) = 120_000.
            max_model_bytes: 120_000,
            preview_direction: "head",
        }
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let Some(runner) = &ctx.subagent else {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "agent_subagent_unavailable: the Task tool is not wired into this host",
            ));
        };
        runner.run(input, ctx.tool_call_id.as_deref()).await
    }
}

/// The durable write/read of the run record (in the child session directory). The record is `session/subagents`' sole
/// source of truth after a restart, kept in a separate file from the transcript (it takes no part in history replay).
pub fn write_run_record(path: &PathBuf, run: &SubagentRun) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&run.to_json())?)?;
    std::fs::rename(&tmp, path)
}

pub fn read_run_record(path: &PathBuf) -> Option<SubagentRun> {
    let raw = std::fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let read_str = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    Some(SubagentRun {
        child_session_id: read_str("childSessionId")?,
        agent_id: read_str("agentId").unwrap_or_default(),
        tool_call_id: read_str("toolCallId"),
        subagent_type: read_str("subagentType").unwrap_or_else(|| "general-purpose".to_string()),
        title: read_str("title").unwrap_or_default(),
        summary: read_str("summary"),
        started_at: value.get("startedAt").and_then(Value::as_u64).unwrap_or(0),
        ended_at: value.get("endedAt").and_then(Value::as_u64),
        status: read_str("status").unwrap_or_else(|| "lost".to_string()),
    })
}

/// The host-level registration entry point (the runner and `session/subagents` share the same in-memory table).
impl HostServer {
    pub(crate) fn record_subagent_start(&self, parent_id: &str, run: SubagentRun) {
        let run_file = self.subagent_run_path(&run.child_session_id);
        if let Err(error) = write_run_record(&run_file, &run) {
            eprintln!(
                "[nc-code-host] subagent run record could not be written for {}: {error}",
                run.child_session_id
            );
        }
        if let Ok(mut registry) = self.subagent_runs.lock() {
            registry.start(parent_id, run);
        }
    }

    pub(crate) fn record_subagent_finish(
        &self,
        parent_id: &str,
        child_id: &str,
        status: &str,
        ended_at: u64,
    ) {
        let mut finished = None;
        if let Ok(mut registry) = self.subagent_runs.lock() {
            registry.finish(parent_id, child_id, status, ended_at);
            finished = registry
                .list(parent_id)
                .into_iter()
                .find(|run| run.child_session_id == child_id);
        }
        if let Some(run) = finished {
            let run_file = self.subagent_run_path(child_id);
            if let Err(error) = write_run_record(&run_file, &run) {
                eprintln!(
                    "[nc-code-host] subagent run record could not be updated for {child_id}: {error}"
                );
            }
        }
    }

    fn subagent_run_path(&self, child_id: &str) -> PathBuf {
        self.store.session_dir(child_id).join("subagent-run.json")
    }

    /// The data source for `session/subagents`: the in-memory registry + disk records (still listable after a restart).
    pub(crate) fn subagent_runs_for(&self, parent_id: &str) -> Vec<SubagentRun> {
        let mut runs = self
            .subagent_runs
            .lock()
            .map(|registry| registry.list(parent_id))
            .unwrap_or_default();
        let known: std::collections::HashSet<String> = runs
            .iter()
            .map(|run| run.child_session_id.clone())
            .collect();
        for session_id in self.store.list_sessions() {
            if known.contains(&session_id) {
                continue;
            }
            let Ok(meta) = self.store.load_meta(&session_id) else {
                continue;
            };
            if meta.parent_session_id.as_deref() != Some(parent_id)
                || meta.task_type.as_deref() != Some(SUBAGENT_TASK_TYPE)
            {
                continue;
            }
            if let Some(run) = read_run_record(&self.subagent_run_path(&session_id)) {
                runs.push(run);
            }
        }
        runs
    }
}

/// Counting helper: the total of `usage_report` (`inputTokens + outputTokens`).
pub(crate) fn total_tokens(usage: &Value) -> u64 {
    usage
        .get("inputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(
            usage
                .get("outputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_record_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("zcode-subagent-test-{}", now_ms()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("subagent-run.json");
        let run = SubagentRun {
            child_session_id: "session-1".to_string(),
            agent_id: "session-1".to_string(),
            tool_call_id: Some("call-1".to_string()),
            subagent_type: "general-purpose".to_string(),
            title: "explore the repo".to_string(),
            summary: None,
            started_at: 10,
            ended_at: Some(20),
            status: "success".to_string(),
        };
        write_run_record(&path, &run).expect("write");
        let read = read_run_record(&path).expect("read");
        assert_eq!(read.child_session_id, "session-1");
        assert_eq!(read.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(read.ended_at, Some(20));
        assert_eq!(read.status, "success");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn registry_tracks_start_and_finish_per_parent() {
        let mut registry = SubagentRegistry::default();
        registry.start(
            "parent-1",
            SubagentRun {
                child_session_id: "child-1".to_string(),
                agent_id: "child-1".to_string(),
                tool_call_id: None,
                subagent_type: "general-purpose".to_string(),
                title: "task".to_string(),
                summary: None,
                started_at: 1,
                ended_at: None,
                status: "running".to_string(),
            },
        );
        assert_eq!(registry.list("parent-1").len(), 1);
        assert_eq!(registry.list("parent-2").len(), 0);
        assert_eq!(registry.list("parent-1")[0].status, "running");
        registry.finish("parent-1", "child-1", "success", 9);
        let run = &registry.list("parent-1")[0];
        assert_eq!(run.status, "success");
        assert_eq!(run.ended_at, Some(9));
        assert_eq!(
            registry.list("parent-1").len(),
            1,
            "finish updates the same record in place without appending the second one"
        );
    }

    #[test]
    fn total_tokens_adds_input_and_output() {
        assert_eq!(
            total_tokens(&json!({"inputTokens": 10, "outputTokens": 4})),
            14
        );
        assert_eq!(total_tokens(&json!({})), 0);
    }
}
