//! `onboarding-record` channel — device-scoped onboarding answers.
//!
//! File: `{dataBaseDir}/.zcode/v2/onboarding-record.json`. Corrupt content is treated
//! as "never recorded" (TS semantics); each user (including anonymous `null`) has at
//! most one entry, and anonymous answers are claimed on login.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::atomic::{atomic_write_text, read_text_if_exists};
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const OCCUPATIONS: [&str; 14] = [
    "office",
    "developer",
    "independent",
    "infrastructure",
    "product",
    "design",
    "student",
    "creator",
    "operations",
    "marketing",
    "finance",
    "accounting",
    "legal",
    "other",
];

/// Signed-in user identity shared by onboarding/oauth/credential services.
#[derive(Clone, Default)]
pub struct AccountState {
    user_id: Arc<RwLock<Option<String>>>,
}

impl AccountState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn user_id(&self) -> Option<String> {
        self.user_id
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn set_user_id(&self, user_id: Option<String>) {
        *self
            .user_id
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = user_id;
    }
}

pub struct OnboardingRecordService {
    paths: AppPaths,
    account: AccountState,
    write_lock: Mutex<()>,
}

impl OnboardingRecordService {
    pub fn new(paths: AppPaths, account: AccountState) -> Self {
        Self {
            paths,
            account,
            write_lock: Mutex::new(()),
        }
    }

    fn record_file(&self) -> std::path::PathBuf {
        self.paths.app_config_dir().join("onboarding-record.json")
    }

    async fn read_file(&self) -> Option<Value> {
        let text = read_text_if_exists(&self.record_file())
            .await
            .ok()
            .flatten()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        validate_file(&value).then_some(value)
    }

    async fn write_file(&self, file: &Value) -> Result<(), ServiceError> {
        let contents = serde_json::to_string_pretty(file)?;
        atomic_write_text(&self.record_file(), &contents).await?;
        Ok(())
    }

    async fn append_record(&self, device_mid: &str, entry: &Value) -> Result<(), ServiceError> {
        if device_mid.trim().is_empty() {
            return Err(ServiceError::invalid_params(
                "deviceMid must be a non-empty string",
            ));
        }
        let user_id = self.account.user_id();
        let _guard = self.write_lock.lock().await;
        let mut file = self
            .read_file()
            .await
            .unwrap_or_else(|| json!({ "version": 1, "deviceMid": device_mid, "entries": [] }));
        let record = json!({
            "userId": user_id,
            "occupation": entry.get("occupation").cloned().unwrap_or(Value::Null),
            "interfaceMode": entry.get("interfaceMode").cloned().unwrap_or(Value::Null),
            "memoryEnabled": entry.get("memoryEnabled").cloned().unwrap_or(Value::Null),
            "proactiveSuggestionsEnabled": entry.get("proactiveSuggestionsEnabled").cloned().unwrap_or(Value::Null),
            "completedAt": entry.get("completedAt").cloned().unwrap_or(Value::Null),
            "uploadState": "pending",
        });
        if !validate_entry(&record) {
            return Err(ServiceError::invalid_params(
                "onboarding record entry is invalid",
            ));
        }
        let entries = file["entries"].as_array_mut().expect("validated array");
        match entries.iter_mut().find(|item| {
            item.get("userId").and_then(Value::as_str) == user_id.as_deref()
                && item.get("userId").map(Value::is_null).unwrap_or(false) == user_id.is_none()
        }) {
            Some(existing) => *existing = record,
            None => entries.push(record),
        }
        self.write_file(&file).await
    }

    async fn claim_anonymous_record(&self) -> Result<(), ServiceError> {
        let Some(user_id) = self.account.user_id() else {
            return Ok(());
        };
        let _guard = self.write_lock.lock().await;
        let Some(mut file) = self.read_file().await else {
            return Ok(());
        };
        let entries = file["entries"].as_array_mut().expect("validated array");
        let already_claimed = entries
            .iter()
            .any(|entry| entry.get("userId").and_then(Value::as_str) == Some(user_id.as_str()));
        if already_claimed {
            return Ok(());
        }
        if let Some(anonymous) = entries
            .iter_mut()
            .rev()
            .find(|entry| entry.get("userId").map(Value::is_null).unwrap_or(false))
        {
            anonymous["userId"] = Value::String(user_id);
        }
        self.write_file(&file).await
    }

