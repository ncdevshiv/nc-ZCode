//! Subagent storage path resolution —— the Rust counterpart of TS `subagents/subagentStorage.ts`.
//!
//! The identity/storage root is read uniformly from `storage.dir` in `~/.zcode/cli/config.json` (default `~/.zcode`),
//! and `~` is expanded with homeDir (options first, otherwise HOME/USERPROFILE).

use std::path::{Path, PathBuf};

use nc_code_service_core::AppPaths;

const HOME_PREFIX: &str = "~/";

/// resolveUserHomeDir: options.homeDir first, otherwise HOME/USERPROFILE.
pub fn resolve_user_home_dir(paths: &AppPaths) -> PathBuf {
    paths.home_dir().to_path_buf()
}

fn read_user_cli_config(paths: &AppPaths) -> serde_json::Value {
    let path = paths
        .home_dir()
        .join(".zcode")
        .join("cli")
        .join("config.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// resolveZCodeStorageRoot: config.storage.dir (default ~/.zcode) plus ~ expansion.
pub fn resolve_nc_code_storage_root(paths: &AppPaths) -> PathBuf {
    let config = read_user_cli_config(paths);
    let storage_dir = config
        .get("storage")
        .and_then(|storage| storage.get("dir"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .unwrap_or("~/.zcode");
    resolve_config_path(paths, storage_dir)
}

/// resolveConfigPath: `~/` expands to home; a relative path is resolved against the current directory (TS resolve(expanded)).
pub fn resolve_config_path(paths: &AppPaths, raw: &str) -> PathBuf {
    let expanded = if let Some(rest) = raw.strip_prefix(HOME_PREFIX) {
        paths.home_dir().join(rest)
    } else {
        PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    }
}

/// resolveUserSubagentRoot：<storageRoot>/agents。
pub fn resolve_user_subagent_root(paths: &AppPaths) -> PathBuf {
    resolve_nc_code_storage_root(paths).join("agents")
}

/// resolveWorkspaceSubagentRoot：<workspace>/.zcode/agents。
pub fn resolve_workspace_subagent_root(workspace_path: &str) -> PathBuf {
    Path::new(workspace_path).join(".zcode").join("agents")
}

/// resolveSubagentStateFile：<storageRoot>/v2/agents-state.json。
pub fn resolve_subagent_state_file(paths: &AppPaths) -> PathBuf {
    resolve_nc_code_storage_root(paths)
        .join("v2")
        .join("agents-state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths() -> AppPaths {
        let root =
            std::env::temp_dir().join(format!("zcode-subagent-storage-{}", std::process::id()));
        AppPaths::new(root.join("home"), root.join("data"))
    }

    #[test]
    fn workspace_subagent_root_is_dot_nc_code_agents() {
        assert_eq!(
            resolve_workspace_subagent_root("/ws"),
            PathBuf::from("/ws/.zcode/agents")
        );
    }

    #[test]
    fn storage_root_defaults_to_home_dot_zcode() {
        let paths = temp_paths();
        let root = resolve_nc_code_storage_root(&paths);
        // No config → default ~/.zcode.
        assert!(root.ends_with(".zcode"));
        assert_eq!(
            resolve_subagent_state_file(&paths),
            root.join("v2").join("agents-state.json")
        );
        assert_eq!(resolve_user_subagent_root(&paths), root.join("agents"));
    }
}
