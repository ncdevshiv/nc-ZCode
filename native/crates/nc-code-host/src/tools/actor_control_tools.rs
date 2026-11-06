//! submit_result and escalate tools (for parity)
use super::{Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};
use serde_json::{json, Value};

const TIMEOUT_MS: u64 = 10000;
const MODEL_BYTES: usize = 4096;

pub struct SubmitResultTool;
impl Tool for SubmitResultTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "submit_result".to_string(),
            description: "Submit the structured terminal result.".to_string(),
            input_schema: json!({"type": "object", "properties": {}, "required": []}),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: super::SCOPE_SESSION,
            timeout_ms: TIMEOUT_MS,
            max_output_bytes: 65536,
            max_model_bytes: MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, _input: &Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        Err(ToolError::new(
            ToolErrorCode::ExecutionFailed,
            "submit_result is not configured in this host context",
        ))
    }
}

pub struct EscalateTool;
impl Tool for EscalateTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "escalate".to_string(),
            description: "Escalate to the coordinator.".to_string(),
            input_schema: json!({"type": "object", "properties": {}, "required": []}),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: super::SCOPE_SESSION,
            timeout_ms: TIMEOUT_MS,
            max_output_bytes: 65536,
            max_model_bytes: MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, _input: &Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        Err(ToolError::new(
            ToolErrorCode::ExecutionFailed,
            "escalate is not configured in this host context",
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
    fn contracts_have_expected_names_and_session_scope() {
        let sr = SubmitResultTool.contract();
        let esc = EscalateTool.contract();
        assert_eq!(sr.name, "submit_result");
        assert_eq!(esc.name, "escalate");
        assert_eq!(sr.side_effect_scope, super::super::SCOPE_SESSION);
        assert_eq!(esc.side_effect_scope, super::super::SCOPE_SESSION);
        assert!(!sr.read_only);
        assert!(!esc.read_only);
    }

    #[test]
    fn sync_stubs_fail_closed_without_workflow_context() {
        let sr = SubmitResultTool.execute(&json!({}), &ctx()).unwrap_err();
        assert_eq!(sr.code, ToolErrorCode::ExecutionFailed);
        assert!(sr.message.contains("not configured"));
        let esc = EscalateTool.execute(&json!({}), &ctx()).unwrap_err();
        assert_eq!(esc.code, ToolErrorCode::ExecutionFailed);
        assert!(esc.message.contains("not configured"));
    }
}
