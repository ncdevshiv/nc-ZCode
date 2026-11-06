//! Orchestration layer for the plugin-management channel — port of
//! `bootstrap/src/plugins.ts` (install/uninstall/overview/configure/validate/describe)
//! over the Rust marketplace engine and plugin discovery.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use nc_code_service_core::{AppPaths, ServiceError};
use serde_json::{Map, Value};

use crate::app_config::{
    add_suppressed_builtin, enable_plugins_by_default, remove_plugin, remove_plugin_enabled,
    remove_suppressed_builtin, set_plugin_enabled, update_plugin_options, ConfigFiles,
    MergedConfig,
};
use crate::diagnostics::{to_protocol_values, Diagnostic};
use crate::discovery::{discover_plugins, DiscoveryOutcome, PluginMetadata};
use crate::fetch::Fetcher;
use crate::fsutil::{is_directory, now_iso, resolve_user_path};
use crate::manifest::MarketplaceEntry;
use crate::marketplace::{
    add_marketplace, declared_marketplace_record, marketplace_source_repoint_diagnostic,
    remove_marketplace, reserved_marketplace_declaration_diagnostic, update_marketplace,
};
use crate::marketplace_store::{
    load_installed_plugins, load_known_marketplaces, load_marketplace_manifest,
};
use crate::official::{is_cua_internal_feature_enabled, scan_official_cache_roots, CUA_PLUGIN_ID};
use crate::project::{
    available_plugin_summary, builtin_installed_summary, dry_run_marketplace_summary,
    installed_plugin_summary, marketplace_summary, missing_configured_plugins, to_plugin_info,
};
use crate::sources::{
    parse_marketplace_source_input, resolve_source_path, source_kind, source_str,
};
use crate::types::{
    is_official_marketplace_id, parse_plugin_id, InstalledPlugin, KnownMarketplace,
    OFFICIAL_MARKETPLACE_ID,
};
use crate::validate::{
    describe_marketplace_plugin, to_validation_diagnostic, validate_local_plugin_path,
    validate_marketplace_plugin, validate_marketplace_source, DescribeResult,
};

pub struct PluginContext {
    pub paths: AppPaths,
    pub full: MergedConfig,
    pub view: MergedConfig,
    pub storage_root: PathBuf,
    pub workspace_path: PathBuf,
}

impl PluginContext {
    pub async fn load(
        paths: &AppPaths,
        workspace_path: &str,
        config_scope: Option<&str>,
    ) -> Result<Self, ServiceError> {
        let workspace_path = PathBuf::from(workspace_path);
        let full =
            ConfigFiles::load(paths, Some(workspace_path.to_string_lossy().as_ref())).await?;
        let view = if config_scope == Some("user") {
            ConfigFiles::load(paths, None).await?
        } else {
            full.clone()
        };
        let storage_root = resolve_plugin_storage_root(paths, &full);
        Ok(Self {
            paths: paths.clone(),
            full,
            view,
            storage_root,
            workspace_path,
        })
    }

    /// Re-read every config layer (`resolvePluginContext({configResult: undefined})`).
    pub async fn reload(&self) -> Result<Self, ServiceError> {
        Self::load(
            &self.paths,
            self.workspace_path.to_string_lossy().as_ref(),
            None,
        )
        .await
    }

    pub async fn discover(&self) -> DiscoveryOutcome {
        discover_plugins(
            &self.view,
            &self.storage_root,
            self.workspace_path.to_string_lossy().as_ref(),
        )
        .await
    }

    pub async fn discover_full(&self) -> DiscoveryOutcome {
        discover_plugins(
            &self.full,
            &self.storage_root,
            self.workspace_path.to_string_lossy().as_ref(),
        )
        .await
    }

    pub fn user_config_path(&self) -> PathBuf {
        self.full.user.path.clone()
    }

    pub fn config_path_for_scope(&self, scope: Option<&str>) -> PathBuf {
        match scope {
            Some("workspace") => self.workspace_path.join(".zcode").join("config.json"),
            _ => self.full.user.path.clone(),
        }
    }
}

/// `resolvePluginStorageRoot`: `<storage.dir>/cli/plugins`.
pub fn resolve_plugin_storage_root(paths: &AppPaths, config: &MergedConfig) -> PathBuf {
    let storage = resolve_user_path(&config.storage_dir(), paths.home_dir());
    let cli_root = if storage.file_name().and_then(|name| name.to_str()) == Some("cli") {
        storage
    } else {
        storage.join("cli")
    };
    cli_root.join("plugins")
}

