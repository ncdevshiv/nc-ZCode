//! The `git-checkpoint` channel — immutable workspace file snapshots (TS:
//! `packages/services/src/git/gitCheckpoint*.ts` + `repo/gitCheckpoint*.{Repo,Store,Helpers}`).
//!
//! The layering matches TS exactly: `repo` (git objects and diff/restore semantics), `store` (the local manifest file),
//! `service` (orchestration + consistency guards), `helpers` (`-z` output parsing and path folding).
//! `gitcmd` is this crate's git execution layer (including the environment needed for the `GIT_INDEX_FILE` temporary index).
//!
//! Known environment limitation (matching TS behavior; TS likewise has no tests for this feature): post-restore verification uses
//! `git hash-object --no-filters` to compare against the blob in the checkpoint tree, and in a repo with `core.autocrlf=true`
//! (the Windows Git default) `git restore` converts LF to CRLF when writing back —— the bytes differ →
//! `Checkpoint restore verification failed.`. Use `force:true` or set `git config core.autocrlf false` in the
//! repository; changing the verification to a filter-aware hash would be a deliberate deviation from TS,
//! so it was not changed on our own.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

pub mod gitcmd;
pub mod helpers;
pub mod repo;
pub mod service;
pub mod store;

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel};
use nc_code_service_core::AppPaths;

pub use service::CheckpointService;

pub struct GitCheckpointServices {
    pub checkpoint: Arc<CheckpointService>,
}

impl GitCheckpointServices {
    pub fn new(paths: AppPaths) -> Self {
        Self {
            checkpoint: CheckpointService::new(paths),
        }
    }

    pub fn register(&self, registry: &ChannelRegistry) -> Result<(), DuplicateChannel> {
        registry.register("git-checkpoint", self.checkpoint.clone())
    }
}

#[cfg(test)]
mod tests {
    /// The git-checkpoint channels register exactly once into a fresh registry.
    #[test]
    fn channels_register_into_a_fresh_registry() {
        let paths = nc_code_service_core::AppPaths::new(
            std::env::temp_dir().join("zcode-git-checkpoint-test-home"),
            std::env::temp_dir().join("zcode-git-checkpoint-test-data"),
        );
        let registry = nc_code_channels::ChannelRegistry::new();
        let services = super::GitCheckpointServices::new(paths);
        services.register(&registry).expect("register");
        assert!(!registry.is_empty());
    }
}
