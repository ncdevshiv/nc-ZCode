//! Plugin diagnostics shared by discovery, marketplace validation and the protocol
//! projection (`PluginDiagnostic` / `ZCodePluginDiagnostic`).
//!
//! TypeScript carries two shapes: the loader's `PluginDiagnostic` (with an optional
//! `path`) and the protocol's `ZCodePluginDiagnostic` (without it). Internal code keeps
//! the path for logging and `to_protocol_value` drops it exactly like `toPluginDiagnostic`.

use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub severity: Severity,
    pub plugin_id: Option<String>,
    pub path: Option<String>,
}

impl Diagnostic {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            severity: Severity::Error,
            plugin_id: None,
            path: None,
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            severity: Severity::Warning,
            plugin_id: None,
            path: None,
        }
    }

    pub fn with_plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.plugin_id = Some(plugin_id.into());
        self
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// `toPluginDiagnostic`: `{code, message, severity?, pluginId?}` (path dropped).
    pub fn to_protocol_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("code".into(), Value::String(self.code.clone()));
        map.insert("message".into(), Value::String(self.message.clone()));
        map.insert(
            "severity".into(),
            Value::String(self.severity.as_str().to_string()),
        );
        if let Some(plugin_id) = &self.plugin_id {
            map.insert("pluginId".into(), Value::String(plugin_id.clone()));
        }
        Value::Object(map)
    }
}

pub fn to_protocol_values(diagnostics: &[Diagnostic]) -> Vec<Value> {
    diagnostics
        .iter()
        .map(Diagnostic::to_protocol_value)
        .collect()
}