/// `listPlugins`: discovered plugins plus configured-but-missing entries.
pub fn list_plugins_value(ctx: &PluginContext, outcome: &DiscoveryOutcome) -> Value {
    let mut plugins: Vec<Value> = outcome
        .plugins
        .iter()
        .map(|plugin| to_plugin_info(plugin, &ctx.view, &ctx.paths))
        .collect();
    let discovered: Vec<String> = outcome
        .plugins
        .iter()
        .map(|plugin| plugin.id.clone())
        .collect();
    plugins.extend(missing_configured_plugins(&ctx.view, &discovered));
    serde_json::json!({
        "plugins": plugins,
        "diagnostics": to_protocol_values(&outcome.diagnostics),
    })
}

pub struct OverviewData {
    pub marketplaces: Vec<Value>,
    pub available: Vec<Value>,
    pub installed: Vec<Value>,
    pub restorable: Vec<Value>,
    pub diagnostics: Vec<Diagnostic>,
}

/// `getZCodePluginsOverview`. Reads cached marketplace manifests only — the overview never
/// touches the network (the TypeScript handler has no fetcher either).
pub async fn overview_data(ctx: &PluginContext) -> OverviewData {
    let outcome = ctx.discover().await;
    let known = load_known_marketplaces(&ctx.storage_root)
        .await
        .unwrap_or_default();
    let declared = declared_marketplace_sources(ctx);
    let effective = effective_marketplace_records(&known, &declared);
    let mut diagnostics = outcome.diagnostics.clone();
    diagnostics.extend(marketplace_declaration_diagnostics(&known, &declared));
    for record in &known {
        if let Some(failure) = &record.last_refresh_failure {
            diagnostics.push(
                Diagnostic::error(
                    failure.get("code").and_then(Value::as_str).unwrap_or(""),
                    failure.get("message").and_then(Value::as_str).unwrap_or(""),
                )
                .with_plugin_id(&record.id),
            );
        }
    }
    let installed = load_installed_plugins(&ctx.storage_root)
        .await
        .unwrap_or_default();
    let installed_ids: HashSet<String> = installed.iter().map(|record| record.id.clone()).collect();

    let mut marketplaces = Vec::new();
    let mut available = Vec::new();
    let mut latest_version: HashMap<String, String> = HashMap::new();
    let mut latest_sha: HashMap<String, String> = HashMap::new();
    let mut listing_by_id: HashMap<String, Map<String, Value>> = HashMap::new();
    let mut official_catalog: Vec<MarketplaceEntry> = Vec::new();
    for (record, use_cached_manifest) in &effective {
        let manifest = if *use_cached_manifest {
            load_marketplace_manifest(&ctx.storage_root, &record.id).await
        } else {
            None
        };
        let featured = manifest
            .as_ref()
            .map(|manifest| manifest.featured.clone())
            .filter(|featured| !featured.is_empty());
        let plugin_count = manifest
            .as_ref()
            .map(|manifest| visible_marketplace_plugin_count(&record.id, &manifest.plugins));
        marketplaces.push(marketplace_summary(record, featured, plugin_count));
        let entries = manifest
            .as_ref()
            .map(|manifest| manifest.plugins.clone())
            .unwrap_or_default();
        if record.id == OFFICIAL_MARKETPLACE_ID {
            official_catalog = entries.clone();
        }
        for entry in entries {
            let data = available_plugin_summary(&entry, &record.id, &installed_ids);
            let id = data
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(version) = &entry.version {
                latest_version.insert(id.clone(), version.clone());
            }
            if let Some(sha) = crate::types::read_source_identity_pin(entry.source.as_ref()) {
                latest_sha.insert(id.clone(), sha);
            }
            if let Some(listing) = &entry.listing {
                listing_by_id.insert(id, listing.clone());
            }
            available.push(data);
        }
    }

    let loaded_by_id: HashMap<String, &PluginMetadata> = outcome
        .plugins
        .iter()
        .map(|plugin| (plugin.id.clone(), plugin))
        .collect();
    let mut installed_summaries = Vec::new();
    for record in &installed {
        let enabled = ctx
            .view
            .enabled_plugins()
            .get(&record.id)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let loaded = loaded_by_id.get(&record.id).copied();
        let base = installed_plugin_summary(record, enabled, loaded);
        let update_status = crate::version::compare_plugin_update(
            base.get("version").and_then(Value::as_str),
            crate::types::read_source_identity_pin(record.source.as_ref()).as_deref(),
            latest_version.get(&record.id).map(String::as_str),
            latest_sha.get(&record.id).map(String::as_str),
        );
        let mut summary = base.as_object().cloned().unwrap_or_default();
        summary.insert(
            "updateStatus".into(),
            Value::String(update_status.to_string()),
        );
        if let Some(label) = latest_version.get(&record.id).cloned().or_else(|| {
            latest_sha
                .get(&record.id)
                .map(|sha| sha.chars().take(7).collect())
        }) {
            summary.insert("latestVersion".into(), Value::String(label));
        }
        if let Some(listing) = listing_by_id.get(&record.id) {
            summary.insert("listing".into(), Value::Object(listing.clone()));
        }
        installed_summaries.push(Value::Object(summary));
    }

    let restorable = restorable_builtins(ctx, &official_catalog).await;
    OverviewData {
        marketplaces,
        available,
        installed: installed_summaries,
        restorable,
        diagnostics,
    }
}

