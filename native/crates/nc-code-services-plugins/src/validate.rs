//! Validation and description for marketplace plugins — port of
//! `validateMarketplacePlugin`, `validateMarketplaceSource`, `validateLocalPluginPath` and
//! `describeMarketplacePlugin` in `adapters/src/plugins/marketplace.ts`.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use nc_code_service_core::ServiceError;
use serde_json::Value;

use crate::archive::cleanup_temp_dir;
use crate::components::read_components_at_root;
use crate::diagnostics::Diagnostic;
use crate::fetch::Fetcher;
use crate::fsutil::{is_directory, normalize_path, read_json_if_exists};
use crate::manifest::{find_plugin_manifest_path, MarketplaceEntry, MarketplaceManifest};
use crate::marketplace::{
    ensure_marketplace_manifest_available, find_marketplace_manifest_path,
    load_marketplace_from_source,
};
use crate::marketplace_store::{
    load_installed_plugins, load_marketplace_manifest, marketplace_manifest_path,
};
use crate::mcp::{
    load_plugin_mcp_server_definitions, resolve_plugin_mcp_servers, McpPluginContext,
};
use crate::source_root::resolve_plugin_source_root;
use crate::sources::{source_kind, source_str};
use crate::types::sanitize_plugin_id;

const UNSUPPORTED_MANIFEST_FIELDS: [&str; 4] =
    ["channels", "lspServers", "outputStyles", "settings"];

pub struct DescribeResult {
    pub components: Vec<Value>,
    pub diagnostics: Vec<Diagnostic>,
    pub metadata: Option<Value>,
}

/// `toValidationDiagnostic`: source-materialization codes and dependency message classes.
pub fn to_validation_diagnostic(error: &ServiceError, plugin_id: Option<&str>) -> Diagnostic {
    let mut diagnostic = if error.name == "UnsupportedMarketplaceSourceError"
        || error.name == "UnsupportedPluginSourceError"
    {
        Diagnostic::error(
            "plugin_marketplace_source_unsupported",
            error.message.clone(),
        )
    } else if let Some(code) = source_diagnostic_code(error) {
        Diagnostic::error(code, error.message.clone())
    } else if error.message.contains("Cross-marketplace dependency") {
        Diagnostic::error("plugin_dependency_cross_marketplace", error.message.clone())
    } else if error.message.contains("dependency cycle") {
        Diagnostic::error("plugin_dependency_cycle", error.message.clone())
    } else if error.message.contains("Dependency not found")
        || error
            .message
            .contains("Marketplace not found for dependency")
    {
        Diagnostic::error("plugin_dependency_missing", error.message.clone())
    } else {
        Diagnostic::error("plugin_marketplace_invalid", error.message.clone())
    };
    if let Some(plugin_id) = plugin_id {
        diagnostic = diagnostic.with_plugin_id(plugin_id);
    }
    diagnostic
}

/// `getPluginSourceDiagnosticCode`: only materialization errors carry a source code.
pub fn source_diagnostic_code(error: &ServiceError) -> Option<String> {
    if error.name == "PluginSourceMaterializationError" {
        return error
            .code
            .as_ref()
            .and_then(|code| code.as_str())
            .map(str::to_string)
            .or_else(|| Some("plugin_archive_fetch_failed".to_string()));
    }
    if error.name == "GitUnavailable" {
        return Some("plugin_git_unavailable".to_string());
    }
    if error.name == "PluginZipDownloadError" {
        return error
            .code
            .as_ref()
            .and_then(|code| code.as_str())
            .map(str::to_string);
    }
    None
}

