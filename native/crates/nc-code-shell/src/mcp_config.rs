//! File-based MCP user-directory config for the Tauri shell.
//!
//! Mirrors the Electron `mcpUserDirectory` read/write subset:
//! - `~/.zcode/cli/config.json` (`mcp.servers`) and `<workspace>/.zcode/config.json`,
//! - `~/.agents/mcp.json` (`mcpServers`) and `<workspace>/.agents/mcp.json`.
//!
//! Load prefers `.zcode` over `.agents` per scope, matching the Electron
//! `readDirectoryServersFromPreferredSources` behavior. Save writes the selected
//! descriptor atomically and preserves unrelated JSON keys.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Descriptor {
    Zcode,
    Agents,
}

impl Descriptor {
    fn source(self) -> &'static str {
        match self {
            Descriptor::Zcode => "zcode",
            Descriptor::Agents => "agents",
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Descriptor::Zcode => "config.json",
            Descriptor::Agents => "mcp.json",
        }
    }

    fn user_path(self, home: &Path) -> PathBuf {
        match self {
            Descriptor::Zcode => home.join(".zcode").join("cli").join("config.json"),
            Descriptor::Agents => home.join(".agents").join("mcp.json"),
        }
    }

    fn workspace_path(self, workspace: &Path) -> PathBuf {
        match self {
            Descriptor::Zcode => workspace.join(".zcode").join("config.json"),
            Descriptor::Agents => workspace.join(".agents").join("mcp.json"),
        }
    }

    fn server_map(self, root: &Map<String, Value>) -> Option<&Map<String, Value>> {
        match self {
            Descriptor::Zcode => root
                .get("mcp")
                .and_then(Value::as_object)
                .and_then(|mcp| mcp.get("servers"))
                .and_then(Value::as_object),
            Descriptor::Agents => root.get("mcpServers").and_then(Value::as_object),
        }
    }

    fn write_server_map(self, root: &mut Map<String, Value>, servers: Map<String, Value>) {
        match self {
            Descriptor::Zcode => {
                let mcp = root
                    .entry("mcp".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if !mcp.is_object() {
                    *mcp = Value::Object(Map::new());
                }
                if let Some(mcp) = mcp.as_object_mut() {
                    mcp.insert("servers".to_string(), Value::Object(servers));
                }
            }
            Descriptor::Agents => {
                root.insert("mcpServers".to_string(), Value::Object(servers));
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDirectoryLocation {
    pub source: String,
    pub scope: String,
    pub directory_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeMcpFileReference {
    pub format: String,
    pub file_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeMcpServerRecord {
    pub source: String,
    pub scope: String,
    pub name: String,
    pub config: Value,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    pub location: SettingsDirectoryLocation,
    pub file: NativeMcpFileReference,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LoadMcpRequest {
    #[serde(default)]
    pub workspace_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadMcpResult {
    pub servers: Vec<NativeMcpServerRecord>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveMcpRequest {
    pub action: String,
    #[serde(default)]
    pub source: Option<String>,
    pub name: String,
    #[serde(default)]
    pub config: Option<Value>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub location: Option<SettingsDirectoryLocation>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveMcpResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SaveMcpResult {
    fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(error.into()),
        }
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_json_object(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|error| format!("invalid_json: {error}"))?;
            match value {
                Value::Object(map) => Ok(Some(map)),
                _ => Err("invalid_json: root must be an object".to_string()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn write_json_atomic(path: &Path, root: &Map<String, Value>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let content =
        serde_json::to_vec_pretty(&Value::Object(root.clone())).map_err(|e| e.to_string())?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, &content).map_err(|error| error.to_string())?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = std::fs::remove_file(path);
            std::fs::rename(&temp, path).map_err(|error| {
                let _ = std::fs::remove_file(&temp);
                error.to_string()
            })
        }
    }
}

fn read_scope(
    home: &Path,
    workspace: Option<&Path>,
    scope: &str,
    descriptor: Descriptor,
) -> Result<Vec<NativeMcpServerRecord>, String> {
    let file_path = match scope {
        "workspace" => {
            let workspace = workspace.ok_or_else(|| "missing_workspace_path".to_string())?;
            descriptor.workspace_path(workspace)
        }
        _ => descriptor.user_path(home),
    };
    let Some(root) = read_json_object(&file_path)? else {
        return Ok(Vec::new());
    };
    let Some(servers) = descriptor.server_map(&root) else {
        return Ok(Vec::new());
    };
    let directory_path = file_path
        .parent()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    let project_path = if scope == "workspace" {
        workspace.map(|path| path.to_string_lossy().to_string())
    } else {
        None
    };
    let location = SettingsDirectoryLocation {
        source: descriptor.source().to_string(),
        scope: if scope == "workspace" {
            "project".to_string()
        } else {
            "user".to_string()
        },
        directory_path,
        project_path: project_path.clone(),
    };
    Ok(servers
        .iter()
        .map(|(name, config)| NativeMcpServerRecord {
            source: "zcodeagentmcp".to_string(),
            scope: scope.to_string(),
            name: name.clone(),
            config: config.clone(),
            enabled: config.get("enabled") != Some(&Value::Bool(false))
                && config.get("enable") != Some(&Value::Bool(false)),
            project_path: project_path.clone(),
            location: location.clone(),
            file: NativeMcpFileReference {
                format: "json".to_string(),
                file_path: file_path.to_string_lossy().to_string(),
            },
        })
        .collect())
}

fn load_from(home: &Path, workspace: Option<&Path>) -> Result<LoadMcpResult, String> {
    let mut servers = Vec::new();
    if let Some(workspace) = workspace {
        let zcode = read_scope(home, Some(workspace), "workspace", Descriptor::Zcode)?;
        if zcode.is_empty() {
            servers.extend(read_scope(
                home,
                Some(workspace),
                "workspace",
                Descriptor::Agents,
            )?);
        } else {
            servers.extend(zcode);
        }
    }
    let zcode = read_scope(home, workspace, "user", Descriptor::Zcode)?;
    if zcode.is_empty() {
        servers.extend(read_scope(home, workspace, "user", Descriptor::Agents)?);
    } else {
        servers.extend(zcode);
    }
    Ok(LoadMcpResult { servers })
}

fn descriptor_for_request(request: &SaveMcpRequest) -> Descriptor {
    let source = request
        .location
        .as_ref()
        .map(|location| location.source.as_str())
        .or(request.source.as_deref())
        .unwrap_or("zcode");
    if source == "agents" {
        Descriptor::Agents
    } else {
        Descriptor::Zcode
    }
}

fn save_target(
    home: &Path,
    request: &SaveMcpRequest,
    descriptor: Descriptor,
) -> Result<(PathBuf, String), String> {
    if let Some(location) = &request.location {
        let scope = if location.scope == "project" || location.project_path.is_some() {
            "workspace"
        } else {
            "user"
        };
        return Ok((
            PathBuf::from(&location.directory_path).join(descriptor.file_name()),
            scope.to_string(),
        ));
    }
    match request.project_path.as_deref() {
        Some(project) if !project.trim().is_empty() => Ok((
            descriptor.workspace_path(Path::new(project)),
            "workspace".to_string(),
        )),
        _ => Ok((descriptor.user_path(home), "user".to_string())),
    }
}

fn save_to(home: &Path, request: &SaveMcpRequest) -> Result<(), String> {
    let descriptor = descriptor_for_request(request);
    let (file_path, _scope) = save_target(home, request, descriptor)?;
    let mut root = read_json_object(&file_path)?.unwrap_or_default();
    let mut servers = descriptor.server_map(&root).cloned().unwrap_or_default();
    match request.action.as_str() {
        "upsert" => {
            let config = request
                .config
                .clone()
                .ok_or_else(|| "missing_config".to_string())?;
            servers.insert(request.name.clone(), config);
        }
        "delete" => {
            servers.remove(&request.name);
        }
        "set-enabled" => {
            let enabled = request
                .enabled
                .ok_or_else(|| "missing_enabled".to_string())?;
            if let Some(Value::Object(config)) = servers.get_mut(&request.name) {
                config.remove("enable");
                if enabled {
                    config.remove("enabled");
                } else {
                    config.insert("enabled".to_string(), Value::Bool(false));
                }
            }
        }
        other => return Err(format!("unsupported_action: {other}")),
    }
    descriptor.write_server_map(&mut root, servers);
    write_json_atomic(&file_path, &root)
}

/// `IPlatformService.loadMcpFromUserDirectory`.
#[tauri::command]
pub fn load_mcp_from_user_directory(
    request: Option<LoadMcpRequest>,
) -> Result<LoadMcpResult, String> {
    let request = request.unwrap_or_default();
    let workspace = request
        .workspace_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(Path::new);
    load_from(&home_dir(), workspace)
}

/// `IPlatformService.saveMcpToUserDirectory`.
#[tauri::command]
pub fn save_mcp_to_user_directory(payload: SaveMcpRequest) -> SaveMcpResult {
    match save_to(&home_dir(), &payload) {
        Ok(()) => SaveMcpResult {
            success: true,
            error: None,
        },
        Err(error) => SaveMcpResult::failure(error),
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MigrateLegacyRequest {
    #[serde(default)]
    pub legacy_storage_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateLegacyResult {
    pub servers: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    pub total_count: usize,
    pub imported_count: usize,
    pub skipped_count: usize,
}

impl MigrateLegacyResult {
    fn empty() -> Self {
        Self {
            servers: Map::new(),
            source_path: None,
            total_count: 0,
            imported_count: 0,
            skipped_count: 0,
        }
    }

    fn from_servers(servers: Map<String, Value>, source_path: String) -> Self {
        let total_count = servers.len();
        let mut imported = Map::new();
        for (name, config) in servers {
            if config.is_object() {
                imported.insert(name, config);
            }
        }
        let imported_count = imported.len();
        Self {
            servers: imported,
            source_path: Some(source_path),
            total_count,
            imported_count,
            skipped_count: total_count.saturating_sub(imported_count),
        }
    }
}

fn normalize_server_map(value: Option<&Value>) -> Map<String, Value> {
    let Some(object) = value.and_then(Value::as_object) else {
        return Map::new();
    };
    object
        .iter()
        .map(|(name, config)| (name.clone(), config.clone()))
        .collect()
}

fn legacy_servers_from_config(value: &Value) -> Option<Map<String, Value>> {
    let config = value.as_object()?;
    let mcp = config.get("mcp")?.as_object()?;
    let servers = mcp.get("mcpServers")?;
    Some(normalize_server_map(Some(servers)))
}

fn read_legacy_store_json(path: &Path) -> Option<MigrateLegacyResult> {
    let bytes = std::fs::read(path).ok()?;
    let root: Value = serde_json::from_slice(&bytes).ok()?;
    let root = root.as_object()?;
    let storage_raw = root.get("mcp-storage")?.as_str()?;
    let storage: Value = serde_json::from_str(storage_raw).ok()?;
    let state = storage.as_object()?.get("state")?.as_object()?;
    let config = state.get("config")?;
    let servers = legacy_servers_from_config(config)?;
    if servers.is_empty() {
        return None;
    }
    Some(MigrateLegacyResult::from_servers(
        servers,
        path.to_string_lossy().to_string(),
    ))
}

fn extract_balanced_json(text: &str, start: usize) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if character == '\\' {
                escaped = true;
                continue;
            }
            if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + character.len_utf8();
                    return Some(&text[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_legacy_servers_from_text(text: &str) -> Option<Map<String, Value>> {
    let mut search_start = 0usize;
    while search_start < text.len() {
        let key_index = text[search_start..].find("mcp-config")? + search_start;
        let json_start = text[key_index..].find('{')? + key_index;
        if let Some(json_text) = extract_balanced_json(text, json_start) {
            if let Ok(parsed) = serde_json::from_str::<Value>(json_text) {
                if let Some(servers) = legacy_servers_from_config(&parsed) {
                    return Some(servers);
                }
            }
        }
        search_start = key_index + "mcp-config".len();
    }
    None
}

fn read_legacy_leveldb_dir(directory: &Path) -> Option<MigrateLegacyResult> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(directory)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|value| value.to_str())
                .map(|extension| {
                    extension.eq_ignore_ascii_case("ldb") || extension.eq_ignore_ascii_case("log")
                })
                .unwrap_or(false)
        })
        .collect();
    entries.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    for path in entries {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        // LevelDB stores Latin-1-ish bytes; map each byte to the same code point
        // before searching for the embedded JSON.
        let text: String = bytes.iter().map(|byte| *byte as char).collect();
        if let Some(servers) = extract_legacy_servers_from_text(&text) {
            return Some(MigrateLegacyResult::from_servers(
                servers,
                path.to_string_lossy().to_string(),
            ));
        }
    }
    None
}

fn legacy_storage_candidates(request: &MigrateLegacyRequest) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(explicit) = request
        .legacy_storage_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        candidates.push(PathBuf::from(explicit));
    }
    let home = home_dir();
    let app_data = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData").join("Roaming"));
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData").join("Local"));
    candidates.push(app_data.join("ai.z.zcode").join("store.json"));
    candidates.push(
        local_app_data
            .join("ai.z.work")
            .join("EBWebView")
            .join("Default")
            .join("Local Storage")
            .join("leveldb"),
    );
    candidates.push(app_data.join("ZCode").join("Local Storage").join("leveldb"));
    candidates.push(
        app_data
            .join("ZCode")
            .join("Partitions")
            .join("zcode-embedded-browser")
            .join("Local Storage")
            .join("leveldb"),
    );
    candidates.push(
        app_data
            .join("ZCode Dev")
            .join("Local Storage")
            .join("leveldb"),
    );
    candidates.push(
        app_data
            .join("ZCode Dev")
            .join("Partitions")
            .join("zcode-embedded-browser")
            .join("Local Storage")
            .join("leveldb"),
    );
    let mut unique = Vec::new();
    for candidate in candidates {
        if !unique.iter().any(|existing| existing == &candidate) {
            unique.push(candidate);
        }
    }
    unique
}

fn migrate_from_candidates(candidates: &[PathBuf]) -> MigrateLegacyResult {
    for candidate in candidates {
        if candidate
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("store.json"))
        {
            if let Some(result) = read_legacy_store_json(candidate) {
                return result;
            }
            continue;
        }
        if let Some(result) = read_legacy_leveldb_dir(candidate) {
            return result;
        }
    }
    MigrateLegacyResult::empty()
}

fn migrate_from(request: &MigrateLegacyRequest) -> MigrateLegacyResult {
    migrate_from_candidates(&legacy_storage_candidates(request))
}

/// `IPlatformService.migrateLegacyCommonMcp`.
#[tauri::command]
pub fn migrate_legacy_common_mcp(request: Option<MigrateLegacyRequest>) -> MigrateLegacyResult {
    migrate_from(&request.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zcode-mcp-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_json(path: &Path, value: Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    #[test]
    fn loads_zcode_servers_and_prefers_them_over_agents() {
        let home = temp_root("load");
        write_json(
            &home.join(".zcode").join("cli").join("config.json"),
            json!({
                "mcp": {
                    "servers": {
                        "zcode-server": { "command": "zcode-mcp", "enabled": false }
                    }
                },
                "unrelated": true
            }),
        );
        write_json(
            &home.join(".agents").join("mcp.json"),
            json!({ "mcpServers": { "agents-server": { "command": "agents-mcp" } } }),
        );
        let result = load_from(&home, None).unwrap();
        assert_eq!(result.servers.len(), 1);
        assert_eq!(result.servers[0].name, "zcode-server");
        assert!(!result.servers[0].enabled);
        assert_eq!(result.servers[0].source, "zcodeagentmcp");
        assert_eq!(result.servers[0].location.source, "zcode");
        assert_eq!(result.servers[0].scope, "user");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn falls_back_to_agents_when_zcode_has_no_servers() {
        let home = temp_root("fallback");
        write_json(
            &home.join(".agents").join("mcp.json"),
            json!({ "mcpServers": { "agents-server": { "command": "agents-mcp" } } }),
        );
        let result = load_from(&home, None).unwrap();
        assert_eq!(result.servers.len(), 1);
        assert_eq!(result.servers[0].name, "agents-server");
        assert_eq!(result.servers[0].location.source, "agents");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn loads_workspace_and_user_scopes() {
        let home = temp_root("scopes-home");
        let workspace = temp_root("scopes-ws");
        write_json(
            &home.join(".zcode").join("cli").join("config.json"),
            json!({ "mcp": { "servers": { "user-server": { "command": "user" } } } }),
        );
        write_json(
            &workspace.join(".zcode").join("config.json"),
            json!({ "mcp": { "servers": { "ws-server": { "command": "ws" } } } }),
        );
        let result = load_from(&home, Some(&workspace)).unwrap();
        assert_eq!(result.servers.len(), 2);
        assert!(result
            .servers
            .iter()
            .any(|server| server.name == "ws-server" && server.scope == "workspace"));
        assert!(result
            .servers
            .iter()
            .any(|server| server.name == "user-server" && server.scope == "user"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn upserts_and_deletes_while_preserving_unrelated_keys() {
        let home = temp_root("save");
        let path = home.join(".zcode").join("cli").join("config.json");
        write_json(&path, json!({ "other": { "keep": true } }));
        save_to(
            &home,
            &SaveMcpRequest {
                action: "upsert".to_string(),
                source: None,
                name: "server-a".to_string(),
                config: Some(json!({ "command": "run-a" })),
                enabled: None,
                project_path: None,
                location: None,
            },
        )
        .unwrap();
        let root = read_json_object(&path).unwrap().unwrap();
        assert_eq!(root["other"]["keep"], true);
        assert_eq!(root["mcp"]["servers"]["server-a"]["command"], "run-a");
        save_to(
            &home,
            &SaveMcpRequest {
                action: "delete".to_string(),
                source: None,
                name: "server-a".to_string(),
                config: None,
                enabled: None,
                project_path: None,
                location: None,
            },
        )
        .unwrap();
        let root = read_json_object(&path).unwrap().unwrap();
        assert!(root["mcp"]["servers"].as_object().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn set_enabled_updates_the_selected_agents_location() {
        let home = temp_root("enabled");
        let agents = home.join(".agents").join("mcp.json");
        write_json(
            &agents,
            json!({ "mcpServers": { "server-a": { "command": "run-a", "enable": true } } }),
        );
        let location = SettingsDirectoryLocation {
            source: "agents".to_string(),
            scope: "user".to_string(),
            directory_path: home.join(".agents").to_string_lossy().to_string(),
            project_path: None,
        };
        save_to(
            &home,
            &SaveMcpRequest {
                action: "set-enabled".to_string(),
                source: None,
                name: "server-a".to_string(),
                config: None,
                enabled: Some(false),
                project_path: None,
                location: Some(location),
            },
        )
        .unwrap();
        let root = read_json_object(&agents).unwrap().unwrap();
        let server = &root["mcpServers"]["server-a"];
        assert_eq!(server["enabled"], false);
        assert!(server.get("enable").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_config_or_enabled_is_rejected() {
        let home = temp_root("errors");
        let error = save_to(
            &home,
            &SaveMcpRequest {
                action: "upsert".to_string(),
                source: None,
                name: "server-a".to_string(),
                config: None,
                enabled: None,
                project_path: None,
                location: None,
            },
        )
        .unwrap_err();
        assert_eq!(error, "missing_config");
        let error = save_to(
            &home,
            &SaveMcpRequest {
                action: "set-enabled".to_string(),
                source: None,
                name: "server-a".to_string(),
                config: None,
                enabled: None,
                project_path: None,
                location: None,
            },
        )
        .unwrap_err();
        assert_eq!(error, "missing_enabled");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn migrates_legacy_store_json_and_counts_skips() {
        let root = temp_root("legacy-store");
        let store = root.join("store.json");
        let config = json!({
            "mcp": {
                "mcpServers": {
                    "good": { "command": "run-good" },
                    "bad": 7
                }
            }
        });
        let storage = json!({ "state": { "config": config } }).to_string();
        write_json(&store, json!({ "mcp-storage": storage }));
        let result = migrate_from_candidates(std::slice::from_ref(&store));
        assert_eq!(result.total_count, 2);
        assert_eq!(result.imported_count, 1);
        assert_eq!(result.skipped_count, 1);
        assert_eq!(result.servers.len(), 1);
        assert_eq!(result.servers["good"]["command"], "run-good");
        assert_eq!(result.source_path.as_deref(), Some(store.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn migrates_legacy_leveldb_embedded_json() {
        let root = temp_root("legacy-leveldb");
        let directory = root.join("leveldb");
        std::fs::create_dir_all(&directory).unwrap();
        let payload = br#"prefix mcp-config {"mcp":{"mcpServers":{"nested":{"command":"run-nested"}}}} suffix"#;
        std::fs::write(directory.join("000001.log"), payload).unwrap();
        let result = migrate_from_candidates(std::slice::from_ref(&directory));
        assert_eq!(result.imported_count, 1);
        assert_eq!(result.servers["nested"]["command"], "run-nested");
        let expected_source = directory.join("000001.log").to_string_lossy().to_string();
        assert_eq!(
            result.source_path.as_deref(),
            Some(expected_source.as_str())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_migration_without_candidates_returns_empty_result() {
        let root = temp_root("legacy-empty");
        let result = migrate_from_candidates(&[root.join("missing")]);
        assert_eq!(result.total_count, 0);
        assert_eq!(result.imported_count, 0);
        assert_eq!(result.skipped_count, 0);
        assert!(result.servers.is_empty());
        assert!(result.source_path.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
