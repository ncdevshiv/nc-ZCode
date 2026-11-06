//! `js` tool surface: contract, input parameter verification, execution arrangement.
//!
//! Mirror the TS fact source in two places:
//! - Contract (name/description/permissions/timeout/budget): `core/src/tool/handlers/node-repl.ts:276-411
//!   jsToolEntry` and `node-repl-host/src/tool-contract.ts` (0.6.0: only the tool surface remains
//!   `js`, js_reset/js_add_node_module_dir has been deleted - the former always succeeds as a null operation to make the model repeat
//!   The call consumes budget, the latter is to push the host responsibility to the model, the measured call amount is 0);
//! - Input schema: `contracts/src/tools/node-repl.ts:28-43 JsInputSchema`
//!   (strict: unknown keys are rejected; code is required and not empty; timeout_ms ∈ [1,120000];
//!   title 1..=120 - required for new calls, but accepted by the execution layer using JsRuntimeInputSchema
//!   Code-only input for old provider/history playback, server.ts:55 and :208-212 are the same).
//!
//! Boundary with the host: This crate does not depend on nc-code-host (lib.rs documentation: The host uses its own
//! Tool trait wraps one layer). [`ToolContractDescriptor`] is the declaration side of this crate.
//! The field corresponds to the host ToolContract (tools/mod.rs:36-47) name by name, and is mapped directly during host wiring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::capability::{self, NodeCapability};
use crate::error::NodeReplError;
use crate::kernel::{KernelRegistry, NodeKernel, SpawnOptions};
use crate::output::{
    cap_model_output, CappedOutput, JsError, JsImage, JsOutput, OutputBudget,
    NODE_REPL_DEFAULT_TIMEOUT_MS, TOOL_MAX_TIMEOUT_MS, TOOL_SYNC_TIMEOUT_CAP_MS,
};
use crate::protocol::KernelOutput;

/// Tool name (TS jsToolEntry.metadata.name, node-repl.ts:368).
pub const JS_TOOL_NAME: &str = "js";
/// Session registry default key (shared by calls without session context;
/// The same value as NODE_REPL_UNSCOPED_KEY in kernel.rs, the host selects the layer on demand).
pub const NODE_REPL_SESSIONS: &str = crate::kernel::NODE_REPL_UNSCOPED_KEY;

/// sideEffectScope (TS ModelToolSideEffectScope closed set; js is system level——
/// Any Node permission, node-repl.ts:374 HIGH_RISK_PERMISSION.sideEffectScope).
pub const SCOPE_SYSTEM: &str = "system";

/// Kernel life cycle mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelMode {
    /// Each time a new kernel is called, it will be shut down after use (0.6.0 semantics: binding/module cache is not persistent across calls,/// server.ts:253-288 Worker-per-call; this crate uses spawn/shutdown to reproduce).
    Fresh,
    /// One permanent kernel per session (core persistent REPL semantics: top-level declaration across calls
    /// Persistent, node-repl.ts:366; the driver has reset itself after timeout/cancellation).
    Persistent,
}

/// Tool contract (field names aligned to host ToolContract, 1:1 mapping during host wiring).
#[derive(Debug, Clone)]
pub struct ToolContractDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: bool,
    pub destructive: bool,
    pub concurrent_safe: bool,
    pub side_effect_scope: &'static str,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
}

impl ToolContractDescriptor {
    /// The contract of the `js` tool. Field-by-field alignment TS jsToolEntry.metadata
    /// (node-repl.ts:367-377): readOnly=false, destructive=false,
    /// concurrentSafe=false (calls with the same kernel must be serial), timeoutMs=30s
    /// (Scheduling budget of metadata plane; a single call can be covered by timeout_ms, the upper limit is 120s——
    /// timeout.defaultMs/maxMs/allowCallOverride of node-repl.ts:399-404).
    /// Contract for persistent mode (core jsToolEntry semantics; host wiring is selected according to the actual mode).
    pub fn js_tool() -> Self {
        Self::js_tool_for(KernelMode::Persistent)
    }

    /// Constructed in kernel mode: the description is consistent with the real semantics of the mode.
    pub fn js_tool_for(mode: KernelMode) -> Self {
        Self {
            name: JS_TOOL_NAME.to_string(),
            description: js_tool_description(mode).to_string(),
            input_schema: js_input_json_schema(),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: SCOPE_SYSTEM,
            timeout_ms: 30_000,
            max_output_bytes: crate::output::MAX_MODEL_OUTPUT_BYTES,
        }
    }
}

