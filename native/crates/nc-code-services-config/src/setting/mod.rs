//! `setting` channel — durable `AppSettings` (mirror of `settingService.ts`).
//!
//! Read path: parse with retries → quarantine corrupt files → legacy migration →
//! schema validation (invalid ⇒ defaults) → fill defaults/strip unknown keys.
//! Write path: serialize all mutations, merge the normalized patch into the latest
//! persisted settings, retain legacy rollback fields, atomic write.

pub mod legacy;
pub mod normalize;
pub mod schema;

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::atomic::{atomic_write_text, read_text_if_exists};
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::setting::legacy::{
    incomplete_legacy_team_connections, migrate_legacy, needs_legacy_migration,
    retain_legacy_fields,
};

const PARSE_RETRY_DELAY_MS: u64 = 300;
const PARSE_RETRY_COUNT: usize = 3;
const MAX_RECENT_PROJECTS: usize = 10;
const DEFAULT_PROJECT_NAME: &str = "ZCodeProject";
pub const DATA_BASE_DIR_FORBIDDEN_WINDOWS_INSTALL_DIR_CODE: &str =
    "DATA_BASE_DIR_FORBIDDEN_WINDOWS_INSTALL_DIR";

pub struct SettingService {
    paths: RwLock<AppPaths>,
    lock: Mutex<()>,
}

impl SettingService {
    pub fn new(paths: AppPaths) -> Self {
        Self {
            paths: RwLock::new(paths),
            lock: Mutex::new(()),
        }
    }

    pub fn paths(&self) -> AppPaths {
        self.paths
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub async fn get(&self) -> Result<Value, ServiceError> {
        let _guard = self.lock.lock().await;
        let (settings, needs_persist) = self.read_settings().await;
        if !needs_persist {
            return Ok(settings);
        }
        let (latest, latest_needs_persist) = self.read_settings().await;
        if latest_needs_persist {
            self.write_settings(&latest, false).await?;
        }
        Ok(latest)
    }

    pub async fn update(
        &self,
        patch: &Value,
        expected_account_settings: Option<&Value>,
    ) -> Result<(), ServiceError> {
        let patch_object = patch
            .as_object()
            .ok_or_else(|| ServiceError::invalid_params("patch must be an object"))?;
        let normalized_patch = normalize::normalize_patch(patch_object);
        let cleared_keys = normalized_patch.cleared.clone();
        let patch_value = Value::Object(normalized_patch.values);
        schema::schema()
            .validate_patch(&patch_value)
            .map_err(ServiceError::invalid_params)?;
        let patch_value = schema::schema().normalize_patch(&patch_value);

        let _guard = self.lock.lock().await;
        let current = self.read_settings().await.0;
        if let Some(expected) = expected_account_settings {
            if !account_selection_matches(&current, expected) {
                return Err(ServiceError::new("Account connection settings changed"));
            }
        }
        let mut merged = top_level_merge(&current, &patch_value);
        if let Some(merged_object) = merged.as_object_mut() {
            for key in &cleared_keys {
                merged_object.remove(key);
            }
        }
        dedupe_recent_projects(&mut merged);
        let merged = schema::schema().normalize_full(&merged);
        let commit_account_selection =
            patch_object.contains_key("providerFamilyConnectionSelections");
        self.write_settings(&merged, commit_account_selection).await
    }

    pub async fn update_data_base_dir(&self, new_dir: Option<String>) -> Result<(), ServiceError> {
        let current_base = self.paths().data_base_dir().to_path_buf();
        let target = new_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.paths().home_dir().to_path_buf());
        validate_data_base_dir_target(&target)?;
        if current_base != target {
            copy_v2_directory(&current_base, &target).await?;
        }
        let patch = match new_dir {
            Some(dir) if !dir.trim().is_empty() => {
                serde_json::json!({ "dataBaseDir": dir.trim() })
            }
            _ => serde_json::json!({}),
        };
        self.update(&patch, None).await
    }

    pub async fn ensure_default_project(&self, user_home_dir: &str) -> Result<Value, ServiceError> {
        let path = Path::new(user_home_dir).join(DEFAULT_PROJECT_NAME);
        let existed_before = path.exists();
        tokio::fs::create_dir_all(&path).await?;
        Ok(serde_json::json!({
            "path": path.to_string_lossy(),
            "created": !existed_before,
        }))
    }

