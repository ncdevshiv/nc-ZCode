//! `plugin-sync` channel — local plugin sync operations.
//!
//! Implements contract-aligned local operations: candidate discovery from
//! `~/.zcode/cli/config.json` (`plugins.dirs`/`plugins.enabledPlugins`),
//! remote status checks, and write-access probes. Archive import/export that
//! requires the TS `pluginSyncArchive` binary format is explicitly unsupported
//! (honest failure) rather than returning fabricated data.

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::fs;

const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 50 * 1024 * 1024;
const MANIFEST_RELATIVE_PATHS: &[&[&str]] = &[
    &[".zcode-plugin", "plugin.json"],
    &[".claude-plugin", "plugin.json"],
    &[".codex-plugin", "plugin.json"],
];

#[derive(Clone)]
pub struct PluginSyncService {
    paths: AppPaths,
}

impl PluginSyncService {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        Arc::new(Self { paths })
    }

    fn user_config_path(&self) -> PathBuf {
        self.paths
            .home_dir()
            .join(".zcode")
            .join("cli")
            .join("config.json")
    }

    fn user_plugin_root(&self) -> PathBuf {
        self.paths.home_dir().join(".zcode").join("plugins")
    }

    async fn read_json_object(path: &Path) -> Result<Option<Map<String, Value>>, ServiceError> {
        match fs::read_to_string(path).await {
            Ok(raw) => {
                let parsed: Value = serde_json::from_str(&raw)
                    .map_err(|e| ServiceError::new(format!("parse {}: {e}", path.display())))?;
                match parsed {
                    Value::Object(m) => Ok(Some(m)),
                    _ => Err(ServiceError::new(format!(
                        "{} must be a JSON object",
                        path.display()
                    ))),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ServiceError::new(format!("read {}: {e}", path.display()))),
        }
    }

    async fn read_plugin_config(&self) -> (Vec<String>, BTreeMap<String, bool>) {
        let Some(root) = Self::read_json_object(&self.user_config_path())
            .await
            .ok()
            .flatten()
        else {
            return (Vec::new(), BTreeMap::new());
        };
        let plugins = root
            .get("plugins")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let dirs = plugins
            .get("dirs")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let mut enabled = BTreeMap::new();
        if let Some(obj) = plugins.get("enabledPlugins").and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(b) = v.as_bool() {
                    enabled.insert(k.to_lowercase(), b);
                }
            }
        }
        (dirs, enabled)
    }

    fn candidate_id(plugin_id: &str, plugin_root: &str) -> String {
        let mut h = Sha256::new();
        h.update(format!("{plugin_id}:{plugin_root}").as_bytes());
        format!("{:x}", h.finalize())
    }

    async fn read_manifest(plugin_root: &Path) -> Option<Value> {
        for segs in MANIFEST_RELATIVE_PATHS {
            let mut p = plugin_root.to_path_buf();
            for s in *segs {
                p = p.join(s);
            }
            let Ok(raw) = fs::read_to_string(&p).await else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
                return None;
            };
            if parsed.is_object() {
                return Some(parsed);
            }
        }
        None
    }

    async fn path_exists(path: &Path) -> bool {
        fs::metadata(path).await.is_ok()
    }

    async fn component_types(plugin_root: &Path, manifest: &Value) -> Vec<Value> {
        let mut types = Vec::new();
        let obj = manifest.as_object();
        let has = |k: &str| obj.map(|o| o.contains_key(k)).unwrap_or(false);
        if has("skills") || Self::path_exists(&plugin_root.join("skills")).await {
            types.push(json!("skills"));
        }
        if has("commands") || Self::path_exists(&plugin_root.join("commands")).await {
            types.push(json!("commands"));
        }
        if has("hooks") || Self::path_exists(&plugin_root.join("hooks").join("hooks.json")).await {
            types.push(json!("hooks"));
        }
        if has("mcpServers") || Self::path_exists(&plugin_root.join(".mcp.json")).await {
            types.push(json!("mcp"));
        }
        types
    }

    async fn dir_size(path: &Path) -> u64 {
        let mut total = 0u64;
        let mut stack = vec![path.to_path_buf()];
        while let Some(d) = stack.pop() {
            let mut rd = match fs::read_dir(&d).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Ok(Some(e)) = rd.next_entry().await {
                let Ok(m) = e.metadata().await else { continue };
                if m.is_dir() {
                    stack.push(e.path());
                } else {
                    total += m.len();
                }
            }
        }
        total
    }

    async fn collect_candidates(&self) -> Result<Vec<Value>, ServiceError> {
        let (dirs, enabled_overrides) = self.read_plugin_config().await;
        let mut candidates = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut used_names = std::collections::HashSet::new();
        for raw_dir in dirs {
            let root = PathBuf::from(&raw_dir);
            let canonical = fs::canonicalize(&root).await.unwrap_or(root);
            let canon_str = canonical.to_string_lossy().to_string();
            if !seen.insert(canon_str.clone()) {
                continue;
            }
            let Some(manifest) = Self::read_manifest(&canonical).await else {
                continue;
            };
            let name = manifest
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if name.is_empty() {
                continue;
            }
            let plugin_id = format!("{name}@inline");
            let plugin_key = plugin_id.to_lowercase();
            let enabled_override = enabled_overrides.get(&plugin_key).copied();
            let base_dir_name = canonical
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&name)
                .to_string();
            let mut dir_name = if base_dir_name.is_empty() {
                name.clone()
            } else {
                base_dir_name
            };
            if !used_names.insert(dir_name.clone()) {
                let mut h = Sha256::new();
                h.update(canon_str.as_bytes());
                let suffix = &format!("{:x}", h.finalize())[..8];
                dir_name = format!("{dir_name}-{suffix}");
                used_names.insert(dir_name.clone());
            }
            let mut c = json!({
                "id": Self::candidate_id(&plugin_id, &canon_str),
                "name": name,
                "pluginId": plugin_id,
                "directoryName": dir_name,
                "path": canon_str,
                "sizeBytes": Self::dir_size(&canonical).await,
                "enabled": enabled_override.unwrap_or(true),
                "componentTypes": Self::component_types(&canonical, &manifest).await,
            });
            if let Some(d) = manifest
                .get("description")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
            {
                c["description"] = json!(d.trim());
            }
            if let Some(v) = manifest
                .get("version")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
            {
                c["version"] = json!(v.trim());
            }
            if let Some(eo) = enabled_override {
                c["enabledOverride"] = json!(eo);
            }
            candidates.push(c);
        }
        candidates.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });
        Ok(candidates)
    }

    async fn list_local(&self) -> Result<Value, ServiceError> {
        Ok(json!({
            "candidates": self.collect_candidates().await?,
            "maxArchiveBytes": DEFAULT_MAX_ARCHIVE_BYTES,
        }))
    }

    async fn list_remote_statuses(&self, args: &ServiceArgs) -> Result<Value, ServiceError> {
        let params = args.required_json(0, "params")?;
        let plugins = params
            .get("plugins")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let target_root = self.user_plugin_root();
        let statuses: Vec<Value> = plugins
            .into_iter()
            .map(|p| {
                let plugin_id = p.get("pluginId").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let dir_name = p.get("directoryName").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                if dir_name.is_empty() || dir_name.contains("..") || dir_name.contains('/') || dir_name.contains('\\') {
                    return json!({"pluginId": plugin_id, "directoryName": dir_name, "exists": false, "error": "invalid directory name"});
                }
                let target = target_root.join(&dir_name);
                if target.exists() {
                    json!({"pluginId": plugin_id, "directoryName": dir_name, "exists": true, "path": target.to_string_lossy(), "reason":"targetExists"})
                } else {
                    json!({"pluginId": plugin_id, "directoryName": dir_name, "exists": false})
                }
            })
            .collect();
        Ok(json!({ "statuses": statuses }))
    }

    async fn check_write_access(&self) -> Result<Value, ServiceError> {
        let dirs = [
            self.user_plugin_root(),
            self.user_config_path()
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.paths.home_dir().join(".zcode").join("cli")),
        ];
        for dir in dirs {
            fs::create_dir_all(&dir)
                .await
                .map_err(|e| ServiceError::new(format!("create {}: {e}", dir.display())))?;
            let probe = dir.join(format!(".zcode-plugin-probe-{}", std::process::id()));
            match fs::write(&probe, b"probe").await {
                Ok(()) => {
                    let _ = fs::remove_file(&probe).await;
                }
                Err(e) => {
                    return Ok(
                        json!({"writable": false, "path": dir.to_string_lossy(), "error": e.to_string()}),
                    )
                }
            }
        }
        Ok(json!({"writable": true, "path": self.user_plugin_root().to_string_lossy()}))
    }
}

