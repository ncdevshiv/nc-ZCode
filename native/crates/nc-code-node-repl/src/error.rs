//! Error shapes.
//!
//! The fail-closed report shape **copies `nc-code-cua`**: when a capability is missing CUA throws a typed error
//! `{name, code, message, details?}` (`packages/nc-code-cua/broker.js:7-22`:
//! `BrokerError` defaults to `code: "unavailable"`, `CuaHelperError` defaults to
//! `code: "helper_unavailable"`, with a message shaped like "Computer Use Helper is unavailable.").
//! node_repl's missing Node is exactly isomorphic, so it uses the same fields and the same kind of naming to keep the product surface consistent:
//!
//! ```text
//! NodeReplUnavailableError { code: "runtime_unavailable", message: "Node REPL runtime is unavailable." }
//! ```
//!
//! The `error` field in the tool output is still bound by a strict schema
//! (the `apps/nc-code-cli/packages/contracts/src/tools/node-repl.ts:45-63` definition of
//! `JsOutputSchema.error = {name, message, stack?}` plus its `.strict()`),
//! so `code` does **not go into** the tool JSON; it only travels through the `[code] message`
//! prefix of `to_model_text()` (consistent with the host's `ToolError::to_model_text`,
//! `native/crates/nc-code-host/src/tools/mod.rs:93-95`) and the host-side structured checks.

use serde_json::{json, Value};

/// Stable, host-branchable error code for when Node is missing/unavailable (never judged from the message text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableCode {
    /// No Node found on this machine — R2's main fail-closed path, on which the host **omits the tool entirely**.
    RuntimeUnavailable,
    /// Node was found but sits below the kernel driver's minimum capability line.
    UnsupportedVersion,
    /// A candidate was found but the `node --version` probe failed / could not be parsed (unconfirmable means unavailable).
    ProbeFailed,
}

impl UnavailableCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::UnsupportedVersion => "unsupported_version",
            Self::ProbeFailed => "probe_failed",
        }
    }
}

/// The analogue of `nc-code-cua`'s `CuaHelperError`: the Node runtime is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeReplUnavailable {
    /// The type name aligned with CUA's `error.name`.
    pub name: &'static str,
    pub code: UnavailableCode,
    pub message: String,
    /// Diagnostic clues. **Contains only the search-source categories and counts**, never paths, environment values or evaluated code
    /// (see the `details` construction in capability.rs and the security boundary in the README).
    pub details: Option<Value>,
}

impl NodeReplUnavailable {
    pub const TYPE_NAME: &'static str = "NodeReplUnavailableError";

    pub fn new(code: UnavailableCode, message: impl Into<String>, details: Option<Value>) -> Self {
        Self {
            name: Self::TYPE_NAME,
            code,
            message: message.into(),
            details,
        }
    }

    /// Stable text the host uses for logging/decisions (CUA's `code` semantics; it never enters the model JSON).
    pub fn code_str(&self) -> &'static str {
        self.code.as_str()
    }

    /// Text fed back to the model: `[code] message`, the same shape as the host ToolError's.
    pub fn to_model_text(&self) -> String {
        format!("[{}] {}", self.code_str(), self.message)
    }

    /// The `error` object inside the tool output (strict `{name, message}`, no stack:
    /// this is not an error thrown by a JS realm, so there is no JS stack to pass back).
    pub fn to_tool_error(&self) -> Value {
        json!({ "name": self.name, "message": self.message })
    }
}

impl std::fmt::Display for NodeReplUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.name, self.message)
    }
}

impl std::error::Error for NodeReplUnavailable {}

/// The unified error type of this crate. `Unavailable` is the fail-closed channel, the rest are runtime faults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeReplError {
    /// Node missing / version too low / probe failed — the tool must be omitted or explicitly reported unavailable.
    Unavailable(NodeReplUnavailable),
    /// Invalid input arguments (the failure surface of the strict zod schema).
    InvalidInput(String),
    /// Kernel protocol/handshake mismatch (driver version disagrees with the Rust side).
    Protocol(String),
    /// The kernel process disappeared during the request (`process.exit`, killed, crashed).
    KernelDied(String),
    /// The call deadline was exceeded and no reply frame arrived within the grace period (the tree kill was escalated).
    TimedOut(String),
    /// The call was explicitly cancelled.
    Cancelled(String),
    /// Child-process/file IO failure.
    Io(String),
}

impl NodeReplError {
    pub fn unavailable(
        code: UnavailableCode,
        message: impl Into<String>,
        details: Option<Value>,
    ) -> Self {
        Self::Unavailable(NodeReplUnavailable::new(code, message, details))
    }

