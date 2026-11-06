//! Marketplace install / cache / uninstall — port of `installMarketplacePlugin`,
//! `cacheMarketplacePlugin` and `uninstallMarketplacePlugin` in
//! `adapters/src/plugins/marketplace.ts`.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use nc_code_service_core::ServiceError;
use serde_json::Value;

use crate::archive::cleanup_temp_dir;
use crate::components::create_manifest_from_marketplace_entry;
use crate::fetch::Fetcher;
use crate::fsutil::{create_staging_directory, normalize_path, now_iso, DirectorySwap};
use crate::manifest::{find_plugin_manifest_path, MarketplaceEntry};
use crate::marketplace::ensure_marketplace_manifest_available;
use crate::marketplace_store::{
    load_installed_plugins, load_marketplace_manifest, plugin_cache_dir, plugin_data_dir,
    write_installed_plugins,
};
use crate::source_root::resolve_plugin_source_root;
use crate::types::{parse_plugin_id, InstalledPlugin, DEFAULT_VERSION};

pub struct InstallOutcome {
    pub closure: Vec<String>,
    pub installed: Vec<InstalledPlugin>,
}

struct CachedPlugin {
    record: InstalledPlugin,
    swap: Option<DirectorySwap>,
}

pub async fn install_marketplace_plugin(
    fetcher: &Fetcher,
    storage_root: &Path,
    marketplace: &str,
    name: &str,
    scope: &str,
    cancelled: &AtomicBool,
) -> Result<InstallOutcome, ServiceError> {
    ensure_marketplace_manifest_available(fetcher, storage_root, marketplace, cancelled).await?;
    crate::fetch::check_cancelled(cancelled)?;
    let root_manifest = load_marketplace_manifest(storage_root, marketplace).await;
    let allow: std::collections::HashSet<String> = root_manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .allow_cross_marketplace_dependencies_on
                .iter()
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let closure = crate::dependencies::resolve_dependency_closure(
        storage_root,
        marketplace,
        name,
        &allow,
        None,
    )
    .await?;
    let mut state = load_installed_plugins(storage_root).await?;
    let mut installed = Vec::new();
    let mut swaps: Vec<DirectorySwap> = Vec::new();
    let mut failed: Option<ServiceError> = None;
    for plugin_id in &closure {
        let result: Result<CachedPlugin, ServiceError> = async {
            let (plugin_name, plugin_marketplace) =
                parse_plugin_id(plugin_id).map_err(ServiceError::new)?;
            let manifest = load_marketplace_manifest(storage_root, &plugin_marketplace)
                .await
                .ok_or_else(|| {
                    ServiceError::new(format!("Marketplace not found: {plugin_marketplace}"))
                })?;
            let entry = manifest
                .plugins
                .iter()
                .find(|entry| entry.name == plugin_name)
                .ok_or_else(|| ServiceError::new(format!("Plugin not found: {plugin_id}")))?;
            cache_marketplace_plugin(
                fetcher,
                storage_root,
                &plugin_marketplace,
                entry,
                scope,
                &mut state,
                cancelled,
            )
            .await
        }
        .await;
        match result {
            Ok(cached) => {
                installed.push(cached.record);
                if let Some(swap) = cached.swap {
                    swaps.push(swap);
                }
            }
            Err(error) => {
                failed = Some(error);
                break;
            }
        }
    }
    if failed.is_none() {
        if let Err(error) = crate::fetch::check_cancelled(cancelled) {
            failed = Some(error);
        }
    }
    if failed.is_none() {
        if let Err(error) = write_installed_plugins(storage_root, &state).await {
            failed = Some(error);
        }
    }
    if let Some(error) = failed {
        for swap in swaps.iter_mut().rev() {
            let _ = swap.rollback();
        }
        return Err(error);
    }
    for swap in swaps.iter_mut() {
        swap.commit();
    }
    Ok(InstallOutcome { closure, installed })
}

async fn cache_marketplace_plugin(
    fetcher: &Fetcher,
    storage_root: &Path,
    marketplace: &str,
    entry: &MarketplaceEntry,
    scope: &str,
    state: &mut Vec<InstalledPlugin>,
    cancelled: &AtomicBool,
) -> Result<CachedPlugin, ServiceError> {
    crate::fetch::check_cancelled(cancelled)?;
    let source = resolve_plugin_source_root(
        fetcher,
        storage_root,
        marketplace,
        entry,
        None,
        None,
        cancelled,
    )
    .await?;
    let result = async {
        if is_zip_plugin_url_source(entry.source.as_ref()) {
            assert_zip_plugin_install_root(&source.path, entry, marketplace)?;
        }
        let version = crate::manifest::resolve_installed_plugin_version(
            &source.path,
            entry.version.as_deref(),
        );
        let target = plugin_cache_dir(storage_root, marketplace, &entry.name, &version);
        let mut swap = None;
        if normalize_path(&source.path) != normalize_path(&target) {
            crate::fetch::check_cancelled(cancelled)?;
            let staging = create_staging_directory(&target, Some(&source.path))?;
            ensure_marketplace_entry_manifest(entry, &staging)?;
            swap = Some(DirectorySwap::activate(&staging, &target)?);
        } else {
            ensure_marketplace_entry_manifest(entry, &target)?;
        }
        let now = now_iso();
        let record = InstalledPlugin {
            id: format!("{}@{marketplace}", entry.name),
            name: entry.name.clone(),
            marketplace: marketplace.to_string(),
            version: version.clone(),
            install_path: target.to_string_lossy().into_owned(),
            installed_at: now.clone(),
            updated_at: Some(now),
            scope: scope.to_string(),
            dependencies: Some(entry.dependencies.clone()),
            source: entry.source.clone(),
            cache_transaction_id: swap.as_ref().map(|_| uuid::Uuid::new_v4().to_string()),
        };
        match state.iter_mut().find(|plugin| plugin.id == record.id) {
            Some(previous) => {
                // `{...previousRecord, ...record, installedAt: previous.installedAt ?? record.installedAt}`
                let mut merged = record.clone();
                if !previous.installed_at.is_empty() {
                    merged.installed_at = previous.installed_at.clone();
                }
                *previous = merged;
            }
            None => state.push(record.clone()),
        }
        Ok(CachedPlugin { record, swap })
    }
    .await;
    let cleanup_error = cleanup_temp_dir(source.cleanup);
    match result {
        Ok(cached) => Ok(cached),
        Err(error) => Err(crate::archive::append_cleanup_error(error, cleanup_error)),
    }
}

