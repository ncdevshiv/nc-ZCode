//! Protocol projections shared by the channel and the reference catalog — port of the
//! `toPluginInfo` / `toAvailablePluginSummary` / `toInstalledPluginSummary` /
//! `toMarketplaceSummary` helpers in `bootstrap/src/nc-code-protocol/plugins.ts`.

use std::path::Path;

use nc_code_service_core::AppPaths;
use serde_json::{Map, Value};

use crate::app_config::MergedConfig;
use crate::discovery::PluginMetadata;
use crate::fsutil::normalize_path;
use crate::types::{InstalledPlugin, KnownMarketplace};

/// `toPluginInfo`: UI-facing plugin view (sensitive options removed).
pub fn to_plugin_info(plugin: &PluginMetadata, config: &MergedConfig, paths: &AppPaths) -> Value {
    let host_mcp_server_names: Vec<String> = Vec::new();
    let mut configured_options = Map::new();
    let user_config = plugin.user_config.as_ref().and_then(Value::as_object);
    for (key, value) in &plugin.configured_options {
        let sensitive = user_config
            .and_then(|config| config.get(key))
            .and_then(|option| option.get("sensitive"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !sensitive {
            configured_options.insert(key.clone(), value.clone());
        }
    }
    let enabled_source = config.enabled_sources().get(&plugin.id).cloned();
    let option_sources = config.option_sources(&plugin.id);
    let root_source = if plugin.source == "inline" {
        resolve_inline_root_source(&plugin.root_path, config, paths)
    } else {
        None
    };

    let mut info = Map::new();
    info.insert("id".into(), Value::String(plugin.id.clone()));
    info.insert("name".into(), Value::String(plugin.name.clone()));
    if let Some(description) = &plugin.description {
        info.insert("description".into(), Value::String(description.clone()));
    }
    if let Some(version) = &plugin.version {
        info.insert("version".into(), Value::String(version.clone()));
    }
    info.insert("enabled".into(), Value::Bool(plugin.enabled));
    info.insert("source".into(), Value::String(plugin.source.clone()));
    info.insert(
        "marketplace".into(),
        Value::String(plugin.marketplace.clone()),
    );
    if let Some(author) = &plugin.author {
        info.insert("author".into(), Value::String(author.clone()));
    }
    if let Some(author_url) = &plugin.author_url {
        info.insert("authorUrl".into(), Value::String(author_url.clone()));
    }
    if let Some(homepage) = &plugin.homepage {
        info.insert("homepage".into(), Value::String(homepage.clone()));
    }
    info.insert("skillCount".into(), Value::from(plugin.skill_count));
    info.insert(
        "skillRootCount".into(),
        Value::from(plugin.skill_root_count),
    );
    info.insert(
        "commandRootCount".into(),
        Value::from(plugin.command_root_count),
    );
    info.insert("components".into(), Value::Array(plugin.components.clone()));
    info.insert(
        "declaredMcpServerNames".into(),
        Value::Array(
            plugin
                .declared_mcp_server_names
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    if !host_mcp_server_names.is_empty() {
        info.insert(
            "hostMcpServerNames".into(),
            Value::Array(
                host_mcp_server_names
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
    info.insert(
        "mcpServerNames".into(),
        Value::Array(
            plugin
                .mcp_server_names
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    info.insert(
        "hookDetails".into(),
        Value::Array(plugin.hook_details.clone()),
    );
    info.insert(
        "rootPath".into(),
        Value::String(plugin.root_path.to_string_lossy().into_owned()),
    );
    if let Some(user_config) = &plugin.user_config {
        info.insert("userConfig".into(), user_config.clone());
    }
    if !configured_options.is_empty() {
        info.insert(
            "configuredOptions".into(),
            Value::Object(configured_options),
        );
    }
    if let Some(root_source) = root_source {
        info.insert("rootSource".into(), Value::String(root_source));
    }
    if let Some(enabled_source) = enabled_source {
        info.insert("enabledSource".into(), enabled_source);
    }
    if !option_sources.is_empty() {
        info.insert("optionSources".into(), Value::Object(option_sources));
    }
    Value::Object(info)
}

/// `resolveInlinePluginRootSource`: workspace declarations win over user ones.
fn resolve_inline_root_source(
    plugin_root: &Path,
    config: &MergedConfig,
    paths: &AppPaths,
) -> Option<String> {
    let target = normalize_for_comparison(plugin_root);
    let workspace_dirs = config.plugin_dirs("workspace");
    if workspace_dirs
        .iter()
        .any(|dir| normalize_for_comparison(&resolve_config_dir(dir, paths)) == target)
    {
        return Some("workspace".to_string());
    }
    let user_dirs = config.plugin_dirs("user");
    if user_dirs
        .iter()
        .any(|dir| normalize_for_comparison(&resolve_config_dir(dir, paths)) == target)
    {
        return Some("user".to_string());
    }
    None
}

fn resolve_config_dir(raw: &str, paths: &AppPaths) -> std::path::PathBuf {
    crate::fsutil::resolve_user_path(raw, paths.home_dir())
}

fn normalize_for_comparison(path: &Path) -> String {
    let resolved = normalize_path(path);
    let text = resolved.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

/// `createMissingConfiguredPluginInfos`: configured ids without a discovered package.
pub fn missing_configured_plugins(config: &MergedConfig, discovered_ids: &[String]) -> Vec<Value> {
    let enabled_plugins = config.enabled_plugins();
    let mut configured: Vec<String> = Vec::new();
    for id in enabled_plugins.keys() {
        if !configured.contains(id) {
            configured.push(id.clone());
        }
    }
    for layer in std::iter::once(&config.user).chain(config.workspace_layers.iter().rev()) {
        for id in layer.plugins().options.keys() {
            if !configured.contains(id) {
                configured.push(id.clone());
            }
        }
    }
    let enabled_sources = config.enabled_sources();
    let mut out = Vec::new();
    for plugin_id in configured {
        if discovered_ids.contains(&plugin_id) {
            continue;
        }
        let Some(separator) = plugin_id.rfind('@') else {
            continue;
        };
        if separator == 0 || separator == plugin_id.len() - 1 {
            continue;
        }
        let name = &plugin_id[..separator];
        let marketplace = &plugin_id[separator + 1..];
        let enabled = enabled_plugins
            .get(&plugin_id)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut info = Map::new();
        info.insert("id".into(), Value::String(plugin_id.clone()));
        info.insert("name".into(), Value::String(name.to_string()));
        info.insert("enabled".into(), Value::Bool(enabled));
        info.insert("source".into(), Value::String("missing".into()));
        info.insert("marketplace".into(), Value::String(marketplace.to_string()));
        info.insert("skillCount".into(), Value::from(0));
        info.insert("skillRootCount".into(), Value::from(0));
        info.insert("commandRootCount".into(), Value::from(0));
        info.insert("components".into(), Value::Array(Vec::new()));
        info.insert("declaredMcpServerNames".into(), Value::Array(Vec::new()));
        info.insert("mcpServerNames".into(), Value::Array(Vec::new()));
        info.insert("rootPath".into(), Value::String(String::new()));
        info.insert("packageStatus".into(), Value::String("missing".into()));
        if let Some(source) = enabled_sources.get(&plugin_id) {
            info.insert("enabledSource".into(), source.clone());
        }
        let option_sources = config.option_sources(&plugin_id);
        if !option_sources.is_empty() {
            info.insert("optionSources".into(), Value::Object(option_sources));
        }
        out.push(Value::Object(info));
    }
    out
}

/// `toMarketplaceSummaryData` for a known/declared record.
pub fn marketplace_summary(
    record: &KnownMarketplace,
    featured: Option<Vec<String>>,
    plugin_count: Option<u64>,
) -> Value {
    record.summary_projection(featured, plugin_count)
}

/// `toAvailablePluginData`.
pub fn available_plugin_summary(
    entry: &crate::manifest::MarketplaceEntry,
    marketplace: &str,
    installed_ids: &std::collections::HashSet<String>,
) -> Value {
    let id = format!("{}@{marketplace}", entry.name);
    let mut summary = Map::new();
    summary.insert("id".into(), Value::String(id.clone()));
    summary.insert("name".into(), Value::String(entry.name.clone()));
    summary.insert("marketplace".into(), Value::String(marketplace.to_string()));
    if let Some(description) = &entry.description {
        summary.insert("description".into(), Value::String(description.clone()));
    }
    if let Some(version) = &entry.version {
        summary.insert("version".into(), Value::String(version.clone()));
    }
    summary.insert("installed".into(), Value::Bool(installed_ids.contains(&id)));
    summary.insert(
        "componentTypes".into(),
        Value::Array(
            crate::dependencies::infer_component_types(&entry.raw)
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
    if let Some(listing) = &entry.listing {
        summary.insert("listing".into(), Value::Object(listing.clone()));
    }
    Value::Object(summary)
}

/// `toInstalledPluginData` (with optional loaded runtime metadata).
pub fn installed_plugin_summary(
    record: &InstalledPlugin,
    enabled: bool,
    loaded: Option<&PluginMetadata>,
) -> Value {
    let mut summary = Map::new();
    summary.insert("id".into(), Value::String(record.id.clone()));
    summary.insert("name".into(), Value::String(record.name.clone()));
    summary.insert(
        "marketplace".into(),
        Value::String(record.marketplace.clone()),
    );
    if let Some(description) = loaded.and_then(|plugin| plugin.description.clone()) {
        if !description.is_empty() {
            summary.insert("description".into(), Value::String(description));
        }
    }
    let version = loaded
        .and_then(|plugin| plugin.version.clone())
        .unwrap_or_else(|| record.version.clone());
    summary.insert("version".into(), Value::String(version));
    summary.insert("enabled".into(), Value::Bool(enabled));
    summary.insert("scope".into(), Value::String(record.scope.clone()));
    summary.insert(
        "installPath".into(),
        Value::String(record.install_path.clone()),
    );
    summary.insert(
        "installedAt".into(),
        Value::String(record.installed_at.clone()),
    );
    if let Some(loaded) = loaded {
        summary.insert(
            "componentTypes".into(),
            Value::Array(
                crate::dependencies::infer_component_types_from_metadata(loaded)
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
        summary.insert(
            "hookDetails".into(),
            Value::Array(loaded.hook_details.clone()),
        );
    }
    Value::Object(summary)
}

/// Installed-shaped view of a builtin plugin that is not in `installed_plugins.json`.
pub fn builtin_installed_summary(
    id: &str,
    name: &str,
    marketplace: &str,
    version: &str,
    root_path: &Path,
    installed_at: &str,
) -> Value {
    let mut summary = Map::new();
    summary.insert("id".into(), Value::String(id.to_string()));
    summary.insert("name".into(), Value::String(name.to_string()));
    summary.insert("marketplace".into(), Value::String(marketplace.to_string()));
    summary.insert("version".into(), Value::String(version.to_string()));
    summary.insert("enabled".into(), Value::Bool(false));
    summary.insert("scope".into(), Value::String("user".into()));
    summary.insert(
        "installPath".into(),
        Value::String(root_path.to_string_lossy().into_owned()),
    );
    summary.insert(
        "installedAt".into(),
        Value::String(installed_at.to_string()),
    );
    Value::Object(summary)
}

/// `toMarketplaceSummary` for the in-memory dry-run record.
pub fn dry_run_marketplace_summary(source: &Value) -> Value {
    let mut summary = Map::new();
    summary.insert("id".into(), Value::String("dry-run".into()));
    summary.insert("name".into(), Value::String("dry-run".into()));
    summary.insert("source".into(), source.clone());
    summary.insert("pluginCount".into(), Value::from(0));
    summary.insert("isOfficial".into(), Value::Bool(false));
    Value::Object(summary)
}
