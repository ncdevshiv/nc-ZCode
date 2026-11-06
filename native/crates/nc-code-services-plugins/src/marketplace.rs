//! Marketplace persistence and refresh — port of the marketplace half of
//! `adapters/src/plugins/marketplace.ts` plus the orchestration guards in
//! `bootstrap/src/plugins.ts`.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use nc_code_service_core::ServiceError;
use serde_json::{Map, Value};

use crate::archive::cleanup_temp_dir;
use crate::diagnostics::Diagnostic;
use crate::fetch::Fetcher;
use crate::fsutil::{
    create_staging_directory, is_directory, is_file, now_iso, read_json_if_exists, resolve_inside,
    write_json_atomic, DirectorySwap,
};
use crate::manifest::{
    find_marketplace_manifest_file, parse_required_marketplace_manifest, MarketplaceManifest,
};
use crate::marketplace_store::{
    load_known_marketplaces, load_marketplace_manifest, marketplace_dir, marketplace_manifest_path,
    persist_marketplace_refresh_failure, remove_known_marketplace, write_known_marketplaces,
};
use crate::sources::{source_kind, source_str};
use crate::types::{KnownMarketplace, MARKETPLACE_FILE, OFFICIAL_MARKETPLACE_ID};

pub struct LoadedMarketplace {
    pub manifest: MarketplaceManifest,
    pub source_root: Option<PathBuf>,
    pub cleanup: Option<PathBuf>,
}

fn check(cancelled: &AtomicBool) -> Result<(), ServiceError> {
    crate::fetch::check_cancelled(cancelled)
}

/// `findMarketplaceManifestPath`: explicit path (contained), then the two conventions.
pub fn find_marketplace_manifest_path(root: &Path, explicit: Option<&str>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = explicit {
        if let Some(resolved) = resolve_inside(root, explicit.trim_start_matches("./")) {
            candidates.push(resolved);
        }
    }
    candidates.push(root.join(".claude-plugin").join("marketplace.json"));
    candidates.push(root.join("marketplace.json"));
    candidates.into_iter().find(|candidate| is_file(candidate))
}