/// `validateMarketplacePlugin`: dependency closure, source resolution and root checks.
pub async fn validate_marketplace_plugin(
    fetcher: &Fetcher,
    storage_root: &Path,
    marketplace: &str,
    name: &str,
    cancelled: &AtomicBool,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if let Err(error) =
        ensure_marketplace_manifest_available(fetcher, storage_root, marketplace, cancelled).await
    {
        diagnostics.push(to_validation_diagnostic(
            &error,
            Some(&format!("{name}@{marketplace}")),
        ));
        return diagnostics;
    }
    let Some(manifest) = load_marketplace_manifest(storage_root, marketplace).await else {
        diagnostics.push(
            Diagnostic::error(
                "plugin_marketplace_invalid",
                format!("Marketplace not found: {marketplace}"),
            )
            .with_path(marketplace_manifest_path(storage_root, marketplace).to_string_lossy()),
        );
        return diagnostics;
    };
    let Some(entry) = manifest.plugins.iter().find(|entry| entry.name == name) else {
        diagnostics.push(
            Diagnostic::error(
                "plugin_not_found",
                format!("Plugin not found: {name}@{marketplace}"),
            )
            .with_path(marketplace_manifest_path(storage_root, marketplace).to_string_lossy()),
        );
        return diagnostics;
    };
    push_dependency_diagnostics(
        &mut diagnostics,
        storage_root,
        &manifest,
        marketplace,
        name,
        cancelled,
    )
    .await;
    match resolve_plugin_source_root(
        fetcher,
        storage_root,
        marketplace,
        entry,
        None,
        Some(&manifest),
        cancelled,
    )
    .await
    {
        Ok(resolved) => {
            let mut root_diagnostics =
                validate_plugin_root(entry, marketplace, &resolved.path, storage_root);
            diagnostics.append(&mut root_diagnostics);
            let _ = cleanup_temp_dir(resolved.cleanup);
        }
        Err(error) => diagnostics.push(to_validation_diagnostic(
            &error,
            Some(&format!("{name}@{marketplace}")),
        )),
    }
    diagnostics
}

/// `validateMarketplaceSource`: dry-run over a source string (no storage writes).
pub async fn validate_marketplace_source(
    fetcher: &Fetcher,
    storage_root: &Path,
    source: &Value,
    expected_id: Option<&str>,
    plugin_name: Option<&str>,
    cancelled: &AtomicBool,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let loaded = match load_marketplace_from_source(fetcher, source, cancelled).await {
        Ok(loaded) => loaded,
        Err(error) => {
            diagnostics.push(to_validation_diagnostic(&error, None));
            return diagnostics;
        }
    };
    let result = validate_loaded_marketplace_source(
        fetcher,
        storage_root,
        &loaded,
        expected_id,
        plugin_name,
        cancelled,
    )
    .await;
    diagnostics.extend(result);
    let _ = cleanup_temp_dir(loaded.cleanup);
    diagnostics
}

async fn validate_loaded_marketplace_source(
    fetcher: &Fetcher,
    storage_root: &Path,
    loaded: &crate::marketplace::LoadedMarketplace,
    expected_id: Option<&str>,
    plugin_name: Option<&str>,
    cancelled: &AtomicBool,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if let Some(expected_id) = expected_id {
        if loaded.manifest.name != expected_id {
            diagnostics.push(
                Diagnostic::error(
                    "plugin_marketplace_invalid",
                    format!(
                        "Marketplace declaration id mismatch: expected {expected_id}, received {}",
                        loaded.manifest.name
                    ),
                )
                .with_plugin_id(expected_id),
            );
            return diagnostics;
        }
    }
    if loaded.manifest.plugins.is_empty() {
        diagnostics.push(Diagnostic::warning(
            "plugin_marketplace_invalid",
            format!("Marketplace has no plugins: {}", loaded.manifest.name),
        ));
    }
    let entries: Vec<&MarketplaceEntry> = match plugin_name {
        Some(name) => loaded
            .manifest
            .plugins
            .iter()
            .filter(|entry| entry.name == name)
            .collect(),
        None => loaded.manifest.plugins.iter().collect(),
    };
    if plugin_name.is_some() && entries.is_empty() {
        let plugin_name = plugin_name.unwrap_or_default();
        diagnostics.push(
            Diagnostic::error(
                "plugin_not_found",
                format!("Plugin not found: {plugin_name}@{}", loaded.manifest.name),
            )
            .with_plugin_id(format!("{plugin_name}@{}", loaded.manifest.name)),
        );
        return diagnostics;
    }
    for entry in entries {
        diagnostics.extend(validate_entry_shape(entry, &loaded.manifest.name, false));
        push_dependency_diagnostics_from_manifest(
            &mut diagnostics,
            storage_root,
            &loaded.manifest,
            &loaded.manifest.name,
            &entry.name,
        )
        .await;
        if let Some(deferred) = source_validation_deferral(entry, &loaded.manifest.name) {
            diagnostics.push(deferred);
            push_entry_compatibility_diagnostics(&mut diagnostics, entry, &loaded.manifest.name);
            continue;
        }
        match resolve_plugin_source_root(
            fetcher,
            storage_root,
            &loaded.manifest.name,
            entry,
            loaded.source_root.as_deref(),
            Some(&loaded.manifest),
            cancelled,
        )
        .await
        {
            Ok(resolved) => {
                let mut root_diagnostics = validate_plugin_root(
                    entry,
                    &loaded.manifest.name,
                    &resolved.path,
                    storage_root,
                );
                diagnostics.append(&mut root_diagnostics);
                let _ = cleanup_temp_dir(resolved.cleanup);
            }
            Err(error) => {
                diagnostics.push(to_validation_diagnostic(
                    &error,
                    Some(&format!("{}@{}", entry.name, loaded.manifest.name)),
                ));
                push_entry_compatibility_diagnostics(
                    &mut diagnostics,
                    entry,
                    &loaded.manifest.name,
                );
            }
        }
    }
    diagnostics
}

