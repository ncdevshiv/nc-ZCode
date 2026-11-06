//! nc-code-services-plugins - Domain layer and storage of plug-in (Plugin / Marketplace) services.
//!
//! Compare TS: `packages/services/src/plugins/*` (installation/configuration/market) and its supporting modules. This crate provides
//! Pure domain layer (manifest parsing, id/dependency normalization, path/cache layout, application configuration hierarchical merging, remote crawling),
//! Orchestration layer and `plugin-management` channel distribution (see `channel.rs`; skills reference catalog reuses skills channel).
//!
//! Layering has the same discipline as nc-code-host::workspace_hooks: parsing/normalization are pure functions (can be tested individually and shared by multiple entries),
//! Disk and network orchestration are in the Fetcher/store layer, not in pure functions.

pub mod app_config;
pub mod archive;
pub mod channel;
pub mod components;
pub mod dependencies;
pub mod diagnostics;
pub mod discovery;
pub mod fetch;
pub mod frontmatter;
pub mod fsutil;
pub mod hooks;
pub mod install;
pub mod manifest;
pub mod marketplace;
pub mod marketplace_store;
pub mod mcp;
pub mod official;
pub mod operations;
pub mod orchestration;
pub mod project;
pub mod reference;
pub mod service;
pub mod source_root;
pub mod sources;
pub mod types;
pub mod validate;
pub mod version;

pub use app_config::{
    default_user_config_path, ConfigFiles, ConfigLayer, MergedConfig, PluginConfigSection,
};
pub use channel::PluginManagementService;
pub use fetch::{check_cancelled, sha256_hex, Fetcher};
pub use fsutil::{
    copy_dir_all, is_directory, is_file, normalize_path, now_iso, now_millis,
    remove_path_best_effort, resolve_inside, resolve_user_path, swap_directories,
};
pub use manifest::{
    find_marketplace_manifest_file, find_plugin_manifest_path, normalize_author,
    parse_entry_store_listing, parse_marketplace_manifest, parse_required_marketplace_manifest,
    read_markdown_frontmatter, resolve_installed_plugin_version, MarketplaceEntry,
    MarketplaceManifest, CLAUDE_MANIFEST_PATH, CODEX_MANIFEST_PATH, ZCODE_MANIFEST_PATH,
};
pub use marketplace_store::{
    marketplace_dir, marketplace_manifest_path, plugin_cache_dir, plugin_data_dir,
};
pub use service::PluginsService;
pub use types::{
    is_official_marketplace_id, is_valid_namespaced_name, normalize_dependency_ref,
    parse_plugin_id, qualify_dependency, read_source_identity_pin, sanitize_plugin_id,
    InstalledPlugin, KnownMarketplace, DEFAULT_VERSION, INSTALLED_PLUGINS_FILE,
    KNOWN_MARKETPLACES_FILE, MARKETPLACE_FILE, OFFICIAL_MARKETPLACE_ID,
};

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert!(true);
    }
}
