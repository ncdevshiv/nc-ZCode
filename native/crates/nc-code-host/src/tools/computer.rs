//! `Computer`: native computer-use (CUA) tool for the Rust agent host.
//!
//! The Electron product exposed computer use through the external
//! `computer-use` plugin (MCP server + native Helper). The Rust host has no JS
//! plugin runtime, so this tool carries a typed command surface directly on the
//! model side and dispatches it through an injected [`ComputerControlPort`].
//!
//! The host defines the contract; the composition root injects the platform
//! implementation (the Tauri shell drives Windows `SendInput`/GDI). Without a
//! port the tool fails closed with `computer_unavailable` — it never fabricates
//! a screen, cursor position, or key press.
//!
//! Observation is `listApps`/`getState`/`screenshot` (window list, foreground
//! window/cursor/screen, and a real PNG capture). Input is `activate`/`move`/
//! `click`/`scroll`/`type`/`press`/`drag`. `wait` is a bounded sleep.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{AsyncTool, ToolContext, ToolContract, ToolError, ToolErrorCode, SCOPE_SYSTEM};

/// Commands the host knows how to route. The shell returns
/// `capability_unsupported` for commands the current platform cannot execute.
pub const COMPUTER_METHODS: &[&str] = &[
    "listApps",
    "getState",
    "screenshot",
    "activate",
    "move",
    "click",
    "scroll",
    "type",
    "press",
    "drag",
    "wait",
];

/// Observation commands never need a permission prompt.
const READ_ONLY_METHODS: &[&str] = &["listApps", "getState", "screenshot", "wait"];

const MAX_WAIT_MS: u64 = 30_000;

/// Port trait for computer control.
///
/// Host defines this trait; the composition root injects an implementation.
/// Without one the `Computer` tool fails closed.
#[async_trait]
pub trait ComputerControlPort: Send + Sync {
    /// Execute one command and return the result JSON.
    async fn execute(&self, command: &Value) -> Result<Value, String>;
}

pub struct ComputerTool;

fn number(input: &Value, name: &str) -> Option<f64> {
    input.get(name).and_then(Value::as_f64)
}

fn required_number(input: &Value, name: &str) -> Result<f64, ToolError> {
    number(input, name).ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::InvalidInput,
            format!("computer command `{name}` must be a number"),
        )
    })
}

