//! `Browser`: native browser-use tool for the Rust agent host.
//!
//! The Electron product exposed browser automation through the `browser-use`
//! plugin's node_repl bridge (`agent.browsers.*`). The Rust host has no JS
//! plugin runtime, so this tool carries the same `BrowserCommand` JSON shape
//! directly on the model surface and dispatches it through an injected
//! [`BrowserControlPort`].
//!
//! The host defines the port; the composition root injects the implementation
//! (the Tauri shell drives its child webview over CDP). Without a port the tool
//! fails closed with `browser_unavailable` — it never fabricates a page.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{AsyncTool, ToolContext, ToolContract, ToolError, ToolErrorCode, SCOPE_NETWORK};

/// Commands the host knows how to route. The shell decides which of them it can
/// actually execute on the current platform and returns `capability_unsupported`
/// for the rest.
pub const BROWSER_METHODS: &[&str] = &[
    "navigate",
    "back",
    "forward",
    "reload",
    "getState",
    "snapshot",
    "evaluate",
    "screenshot",
    "click",
    "fill",
    "type",
    "press",
    "scroll",
    "waitFor",
    "elementInfo",
    "playwright",
    "playwrightWaitForTimeout",
    "listUserTabs",
    "activateTab",
    "claimTab",
    "newTab",
    "finalize",
    "finalizeTabs",
    "markDeliverable",
    "markHandoff",
    "nameSession",
    "closeSession",
    "recordingStart",
    "recordingStatus",
    "recordingCancel",
    "close",
    "list",
];

/// Read-only commands never need a permission prompt.
const READ_ONLY_METHODS: &[&str] = &[
    "getState",
    "snapshot",
    "elementInfo",
    "playwrightWaitForTimeout",
    "listUserTabs",
    "list",
];

/// Playwright is read-only for snapshots/waits/locator readers; evaluate and
/// pointer/input operations still require approval.
fn playwright_read_only(input: &Value) -> bool {
    let Some(action) = input.get("action") else {
        return false;
    };
    match action.get("name").and_then(Value::as_str) {
        Some(
            "domSnapshot" | "elementInfo" | "elementScreenshot" | "waitForLoadState" | "waitForURL"
            | "waitForEvent" | "downloadPath",
        ) => true,
        Some("locator") => matches!(
            action.get("operation").and_then(Value::as_str),
            Some(
                "count"
                    | "allTextContents"
                    | "isVisible"
                    | "isEnabled"
                    | "textContent"
                    | "innerText"
                    | "getAttribute"
                    | "waitFor"
            )
        ),
        _ => false,
    }
}

/// Port trait for browser control.
///
/// Host defines this trait; the composition root injects an implementation.
/// Without one the `Browser` tool fails closed.
#[async_trait]
pub trait BrowserControlPort: Send + Sync {
    /// Enumerate the browser backends reachable from the current shell.
    async fn list(&self) -> Result<Value, String>;

    /// Execute one `BrowserCommand` and return the `BrowserCommandResult` JSON.
    async fn execute(&self, command: &Value) -> Result<Value, String>;
}

pub struct BrowserTool;

impl BrowserTool {
    fn method_of(input: &Value) -> Result<&str, ToolError> {
        let method = input
            .get("method")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ToolError::new(
                    ToolErrorCode::InvalidInput,
                    "browser command is missing a non-empty `method`",
                )
            })?;
        if !BROWSER_METHODS.contains(&method) {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                format!(
                    "unsupported browser method `{method}`; expected one of {}",
                    BROWSER_METHODS.join(", ")
                ),
            ));
        }
        Ok(method)
    }
}