fn visible_marketplace_plugin_count(marketplace: &str, plugins: &[MarketplaceEntry]) -> u64 {
    if marketplace == OFFICIAL_MARKETPLACE_ID {
        plugins
            .iter()
            .filter(|entry| entry.name != "node-repl-host")
            .count() as u64
    } else {
        plugins.len() as u64
    }
}

/// `restorableBuiltins`: suppressed official plugins that still exist in the official cache.
async fn restorable_builtins(ctx: &PluginContext, catalog: &[MarketplaceEntry]) -> Vec<Value> {
    let suppressed = ctx.view.suppressed_builtins();
    if suppressed.is_empty() {
        return Vec::new();
    }
    let cua_enabled = is_cua_internal_feature_enabled();
    let roots = scan_official_cache_roots(&ctx.storage_root).await;
    let mut cached: HashMap<String, (String, PathBuf)> = HashMap::new();
    for root in roots {
        if let Ok(Some((manifest, _))) = crate::components::read_manifest_from_root(&root, None) {
            let name = manifest
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let version = manifest
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("0.0.0")
                .to_string();
            cached.entry(name).or_insert((version, root));
        }
    }
    let mut out = Vec::new();
    for plugin_id in suppressed {
        let Ok((name, marketplace)) = parse_plugin_id(&plugin_id) else {
            continue;
        };
        if marketplace != OFFICIAL_MARKETPLACE_ID {
            continue;
        }
        if plugin_id == CUA_PLUGIN_ID && !cua_enabled {
            continue;
        }
        let Some((version, _root)) = cached.get(&name) else {
            continue;
        };
        let mut summary = Map::new();
        summary.insert("id".into(), Value::String(plugin_id));
        summary.insert("name".into(), Value::String(name.clone()));
        summary.insert(
            "marketplace".into(),
            Value::String(OFFICIAL_MARKETPLACE_ID.into()),
        );
        summary.insert("version".into(), Value::String(version.clone()));
        summary.insert("installed".into(), Value::Bool(false));
        if let Some(listing) = catalog
            .iter()
            .find(|entry| entry.name == name)
            .and_then(|entry| entry.listing.clone())
        {
            summary.insert("listing".into(), Value::Object(listing));
        }
        out.push(Value::Object(summary));
    }
    out
}

/// `resolveDeclaredMarketplaceSources` from `plugins.extraKnownMarketplaces`.
fn declared_marketplace_sources(ctx: &PluginContext) -> Map<String, Value> {
    let base_directory = ctx
        .full
        .user
        .path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut declared = Map::new();
    for (marketplace_id, declaration) in ctx.view.extra_known_marketplaces() {
        let Some(source) = declaration.get("source").cloned() else {
            continue;
        };
        let kind = source_kind(&source);
        if kind == "file" || kind == "directory" {
            if let Some(path) = source_str(&source, "path") {
                let resolved = resolve_source_path(path, &base_directory, ctx.paths.home_dir());
                let mut source = source.clone();
                if let Some(object) = source.as_object_mut() {
                    object.insert(
                        "path".into(),
                        Value::String(resolved.to_string_lossy().into_owned()),
                    );
                }
                declared.insert(marketplace_id, source);
                continue;
            }
        }
        declared.insert(marketplace_id, source);
    }
    declared
}

fn effective_marketplace_records(
    known: &[KnownMarketplace],
    declared: &Map<String, Value>,
) -> Vec<(KnownMarketplace, bool)> {
    let mut records: Vec<(KnownMarketplace, bool)> = known
        .iter()
        .map(|record| match declared.get(&record.id) {
            None => (record.clone(), true),
            Some(source) if record.source == *source => (record.clone(), true),
            Some(_) if is_official_marketplace_id(&record.id) => (record.clone(), true),
            Some(source) => (
                declared_marketplace_record(&record.id, source.clone()),
                false,
            ),
        })
        .collect();
    for (marketplace_id, source) in declared {
        if known.iter().any(|record| record.id == *marketplace_id)
            || is_official_marketplace_id(marketplace_id)
        {
            continue;
        }
        records.push((
            declared_marketplace_record(marketplace_id, source.clone()),
            false,
        ));
    }
    records
}

