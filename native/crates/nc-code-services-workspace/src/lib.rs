//! Workspace service channels (system, file, …). Each module owns one channel.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

pub mod file;
pub mod media_preview;
pub mod system;
pub mod watcher;

use std::sync::Arc;

use nc_code_channels::{ChannelRegistry, DuplicateChannel};
use nc_code_service_core::AppPaths;

pub use file::FileService;
pub use media_preview::MediaPreviewService;
pub use system::SystemService;
pub use watcher::FileWatcherService;

pub struct WorkspaceServices {
    pub file: Arc<FileService>,
    pub system: Arc<SystemService>,
    pub watcher: Arc<FileWatcherService>,
    pub media_preview: Arc<MediaPreviewService>,
}

impl WorkspaceServices {
    pub fn new(paths: AppPaths) -> Self {
        Self {
            file: FileService::new(paths.clone()),
            system: SystemService::new(paths),
            watcher: FileWatcherService::new(),
            media_preview: MediaPreviewService::new(),
        }
    }

    pub fn register(&self, registry: &ChannelRegistry) -> Result<(), DuplicateChannel> {
        registry.register("file", self.file.clone())?;
        registry.register("system", self.system.clone())?;
        registry.register("file-watcher", self.watcher.clone())?;
        registry.register("media-preview", self.media_preview.clone())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// The workspace family owns the file/system/watcher/media-preview channels and
    /// registers each of them exactly once.
    #[test]
    fn workspace_channels_register_into_a_fresh_registry() {
        let paths = nc_code_service_core::AppPaths::new(
            std::env::temp_dir().join("zcode-workspace-test-home"),
            std::env::temp_dir().join("zcode-workspace-test-data"),
        );
        let registry = nc_code_channels::ChannelRegistry::new();
        let services = super::WorkspaceServices::new(paths);
        services.register(&registry).expect("register");
        for channel in ["file", "system", "file-watcher", "media-preview"] {
            assert!(
                registry.get(channel).is_some(),
                "{channel} must be registered"
            );
        }
    }
}
