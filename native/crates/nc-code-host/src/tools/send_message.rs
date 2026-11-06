//! `SendMessage`: delivers a message to a subagent derived from this session (TS `core/src/tool/handlers/send-message.ts`).
//!
//! [`crate::subagent::SubagentRunner::send_message`] carries the semantics:
//! running -> push into the child session's input queue; already finished -> resume in the background. This file is only the tool surface
//! (contract + input validation + turning the runner's result into model-visible JSON).
//!
//! Difference from TS: TS rejects SendMessage outright in an off-peak turn - this host has no
//! off-peak turn concept, so that rejection does not exist here (recorded in `specs/rust-native-crates.md`).
use super::{AsyncTool, ToolContext, ToolContract, ToolError, ToolErrorCode};
use async_trait::async_trait;
use serde_json::Value;
const MAX_SEND_MESSAGE_MODEL_BYTES: usize = 4096;
pub struct SendMessageTool;
#[async_trait]
impl AsyncTool for SendMessageTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "SendMessage".to_string(),
            description: "Send a message to another agent. Your plain text output is NOT visible to other agents — to communicate, you MUST call this tool. Refer to local agents by the agentId returned in the Agent spawn result; a completed agent resumes in the background.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "agent id returned by the Agent tool"},
                    "summary": {"type": "string", "description": "short label for the message"},
                    "message": {"type": "string", "description": "message content"},
                },
                "required": ["to", "summary", "message"],
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: super::SCOPE_SESSION,
            timeout_ms: 10_000,
            max_output_bytes: MAX_SEND_MESSAGE_MODEL_BYTES,
            max_model_bytes: MAX_SEND_MESSAGE_MODEL_BYTES,
            preview_direction: "head",
        }
    }
    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        // TS's order: first `SendMessageInputSchema.parse` (contract validation), then look up the port.
        // `summary` is required by the strict schema; it plays no part in routing but must be rejected before any side effect.
        if input
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                "summary: required string",
            ));
        }
        let runner = ctx.subagent.as_ref().ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "Subagent port is not configured for SendMessage",
            )
        })?;
        runner.send_message(input).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    /// The runner needs a real HostServer; these two cases only cover the "no runner" fail-closed path.
    fn ctx() -> ToolContext {
        ToolContext {
            workspace_root: std::env::temp_dir(),
            full_access: true,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            session_id: "session-1".to_string(),
            model_ref: None,
            read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            tool_call_id: None,
            subagent: None,
            ask_user: None,
            plan_mode: false,
            plan: None,
            background: None,
            automation: None,
            browser: None,
            computer: None,
            offpeak: None,
            sessions: None,
            workflow: None,
        }
    }
    #[tokio::test]
    async fn missing_runner_fails_closed_with_the_ts_message() {
        let error = SendMessageTool
            .execute(
                &serde_json::json!({"to": "agent-1", "summary": "s", "message": "m"}),
                &ctx(),
            )
            .await
            .expect_err("no runner");
        assert!(error.message.contains("Subagent port is not configured"));
    }
    #[tokio::test]
    async fn summary_is_required_before_any_delivery() {
        let error = SendMessageTool
            .execute(
                &serde_json::json!({"to": "agent-1", "summary": " ", "message": "m"}),
                &ctx(),
            )
            .await
            .expect_err("empty summary");
        assert_eq!(error.code, ToolErrorCode::InvalidInput);
    }
}