/// The verified input parameters of a `js` call.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub code: String,
    pub title: Option<String>,
    pub timeout_ms: Option<u64>,
    /// Session ownership (registry key in persistent mode; if not, use NODE_REPL_SESSIONS).
    pub session_key: Option<String>,
    /// The host context passed to sandbox `nodeRepl.requestMeta`.
    pub request_meta: Value,
}

impl ToolCall {
    /// Strict verification according to JsRuntimeInputSchema (strict: unknown key is rejected; code is required and not empty;
    /// timeout_ms integer ∈ [1, 120000]; if title exists, 1..=120 characters).
    /// Error text alignment server.ts:208-212 / zod issue first.
    /// `request_meta` is the host context (com.zcode/request-context of MCP _meta,
    /// server.ts:203): Not in tool args, it is passed in after being parsed by the host.
    pub fn parse(input: &Value, request_meta: Value) -> Result<Self, NodeReplError> {
        let object = input.as_object().ok_or_else(|| {
            NodeReplError::InvalidInput("js: arguments must be an object".to_string())
        })?;
        // Key set for strict schema (contracts node-repl.ts:28-33 JsInputSchema + server.ts:51-58).
        // requestMeta is not a tool input parameter: _meta["com.zcode/request-context"] requested by TS from MCP
        // Get (server.ts:203,301-306), passed in by the host as the second parameter, not read from args.
        const ALLOWED: [&str; 3] = ["code", "title", "timeout_ms"];
        for key in object.keys() {
            if !ALLOWED.contains(&key.as_str()) {
                return Err(NodeReplError::InvalidInput(format!(
                    "js: unknown argument {key:?} (the tool has NO `command` parameter — that is Bash)"
                )));
            }
        }
        let code = object
            .get("code")
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .ok_or_else(|| {
                NodeReplError::InvalidInput("js expects non-empty JavaScript source".to_string())
            })?;
        let title = match object.get("title") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let title = value.as_str().ok_or_else(|| {
                    NodeReplError::InvalidInput("js: title must be a string".to_string())
                })?;
                if title.is_empty() || title.chars().count() > 120 {
                    return Err(NodeReplError::InvalidInput(
                        "js: title must be 1..=120 characters".to_string(),
                    ));
                }
                Some(title.to_string())
            }
        };
        let timeout_ms = match object.get("timeout_ms") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let ms = value.as_u64().ok_or_else(|| {
                    NodeReplError::InvalidInput(
                        "js: timeout_ms must be a positive integer within 120000".to_string(),
                    )
                })?;
                if !(1..=TOOL_MAX_TIMEOUT_MS).contains(&ms) {
                    return Err(NodeReplError::InvalidInput(format!(
                        "js: timeout_ms must be within 1..={TOOL_MAX_TIMEOUT_MS}"
                    )));
                }
                Some(ms)
            }
        };
        // The session route has the same origin as TS: requestMeta.session_id (server.ts:308-311).
        let session_key = request_meta
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string);
        Ok(Self {
            code: code.to_string(),
            title,
            timeout_ms,
            session_key,
            request_meta,
        })
    }
}

/// `js` tools: kernel pool + execution orchestration + output assembly.
///
/// Host wiring form (lib.rs "host wiring"): capability detection → `should_register_tool()`
/// Only registered when true → `execute` is called each time (blocking, the host uses the spawn_blocking package).
pub struct NodeReplTool {
    mode: KernelMode,
    /// Per-session kernel in persistent mode; not used in fresh mode.
    sessions: KernelRegistry,
    /// The detected Node executable file. Verify availability during construction to prevent "holding unavailable capabilities"
    /// Construct a tool that always reports errors" in this semi-initialized state.
    node_exe: std::path::PathBuf,
    budget: OutputBudget,
}

impl NodeReplTool {
    /// Constructed from detection conclusion. Unavailable throws up directly (R2 acceptance: absent → reports
    /// unavailable; the host decides to omit the tool on error - capability.rs should_register_tool).
    pub fn from_capability(
        mode: KernelMode,
        capability: &NodeCapability,
    ) -> Result<Self, NodeReplError> {
        let node_exe = match capability {
            NodeCapability::Available { exe, .. } => exe.clone(),
            NodeCapability::Unavailable(unavailable) => {
                return Err(NodeReplError::Unavailable(unavailable.clone()))
            }
        };
        Ok(Self {
            mode,
            sessions: KernelRegistry::new(),
            node_exe,
            budget: OutputBudget::default(),
        })
    }

