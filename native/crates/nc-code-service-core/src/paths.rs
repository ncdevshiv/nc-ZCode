//! App data paths — mirror of `packages/services/src/paths.ts` / `settingService.ts`.
//!
//! - `homeDir`: `nc_code_DESKTOP_HOME_DIR` → `HOME` → `USERPROFILE` (matches the TS
//!   desktop override used to isolate dev instances).
//! - `dataBaseDir`: `nc_code_DATA_BASE_DIR` → home (the persisted `dataBaseDir`
//!   setting is applied by the setting service on startup).
//! - `~/.zcode/v2` is the settings/config dir; `{dataBaseDir}/.zcode` holds device data.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    home_dir: PathBuf,
    data_base_dir: PathBuf,
}

impl AppPaths {
    pub fn new(home_dir: impl Into<PathBuf>, data_base_dir: impl Into<PathBuf>) -> Self {
        Self {
            home_dir: home_dir.into(),
            data_base_dir: data_base_dir.into(),
        }
    }

    pub fn from_env() -> Self {
        let home = read_env("ZCODE_DESKTOP_HOME_DIR")
            .or_else(|| read_env("HOME"))
            .or_else(|| read_env("USERPROFILE"))
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let data_base = read_env("ZCODE_DATA_BASE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.clone());
        Self::new(home, data_base)
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    pub fn data_base_dir(&self) -> &Path {
        &self.data_base_dir
    }

    pub fn with_data_base_dir(&self, data_base_dir: impl Into<PathBuf>) -> Self {
        Self {
            home_dir: self.home_dir.clone(),
            data_base_dir: data_base_dir.into(),
        }
    }

    /// `~/.zcode`
    pub fn nc_code_data_root(&self) -> PathBuf {
        self.data_base_dir.join(".zcode")
    }

    /// `~/.zcode/v2` — settings/config dir.
    pub fn app_config_dir(&self) -> PathBuf {
        self.nc_code_data_root().join("v2")
    }

    /// `~/.zcode/v2` — kept as a named method for parity with TS `getSettingsDir`.
    pub fn settings_dir(&self) -> PathBuf {
        self.home_dir.join(".zcode").join("v2")
    }

    pub fn settings_file(&self) -> PathBuf {
        self.settings_dir().join("setting.json")
    }

    pub fn conversation_workspace_dir(&self) -> PathBuf {
        self.nc_code_data_root().join("workspace").join("default")
    }

    pub fn tasks_index_database_path(&self) -> PathBuf {
        self.app_config_dir().join("tasks-index.sqlite")
    }

    pub fn feedback_root_dir(&self) -> PathBuf {
        self.nc_code_data_root().join("feedback")
    }

    pub fn git_checkpoint_index_root_dir(&self) -> PathBuf {
        self.nc_code_data_root().join("git-checkpoint-index")
    }

    /// `sha256(workspaceIdentity?.trim() || workspacePath)` first 12 hex chars.
    pub fn workspace_hash(&self, workspace_path: &str, workspace_identity: Option<&str>) -> String {
        let key = workspace_identity
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(workspace_path);
        let digest = Sha256::digest(key.as_bytes());
        digest[..6]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn read_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_follow_the_typescript_layout() {
        let paths = AppPaths::new("/home/z", "/data");
        assert_eq!(
            paths.settings_file(),
            Path::new("/home/z/.zcode/v2/setting.json")
        );
        assert_eq!(paths.app_config_dir(), Path::new("/data/.zcode/v2"));
        assert_eq!(
            paths.conversation_workspace_dir(),
            Path::new("/data/.zcode/workspace/default")
        );
    }

    #[test]
    fn workspace_hash_prefers_identity_and_truncates_to_twelve_hex() {
        let paths = AppPaths::new("/h", "/d");
        let by_path = paths.workspace_hash("G:/work", None);
        assert_eq!(by_path.len(), 12);
        assert_eq!(by_path, paths.workspace_hash("G:/work", Some("   ")));
        assert_ne!(by_path, paths.workspace_hash("G:/work", Some("identity")));
    }
}