/// `validateLocalPluginPath`: marketplace directory first, plugin root second.
pub async fn validate_local_plugin_path(
    fetcher: &Fetcher,
    storage_root: &Path,
    path: &str,
    cancelled: &AtomicBool,
) -> Vec<Diagnostic> {
    let resolved = normalize_path(&PathBuf::from(path));
    if !resolved.exists() {
        return vec![Diagnostic::error(
            "plugin_manifest_not_found",
            format!("Path does not exist: {}", resolved.display()),
        )
        .with_path(resolved.to_string_lossy())];
    }
    let root_path = if is_directory(&resolved) {
        resolved
    } else {
        resolve_manifest_root_from_file(&resolved)
    };
    if find_marketplace_manifest_path(&root_path, None).is_some() {
        let source = serde_json::json!({
            "source": "directory",
            "path": root_path.to_string_lossy(),
        });
        return validate_marketplace_source(fetcher, storage_root, &source, None, None, cancelled)
            .await;
    }
    let Some(manifest_path) = find_plugin_manifest_path(&root_path) else {
        return vec![Diagnostic::error(
            "plugin_manifest_not_found",
            format!("Plugin manifest not found: {}", root_path.display()),
        )
        .with_path(root_path.to_string_lossy())];
    };
    let name = match read_json_if_exists(&manifest_path).await {
        Ok(Some(value)) => value
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string(),
        Ok(None) => String::new(),
        Err(error) => {
            return vec![Diagnostic::error("plugin_manifest_invalid", error.message)
                .with_path(manifest_path.to_string_lossy())];
        }
    };
    let name = if name.is_empty() {
        root_path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        name
    };
    let entry = synthetic_entry(&name);
    validate_plugin_root(&entry, "inline", &root_path, storage_root)
}

fn resolve_manifest_root_from_file(file_path: &Path) -> PathBuf {
    let directory = file_path.parent().unwrap_or(file_path);
    let dir_name = directory
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    if dir_name.starts_with('.') && dir_name.ends_with("-plugin") {
        directory.parent().unwrap_or(directory).to_path_buf()
    } else {
        directory.to_path_buf()
    }
}

fn synthetic_entry(name: &str) -> MarketplaceEntry {
    let mut raw = serde_json::Map::new();
    raw.insert("name".into(), Value::String(name.to_string()));
    MarketplaceEntry {
        name: name.to_string(),
        category: None,
        description: None,
        version: None,
        source: None,
        cache_path: None,
        dependencies: Vec::new(),
        strict: true,
        tags: Vec::new(),
        listing: None,
        raw,
    }
}

