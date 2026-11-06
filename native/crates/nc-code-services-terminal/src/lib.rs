//! `terminal` service channel (Rust backend), the counterpart of
//! `packages/services/src/terminal/terminalService.ts` and its siblings.
//!
//! Dependencies follow `specs/rust-backend-replacement.md` §2.1: only `nc-code-channels`,
//! `nc-code-service-core`, `nc-code-rpc` plus the PTY/profile helpers below. Settings are read
//! from the file the `setting` channel owns; this crate never writes it.

pub mod profile;
pub mod profile_macos;
pub mod pty;
pub mod shell;
pub mod terminal;
pub mod utf8_stream;
pub mod winpty;

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel};
use nc_code_service_core::AppPaths;

pub use profile::{resolve_terminal_font_profile, FontProfile, TerminalProfileSettings};
pub use shell::{
    resolve_terminal_cwd, resolve_terminal_environment, resolve_terminal_shell, Environment,
    Platform,
};
pub use terminal::TerminalService;

/// The channels this crate owns; `nc-code-app` registers them in the composition root.
pub struct TerminalServices {
    pub terminal: Arc<TerminalService>,
}

impl TerminalServices {
    pub fn new(paths: AppPaths) -> Self {
        Self {
            terminal: TerminalService::new(paths),
        }
    }

    pub fn register(&self, registry: &ChannelRegistry) -> Result<(), DuplicateChannel> {
        registry.register("terminal", self.terminal.clone())
    }

    /// Host shutdown: kill and reap every live child so no orphan shell outlives the
    /// process, mirroring the `disposeAll()` entry point of the TypeScript service.
    pub fn dispose_all(&self) {
        self.terminal.dispose_all();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert!(true);
    }
}
