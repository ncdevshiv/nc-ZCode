//! File/config-backed extension service channels (Rust backend).

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

pub mod command_markdown;
pub mod commands;
pub mod hooks;
pub mod memory;
pub mod model_selection;
pub mod plugins;
pub mod skills;
pub mod subagents;
pub mod text;
pub mod user_config;
pub mod yaml_lite;

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel};
use nc_code_service_core::AppPaths;

pub use commands::CommandsService;
pub use memory::MemoryService;
pub use skills::SkillsService;

pub struct ExtensionsServices {
    pub commands: Arc<CommandsService>,
    pub hooks: Arc<crate::hooks::HooksService>,
    pub subagents: Arc<crate::subagents::SubagentsService>,
    pub memory: Arc<MemoryService>,
    pub skills: Arc<SkillsService>,
}

impl ExtensionsServices {
    pub fn new(paths: AppPaths) -> Self {
        Self {
            commands: Arc::new(CommandsService::new(paths.clone())),
            hooks: crate::hooks::HooksService::new(paths.clone()),
            subagents: crate::subagents::SubagentsService::new(paths.clone()),
            memory: Arc::new(MemoryService::new(paths.clone())),
            skills: Arc::new(SkillsService::new(paths)),
        }
    }

    pub fn register(&self, registry: &ChannelRegistry) -> Result<(), DuplicateChannel> {
        registry.register("commands", self.commands.clone())?;
        registry.register("hooks", self.hooks.clone())?;
        registry.register("subagents", self.subagents.clone())?;
        registry.register("memory", self.memory.clone())?;
        registry.register("skills", self.skills.clone())?;
        Ok(())
    }
}
