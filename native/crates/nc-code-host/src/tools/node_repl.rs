//! `js` (node-repl): wires the capabilities of `nc-code-node-repl` onto the host's `Tool` trait.
//!
//! That crate deliberately depends only on `nc-code-proc` and is not compile-time coupled to the host (README "host wiring"),
//! so this layer is as thin as possible: the contract maps 1:1, inputs go through `ToolCall::parse`, execution goes through
//! `execute_cancellable` (the turn cancel bit is passed through) and the output is already the converged JSON the model sees.
//!
//! Differences from TS (recorded in `specs/rust-native-crates.md` S3):
//! - TS's `js` is gated by the official browser-use plugin (`includeNodeRepl`); this host has no plugin gate,
//!   so the tool is registered **always**, and when no usable Node exists the first call returns a typed error
//!   (`js_unavailable: …`) instead of letting the tool appear and disappear on the model surface.
//! - Detection (`node --version`) runs lazily on the first call: building the registry no longer pays a process spawn for `js`.
use super::{Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};
use nc_code_node_repl::{
    KernelMode, NodeReplError, NodeReplTool, ToolCall, ToolContractDescriptor,
};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
/// TS `MAX_NODE_REPL_DISPLAY_IMAGE_BASE64_BYTES` / `MAX_NODE_REPL_DISPLAY_IMAGES`。
const MAX_DISPLAY_IMAGE_BASE64_BYTES: usize = 200 * 1024;
const MAX_DISPLAY_IMAGES: usize = 2;
/// The request-context sibling key of TS `nc_code_MCP_NODE_REPL_CUA_APP_META_KEY`;
/// only host facts are passed here, no identity inference of any kind.
const REQUEST_CONTEXT_KEY: &str = "com.zcode/request-context";
pub struct JsTool {
    inner: OnceLock<Option<Arc<NodeReplTool>>>,
}
impl JsTool {
    pub fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }
    fn tool(&self) -> Result<Arc<NodeReplTool>, ToolError> {
        let detected = self.inner.get_or_init(|| {
            NodeReplTool::detect(KernelMode::Persistent)
                .ok()
                .map(Arc::new)
        });
        detected.clone().ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "js_unavailable: no supported Node.js runtime was found for the node_repl kernel",
            )
        })
    }
}
impl Default for JsTool {
    fn default() -> Self {
        Self::new()
    }
}
fn map_error(error: NodeReplError) -> ToolError {
    match error {
        NodeReplError::Cancelled(message) => ToolError::new(ToolErrorCode::Cancelled, message),
        NodeReplError::InvalidInput(message) => {
            ToolError::new(ToolErrorCode::InvalidInput, message)
        }
        NodeReplError::Unavailable(unavailable) => ToolError::new(
            ToolErrorCode::ExecutionFailed,
            format!("js_unavailable: {}", unavailable.message),
        ),
        other => ToolError::new(ToolErrorCode::ExecutionFailed, other.to_string()),
    }
}
impl Tool for JsTool {
    fn contract(&self) -> ToolContract {
        let descriptor = ToolContractDescriptor::js_tool_for(KernelMode::Persistent);
        ToolContract {
            name: descriptor.name,
            description: descriptor.description,
            input_schema: descriptor.input_schema,
            read_only: descriptor.read_only,
            destructive: descriptor.destructive,
            concurrent_safe: descriptor.concurrent_safe,
            side_effect_scope: descriptor.side_effect_scope,
            timeout_ms: descriptor.timeout_ms,
            max_output_bytes: descriptor.max_output_bytes,
            // TS's js direction is tail (the errors of an execution result are at the end).
            max_model_bytes: descriptor.max_output_bytes,
            preview_direction: "tail",
        }
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        let request_meta = json!({
            REQUEST_CONTEXT_KEY: {
                "traceId": ctx.trace_id,
                "sessionId": ctx.session_id,
                "turnId": ctx.turn_id,
                "toolCallId": ctx.tool_call_id,
            }
        });
        let mut call = ToolCall::parse(input, request_meta).map_err(map_error)?;
        // The persistent kernel is owned per session (TS node-repl.ts:366: top-level declarations persist across calls).
        call.session_key = Some(ctx.session_id.clone());
        let cancel: &AtomicBool = ctx.cancel.as_ref();
        let capped = self
            .tool()?
            .execute_cancellable(&call, cancel)
            .map_err(map_error)?;
        Ok(capped.json)
    }

