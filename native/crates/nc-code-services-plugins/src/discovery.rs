//! Plugin discovery for the plugin-management channel — port of
//! `adapters/src/plugins/index.ts` (`NodePluginAdapter.discoverPluginsSync`) and the
//! official plugin roots helper in `bootstrap/src/app/bundled-plugins.ts`.
//!
//! Discovery is stateless: every call rescans the config dirs, official cache and
//! `installed_plugins.json`, exactly like the TypeScript loader.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use nc_code_service_core::ServiceError;
use serde_json::{Map, Value};

use crate::app_config::MergedConfig;
use crate::components::{
    count_skill_files, enumerate_plugin_components, inspect_hooks_for_discovery,
    read_manifest_from_root, resolve_command_roots, resolve_component_roots, ComponentContext,
};
use crate::diagnostics::Diagnostic;
use crate::fsutil::{is_directory, normalize_path, resolve_inside};
use crate::manifest::{normalize_author, MarketplaceEntry};
use crate::marketplace_store::{load_installed_plugins, plugin_cache_dir};
use crate::mcp::{
    load_plugin_mcp_server_definitions, resolve_plugin_mcp_servers, McpPluginContext,
};
use crate::official::{is_cua_internal_feature_enabled, scan_official_cache_roots, CUA_PLUGIN_ID};
use crate::types::{is_valid_namespaced_name, DEFAULT_VERSION, OFFICIAL_MARKETPLACE_ID};

pub const INLINE_MARKETPLACE: &str = "inline";

/// `DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS`.
pub const DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS: [&str; 10] = [
    "browser-use@zcode-plugins-official",
    "image-search@zcode-plugins-official",
    "documents@zcode-plugins-official",
    "pdf@zcode-plugins-official",
    "presentations@zcode-plugins-official",
    "spreadsheets@zcode-plugins-official",
    "node-repl-host@zcode-plugins-official",
    "skill-creator@zcode-plugins-official",
    "plugin-creator@zcode-plugins-official",
    "zcode-guide@zcode-plugins-official",
];

pub fn is_default_enabled_official_plugin(plugin_id: &str) -> bool {
    DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS.contains(&plugin_id)
}