/// `loadMarketplaceFromSource` (`persist: false`): never writes storage here; `add_marketplace`
/// stages the source tree after the trust guards pass. `storage_root` is only needed for the
/// persist half of the TypeScript helper, so it stays out of this signature.
pub async fn load_marketplace_from_source(
    fetcher: &Fetcher,
    source: &Value,
    cancelled: &AtomicBool,
) -> Result<LoadedMarketplace, ServiceError> {
    check(cancelled)?;
    match source_kind(source) {
        "settings" => {
            let manifest = source
                .get("marketplace")
                .cloned()
                .ok_or_else(|| ServiceError::new("Marketplace manifest is invalid"))?;
            let parsed =
                parse_required_marketplace_manifest(&manifest).map_err(ServiceError::new)?;
            Ok(LoadedMarketplace {
                manifest: parsed,
                source_root: None,
                cleanup: None,
            })
        }
        "file" => {
            let path = source_str(source, "path")
                .ok_or_else(|| ServiceError::new("Marketplace source path is required"))?;
            let parsed = read_marketplace_json(Path::new(path)).await?;
            Ok(LoadedMarketplace {
                manifest: parse_required_marketplace_manifest(&parsed)
                    .map_err(ServiceError::new)?,
                source_root: Path::new(path).parent().map(Path::to_path_buf),
                cleanup: None,
            })
        }
        "directory" => {
            let path = source_str(source, "path")
                .ok_or_else(|| ServiceError::new("Marketplace source path is required"))?;
            let root = PathBuf::from(path);
            let Some(manifest_path) = find_marketplace_manifest_path(&root, None) else {
                return Err(ServiceError::new(format!(
                    "Marketplace manifest not found in directory: {path}"
                )));
            };
            let parsed = read_marketplace_json(&manifest_path).await?;
            Ok(LoadedMarketplace {
                manifest: parse_required_marketplace_manifest(&parsed)
                    .map_err(ServiceError::new)?,
                source_root: Some(root),
                cleanup: None,
            })
        }
        "url" => {
            let url = source_str(source, "url")
                .ok_or_else(|| ServiceError::new("Marketplace URL is required"))?;
            let headers = source.get("headers").cloned();
            let parsed = fetcher
                .get_marketplace_json(url, headers.as_ref(), cancelled)
                .await?;
            Ok(LoadedMarketplace {
                manifest: parse_required_marketplace_manifest(&parsed)
                    .map_err(ServiceError::new)?,
                source_root: None,
                cleanup: None,
            })
        }
        "github" | "git" => {
            let url = if source_kind(source) == "github" {
                let repo = source_str(source, "repo")
                    .ok_or_else(|| ServiceError::new("GitHub repo is required"))?;
                format!("https://github.com/{repo}.git")
            } else {
                source_str(source, "url")
                    .ok_or_else(|| ServiceError::new("Git URL is required"))?
                    .to_string()
            };
            let reference = source_str(source, "ref");
            let sparse_paths = crate::sources::source_string_list(source, "sparsePaths");
            let resolved = resolve_repository_marketplace_source(
                fetcher,
                &url,
                reference,
                &sparse_paths,
                cancelled,
            )
            .await?;
            let Some(manifest_path) =
                find_marketplace_manifest_path(&resolved.path, source_str(source, "path"))
            else {
                let _ = cleanup_temp_dir(resolved.cleanup);
                return Err(ServiceError::new(format!(
                    "Marketplace manifest not found in {} repo: {url}",
                    if source_kind(source) == "github" {
                        "GitHub"
                    } else {
                        "git"
                    }
                )));
            };
            let parsed = match read_marketplace_json(&manifest_path).await {
                Ok(parsed) => parsed,
                Err(error) => {
                    let _ = cleanup_temp_dir(resolved.cleanup);
                    return Err(error);
                }
            };
            let manifest = match parse_required_marketplace_manifest(&parsed) {
                Ok(manifest) => manifest,
                Err(error) => {
                    let _ = cleanup_temp_dir(resolved.cleanup);
                    return Err(ServiceError::new(error));
                }
            };
            Ok(LoadedMarketplace {
                manifest,
                source_root: Some(resolved.path),
                cleanup: resolved.cleanup,
            })
        }
        "npm" | "hostPattern" | "pathPattern" => {
            Err(unsupported_marketplace_source(source_kind(source)))
        }
        other => Err(ServiceError::new(format!(
            "Unsupported marketplace source: {other}"
        ))),
    }
}

async fn read_marketplace_json(path: &Path) -> Result<Value, ServiceError> {
    let Some(value) = read_json_if_exists(path).await? else {
        return Err(ServiceError::new(format!(
            "Marketplace manifest not found: {}",
            path.display()
        )));
    };
    Ok(value)
}

struct RepositorySource {
    path: PathBuf,
    cleanup: Option<PathBuf>,
}

