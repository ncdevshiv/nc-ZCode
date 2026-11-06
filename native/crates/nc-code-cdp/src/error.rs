//! Error types: CDP transport, protocol, command and launch failures are first-class design objects
//! that bubble up to the caller; this crate never silently absorbs them (aligned with the AGENTS.md
//! "error handling first" rule).

use std::fmt;

/// The structured error codes: a mirror of the subset that the raw CDP path can produce within the TS
/// `browserErrorCodeSchema` (`packages/shared/src/browser-use/command-metadata.ts:144`). Host wiring
/// maps them onto protocol error codes, so flows are never decided by error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdpErrorCode {
    /// No Chromium-family browser could be discovered (fail-closed).
    BackendUnavailable,
    /// The browser process failed to launch or the WS handshake failed.
    LaunchFailed,
    /// The command id correlation timed out or the transport read timed out.
    Timeout,
    /// A method-level error returned by CDP (`{id, error:{code, message}}`).
    Protocol,
    /// The target/session is no longer valid (tab closed, browser disconnected).
    TargetGone,
    /// The snapshot handle (ref) is stale or unavailable.
    RefNotFound,
    /// The URL was rejected by the navigation policy.
    NavigationBlocked,
    /// The command is unsupported on the current raw CDP surface (Firefox/WebKit, locator engine, recording, etc.).
    CapabilityUnsupported,
    /// Cancelled by the caller (turn ended / closeSession).
    Cancelled,
    /// An IO error in the transport layer.
    Transport,
    /// An illegal input shape or a runtime error (maps to TS `execution_error`).
    ExecutionError,
}

impl CdpErrorCode {
    /// The wire code: mapped onto the strict set of the TS `browserErrorCodeSchema`
    /// (`packages/shared/src/browser-use/command-metadata.ts:144-154`, 9 values in total). This enum is
    /// a superset (it contains the four Rust internal diagnostic codes launch_failed/protocol_error/
    /// target_gone/transport_error), but once the result JSON enters `browserCommandResultSchema`
    /// (strict) it may only carry codes from the schema —— TS's `classifyError` (request.ts:48-55) falls
    /// back to execution_error for unrecognized errors too, and classifies closed/disconnected wording as
    /// backend_unavailable; the mapping here aligns with those two criteria one by one.
    pub fn wire_code(self) -> &'static str {
        match self {
            Self::BackendUnavailable | Self::LaunchFailed | Self::TargetGone | Self::Transport => {
                "backend_unavailable"
            }
            Self::Timeout => "timeout",
            Self::Protocol | Self::ExecutionError => "execution_error",
            Self::RefNotFound => "ref_not_found",
            Self::NavigationBlocked => "navigation_blocked",
            Self::CapabilityUnsupported => "capability_unsupported",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BackendUnavailable => "backend_unavailable",
            Self::LaunchFailed => "launch_failed",
            Self::Timeout => "timeout",
            Self::Protocol => "protocol_error",
            Self::TargetGone => "target_gone",
            Self::RefNotFound => "ref_not_found",
            Self::NavigationBlocked => "navigation_blocked",
            Self::CapabilityUnsupported => "capability_unsupported",
            Self::Cancelled => "cancelled",
            Self::Transport => "transport_error",
            Self::ExecutionError => "execution_error",
        }
    }
}

/// The unified error of the CDP layer. `message` is aimed at the model/logs (must contain no credentials or user privacy, per the AGENTS.md logging rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpError {
    pub code: CdpErrorCode,
    pub message: String,
}

impl CdpError {
    pub fn new(code: CdpErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn protocol(id: u64, code: i64, message: String) -> Self {
        Self::new(
            CdpErrorCode::Protocol,
            format!("CDP method for request {id} failed (code {code}): {message}"),
        )
    }
}

impl fmt::Display for CdpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for CdpError {}

pub type CdpResult<T> = Result<T, CdpError>;

#[cfg(test)]
mod tests {
    #[test]
    fn basic_test() {
        assert!(true);
    }
}