fn marketplace_declaration_diagnostics(
    known: &[KnownMarketplace],
    declared: &Map<String, Value>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (marketplace_id, source) in declared {
        if !is_official_marketplace_id(marketplace_id) {
            continue;
        }
        let matches_known = known
            .iter()
            .find(|record| record.id == *marketplace_id)
            .is_some_and(|record| record.source == *source);
        if !matches_known {
            diagnostics.push(reserved_marketplace_declaration_diagnostic(marketplace_id));
        }
    }
    diagnostics
}

/// `resolvePluginSelector`: exact id, then a unique bare name.
pub fn resolve_plugin_selector(
    selector: &str,
    plugins: &[PluginMetadata],
) -> Result<PluginMetadata, ServiceError> {
    let normalized = selector.trim();
    if let Some(exact) = plugins.iter().find(|plugin| plugin.id == normalized) {
        return Ok(exact.clone());
    }
    let matches: Vec<&PluginMetadata> = plugins
        .iter()
        .filter(|plugin| plugin.name == normalized)
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => Err(ServiceError::new(format!("Plugin not found: {normalized}"))),
        _ => Err(ServiceError::new(format!(
            "Plugin name is ambiguous, use full plugin id: {normalized}"
        ))),
    }
}

/// `toMarketplaceInstallDiagnostic` used by install/update.
pub fn to_install_diagnostic(error: &ServiceError, plugin_id: &str) -> Diagnostic {
    if error.name == "MarketplaceSourceRepointError" {
        return Diagnostic::error("plugin_marketplace_invalid", error.message.clone())
            .with_plugin_id(plugin_id);
    }
    if let Some(code) = crate::validate::source_diagnostic_code(error) {
        return Diagnostic::error(code, error.message.clone()).with_plugin_id(plugin_id);
    }
    if error.message.starts_with("Plugin not found:") {
        return Diagnostic::error("plugin_not_found", error.message.clone())
            .with_plugin_id(plugin_id);
    }
    if error.message.contains("Cross-marketplace dependency") {
        return Diagnostic::error("plugin_dependency_cross_marketplace", error.message.clone())
            .with_plugin_id(plugin_id);
    }
    if error.message.contains("dependency cycle") {
        return Diagnostic::error("plugin_dependency_cycle", error.message.clone())
            .with_plugin_id(plugin_id);
    }
    if error.message.contains("Dependency not found")
        || error
            .message
            .contains("Marketplace not found for dependency")
    {
        return Diagnostic::error("plugin_dependency_missing", error.message.clone())
            .with_plugin_id(plugin_id);
    }
    if error
        .message
        .contains("source is recognized but not supported")
    {
        return Diagnostic::error(
            "plugin_marketplace_source_unsupported",
            error.message.clone(),
        )
        .with_plugin_id(plugin_id);
    }
    Diagnostic::error("plugin_marketplace_invalid", error.message.clone()).with_plugin_id(plugin_id)
}

/// `addZCodePluginMarketplace`.
pub async fn add_plugin_marketplace(
    fetcher: &Fetcher,
    ctx: &PluginContext,
    source: &str,
    dry_run: bool,
    canceled: &AtomicBool,
) -> Result<Value, ServiceError> {
    let parsed =
        parse_marketplace_source_input(source, ctx.paths.home_dir(), ctx.workspace_path.as_path())?;
    if dry_run {
        return Ok(dry_run_marketplace_summary(&parsed));
    }
    let record = add_marketplace(fetcher, &ctx.storage_root, &parsed, None, None, canceled).await?;
    Ok(marketplace_summary(&record, None, None))
}

