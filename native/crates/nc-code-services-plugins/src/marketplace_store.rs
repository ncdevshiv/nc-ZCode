//! Plugin-storage files: `known_marketplaces.json`, `installed_plugins.json` and the
//! per-marketplace cached `marketplace.json` (paths from
//! `adapters/src/plugins/marketplace.ts`).

use std::path::{Path, PathBuf};

use nc_code_service_core::ServiceError;
use serde_json::{Map, Value};

use crate::fsutil::{read_json_if_exists, write_json_atomic};
use crate::types::{
    InstalledPlugin, KnownMarketplace, INSTALLED_PLUGINS_FILE, KNOWN_MARKETPLACES_FILE,
};

pub fn marketplace_dir(storage_root: &Path, marketplace: &str) -> PathBuf {
    storage_root
        .join("marketplaces")
        .join(crate::types::sanitize_plugin_id(marketplace))
}

pub fn marketplace_manifest_path(storage_root: &Path, marketplace: &str) -> PathBuf {
    marketplace_dir(storage_root, marketplace).join(crate::types::MARKETPLACE_FILE)
}

/// `loadMarketplaceManifestSync`: parsed cached catalog or `None`.
pub async fn load_marketplace_manifest(
    storage_root: &Path,
    marketplace: &str,
) -> Option<crate::manifest::MarketplaceManifest> {
    let value = read_json_if_exists(&marketplace_manifest_path(storage_root, marketplace))
        .await
        .ok()??;
    crate::manifest::parse_marketplace_manifest(&value)
}

pub fn plugin_cache_dir(
    storage_root: &Path,
    marketplace: &str,
    name: &str,
    version: &str,
) -> PathBuf {
    storage_root
        .join("cache")
        .join(crate::types::sanitize_plugin_id(marketplace))
        .join(crate::types::sanitize_plugin_id(name))
        .join(crate::types::sanitize_plugin_id(version))
}

pub fn plugin_data_dir(storage_root: &Path, plugin_id: &str) -> PathBuf {
    storage_root
        .join("data")
        .join(crate::types::sanitize_plugin_id(plugin_id))
}

fn known_path(storage_root: &Path) -> PathBuf {
    storage_root.join(KNOWN_MARKETPLACES_FILE)
}

fn installed_path(storage_root: &Path) -> PathBuf {
    storage_root.join(INSTALLED_PLUGINS_FILE)
}

/// `loadKnownMarketplacesSync`: tolerant of the legacy object map and of corruption
/// (invalid entries are dropped, unreadable files read as empty).
pub async fn load_known_marketplaces(
    storage_root: &Path,
) -> Result<Vec<KnownMarketplace>, ServiceError> {
    let Some(value) = read_json_if_exists(&known_path(storage_root)).await? else {
        return Ok(Vec::new());
    };
    let marketplaces = value.get("marketplaces").cloned().unwrap_or(Value::Null);
    let items: Vec<Value> = match &marketplaces {
        Value::Array(items) => items.clone(),
        Value::Object(record) => record.values().cloned().collect(),
        _ => Vec::new(),
    };
    Ok(items
        .iter()
        .filter_map(KnownMarketplace::from_value)
        .collect())
}

/// `writeKnownMarketplaces` (`{version:1, marketplaces:[...]}`, two-space JSON).
pub async fn write_known_marketplaces(
    storage_root: &Path,
    records: &[KnownMarketplace],
) -> Result<(), ServiceError> {
    let mut root = Map::new();
    root.insert("version".into(), Value::from(1));
    root.insert(
        "marketplaces".into(),
        Value::Array(records.iter().map(KnownMarketplace::to_value).collect()),
    );
    write_json_atomic(&known_path(storage_root), &Value::Object(root)).await
}

/// `loadInstalledPluginsSync` + `normalizeInstalledPluginsState` (array or legacy map).
pub async fn load_installed_plugins(
    storage_root: &Path,
) -> Result<Vec<InstalledPlugin>, ServiceError> {
    let Some(value) = read_json_if_exists(&installed_path(storage_root)).await? else {
        return Ok(Vec::new());
    };
    let plugins = value.get("plugins").cloned().unwrap_or(Value::Null);
    let items: Vec<Value> = match &plugins {
        Value::Array(items) => items.clone(),
        Value::Object(record) => record
            .iter()
            .flat_map(|(plugin_id, entry)| {
                let entries = entry
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![entry.clone()]);
                entries.into_iter().filter_map(|item| {
                    let mut object = item.as_object().cloned()?;
                    object.insert("id".to_string(), Value::String(plugin_id.clone()));
                    Some(Value::Object(object))
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(items
        .iter()
        .filter_map(InstalledPlugin::from_value)
        .collect())
}

/// `saveInstalledPlugins` (`{version:1, plugins:[...]}`).
pub async fn write_installed_plugins(
    storage_root: &Path,
    records: &[InstalledPlugin],
) -> Result<(), ServiceError> {
    let mut root = Map::new();
    root.insert("version".into(), Value::from(1));
    root.insert(
        "plugins".into(),
        Value::Array(records.iter().map(InstalledPlugin::to_value).collect()),
    );
    write_json_atomic(&installed_path(storage_root), &Value::Object(root)).await
}

/// `upsertKnownMarketplace`: replace by id, keep `addedAt`, clear stale
/// `lastRefreshFailure` on success (the record simply omits it).
pub async fn upsert_known_marketplace(
    storage_root: &Path,
    record: &KnownMarketplace,
) -> Result<(), ServiceError> {
    let mut known = load_known_marketplaces(storage_root).await?;
    match known.iter_mut().find(|item| item.id == record.id) {
        Some(existing) => {
            let added_at = existing.added_at.clone();
            *existing = record.clone();
            existing.added_at = added_at;
        }
        None => known.push(record.clone()),
    }
    write_known_marketplaces(storage_root, &known).await
}

/// `persistMarketplaceRefreshFailure`: attach the failure to an existing record.
pub async fn persist_marketplace_refresh_failure(
    storage_root: &Path,
    marketplace: &str,
    failure: Value,
) -> Result<(), ServiceError> {
    let mut known = load_known_marketplaces(storage_root).await?;
    let mut changed = false;
    for record in &mut known {
        if record.id == marketplace {
            record.last_refresh_failure = Some(failure.clone());
            changed = true;
        }
    }
    if changed {
        write_known_marketplaces(storage_root, &known).await?;
    }
    Ok(())
}

/// `removeMarketplace`: filter the record out; caches stay (TS behaviour).
pub async fn remove_known_marketplace(
    storage_root: &Path,
    marketplace: &str,
) -> Result<bool, ServiceError> {
    let known = load_known_marketplaces(storage_root).await?;
    let total = known.len();
    let kept: Vec<KnownMarketplace> = known
        .into_iter()
        .filter(|record| record.id != marketplace)
        .collect();
    let removed = kept.len() != total;
    write_known_marketplaces(storage_root, &kept).await?;
    Ok(removed)
}