#[async_trait]
impl AsyncTool for BrowserTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "Browser".to_string(),
            description: "Control the ZCode built-in browser tab. Commands: navigate {url}, back, \
                          forward, reload, getState, snapshot {maxElements?, includeHidden?}, \
                          evaluate {expression}, screenshot {fullPage?}, click {ref?|x,y}, \
                          fill {ref,value}, type {text}, press {key}, scroll {x?,y?}, \
                          waitFor {selector?|text?,timeoutMs?}, elementInfo {x,y}, playwright {action}, \
                          playwrightWaitForTimeout {timeoutMs}, list, \
                          listUserTabs, activateTab {tabId}, claimTab {tabId}, newTab, finalize {tabId?,deliverable?}, finalizeTabs {keep:[{tabId,status}]}, markDeliverable {tabId}, markHandoff {tabId}, nameSession {name}, closeSession, recordingStart {options?}, recordingStatus {recordingId,outputPath?}, recordingCancel {recordingId}, close {tabId}. Run snapshot first \
                          to get element refs, then click/fill by ref. Navigation and input \
                          mutate the page; snapshot/getState/elementInfo/listUserTabs/list are \
                          read-only."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["method"],
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": BROWSER_METHODS,
                    },
                    "url": { "type": "string" },
                    "expression": { "type": "string" },
                    "ref": { "type": "string" },
                    "x": { "type": "number" },
                    "y": { "type": "number" },
                    "value": { "type": "string" },
                    "text": { "type": "string" },
                    "name": { "type": "string" },
                    "deliverable": { "type": "boolean" },
                    "recordingId": { "type": "string" },
                    "outputPath": { "type": "string" },
                    "options": { "type": "object" },
                    "key": { "type": "string" },
                    "selector": { "type": "string" },
                    "timeoutMs": { "type": "number" },
                    "action": { "type": "object" },
                    "maxElements": { "type": "number" },
                    "includeHidden": { "type": "boolean" },
                    "fullPage": { "type": "boolean" },
                    "doubleClick": { "type": "boolean" },
                    "tabId": { "type": "string" },
                    "keep": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["tabId", "status"],
                            "properties": {
                                "tabId": { "type": "string" },
                                "status": { "type": "string", "enum": ["handoff", "deliverable"] }
                            },
                            "additionalProperties": false
                        }
                    },
                    "modifiers": { "type": "array", "items": { "type": "string" } }
                },
                "additionalProperties": false
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: SCOPE_NETWORK,
            timeout_ms: 60_000,
            max_output_bytes: 2 * 1024 * 1024,
            max_model_bytes: 256 * 1024,
            preview_direction: "head",
        }
    }

    fn is_read_only_invocation(&self, input: &Value) -> bool {
        let Some(method) = input.get("method").and_then(Value::as_str) else {
            return false;
        };
        if method == "playwright" {
            return playwright_read_only(input);
        }
        READ_ONLY_METHODS.contains(&method)
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let method = Self::method_of(input)?;
        let port = ctx.browser.as_ref().ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "browser_unavailable: no browser control port is attached to this host",
            )
        })?;

        let result = if method == "list" {
            port.list()
                .await
                .map(|browsers| json!({ "ok": true, "browsers": browsers }))
        } else {
            port.execute(input).await
        }
        .map_err(|error| {
            // The shell reports stable error codes; keep them on the wire so the
            // model can distinguish "unsupported" from "failed".
            ToolError::new(ToolErrorCode::ExecutionFailed, error)
        })?;

        serde_json::to_string(&result).map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("browser result serialize failed: {error}"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn ctx(browser: Option<Arc<dyn BrowserControlPort>>) -> ToolContext {
        ToolContext {
            workspace_root: std::env::temp_dir(),
            full_access: false,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            session_id: "test-session".to_string(),
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
            browser,
            computer: None,
            offpeak: None,
        }
    }

    #[test]
    fn contract_matches_the_browser_command_surface() {
        let contract = BrowserTool.contract();
        assert_eq!(contract.name, "Browser");
        assert!(!contract.read_only);
        assert_eq!(contract.side_effect_scope, SCOPE_NETWORK);
        let methods = contract.input_schema["properties"]["method"]["enum"]
            .as_array()
            .unwrap();
        assert!(methods.iter().any(|value| value == "navigate"));
        assert!(methods.iter().any(|value| value == "snapshot"));
        assert!(methods.iter().any(|value| value == "claimTab"));
        assert!(methods.iter().any(|value| value == "finalizeTabs"));
        assert!(methods.iter().any(|value| value == "newTab"));
        assert!(methods.iter().any(|value| value == "finalize"));
        assert!(methods.iter().any(|value| value == "nameSession"));
        assert!(methods.iter().any(|value| value == "closeSession"));
        assert!(methods.iter().any(|value| value == "recordingStart"));
        assert!(methods.iter().any(|value| value == "recordingStatus"));
        assert!(methods.iter().any(|value| value == "recordingCancel"));
        assert!(methods.iter().any(|value| value == "playwright"));
        assert!(methods
            .iter()
            .any(|value| value == "playwrightWaitForTimeout"));
    }

    #[test]
    fn read_only_classification_matches_the_command_effects() {
        let tool = BrowserTool;
        for method in [
            "getState",
            "snapshot",
            "elementInfo",
            "listUserTabs",
            "list",
        ] {
            assert!(
                tool.is_read_only_invocation(&json!({ "method": method })),
                "{method} must be read-only"
            );
        }
        for method in [
            "navigate",
            "click",
            "fill",
            "evaluate",
            "screenshot",
            "activateTab",
            "claimTab",
            "newTab",
            "finalize",
            "finalizeTabs",
            "markDeliverable",
            "markHandoff",
            "nameSession",
            "closeSession",
            "recordingStart",
            "recordingStatus",
            "recordingCancel",
            "close",
        ] {
            assert!(
                !tool.is_read_only_invocation(&json!({ "method": method })),
                "{method} must require approval"
            );
        }
        assert!(tool.is_read_only_invocation(
            &json!({ "method": "playwright", "action": { "name": "domSnapshot" } })
        ));
        assert!(tool.is_read_only_invocation(&json!({
            "method": "playwright",
            "action": { "name": "locator", "operation": "count" }
        })));
        assert!(!tool.is_read_only_invocation(&json!({
            "method": "playwright",
            "action": { "name": "locator", "operation": "click" }
        })));
        assert!(!tool.is_read_only_invocation(
            &json!({ "method": "playwright", "action": { "name": "evaluate" } })
        ));
        assert!(tool.is_read_only_invocation(
            &json!({ "method": "playwrightWaitForTimeout", "timeoutMs": 10 })
        ));
    }

    #[tokio::test]
    async fn fails_closed_without_a_port() {
        let error = BrowserTool
            .execute(&json!({ "method": "getState" }), &ctx(None))
            .await
            .unwrap_err();
        assert_eq!(error.code, ToolErrorCode::ExecutionFailed);
        assert!(error.message.contains("browser_unavailable"));
    }

    #[tokio::test]
    async fn rejects_missing_and_unknown_methods_before_touching_the_port() {
        let missing = BrowserTool
            .execute(&json!({}), &ctx(None))
            .await
            .unwrap_err();
        assert_eq!(missing.code, ToolErrorCode::InvalidInput);

        let unknown = BrowserTool
            .execute(&json!({ "method": "launchMissiles" }), &ctx(None))
            .await
            .unwrap_err();
        assert_eq!(unknown.code, ToolErrorCode::InvalidInput);
        assert!(unknown.message.contains("unsupported browser method"));
    }
}