    async fn read_settings(&self) -> (Value, bool) {
        let file = self.paths().settings_file();
        let Some(raw) = self.read_raw(&file).await else {
            return (schema::schema().defaults(), false);
        };
        let needs_persist = needs_migration_persist(&raw);
        let migrated = migrate_legacy(&raw);
        if schema::schema().validate_full(&migrated).is_err() {
            // TS logs and returns defaults; it does not quarantine a schema-invalid file.
            return (schema::schema().defaults(), false);
        }
        (schema::schema().normalize_full(&migrated), needs_persist)
    }

    async fn read_raw(&self, file: &Path) -> Option<Value> {
        let text = match read_text_if_exists(file).await {
            Ok(Some(text)) => text,
            Ok(None) => return None,
            Err(_) => return None,
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => Some(value),
            Err(_) => {
                for _ in 0..PARSE_RETRY_COUNT {
                    tokio::time::sleep(std::time::Duration::from_millis(PARSE_RETRY_DELAY_MS))
                        .await;
                    if let Ok(Some(next)) = read_text_if_exists(file).await {
                        if let Ok(value) = serde_json::from_str::<Value>(&next) {
                            return Some(value);
                        }
                    }
                }
                quarantine_corrupt(file).await;
                None
            }
        }
    }

    async fn write_settings(
        &self,
        settings: &Value,
        commit_account_selection: bool,
    ) -> Result<(), ServiceError> {
        let file = self.paths().settings_file();
        let existing = match read_text_if_exists(&file).await {
            Ok(Some(text)) => serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
            _ => Value::Null,
        };
        let mut persisted = retain_legacy_fields(&existing);
        if let Some(object) = settings.as_object() {
            for (key, value) in object {
                persisted.insert(key.clone(), value.clone());
            }
        }
        if !commit_account_selection && !incomplete_legacy_team_connections(&existing).is_empty() {
            persisted.remove("providerFamilyConnectionSelections");
        }
        let contents = serde_json::to_string_pretty(&Value::Object(persisted))?;
        atomic_write_text(&file, &contents).await?;
        Ok(())
    }
}

#[async_trait]
impl Channel for SettingService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "get" => Ok(WireValue::Json(self.get().await?)),
            "update" => {
                let patch = args.required_json(0, "patch")?;
                let expected = args.json(1)?;
                self.update(&patch, expected.as_ref()).await?;
                Ok(WireValue::Undefined)
            }
            "updateDataBaseDir" => {
                let new_dir = args.string(0)?;
                self.update_data_base_dir(new_dir).await?;
                Ok(WireValue::Undefined)
            }
            "ensureDefaultProject" => {
                let homedir = args.required_string(0, "homedir")?;
                Ok(WireValue::Json(
                    self.ensure_default_project(&homedir).await?,
                ))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

fn top_level_merge(current: &Value, patch: &Value) -> Value {
    let mut merged = current.as_object().cloned().unwrap_or_default();
    if let Some(patch) = patch.as_object() {
        for (key, value) in patch {
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

fn dedupe_recent_projects(settings: &mut Value) {
    let Some(projects) = settings
        .get_mut("recentProjects")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let mut seen = std::collections::HashSet::new();
    projects.retain(|project| match project.as_str() {
        Some(path) => seen.insert(path.to_string()),
        None => true,
    });
    projects.truncate(MAX_RECENT_PROJECTS);
}

fn account_selection_matches(current: &Value, expected: &Value) -> bool {
    let current_domain = current
        .get("providerFamilyDomain")
        .cloned()
        .unwrap_or(Value::Null);
    let expected_domain = expected
        .get("providerFamilyDomain")
        .cloned()
        .unwrap_or(Value::Null);
    if current_domain != expected_domain {
        return false;
    }
    let current_selections = current
        .get("providerFamilyConnectionSelections")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let expected_selections = expected
        .get("providerFamilyConnectionSelections")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    current_selections == expected_selections
}

fn needs_migration_persist(raw: &Value) -> bool {
    let Some(object) = raw.as_object() else {
        return false;
    };
    let legacy_ready =
        needs_legacy_migration(raw) && incomplete_legacy_team_connections(raw).is_empty();
    let close_to_tray_missing =
        object.get("closeToTrayOnWindowsMigrationInitialized") != Some(&Value::Bool(true));
    let reasoning_missing =
        object.get("messageStreamShowReasoningMigrationInitialized") != Some(&Value::Bool(true));
    legacy_ready || close_to_tray_missing || reasoning_missing
}

async fn quarantine_corrupt(file: &Path) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let backup = PathBuf::from(format!("{}.corrupt-{timestamp}", file.to_string_lossy()));
    let _ = tokio::fs::rename(file, backup).await;
}

fn validate_data_base_dir_target(target: &Path) -> Result<(), ServiceError> {
    if !cfg!(windows) {
        return Ok(());
    }
    for forbidden in windows_forbidden_install_dirs() {
        if is_equal_or_inside(target, &forbidden) {
            return Err(ServiceError::new(format!(
                "{DATA_BASE_DIR_FORBIDDEN_WINDOWS_INSTALL_DIR_CODE}: {}",
                forbidden.display()
            ))
            .with_code(DATA_BASE_DIR_FORBIDDEN_WINDOWS_INSTALL_DIR_CODE));
        }
    }
    Ok(())
}

fn windows_forbidden_install_dirs() -> Vec<PathBuf> {
    let read = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let mut candidates = Vec::new();
    if let Some(dir) = read("ZCODE_WINDOWS_APP_INSTALL_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    for base in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(dir) = read(base) {
            candidates.push(PathBuf::from(dir).join("ZCode"));
        }
    }
    if let Some(local) = read("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Programs").join("ZCode"));
    }
    candidates
}

