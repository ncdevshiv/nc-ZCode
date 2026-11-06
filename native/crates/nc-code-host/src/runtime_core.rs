//! Runtime core business logic (ported from TS core/src/runtime).
//!
//! Core runtime functionality for agent execution.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub timeout_ms: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self { timeout_ms: 60000 }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeCore {
    config: RuntimeConfig,
}

impl RuntimeCore {
    pub fn new(config: RuntimeConfig) -> Self {
        Self { config }
    }

    pub fn timeout_ms(&self) -> u64 {
        self.config.timeout_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_core_default() {
        let rt = RuntimeCore::new(RuntimeConfig::default());
        assert_eq!(rt.timeout_ms(), 60000);
    }
}
