//! NC-AG configuration: environment + optional JSON file, validated before use.
//!
//! Precedence: explicit field/env var > config file > defaults. Nothing here silently
//! invents a provider; credentials stay in the provider store.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{NcagError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigFile {
    pub workspace_root: String,
    pub data_dir: String,
    pub model: ModelSelection,
    pub budgets: Budgets,
    pub policy: Policy,
    pub api_token: String,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            workspace_root: ".".to_string(),
            data_dir: String::new(),
            model: ModelSelection::default(),
            budgets: Budgets::default(),
            policy: Policy::default(),
            api_token: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    /// Optional override of the model's context window (tokens).
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Budgets {
    pub project_usd: f64,
    pub task_tokens: u64,
    pub concurrency: usize,
    pub max_agents_per_task: u32,
    pub max_retries: u32,
    pub max_review_cycles: u32,
    pub max_task_minutes: u64,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            project_usd: 25.0,
            task_tokens: 250_000,
            concurrency: 4,
            max_agents_per_task: 3,
            max_retries: 2,
            max_review_cycles: 2,
            max_task_minutes: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Policy {
    pub require_plan_approval: bool,
    pub require_material_approval: bool,
    pub stall_after_secs: u64,
    pub heartbeat_timeout_secs: u64,
    pub lease_ttl_secs: u64,
    pub default_decision_class: String,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            require_plan_approval: false,
            require_material_approval: true,
            stall_after_secs: 300,
            heartbeat_timeout_secs: 90,
            lease_ttl_secs: 900,
            default_decision_class: "technical_recommendation".to_string(),
        }
    }
}

/// Effective runtime config.
#[derive(Debug, Clone)]
pub struct NcagConfig {
    pub workspace_root: PathBuf,
    pub data_dir: PathBuf,
    pub database_path: PathBuf,
    pub model: ModelSelection,
    pub budgets: Budgets,
    pub policy: Policy,
    pub api_token: Option<String>,
}

impl NcagConfig {
    pub fn from_env() -> Result<Self> {
        Self::load(std::env::var("NCAG_CONFIG_FILE").ok().as_deref(), |name| {
            std::env::var(name).ok()
        })
    }

    /// Testable loader: the env reader is injected so tests never touch the process env.
    pub fn load(path: Option<&str>, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut file = ConfigFile::default();
        if let Some(path) = path.filter(|value| !value.trim().is_empty()) {
            let raw = std::fs::read_to_string(path).map_err(|error| {
                NcagError::invalid(format!("cannot read NCAG_CONFIG_FILE {path}: {error}"))
            })?;
            file = serde_json::from_str(&raw).map_err(|error| {
                NcagError::invalid(format!("invalid NCAG_CONFIG_FILE {path}: {error}"))
            })?;
        }
        if let Some(value) = non_empty(env("NCAG_WORKSPACE_ROOT")) {
            file.workspace_root = value;
        }
        if let Some(value) = non_empty(env("NCAG_DATA_DIR")) {
            file.data_dir = value;
        }
        if let Some(value) = non_empty(env("NCAG_PROVIDER_ID")) {
            file.model.provider_id = value;
        }
        if let Some(value) = non_empty(env("NCAG_MODEL_ID")) {
            file.model.model_id = value;
        }
        if let Some(value) = non_empty(env("NCAG_API_TOKEN")) {
            file.api_token = value;
        }
        if let Some(value) = non_empty(env("NCAG_PROJECT_BUDGET_USD")).and_then(|v| v.parse().ok())
        {
            file.budgets.project_usd = value;
        }
        if let Some(value) = non_empty(env("NCAG_CONCURRENCY")).and_then(|v| v.parse().ok()) {
            file.budgets.concurrency = value;
        }
        let validation = validate(&file)?;
        Ok(validation)
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn validate(file: &ConfigFile) -> Result<NcagConfig> {
    let workspace_root = PathBuf::from(&file.workspace_root);
    if workspace_root.as_os_str().is_empty() {
        return Err(NcagError::invalid("workspaceRoot must not be empty"));
    }
    if file.budgets.concurrency == 0 || file.budgets.concurrency > 64 {
        return Err(NcagError::invalid(
            "budgets.concurrency must be between 1 and 64",
        ));
    }
    if file.budgets.project_usd < 0.0 {
        return Err(NcagError::invalid("budgets.projectUsd must be >= 0"));
    }
    if file.budgets.max_review_cycles > 5 {
        return Err(NcagError::invalid(
            "budgets.maxReviewCycles must be <= 5 (endless review loops are forbidden)",
        ));
    }
    let data_dir = if file.data_dir.trim().is_empty() {
        workspace_root.join(".ncag")
    } else {
        PathBuf::from(file.data_dir.trim())
    };
    let database_path = data_dir.join("ncag.sqlite");
    Ok(NcagConfig {
        workspace_root,
        data_dir,
        database_path,
        model: file.model.clone(),
        budgets: file.budgets.clone(),
        policy: file.policy.clone(),
        api_token: non_empty(Some(file.api_token.clone())),
    })
}

impl NcagConfig {
    /// Project config derived from global budgets/policy.
    pub fn project_config(&self) -> crate::types::ProjectConfig {
        crate::types::ProjectConfig {
            max_concurrent_agents: self.budgets.concurrency,
            max_agents_per_task: self.budgets.max_agents_per_task,
            max_retries: self.budgets.max_retries,
            max_review_cycles: self.budgets.max_review_cycles,
            project_budget_usd: self.budgets.project_usd,
            task_token_budget: self.budgets.task_tokens,
            require_plan_approval: self.policy.require_plan_approval,
            require_material_approval: self.policy.require_material_approval,
            stall_after_secs: self.policy.stall_after_secs,
            heartbeat_timeout_secs: self.policy.heartbeat_timeout_secs,
            lease_ttl_secs: self.policy.lease_ttl_secs,
            allowed_commands: crate::types::ProjectConfig::default().allowed_commands,
            default_decision_class: self.policy.default_decision_class.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_are_valid_and_derive_project_config() {
        let config = NcagConfig::load(None, empty_env).expect("config");
        assert_eq!(config.budgets.concurrency, 4);
        let project = config.project_config();
        assert_eq!(project.max_concurrent_agents, 4);
        assert!(config.database_path.ends_with("ncag.sqlite"));
    }

    #[test]
    fn env_overrides_and_invalid_values_fail() {
        let config = NcagConfig::load(None, |name| match name {
            "NCAG_CONCURRENCY" => Some("7".into()),
            "NCAG_PROJECT_BUDGET_USD" => Some("3.5".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.budgets.concurrency, 7);
        assert!((config.budgets.project_usd - 3.5).abs() < 1e-9);
        let error = NcagConfig::load(None, |name| match name {
            "NCAG_CONCURRENCY" => Some("0".into()),
            _ => None,
        })
        .unwrap_err();
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn config_file_is_read_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ncag.json");
        std::fs::write(
            &path,
            r#"{"workspaceRoot":".","budgets":{"concurrency":2,"maxReviewCycles":9}}"#,
        )
        .unwrap();
        let error = NcagConfig::load(Some(&path.to_string_lossy()), empty_env).unwrap_err();
        assert!(error.message.contains("maxReviewCycles"));
    }
}