fn is_equal_or_inside(path: &Path, root: &Path) -> bool {
    let normalize = |value: &Path| {
        value
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    let path = normalize(path);
    let root = normalize(root);
    !root.is_empty() && (path == root || path.starts_with(&format!("{root}\\")))
}

async fn copy_v2_directory(old_base: &Path, new_base: &Path) -> Result<(), ServiceError> {
    let source = old_base.join(".zcode").join("v2");
    let destination = new_base.join(".zcode").join("v2");
    copy_directory(&source, &destination).await
}

async fn copy_directory(source: &Path, destination: &Path) -> Result<(), ServiceError> {
    if !source.exists() {
        return Ok(());
    }
    tokio::fs::create_dir_all(destination).await?;
    let mut entries = tokio::fs::read_dir(source).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "setting.json" || name.starts_with("setting.json.") {
            continue;
        }
        let file_type = entry.file_type().await?;
        if file_type.is_symlink() {
            continue;
        }
        let from = entry.path();
        let to = destination.join(&name);
        if file_type.is_dir() {
            Box::pin(copy_directory(&from, &to)).await?;
        } else if !to.exists() {
            tokio::fs::copy(&from, &to).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_paths(tag: &str) -> (AppPaths, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-setting-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("dir");
        let home = root.join("home");
        let data = root.join("data");
        (AppPaths::new(&home, &data), root)
    }

    #[tokio::test]
    async fn missing_file_returns_defaults_without_writing() {
        let (paths, root) = temp_paths("defaults");
        let service = SettingService::new(paths.clone());
        let settings = service.get().await.expect("get");
        assert_eq!(settings["locale"], json!("zh-CN"));
        assert!(
            !paths.settings_file().is_file(),
            "a missing setting.json must not be created by a read"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn existing_file_without_migration_flags_is_persisted_on_read() {
        let (paths, root) = temp_paths("migration-flags");
        std::fs::create_dir_all(paths.settings_dir()).expect("dir");
        std::fs::write(paths.settings_file(), "{}").expect("write");
        let service = SettingService::new(paths.clone());
        let settings = service.get().await.expect("get");
        assert_eq!(settings["locale"], json!("zh-CN"));
        let persisted: Value =
            serde_json::from_str(&std::fs::read_to_string(paths.settings_file()).expect("read"))
                .expect("json");
        assert_eq!(
            persisted["closeToTrayOnWindowsMigrationInitialized"],
            json!(true)
        );
        assert_eq!(
            persisted["messageStreamShowReasoningMigrationInitialized"],
            json!(true)
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn update_merges_serializes_and_normalizes_patch() {
        let (paths, root) = temp_paths("update");
        let service = SettingService::new(paths.clone());
        let _ = service.get().await.expect("get");
        service
            .update(&json!({"locale": "en-US"}), None)
            .await
            .expect("update");
        let settings = service.get().await.expect("get");
        assert_eq!(settings["locale"], json!("en-US"));
        assert_eq!(settings["localePreference"], json!("en-US"));

        service
            .update(&json!({"terminalFontFamily": "  Fira Code  "}), None)
            .await
            .expect("update");
        assert_eq!(
            service.get().await.expect("get")["terminalFontFamily"],
            json!("Fira Code")
        );
        service
            .update(&json!({"terminalFontFamily": "  "}), None)
            .await
            .expect("clear");
        assert!(service
            .get()
            .await
            .expect("get")
            .get("terminalFontFamily")
            .is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn recent_projects_are_deduped_and_capped() {
        let (paths, root) = temp_paths("recent");
        let service = SettingService::new(paths);
        let projects: Vec<String> = (0..15).map(|index| format!("G:/p{index}")).collect();
        let mut patch_projects = projects.clone();
        patch_projects.push("G:/p1".to_string());
        service
            .update(&json!({"recentProjects": patch_projects}), None)
            .await
            .expect("update");
        let settings = service.get().await.expect("get");
        let recent = settings["recentProjects"].as_array().expect("array");
        assert_eq!(recent.len(), MAX_RECENT_PROJECTS);
        assert_eq!(recent[0], json!("G:/p0"));
        let unique: std::collections::HashSet<_> = recent.iter().collect();
        assert_eq!(unique.len(), recent.len());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn corrupt_file_is_quarantined_and_defaults_returned() {
        let (paths, root) = temp_paths("corrupt");
        std::fs::create_dir_all(paths.settings_dir()).expect("dir");
        std::fs::write(paths.settings_file(), "{ not json").expect("write");
        let service = SettingService::new(paths.clone());
        let settings = service.get().await.expect("get");
        assert_eq!(settings["locale"], json!("zh-CN"));
        assert!(!paths.settings_file().is_file());
        let quarantined: Vec<_> = std::fs::read_dir(paths.settings_dir())
            .expect("entries")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("setting.json.corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn expected_account_selection_guard_rejects_mismatch() {
        let (paths, root) = temp_paths("guard");
        let service = SettingService::new(paths);
        let expected = json!({
            "providerFamilyDomain": "zai",
            "providerFamilyConnectionSelections": {},
        });
        let error = service
            .update(
                &json!({"providerFamilyConnectionSelections": {"zai": {"kind": "start-plan"}}}),
                Some(&expected),
            )
            .await
            .expect_err("guard must reject");
        assert_eq!(error.message, "Account connection settings changed");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn legacy_fields_are_migrated_and_retained_for_rollback() {
        let (paths, root) = temp_paths("legacy");
        std::fs::create_dir_all(paths.settings_dir()).expect("dir");
        std::fs::write(
            paths.settings_file(),
            serde_json::to_string_pretty(&json!({
                "modelProviderFamilySelectedKeys": {
                    "zai": "coding-plan:builtin:zai-coding-plan"
                },
                "closeToTrayOnWindowsMigrationInitialized": true,
                "messageStreamShowReasoningMigrationInitialized": true
            }))
            .expect("json"),
        )
        .expect("write");
        let service = SettingService::new(paths.clone());
        let settings = service.get().await.expect("get");
        assert_eq!(
            settings["providerFamilyConnectionSelections"]["zai"]["kind"],
            json!("individual-coding-plan")
        );
        let persisted: Value =
            serde_json::from_str(&std::fs::read_to_string(paths.settings_file()).expect("read"))
                .expect("json");
        assert!(persisted.get("modelProviderFamilySelectedKeys").is_some());
        assert_eq!(
            persisted["providerFamilyConnectionSelections"]["zai"]["kind"],
            json!("individual-coding-plan")
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn ensure_default_project_reports_creation() {
        let (paths, root) = temp_paths("project");
        let service = SettingService::new(paths);
        let home = root.join("user");
        let first = service
            .ensure_default_project(&home.to_string_lossy())
            .await
            .expect("first");
        assert_eq!(first["created"], json!(true));
        let second = service
            .ensure_default_project(&home.to_string_lossy())
            .await
            .expect("second");
        assert_eq!(second["created"], json!(false));
        assert!(home.join(DEFAULT_PROJECT_NAME).is_dir());
        std::fs::remove_dir_all(root).ok();
    }
}