    async fn should_onboard(&self) -> bool {
        let user_id = self.account.user_id();
        match self.read_file().await {
            None => true,
            Some(file) => !file["entries"].as_array().is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry.get("userId").and_then(Value::as_str) == user_id.as_deref()
                        && entry.get("userId").map(Value::is_null).unwrap_or(false)
                            == user_id.is_none()
                })
            }),
        }
    }

    async fn latest_entry(&self) -> Option<Value> {
        let user_id = self.account.user_id();
        let file = self.read_file().await?;
        file["entries"]
            .as_array()?
            .iter()
            .rev()
            .find(|entry| {
                entry.get("userId").map(Value::is_null).unwrap_or(false) == user_id.is_none()
                    && entry.get("userId").and_then(Value::as_str) == user_id.as_deref()
            })
            .cloned()
    }

    async fn sync_settings_from_record(&self) -> Option<Value> {
        let entry = self.latest_entry().await?;
        let occupation = entry
            .get("occupation")
            .and_then(Value::as_str)
            .filter(|value| OCCUPATIONS.contains(value))
            .unwrap_or("other");
        Some(json!({
            "onboardingOccupation": occupation,
            "proactiveSuggestionsEnabled": entry
                .get("proactiveSuggestionsEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            "memoryEnabled": entry
                .get("memoryEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }))
    }

    async fn update_record_preferences(&self, patch: &Value) -> Result<(), ServiceError> {
        let user_id = self.account.user_id();
        let _guard = self.write_lock.lock().await;
        let Some(mut file) = self.read_file().await else {
            return Ok(());
        };
        let entries = file["entries"].as_array_mut().expect("validated array");
        let Some(index) = entries.iter().rposition(|entry| {
            entry.get("userId").map(Value::is_null).unwrap_or(false) == user_id.is_none()
                && entry.get("userId").and_then(Value::as_str) == user_id.as_deref()
        }) else {
            return Ok(());
        };
        let mut updated = entries[index].clone();
        for key in ["memoryEnabled", "proactiveSuggestionsEnabled"] {
            if let Some(value) = patch.get(key) {
                if value.is_boolean() || value.is_null() {
                    updated[key] = value.clone();
                }
            }
        }
        if !validate_entry(&updated) {
            return Err(ServiceError::invalid_params(
                "onboarding record update is invalid",
            ));
        }
        entries[index] = updated;
        self.write_file(&file).await
    }

    async fn clear_records(&self) -> Result<(), ServiceError> {
        let _guard = self.write_lock.lock().await;
        match tokio::fs::remove_file(self.record_file()).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[async_trait]
impl Channel for OnboardingRecordService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "appendRecord" => {
                let device_mid = args.required_string(0, "deviceMid")?;
                let entry = args.required_json(1, "entry")?;
                self.append_record(&device_mid, &entry).await?;
                Ok(WireValue::Undefined)
            }
            "claimAnonymousRecord" => {
                self.claim_anonymous_record().await?;
                Ok(WireValue::Undefined)
            }
            "shouldOnboard" => Ok(WireValue::Json(json!(self.should_onboard().await))),
            "getLatestEntry" => Ok(WireValue::Json(
                self.latest_entry().await.unwrap_or(Value::Null),
            )),
            "syncSettingsFromRecord" => Ok(WireValue::Json(
                self.sync_settings_from_record()
                    .await
                    .unwrap_or(Value::Null),
            )),
            "updateRecordPreferences" => {
                let patch = args.required_json(0, "patch")?;
                self.update_record_preferences(&patch).await?;
                Ok(WireValue::Undefined)
            }
            "getRecords" => Ok(WireValue::Json(
                self.read_file().await.unwrap_or(Value::Null),
            )),
            "clearRecords" => {
                self.clear_records().await?;
                Ok(WireValue::Undefined)
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

fn validate_file(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("version").and_then(Value::as_u64) == Some(1)
        && object
            .get("deviceMid")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
        && object
            .get("entries")
            .and_then(Value::as_array)
            .is_some_and(|entries| entries.iter().all(validate_entry))
}

fn validate_entry(entry: &Value) -> bool {
    let Some(object) = entry.as_object() else {
        return false;
    };
    let user_id_ok = match object.get("userId") {
        Some(Value::Null) => true,
        Some(Value::String(value)) => !value.trim().is_empty(),
        _ => false,
    };
    let occupation_ok = match object.get("occupation") {
        Some(Value::Null) => true,
        Some(Value::String(value)) => !value.trim().is_empty(),
        _ => false,
    };
    let interface_ok = matches!(
        object.get("interfaceMode"),
        Some(Value::Null) | Some(Value::String(_))
    ) && object
        .get("interfaceMode")
        .and_then(Value::as_str)
        .map(|mode| matches!(mode, "coding" | "office"))
        .unwrap_or(true);
    let booleans_ok = ["memoryEnabled", "proactiveSuggestionsEnabled"]
        .iter()
        .all(|key| matches!(object.get(*key), Some(Value::Null) | Some(Value::Bool(_))));
    let completed_ok = object
        .get("completedAt")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    user_id_ok
        && occupation_ok
        && interface_ok
        && booleans_ok
        && completed_ok
        && object.get("uploadState").and_then(Value::as_str) == Some("pending")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> (AppPaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-onboarding-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        (AppPaths::new(root.join("home"), root.join("data")), root)
    }

    fn entry(occupation: &str) -> Value {
        json!({
            "occupation": occupation,
            "interfaceMode": "coding",
            "memoryEnabled": true,
            "proactiveSuggestionsEnabled": null,
            "completedAt": "2026-10-28T00:00:00.000Z",
        })
    }

    #[tokio::test]
    async fn anonymous_record_lifecycle_and_settings_sync() {
        let (paths, root) = temp_paths("anon");
        let service = OnboardingRecordService::new(paths, AccountState::new());
        assert!(service.should_onboard().await);
        service
            .append_record("device-1", &entry("developer"))
            .await
            .expect("append");
        assert!(!service.should_onboard().await);
        assert_eq!(
            service.latest_entry().await.expect("entry")["userId"],
            Value::Null
        );
        assert_eq!(
            service.sync_settings_from_record().await.expect("sync")["onboardingOccupation"],
            json!("developer")
        );
        // Rewriting the same anonymous entry replaces rather than appends.
        service
            .append_record("device-1", &entry("student"))
            .await
            .expect("append");
        let file = service.read_file().await.expect("file");
        assert_eq!(file["entries"].as_array().expect("entries").len(), 1);
        assert_eq!(file["entries"][0]["occupation"], json!("student"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn claim_and_sync_for_signed_in_user() {
        let (paths, root) = temp_paths("claim");
        let account = AccountState::new();
        let service = OnboardingRecordService::new(paths, account.clone());
        service
            .append_record("device-1", &entry("developer"))
            .await
            .expect("append");
        account.set_user_id(Some("user-1".to_string()));
        assert!(service.should_onboard().await);
        service.claim_anonymous_record().await.expect("claim");
        assert!(!service.should_onboard().await);
        assert_eq!(
            service.latest_entry().await.expect("entry")["userId"],
            json!("user-1")
        );
        service
            .update_record_preferences(&json!({"memoryEnabled": false}))
            .await
            .expect("update");
        assert_eq!(
            service.latest_entry().await.expect("entry")["memoryEnabled"],
            json!(false)
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn corrupt_entries_are_invalid() {
        assert!(!validate_entry(&json!({"userId": null})));
        assert!(!validate_entry(&json!({
            "userId": null,
            "occupation": "x",
            "interfaceMode": "nope",
            "memoryEnabled": null,
            "proactiveSuggestionsEnabled": null,
            "completedAt": "t",
            "uploadState": "pending",
        })));
    }
}
