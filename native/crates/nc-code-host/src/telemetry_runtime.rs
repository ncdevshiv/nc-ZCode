//! Telemetry runtime business logic (ported from CLI).
//!
//! Business logic for telemetry separated from transport/export.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub service_name: String,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: None,
            service_name: "zcode".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TelemetryRuntime {
    config: TelemetryConfig,
    attributes: HashMap<String, String>,
}

impl TelemetryRuntime {
    pub fn new(config: TelemetryConfig) -> Self {
        Self {
            config,
            attributes: HashMap::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled && self.config.endpoint.is_some()
    }

    pub fn service_name(&self) -> &str {
        &self.config.service_name
    }

    pub fn set_attribute(&mut self, key: String, value: String) {
        self.attributes.insert(key, value);
    }

    pub fn get_attribute(&self, key: &str) -> Option<&String> {
        self.attributes.get(key)
    }

    pub fn attributes(&self) -> &HashMap<String, String> {
        &self.attributes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_telemetry_config_default() {
        let config = TelemetryConfig::default();
        assert!(config.enabled);
        assert_eq!(config.service_name, "zcode");
    }

    #[test]
    fn test_telemetry_runtime_disabled_without_endpoint() {
        let runtime = TelemetryRuntime::new(TelemetryConfig::default());
        assert!(!runtime.is_enabled());
    }

    #[test]
    fn test_telemetry_runtime_enabled_with_endpoint() {
        let config = TelemetryConfig {
            enabled: true,
            endpoint: Some("http://localhost:4318".to_string()),
            service_name: "test".to_string(),
        };
        let runtime = TelemetryRuntime::new(config);
        assert!(runtime.is_enabled());
        assert_eq!(runtime.service_name(), "test");
    }

    #[test]
    fn test_telemetry_attributes() {
        let mut runtime = TelemetryRuntime::new(TelemetryConfig::default());
        runtime.set_attribute("key1".to_string(), "value1".to_string());
        assert_eq!(runtime.get_attribute("key1"), Some(&"value1".to_string()));
    }
}