impl ComputerTool {
    fn method_of(input: &Value) -> Result<&str, ToolError> {
        let method = input
            .get("method")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ToolError::new(
                    ToolErrorCode::InvalidInput,
                    "computer command is missing a non-empty `method`",
                )
            })?;
        if !COMPUTER_METHODS.contains(&method) {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                format!(
                    "unsupported computer method `{method}`; expected one of {}",
                    COMPUTER_METHODS.join(", ")
                ),
            ));
        }
        Ok(method)
    }

    /// Validate required inputs before the port is touched, so a malformed
    /// command can never move the cursor or send keystrokes.
    fn validate(method: &str, input: &Value) -> Result<(), ToolError> {
        match method {
            "activate" => {
                let has_window = input
                    .get("windowId")
                    .and_then(Value::as_u64)
                    .is_some_and(|value| value > 0);
                let has_pid = input
                    .get("pid")
                    .and_then(Value::as_u64)
                    .is_some_and(|value| value > 0);
                if !has_window && !has_pid {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidInput,
                        "computer.activate requires `windowId` or `pid`",
                    ));
                }
            }
            "move" => {
                required_number(input, "x")?;
                required_number(input, "y")?;
            }
            "click" => {
                required_number(input, "x")?;
                required_number(input, "y")?;
                if let Some(button) = input.get("button").and_then(Value::as_str) {
                    if !matches!(button, "left" | "right" | "middle") {
                        return Err(ToolError::new(
                            ToolErrorCode::InvalidInput,
                            "computer.click `button` must be left, right or middle",
                        ));
                    }
                }
                if let Some(clicks) = input.get("clicks").and_then(Value::as_u64) {
                    if !(1..=3).contains(&clicks) {
                        return Err(ToolError::new(
                            ToolErrorCode::InvalidInput,
                            "computer.click `clicks` must be between 1 and 3",
                        ));
                    }
                }
            }
            "drag" => {
                required_number(input, "fromX")?;
                required_number(input, "fromY")?;
                required_number(input, "toX")?;
                required_number(input, "toY")?;
            }
            "type" => {
                let text = input.get("text").and_then(Value::as_str).ok_or_else(|| {
                    ToolError::new(ToolErrorCode::InvalidInput, "computer.type requires `text`")
                })?;
                if text.is_empty() {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidInput,
                        "computer.type `text` must not be empty",
                    ));
                }
            }
            "press" => {
                let key = input
                    .get("key")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        ToolError::new(
                            ToolErrorCode::InvalidInput,
                            "computer.press requires a non-empty `key`",
                        )
                    })?;
                let _ = key;
            }
            "scroll" => {
                if number(input, "deltaX").is_none() && number(input, "deltaY").is_none() {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidInput,
                        "computer.scroll requires `deltaX` or `deltaY`",
                    ));
                }
            }
            "wait" => {
                if let Some(timeout) = input.get("timeoutMs").and_then(Value::as_u64) {
                    if timeout > MAX_WAIT_MS {
                        return Err(ToolError::new(
                            ToolErrorCode::InvalidInput,
                            format!("computer.wait timeoutMs must be <= {MAX_WAIT_MS}"),
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[async_trait]
impl AsyncTool for ComputerTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "Computer".to_string(),
            description: "Control the desktop (computer use). Commands: listApps, getState, \
                          screenshot {windowId?|pid?|x?,y?,width?,height?}, activate {windowId?|pid?}, \
                          move {x,y}, click {x,y,button?,clicks?}, scroll {x?,y?,deltaX?,deltaY?}, \
                          type {text}, press {key,modifiers?}, drag {fromX,fromY,toX,toY}, \
                          wait {timeoutMs?}. listApps/getState/screenshot/wait are read-only; \
                          activation and input require approval. Coordinates are physical screen \
                          pixels; use listApps/getState to observe windows and the cursor."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["method"],
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": COMPUTER_METHODS,
                    },
                    "pid": { "type": "integer", "minimum": 1 },
                    "windowId": { "type": "integer", "minimum": 1 },
                    "x": { "type": "number" },
                    "y": { "type": "number" },
                    "fromX": { "type": "number" },
                    "fromY": { "type": "number" },
                    "toX": { "type": "number" },
                    "toY": { "type": "number" },
                    "width": { "type": "number" },
                    "height": { "type": "number" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"] },
                    "clicks": { "type": "integer", "minimum": 1, "maximum": 3 },
                    "deltaX": { "type": "number" },
                    "deltaY": { "type": "number" },
                    "text": { "type": "string" },
                    "key": { "type": "string" },
                    "modifiers": { "type": "array", "items": { "type": "string" } },
                    "timeoutMs": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS },
                    "maxBytes": { "type": "integer", "minimum": 1024 }
                },
                "additionalProperties": false
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: SCOPE_SYSTEM,
            timeout_ms: 60_000,
            max_output_bytes: 8 * 1024 * 1024,
            max_model_bytes: 128 * 1024,
            preview_direction: "head",
        }
    }

    fn is_read_only_invocation(&self, input: &Value) -> bool {
        input
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| READ_ONLY_METHODS.contains(&method))
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let method = Self::method_of(input)?;
        Self::validate(method, input)?;
        let port = ctx.computer.as_ref().ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "computer_unavailable: no computer control port is attached to this host",
            )
        })?;

        let result = port
            .execute(input)
            .await
            .map_err(|error| ToolError::new(ToolErrorCode::ExecutionFailed, error))?;

        serde_json::to_string(&result).map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("computer result serialize failed: {error}"),
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

    fn ctx(computer: Option<Arc<dyn ComputerControlPort>>) -> ToolContext {
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
            browser: None,
            computer,
            offpeak: None,
        }
    }

    struct EchoPort;

    #[async_trait]
    impl ComputerControlPort for EchoPort {
        async fn execute(&self, command: &Value) -> Result<Value, String> {
            Ok(json!({ "ok": true, "echo": command }))
        }
    }

    #[test]
    fn contract_matches_the_computer_command_surface() {
        let contract = ComputerTool.contract();
        assert_eq!(contract.name, "Computer");
        assert!(!contract.read_only);
        assert_eq!(contract.side_effect_scope, SCOPE_SYSTEM);
        let methods = contract.input_schema["properties"]["method"]["enum"]
            .as_array()
            .unwrap();
        for method in [
            "listApps",
            "getState",
            "screenshot",
            "activate",
            "move",
            "click",
            "scroll",
            "type",
            "press",
            "drag",
            "wait",
        ] {
            assert!(
                methods.iter().any(|value| value == method),
                "{method} must be advertised"
            );
        }
    }

    #[test]
    fn read_only_classification_matches_the_command_effects() {
        let tool = ComputerTool;
        for method in ["listApps", "getState", "screenshot", "wait"] {
            assert!(
                tool.is_read_only_invocation(&json!({ "method": method })),
                "{method} must be read-only"
            );
        }
        for method in [
            "activate", "move", "click", "scroll", "type", "press", "drag",
        ] {
            assert!(
                !tool.is_read_only_invocation(&json!({ "method": method })),
                "{method} must require approval"
            );
        }
    }

    #[tokio::test]
    async fn fails_closed_without_a_port() {
        let error = ComputerTool
            .execute(&json!({ "method": "getState" }), &ctx(None))
            .await
            .unwrap_err();
        assert_eq!(error.code, ToolErrorCode::ExecutionFailed);
        assert!(error.message.contains("computer_unavailable"));
    }

    #[tokio::test]
    async fn rejects_missing_and_unknown_methods_before_touching_the_port() {
        let missing = ComputerTool
            .execute(&json!({}), &ctx(None))
            .await
            .unwrap_err();
        assert_eq!(missing.code, ToolErrorCode::InvalidInput);

        let unknown = ComputerTool
            .execute(&json!({ "method": "launchMissiles" }), &ctx(None))
            .await
            .unwrap_err();
        assert_eq!(unknown.code, ToolErrorCode::InvalidInput);
        assert!(unknown.message.contains("unsupported computer method"));
    }

    #[tokio::test]
    async fn validates_input_commands_before_touching_the_port() {
        let port: Arc<dyn ComputerControlPort> = Arc::new(EchoPort);
        let cases = [
            json!({ "method": "activate" }),
            json!({ "method": "move", "x": 1 }),
            json!({ "method": "click", "x": 1, "y": 2, "button": "fourth" }),
            json!({ "method": "click", "x": 1, "y": 2, "clicks": 4 }),
            json!({ "method": "drag", "fromX": 1, "fromY": 2 }),
            json!({ "method": "type", "text": "" }),
            json!({ "method": "press" }),
            json!({ "method": "scroll" }),
            json!({ "method": "wait", "timeoutMs": 30_001 }),
        ];
        for input in cases {
            let error = ComputerTool
                .execute(&input, &ctx(Some(Arc::clone(&port))))
                .await
                .unwrap_err();
            assert_eq!(
                error.code,
                ToolErrorCode::InvalidInput,
                "input {input} must be rejected before the port"
            );
        }
    }

    #[tokio::test]
    async fn forwards_valid_commands_to_the_port() {
        let port: Arc<dyn ComputerControlPort> = Arc::new(EchoPort);
        let output = ComputerTool
            .execute(
                &json!({ "method": "move", "x": 10, "y": 20 }),
                &ctx(Some(port)),
            )
            .await
            .expect("execute");
        let value: Value = serde_json::from_str(&output).expect("json");
        assert_eq!(value["ok"], true);
        assert_eq!(value["echo"]["method"], "move");
        assert_eq!(value["echo"]["x"], 10);
    }
}