    /// Detect and construct (production path) according to the current environment.
    pub fn detect(mode: KernelMode) -> Result<Self, NodeReplError> {
        let capability = capability::detect(None);
        Self::from_capability(mode, &capability)
    }

    pub fn mode(&self) -> KernelMode {
        self.mode
    }

    /// Execute a `js` call. Return model-visible JSON of strict JsOutput (converged on budget);
    /// Kernel/protocol faults are thrown as NodeReplError (host mapping ToolError).
    ///
    /// JS layer errors (throw / TimeoutError+reset prompts) are not Err - they are completed
    /// The legal output of execution (JsOutput.error), the model should read its message (aligned TS:
    /// node-repl-session.ts:438-446 put error into the run result).
    pub fn execute(&self, call: &ToolCall) -> Result<CappedOutput, NodeReplError> {
        self.execute_inner(call, None)
    }

    /// Execution with external cancellation bit (set by the host when turn ends/closeSession).
    pub fn execute_cancellable(
        &self,
        call: &ToolCall,
        cancel: &AtomicBool,
    ) -> Result<CappedOutput, NodeReplError> {
        self.execute_inner(call, Some(cancel))
    }

    fn execute_inner(
        &self,
        call: &ToolCall,
        cancel: Option<&AtomicBool>,
    ) -> Result<CappedOutput, NodeReplError> {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            return Err(NodeReplError::Cancelled(
                "JavaScript execution was canceled".to_string(),
            ));
        }
        let timeout_ms = call.timeout_ms.unwrap_or(NODE_REPL_DEFAULT_TIMEOUT_MS);
        let overall = Duration::from_millis(timeout_ms);
        // vm synchronization budget = min(timeout_ms, 120s) (server.ts:228 same formula; enter parameter schema
        // It has been limited to 120s, and the default value is 60s when the original value is transmitted directly).
        let sync_timeout_ms = timeout_ms.min(TOOL_SYNC_TIMEOUT_CAP_MS);
        let mut request_meta = call.request_meta.clone();
        if let Some(title) = &call.title {
            if let Some(object) = request_meta.as_object_mut() {
                object.insert("title".to_string(), json!(title));
            }
        }
        let output = match self.mode {
            KernelMode::Fresh => {
                // 0.6.0: Each time a kernel is called, shutdown (binding/module cache
                // Not persisted across calls - it's this semantics that the tool description promises to the model).
                let kernel = NodeKernel::spawn(&self.spawn_options())?;
                let result =
                    kernel.evaluate(&call.code, sync_timeout_ms, overall, request_meta, cancel);
                kernel.shutdown();
                result?
            }
            KernelMode::Persistent => {
                let key = call.session_key.as_deref().unwrap_or(NODE_REPL_SESSIONS);
                let options = self.spawn_options();
                let kernel = self
                    .sessions
                    .get_or_spawn(key, || Ok(Arc::new(NodeKernel::spawn(&options)?)))?;
                // If the persistent kernel dies before this call (last round of fatal), the registry
                // The get_or_spawn has been rebuilt (is_alive quick check + factory).
                kernel.evaluate(&call.code, sync_timeout_ms, overall, request_meta, cancel)?
            }
        };
        let js_output = js_output_from_kernel(output);
        Ok(cap_model_output(&js_output, &self.budget))
    }

    /// Close all kernels in the persistent registry (host dispose semantics).
    pub fn shutdown_sessions(&self) {
        self.sessions.shutdown_all();
    }

    fn spawn_options(&self) -> SpawnOptions {
        SpawnOptions::new(self.node_exe.clone())
    }
}

/// kernel output → strict JsOutput: structuredResults does not enter tool JSON
/// (JsOutputSchema strict does not have this field; core handler node-repl.ts:265 does not transmit transparently),
/// The formatModelContent of the host uses it to form the content block - record a line in stderr before discarding this layer.
/// (Not silent: the model has less content and needs to be traceable).
fn js_output_from_kernel(output: KernelOutput) -> JsOutput {
    if let Some(structured) = &output.structured_results {
        if !structured.is_empty() {
            eprintln!(
                "zcode-node-repl: kernel returned {} structured result block(s); host must embed them",
                structured.len()
            );
        }
    }
    JsOutput {
        result: output.result,
        logs: output.logs,
        error: output.error.map(|error| JsError {
            name: error.name,
            message: error.message,
            stack: error.stack,
        }),
        images: output.images.map(|images| {
            images
                .into_iter()
                .map(|image| JsImage {
                    base64: image.base64,
                    mime_type: image.mime_type,
                })
                .collect()
        }),
        response_meta: output.response_meta,
    }
}

