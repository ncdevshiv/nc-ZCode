//! Layered CLI config (`~/.zcode/cli/config.json` + workspace `.zcode/config.json`)
//! with the plugin patches the TS file-config adapter performs
//! (`apps/nc-code-cli/packages/adapters/src/config/file-config.adapter.ts`).
//!
//! The Rust host reads/writes the *same* files with the same read-modify-write shape:
//! unrelated keys keep their position, new `plugins` objects are appended, and writes
//! go through the shared atomic temp+rename helper.

use std::path::{Path, PathBuf};

use nc_code_service_core::{AppPaths, ServiceError};
use serde_json::{Map, Value};

use crate::fsutil::{read_json_if_exists, resolve_user_path, write_json_atomic};

/// Default storage dir from `DefaultRuntimeConfig.storage.dir = "~/.zcode"`.
const DEFAULT_STORAGE_DIR: &str = "~/.zcode";

#[derive(Debug, Clone, Default)]
pub struct PluginConfigSection {
    pub enabled: bool,
    pub dirs: Vec<String>,
    pub enabled_plugins: Map<String, Value>,
    pub options: Map<String, Value>,
    pub extra_known_marketplaces: Map<String, Value>,
    pub suppressed_builtins: Vec<String>,
}

/// One discovered config layer.
#[derive(Debug, Clone)]
pub struct ConfigLayer {
    pub path: PathBuf,
    pub scope: &'static str, // "user" | "workspace"
    pub raw: Value,
}