/// `updateZCodePluginMarketplace`.
pub async fn update_plugin_marketplace(
    fetcher: &Fetcher,
    ctx: &PluginContext,
    marketplace: Option<&str>,
    canceled: &AtomicBool,
) -> Result<(Vec<Value>, Vec<Diagnostic>), ServiceError> {
    let declared = declared_marketplace_sources(ctx);
    let known = load_known_marketplaces(&ctx.storage_root).await?;
    let known_by_id: HashMap<String, KnownMarketplace> = known
        .iter()
        .map(|record| (record.id.clone(), record.clone()))
        .collect();
    let target_ids: Vec<String> = match marketplace {
        Some(id) => vec![id.to_string()],
        None => {
            let mut ids: Vec<String> = Vec::new();
            for id in known_by_id.keys() {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
            ids
        }
    };
    if let Some(marketplace) = marketplace {
        if !known_by_id.contains_key(marketplace) && !declared.contains_key(marketplace) {
            return Err(ServiceError::new(format!(
                "Marketplace not found: {marketplace}"
            )));
        }
    }
    let mut updated = Vec::new();
    let mut diagnostics = Vec::new();
    for marketplace_id in target_ids {
        let declaration = declared.get(&marketplace_id);
        let known_record = known_by_id.get(&marketplace_id);
        if marketplace.is_some()
            && declaration
                .is_some_and(|source| known_record.is_some_and(|record| record.source != *source))
        {
            diagnostics.push(marketplace_source_repoint_diagnostic(&marketplace_id));
            continue;
        }
        if let Some(source) = declaration {
            if known_record.is_none() {
                match add_marketplace(
                    fetcher,
                    &ctx.storage_root,
                    source,
                    Some(&marketplace_id),
                    None,
                    canceled,
                )
                .await
                {
                    Ok(record) => updated.push(record),
                    Err(error) => {
                        diagnostics.push(to_validation_diagnostic(&error, Some(&marketplace_id)));
                    }
                }
                continue;
            }
        }
        let refreshed =
            update_marketplace(fetcher, &ctx.storage_root, Some(&marketplace_id), canceled).await?;
        updated.extend(refreshed);
    }
    let records = load_known_marketplaces(&ctx.storage_root).await?;
    for record in records {
        if let Some(selected) = marketplace {
            if record.id != selected {
                continue;
            }
        }
        if let Some(failure) = &record.last_refresh_failure {
            diagnostics.push(
                Diagnostic::error(
                    failure.get("code").and_then(Value::as_str).unwrap_or(""),
                    failure.get("message").and_then(Value::as_str).unwrap_or(""),
                )
                .with_plugin_id(&record.id),
            );
        }
    }
    let summaries = updated
        .iter()
        .map(|record| marketplace_summary(record, None, None))
        .collect();
    Ok((summaries, diagnostics))
}

/// `removeZCodePluginMarketplace`: drop the known record; cached catalogs stay.
pub async fn remove_plugin_marketplace(
    ctx: &PluginContext,
    marketplace: &str,
) -> Result<(), ServiceError> {
    remove_marketplace(&ctx.storage_root, marketplace).await
}

pub struct InstallProtocolOutcome {
    pub closure: Vec<String>,
    pub installed: Vec<Value>,
    pub diagnostics: Vec<Diagnostic>,
}

/// `installZCodeMarketplacePlugin` (the protocol handler's body).
pub async fn install_plugin(
    fetcher: &Fetcher,
    ctx: &PluginContext,
    marketplace: &str,
    plugin_name: &str,
    dry_run: bool,
    canceled: &AtomicBool,
) -> Result<InstallProtocolOutcome, ServiceError> {
    if dry_run {
        return dry_run_install(fetcher, ctx, marketplace, plugin_name, canceled).await;
    }
    let plugin_id = format!("{plugin_name}@{marketplace}");
    let bundled_entry = load_marketplace_manifest(&ctx.storage_root, marketplace)
        .await
        .and_then(|manifest| {
            manifest
                .plugins
                .into_iter()
                .find(|entry| entry.name == plugin_name)
        });
    let suppressed = ctx.view.suppressed_builtins();
    let is_suppressed_bundled_official = marketplace == OFFICIAL_MARKETPLACE_ID
        && suppressed.contains(&plugin_id)
        && bundled_entry.as_ref().is_some_and(|entry| {
            matches!(
                entry.source.as_ref().and_then(Value::as_str),
                Some("filesystem") | Some("sea")
            )
        });
    if is_suppressed_bundled_official {
        restore_builtin_core(ctx, &plugin_id).await?;
        let fresh = ctx.reload().await?;
        let outcome = fresh.discover_full().await;
        let Some(restored) = outcome.plugins.iter().find(|plugin| plugin.id == plugin_id) else {
            return Ok(InstallProtocolOutcome {
                closure: Vec::new(),
                installed: Vec::new(),
                diagnostics: vec![Diagnostic::error(
                    "plugin_not_found",
                    format!("Bundled plugin could not be restored: {plugin_id}"),
                )
                .with_plugin_id(&plugin_id)],
            });
        };
        let now = now_iso();
        let record = InstalledPlugin {
            id: restored.id.clone(),
            name: restored.name.clone(),
            marketplace: restored.marketplace.clone(),
            version: restored.version.clone().unwrap_or_default(),
            install_path: restored.root_path.to_string_lossy().into_owned(),
            installed_at: now.clone(),
            updated_at: Some(now),
            scope: "user".into(),
            dependencies: None,
            source: None,
            cache_transaction_id: None,
        };
        return Ok(InstallProtocolOutcome {
            closure: vec![plugin_id],
            installed: vec![installed_plugin_summary(
                &record,
                restored.enabled,
                Some(restored),
            )],
            diagnostics: Vec::new(),
        });
    }

    let install_result = async {
        materialize_declared_marketplace(fetcher, ctx, marketplace, canceled).await?;
        crate::install::install_marketplace_plugin(
            fetcher,
            &ctx.storage_root,
            marketplace,
            plugin_name,
            "user",
            canceled,
        )
        .await
    }
    .await;
    let outcome = match install_result {
        Ok(outcome) => outcome,
        Err(error) => {
            return Ok(InstallProtocolOutcome {
                closure: Vec::new(),
                installed: Vec::new(),
                diagnostics: vec![to_install_diagnostic(&error, &plugin_id)],
            });
        }
    };
    let user_config = ctx.user_config_path();
    if marketplace == OFFICIAL_MARKETPLACE_ID {
        for record in &outcome.installed {
            let _ = remove_suppressed_builtin(&user_config, &record.id).await;
        }
    }
    let ids: Vec<String> = outcome
        .installed
        .iter()
        .map(|record| record.id.clone())
        .collect();
    let enabled_ids = enable_plugins_by_default(&user_config, &ids).await?;
    let enabled_set: HashSet<String> = enabled_ids.into_iter().collect();
    let enabled_plugins = ctx.view.enabled_plugins();
    let enabled_by_id = |id: &str| {
        enabled_set.contains(id)
            || enabled_plugins
                .get(id)
                .and_then(Value::as_bool)
                .unwrap_or(false)
    };
    let installed = outcome
        .installed
        .iter()
        .map(|record| installed_plugin_summary(record, enabled_by_id(&record.id), None))
        .collect();
    Ok(InstallProtocolOutcome {
        closure: outcome.closure,
        installed,
        diagnostics: Vec::new(),
    })
}

async fn dry_run_install(
    fetcher: &Fetcher,
    ctx: &PluginContext,
    marketplace: &str,
    plugin_name: &str,
    canceled: &AtomicBool,
) -> Result<InstallProtocolOutcome, ServiceError> {
    let declared = declared_marketplace_sources(ctx);
    let known = load_known_marketplaces(&ctx.storage_root)
        .await?
        .into_iter()
        .find(|record| record.id == marketplace);
    let declaration = declared.get(marketplace);
    if let (Some(declaration), Some(known)) = (declaration, known.as_ref()) {
        if known.source != *declaration {
            return Ok(InstallProtocolOutcome {
                closure: Vec::new(),
                installed: Vec::new(),
                diagnostics: vec![marketplace_source_repoint_diagnostic(marketplace)],
            });
        }
    }
    if let Some(declaration) = declaration {
        if known.is_none()
            || load_marketplace_manifest(&ctx.storage_root, marketplace)
                .await
                .is_none()
        {
            let diagnostics = validate_marketplace_source(
                fetcher,
                &ctx.storage_root,
                declaration,
                Some(marketplace),
                Some(plugin_name),
                canceled,
            )
            .await;
            return Ok(InstallProtocolOutcome {
                closure: Vec::new(),
                installed: Vec::new(),
                diagnostics,
            });
        }
    }
    let diagnostics = validate_marketplace_plugin(
        fetcher,
        &ctx.storage_root,
        marketplace,
        plugin_name,
        canceled,
    )
    .await;
    Ok(InstallProtocolOutcome {
        closure: Vec::new(),
        installed: Vec::new(),
        diagnostics,
    })
}

/// `materializeDeclaredMarketplaceForExplicitAction`.
async fn materialize_declared_marketplace(
    fetcher: &Fetcher,
    ctx: &PluginContext,
    marketplace_id: &str,
    canceled: &AtomicBool,
) -> Result<(), ServiceError> {
    let Some(source) = declared_marketplace_sources(ctx)
        .get(marketplace_id)
        .cloned()
    else {
        return Ok(());
    };
    let known = load_known_marketplaces(&ctx.storage_root)
        .await?
        .into_iter()
        .find(|record| record.id == marketplace_id);
    if let Some(known) = &known {
        if known.source != source {
            return Err(ServiceError::named(
                "MarketplaceSourceRepointError",
                marketplace_source_repoint_diagnostic(marketplace_id).message,
            ));
        }
        if load_marketplace_manifest(&ctx.storage_root, marketplace_id)
            .await
            .is_some()
        {
            return Ok(());
        }
    }
    add_marketplace(
        fetcher,
        &ctx.storage_root,
        &source,
        Some(marketplace_id),
        None,
        canceled,
    )
    .await?;
    Ok(())
}

/// `uninstallZCodeMarketplacePlugin`.
pub async fn uninstall_plugin(
    ctx: &PluginContext,
    plugin_id: Option<&str>,
    plugin_name: Option<&str>,
    marketplace: Option<&str>,
    remove_cache: Option<bool>,
) -> Result<Option<Value>, ServiceError> {
    let plugin_id = match (plugin_id, plugin_name, marketplace) {
        (Some(id), _, _) => id.to_string(),
        (None, Some(name), Some(marketplace)) => format!("{name}@{marketplace}"),
        _ => {
            return Err(ServiceError::new(
                "pluginId or pluginName + marketplace is required",
            ))
        }
    };
    let installed = load_installed_plugins(&ctx.storage_root).await?;
    if installed.iter().any(|record| record.id == plugin_id) {
        let removed = crate::install::uninstall_marketplace_plugin(
            &ctx.storage_root,
            &plugin_id,
            remove_cache.unwrap_or(true),
            false,
        )
        .await?;
        let Some(removed) = removed else {
            return Ok(None);
        };
        let user_config = ctx.user_config_path();
        let _ = remove_plugin(&user_config, &removed.id).await;
        let _ = remove_suppressed_builtin(&user_config, &removed.id).await;
        return Ok(Some(installed_plugin_summary(&removed, false, None)));
    }
    let outcome = ctx.discover_full().await;
    let builtin = outcome
        .plugins
        .iter()
        .find(|plugin| plugin.id == plugin_id && plugin.source == "official");
    let Some(builtin) = builtin else {
        return Ok(None);
    };
    let user_config = ctx.user_config_path();
    add_suppressed_builtin(&user_config, &plugin_id).await?;
    let _ = remove_plugin(&user_config, &plugin_id).await;
    crate::fsutil::remove_path_best_effort(&crate::marketplace_store::plugin_data_dir(
        &ctx.storage_root,
        &plugin_id,
    ));
    let now = now_iso();
    Ok(Some(builtin_installed_summary(
        &builtin.id,
        &builtin.name,
        &builtin.marketplace,
        builtin.version.as_deref().unwrap_or(""),
        &builtin.root_path,
        &now,
    )))
}

/// `updatePlugin`: reinstall every selected installed record through the same install
/// core (enable-by-default and suppression cleanup included).
pub async fn update_plugin(
    fetcher: &Fetcher,
    ctx: &PluginContext,
    plugin_id: Option<&str>,
    marketplace: Option<&str>,
    canceled: &AtomicBool,
) -> Result<InstallProtocolOutcome, ServiceError> {
    let installed = load_installed_plugins(&ctx.storage_root).await?;
    let selected: Vec<InstalledPlugin> = installed
        .into_iter()
        .filter(|record| match plugin_id {
            Some(id) => record.id == id,
            None => match marketplace {
                Some(marketplace) => record.marketplace == marketplace,
                None => true,
            },
        })
        .collect();
    let mut installed_plugins = Vec::new();
    let mut dependency_closure = Vec::new();
    let mut diagnostics = Vec::new();
    for record in selected {
        // Each reload is a new network/disk boundary: cancellation takes effect between records to avoid a canceled
        // update continues reinstalling the remaining plugins (AbortError is projected by the caller as a cancellation, not a diagnostic).
        if canceled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ServiceError::named(
                "AbortError",
                "Plugin operation canceled",
            ));
        }
        let outcome = install_plugin(
            fetcher,
            ctx,
            &record.marketplace,
            &record.name,
            false,
            canceled,
        )
        .await?;
        installed_plugins.extend(outcome.installed);
        dependency_closure.extend(outcome.closure);
        diagnostics.extend(outcome.diagnostics);
    }
    if canceled.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(ServiceError::named(
            "AbortError",
            "Plugin operation canceled",
        ));
    }
    Ok(InstallProtocolOutcome {
        closure: dependency_closure,
        installed: installed_plugins,
        diagnostics,
    })
}
/// `restoreBuiltinPlugin` core: clear the suppression marker; discovery is stateless, so
/// the plugin becomes visible again without a seed pass (the Rust host bundles no assets).
pub async fn restore_builtin_core(
    ctx: &PluginContext,
    plugin_id: &str,
) -> Result<(), ServiceError> {
    if plugin_id == CUA_PLUGIN_ID && !is_cua_internal_feature_enabled() {
        return Err(ServiceError::new(
            "computer-use built-in plugin requires nc_code_CUA_PRODUCT_HELPER to be enabled",
        ));
    }
    let path = ctx.user_config_path();
    remove_suppressed_builtin(&path, plugin_id).await
}

