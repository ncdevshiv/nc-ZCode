//! OffPeakCreate / OffPeakList tools
use super::{require_string, Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};
use serde_json::{json, Value};
use std::sync::Arc;

const OFF_PEAK_TIMEOUT_MS: u64 = 30000;
const OFF_PEAK_MODEL_BYTES: usize = 32000;

pub trait OffPeakPort: Send + Sync {
    fn create(
        &self,
        input: &serde_json::Value,
        session_id: Option<&str>,
    ) -> Result<serde_json::Value, String>;
    fn list(&self) -> Result<Vec<serde_json::Value>, String>;
}

fn port(ctx: &ToolContext, tool_name: &str) -> Result<Arc<dyn OffPeakPort>, ToolError> {
    let port = ctx.offpeak.as_ref().ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::ExecutionFailed,
            format!("OffPeakPort is not configured for {}", tool_name),
        )
    })?;
    Ok(Arc::clone(port))
}

fn service_error(tool_name: &str, message: String) -> ToolError {
    ToolError::new(
        ToolErrorCode::ExecutionFailed,
        format!("{}: {}", tool_name, message),
    )
}

pub struct OffPeakCreateTool;
impl Tool for OffPeakCreateTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "OffPeakCreate".to_string(),
            description: "Create an idle-time task.".to_string(),
            input_schema: json!({"type": "object", "properties": {"prompt": {"type": "string"}}, "required": ["prompt"]}),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: super::SCOPE_WORKSPACE,
            timeout_ms: OFF_PEAK_TIMEOUT_MS,
            max_output_bytes: 65536,
            max_model_bytes: OFF_PEAK_MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        let port = port(ctx, "OffPeakCreate")?;
        let prompt = require_string(input, "prompt")?.trim().to_string();
        if prompt.is_empty() {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                "prompt: must be a non-empty string",
            ));
        }
        let mut payload = input.clone();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("prompt".to_string(), json!(prompt));
        }
        let result = port
            .create(&payload, Some(&ctx.session_id))
            .map_err(|e| service_error("OffPeakCreate", e))?;
        Ok(serde_json::to_string(&result).unwrap_or_else(|_| result.to_string()))
    }
}

pub struct OffPeakListTool;
impl Tool for OffPeakListTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "OffPeakList".to_string(),
            description: "List idle-time tasks.".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: super::SCOPE_WORKSPACE,
            timeout_ms: OFF_PEAK_TIMEOUT_MS,
            max_output_bytes: 65536,
            max_model_bytes: OFF_PEAK_MODEL_BYTES,
            preview_direction: "head",
        }
    }
    fn execute(&self, _input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        let port = port(ctx, "OffPeakList")?;
        let tasks = port.list().map_err(|e| service_error("OffPeakList", e))?;
        Ok(serde_json::to_string(&json!({"tasks": tasks}))
            .unwrap_or_else(|_| json!({"tasks": tasks}).to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    struct DummyPort;
    impl OffPeakPort for DummyPort {
        fn create(&self, _input: &Value, _session_id: Option<&str>) -> Result<Value, String> {
            Ok(json!({"ok": true}))
        }
        fn list(&self) -> Result<Vec<Value>, String> {
            Ok(vec![])
        }
    }
    fn test_ctx() -> ToolContext {
        ToolContext {
            workspace_root: PathBuf::from("."),
            full_access: true,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            session_id: "s".to_string(),
            model_ref: None,
            tool_call_id: None,
            plan_mode: false,
            ask_user: None,
            plan: None,
            subagent: None,
            background: None,
            sessions: None,
            workflow: None,
            automation: None,
            browser: None,
            computer: None,
            offpeak: Some(Arc::new(DummyPort)),
        }
    }
    #[test]
    fn offpeak_create_contract() {
        let c = OffPeakCreateTool.contract();
        assert_eq!(c.name, "OffPeakCreate");
        assert!(!c.read_only);
        assert_eq!(c.input_schema["required"], json!(["prompt"]));
    }
    #[test]
    fn offpeak_list_contract() {
        let c = OffPeakListTool.contract();
        assert_eq!(c.name, "OffPeakList");
        assert!(c.read_only);
    }
    #[test]
    fn offpeak_create_validates_prompt_and_executes_with_port() {
        let ctx = test_ctx();
        let err = OffPeakCreateTool
            .execute(&json!({"prompt":"   "}), &ctx)
            .unwrap_err();
        assert_eq!(err.code, ToolErrorCode::InvalidInput);
        let ok = OffPeakCreateTool
            .execute(&json!({"prompt":"do it later"}), &ctx)
            .unwrap();
        assert!(ok.contains("ok"));
    }
    #[test]
    fn offpeak_list_executes_with_port() {
        let ctx = test_ctx();
        let ok = OffPeakListTool.execute(&json!({}), &ctx).unwrap();
        let v: Value = serde_json::from_str(&ok).unwrap();
        assert!(v["tasks"].is_array());
    }
}