fn is_zip_plugin_url_source(source: Option<&Value>) -> bool {
    source.is_some_and(|source| {
        source.get("source").and_then(Value::as_str) == Some("url")
            && source.get("type").and_then(Value::as_str) == Some("zip")
            && source.get("url").and_then(Value::as_str).is_some()
            && source.get("sha256").and_then(Value::as_str).is_some()
    })
}

fn assert_zip_plugin_install_root(
    root_path: &Path,
    entry: &MarketplaceEntry,
    marketplace: &str,
) -> Result<(), ServiceError> {
    let plugin_id = format!("{}@{marketplace}", entry.name);
    let loaded = crate::components::read_manifest_from_root(root_path, Some(entry))
        .ok()
        .flatten();
    let Some((manifest, _)) = loaded else {
        return Err(ServiceError::new(format!(
            "Plugin manifest not found: {plugin_id}"
        )));
    };
    let manifest_name = manifest
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if manifest_name != entry.name {
        return Err(ServiceError::new(format!(
            "Plugin manifest name '{manifest_name}' does not match marketplace entry '{}'",
            entry.name
        )));
    }
    Ok(())
}

/// `ensureMarketplaceEntryManifest`: only non-strict entries get a synthetic manifest.
pub fn ensure_marketplace_entry_manifest(
    entry: &MarketplaceEntry,
    target: &Path,
) -> Result<(), ServiceError> {
    if find_plugin_manifest_path(target).is_some() {
        return Ok(());
    }
    if entry.strict {
        return Ok(());
    }
    let manifest_dir = target.join(".claude-plugin");
    std::fs::create_dir_all(&manifest_dir).map_err(|error| {
        ServiceError::new(format!("create {}: {error}", manifest_dir.display()))
    })?;
    let manifest = create_manifest_from_marketplace_entry(entry);
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(manifest))?
    );
    std::fs::write(manifest_dir.join("plugin.json"), text)
        .map_err(|error| ServiceError::new(format!("write plugin manifest: {error}")))
}

/// `uninstallMarketplacePlugin`: drop the record, optionally the cache and data dirs.
pub async fn uninstall_marketplace_plugin(
    storage_root: &Path,
    plugin_id: &str,
    remove_cache: bool,
    keep_data: bool,
) -> Result<Option<InstalledPlugin>, ServiceError> {
    let mut state = load_installed_plugins(storage_root).await?;
    let Some(index) = state.iter().position(|record| record.id == plugin_id) else {
        return Ok(None);
    };
    let removed = state.remove(index);
    write_installed_plugins(storage_root, &state).await?;
    if remove_cache {
        remove_path_force(Path::new(&removed.install_path))?;
        if !keep_data {
            remove_path_force(&plugin_data_dir(storage_root, &removed.id))?;
        }
    }
    Ok(Some(removed))
}

fn remove_path_force(path: &Path) -> Result<(), ServiceError> {
    if !path.exists() {
        return Ok(());
    }
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    result.map_err(|error| ServiceError::new(format!("remove {}: {error}", path.display())))
}

/// Used by `plugins/update`: reinstall an installed record through its marketplace entry.
pub async fn reinstall_record(
    fetcher: &Fetcher,
    storage_root: &Path,
    record: &InstalledPlugin,
    cancelled: &AtomicBool,
) -> Result<InstallOutcome, ServiceError> {
    install_marketplace_plugin(
        fetcher,
        storage_root,
        &record.marketplace,
        &record.name,
        &record.scope,
        cancelled,
    )
    .await
}

pub fn installed_record_root(storage_root: &Path, record: &InstalledPlugin) -> PathBuf {
    if record.install_path.trim().is_empty() {
        plugin_cache_dir(
            storage_root,
            &record.marketplace,
            &record.name,
            if record.version.is_empty() {
                DEFAULT_VERSION
            } else {
                &record.version
            },
        )
    } else {
        PathBuf::from(&record.install_path)
    }
}
