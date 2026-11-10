//! NC-AG error type. One error shape crosses the API edge: `{code, message}`.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct NcagError {
    pub code: String,
    pub message: String,
    pub detail: Option<Value>,
}

pub type Result<T> = std::result::Result<T, NcagError>;

impl NcagError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new("not_found", what)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_request", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new("conflict", message)
    }

    pub fn provider(message: impl Into<String>) -> Self {
        Self::new("provider_unavailable", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }

    pub fn to_json(&self) -> Value {
        let mut body = serde_json::json!({ "code": self.code, "message": self.message });
        if let Some(detail) = &self.detail {
            body["detail"] = detail.clone();
        }
        serde_json::json!({ "error": body })
    }
}

impl std::fmt::Display for NcagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for NcagError {}

impl From<rusqlite::Error> for NcagError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new("storage_error", error.to_string())
    }
}

impl From<serde_json::Error> for NcagError {
    fn from(error: serde_json::Error) -> Self {
        Self::new("serialization_error", error.to_string())
    }
}

impl From<std::io::Error> for NcagError {
    fn from(error: std::io::Error) -> Self {
        Self::new("io_error", error.to_string())
    }
}

impl From<crate::state::TransitionError> for NcagError {
    fn from(error: crate::state::TransitionError) -> Self {
        Self::new("illegal_transition", error.to_string())
    }
}