    /// fail-closed decision: the host can omit the tool from the registry based on it.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable(unavailable) => unavailable.code.as_str(),
            Self::InvalidInput(_) => "tool.invalidInput",
            Self::Protocol(_) => "nodeRepl.protocolMismatch",
            Self::KernelDied(_) => "nodeRepl.kernelDied",
            Self::TimedOut(_) => "tool.timeout",
            Self::Cancelled(_) => "tool.cancelled",
            Self::Io(_) => "nodeRepl.ioFailure",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(unavailable) => &unavailable.message,
            Self::InvalidInput(message)
            | Self::Protocol(message)
            | Self::KernelDied(message)
            | Self::TimedOut(message)
            | Self::Cancelled(message)
            | Self::Io(message) => message,
        }
    }

    /// Stable text fed back to the model (same shape as the host's `[code] message`).
    pub fn to_model_text(&self) -> String {
        format!("[{}] {}", self.code(), self.message())
    }

    /// The `error` object inside the tool output; cancellation/timeout also carry the kernel-reset hint
    /// (the reset notice on the kernel side is appended by the driver, see RESET_SUFFIX in src/driver/kernel_driver.mjs).
    pub fn to_tool_error(&self) -> Value {
        let name = match self {
            Self::Unavailable(unavailable) => unavailable.name,
            Self::InvalidInput(_) => "InvalidInputError",
            Self::Protocol(_) => "KernelProtocolError",
            Self::KernelDied(_) => "KernelDiedError",
            Self::TimedOut(_) => "TimeoutError",
            Self::Cancelled(_) => "AbortError",
            Self::Io(_) => "KernelIoError",
        };
        json!({ "name": name, "message": self.message() })
    }
}

impl std::fmt::Display for NodeReplError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_model_text())
    }
}

impl std::error::Error for NodeReplError {}

impl From<std::io::Error> for NodeReplError {
    fn from(error: std::io::Error) -> Self {
        // The message carries only io::Error's Display (includes the OS wording, never env/code content).
        Self::Io(error.to_string())
    }
}

impl From<serde_json::Error> for NodeReplError {
    fn from(error: serde_json::Error) -> Self {
        Self::Protocol(format!("kernel JSON frame is invalid: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeReplError, NodeReplUnavailable, UnavailableCode};
    use serde_json::json;

    #[test]
    fn unavailable_carries_the_cua_error_shape() {
        // CUA's shape is {name, code, message, details?} (packages/nc-code-cua/broker.js:7-22).
        let unavailable = NodeReplUnavailable::new(
            UnavailableCode::RuntimeUnavailable,
            "Node REPL runtime is unavailable.",
            Some(json!({ "searchedSources": ["path", "standardLocation"] })),
        );
        assert_eq!(unavailable.name, NodeReplUnavailable::TYPE_NAME);
        assert_eq!(unavailable.code_str(), "runtime_unavailable");
        assert_eq!(
            unavailable.to_model_text(),
            "[runtime_unavailable] Node REPL runtime is unavailable."
        );
    }

    #[test]
    fn tool_error_stays_inside_the_strict_js_output_schema() {
        // JsOutputSchema.error only allows {name, message, stack?}; code must never appear here.
        let error = NodeReplError::unavailable(
            UnavailableCode::UnsupportedVersion,
            "found v16.0.0 but the kernel needs >= v18.0.0",
            None,
        );
        let value = error.to_tool_error();
        assert_eq!(
            value,
            json!({
                "name": NodeReplUnavailable::TYPE_NAME,
                "message": "found v16.0.0 but the kernel needs >= v18.0.0"
            })
        );
        assert!(value.get("code").is_none());
        assert!(error.is_unavailable());
        assert_eq!(error.code(), "unsupported_version");
    }

    #[test]
    fn runtime_codes_are_distinguishable_without_message_matching() {
        // AGENTS.md: never depend on error text for flow decisions, so each failure kind has its own code.
        assert_eq!(
            NodeReplError::KernelDied("bye".to_string()).code(),
            "nodeRepl.kernelDied"
        );
        assert_eq!(
            NodeReplError::TimedOut("t".to_string()).code(),
            "tool.timeout"
        );
        assert_eq!(
            NodeReplError::Cancelled("c".to_string()).code(),
            "tool.cancelled"
        );
        assert!(!NodeReplError::Cancelled("c".to_string()).is_unavailable());
    }
}
