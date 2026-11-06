//! RespondToCoordinator tool
use super::{Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};
use serde_json::{json, Value};

const RESPOND_TIMEOUT_MS: u64 = 10000;
const RESPOND_MODEL_BYTES: usize = 4096;

pub struct RespondToCoordinatorTool;
impl Tool for RespondToCoordinatorTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "RespondToCoordinator".to_string(),
            description: "Respond to the coordinator that owns this subagent.".to_string(),
            input_schema: json!({"type": "object", "properties": {"summary": {"type": "string"}, "message": {"type": "string"}}, "required": ["summary", "message"]}),
            read_only: false,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: super::SCOPE_SESSION,
            timeout_ms: RESPOND_TIMEOUT_MS,
            max_output_bytes: 65536,
            max_model_bytes: RESPOND_MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, _input: &Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        Err(ToolError::new(
            ToolErrorCode::ExecutionFailed,
            "Coordinator response port is not configured for RespondToCoordinator",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn ctx() -> ToolContext {
        ToolContext {
            workspace_root: PathBuf::from("."),
            full_access: true,
            interactive: false,
            trace_id: "t".into(),
            turn_id: "turn".into(),
            session_id: "s".into(),
            model_ref: None,
            tool_call_id: None,
            plan_mode: false,
            read_state: Arc::new(Mutex::new(HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            ask_user: None,
            plan: None,
            subagent: None,
            background: None,
            sessions: None,
            workflow: None,
            automation: None,
            browser: None,
            computer: None,
            offpeak: None,
        }
    }

    #[test]
    fn contract_matches_boundary_expectations() {
        let c = RespondToCoordinatorTool.contract();
        assert_eq!(c.name, "RespondToCoordinator");
        assert!(!c.read_only);
        assert!(!c.destructive);
        assert!(c.concurrent_safe);
        assert_eq!(c.side_effect_scope, super::super::SCOPE_SESSION);
        assert_eq!(c.input_schema["required"], json!(["summary", "message"]));
        assert!(c.input_schema["properties"]["summary"].is_object());
        assert!(c.input_schema["properties"]["message"].is_object());
    }

    #[test]
    fn execute_fails_closed_without_coordinator_port() {
        let err = RespondToCoordinatorTool
            .execute(&json!({"summary":"s","message":"m"}), &ctx())
            .unwrap_err();
        assert_eq!(err.code, ToolErrorCode::ExecutionFailed);
        assert!(err
            .message
            .contains("Coordinator response port is not configured"));
    }
}