async fn resolve_repository_marketplace_source(
    fetcher: &Fetcher,
    url: &str,
    reference: Option<&str>,
    sparse_paths: &[String],
    cancelled: &AtomicBool,
) -> Result<RepositorySource, ServiceError> {
    if sparse_paths.is_empty() {
        match crate::archive::resolve_github_archive_source(
            fetcher, url, reference, None, cancelled,
        )
        .await
        {
            Ok(archive) => {
                return Ok(RepositorySource {
                    path: archive.path,
                    cleanup: archive.cleanup,
                })
            }
            Err(error) => {
                if !crate::archive::should_fallback_github_archive_to_git(&error) {
                    return Err(crate::archive::create_archive_fetch_error(url, &error));
                }
            }
        }
    }
    let temp = std::env::temp_dir().join(format!("zcode-marketplace-src-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp)
        .map_err(|error| ServiceError::new(format!("create {}: {error}", temp.display())))?;
    if let Err(error) = fetcher
        .git_clone(url, reference, None, sparse_paths, &temp, cancelled)
        .await
    {
        let _ = cleanup_temp_dir(Some(temp));
        return Err(error);
    }
    Ok(RepositorySource {
        path: temp.clone(),
        cleanup: Some(temp),
    })
}

/// `addMarketplace`: trust guards → staging → authority upsert → finalize.
pub async fn add_marketplace(
    fetcher: &Fetcher,
    storage_root: &Path,
    source: &Value,
    expected_id: Option<&str>,
    trusted_id: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<KnownMarketplace, ServiceError> {
    check(cancelled)?;
    let loaded = load_marketplace_from_source(fetcher, source, cancelled).await?;
    let result = add_loaded_marketplace(
        storage_root,
        source,
        loaded,
        expected_id,
        trusted_id,
        cancelled,
    )
    .await;
    result
}

async fn add_loaded_marketplace(
    storage_root: &Path,
    source: &Value,
    loaded: LoadedMarketplace,
    expected_id: Option<&str>,
    trusted_id: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<KnownMarketplace, ServiceError> {
    let cleanup = loaded.cleanup.clone();
    let outcome = async {
        check(cancelled)?;
        let manifest_name = loaded.manifest.name.clone();
        if crate::types::is_official_marketplace_id(&manifest_name)
            && Some(manifest_name.as_str()) != trusted_id
        {
            return Err(ServiceError::new(format!(
                "Cannot add a marketplace named \"{manifest_name}\": that id is reserved for the official marketplace."
            )));
        }
        if let Some(expected_id) = expected_id {
            if manifest_name != expected_id {
                return Err(ServiceError::new(format!(
                    "Marketplace declaration id mismatch: expected {expected_id}, received {manifest_name}"
                )));
            }
        }
        if trusted_id == Some(OFFICIAL_MARKETPLACE_ID) && manifest_name != OFFICIAL_MARKETPLACE_ID {
            return Err(ServiceError::new(format!(
                "Official marketplace source must provide {OFFICIAL_MARKETPLACE_ID}, received {manifest_name}"
            )));
        }
        let persisted = if manifest_name == OFFICIAL_MARKETPLACE_ID {
            let merged = crate::official::write_cdn_official_marketplace_partition(
                storage_root,
                &loaded.manifest.raw,
            )
            .await?;
            parse_required_marketplace_manifest(&merged).map_err(ServiceError::new)?
        } else {
            loaded.manifest.clone()
        };

        let mut swap: Option<DirectorySwap> = if let Some(source_root) = &loaded.source_root {
            stage_marketplace_directory(
                source_root,
                storage_root,
                &manifest_name,
                &persisted.raw,
            )?
        } else if manifest_name != OFFICIAL_MARKETPLACE_ID {
            stage_marketplace_manifest(storage_root, &manifest_name, &persisted.raw)?
        } else {
            None
        };
        if check(cancelled).is_err() {
            if let Some(swap) = swap.as_mut() {
                let _ = swap.rollback();
            }
            return Err(cancelled_error());
        }
        let now = now_iso();
        let record = KnownMarketplace {
            id: manifest_name.clone(),
            source: source.clone(),
            name: manifest_name.clone(),
            description: persisted.description.clone(),
            added_at: now.clone(),
            last_updated: Some(now),
            plugin_count: persisted.plugins.len() as u64,
            cache_transaction_id: swap.as_ref().map(|_| uuid::Uuid::new_v4().to_string()),
            last_refresh_failure: None,
        };
        if let Err(error) =
            crate::marketplace_store::upsert_known_marketplace(storage_root, &record).await
        {
            if let Some(swap) = swap.as_mut() {
                let _ = swap.rollback();
            }
            return Err(error);
        }
        if let Some(swap) = swap.as_mut() {
            swap.commit();
        }
        Ok::<KnownMarketplace, ServiceError>(record)
    }
    .await;
    let cleanup_error = cleanup_temp_dir(cleanup);
    match outcome {
        Ok(record) => Ok(record),
        Err(error) => Err(crate::archive::append_cleanup_error(error, cleanup_error)),
    }
}

fn stage_marketplace_directory(
    source_root: &Path,
    storage_root: &Path,
    marketplace: &str,
    manifest: &Value,
) -> Result<Option<DirectorySwap>, ServiceError> {
    let target = marketplace_dir(storage_root, marketplace);
    let staging = create_staging_directory(&target, Some(source_root))?;
    write_manifest_sync(&staging.join(MARKETPLACE_FILE), manifest)?;
    DirectorySwap::activate(&staging, &target).map(Some)
}

fn stage_marketplace_manifest(
    storage_root: &Path,
    marketplace: &str,
    manifest: &Value,
) -> Result<Option<DirectorySwap>, ServiceError> {
    let target = marketplace_dir(storage_root, marketplace);
    let staging = create_staging_directory(&target, None)?;
    write_manifest_sync(&staging.join(MARKETPLACE_FILE), manifest)?;
    DirectorySwap::activate(&staging, &target).map(Some)
}

fn write_manifest_sync(path: &Path, value: &Value) -> Result<(), ServiceError> {
    let text = format!("{}\n", serde_json::to_string_pretty(value)?);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ServiceError::new(format!("create {}: {error}", parent.display())))?;
    }
    std::fs::write(path, text)
        .map_err(|error| ServiceError::new(format!("write {}: {error}", path.display())))
}

pub fn cancelled_error() -> ServiceError {
    ServiceError::named("AbortError", "Plugin operation cancelled")
}

/// `ensureMarketplaceManifestAvailable`: cached manifest, else refresh the known record.
pub async fn ensure_marketplace_manifest_available(
    fetcher: &Fetcher,
    storage_root: &Path,
    marketplace: &str,
    cancelled: &AtomicBool,
) -> Result<Option<KnownMarketplace>, ServiceError> {
    check(cancelled)?;
    if load_marketplace_manifest(storage_root, marketplace)
        .await
        .is_some()
    {
        return Ok(load_known_marketplaces(storage_root)
            .await?
            .into_iter()
            .find(|record| record.id == marketplace));
    }
    let Some(record) = load_known_marketplaces(storage_root)
        .await?
        .into_iter()
        .find(|record| record.id == marketplace)
    else {
        return Ok(None);
    };
    let refreshed = add_marketplace(
        fetcher,
        storage_root,
        &record.source,
        None,
        Some(&record.id),
        cancelled,
    )
    .await?;
    Ok(Some(refreshed))
}

/// `updateMarketplace`: refresh known records; failures are persisted on the record.
pub async fn update_marketplace(
    fetcher: &Fetcher,
    storage_root: &Path,
    marketplace: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<Vec<KnownMarketplace>, ServiceError> {
    let known = load_known_marketplaces(storage_root).await?;
    let selected: Vec<KnownMarketplace> = match marketplace {
        Some(id) => known.into_iter().filter(|record| record.id == id).collect(),
        None => known,
    };
    if let Some(id) = marketplace {
        if selected.is_empty() {
            return Err(ServiceError::new(format!("Marketplace not found: {id}")));
        }
    }
    let mut updated = Vec::new();
    for record in selected {
        check(cancelled)?;
        match add_marketplace(
            fetcher,
            storage_root,
            &record.source,
            None,
            Some(&record.id),
            cancelled,
        )
        .await
        {
            Ok(record) => updated.push(record),
            Err(error) => {
                if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(error);
                }
                let diagnostic =
                    crate::validate::to_validation_diagnostic(&error, Some(&record.id));
                persist_marketplace_refresh_failure(
                    storage_root,
                    &record.id,
                    serde_json::json!({
                        "code": diagnostic.code,
                        "failedAt": now_iso(),
                        "message": diagnostic.message,
                    }),
                )
                .await?;
            }
        }
    }
    Ok(updated)
}

/// `removeMarketplace`: known record only; cached catalogs stay.
pub async fn remove_marketplace(
    storage_root: &Path,
    marketplace: &str,
) -> Result<(), ServiceError> {
    remove_known_marketplace(storage_root, marketplace).await?;
    Ok(())
}

/// Marketplace summary projection (`toMarketplaceSummaryData` in bootstrap).
pub fn marketplace_summary(record: &KnownMarketplace, featured: Option<Vec<String>>) -> Value {
    record.summary_projection(featured, None)
}

pub fn manifest_plugin_count(manifest: &MarketplaceManifest) -> u64 {
    manifest.plugins.len() as u64
}

pub fn marketplace_manifest_file(root: &Path) -> Option<PathBuf> {
    find_marketplace_manifest_file(root, None)
}

/// Diagnostics helper for marketplace declarations that clash with the reserved id.
pub fn reserved_marketplace_declaration_diagnostic(marketplace_id: &str) -> Diagnostic {
    Diagnostic::warning(
        "plugin_marketplace_declaration_reserved",
        format!(
            "Workspace marketplace declaration \"{marketplace_id}\" uses a reserved official id and was ignored. Use a different marketplace id for project declarations."
        ),
    )
    .with_plugin_id(marketplace_id)
}

pub fn marketplace_source_repoint_diagnostic(marketplace_id: &str) -> Diagnostic {
    Diagnostic::error(
        "plugin_marketplace_invalid",
        format!(
            "Workspace marketplace declaration \"{marketplace_id}\" conflicts with an existing Host source. Remove the existing marketplace or use a different marketplace id before materializing it."
        ),
    )
    .with_plugin_id(marketplace_id)
}

pub fn declared_marketplace_record(marketplace_id: &str, source: Value) -> KnownMarketplace {
    KnownMarketplace {
        id: marketplace_id.to_string(),
        source,
        name: marketplace_id.to_string(),
        description: None,
        added_at: String::new(),
        last_updated: None,
        plugin_count: 0,
        cache_transaction_id: None,
        last_refresh_failure: None,
    }
}

pub fn unsupported_marketplace_source(kind: &str) -> ServiceError {
    ServiceError::named(
        "UnsupportedMarketplaceSourceError",
        format!("Marketplace source is recognized but not supported in this runtime: {kind}"),
    )
}

/// Write a merged official marketplace manifest (`writeCdnOfficialMarketplacePartitionSync`).
pub async fn write_official_partition(
    storage_root: &Path,
    manifest: &Value,
) -> Result<Value, ServiceError> {
    crate::official::write_cdn_official_marketplace_partition(storage_root, manifest).await
}

pub async fn write_json(path: &Path, value: &Value) -> Result<(), ServiceError> {
    write_json_atomic(path, value).await
}

pub fn default_entry_map() -> Map<String, Value> {
    Map::new()
}

pub fn is_known_marketplace_dir(storage_root: &Path, marketplace: &str) -> bool {
    is_directory(&marketplace_dir(storage_root, marketplace))
}

pub fn manifest_cache_path(storage_root: &Path, marketplace: &str) -> PathBuf {
    marketplace_manifest_path(storage_root, marketplace)
}

/// `loadKnownMarketplaces` re-export for orchestration modules.
pub async fn known_marketplaces(
    storage_root: &Path,
) -> Result<Vec<KnownMarketplace>, ServiceError> {
    load_known_marketplaces(storage_root).await
}

pub async fn write_known(
    storage_root: &Path,
    records: &[KnownMarketplace],
) -> Result<(), ServiceError> {
    write_known_marketplaces(storage_root, records).await
}
