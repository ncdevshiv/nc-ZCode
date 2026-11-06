//! Workspace service channels (system, file, …). Each module owns one channel.

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
    #[test]
    fn crate_compiles() {
        assert!(true);
    }
}
