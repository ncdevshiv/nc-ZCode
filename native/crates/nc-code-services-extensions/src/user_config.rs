//! The user-level CLI config (`~/.zcode/cli/config.json`) shared by `commands`, `skills`,
//! `subagents` and `hooks`.
//!
//! Mirrors `readUserCliConfig`/`writeUserCliConfig` in `packages/services/src/commands/commandsService.ts`,
//! `readCliConfigFile`/`writeSkillEnabledMap` in `packages/services/src/skills/skillsService.ts` and
//! `readUserCliConfig` in `packages/services/src/subagents/subagentStorage.ts`: one file, one
//! reader, one writer — the same keys the TypeScript backend writes and nothing else.
//!
//! Serialization is `JSON.stringify(config, null, 2) + "\n"`, which TypeScript uses on every
//! write of this file; `serde_json` with `preserve_order` keeps the parsed key order, so an
//! unrelated section (e.g. `plugins`) is not reordered when only `skills` changes.
//! The writes go through `nc_code_service_core::atomic` because the CLI process and this host
//! read the same file; the TypeScript writers are plain `writeFile`, which can expose a
//! half-written config to a concurrent reader.
//!
//! A missing, unreadable or non-object file is `{}` (the TypeScript behaviour): the config is
//! advisory for these channels, and a broken file must not make the settings page unusable.

use std::path::{Path, PathBuf};

use nc_code_service_core::atomic::{atomic_write_text, read_text_if_exists};
use nc_code_service_core::{AppPaths, ServiceError};
use serde_json::{Map, Value};

/// `~/` prefix accepted inside config-provided paths (`HOME_PREFIX` in TypeScript).
const HOME_PREFIX: &str = "~/";
/// Fallback `storage.dir` when the config does not set one.
const DEFAULT_STORAGE_DIR: &str = "~/.zcode";

pub fn cli_config_path(paths: &AppPaths) -> PathBuf {
    paths
        .home_dir()
        .join(".zcode")
        .join("cli")
        .join("config.json")
}

/// `<home>/.zcode/cli` — the directory TypeScript creates before writing the file.
pub async fn read_cli_config(paths: &AppPaths) -> Map<String, Value> {
    read_object_file(&cli_config_path(paths)).await
}

/// Read a JSON object file; anything unreadable or non-object is `{}`.
pub async fn read_object_file(path: &Path) -> Map<String, Value> {
    let Ok(Some(text)) = read_text_if_exists(path).await else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

/// Read a JSON object file, distinguishing "absent" from "invalid".
pub async fn read_optional_object_file(path: &Path) -> Option<Map<String, Value>> {
    let Ok(Some(text)) = read_text_if_exists(path).await else {
        return None;
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => Some(object),
        _ => None,
    }
}

pub async fn write_cli_config(
    paths: &AppPaths,
    config: &Map<String, Value>,
) -> Result<(), ServiceError> {
    write_json_file(&cli_config_path(paths), config).await
}

/// Serialize exactly like `JSON.stringify(value, null, 2) + "\n"`.
pub async fn write_json_file(path: &Path, config: &Map<String, Value>) -> Result<(), ServiceError> {
    let contents = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(config.clone()))?
    );
    atomic_write_text(path, &contents).await?;
    Ok(())
}

/// `readStorageDirFromConfig`: `storage.dir` when it is a non-blank string, else `~/.zcode`.
pub fn read_storage_dir(config: &Map<String, Value>) -> String {
    let storage = match config.get("storage").and_then(Value::as_object) {
        Some(storage) => storage,
        None => return DEFAULT_STORAGE_DIR.to_string(),
    };
    match storage.get("dir").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => value.to_string(),
        _ => DEFAULT_STORAGE_DIR.to_string(),
    }
}

/// `resolveConfigPath`: `~/x` joins the home dir; absolute paths pass through; anything else
/// resolves against the process working directory.
pub fn resolve_config_path(paths: &AppPaths, raw: &str) -> PathBuf {
    let expanded = match raw.strip_prefix(HOME_PREFIX) {
        Some(rest) => paths.home_dir().join(rest),
        None => PathBuf::from(raw),
    };
    if expanded.is_absolute() {
        return absolute_lexical(&expanded);
    }
    absolute_lexical(
        &std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded),
    )
}