/// `setZCodePluginEnabled`.
pub async fn set_plugin_enabled_for_plugin(
    ctx: &PluginContext,
    plugin_selector: &str,
    enabled: bool,
    scope: Option<&str>,
) -> Result<(PluginMetadata, bool), ServiceError> {
    let outcome = ctx.discover_full().await;
    let mut plugin = resolve_plugin_selector(plugin_selector, &outcome.plugins)?;
    let path = ctx.config_path_for_scope(scope);
    set_plugin_enabled(&path, &plugin.id, enabled).await?;
    // `setZCodePluginEnabled` returns the enabled value after writing (patch.enabled), not when it was discovered
    // The old state; `plugin.enabled` of the protocol layer must be consistent with `enabled`.
    plugin.enabled = enabled;
    Ok((plugin, enabled))
}

/// `configureZCodePlugin`.
pub async fn configure_plugin(
    ctx: &PluginContext,
    plugin_id: &str,
    options: &Map<String, Value>,
    clear_option_keys: Option<&Vec<String>>,
    scope: Option<&str>,
    dry_run: bool,
) -> Result<(), ServiceError> {
    let normalized = normalize_plugin_options(options);
    let clear_keys = normalize_option_keys(clear_option_keys);
    let outcome = ctx.discover_full().await;
    let plugin = resolve_plugin_selector(plugin_id, &outcome.plugins)?;
    if dry_run {
        return Ok(());
    }
    let path = ctx.config_path_for_scope(scope);
    update_plugin_options(&path, &plugin.id, &normalized, &clear_keys).await
}

