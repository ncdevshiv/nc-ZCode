//! TUI non-visual runtime (ported from CLI).
//!
//! Business logic for TUI runtime orchestration separated from rendering.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TuiProcessRuntimeState {
    pub provider_registry_initialized: bool,
    pub telemetry_shutdown_registered: bool,
}

impl TuiProcessRuntimeState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_provider_registry_initialized(&mut self) {
        self.provider_registry_initialized = true;
    }

    pub fn mark_telemetry_shutdown_registered(&mut self) {
        self.telemetry_shutdown_registered = true;
    }

    pub fn is_ready(&self) -> bool {
        self.provider_registry_initialized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tui_process_runtime_state_creation() {
        let state = TuiProcessRuntimeState::new();
        assert!(!state.provider_registry_initialized);
        assert!(!state.telemetry_shutdown_registered);
        assert!(!state.is_ready());
    }

    #[test]
    fn test_tui_process_runtime_state_markers() {
        let mut state = TuiProcessRuntimeState::new();
        state.mark_provider_registry_initialized();
        assert!(state.provider_registry_initialized);
        assert!(state.is_ready());
        state.mark_telemetry_shutdown_registered();
        assert!(state.telemetry_shutdown_registered);
    }
}
