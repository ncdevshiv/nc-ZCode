//! Handler failures and their wire projection.
//!
//! TypeScript semantics being mirrored (`channelServer.ts` / `proxy-channel.ts`):
//! - a thrown `Error` → `PromiseError` with `{message, name, stack?}` plus the
//!   passthrough keys (`code`, `data`, …);
//! - a thrown non-Error value → `PromiseErrorObj` carrying the raw value;
//! - an unknown method throws `Error("Method not found: <name>")`.

use nc_code_rpc::channel::RpcErrorPayload;
use nc_code_rpc::WireValue;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum ChannelFailure {
    /// JS `Error`-shaped rejection → `ResponseType.PromiseError`.
    Error(Box<RpcErrorPayload>),
    /// JS non-Error rejection → `ResponseType.PromiseErrorObj`.
    Thrown(WireValue),
}

impl ChannelFailure {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(Box::new(RpcErrorPayload::new(message, "Error")))
    }

    pub fn error_named(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error(Box::new(RpcErrorPayload::new(message, name)))
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::error(format!("Method not found: {method}"))
    }

    pub fn event_not_found(event: &str) -> Self {
        Self::error(format!("Event not found: {event}"))
    }

    pub fn unknown_channel(channel: &str) -> Self {
        Self::error_named(
            "Unknown channel",
            format!("Channel name '{channel}' is not registered"),
        )
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        if let Self::Error(payload) = &mut self {
            payload.code = Some(Value::String(code.into()));
        }
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        if let Self::Error(payload) = &mut self {
            payload.data = Some(data);
        }
        self
    }

    /// `None` for [`ChannelFailure::Thrown`] (that variant has no payload projection).
    pub fn payload(&self) -> Option<&RpcErrorPayload> {
        match self {
            Self::Error(payload) => Some(payload),
            Self::Thrown(_) => None,
        }
    }
}

impl std::fmt::Display for ChannelFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error(payload) => write!(formatter, "{}: {}", payload.name, payload.message),
            Self::Thrown(value) => write!(formatter, "thrown non-Error value: {value:?}"),
        }
    }
}

impl std::error::Error for ChannelFailure {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each constructor carries the wire `name` the renderer switches on; a wrong
    /// name turns a protocol fact into an anonymous "Error".
    #[test]
    fn failure_constructors_keep_the_wire_names() {
        let missing = ChannelFailure::unknown_channel("git");
        assert_eq!(
            missing.payload().map(|p| p.name.as_str()),
            Some("Unknown channel")
        );
        let method = ChannelFailure::method_not_found("list");
        assert!(
            method.to_string().contains("Method not found: list"),
            "{method}"
        );
        let named = ChannelFailure::error_named("ZodError", "params.x is required");
        assert_eq!(named.payload().map(|p| p.name.as_str()), Some("ZodError"));
    }
}