/// `describeMarketplacePlugin`: installed cache first, then a temporary source resolve.
pub async fn describe_marketplace_plugin(
    fetcher: &Fetcher,
    storage_root: &Path,
    marketplace: &str,
    name: &str,
    cancelled: &AtomicBool,
) -> DescribeResult {
    let mut diagnostics = Vec::new();
    let plugin_id = format!("{name}@{marketplace}");
    if let Ok(installed) = load_installed_plugins(storage_root).await {
        if let Some(record) = installed
            .iter()
            .find(|record| record.marketplace == marketplace && record.name == name)
        {
            let root_path = if record.install_path.trim().is_empty() {
                crate::marketplace_store::plugin_cache_dir(
                    storage_root,
                    &record.marketplace,
                    &record.name,
                    &record.version,
                )
            } else {
                PathBuf::from(&record.install_path)
            };
            if is_directory(&root_path) {
                let (components, metadata) = read_components_at_root(
                    &root_path,
                    None,
                    marketplace,
                    &plugin_id,
                    &mut diagnostics,
                );
                return DescribeResult {
                    components,
                    diagnostics,
                    metadata,
                };
            }
        }
    }
    let manifest =
        match ensure_marketplace_manifest_available(fetcher, storage_root, marketplace, cancelled)
            .await
        {
            Ok(_) => load_marketplace_manifest(storage_root, marketplace).await,
            Err(error) => {
                diagnostics.push(to_validation_diagnostic(&error, Some(&plugin_id)));
                return DescribeResult {
                    components: Vec::new(),
                    diagnostics,
                    metadata: None,
                };
            }
        };
    let Some(manifest) = manifest else {
        diagnostics.push(
            Diagnostic::error(
                "plugin_marketplace_invalid",
                format!("Marketplace not found: {marketplace}"),
            )
            .with_path(marketplace_manifest_path(storage_root, marketplace).to_string_lossy()),
        );
        return DescribeResult {
            components: Vec::new(),
            diagnostics,
            metadata: None,
        };
    };
    let Some(entry) = manifest.plugins.iter().find(|entry| entry.name == name) else {
        diagnostics.push(
            Diagnostic::error("plugin_not_found", format!("Plugin not found: {plugin_id}"))
                .with_path(marketplace_manifest_path(storage_root, marketplace).to_string_lossy()),
        );
        return DescribeResult {
            components: Vec::new(),
            diagnostics,
            metadata: None,
        };
    };
    match resolve_plugin_source_root(
        fetcher,
        storage_root,
        marketplace,
        entry,
        None,
        Some(&manifest),
        cancelled,
    )
    .await
    {
        Ok(resolved) => {
            let (components, metadata) = read_components_at_root(
                &resolved.path,
                Some(entry),
                marketplace,
                &plugin_id,
                &mut diagnostics,
            );
            let _ = cleanup_temp_dir(resolved.cleanup);
            DescribeResult {
                components,
                diagnostics,
                metadata,
            }
        }
        Err(error) => {
            diagnostics.push(to_validation_diagnostic(&error, Some(&plugin_id)));
            DescribeResult {
                components: Vec::new(),
                diagnostics,
                metadata: None,
            }
        }
    }
}

/// `validatePluginRoot`: manifest parse, name match, compatibility and MCP checks.
pub fn validate_plugin_root(
    entry: &MarketplaceEntry,
    marketplace: &str,
    root_path: &Path,
    storage_root: &Path,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let plugin_id = format!("{}@{marketplace}", entry.name);
    let loaded = match crate::components::read_manifest_from_root(root_path, Some(entry)) {
        Ok(loaded) => loaded,
        Err(message) => {
            diagnostics.push(
                Diagnostic::error("plugin_manifest_invalid", message)
                    .with_path(root_path.to_string_lossy())
                    .with_plugin_id(plugin_id),
            );
            return diagnostics;
        }
    };
    let Some((manifest, manifest_path)) = loaded else {
        diagnostics.push(
            Diagnostic::error(
                "plugin_manifest_not_found",
                format!("Plugin manifest not found: {plugin_id}"),
            )
            .with_path(root_path.to_string_lossy())
            .with_plugin_id(plugin_id),
        );
        return diagnostics;
    };
    let manifest_path = manifest_path.unwrap_or_else(|| root_path.to_path_buf());
    let manifest_name = manifest
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if manifest_name != entry.name {
        diagnostics.push(
            Diagnostic::error(
                "plugin_manifest_invalid",
                format!(
                    "Plugin manifest name '{manifest_name}' does not match marketplace entry '{}'",
                    entry.name
                ),
            )
            .with_path(manifest_path.to_string_lossy())
            .with_plugin_id(plugin_id.clone()),
        );
    }
    push_manifest_compatibility_diagnostics(
        &mut diagnostics,
        &manifest,
        &manifest_path,
        &plugin_id,
    );
    push_mcp_validation_diagnostics(
        &mut diagnostics,
        &manifest,
        &manifest_path,
        marketplace,
        &plugin_id,
        root_path,
        storage_root,
    );
    diagnostics
}

