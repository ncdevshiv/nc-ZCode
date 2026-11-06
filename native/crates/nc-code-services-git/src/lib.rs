//! `git` service channel — the Rust port of `packages/services/src/git/`.

pub mod channel;
pub mod cmd;
pub mod commit_message;
pub mod parse;
pub mod path;
pub mod refs;
pub mod repo;
pub mod worktree;

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel};
use nc_code_services_provider::ProviderStore;

pub use channel::GitChannelService;

/// Assembly unit for the composition root: construction and registration of the `git` channel.
pub struct GitServices {
    pub git: Arc<GitChannelService>,
}

impl GitServices {
    pub fn new(store: Arc<ProviderStore>) -> Self {
        let repo = crate::repo::GitRepo::new(crate::cmd::GitCommandProvider::new());
        let messages = crate::commit_message::ProviderCommitMessageGenerator::from_store(store);
        Self {
            git: GitChannelService::new(repo, messages),
        }
    }

    pub fn register(&self, registry: &ChannelRegistry) -> Result<(), DuplicateChannel> {
        registry.register("git", self.git.clone())
    }
}
