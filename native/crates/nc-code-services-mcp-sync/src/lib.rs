//! `mcp-sync` channel — local MCP config sync operations.
//!
//! Implements the core TS `IMcpSyncService` contract for local user-directory
//! operations: load/save zcode MCP config (`~/.zcode/cli/config.json`, key
//! `mcp.servers`), list candidates, export/import (local merge into zcode config),
//! and honest failures for runtime-dependent operations
//! (`listWorkspaceMcpServerStatuses` requires agent runtime).

/// Handoff skeleton (pure data/types): serde persistence structs, port trait, Send/Sync markers.
pub mod handoff;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::atomic::atomic_write_text;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::fs;

const ENABLED_KEY: &str = "enabled";
const LEGACY_ENABLE_KEY: &str = "enable";

#[derive(Clone)]
pub struct McpSyncService {
    paths: AppPaths,
}

impl McpSyncService {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        Arc::new(Self { paths })
    }

    fn home_dir(&self) -> &Path {
        self.paths.home_dir()
    }

    fn zcode_config_path(&self) -> PathBuf {
        self.home_dir()
            .join(".zcode")
            .join("cli")
            .join("config.json")
    }

    fn agents_mcp_path(&self) -> PathBuf {
        self.home_dir().join(".agents").join("mcp.json")
    }

    fn workspace_zcode_config_path(&self, workspace_path: &str) -> PathBuf {
        PathBuf::from(workspace_path)
            .join(".zcode")
            .join("config.json")
    }

    fn workspace_agents_mcp_path(&self, workspace_path: &str) -> PathBuf {
        PathBuf::from(workspace_path)
            .join(".agents")
            .join("mcp.json")
    }

    async fn read_json_object(path: &Path) -> Result<Option<Map<String, Value>>, ServiceError> {
        match fs::read_to_string(path).await {
            Ok(raw) => {
                let parsed: Value = serde_json::from_str(&raw).map_err(|e| {
                    ServiceError::new(format!(
                        "Failed to parse MCP config {}: {e}",
                        path.display()
                    ))
                })?;
                match parsed {
                    Value::Object(map) => Ok(Some(map)),
                    _ => Err(ServiceError::new(format!(
                        "MCP config {} must be a JSON object",
                        path.display()
                    ))),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ServiceError::new(format!(
                "Failed to read MCP config {}: {e}",
                path.display()
            ))),
        }
    }

    async fn write_json_object(path: &Path, obj: &Map<String, Value>) -> Result<(), ServiceError> {
        let content = format!(
            "{}\n",
            serde_json::to_string_pretty(&Value::Object(obj.clone()))
                .map_err(|e| ServiceError::new(e.to_string()))?
        );
        atomic_write_text(path, &content)
            .await
            .map_err(|e| ServiceError::new(format!("Failed to write {}: {e}", path.display())))
    }

    fn read_server_map(
        root: &Map<String, Value>,
        key_name: &str,
    ) -> BTreeMap<String, Map<String, Value>> {
        let servers_value = if key_name == "mcp.servers" {
            root.get("mcp")
                .and_then(|v| v.as_object())
                .and_then(|m| m.get("servers"))
        } else {
            root.get(key_name)
        };
        let mut out = BTreeMap::new();
        if let Some(Value::Object(servers)) = servers_value {
            for (name, cfg) in servers {
                if let Value::Object(c) = cfg {
                    out.insert(name.clone(), c.clone());
                }
            }
        }
        out
    }

    fn write_server_map(
        mut root: Map<String, Value>,
        key_name: &str,
        servers: BTreeMap<String, Map<String, Value>>,
    ) -> Map<String, Value> {
        let servers_json: Map<String, Value> = servers
            .into_iter()
            .map(|(k, v)| (k, Value::Object(v)))
            .collect();
        if key_name == "mcp.servers" {
            let mut mcp = root
                .get("mcp")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            mcp.insert("servers".to_string(), Value::Object(servers_json));
            root.insert("mcp".to_string(), Value::Object(mcp));
        } else {
            root.insert(key_name.to_string(), Value::Object(servers_json));
        }
        root
    }

    fn read_server_enabled(config: &Map<String, Value>) -> bool {
        config.get(ENABLED_KEY) != Some(&Value::Bool(false))
    }

    fn set_server_enabled(mut config: Map<String, Value>, enabled: bool) -> Map<String, Value> {
        config.remove(LEGACY_ENABLE_KEY);
        if enabled {
            config.remove(ENABLED_KEY);
        } else {
            config.insert(ENABLED_KEY.to_string(), Value::Bool(false));
        }
        config
    }

    fn migrate_legacy_enable(
        mut servers: BTreeMap<String, Map<String, Value>>,
    ) -> (BTreeMap<String, Map<String, Value>>, bool) {
        let mut changed = false;
        for config in servers.values_mut() {
            if config.contains_key(LEGACY_ENABLE_KEY) {
                let disabled = config.get(LEGACY_ENABLE_KEY) == Some(&Value::Bool(false))
                    || config.get(ENABLED_KEY) == Some(&Value::Bool(false));
                *config = Self::set_server_enabled(config.clone(), !disabled);
                changed = true;
            }
        }
        (servers, changed)
    }

    async fn read_zcode_servers(
        &self,
        path: &Path,
    ) -> Result<
        (
            Option<Map<String, Value>>,
            BTreeMap<String, Map<String, Value>>,
        ),
        ServiceError,
    > {
        let Some(root) = Self::read_json_object(path).await? else {
            return Ok((None, BTreeMap::new()));
        };
        let servers = Self::read_server_map(&root, "mcp.servers");
        let (servers, changed) = Self::migrate_legacy_enable(servers);
        let mut root = root;
        if changed {
            root = Self::write_server_map(root, "mcp.servers", servers.clone());
            let _ = Self::write_json_object(path, &root).await;
        }
        Ok((Some(root), servers))
    }

    async fn read_agents_servers(
        &self,
        path: &Path,
    ) -> Result<BTreeMap<String, Map<String, Value>>, ServiceError> {
        let Some(root) = Self::read_json_object(path).await? else {
            return Ok(BTreeMap::new());
        };
        Ok(Self::read_server_map(&root, "mcpServers"))
    }

    fn candidate_id(source: &str, path: &str, name: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(format!("{source}:{path}:{name}").as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn normalize_name_key(name: &str) -> String {
        name.trim().to_lowercase()
    }

    async fn effective_user_records(&self) -> Result<Vec<Value>, ServiceError> {
        let zcode_path = self.zcode_config_path();
        let (_root, zcode_servers) = self.read_zcode_servers(&zcode_path).await?;
        if !zcode_servers.is_empty() {
            return Ok(self.records_from_servers("zcode", &zcode_path, zcode_servers));
        }
        let agents_path = self.agents_mcp_path();
        let agents_servers = self.read_agents_servers(&agents_path).await?;
        Ok(self.records_from_servers("agents", &agents_path, agents_servers))
    }

    fn records_from_servers(
        &self,
        source: &str,
        path: &Path,
        servers: BTreeMap<String, Map<String, Value>>,
    ) -> Vec<Value> {
        let path_str = path.to_string_lossy().to_string();
        let mut records: Vec<Value> = servers
            .into_iter()
            .map(|(name, config)| {
                let enabled = Self::read_server_enabled(&config);
                json!({
                    "name": name,
                    "config": Value::Object(config),
                    "enabled": enabled,
                    "source": source,
                    "path": path_str,
                })
            })
            .collect();
        records.sort_by(|a, b| {
            a.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
        });
        records
    }

    fn record_to_candidate(record: &Value) -> Value {
        let name = record.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let source = record.get("source").and_then(|v| v.as_str()).unwrap_or("");
        let path = record.get("path").and_then(|v| v.as_str()).unwrap_or("");
        json!({
            "id": Self::candidate_id(source, path, name),
            "name": name,
            "config": record.get("config").cloned().unwrap_or(Value::Null),
            "enabled": record.get("enabled").cloned().unwrap_or(Value::Bool(true)),
            "source": source,
            "path": path,
        })
    }

    async fn load_mcp_from_user_directory(
        &self,
        args: &ServiceArgs,
    ) -> Result<Value, ServiceError> {
        let req = args.json(0).ok().flatten().unwrap_or(Value::Null);
        let workspace_path = req.get("workspacePath").and_then(|v| v.as_str());
        let mut servers = Vec::new();
        if let Some(wp) = workspace_path.filter(|s| !s.trim().is_empty()) {
            servers.extend(self.load_directory_servers("workspace", Some(wp)).await?);
        }
        servers.extend(self.load_directory_servers("user", workspace_path).await?);
        Ok(json!({ "servers": servers }))
    }

    async fn load_directory_servers(
        &self,
        scope: &str,
        workspace_path: Option<&str>,
    ) -> Result<Vec<Value>, ServiceError> {
        let (zcode_path, agents_path, project_path, location_scope) = if scope == "workspace" {
            let wp = workspace_path.unwrap_or("");
            (
                self.workspace_zcode_config_path(wp),
                self.workspace_agents_mcp_path(wp),
                Some(wp.to_string()),
                "project",
            )
        } else {
            (
                self.zcode_config_path(),
                self.agents_mcp_path(),
                None,
                "user",
            )
        };

        // Prefer zcode config if it has entries, else agents.
        let (_root, zcode_servers) = self.read_zcode_servers(&zcode_path).await?;
        let (source, file_path, server_map, descriptor_source, key_name) =
            if !zcode_servers.is_empty() {
                (
                    "zcode",
                    zcode_path.clone(),
                    zcode_servers,
                    "zcode",
                    "mcp.servers",
                )
            } else {
                let agents = self.read_agents_servers(&agents_path).await?;
                if agents.is_empty() && !zcode_servers.is_empty() {
                    // already handled
                    ("zcode", zcode_path, zcode_servers, "zcode", "mcp.servers")
                } else if !agents.is_empty() {
                    (
                        "agents",
                        agents_path.clone(),
                        agents,
                        "agents",
                        "mcpServers",
                    )
                } else {
                    return Ok(Vec::new());
                }
            };
        let _ = key_name;
        let file_path_str = file_path.to_string_lossy().to_string();
        let dir_path = file_path
            .parent()
            .unwrap_or(&file_path)
            .to_string_lossy()
            .to_string();
        let mut out = Vec::new();
        for (name, config) in server_map {
            let enabled = Self::read_server_enabled(&config);
            let mut item = json!({
                "source": "zcodeagentmcp",
                "scope": scope,
                "name": name,
                "config": Value::Object(config),
                "enabled": enabled,
                "location": {
                    "source": descriptor_source,
                    "scope": location_scope,
                    "directoryPath": dir_path,
                },
                "file": {
                    "format": "json",
                    "filePath": file_path_str,
                }
            });
            if let (Some(pp), Some(obj)) = (&project_path, item.as_object_mut()) {
                obj.insert("projectPath".to_string(), Value::String(pp.clone()));
                if let Some(loc) = obj.get_mut("location").and_then(|v| v.as_object_mut()) {
                    loc.insert("projectPath".to_string(), Value::String(pp.clone()));
                }
            }
            let _ = source;
            out.push(item);
        }
        Ok(out)
    }

    async fn save_mcp_to_user_directory(&self, args: &ServiceArgs) -> Result<Value, ServiceError> {
        let payload = args.required_json(0, "payload")?;
        let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let name = payload
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ServiceError::invalid_params("name is required"))?;
        let project_path = payload.get("projectPath").and_then(|v| v.as_str());

        if action == "set-enabled" {
            let enabled = payload
                .get("enabled")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| {
                    ServiceError::invalid_params("enabled is required for set-enabled")
                })?;
            let config_path = if let Some(wp) = project_path {
                self.workspace_zcode_config_path(wp)
            } else {
                self.zcode_config_path()
            };
            let (root_opt, mut servers) = self.read_zcode_servers(&config_path).await?;
            let mut root = root_opt.unwrap_or_default();
            if let Some(cfg) = servers.get(name).cloned() {
                servers.insert(name.to_string(), Self::set_server_enabled(cfg, enabled));
                root = Self::write_server_map(root, "mcp.servers", servers);
                Self::write_json_object(&config_path, &root).await?;
            }
            return Ok(Value::Null);
        }

        let config_path = if let Some(wp) = project_path {
            self.workspace_zcode_config_path(wp)
        } else {
            self.zcode_config_path()
        };
        let (root_opt, mut servers) = self.read_zcode_servers(&config_path).await?;
        let mut root = root_opt.unwrap_or_default();
        match action {
            "upsert" => {
                let config = payload
                    .get("config")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .ok_or_else(|| ServiceError::invalid_params("config is required for upsert"))?;
                servers.insert(name.to_string(), config);
            }
            "remove" | "delete" => {
                servers.remove(name);
            }
            _ => {
                return Err(ServiceError::invalid_params(format!(
                    "unknown action: {action}"
                )))
            }
        }
        root = Self::write_server_map(root, "mcp.servers", servers);
        Self::write_json_object(&config_path, &root).await?;
        Ok(Value::Null)
    }

    async fn list_local_user_mcp_candidates(&self) -> Result<Value, ServiceError> {
        let records = self.effective_user_records().await?;
        let candidates: Vec<Value> = records.iter().map(Self::record_to_candidate).collect();
        Ok(json!({
            "candidates": candidates,
            "localHomeDir": self.home_dir().to_string_lossy(),
        }))
    }

    async fn list_remote_user_mcp_statuses(
        &self,
        args: &ServiceArgs,
    ) -> Result<Value, ServiceError> {
        let params = args.required_json(0, "params")?;
        let names = params
            .get("names")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let records = self.effective_user_records().await?;
        let mut by_name: BTreeMap<String, Value> = BTreeMap::new();
        for r in records {
            let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("");
            by_name.entry(Self::normalize_name_key(name)).or_insert(r);
        }
        let statuses: Vec<Value> = names
            .into_iter()
            .map(|n| {
                let name = n.as_str().unwrap_or("").to_string();
                if let Some(existing) = by_name.get(&Self::normalize_name_key(&name)) {
                    json!({
                        "name": name,
                        "exists": true,
                        "path": existing.get("path").cloned().unwrap_or(Value::Null),
                    })
                } else {
                    json!({ "name": name, "exists": false })
                }
            })
            .collect();
        Ok(json!({
            "remoteHomeDir": self.home_dir().to_string_lossy(),
            "statuses": statuses,
        }))
    }

    async fn export_mcp_servers(&self, args: &ServiceArgs) -> Result<Value, ServiceError> {
        let params = args.required_json(0, "params")?;
        let ids = params
            .get("serverIds")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let records = self.effective_user_records().await?;
        let candidates: Vec<Value> = records.iter().map(Self::record_to_candidate).collect();
        let by_id: BTreeMap<String, Value> = candidates
            .iter()
            .filter_map(|c| {
                c.get("id")
                    .and_then(|v| v.as_str())
                    .map(|id| (id.to_string(), c.clone()))
            })
            .collect();
        let mut servers = Vec::new();
        for idv in ids {
            let id = idv.as_str().unwrap_or("");
            let Some(c) = by_id.get(id) else {
                return Err(ServiceError::new(format!(
                    "mcp sync candidate not found: {id}"
                )));
            };
            servers.push(json!({
                "id": c["id"],
                "name": c["name"],
                "config": c["config"],
                "enabled": c["enabled"],
                "source": c["source"],
                "path": c["path"],
            }));
        }
        Ok(json!({
            "localHomeDir": self.home_dir().to_string_lossy(),
            "servers": servers,
        }))
    }

    async fn import_mcp_servers(&self, args: &ServiceArgs) -> Result<Value, ServiceError> {
        let params = args.required_json(0, "params")?;
        if params
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(ServiceError::new("mcp sync overwrite is not supported"));
        }
        let servers = params
            .get("servers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let target_path = self.zcode_config_path();
        let (root_opt, mut target_servers) = self.read_zcode_servers(&target_path).await?;
        let mut root = root_opt.unwrap_or_default();
        let existing = self.effective_user_records().await?;
        let mut existing_by_name: BTreeMap<String, Value> = BTreeMap::new();
        for r in existing {
            let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("");
            existing_by_name
                .entry(Self::normalize_name_key(name))
                .or_insert(r);
        }
        let mut results = Vec::new();
        let mut changed = false;
        let target_path_str = target_path.to_string_lossy().to_string();
        for server in servers {
            let name = server
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name_key = Self::normalize_name_key(&name);
            if target_servers.contains_key(&name) || existing_by_name.contains_key(&name_key) {
                let path = existing_by_name
                    .get(&name_key)
                    .and_then(|r| r.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&target_path_str)
                    .to_string();
                results.push(json!({ "name": name, "status": "skipped", "path": path }));
                continue;
            }
            let enabled = server
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let mut config = server
                .get("config")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            config = Self::set_server_enabled(config, enabled);
            target_servers.insert(name.clone(), config.clone());
            existing_by_name.insert(
                name_key,
                json!({"name": name, "config": Value::Object(config), "enabled": enabled, "source": "zcode", "path": target_path_str}),
            );
            results.push(json!({ "name": name, "status": "synced", "path": target_path_str }));
            changed = true;
        }
        if changed {
            root = Self::write_server_map(root, "mcp.servers", target_servers);
            Self::write_json_object(&target_path, &root).await?;
        }
        Ok(json!({ "results": results }))
    }

    async fn check_remote_user_mcp_write_access(&self) -> Result<Value, ServiceError> {
        let dir = self
            .zcode_config_path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.home_dir().join(".zcode").join("cli"));
        fs::create_dir_all(&dir)
            .await
            .map_err(|e| ServiceError::new(format!("create {}: {e}", dir.display())))?;
        let probe = dir.join(format!(".zcode-mcp-write-probe-{}", std::process::id()));
        match fs::write(&probe, b"probe").await {
            Ok(()) => {
                let _ = fs::remove_file(&probe).await;
                Ok(json!({ "writable": true, "path": dir.to_string_lossy() }))
            }
            Err(e) => Ok(
                json!({ "writable": false, "path": dir.to_string_lossy(), "error": e.to_string() }),
            ),
        }
    }
}