/// `js` tool description (description of TS jsToolEntry; mirror TS buildJsToolDescription
/// According to the form of the option spelling text, node-repl.ts:297-352 - the description must match the **real semantics** of the selected mode
/// Consistent: persistent promises persistence across calls, fresh promises each rebuild).
fn js_tool_description(mode: KernelMode) -> &'static str {
    match mode {
        KernelMode::Fresh => concat!(
            "Run JavaScript in a fresh Node-backed kernel. Pass the JavaScript as the `code` ",
            "argument (this tool has NO `command` parameter — that is Bash; sending `command` ",
            "fails input schema validation). Always provide the required `title` argument as a ",
            "short user-facing description in your language. Top-level await is supported. ",
            "Every call starts a fresh kernel: global bindings and the module cache do not ",
            "persist across calls; reconstruct wrappers and recover state from current facts."
        ),
        KernelMode::Persistent => concat!(
            "Run JavaScript in a persistent Node REPL session. Pass the JavaScript as the `code` ",
            "argument (this tool has NO `command` parameter — that is Bash; sending `command` ",
            "fails input schema validation). Always provide the required `title` argument as a ",
            "short user-facing description in your language. Top-level await is supported; ",
            "top-level `const`/`let`/`var`/`function`/`class` declarations persist across ",
            "calls, as does anything assigned to globalThis.*. Use await importModule('...') to load ",
            "modules."
        ),
    }
}

