//! The `subagents` channel —— the Rust counterpart of TS `packages/services/src/subagents/subagentsService.ts`.
//!
//! Layering: Markdown parsing/serialization, the state file, and storage paths are each their own module (markdown.rs/state.rs/storage.rs);
//! this file is the service layer: built-in agents, file agent discovery, plugin agent discovery, and the orchestration of the 8 channel members.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceError};
use serde_json::{json, Map, Value};

use super::markdown::{
    create_agent_state_id, create_plugin_agent_state_id, parse_subagent_markdown,
    serialize_subagent_markdown,
};
use super::state::{is_disabled, read_agents_state, write_agents_state};
use super::storage::{resolve_user_subagent_root, resolve_workspace_subagent_root};

const BUILT_IN_AGENT_NAMES: [&str; 2] = ["general-purpose", "Explore"];
const PLUGIN_MARKETPLACE_ID: &str = "zcode-plugins-official";

pub struct SubagentsService {
    paths: AppPaths,
}

impl SubagentsService {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        Arc::new(Self { paths })
    }

    fn state_file(&self) -> PathBuf {
        super::storage::resolve_subagent_state_file(&self.paths)
    }

    /// createBuiltInAgents: the two built-in agents, with model-selection overrides applied.
    fn built_in_agents(&self, overrides: &Value) -> Vec<Value> {
        let general_override = overrides.get("general-purpose").cloned();
        let explore_override = overrides.get("Explore").cloned();
        vec![
            built_in(
                "general-purpose",
                "General-purpose agent for researching complex questions, searching for code, and executing multi-step tasks.",
                "blue",
                &["*"],
                general_override,
            ),
            built_in(
                "Explore",
                "Read-only search agent for broad fan-out searches.",
                "cyan",
                &["Bash", "Glob", "Grep", "Read", "WebFetch", "WebSearch", "TodoWrite"],
                explore_override,
            ),
        ]
    }

    /// discoverFileAgents: .md/.markdown files (recursively) under the two roots user + workspace.
    fn discover_file_agents(
        &self,
        diagnostics: &mut Vec<Value>,
        include_user: bool,
        include_workspace: bool,
        workspace_path: &str,
    ) -> Vec<Value> {
        let mut roots: Vec<(&str, PathBuf)> = Vec::new();
        if include_user {
            roots.push(("user", resolve_user_subagent_root(&self.paths)));
        }
        if include_workspace {
            roots.push(("workspace", resolve_workspace_subagent_root(workspace_path)));
        }
        let mut agents = Vec::new();
        for (scope, root) in roots {
            for agent_path in collect_agent_markdown_paths(&root) {
                let path_text = agent_path.to_string_lossy().to_string();
                match std::fs::read_to_string(&agent_path) {
                    Ok(markdown) => {
                        let parsed = parse_subagent_markdown(&markdown, &path_text, scope);
                        if let Some(diagnostic) = parsed.diagnostic {
                            diagnostics.push(diagnostic);
                            continue;
                        }
                        if let Some(mut agent) = parsed.agent {
                            if scope == "workspace" {
                                agent["projectPath"] = json!(workspace_path);
                            }
                            agents.push(agent);
                        }
                    }
                    Err(_) => diagnostics.push(json!({
                        "code": "agent_read_failed",
                        "message": format!("Failed to read agent Markdown: {path_text}"),
                        "path": path_text,
                    })),
                }
            }
        }
        agents
    }

    /// discoverPluginAgents: scan agent .md files from the installPath of enabled plugins (the bare name serves as the runtime alias).
    fn discover_plugin_agents(
        &self,
        diagnostics: &mut Vec<Value>,
        state: &Value,
        reserved_names: &[String],
    ) -> (Vec<Value>, Vec<Value>) {
        let records = self.enabled_plugin_records();
        let mut parsed_agents: Vec<(Value, String, String)> = Vec::new();
        for record in &records {
            let install_path = PathBuf::from(record["installPath"].as_str().unwrap_or(""));
            let plugin_id = record["id"].as_str().unwrap_or("").to_string();
            let plugin_name = record["name"].as_str().unwrap_or("").to_string();
            let scope = record["scope"].as_str().unwrap_or("user");
            for agent_path in collect_plugin_agent_markdown_paths(&install_path) {
                let path_text = agent_path.to_string_lossy().to_string();
                let Ok(markdown) = std::fs::read_to_string(&agent_path) else {
                    diagnostics.push(json!({
                        "code": "agent_read_failed",
                        "message": format!("Failed to read plugin agent Markdown: {path_text}"),
                        "path": path_text,
                    }));
                    continue;
                };
                let parse_scope = if scope == "workspace" {
                    "workspace"
                } else {
                    "user"
                };
                let parsed = parse_subagent_markdown(&markdown, &path_text, parse_scope);
                if let Some(diagnostic) = parsed.diagnostic {
                    diagnostics.push(diagnostic);
                    continue;
                }
                if let Some(agent) = parsed.agent {
                    let bare_name = agent["name"].as_str().unwrap_or("").to_string();
                    let id = create_plugin_agent_state_id(&plugin_id, &bare_name);
                    let mut agent = agent;
                    agent["id"] = json!(id);
                    agent["name"] = json!(format!("{plugin_name}:{bare_name}"));
                    agent["readOnly"] = json!(true);
                    agent["source"] = json!("plugin");
                    agent["pluginId"] = json!(plugin_id.clone());
                    agent["pluginName"] = json!(plugin_name.clone());
                    if scope == "workspace" {
                        agent["projectPath"] = record["projectPath"].clone();
                    }
                    let agent = apply_plugin_agent_overrides(agent, state);
                    parsed_agents.push((agent, bare_name, plugin_name.clone()));
                }
            }
        }

        let mut reserved: HashSet<String> =
            BUILT_IN_AGENT_NAMES.iter().map(|s| s.to_string()).collect();
        for name in reserved_names {
            reserved.insert(name.clone());
        }
        let mut bare_counts: std::collections::HashMap<String, usize> = Default::default();
        for (_, bare, _) in &parsed_agents {
            *bare_counts.entry(bare.clone()).or_insert(0) += 1;
        }

        let mut profiles: Vec<Value> = Vec::new();
        let mut runtime: Vec<Value> = Vec::new();
        for (agent, bare, plugin_name) in &parsed_agents {
            profiles.push(agent.clone());
            runtime.push(agent.clone());
            let count = bare_counts.get(bare).copied().unwrap_or(0);
            if count == 1 && !reserved.contains(bare) {
                let alias_id =
                    create_plugin_agent_state_id(&format!("{}:alias", agent["id"]), bare);
                let mut alias = agent.clone();
                alias["id"] = json!(alias_id);
                alias["name"] = json!(bare);
                runtime.push(alias);
            } else if reserved.contains(bare) {
                diagnostics.push(json!({
                    "code": "agent_ambiguous_name",
                    "message": format!("Plugin agent bare name conflicts with an existing profile; use {plugin_name}:{bare}"),
                    "path": agent["path"],
                }));
            } else if count > 1 {
                diagnostics.push(json!({
                    "code": "agent_ambiguous_name",
                    "message": format!("Plugin agent bare name is ambiguous; use {plugin_name}:{bare}"),
                    "path": agent["path"],
                }));
            }
        }
        profiles.sort_by_key(|a| a["name"].as_str().unwrap_or("").to_string());
        runtime.sort_by_key(|a| a["name"].as_str().unwrap_or("").to_string());
        (profiles, runtime)
    }

    /// Enabled plugin records: entries in installed_plugins.json where enabledPlugins[id]===true and not suppressed.
    fn enabled_plugin_records(&self) -> Vec<Value> {
        let storage_root = super::storage::resolve_nc_code_storage_root(&self.paths);
        let cli_root = if storage_root
            .file_name()
            .map(|n| n == "cli")
            .unwrap_or(false)
        {
            storage_root.clone()
        } else {
            storage_root.join("cli")
        };
        let config = self.read_plugin_config();
        let enabled = config
            .get("enabledPlugins")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let suppressed: HashSet<String> = config
            .get("suppressedBuiltins")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let installed_path = cli_root.join("plugins").join("installed_plugins.json");
        let Ok(text) = std::fs::read_to_string(&installed_path) else {
            return Vec::new();
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
            return Vec::new();
        };
        parsed
            .get("plugins")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|record| {
                        record["id"].is_string()
                            && record["installPath"].is_string()
                            && record["name"].is_string()
                    })
                    .filter(|record| {
                        let id = record["id"].as_str().unwrap_or("");
                        enabled.get(id) == Some(&json!(true))
                            && !(id.ends_with(&format!("@{PLUGIN_MARKETPLACE_ID}"))
                                && suppressed.contains(id))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn read_plugin_config(&self) -> Value {
        let path = self
            .paths
            .home_dir()
            .join(".zcode")
            .join("cli")
            .join("config.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.get("plugins").cloned())
            .unwrap_or(json!({}))
    }

    fn user_agent_dir(&self) -> PathBuf {
        resolve_user_subagent_root(&self.paths)
    }

    fn workspace_agent_dir(&self, workspace_path: &str) -> Result<PathBuf, ServiceError> {
        let trimmed = workspace_path.trim();
        if trimmed.is_empty() {
            return Err(ServiceError::invalid_params(
                "Workspace path is required for workspace subagents",
            ));
        }
        Ok(resolve_workspace_subagent_root(trimmed))
    }

    fn agent_file_path(&self, dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{}.md", name.trim().to_lowercase()))
    }

    fn validate_config(&self, config: &Value) -> Result<(), ServiceError> {
        let name = config
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if name.len() < 3 || name.len() > 50 {
            return Err(ServiceError::new(
                "Name must be between 3 and 50 characters",
            ));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(ServiceError::new(
                "Name can only contain letters, numbers, and hyphens",
            ));
        }
        if config
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            return Err(ServiceError::new("Description is required"));
        }
        if config
            .get("systemPrompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            return Err(ServiceError::new("System prompt is required"));
        }
        Ok(())
    }

    fn assert_not_built_in(&self, name: &str) -> Result<(), ServiceError> {
        if BUILT_IN_AGENT_NAMES.contains(&name.trim()) {
            return Err(ServiceError::new(format!(
                "Agent name \"{name}\" is reserved by a built-in agent"
            )));
        }
        Ok(())
    }

    fn normalize_config(&self, config: &Value) -> Value {
        let mut normalized = config.as_object().cloned().unwrap_or_default();
        for key in ["name", "description", "systemPrompt"] {
            if let Some(text) = normalized.get(key).and_then(Value::as_str) {
                normalized.insert(key.to_string(), json!(text.trim()));
            }
        }
        Value::Object(normalized)
    }

    async fn set_disabled(&self, agent_id: &str, disabled: bool) -> Result<(), ServiceError> {
        let path = self.state_file();
        let mut state = read_agents_state(&path);
        let mut ids: Vec<String> = state["disabledAgentIds"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if disabled {
            if !ids.iter().any(|id| id == agent_id) {
                ids.push(agent_id.to_string());
            }
        } else {
            ids.retain(|id| id != agent_id);
        }
        ids.sort();
        state["disabledAgentIds"] = Value::Array(ids.into_iter().map(|id| json!(id)).collect());
        write_agents_state(&path, &state)
            .await
            .map_err(ServiceError::new)
    }
    /// list (subagentsService.ts:566-630): built-in + file agents + plugin agents + state (enabled/overrides).
    async fn list(&self, params: &Value) -> Result<Value, ServiceError> {
        let workspace_path = params
            .get("workspacePath")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("allRuntimeScopes");
        let include_workspace = mode == "allRuntimeScopes";
        let state = read_agents_state(&self.state_file());
        let built_ins = self.built_in_agents(&state["builtInModelSelectionOverrides"]);
        let mut diagnostics: Vec<Value> = Vec::new();
        let file_agents =
            self.discover_file_agents(&mut diagnostics, true, include_workspace, &workspace_path);
        let mut sorted_user: Vec<Value> = file_agents
            .iter()
            .filter(|a| a["scope"] == "user")
            .cloned()
            .collect();
        sorted_user.sort_by_key(|a| a["name"].as_str().unwrap_or("").to_string());
        let mut sorted_workspace: Vec<Value> = file_agents
            .iter()
            .filter(|a| a["scope"] == "workspace")
            .cloned()
            .collect();
        sorted_workspace.sort_by_key(|a| a["name"].as_str().unwrap_or("").to_string());
        let reserved: Vec<String> = built_ins
            .iter()
            .chain(sorted_user.iter())
            .chain(sorted_workspace.iter())
            .filter_map(|a| a["name"].as_str().map(str::to_string))
            .collect();
        let (profiles, runtime) = self.discover_plugin_agents(&mut diagnostics, &state, &reserved);

        let mut discovered: Vec<Value> = if mode == "settingsUserOnly" {
            let mut list = built_ins.clone();
            list.extend(sorted_user.clone());
            list
        } else {
            let mut list = built_ins.clone();
            list.extend(sorted_user.clone());
            list.extend(sorted_workspace.clone());
            list.extend(runtime);
            apply_runtime_precedence(list)
        };
        let agents = attach_enabled_state(std::mem::take(&mut discovered), &state);
        let user_agents: Vec<Value> = agents
            .iter()
            .filter(|a| a["source"] == "user")
            .cloned()
            .collect();
        let plugin_agents = attach_enabled_state(profiles, &state);
        Ok(json!({
            "agents": agents,
            "userAgents": user_agents,
            "pluginAgents": plugin_agents,
            "capability": { "userScopeAvailable": true },
            "diagnostics": diagnostics,
        }))
    }

    async fn set_enabled(&self, params: &Value) -> Result<(), ServiceError> {
        let agent_id = params.get("agentId").and_then(Value::as_str).unwrap_or("");
        let enabled = params
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.set_disabled(agent_id, !enabled).await
    }

    async fn set_built_in_model_override(&self, params: &Value) -> Result<(), ServiceError> {
        let agent_name = params
            .get("agentName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let path = self.state_file();
        let mut state = read_agents_state(&path);
        let mut overrides = state["builtInModelSelectionOverrides"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let selection = params
            .get("modelSelection")
            .filter(|s| s.is_object())
            .cloned();
        if let Some(selection) = selection {
            overrides.insert(agent_name.to_string(), selection);
        } else {
            overrides.remove(agent_name);
        }
        state["builtInModelSelectionOverrides"] = Value::Object(overrides);
        write_agents_state(&path, &state)
            .await
            .map_err(ServiceError::new)
    }

    async fn set_plugin_agent_model_override(&self, params: &Value) -> Result<(), ServiceError> {
        let agent_id = params.get("agentId").and_then(Value::as_str).unwrap_or("");
        if !agent_id.starts_with("plugin:") || agent_id.trim() != agent_id {
            return Err(ServiceError::new("Invalid plug-in Subagent identity"));
        }
        let path = self.state_file();
        let mut state = read_agents_state(&path);
        let mut overrides = state["pluginAgentModelSelectionOverrides"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let selection = params
            .get("modelSelection")
            .filter(|s| s.is_object())
            .cloned();
        if let Some(selection) = selection {
            overrides.insert(agent_id.to_string(), selection);
        } else {
            overrides.remove(agent_id);
        }
        state["pluginAgentModelSelectionOverrides"] = Value::Object(overrides);
        write_agents_state(&path, &state)
            .await
            .map_err(ServiceError::new)
    }

    async fn get_primary_user_agents_directory(&self) -> Result<Value, ServiceError> {
        let dir = self.user_agent_dir();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| ServiceError::new(format!("create {}: {error}", dir.display())))?;
        Ok(json!({ "path": dir.to_string_lossy() }))
    }

    async fn create_agent(&self, params: &Value) -> Result<Value, ServiceError> {
        let config = params.get("config").cloned().unwrap_or(Value::Null);
        self.validate_config(&config)?;
        let name = config.get("name").and_then(Value::as_str).unwrap_or("");
        self.assert_not_built_in(name)?;
        let scope = params
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let dir = if scope == "workspace" {
            self.workspace_agent_dir(
                params
                    .get("workspacePath")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )?
        } else {
            self.user_agent_dir()
        };
        let file_path = self.agent_file_path(&dir, name);
        if file_path.exists() {
            return Err(ServiceError::new(format!(
                "Agent file \"{}\" already exists",
                file_path.file_name().unwrap_or_default().to_string_lossy()
            )));
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| ServiceError::new(format!("create {}: {error}", dir.display())))?;
        let normalized = self.normalize_config(&config);
        let content = serialize_subagent_markdown(&normalized);
        let agent = self.parse_saved_agent(&content, &file_path, scope, params)?;
        tokio::fs::write(&file_path, &content)
            .await
            .map_err(|error| {
                ServiceError::new(format!("write {}: {error}", file_path.display()))
            })?;
        Ok(json!({ "agent": agent }))
    }

    async fn update_agent(&self, params: &Value) -> Result<Value, ServiceError> {
        let config = params.get("config").cloned().unwrap_or(Value::Null);
        self.validate_config(&config)?;
        let name = config.get("name").and_then(Value::as_str).unwrap_or("");
        self.assert_not_built_in(name)?;
        let scope = params
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let dir = if scope == "workspace" {
            self.workspace_agent_dir(
                params
                    .get("workspacePath")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )?
        } else {
            self.user_agent_dir()
        };
        let file_path = self.agent_file_path(&dir, name);
        let old_file_path = params.get("oldFilePath").and_then(Value::as_str);
        if let Some(old) = old_file_path {
            if old != file_path.to_string_lossy() && file_path.exists() {
                return Err(ServiceError::new(format!(
                    "Agent file \"{}\" already exists",
                    file_path.file_name().unwrap_or_default().to_string_lossy()
                )));
            }
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| ServiceError::new(format!("create {}: {error}", dir.display())))?;
        let normalized = self.normalize_config(&config);
        let content = serialize_subagent_markdown(&normalized);
        let agent = self.parse_saved_agent(&content, &file_path, scope, params)?;
        tokio::fs::write(&file_path, &content)
            .await
            .map_err(|error| {
                ServiceError::new(format!("write {}: {error}", file_path.display()))
            })?;
        // Renaming generates a new id, so the old disable record must be migrated, otherwise the agent becomes enabled again after the rename.
        if let Some(previous_id) = params.get("agentId").and_then(Value::as_str) {
            self.migrate_disabled_agent_id(previous_id, agent["id"].as_str().unwrap_or(""))
                .await?;
        }
        if let Some(old) = old_file_path {
            if old != file_path.to_string_lossy() {
                let _ = tokio::fs::remove_file(old).await;
            }
        }
        Ok(json!({ "agent": agent }))
    }

    async fn delete_agent(&self, params: &Value) -> Result<(), ServiceError> {
        if let Some(file_path) = params.get("filePath").and_then(Value::as_str) {
            let _ = tokio::fs::remove_file(file_path).await;
        }
        if let Some(agent_id) = params.get("agentId").and_then(Value::as_str) {
            self.set_disabled(agent_id, false).await?;
        }
        Ok(())
    }

    fn parse_saved_agent(
        &self,
        content: &str,
        file_path: &Path,
        scope: &str,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let path_text = file_path.to_string_lossy().to_string();
        let parsed = parse_subagent_markdown(content, &path_text, scope);
        match (parsed.agent, parsed.diagnostic) {
            (Some(mut agent), _) => {
                if scope == "workspace" {
                    agent["projectPath"] = json!(params
                        .get("workspacePath")
                        .and_then(Value::as_str)
                        .unwrap_or(""));
                }
                Ok(agent)
            }
            (None, Some(diagnostic)) => Err(ServiceError::new(
                diagnostic["message"]
                    .as_str()
                    .unwrap_or("Failed to parse saved agent")
                    .to_string(),
            )),
            (None, None) => Err(ServiceError::new(format!(
                "Failed to parse saved agent: {path_text}"
            ))),
        }
    }

    async fn migrate_disabled_agent_id(
        &self,
        previous: &str,
        next: &str,
    ) -> Result<(), ServiceError> {
        if previous == next {
            return Ok(());
        }
        let path = self.state_file();
        let mut state = read_agents_state(&path);
        let mut ids: Vec<String> = state["disabledAgentIds"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !ids.iter().any(|id| id == previous) {
            return Ok(());
        }
        ids.retain(|id| id != previous);
        if !ids.iter().any(|id| id == next) {
            ids.push(next.to_string());
        }
        ids.sort();
        state["disabledAgentIds"] = Value::Array(ids.into_iter().map(|id| json!(id)).collect());
        write_agents_state(&path, &state)
            .await
            .map_err(ServiceError::new)
    }
}

fn built_in(
    name: &str,
    description: &str,
    color: &str,
    tools: &[&str],
    override_selection: Option<Value>,
) -> Value {
    let tools: Vec<Value> = tools.iter().map(|tool| json!(tool)).collect();
    let mut agent = json!({
        "id": create_agent_state_id(name, "built-in", "built-in"),
        "name": name,
        "description": description,
        "color": color,
        "systemPrompt": "",
        "tools": tools,
        "path": format!("built-in:{name}"),
        "scope": "built-in",
        "source": "built-in",
        "enabled": true,
        "readOnly": true,
    });
    if let Some(selection) = override_selection {
        agent["modelSelection"] = selection.clone();
        agent["modelSelectionOverride"] = selection;
    }
    agent
}

fn apply_plugin_agent_overrides(mut agent: Value, state: &Value) -> Value {
    let id = agent["id"].as_str().unwrap_or("").to_string();
    let default_selection = agent.get("modelSelection").cloned();
    if let Some(default_selection) = default_selection {
        agent["defaultModelSelection"] = default_selection;
    }
    let override_selection = state
        .get("pluginAgentModelSelectionOverrides")
        .and_then(|o| o.get(&id))
        .cloned();
    if let Some(selection) = override_selection {
        agent["modelSelection"] = selection.clone();
        agent["modelSelectionOverride"] = selection;
    }
    agent
}

fn collect_agent_markdown_paths(root: &Path) -> Vec<PathBuf> {
    let mut result: Vec<PathBuf> = Vec::new();
    collect_agent_markdown_paths_into(root, &mut result);
    result.sort();
    result
}

fn collect_agent_markdown_paths_into(root: &Path, result: &mut Vec<PathBuf>) {
    let Ok(metadata) = std::fs::metadata(root) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(entry_meta) = entry.metadata() else {
            continue;
        };
        if entry_meta.is_dir() {
            collect_agent_markdown_paths_into(&path, result);
        } else if entry_meta.is_file() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name.ends_with(".md") || name.ends_with(".markdown") {
                result.push(path);
            }
        }
    }
}

const PLUGIN_MANIFEST_DIRS: [&str; 3] = [".nc-code-plugin", ".claude-plugin", ".codex-plugin"];

fn collect_plugin_agent_markdown_paths(root: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = vec![root.join("agents")];
    // readPluginManifest: try the three plugin directory conventions in order, taking the first manifest that parses.
    for dir in PLUGIN_MANIFEST_DIRS {
        let manifest_path = root.join(dir).join("plugin.json");
        let Ok(text) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
            collect_manifest_agent_roots(root, parsed.get("agents"), &mut roots);
            break;
        }
    }
    let mut result: Vec<PathBuf> = Vec::new();
    for agent_root in roots {
        collect_agent_markdown_paths_into(&agent_root, &mut result);
    }
    result.sort();
    result
}

fn collect_manifest_agent_roots(root: &Path, agents: Option<&Value>, roots: &mut Vec<PathBuf>) {
    match agents {
        Some(Value::String(relative)) => {
            if let Some(resolved) = resolve_inside(root, relative) {
                roots.push(resolved);
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(relative) = item.as_str() {
                    if let Some(resolved) = resolve_inside(root, relative) {
                        roots.push(resolved);
                    }
                }
            }
        }
        _ => {}
    }
}

fn resolve_inside(root: &Path, relative: &str) -> Option<PathBuf> {
    let stripped = relative.strip_prefix("./").unwrap_or(relative);
    let candidate = root.join(stripped);
    let resolved = candidate.canonicalize().ok()?;
    let root_resolved = root.canonicalize().ok()?;
    resolved.starts_with(&root_resolved).then_some(resolved)
}

/// attachEnabledState: only user-scope agents are affected by disabledAgentIds (built-in/plugin agents are always enabled).
fn attach_enabled_state(agents: Vec<Value>, state: &Value) -> Vec<Value> {
    agents
        .into_iter()
        .map(|mut agent| {
            if agent["scope"] == "user" {
                let id = agent["id"].as_str().unwrap_or("");
                agent["enabled"] = json!(!is_disabled(state, id));
            } else {
                agent["enabled"] = json!(true);
            }
            agent
        })
        .collect()
}

/// applyRuntimePrecedence: dedupe by name, where a later occurrence overrides an earlier one (Map semantics).
fn apply_runtime_precedence(agents: Vec<Value>) -> Vec<Value> {
    let mut by_name: Map<String, Value> = Map::new();
    for agent in agents {
        let name = agent["name"].as_str().unwrap_or("").to_string();
        by_name.insert(name, agent);
    }
    by_name.into_values().collect()
}

#[async_trait]
impl Channel for SubagentsService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let params = match args.into_iter().next() {
            Some(WireValue::Json(value)) => value,
            _ => Value::Object(Map::new()),
        };
        let to_failure = |error: ServiceError| ChannelFailure::error(error.to_string());
        match command {
            "list" => self
                .list(&params)
                .await
                .map(WireValue::Json)
                .map_err(to_failure),
            "setEnabled" => self
                .set_enabled(&params)
                .await
                .map(|()| WireValue::Undefined)
                .map_err(to_failure),
            "setBuiltInModelOverride" => self
                .set_built_in_model_override(&params)
                .await
                .map(|()| WireValue::Undefined)
                .map_err(to_failure),
            "setPluginAgentModelOverride" => self
                .set_plugin_agent_model_override(&params)
                .await
                .map(|()| WireValue::Undefined)
                .map_err(to_failure),
            "getPrimaryUserAgentsDirectory" => self
                .get_primary_user_agents_directory()
                .await
                .map(WireValue::Json)
                .map_err(to_failure),
            "createAgent" => self
                .create_agent(&params)
                .await
                .map(WireValue::Json)
                .map_err(to_failure),
            "updateAgent" => self
                .update_agent(&params)
                .await
                .map(WireValue::Json)
                .map_err(to_failure),
            "deleteAgent" => self
                .delete_agent(&params)
                .await
                .map(|()| WireValue::Undefined)
                .map_err(to_failure),
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;

    fn temp_paths(tag: &str) -> AppPaths {
        let root =
            std::env::temp_dir().join(format!("zcode-subagents-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("mkdir");
        AppPaths::new(root.join("home"), root.join("data"))
    }

    fn ctx() -> ChannelContext {
        ChannelContext::new("subagents-test", ClientMode::DesktopContinuous)
    }

    #[tokio::test]
    async fn list_returns_built_in_agents_and_create_delete_round_trips() {
        let paths = temp_paths("list");
        let service = SubagentsService::new(paths.clone());
        // The list contains the two built-in agents.
        let result = service
            .call(
                &ctx(),
                "list",
                vec![WireValue::Json(json!({"workspacePath": "/ws"}))],
                CancelToken::new(),
            )
            .await
            .expect("list");
        let WireValue::Json(value) = result else {
            panic!("list must return JSON")
        };
        let agents = value["agents"].as_array().expect("agents");
        assert!(agents.iter().any(|a| a["name"] == "general-purpose"));
        assert!(agents.iter().any(|a| a["name"] == "Explore"));

        // create → list contains the new agent → it disappears after delete.
        let config = json!({
            "name": "my-helper",
            "description": "A helper",
            "systemPrompt": "Be helpful.",
        });
        let created = service
            .call(
                &ctx(),
                "createAgent",
                vec![WireValue::Json(json!({"config": config}))],
                CancelToken::new(),
            )
            .await
            .expect("createAgent");
        let WireValue::Json(agent) = created else {
            panic!("createAgent must return JSON")
        };
        assert_eq!(agent["agent"]["name"], json!("my-helper"));

        let listed = service
            .call(
                &ctx(),
                "list",
                vec![WireValue::Json(json!({"workspacePath": "/ws"}))],
                CancelToken::new(),
            )
            .await
            .expect("list after create");
        let WireValue::Json(listed) = listed else {
            panic!("list JSON")
        };
        assert!(listed["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "my-helper"));

        let file_path = service.user_agent_dir().join("my-helper.md");
        service
            .call(
                &ctx(),
                "deleteAgent",
                vec![WireValue::Json(json!({
                    "filePath": file_path.to_string_lossy(),
                    "agentId": "user:user:my-helper",
                }))],
                CancelToken::new(),
            )
            .await
            .expect("deleteAgent");
        assert!(!file_path.exists());

        let _ = std::fs::remove_dir_all(paths.home_dir());
    }

    #[tokio::test]
    async fn create_rejects_reserved_name_and_invalid_config() {
        let paths = temp_paths("validate");
        let service = SubagentsService::new(paths);
        let reserved = service
            .call(
                &ctx(),
                "createAgent",
                vec![WireValue::Json(json!({
                    "config": {"name": "Explore", "description": "d", "systemPrompt": "s"},
                }))],
                CancelToken::new(),
            )
            .await;
        assert!(reserved.is_err(), "built-in name must be rejected");
        let invalid = service
            .call(
                &ctx(),
                "createAgent",
                vec![WireValue::Json(json!({
                    "config": {"name": "ab", "description": "d", "systemPrompt": "s"},
                }))],
                CancelToken::new(),
            )
            .await;
        assert!(invalid.is_err(), "short name must be rejected");
    }

    #[tokio::test]
    async fn set_enabled_persists_and_unknown_method_fails_closed() {
        let paths = temp_paths("enabled");
        let service = SubagentsService::new(paths);
        service
            .call(
                &ctx(),
                "setEnabled",
                vec![WireValue::Json(json!({
                    "agentId": "user:user:my-helper", "enabled": false,
                }))],
                CancelToken::new(),
            )
            .await
            .expect("setEnabled");
        let state = read_agents_state(&service.state_file());
        assert!(is_disabled(&state, "user:user:my-helper"));
        let unknown = service
            .call(&ctx(), "bogus", vec![], CancelToken::new())
            .await
            .expect_err("unknown method");
        assert!(unknown.to_string().contains("Method not found"));
    }
}
