//! Official marketplace partitions and official plugin cache roots — port of
//! `adapters/src/plugins/official-marketplace.ts` plus the official-cache scan in
//! `adapters/src/plugins/index.ts`.
//!
//! The TypeScript product seeds bundled official plugins from application assets
//! (`app/bundled-plugins.ts`). The Rust host has no bundled assets: when a
//! `bundled-marketplace.json` partition already exists (written by an earlier TS run
//! against the same storage root) it is honoured; otherwise only the official cache
//! written by explicit user actions exists.

use std::path::{Path, PathBuf};

use nc_code_service_core::ServiceError;
use serde_json::{Map, Value};

use crate::fsutil::{
    is_directory, is_file, normalize_path, read_json_if_exists, write_json_atomic,
};
use crate::types::OFFICIAL_MARKETPLACE_ID;

const BUNDLED_PARTITION_FILE: &str = "bundled-marketplace.json";
const CDN_PARTITION_FILE: &str = "cdn-marketplace.json";
const MERGED_MARKETPLACE_FILE: &str = "marketplace.json";

pub const CUA_PLUGIN_ID: &str = "computer-use@zcode-plugins-official";

fn partition_path(storage_root: &Path, file_name: &str) -> PathBuf {
    storage_root
        .join("marketplaces")
        .join(OFFICIAL_MARKETPLACE_ID)
        .join(file_name)
}

fn assert_official_manifest(manifest: &Value) -> Result<(), ServiceError> {
    let name = manifest.get("name").and_then(Value::as_str).unwrap_or("");
    if name != OFFICIAL_MARKETPLACE_ID {
        return Err(ServiceError::new(format!(
            "Official marketplace manifest must be named {OFFICIAL_MARKETPLACE_ID}"
        )));
    }
    Ok(())
}

/// `writeCdnOfficialMarketplacePartitionSync`: persist the refreshable CDN partition
/// and rebuild the merged `marketplace.json`.
pub async fn write_cdn_official_marketplace_partition(
    storage_root: &Path,
    manifest: &Value,
) -> Result<Value, ServiceError> {
    assert_official_manifest(manifest)?;
    write_json_atomic(&partition_path(storage_root, CDN_PARTITION_FILE), manifest).await?;
    rebuild_official_marketplace(storage_root).await
}

