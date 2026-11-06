//! Node-REPL business logic (ported from CLI).
//!
//! Business logic for node-repl session management separated from transport.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeReplConfig {
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub allow_network: bool,
}

impl Default for NodeReplConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 60_000,
            max_output_bytes: 10 * 1024 * 1024,
            allow_network: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NodeReplSessionState {
    config: NodeReplConfig,
    disposed: bool,
    run_count: u64,
}

impl NodeReplSessionState {
    pub fn new(config: NodeReplConfig) -> Self {
        Self {
            config,
            disposed: false,
            run_count: 0,
        }
    }

    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    pub fn dispose(&mut self) {
        self.disposed = true;
    }

    pub fn increment_run_count(&mut self) {
        self.run_count += 1;
    }

    pub fn run_count(&self) -> u64 {
        self.run_count
    }

    pub fn config(&self) -> &NodeReplConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_repl_session_state() {
        let mut state = NodeReplSessionState::new(NodeReplConfig::default());
        assert!(!state.is_disposed());
        assert_eq!(state.run_count(), 0);
        state.increment_run_count();
        assert_eq!(state.run_count(), 1);
        state.dispose();
        assert!(state.is_disposed());
    }

    #[test]
    fn test_node_repl_config_default() {
        let config = NodeReplConfig::default();
        assert_eq!(config.timeout_ms, 60_000);
        assert!(config.allow_network);
    }
}