/// `<home>/.zcode` unless `storage.dir` points elsewhere; matches `resolveZCodeStorageRoot`.
pub async fn resolve_storage_root(paths: &AppPaths) -> PathBuf {
    resolve_config_path(paths, &read_storage_dir(&read_cli_config(paths).await))
}

/// `resolveCliStorageRoot`: append `cli` unless the storage root already is the `cli` dir.
pub fn resolve_cli_storage_root(_paths: &AppPaths, storage_root: &Path) -> PathBuf {
    if storage_root.file_name().and_then(|name| name.to_str()) == Some("cli") {
        return storage_root.to_path_buf();
    }
    storage_root.join("cli")
}

/// `resolvePluginStorageRoot`: `<cli storage root>/plugins`.
pub fn resolve_plugin_storage_root(paths: &AppPaths, storage_root: &Path) -> PathBuf {
    resolve_cli_storage_root(paths, storage_root).join("plugins")
}

/// String array reader (`readStringArray`): non-arrays and non-strings are dropped.
pub fn read_string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Boolean record reader (`readBooleanRecord`): only boolean values survive.
pub fn read_boolean_record(value: Option<&Value>) -> Map<String, Value> {
    let mut result = Map::new();
    if let Some(object) = value.and_then(Value::as_object) {
        for (key, enabled) in object {
            if enabled.is_boolean() {
                result.insert(key.clone(), enabled.clone());
            }
        }
    }
    result
}

/// Lexical absolute path, the Rust counterpart of `path.resolve` for paths that already exist
/// as strings: no symlink resolution, only `.`/`..` cleanup. Keeping this lexical matches the
/// TypeScript services, which resolve hook/command paths lexically and reserve `realpath` for
/// the deletion guards.
pub fn absolute_lexical(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(root: &Path) -> AppPaths {
        AppPaths::new(root.join("home"), root.join("data"))
    }

    #[tokio::test]
    async fn missing_and_invalid_config_read_as_empty() {
        let root =
            std::env::temp_dir().join(format!("zcode-ext-cfg-missing-{}", std::process::id()));
        let app_paths = paths(&root);
        assert!(read_cli_config(&app_paths).await.is_empty());
        tokio::fs::create_dir_all(cli_config_path(&app_paths).parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            cli_config_path(&app_paths)
                .parent()
                .unwrap()
                .join("config.json"),
            "[1,2]",
        )
        .await
        .unwrap();
        assert!(read_cli_config(&app_paths).await.is_empty());
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn writes_keep_unrelated_sections_and_end_with_a_newline() {
        let root = std::env::temp_dir().join(format!("zcode-ext-cfg-write-{}", std::process::id()));
        let app_paths = paths(&root);
        let mut config = Map::new();
        config.insert("storage".to_string(), json!({"dir": "~/.zcode"}));
        config.insert("plugins".to_string(), json!({"enabled": false}));
        write_cli_config(&app_paths, &config).await.unwrap();
        let text = tokio::fs::read_to_string(cli_config_path(&app_paths))
            .await
            .unwrap();
        assert!(text.ends_with("}\n"), "{text}");
        assert!(text.contains("\"plugins\""), "{text}");
        assert_eq!(read_cli_config(&app_paths).await, config);
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[test]
    fn storage_dir_defaults_and_appends_cli() {
        let root = PathBuf::from("/tmp/nc-code-ext-unit");
        let app_paths = paths(&root);
        let mut config = Map::new();
        assert_eq!(read_storage_dir(&config), "~/.zcode");
        config.insert("storage".to_string(), json!({"dir": "~/custom"}));
        // On Windows AppPaths prepends the current drive letter (G:\tmp\...): assert the tail per component, do not pin the POSIX literal.
        let resolved =
            resolve_cli_storage_root(&app_paths, &resolve_config_path(&app_paths, "~/custom"));
        assert!(
            resolved.ends_with(PathBuf::from("home/custom/cli")),
            "{resolved:?}"
        );
        config.insert("storage".to_string(), json!({"dir": "/abs/cli"}));
        let plugin_root =
            resolve_plugin_storage_root(&app_paths, &resolve_config_path(&app_paths, "/abs/cli"));
        // Also assert per component: Windows turns the root path into an absolute path with a drive letter (G:\abs\cli\plugins).
        assert!(
            plugin_root.ends_with(PathBuf::from("/abs/cli/plugins")),
            "{plugin_root:?}"
        );
    }
}
