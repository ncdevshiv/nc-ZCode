with open('G:/ZCode-main/native/crates/nc-code-services-workspace/src/lib.rs') as f:
    c = f.read()

if 'media_preview' not in c:
    c = c.replace('pub mod watcher;\n', 'pub mod watcher;\npub mod media_preview;\n')
if 'MediaPreviewService' not in c:
    c = c.replace('pub use watcher::FileWatcherService;\n', 
                  'pub use watcher::FileWatcherService;\npub use media_preview::MediaPreviewService;\n')
if 'pub media_preview' not in c:
    c = c.replace('    pub watcher: Arc<FileWatcherService>,\n',
                  '    pub watcher: Arc<FileWatcherService>,\n    pub media_preview: Arc<MediaPreviewService>,\n')
if 'MediaPreviewService::new()' not in c:
    c = c.replace('            watcher: FileWatcherService::new(),\n',
                  '            watcher: FileWatcherService::new(),\n            media_preview: MediaPreviewService::new(),\n')
if '"media-preview"' not in c:
    c = c.replace('        registry.register("file-watcher", self.watcher.clone())?;\n',
                  '        registry.register("file-watcher", self.watcher.clone())?;\n        registry.register("media-preview", self.media_preview.clone())?;\n')

with open('G:/ZCode-main/native/crates/nc-code-services-workspace/src/lib.rs', 'w') as f:
    f.write(c)
print('done')