#[derive(Clone, Debug)]
pub struct LoadedPlugin {
    pub id: String,
    pub manifest: Map<String, Value>,
    pub manifest_path: PathBuf,
    pub marketplace: String,
    pub root_path: PathBuf,
    /// `inline` | `official` | `cache`.
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct PluginMetadata {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub enabled: bool,
    pub source: String,
    pub marketplace: String,
    pub author: Option<String>,
    pub author_url: Option<String>,
    pub homepage: Option<String>,
    pub skill_count: usize,
    pub skill_root_count: usize,
    pub command_root_count: usize,
    pub components: Vec<Value>,
    pub declared_mcp_server_names: Vec<String>,
    pub mcp_server_names: Vec<String>,
    pub hook_details: Vec<Value>,
    pub root_path: PathBuf,
    pub user_config: Option<Value>,
    pub configured_options: Map<String, Value>,
    pub data_path: PathBuf,
}

pub struct DiscoveryOutcome {
    pub plugins: Vec<PluginMetadata>,
    pub diagnostics: Vec<Diagnostic>,
}

struct Candidate {
    default_enabled: bool,
    marketplace: String,
    root_path: PathBuf,
    source: &'static str,
}

pub async fn discover_plugins(
    config: &MergedConfig,
    storage_root: &Path,
    working_directory: &str,
) -> DiscoveryOutcome {
    if !config.plugins_enabled() {
        return DiscoveryOutcome {
            plugins: Vec::new(),
            diagnostics: Vec::new(),
        };
    }
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let data_root = storage_root.join("data");
    let mut candidates: Vec<Candidate> = Vec::new();
    for root in config.plugin_dirs_all() {
        candidates.push(Candidate {
            default_enabled: true,
            marketplace: INLINE_MARKETPLACE.to_string(),
            root_path: normalize_path(&PathBuf::from(root)),
            source: "inline",
        });
    }
    for root in scan_official_cache_roots(storage_root).await {
        candidates.push(Candidate {
            default_enabled: false,
            marketplace: OFFICIAL_MARKETPLACE_ID.to_string(),
            root_path: root,
            source: "official",
        });
    }
    if let Ok(installed) = load_installed_plugins(storage_root).await {
        for record in installed {
            let root = if record.install_path.trim().is_empty() {
                plugin_cache_dir(
                    storage_root,
                    &record.marketplace,
                    &record.name,
                    &record.version,
                )
            } else {
                PathBuf::from(&record.install_path)
            };
            candidates.push(Candidate {
                default_enabled: false,
                marketplace: record.marketplace,
                root_path: normalize_path(&root),
                source: "cache",
            });
        }
    }

    let enabled_plugins = config.enabled_plugins();
    let suppressed = config.suppressed_builtins();
    let cua_enabled = is_cua_internal_feature_enabled();
    let mut plugins: Vec<PluginMetadata> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for candidate in candidates {
        let Some(loaded) = load_plugin(&candidate, &mut diagnostics) else {
            continue;
        };
        if loaded.source == "official" && suppressed.contains(&loaded.id) {
            continue;
        }
        if loaded.source == "official" && loaded.id == CUA_PLUGIN_ID && !cua_enabled {
            continue;
        }
        if !seen.insert(loaded.id.clone()) {
            diagnostics.push(
                Diagnostic::warning(
                    "plugin_duplicate_id",
                    format!("Duplicate plugin ignored: {}", loaded.id),
                )
                .with_path(loaded.root_path.to_string_lossy())
                .with_plugin_id(loaded.id),
            );
            continue;
        }
        warn_unsupported_components(&loaded, &mut diagnostics);
        let default_enabled =
            candidate.default_enabled || is_default_enabled_official_plugin(&loaded.id);
        let enabled = enabled_plugins
            .get(&loaded.id)
            .and_then(Value::as_bool)
            .unwrap_or(default_enabled);
        let metadata = build_plugin_metadata(
            &loaded,
            &data_root,
            enabled,
            config,
            working_directory,
            &mut diagnostics,
        );
        plugins.push(metadata);
    }
    DiscoveryOutcome {
        plugins,
        diagnostics,
    }
}

fn build_plugin_metadata(
    loaded: &LoadedPlugin,
    data_root: &Path,
    enabled: bool,
    config: &MergedConfig,
    working_directory: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> PluginMetadata {
    let manifest = &loaded.manifest;
    let data_path = data_root.join(crate::types::sanitize_plugin_id(&loaded.id));
    let mcp_ctx = McpPluginContext {
        root_path: &loaded.root_path,
        manifest: Some(manifest),
        manifest_path: &loaded.manifest_path,
        plugin_id: &loaded.id,
        marketplace: &loaded.marketplace,
    };
    let definitions = load_plugin_mcp_server_definitions(&mcp_ctx, diagnostics);
    let declared_mcp_server_names: Vec<String> = definitions.keys().cloned().collect();
    let component_ctx = ComponentContext {
        root_path: &loaded.root_path,
        manifest: Some(manifest),
        manifest_path: &loaded.manifest_path,
        plugin_id: &loaded.id,
        marketplace: &loaded.marketplace,
    };
    let (hook_details, _events) = inspect_hooks_for_discovery(&component_ctx, diagnostics);
    let components = enumerate_plugin_components(&component_ctx, diagnostics);

    let (skill_roots, command_roots, mcp_server_names, skill_count) = if enabled {
        let _ = std::fs::create_dir_all(&data_path);
        let skill_roots = resolve_component_roots(&loaded.root_path, manifest.get("skills"));
        let mut command_roots = resolve_command_roots(&loaded.root_path, manifest.get("commands"));
        if let Some(generated) = materialize_command_metadata_root(
            &loaded.root_path,
            &loaded.manifest_path,
            &loaded.id,
            &data_path,
            manifest,
            diagnostics,
        ) {
            command_roots.push(generated);
        }
        let mcp_servers = resolve_plugin_mcp_servers(
            &definitions,
            &data_path,
            Path::new(working_directory),
            &mcp_ctx,
            diagnostics,
        );
        let mcp_names = mcp_servers.keys().cloned().collect();
        let skill_count = count_skill_files(&skill_roots);
        (skill_roots, command_roots, mcp_names, skill_count)
    } else {
        (Vec::new(), Vec::new(), Vec::new(), 0)
    };

    let author = normalize_author(manifest.get("author"));
    let homepage = manifest
        .get("homepage")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|homepage| !homepage.is_empty())
        .map(str::to_string);
    PluginMetadata {
        id: loaded.id.clone(),
        name: manifest
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        description: manifest
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        version: manifest
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        enabled,
        source: loaded.source.clone(),
        marketplace: loaded.marketplace.clone(),
        author: author
            .as_ref()
            .and_then(|author| author.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        author_url: author
            .as_ref()
            .and_then(|author| author.get("url"))
            .and_then(Value::as_str)
            .map(str::to_string),
        homepage,
        skill_count,
        skill_root_count: skill_roots.len(),
        command_root_count: command_roots.len(),
        components,
        declared_mcp_server_names,
        mcp_server_names,
        hook_details,
        root_path: loaded.root_path.clone(),
        user_config: manifest.get("userConfig").cloned(),
        configured_options: config.plugin_options(&loaded.id),
        data_path,
    }
}

fn load_plugin(candidate: &Candidate, diagnostics: &mut Vec<Diagnostic>) -> Option<LoadedPlugin> {
    if !is_directory(&candidate.root_path) {
        diagnostics.push(
            Diagnostic::warning(
                "plugin_root_not_found",
                format!(
                    "Plugin root does not exist: {}",
                    candidate.root_path.display()
                ),
            )
            .with_path(candidate.root_path.to_string_lossy()),
        );
        return None;
    }
    let manifest_path = crate::components::find_manifest(&candidate.root_path)?;
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error) => {
            diagnostics.push(
                Diagnostic::error(
                    "plugin_manifest_invalid",
                    format!("Invalid plugin manifest: {error}"),
                )
                .with_path(manifest_path.to_string_lossy()),
            );
            return None;
        }
    };
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(
                Diagnostic::error("plugin_manifest_invalid", error.to_string())
                    .with_path(manifest_path.to_string_lossy()),
            );
            return None;
        }
    };
    let Some(mut manifest) = parsed.as_object().cloned() else {
        diagnostics.push(
            Diagnostic::error("plugin_manifest_invalid", "Manifest must be a JSON object")
                .with_path(manifest_path.to_string_lossy()),
        );
        return None;
    };
    let name = manifest
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !is_valid_namespaced_name(&name) {
        diagnostics.push(
            Diagnostic::error(
                "plugin_manifest_invalid",
                format!("Invalid plugin name: {name}"),
            )
            .with_path(manifest_path.to_string_lossy()),
        );
        return None;
    }
    manifest.insert("name".into(), Value::String(name.clone()));
    if !manifest.get("version").is_some_and(Value::is_string) {
        manifest.insert("version".into(), Value::String(DEFAULT_VERSION.into()));
    }
    Some(LoadedPlugin {
        id: format!("{}@{}", name, candidate.marketplace),
        manifest,
        manifest_path,
        marketplace: candidate.marketplace.clone(),
        root_path: candidate.root_path.clone(),
        source: candidate.source.to_string(),
    })
}

