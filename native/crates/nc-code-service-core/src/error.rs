//! `ServiceError` → channel `PromiseError` projection.

use nc_code_channels::ChannelFailure;
use nc_code_rpc::channel::RpcErrorPayload;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceError {
    pub name: String,
    pub message: String,
    pub code: Option<Box<Value>>,
    pub data: Option<Box<Value>>,
}

impl ServiceError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            name: "Error".to_string(),
            message: message.into(),
            code: None,
            data: None,
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self::new(message)
    }

    pub fn named(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
            code: None,
            data: None,
        }
    }

    /// Argument schema failures: TS throws a `ZodError`; keep the name so clients can
    /// distinguish bad input from a server fault, without pretending to be Zod.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::named("ZodError", message)
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(Box::new(Value::String(code.into())));
        self
    }

    pub fn with_code_value(mut self, code: Value) -> Self {
        self.code = Some(Box::new(code));
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(Box::new(data));
        self
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.name, self.message)
    }
}

impl std::error::Error for ServiceError {}

impl From<ServiceError> for ChannelFailure {
    fn from(error: ServiceError) -> Self {
        let mut payload = RpcErrorPayload::new(error.message, error.name);
        payload.code = error.code.map(|code| *code);
        payload.data = error.data.map(|data| *data);
        ChannelFailure::Error(Box::new(payload))
    }
}

impl From<std::io::Error> for ServiceError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<serde_json::Error> for ServiceError {
    fn from(error: serde_json::Error) -> Self {
        Self::named("SyntaxError", error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn service_error_maps_to_a_promise_error_payload() {
        let failure: ChannelFailure = ServiceError::new("boom")
            .with_code("guard.stale")
            .with_data(json!({"expected": 2}))
            .into();
        let payload = failure.payload().expect("Error payload");
        assert_eq!(payload.name, "Error");
        assert_eq!(payload.message, "boom");
        assert_eq!(payload.code, Some(json!("guard.stale")));
        assert_eq!(payload.data, Some(json!({"expected": 2})));
        assert!(payload.stack.is_none());
    }
}