fn normalize_plugin_options(options: &Map<String, Value>) -> Map<String, Value> {
    options
        .iter()
        .filter(|(_, value)| value.is_string() || value.is_number() || value.is_boolean())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn normalize_option_keys(keys: Option<&Vec<String>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in keys.into_iter().flatten() {
        let trimmed = key.trim();
        if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// `resetZCodePluginConfig`.
pub async fn reset_plugin_config(
    ctx: &PluginContext,
    plugin_id: &str,
    scope: Option<&str>,
) -> Result<(), ServiceError> {
    let path = ctx.config_path_for_scope(scope);
    if scope == Some("workspace") {
        // "Restore inheritance" only deletes Workspace enable override, options is an independent configuration dimension.
        remove_plugin_enabled(&path, plugin_id).await
    } else {
        remove_plugin(&path, plugin_id).await
    }
}

/// `validateZCodePlugin` (without the compatibility projection).
pub async fn validate_plugin(
    fetcher: &Fetcher,
    ctx: &PluginContext,
    marketplace: Option<&str>,
    plugin_name: Option<&str>,
    source: Option<&str>,
    canceled: &AtomicBool,
) -> Vec<Diagnostic> {
    if let Some(source) = source {
        let parsed = match parse_marketplace_source_input(
            source,
            ctx.paths.home_dir(),
            ctx.workspace_path.as_path(),
        ) {
            Ok(parsed) => parsed,
            Err(error) => {
                return vec![Diagnostic::error(
                    "plugin_marketplace_invalid",
                    error.message,
                )]
            }
        };
        return validate_marketplace_source(
            fetcher,
            &ctx.storage_root,
            &parsed,
            None,
            None,
            canceled,
        )
        .await;
    }
    if let (Some(marketplace), Some(plugin_name)) = (marketplace, plugin_name) {
        return validate_marketplace_plugin(
            fetcher,
            &ctx.storage_root,
            marketplace,
            plugin_name,
            canceled,
        )
        .await;
    }
    Vec::new()
}

/// `validateZCodePluginPath`.
pub async fn validate_local_path(
    fetcher: &Fetcher,
    ctx: &PluginContext,
    path: &str,
    canceled: &AtomicBool,
) -> Vec<Diagnostic> {
    if is_directory(Path::new(path)) || Path::new(path).is_file() {
        return validate_local_plugin_path(fetcher, &ctx.storage_root, path, canceled).await;
    }
    vec![Diagnostic::error(
        "plugin_manifest_not_found",
        format!("Path does not exist: {path}"),
    )
    .with_path(path)]
}

/// `describeZCodePlugin`.
pub async fn describe_plugin(
    fetcher: &Fetcher,
    ctx: &PluginContext,
    marketplace: &str,
    plugin_name: &str,
    canceled: &AtomicBool,
) -> DescribeResult {
    describe_marketplace_plugin(
        fetcher,
        &ctx.storage_root,
        marketplace,
        plugin_name,
        canceled,
    )
    .await
}