fn warn_unsupported_components(loaded: &LoadedPlugin, diagnostics: &mut Vec<Diagnostic>) {
    for key in ["channels", "lspServers", "outputStyles", "settings"] {
        if loaded.manifest.contains_key(key) {
            diagnostics.push(
                Diagnostic::warning(
                    "plugin_unsupported_component",
                    format!("Plugin component is diagnostic-only in this ZCode runtime: {key}"),
                )
                .with_path(loaded.manifest_path.to_string_lossy())
                .with_plugin_id(loaded.id.clone()),
            );
        }
    }
}

/// `materializeCommandMetadataRoot`: object-form `commands` entries become markdown files
/// under `<plugin data>/generated-commands` so the runtime command loader sees them.
fn materialize_command_metadata_root(
    root_path: &Path,
    manifest_path: &Path,
    plugin_id: &str,
    data_path: &Path,
    manifest: &Map<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<PathBuf> {
    let spec = manifest.get("commands")?.as_object()?;
    let generated_root = data_path.join("generated-commands");
    let _ = std::fs::create_dir_all(&generated_root);
    let mut wrote_command = false;
    for (raw_name, raw_metadata) in spec {
        let Some(metadata) = raw_metadata.as_object() else {
            diagnostics.push(
                Diagnostic::error(
                    "plugin_manifest_invalid",
                    format!("Plugin command metadata must be an object: {raw_name}"),
                )
                .with_path(manifest_path.to_string_lossy())
                .with_plugin_id(plugin_id),
            );
            continue;
        };
        let Some(name) = normalize_generated_command_name(raw_name) else {
            diagnostics.push(
                Diagnostic::error(
                    "plugin_manifest_invalid",
                    format!("Invalid plugin command name: {raw_name}"),
                )
                .with_path(manifest_path.to_string_lossy())
                .with_plugin_id(plugin_id),
            );
            continue;
        };
        let source = metadata.get("source").and_then(Value::as_str);
        let content = metadata.get("content").and_then(Value::as_str);
        if source.is_some() == content.is_some() {
            diagnostics.push(
                Diagnostic::error(
                    "plugin_manifest_invalid",
                    format!(
                        "Plugin command '{raw_name}' must provide exactly one of source or content"
                    ),
                )
                .with_path(manifest_path.to_string_lossy())
                .with_plugin_id(plugin_id),
            );
            continue;
        }
        let mut markdown = content.map(str::to_string);
        if let Some(source) = source {
            let Some(source_path) = resolve_inside(root_path, source.trim_start_matches("./"))
            else {
                diagnostics.push(
                    Diagnostic::error(
                        "plugin_component_path_invalid",
                        format!("Plugin command source escapes plugin root: {source}"),
                    )
                    .with_path(manifest_path.to_string_lossy())
                    .with_plugin_id(plugin_id),
                );
                continue;
            };
            if !source_path.is_file() {
                diagnostics.push(
                    Diagnostic::error(
                        "plugin_component_path_invalid",
                        format!("Plugin command source file not found: {source}"),
                    )
                    .with_path(source_path.to_string_lossy())
                    .with_plugin_id(plugin_id),
                );
                continue;
            }
            match std::fs::read_to_string(&source_path) {
                Ok(text) => markdown = Some(text),
                Err(error) => {
                    diagnostics.push(
                        Diagnostic::error(
                            "plugin_component_path_invalid",
                            format!("Plugin command source file unreadable: {error}"),
                        )
                        .with_path(source_path.to_string_lossy())
                        .with_plugin_id(plugin_id),
                    );
                    continue;
                }
            }
        }
        let Some(markdown) = markdown else {
            continue;
        };
        let body = apply_command_metadata_frontmatter(&markdown, metadata);
        let _ = std::fs::write(generated_root.join(format!("{name}.md")), body);
        wrote_command = true;
    }
    wrote_command.then_some(generated_root)
}

fn normalize_generated_command_name(name: &str) -> Option<String> {
    let normalized = name.trim().trim_start_matches('/').to_ascii_lowercase();
    let bytes = normalized.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return None;
    }
    let head = bytes[0];
    if !(head.is_ascii_lowercase() || head.is_ascii_digit()) {
        return None;
    }
    if !bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b':' | b'-')
    }) {
        return None;
    }
    Some(normalized)
}