#[async_trait]
impl Channel for PluginSyncService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        let result = match command {
            "listLocalUserPluginCandidates" => self.list_local().await,
            "listRemoteUserPluginStatuses" => self.list_remote_statuses(&args).await,
            "checkRemoteUserPluginWriteAccess" => self.check_write_access().await,
            "exportPluginsArchive" | "exportMarketplaceSourceArchive" => {
                return Err(ChannelFailure::error(
                    "plugin sync archive export is not supported in this backend",
                ));
            }
            "importPluginsArchive" | "importMarketplaceSourceArchive" => {
                return Err(ChannelFailure::error(
                    "plugin sync archive import is not supported in this backend",
                ));
            }
            other => return Err(ChannelFailure::method_not_found(other)),
        };
        result.map(WireValue::Json).map_err(ChannelFailure::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use tempfile::TempDir;

    fn ctx() -> ChannelContext {
        ChannelContext::new("plugin-sync", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn discovers_configured_inline_plugin() {
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::new(tmp.path().join("home"), tmp.path().join("data"));
        let plugin_dir = tmp.path().join("plug1");
        fs::create_dir_all(plugin_dir.join(".zcode-plugin"))
            .await
            .unwrap();
        fs::write(
            plugin_dir.join(".zcode-plugin/plugin.json"),
            r#"{"name":"demo","description":"d","version":"1.0.0"}"#,
        )
        .await
        .unwrap();
        let cfg_dir = paths.home_dir().join(".zcode").join("cli");
        fs::create_dir_all(&cfg_dir).await.unwrap();
        fs::write(cfg_dir.join("config.json"), serde_json::to_string(&json!({
            "plugins": {"dirs":[plugin_dir.to_string_lossy()], "enabledPlugins": {"demo@inline": false}}
        })).unwrap()).await.unwrap();
        let svc = PluginSyncService::new(paths);
        let v = svc.list_local().await.unwrap();
        let c = v["candidates"].as_array().unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["pluginId"], "demo@inline");
        assert_eq!(c[0]["enabled"], false);
    }

    #[tokio::test]
    async fn archive_ops_fail_honestly() {
        let tmp = TempDir::new().unwrap();
        let svc = PluginSyncService::new(AppPaths::new(
            tmp.path().join("home"),
            tmp.path().join("data"),
        ));
        let err = svc
            .call(
                &ctx(),
                "exportPluginsArchive",
                vec![WireValue::Json(json!({"pluginIds":[]}))],
                CancelToken::new(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not supported"));
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use tempfile::TempDir;

    #[tokio::test]
    async fn remote_status_and_write_access() {
        let tmp = TempDir::new().unwrap();
        let svc = PluginSyncService::new(AppPaths::new(
            tmp.path().join("home"),
            tmp.path().join("data"),
        ));
        let ctx = ChannelContext::new("plugin-sync", ClientMode::DesktopContinuous);
        let r = svc
            .call(
                &ctx,
                "listRemoteUserPluginStatuses",
                vec![WireValue::Json(
                    json!({"plugins":[{"pluginId":"x","directoryName":"x"}]}),
                )],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else { panic!() };
        assert_eq!(v["statuses"][0]["exists"], false);
        let r = svc
            .call(
                &ctx,
                "checkRemoteUserPluginWriteAccess",
                vec![],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = r else { panic!() };
        assert!(v["writable"].as_bool().unwrap());
    }
}