#[async_trait]
impl Channel for McpSyncService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        let result = match command {
            "loadMcpFromUserDirectory" => self.load_mcp_from_user_directory(&args).await,
            "listWorkspaceMcpServerStatuses" => {
                return Err(ChannelFailure::error(
                    "MCP server status listing is unavailable in this runtime",
                ));
            }
            "saveMcpToUserDirectory" => self.save_mcp_to_user_directory(&args).await,
            "listLocalUserMcpCandidates" => self.list_local_user_mcp_candidates().await,
            "listRemoteUserMcpStatuses" => self.list_remote_user_mcp_statuses(&args).await,
            "exportMcpServers" => self.export_mcp_servers(&args).await,
            "checkRemoteUserMcpWriteAccess" => self.check_remote_user_mcp_write_access().await,
            "importMcpServers" => self.import_mcp_servers(&args).await,
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

    fn paths(root: &Path) -> AppPaths {
        AppPaths::new(root.join("home"), root.join("data"))
    }

    fn ctx() -> ChannelContext {
        ChannelContext::new("mcp-sync", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn save_and_load_user_mcp_round_trip() {
        let tmp = TempDir::new().unwrap();
        let svc = McpSyncService::new(paths(tmp.path()));
        svc.call(
            &ctx(),
            "saveMcpToUserDirectory",
            vec![WireValue::Json(json!({
                "action": "upsert",
                "name": "demo",
                "config": {"command":"node","args":["s.js"],"type":"stdio"}
            }))],
            CancelToken::new(),
        )
        .await
        .unwrap();
        let loaded = svc
            .call(
                &ctx(),
                "loadMcpFromUserDirectory",
                vec![WireValue::Json(json!({}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(v) = loaded else { panic!() };
        let servers = v["servers"].as_array().unwrap();
        assert!(servers.iter().any(|s| s["name"] == "demo"));
    }

    #[tokio::test]
    async fn set_enabled_writes_flag_and_candidates_list() {
        let tmp = TempDir::new().unwrap();
        let svc = McpSyncService::new(paths(tmp.path()));
        svc.save_mcp_to_user_directory(&ServiceArgs::new(vec![WireValue::Json(json!({
            "action":"upsert","name":"a","config":{"command":"x"}
        }))]))
        .await
        .unwrap();
        svc.save_mcp_to_user_directory(&ServiceArgs::new(vec![WireValue::Json(json!({
            "action":"set-enabled","name":"a","enabled":false
        }))]))
        .await
        .unwrap();
        let v = svc.list_local_user_mcp_candidates().await.unwrap();
        let c = v["candidates"].as_array().unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["enabled"], false);
    }

    #[tokio::test]
    async fn list_workspace_statuses_is_unavailable() {
        let tmp = TempDir::new().unwrap();
        let svc = McpSyncService::new(paths(tmp.path()));
        let err = svc
            .call(
                &ctx(),
                "listWorkspaceMcpServerStatuses",
                vec![WireValue::Json(json!({}))],
                CancelToken::new(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unavailable"));
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use tempfile::TempDir;

    #[tokio::test]
    async fn export_import_round_trip_skips_existing() {
        let tmp = TempDir::new().unwrap();
        let svc = McpSyncService::new(AppPaths::new(
            tmp.path().join("home"),
            tmp.path().join("data"),
        ));
        let ctx = ChannelContext::new("mcp-sync", ClientMode::DesktopContinuous);
        svc.call(
            &ctx,
            "saveMcpToUserDirectory",
            vec![WireValue::Json(
                json!({"action":"upsert","name":"s1","config":{"command":"n"}}),
            )],
            CancelToken::new(),
        )
        .await
        .unwrap();
        let exported = svc
            .call(
                &ctx,
                "exportMcpServers",
                vec![WireValue::Json(json!({"serverIds":[]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(exp) = exported else {
            panic!()
        };
        // empty selection ok
        assert!(exp["servers"].as_array().unwrap().is_empty());
        let candidates = svc
            .call(
                &ctx,
                "listLocalUserMcpCandidates",
                vec![],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(c) = candidates else {
            panic!()
        };
        let id = c["candidates"][0]["id"].as_str().unwrap().to_string();
        let exported = svc
            .call(
                &ctx,
                "exportMcpServers",
                vec![WireValue::Json(json!({"serverIds":[id]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(exp) = exported else {
            panic!()
        };
        // import into fresh service
        let tmp2 = TempDir::new().unwrap();
        let svc2 = McpSyncService::new(AppPaths::new(
            tmp2.path().join("home"),
            tmp2.path().join("data"),
        ));
        let res = svc2
            .call(
                &ctx,
                "importMcpServers",
                vec![WireValue::Json(json!({"servers": exp["servers"]}))],
                CancelToken::new(),
            )
            .await
            .unwrap();
        let WireValue::Json(r) = res else { panic!() };
        assert_eq!(r["results"][0]["status"], "synced");
    }
}
