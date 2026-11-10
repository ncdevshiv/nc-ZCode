//! Node-REPL session business logic.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 60_000,
            max_output_bytes: 10 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct Session {
    config: SessionConfig,
    disposed: bool,
    execution_count: u64,
}

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            config,
            disposed: false,
            execution_count: 0,
        }
    }

    /// The session limits this session was created with (timeout/output caps are
    /// enforced by the kernel; the session only carries them for inspection).
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    pub fn is_disposed(&self) -> bool {
        self.disposed
    }

    pub fn dispose(&mut self) {
        self.disposed = true;
    }

    pub fn increment_execution(&mut self) {
        self.execution_count += 1;
    }

    pub fn execution_count(&self) -> u64 {
        self.execution_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_lifecycle() {
        let mut session = Session::new(SessionConfig::default());
        assert!(!session.is_disposed());
        assert_eq!(session.execution_count(), 0);
        session.increment_execution();
        assert_eq!(session.execution_count(), 1);
        session.dispose();
        assert!(session.is_disposed());
    }
}