async fn rebuild_official_marketplace(storage_root: &Path) -> Result<Value, ServiceError> {
    let bundled_manifest = read_bundled_partition(storage_root).await;
    let cdn_manifest = read_json_if_exists(&partition_path(storage_root, CDN_PARTITION_FILE))
        .await?
        .filter(Value::is_object);
    let cdn_plugins = read_plugin_entries(cdn_manifest.as_ref());
    let cdn_names: Vec<String> = cdn_plugins.iter().filter_map(read_plugin_name).collect();
    let bundled_plugins: Vec<Value> = read_plugin_entries(bundled_manifest.as_ref())
        .into_iter()
        .filter(|plugin| read_plugin_name(plugin).is_some_and(|name| !cdn_names.contains(&name)))
        .collect();

    let mut merged = Map::new();
    if let Some(Value::Object(bundled)) = &bundled_manifest {
        for (key, value) in bundled {
            merged.insert(key.clone(), value.clone());
        }
    }
    if let Some(Value::Object(cdn)) = &cdn_manifest {
        for (key, value) in cdn {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged.insert(
        "name".into(),
        Value::String(OFFICIAL_MARKETPLACE_ID.to_string()),
    );
    let mut plugins = cdn_plugins;
    plugins.extend(bundled_plugins);
    merged.insert("plugins".into(), Value::Array(plugins));
    let merged = Value::Object(merged);
    write_json_atomic(
        &partition_path(storage_root, MERGED_MARKETPLACE_FILE),
        &merged,
    )
    .await?;
    Ok(merged)
}

async fn read_bundled_partition(storage_root: &Path) -> Option<Value> {
    let value = read_json_if_exists(&partition_path(storage_root, BUNDLED_PARTITION_FILE))
        .await
        .ok()??;
    if value.get("version").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    value.get("manifest").filter(|m| m.is_object()).cloned()
}

fn read_plugin_entries(manifest: Option<&Value>) -> Vec<Value> {
    manifest
        .and_then(|manifest| manifest.get("plugins"))
        .and_then(Value::as_array)
        .map(|items| items.to_vec())
        .unwrap_or_default()
}

fn read_plugin_name(plugin: &Value) -> Option<String> {
    plugin
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn is_strict_descendant(parent: &Path, child: &Path) -> bool {
    match child.strip_prefix(parent) {
        Ok(relative) => !relative.as_os_str().is_empty(),
        Err(_) => false,
    }
}

/// `loadBundledOfficialPluginRootsSync`: `None` when no bundled partition exists,
/// otherwise the cache paths that live strictly under
/// `cache/zcode-plugins-official/<name>/`.
pub async fn load_bundled_official_plugin_roots(storage_root: &Path) -> Option<Vec<PathBuf>> {
    let manifest = read_bundled_partition(storage_root).await?;
    let official_cache_root =
        normalize_path(&storage_root.join("cache").join(OFFICIAL_MARKETPLACE_ID));
    let mut roots = Vec::new();
    for plugin in read_plugin_entries(Some(&manifest)) {
        let Some(name) = read_plugin_name(&plugin) else {
            continue;
        };
        let Some(cache_path) = plugin.get("cachePath").and_then(Value::as_str) else {
            continue;
        };
        let plugin_cache_root = official_cache_root.join(&name);
        let resolved = normalize_path(&PathBuf::from(cache_path));
        if !is_strict_descendant(&official_cache_root, &plugin_cache_root)
            || !is_strict_descendant(&plugin_cache_root, &resolved)
        {
            continue;
        }
        roots.push(resolved);
    }
    Some(roots)
}

/// `scanOfficialCache` fallback in `index.ts`: every
/// `cache/zcode-plugins-official/<plugin>/<version>/` directory.
pub async fn scan_official_cache_roots(storage_root: &Path) -> Vec<PathBuf> {
    if let Some(roots) = load_bundled_official_plugin_roots(storage_root).await {
        return roots;
    }
    let cache_root = storage_root.join("cache").join(OFFICIAL_MARKETPLACE_ID);
    let Ok(mut plugins) = tokio::fs::read_dir(&cache_root).await else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    while let Ok(Some(entry)) = plugins.next_entry().await {
        if entry
            .file_type()
            .await
            .map(|kind| kind.is_dir())
            .unwrap_or(false)
        {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    let mut roots = Vec::new();
    for name in names {
        let plugin_dir = cache_root.join(&name);
        let Ok(mut versions) = tokio::fs::read_dir(&plugin_dir).await else {
            continue;
        };
        let mut version_names: Vec<String> = Vec::new();
        while let Ok(Some(entry)) = versions.next_entry().await {
            if entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false)
            {
                version_names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        version_names.sort();
        for version in version_names {
            roots.push(plugin_dir.join(version));
        }
    }
    roots
}

/// `isZCodeCuaInternalFeatureEnabled`: on by default; `nc_code_CUA_PRODUCT_HELPER`
/// set to 0/false/off turns it off, `nc_code_CUA_DEV_MODE` turns it on.
pub fn is_cua_internal_feature_enabled() -> bool {
    let read = |key: &str| {
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_ascii_lowercase())
    };
    if matches!(
        read("ZCODE_CUA_DEV_MODE").as_deref(),
        Some("1" | "true" | "on")
    ) {
        return true;
    }
    !matches!(
        read("ZCODE_CUA_PRODUCT_HELPER").as_deref(),
        Some("0" | "false" | "off")
    )
}

/// `<storage>/cache/zcode-plugins-official/<name>/<version>` (TS `officialPluginCacheRoot`).
pub fn official_plugin_cache_root(storage_root: &Path, name: &str, version: &str) -> PathBuf {
    storage_root
        .join("cache")
        .join(OFFICIAL_MARKETPLACE_ID)
        .join(crate::types::sanitize_plugin_id(name))
        .join(crate::types::sanitize_plugin_id(version))
}

/// `isSeedUsable`: a cache root counts when its manifest names the plugin.
pub fn official_cache_root_is_usable(root: &Path, name: &str) -> bool {
    let manifest_path = root.join(".zcode-plugin/plugin.json");
    is_file(&manifest_path)
        && std::fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| {
                value
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .is_some_and(|manifest_name| manifest_name == name)
}

pub fn official_cache_root_exists(path: &Path) -> bool {
    is_directory(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zcode-plugin-official-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        root
    }

    #[tokio::test]
    async fn cdn_partition_merges_with_bundled_partition() {
        let root = temp_root("merge");
        let bundled = serde_json::json!({
            "name": OFFICIAL_MARKETPLACE_ID,
            "plugins": [
                {"name": "browser-use", "version": "0.1.0"},
                {"name": "shared", "version": "1.0.0"}
            ]
        });
        write_json_atomic(
            &partition_path(&root, BUNDLED_PARTITION_FILE),
            &serde_json::json!({"manifest": bundled, "version": 1}),
        )
        .await
        .expect("bundled partition");
        let cdn = serde_json::json!({
            "name": OFFICIAL_MARKETPLACE_ID,
            "plugins": [{"name": "shared", "version": "2.0.0"}]
        });
        let merged = write_cdn_official_marketplace_partition(&root, &cdn)
            .await
            .expect("merged");
        let names: Vec<&str> = merged["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|plugin| plugin["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["shared", "browser-use"]);
        assert_eq!(merged["plugins"][0]["version"], serde_json::json!("2.0.0"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cua_feature_flag_defaults_on() {
        //Only verify the default branch; environment variable changes are not safe in this test process, and the specific switches are guaranteed by code reading.
        let value = std::env::var("ZCODE_CUA_PRODUCT_HELPER").ok();
        if value.is_none() {
            assert!(is_cua_internal_feature_enabled());
        }
    }
}