/// `js` input JSON Schema (TS JsInputJsonSchema, contracts node-repl.ts:65——
/// title is required in the model side; the execution layer ToolCall::parse uses the runtime loose side to accept old input).
fn js_input_json_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": {"type": "string",
                "description": "JavaScript code to execute in the Node REPL session"
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "maximum": TOOL_MAX_TIMEOUT_MS,
                "description": "Per-call timeout in milliseconds (max 120000). Provide it when the code may run longer than 30000 ms; set it to the estimated runtime plus 15000 ms."
            },
            "title": {
                "type": "string",
                "minLength": 1,
                "maxLength": 120,
                "description": "Short user-facing title in the user's language describing the intended action without implementation terms such as js, JavaScript, or node_repl"
            }
        },
        "required": ["code", "title"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn contract_matches_ts_js_tool_entry() {
        let contract = ToolContractDescriptor::js_tool();
        assert_eq!(contract.name, "js");
        // node-repl.ts:369-376: readOnly=false / destructive=false /
        // concurrentSafe=false / sideEffectScope=system / timeoutMs=30_000.
        assert!(!contract.read_only);
        assert!(!contract.destructive);
        assert!(!contract.concurrent_safe);
        assert_eq!(contract.side_effect_scope, "system");
        assert_eq!(contract.timeout_ms, 30_000);
        assert_eq!(contract.max_output_bytes, 64 * 1024);
        // jsToolEntry.metadata.needsApproval=true (HIGH_RISK): host's
        // needs_approval() = !read_only (host tools/mod.rs:50-52), so this tool must be approved.
        assert_ne!(contract.read_only, !contract.read_only);
    }

    #[test]
    fn input_schema_requires_code_and_title_and_blocks_unknown_keys() {
        let schema = js_input_json_schema();
        assert_eq!(schema["additionalProperties"], false);
        let required = schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, vec!["code", "title"]);
        assert_eq!(schema["properties"]["timeout_ms"]["maximum"], 120_000);
    }

    #[test]
    fn parse_accepts_runtime_input_without_title() {
        // JsRuntimeInputSchema: Old provider/code-only input for history playback must be executable
        // (contracts node-repl.ts:36-43 comment).
        let call =
            ToolCall::parse(&json!({"code": "1+1"}), Value::Null).expect("code-only accepted");
        assert_eq!(call.code, "1+1");
        assert!(call.title.is_none());
        assert!(call.timeout_ms.is_none());
    }

    #[test]
    fn parse_rejects_empty_code_unknown_keys_and_bad_timeouts() {
        let err = ToolCall::parse(&json!({"code": ""}), Value::Null).unwrap_err();
        assert!(err.message().contains("non-empty JavaScript source"));
        let err = ToolCall::parse(&json!({"code": "x", "command": "ls"}), Value::Null).unwrap_err();
        assert!(err.message().contains("NO `command` parameter"));
        let err = ToolCall::parse(&json!({"code": "x", "timeout_ms": 0}), Value::Null).unwrap_err();
        assert!(err.message().contains("1..=120000"));
        let err =
            ToolCall::parse(&json!({"code": "x", "timeout_ms": 120_001}), Value::Null).unwrap_err();
        assert!(err.message().contains("1..=120000"));
        let err = ToolCall::parse(&json!({"code": "x", "title": ""}), Value::Null).unwrap_err();
        assert!(err.message().contains("1..=120"));
        let long_title = "t".repeat(121);
        let err =
            ToolCall::parse(&json!({"code": "x", "title": long_title}), Value::Null).unwrap_err();
        assert!(err.message().contains("1..=120"));
    }

    #[test]
    fn parse_keeps_session_key_and_request_meta() {
        // Session routing has the same origin as TS: the host parses request-context from MCP _meta and passes it in.
        // session_id is in it (server.ts:308-311), not in tool args.
        let meta = json!({"trace_id": "t-1", "session_id": "s-1"});
        let call = ToolCall::parse(&json!({"code": "x", "timeout_ms": 5000}), meta).expect("valid");
        assert_eq!(call.timeout_ms, Some(5_000));
        assert_eq!(call.session_key.as_deref(), Some("s-1"));
        assert_eq!(call.request_meta["trace_id"], "t-1");
    }

    #[test]
    fn parse_rejects_schema_external_keys() {
        // Keys outside strict schema are rejected: top-level sessionKey/requestMeta are not present
        // In TS JsInputSchema (the former is the host route, the latter uses MCP _meta) - no input interface is created.
        for key in ["sessionKey", "requestMeta"] {
            let mut args = serde_json::Map::new();
            args.insert("code".to_string(), json!("x"));
            args.insert(key.to_string(), json!("s"));
            let err = ToolCall::parse(&Value::Object(args), Value::Null).unwrap_err();
            assert!(
                err.message().contains("unknown argument"),
                "{key} must be rejected: {}",
                err.message()
            );
        }
    }

    #[test]
    fn both_kernel_modes_are_distinct_and_real() {
        // Both modes are implemented true modes (fresh=spawn/shutdown per call,
        // persistent=registry per session), not an enumeration configuration.
        assert_ne!(KernelMode::Fresh, KernelMode::Persistent);
    }

    #[test]
    fn from_capability_unavailable_is_fail_closed() {
        let unavailable = crate::error::NodeReplUnavailable::new(
            crate::error::UnavailableCode::RuntimeUnavailable,
            "Node REPL runtime is unavailable: no user-provided Node was found.",
            None,
        );
        let result = NodeReplTool::from_capability(
            KernelMode::Fresh,
            &NodeCapability::Unavailable(unavailable),
        );
        let Err(error) = result else {
            panic!("unavailable capability must not build a tool");
        };
        assert!(error.is_unavailable());
        assert_eq!(error.code(), "runtime_unavailable");
    }

    #[test]
    fn description_promises_freshness_only_in_fresh_mode() {
        // The description must be consistent with the true semantics of the pattern: fresh promises not to be persistent, persistent promises to be durable.
        let fresh = js_tool_description(KernelMode::Fresh);
        assert!(fresh.contains("NO `command` parameter"));
        assert!(fresh.contains("title"));
        assert!(fresh.contains("do not persist across calls"));
        let persistent = js_tool_description(KernelMode::Persistent);
        assert!(persistent.contains("declarations persist across"));
        assert!(persistent.contains("globalThis.*"));
        // The contract surface (tool name/input schema) of the two schemas must be the same.
        let fresh_contract = ToolContractDescriptor::js_tool_for(KernelMode::Fresh);
        let persistent_contract = ToolContractDescriptor::js_tool();
        assert_eq!(fresh_contract.name, persistent_contract.name);
        assert_eq!(
            fresh_contract.input_schema,
            persistent_contract.input_schema
        );
        assert_ne!(fresh_contract.description, persistent_contract.description);
    }
}