    /// The `node_repl_images` display variant (TS `createNodeReplDisplay`): take `images` from this
    ///
    /// result, and set `truncated: true` when one image exceeds 200 KiB of base64 or there are more than 2 images.
    ///
    /// No images -> the display is absent (this host has no CUA app identity to project).
    fn result_display_for(&self, _input: &Value, output_text: &str) -> Option<Value> {
        let parsed: Value = serde_json::from_str(output_text).ok()?;
        let candidates = parsed.get("images")?.as_array()?;
        let mut images = Vec::new();
        let mut truncated = false;
        for candidate in candidates {
            let base64 = candidate.get("base64")?.as_str()?;
            let mime_type = candidate.get("mimeType")?.as_str()?;
            if base64.len() > MAX_DISPLAY_IMAGE_BASE64_BYTES {
                truncated = true;
                continue;
            }
            if images.len() >= MAX_DISPLAY_IMAGES {
                truncated = true;
                continue;
            }
            images.push(json!({"base64": base64, "mimeType": mime_type}));
        }
        if images.is_empty() {
            return None;
        }
        let mut display = json!({"kind": "node_repl_images", "images": images});
        if truncated {
            display["truncated"] = json!(true);
        }
        Some(display)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
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
            tool_call_id: Some("call-1".to_string()),
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
    #[test]
    fn contract_matches_the_node_repl_descriptor() {
        let tool = JsTool::new();
        let contract = tool.contract();
        let descriptor = ToolContractDescriptor::js_tool_for(KernelMode::Persistent);
        assert_eq!(contract.name, "js");
        assert_eq!(contract.name, descriptor.name);
        assert_eq!(contract.timeout_ms, descriptor.timeout_ms);
        assert_eq!(contract.side_effect_scope, "system");
        assert!(!contract.read_only);
        assert!(!contract.concurrent_safe);
    }
    #[test]
    fn invalid_input_is_rejected_before_any_kernel_is_spawned() {
        let error = JsTool::new()
            .execute(&json!({"code": "1+1", "unexpected": true}), &ctx())
            .expect_err("strict schema rejects unknown keys");
        assert_eq!(error.code, ToolErrorCode::InvalidInput);
    }
    #[test]
    fn display_projects_images_and_drops_oversized_ones() {
        let small = json!({"images": [{"base64": "aGk=", "mimeType": "image/png"}]}).to_string();
        let display = JsTool::new()
            .result_display_for(&json!({}), &small)
            .expect("display");
        assert_eq!(display["kind"], json!("node_repl_images"));
        assert_eq!(display["images"][0]["base64"], json!("aGk="));
        assert!(display.get("truncated").is_none());
        let oversized =
            json!({"images": [{"base64": "x".repeat(200 * 1024 + 1), "mimeType": "image/png"}]})
                .to_string();
        assert!(
            JsTool::new()
                .result_display_for(&json!({}), &oversized)
                .is_none(),
            "an oversized image leaves no display rather than an empty one"
        );
        assert!(JsTool::new()
            .result_display_for(&json!({}), &json!({"logs": ""}).to_string())
            .is_none());
    }

    /// Actually executes one piece of JS (when a local Node is available); when it is not, skip and say why.
    #[test]
    fn executes_real_javascript_when_node_is_available() {
        let tool = JsTool::new();
        if tool.tool().is_err() {
            eprintln!("skipping: no supported Node.js runtime on this machine");
            return;
        }
        let output = tool
            .execute(&json!({"code": "1 + 41"}), &ctx())
            .expect("js execution");
        let parsed: Value = serde_json::from_str(&output).expect("JsOutput json");
        assert_eq!(parsed["result"], json!("42"), "{output}");
    }
}