fn apply_command_metadata_frontmatter(markdown: &str, metadata: &Map<String, Value>) -> String {
    let read = |key: &str| {
        metadata
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let mut frontmatter: Vec<(&str, String)> = Vec::new();
    if let Some(description) = read("description") {
        frontmatter.push(("description", description.to_string()));
    }
    if let Some(argument_hint) = read("argumentHint") {
        frontmatter.push(("argument-hint", argument_hint.to_string()));
    }
    if let Some(model) = read("model") {
        frontmatter.push(("model", model.to_string()));
    }
    if let Some(tools) = metadata.get("allowedTools").and_then(Value::as_array) {
        let allowed: Vec<String> = tools
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(str::to_string)
            .collect();
        if !allowed.is_empty() {
            frontmatter.push(("allowed-tools", allowed.join(", ")));
        }
    }
    if frontmatter.is_empty() {
        return markdown.to_string();
    }
    let stripped = strip_markdown_frontmatter(markdown);
    let body = stripped.trim_start();
    let header: Vec<String> = frontmatter
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .collect();
    format!("---\n{}\n---\n\n{body}", header.join("\n"))
}

fn strip_markdown_frontmatter(markdown: &str) -> String {
    let normalized = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    if !normalized.starts_with("---") {
        return markdown.to_string();
    }
    let lines: Vec<&str> = normalized.split('\n').collect();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return markdown.to_string();
    }
    let end = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.trim() == "---")
        .map(|(index, _)| index);
    match end {
        Some(index) => normalized
            .lines()
            .skip(index + 1)
            .collect::<Vec<_>>()
            .join("\n"),
        None => markdown.to_string(),
    }
}

/// Read a plugin manifest rooted at `root` (strict name validation).
pub fn read_plugin_manifest(
    root: &Path,
    entry: Option<&MarketplaceEntry>,
) -> Result<Option<crate::components::ReadManifest>, ServiceError> {
    read_manifest_from_root(root, entry).map_err(ServiceError::new)
}