fn push_manifest_compatibility_diagnostics(
    diagnostics: &mut Vec<Diagnostic>,
    manifest: &serde_json::Map<String, Value>,
    manifest_path: &Path,
    plugin_id: &str,
) {
    for key in UNSUPPORTED_MANIFEST_FIELDS {
        if manifest.contains_key(key) {
            diagnostics.push(
                Diagnostic::warning(
                    "plugin_unsupported_component",
                    format!("Plugin component is diagnostic-only in this ZCode runtime: {key}"),
                )
                .with_path(manifest_path.to_string_lossy())
                .with_plugin_id(plugin_id),
            );
        }
    }
    if let Some(user_config) = manifest.get("userConfig").and_then(Value::as_object) {
        for (key, option) in user_config {
            let required = option
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if required && option.get("default").is_none() {
                diagnostics.push(
                    Diagnostic::warning(
                        "plugin_variable_missing",
                        format!(
                            "Required plugin userConfig has no default and must be configured: {key}"
                        ),
                    )
                    .with_path(manifest_path.to_string_lossy())
                    .with_plugin_id(plugin_id),
                );
            }
        }
    }
    if manifest
        .get("mcpServers")
        .is_some_and(contains_mcp_bundle_source)
    {
        diagnostics.push(
            Diagnostic::warning(
                "plugin_marketplace_source_unsupported",
                "MCPB/DXT plugin bundles are recognized but not supported in this runtime",
            )
            .with_path(manifest_path.to_string_lossy())
            .with_plugin_id(plugin_id),
        );
    }
}

fn push_mcp_validation_diagnostics(
    diagnostics: &mut Vec<Diagnostic>,
    manifest: &serde_json::Map<String, Value>,
    manifest_path: &Path,
    marketplace: &str,
    plugin_id: &str,
    root_path: &Path,
    storage_root: &Path,
) {
    let ctx = McpPluginContext {
        root_path,
        manifest: Some(manifest),
        manifest_path,
        plugin_id,
        marketplace,
    };
    let definitions = load_plugin_mcp_server_definitions(&ctx, diagnostics);
    let data_path = storage_root
        .join("data")
        .join(sanitize_plugin_id(plugin_id));
    let working_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let _ = resolve_plugin_mcp_servers(
        &definitions,
        &data_path,
        &working_directory,
        &ctx,
        diagnostics,
    );
}

fn contains_mcp_bundle_source(value: &Value) -> bool {
    match value {
        Value::String(text) => text.ends_with(".mcpb") || text.ends_with(".dxt"),
        Value::Array(items) => items.iter().any(contains_mcp_bundle_source),
        _ => false,
    }
}

/// `validateMarketplaceEntryShape` with `includeEntryCompatibility=false`.
fn validate_entry_shape(
    entry: &MarketplaceEntry,
    marketplace: &str,
    include_compatibility: bool,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let plugin_id = format!("{}@{marketplace}", entry.name);
    if entry.source.is_none() {
        diagnostics.push(
            Diagnostic::error(
                "plugin_marketplace_invalid",
                format!("Plugin has no install source: {plugin_id}"),
            )
            .with_plugin_id(&plugin_id),
        );
    }
    if let Some(source) = entry.source.as_ref().and_then(Value::as_object) {
        let kind = entry
            .source
            .as_ref()
            .map(source_kind)
            .unwrap_or("")
            .to_string();
        if kind == "npm" || kind == "pip" {
            diagnostics.push(
                Diagnostic::warning(
                    "plugin_marketplace_source_unsupported",
                    format!("Plugin source is recognized but not supported in V1 install: {kind}"),
                )
                .with_plugin_id(&plugin_id),
            );
        }
        if kind == "url" {
            let source_type = source.get("type").and_then(Value::as_str).unwrap_or("");
            if !source_type.is_empty() && source_type != "git" && source_type != "zip" {
                diagnostics.push(
                    Diagnostic::error(
                        "plugin_marketplace_source_unsupported",
                        format!("Plugin URL source type is not supported: {source_type}"),
                    )
                    .with_plugin_id(&plugin_id),
                );
            }
            let validation = validate_url_source(source, source_type);
            if let Err(message) = validation {
                diagnostics.push(
                    Diagnostic::error("plugin_marketplace_invalid", message)
                        .with_plugin_id(&plugin_id),
                );
            }
        }
    }
    if include_compatibility {
        push_entry_compatibility_diagnostics(&mut diagnostics, entry, marketplace);
    }
    diagnostics
}

