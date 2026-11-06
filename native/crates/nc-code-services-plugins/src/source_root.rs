//! Resolve a marketplace entry to a real plugin source directory — port of
//! `resolvePluginSourceRoot`, `resolveRepositoryPluginSource`, `resolveGitPluginSource`
//! and `clonePluginSource` in `adapters/src/plugins/marketplace.ts`.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use nc_code_service_core::ServiceError;
use serde_json::Value;

use crate::archive::{
    create_archive_fetch_error, resolve_github_archive_source, resolve_zip_plugin_source,
    should_fallback_github_archive_to_git, ResolvedArchive,
};
use crate::fetch::Fetcher;
use crate::fsutil::{is_directory, normalize_path, resolve_inside};
use crate::manifest::{MarketplaceEntry, MarketplaceManifest};
use crate::marketplace_store::{marketplace_manifest_path, plugin_cache_dir};
use crate::sources::{normalize_git_url, source_kind, source_str};
use crate::types::{read_source_identity_pin, DEFAULT_VERSION};

pub struct ResolvedSource {
    pub path: PathBuf,
    pub cleanup: Option<PathBuf>,
}

impl ResolvedSource {
    pub fn into_archive(self) -> ResolvedArchive {
        ResolvedArchive {
            path: self.path,
            cleanup: self.cleanup,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn resolve_plugin_source_root(
    fetcher: &Fetcher,
    storage_root: &Path,
    marketplace: &str,
    entry: &MarketplaceEntry,
    source_root_override: Option<&Path>,
    manifest_override: Option<&MarketplaceManifest>,
    cancelled: &AtomicBool,
) -> Result<ResolvedSource, ServiceError> {
    crate::fetch::check_cancelled(cancelled)?;
    let marketplace_dir = match source_root_override {
        Some(root) => root.to_path_buf(),
        None => marketplace_manifest_path(storage_root, marketplace)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| storage_root.to_path_buf()),
    };
    let loaded_manifest = match manifest_override {
        Some(manifest) => Some(manifest.clone()),
        None => {
            crate::marketplace_store::load_marketplace_manifest(storage_root, marketplace).await
        }
    };
    let plugin_base_dir = resolve_plugin_base_dir(&marketplace_dir, loaded_manifest.as_ref());
    let Some(source) = &entry.source else {
        let local_by_name = plugin_base_dir.join(&entry.name);
        if is_directory(&local_by_name) {
            return Ok(ResolvedSource {
                path: local_by_name,
                cleanup: None,
            });
        }
        return Err(ServiceError::new(format!(
            "Plugin source is not supported for {}@{}",
            entry.name, marketplace
        )));
    };

    if let Value::String(kind) = source {
        if kind == "filesystem" || kind == "sea" {
            if let Some(cache_path) = entry
                .cache_path
                .as_deref()
                .map(PathBuf::from)
                .filter(|path| is_directory(path))
            {
                return Ok(ResolvedSource {
                    path: cache_path,
                    cleanup: None,
                });
            }
            let computed = plugin_cache_dir(
                storage_root,
                marketplace,
                &entry.name,
                entry.version.as_deref().unwrap_or(DEFAULT_VERSION),
            );
            if is_directory(&computed) {
                return Ok(ResolvedSource {
                    path: computed,
                    cleanup: None,
                });
            }
            return Err(ServiceError::new(format!(
                "Bundled plugin cache directory missing: {}@{}",
                entry.name, marketplace
            )));
        }
        let local = resolve_inside(&plugin_base_dir, kind.trim_start_matches("./"))
            .filter(|path| is_directory(path));
        if let Some(local) = local {
            return Ok(ResolvedSource {
                path: local,
                cleanup: None,
            });
        }
        let fallback = normalize_path(&PathBuf::from(kind));
        if is_directory(&fallback) {
            return Ok(ResolvedSource {
                path: fallback,
                cleanup: None,
            });
        }
        return Err(ServiceError::new(format!(
            "Unsupported or missing plugin source: {kind}"
        )));
    }

    let Some(record) = source.as_object() else {
        let local_by_name = plugin_base_dir.join(&entry.name);
        if is_directory(&local_by_name) {
            return Ok(ResolvedSource {
                path: local_by_name,
                cleanup: None,
            });
        }
        return Err(ServiceError::new(format!(
            "Plugin source is not supported for {}@{}",
            entry.name, marketplace
        )));
    };
    let kind = source_kind(source).to_string();
    match kind.as_str() {
        "directory" => {
            let path = read_required_string(record.get("path"), "path", "directory path")?;
            let path = normalize_path(&PathBuf::from(path));
            if is_directory(&path) {
                Ok(ResolvedSource {
                    path,
                    cleanup: None,
                })
            } else {
                Err(ServiceError::new(format!(
                    "Plugin source directory does not exist: {}",
                    path.display()
                )))
            }
        }
        "github" => {
            let repo = read_required_string(record.get("repo"), "repo", "GitHub repo")?;
            let url = format!("https://github.com/{repo}.git");
            resolve_repository_plugin_source(
                fetcher,
                &url,
                source_str(source, "path"),
                source_str(source, "ref"),
                read_source_identity_pin(Some(source)).as_deref(),
                cancelled,
            )
            .await
        }
        "git" => {
            let url = read_required_string(record.get("url"), "url", "Git URL")?;
            resolve_repository_plugin_source(
                fetcher,
                &url,
                source_str(source, "path"),
                source_str(source, "ref"),
                read_source_identity_pin(Some(source)).as_deref(),
                cancelled,
            )
            .await
        }
        "url" => {
            let url = read_required_string(record.get("url"), "url", "URL")?;
            let source_type = source_str(source, "type").unwrap_or("");
            if source_type == "zip" {
                let sha256 = record
                    .get("sha256")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ServiceError::new("Plugin zip source sha256 is required"))?;
                let headers = read_source_headers(record.get("headers"))?;
                let path = read_optional_zip_path(record.get("path"))?;
                let strip_root = read_optional_strip_root(record.get("stripRoot"))?;
                return resolve_zip_plugin_source(
                    fetcher,
                    &url,
                    headers.as_ref(),
                    sha256,
                    path.as_deref(),
                    strip_root,
                    cancelled,
                )
                .await
                .map(ResolvedSource::from_archive);
            }
            if !source_type.is_empty() && source_type != "git" {
                return Err(unsupported_plugin_source(&format!("url:{source_type}")));
            }
            resolve_repository_plugin_source(
                fetcher,
                &url,
                source_str(source, "path"),
                source_str(source, "ref"),
                read_source_identity_pin(Some(source)).as_deref(),
                cancelled,
            )
            .await
        }
        "git-subdir" => {
            let path = read_required_string(record.get("path"), "path", "git-subdir path")?;
            let url = read_required_string(record.get("url"), "url", "git-subdir URL")?;
            resolve_repository_plugin_source(
                fetcher,
                &normalize_git_url(&url),
                Some(&path),
                source_str(source, "ref"),
                read_source_identity_pin(Some(source)).as_deref(),
                cancelled,
            )
            .await
        }
        "npm" | "pip" => Err(unsupported_plugin_source(&kind)),
        _ => Err(ServiceError::new(format!(
            "Plugin source is invalid or unsupported for {}@{}: {}",
            entry.name,
            marketplace,
            if kind.is_empty() {
                "missing kind"
            } else {
                &kind
            }
        ))),
    }
}

impl ResolvedSource {
    fn from_archive(archive: ResolvedArchive) -> Self {
        Self {
            path: archive.path,
            cleanup: archive.cleanup,
        }
    }
}

fn resolve_plugin_base_dir(
    marketplace_dir: &Path,
    manifest: Option<&MarketplaceManifest>,
) -> PathBuf {
    let Some(plugin_root) = manifest.and_then(|manifest| manifest.plugin_root.as_deref()) else {
        return marketplace_dir.to_path_buf();
    };
    resolve_inside(marketplace_dir, plugin_root)
        .filter(|resolved| is_directory(resolved))
        .unwrap_or_else(|| marketplace_dir.to_path_buf())
}

async fn resolve_repository_plugin_source(
    fetcher: &Fetcher,
    url: &str,
    path: Option<&str>,
    reference: Option<&str>,
    sha: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<ResolvedSource, ServiceError> {
    let pin = sha.or(reference);
    match resolve_github_archive_source(fetcher, url, pin, path, cancelled).await {
        Ok(archive) => Ok(ResolvedSource::from_archive(archive)),
        Err(error) => {
            if !should_fallback_github_archive_to_git(&error) {
                return Err(create_archive_fetch_error(url, &error));
            }
            resolve_git_plugin_source(fetcher, url, reference, sha, path, cancelled).await
        }
    }
}

async fn resolve_git_plugin_source(
    fetcher: &Fetcher,
    url: &str,
    reference: Option<&str>,
    sha: Option<&str>,
    path: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<ResolvedSource, ServiceError> {
    let temp = std::env::temp_dir().join(format!("zcode-plugin-src-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp)
        .map_err(|error| ServiceError::new(format!("create {}: {error}", temp.display())))?;
    if let Err(error) = fetcher
        .git_clone(url, reference, sha, &[], &temp, cancelled)
        .await
    {
        let _ = crate::archive::cleanup_temp_dir(Some(temp));
        return Err(error);
    }
    let Some(path) = path else {
        return Ok(ResolvedSource {
            path: temp.clone(),
            cleanup: Some(temp),
        });
    };
    crate::fetch::check_cancelled(cancelled)?;
    match resolve_inside(&temp, path.trim_start_matches("./")) {
        Some(subdir) if is_directory(&subdir) => Ok(ResolvedSource {
            path: subdir,
            cleanup: Some(temp),
        }),
        _ => {
            let _ = crate::archive::cleanup_temp_dir(Some(temp));
            Err(ServiceError::new(format!(
                "Plugin source subdirectory does not exist: {path}"
            )))
        }
    }
}

fn read_required_string(
    value: Option<&Value>,
    field: &str,
    label: &str,
) -> Result<String, ServiceError> {
    match value.and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => Ok(text.to_string()),
        _ => Err(ServiceError::new(format!(
            "Plugin {label} source requires a non-empty {field}"
        ))),
    }
}

fn read_source_headers(value: Option<&Value>) -> Result<Option<Value>, ServiceError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(record) = value.as_object() else {
        return Err(ServiceError::new(
            "Plugin zip source headers must be an object",
        ));
    };
    for (key, value) in record {
        if !value.is_string() {
            return Err(ServiceError::new(format!(
                "Plugin zip source header must be a string: {key}"
            )));
        }
    }
    Ok(Some(value.clone()))
}

fn read_optional_zip_path(value: Option<&Value>) -> Result<Option<String>, ServiceError> {
    match value {
        None => Ok(None),
        Some(Value::String(path)) => Ok(Some(path.clone())),
        Some(_) => Err(ServiceError::new("Plugin zip source path must be a string")),
    }
}

fn read_optional_strip_root(value: Option<&Value>) -> Result<Option<bool>, ServiceError> {
    match value {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ServiceError::new(
            "Plugin zip source stripRoot must be a boolean",
        )),
    }
}

fn unsupported_plugin_source(kind: &str) -> ServiceError {
    ServiceError::named(
        "UnsupportedPluginSourceError",
        format!("Plugin source is recognized but not supported in this runtime: {kind}"),
    )
}

/// `resolveInstalledPluginVersion`: manifest version, then entry version, then 0.0.0.
pub fn resolve_installed_plugin_version(root: &Path, entry_version: Option<&str>) -> String {
    crate::manifest::resolve_installed_plugin_version(root, entry_version)
}