impl ConfigLayer {
    pub fn plugins(&self) -> PluginConfigSection {
        let Some(plugins) = self.raw.get("plugins").and_then(Value::as_object) else {
            return PluginConfigSection {
                enabled: true,
                ..Default::default()
            };
        };
        PluginConfigSection {
            enabled: plugins
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            dirs: plugins
                .get("dirs")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            enabled_plugins: plugins
                .get("enabledPlugins")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            options: plugins
                .get("options")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            extra_known_marketplaces: plugins
                .get("extraKnownMarketplaces")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            suppressed_builtins: plugins
                .get("suppressedBuiltins")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub fn storage_dir(&self) -> Option<String> {
        self.raw
            .get("storage")
            .and_then(|storage| storage.get("dir"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

/// The merged view the engine consumes: workspace layers override the user layer.
#[derive(Clone)]
pub struct MergedConfig {
    pub user: ConfigLayer,
    /// Innermost workspace layer first (TS merges outward→inward, innermost wins).
    pub workspace_layers: Vec<ConfigLayer>,
}

impl MergedConfig {
    pub fn enabled_plugins(&self) -> Map<String, Value> {
        let mut merged = self.user.plugins().enabled_plugins;
        for layer in self.workspace_layers.iter().rev() {
            for (id, value) in layer.plugins().enabled_plugins {
                merged.insert(id, value);
            }
        }
        merged
    }

    /// Which layer declared each plugin's enablement (TS `sources.plugins.enabled`).
    pub fn enabled_sources(&self) -> Map<String, Value> {
        let mut sources = Map::new();
        for id in self.user.plugins().enabled_plugins.keys() {
            sources.insert(id.clone(), Value::String("user".into()));
        }
        for layer in self.workspace_layers.iter().rev() {
            for id in layer.plugins().enabled_plugins.keys() {
                sources.insert(id.clone(), Value::String(layer.scope.into()));
            }
        }
        sources
    }

    pub fn plugin_options(&self, plugin_id: &str) -> Map<String, Value> {
        let mut merged = Map::new();
        for layer in std::iter::once(&self.user).chain(self.workspace_layers.iter().rev()) {
            if let Some(options) = layer
                .plugins()
                .options
                .get(plugin_id)
                .and_then(Value::as_object)
            {
                for (key, value) in options {
                    merged.insert(key.clone(), value.clone());
                }
            }
        }
        merged
    }

    pub fn option_sources(&self, plugin_id: &str) -> Map<String, Value> {
        let mut sources = Map::new();
        for layer in std::iter::once(&self.user).chain(self.workspace_layers.iter().rev()) {
            if let Some(options) = layer
                .plugins()
                .options
                .get(plugin_id)
                .and_then(Value::as_object)
            {
                for key in options.keys() {
                    sources.insert(key.clone(), Value::String(layer.scope.into()));
                }
            }
        }
        sources
    }

    pub fn suppressed_builtins(&self) -> Vec<String> {
        let mut all: Vec<String> = Vec::new();
        for layer in std::iter::once(&self.user).chain(self.workspace_layers.iter()) {
            for id in layer.plugins().suppressed_builtins {
                if !all.contains(&id) {
                    all.push(id);
                }
            }
        }
        all
    }

    pub fn extra_known_marketplaces(&self) -> Map<String, Value> {
        let mut merged = self.user.plugins().extra_known_marketplaces;
        for layer in self.workspace_layers.iter().rev() {
            for (id, declaration) in layer.plugins().extra_known_marketplaces {
                merged.insert(id, declaration);
            }
        }
        merged
    }

    pub fn plugin_dirs(&self, scope: &str) -> Vec<String> {
        if scope == "user" {
            self.user.plugins().dirs
        } else {
            self.workspace_layers
                .iter()
                .flat_map(|layer| layer.plugins().dirs)
                .collect()
        }
    }

    /// `config.plugins.enabled` after merging: the innermost declaring layer wins,
    /// defaulting to enabled (TS `DefaultConfig.plugins.enabled`).
    pub fn plugins_enabled(&self) -> bool {
        let mut enabled = self.user.plugins().enabled;
        for layer in self.workspace_layers.iter().rev() {
            enabled = layer.plugins().enabled;
        }
        enabled
    }

    /// `config.plugins.dirs` after merging: user roots first, then workspace layers
    /// outermost→innermost, de-duplicated in declaration order.
    pub fn plugin_dirs_all(&self) -> Vec<String> {
        let mut dirs: Vec<String> = Vec::new();
        let push = |items: Vec<String>, dirs: &mut Vec<String>| {
            for item in items {
                if !dirs.contains(&item) {
                    dirs.push(item);
                }
            }
        };
        push(self.user.plugins().dirs, &mut dirs);
        for layer in self.workspace_layers.iter().rev() {
            push(layer.plugins().dirs, &mut dirs);
        }
        dirs
    }

    /// Effective `storage.dir` (workspace layers win, like the TS config merge).
    pub fn storage_dir(&self) -> String {
        for layer in self.workspace_layers.iter() {
            if let Some(dir) = layer.storage_dir() {
                return dir;
            }
        }
        self.user
            .storage_dir()
            .unwrap_or_else(|| DEFAULT_STORAGE_DIR.to_string())
    }

    /// Write target for a scope (TS `resolvePluginConfigPath`): workspace writes go
    /// to `<innermost workspace>/.zcode/config.json`; user writes to the user config.
    pub fn config_path_for_scope(&self, scope: Option<&str>) -> PathBuf {
        match scope {
            Some("workspace") => self
                .workspace_layers
                .first()
                .map(|layer| layer.path.clone())
                .unwrap_or_else(|| self.user.path.clone()),
            _ => self.user.path.clone(),
        }
    }
}

pub struct ConfigFiles;

impl ConfigFiles {
    /// Load the user layer plus the workspace chain for `workspace_path`.
    pub async fn load(
        paths: &AppPaths,
        workspace_path: Option<&str>,
    ) -> Result<MergedConfig, ServiceError> {
        let user_path = default_user_config_path(paths);
        let user = load_layer(&user_path, "user").await?;
        let mut workspace_layers = Vec::new();
        if let Some(workspace) = workspace_path {
            for dir in workspace_config_chain(Path::new(workspace)) {
                let candidate = dir.join(".zcode").join("config.json");
                if candidate == user_path {
                    continue;
                }
                if candidate.exists() {
                    workspace_layers.push(load_layer(&candidate, "workspace").await?);
                }
            }
        }
        Ok(MergedConfig {
            user,
            workspace_layers,
        })
    }
}

pub fn default_user_config_path(paths: &AppPaths) -> PathBuf {
    resolve_user_path("~/.zcode/cli", paths.home_dir()).join("config.json")
}

async fn load_layer(path: &Path, scope: &'static str) -> Result<ConfigLayer, ServiceError> {
    let raw = read_json_if_exists(path)
        .await?
        .filter(Value::is_object)
        .unwrap_or(Value::Object(Map::new()));
    Ok(ConfigLayer {
        path: path.to_path_buf(),
        scope,
        raw,
    })
}

/// Workspace dirs from innermost outward (TS project config discovery).
fn workspace_config_chain(start: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = Some(start.to_path_buf());
    while let Some(dir) = current {
        dirs.push(dir.clone());
        current = dir.parent().map(|parent| parent.to_path_buf());
    }
    dirs
}

/// Apply `plugins`-level patches to a config file with read-modify-write, preserving
/// every unrelated key and its order (TS `atomicWriteJson` after `{...parsed, plugins}`).
pub async fn patch_plugins(
    path: &Path,
    patch: impl FnOnce(&mut Map<String, Value>),
) -> Result<(), ServiceError> {
    let mut root = read_json_if_exists(path)
        .await?
        .filter(|value| value.is_object())
        .unwrap_or_else(|| Value::Object(Map::new()));
    let root_map = root.as_object_mut().ok_or_else(|| {
        ServiceError::new(format!(
            "config file must be a JSON object: {}",
            path.display()
        ))
    })?;
    let mut plugins = root_map
        .get("plugins")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    patch(&mut plugins);
    root_map.insert("plugins".to_string(), Value::Object(plugins));
    write_json_atomic(path, &root).await
}

fn plugins_child<'a>(plugins: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    if !plugins.get(key).is_some_and(Value::is_object) {
        plugins.insert(key.to_string(), Value::Object(Map::new()));
    }
    plugins
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .expect("object inserted above")
}

/// `plugins.enabledPlugins[id] = enabled` (TS `updatePluginEnabledInFileConfig`).
pub async fn set_plugin_enabled(
    path: &Path,
    plugin_id: &str,
    enabled: bool,
) -> Result<(), ServiceError> {
    let id = plugin_id.to_string();
    patch_plugins(path, move |plugins| {
        plugins_child(plugins, "enabledPlugins").insert(id, Value::Bool(enabled));
    })
    .await
}

/// Only flips ids the file has not declared yet; returns the ids newly enabled
/// (TS `enablePluginsByDefaultInFileConfig`).
pub async fn enable_plugins_by_default(
    path: &Path,
    plugin_ids: &[String],
) -> Result<Vec<String>, ServiceError> {
    if plugin_ids.is_empty() {
        return Ok(Vec::new());
    }
    let existing = read_json_if_exists(path)
        .await?
        .filter(|value| value.is_object())
        .unwrap_or_else(|| Value::Object(Map::new()));
    let enabled_map = existing
        .as_object()
        .and_then(|root| root.get("plugins"))
        .and_then(Value::as_object)
        .and_then(|plugins| plugins.get("enabledPlugins"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let new_ids: Vec<String> = plugin_ids
        .iter()
        .filter(|id| !enabled_map.contains_key(id.as_str()))
        .cloned()
        .collect();
    if new_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = new_ids.clone();
    patch_plugins(path, move |plugins| {
        let child = plugins_child(plugins, "enabledPlugins");
        for id in ids {
            child.insert(id, Value::Bool(true));
        }
    })
    .await?;
    Ok(new_ids)
}

/// Merge/clear `plugins.options[id]` (TS `updatePluginOptionsInFileConfig`).
pub async fn update_plugin_options(
    path: &Path,
    plugin_id: &str,
    options: &Map<String, Value>,
    clear_keys: &[String],
) -> Result<(), ServiceError> {
    let id = plugin_id.to_string();
    let options = options.clone();
    let clear_keys = clear_keys.to_vec();
    patch_plugins(path, move |plugins| {
        let all = plugins_child(plugins, "options");
        let entry = all.entry(id).or_insert_with(|| Value::Object(Map::new()));
        let Some(map) = entry.as_object_mut() else {
            *entry = Value::Object(Map::new());
            return;
        };
        for key in &clear_keys {
            map.remove(key);
        }
        for (key, value) in options {
            map.insert(key, value);
        }
    })
    .await
}

/// Remove both enablement and options for a plugin (TS `removePluginFromFileConfig`).
pub async fn remove_plugin(path: &Path, plugin_id: &str) -> Result<(), ServiceError> {
    let id = plugin_id.to_string();
    patch_plugins(path, move |plugins| {
        if let Some(enabled) = plugins
            .get_mut("enabledPlugins")
            .and_then(Value::as_object_mut)
        {
            enabled.remove(&id);
        }
        if let Some(options) = plugins.get_mut("options").and_then(Value::as_object_mut) {
            options.remove(&id);
        }
    })
    .await
}

/// Drop only the enablement override (TS `removePluginEnabledFromFileConfig`).
pub async fn remove_plugin_enabled(path: &Path, plugin_id: &str) -> Result<(), ServiceError> {
    let id = plugin_id.to_string();
    patch_plugins(path, move |plugins| {
        if let Some(enabled) = plugins
            .get_mut("enabledPlugins")
            .and_then(Value::as_object_mut)
        {
            enabled.remove(&id);
        }
    })
    .await
}

fn suppressed_list(plugins: &mut Map<String, Value>) -> Vec<String> {
    let list = plugins
        .entry("suppressedBuiltins".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !list.is_array() {
        *list = Value::Array(Vec::new());
    }
    list.as_array_mut()
        .expect("array")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// `addSuppressedBuiltinInFileConfig` (idempotent, alias-free on the Rust host).
pub async fn add_suppressed_builtin(path: &Path, plugin_id: &str) -> Result<(), ServiceError> {
    let id = plugin_id.to_string();
    patch_plugins(path, move |plugins| {
        let current = suppressed_list(plugins);
        if !current.contains(&id) {
            let mut next = current;
            next.push(id.clone());
            plugins.insert(
                "suppressedBuiltins".to_string(),
                Value::Array(next.into_iter().map(Value::String).collect()),
            );
        }
    })
    .await
}

/// `removeSuppressedBuiltinInFileConfig`.
pub async fn remove_suppressed_builtin(path: &Path, plugin_id: &str) -> Result<(), ServiceError> {
    let id = plugin_id.to_string();
    patch_plugins(path, move |plugins| {
        let current = suppressed_list(plugins);
        let next: Vec<String> = current.into_iter().filter(|item| item != &id).collect();
        plugins.insert(
            "suppressedBuiltins".to_string(),
            Value::Array(next.into_iter().map(Value::String).collect()),
        );
    })
    .await
}