fn validate_url_source(
    source: &serde_json::Map<String, Value>,
    source_type: &str,
) -> Result<(), String> {
    match source.get("url").and_then(Value::as_str) {
        Some(url) if !url.trim().is_empty() => {}
        _ => return Err("Plugin URL source requires a non-empty url".to_string()),
    }
    if source_type == "zip" {
        let sha256 = source
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| "Plugin zip source sha256 is required".to_string())?
            .to_ascii_lowercase();
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("Plugin zip source sha256 must be a 64 character hex string".to_string());
        }
        if let Some(headers) = source.get("headers") {
            let Some(headers) = headers.as_object() else {
                return Err("Plugin zip source headers must be an object".to_string());
            };
            for (key, value) in headers {
                if !value.is_string() {
                    return Err(format!("Plugin zip source header must be a string: {key}"));
                }
            }
        }
        if let Some(path) = source.get("path") {
            if !path.is_string() {
                return Err("Plugin zip source path must be a string".to_string());
            }
        }
        if let Some(strip_root) = source.get("stripRoot") {
            if !strip_root.is_boolean() {
                return Err("Plugin zip source stripRoot must be a boolean".to_string());
            }
        }
    }
    Ok(())
}

fn push_entry_compatibility_diagnostics(
    diagnostics: &mut Vec<Diagnostic>,
    entry: &MarketplaceEntry,
    marketplace: &str,
) {
    let plugin_id = format!("{}@{marketplace}", entry.name);
    let manifest = crate::components::create_manifest_from_marketplace_entry(entry);
    push_manifest_compatibility_diagnostics(
        diagnostics,
        &manifest,
        Path::new(&plugin_id),
        &plugin_id,
    );
}

fn source_validation_deferral(entry: &MarketplaceEntry, marketplace: &str) -> Option<Diagnostic> {
    let source = entry.source.as_ref()?.as_object()?;
    let kind = entry
        .source
        .as_ref()
        .map(source_kind)
        .unwrap_or("")
        .to_string();
    if kind == "url" {
        let source_type = source.get("type").and_then(Value::as_str).unwrap_or("");
        if !source_type.is_empty() && source_type != "git" && source_type != "zip" {
            return None;
        }
    }
    if !["github", "git", "url", "git-subdir"].contains(&kind.as_str()) {
        return None;
    }
    let source_value = entry.source.as_ref().unwrap_or(&Value::Null);
    let label = source_str(source_value, "repo")
        .or_else(|| source_str(source_value, "url"))
        .unwrap_or(&kind);
    Some(
        Diagnostic::warning(
            "plugin_validation_deferred",
            format!(
                "Remote plugin source validation is deferred until install or single-plugin validate: {label}"
            ),
        )
        .with_plugin_id(format!("{}@{marketplace}", entry.name)),
    )
}

async fn push_dependency_diagnostics(
    diagnostics: &mut Vec<Diagnostic>,
    storage_root: &Path,
    manifest: &MarketplaceManifest,
    marketplace: &str,
    name: &str,
    cancelled: &AtomicBool,
) {
    let allow: std::collections::HashSet<String> = manifest
        .allow_cross_marketplace_dependencies_on
        .iter()
        .cloned()
        .collect();
    match crate::dependencies::resolve_dependency_closure(
        storage_root,
        marketplace,
        name,
        &allow,
        None,
    )
    .await
    {
        Ok(_) => {}
        Err(error) => {
            if !cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                diagnostics.push(to_validation_diagnostic(
                    &error,
                    Some(&format!("{name}@{marketplace}")),
                ));
            }
        }
    }
}

async fn push_dependency_diagnostics_from_manifest(
    diagnostics: &mut Vec<Diagnostic>,
    storage_root: &Path,
    manifest: &MarketplaceManifest,
    marketplace: &str,
    name: &str,
) {
    let allow: std::collections::HashSet<String> = manifest
        .allow_cross_marketplace_dependencies_on
        .iter()
        .cloned()
        .collect();
    if let Err(error) = crate::dependencies::resolve_dependency_closure(
        storage_root,
        marketplace,
        name,
        &allow,
        Some(manifest),
    )
    .await
    {
        diagnostics.push(to_validation_diagnostic(
            &error,
            Some(&format!("{name}@{marketplace}")),
        ));
    }
}
