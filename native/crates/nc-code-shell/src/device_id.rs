//! Persistent device identity for the Tauri shell.
//!
//! Mirrors the Electron `ensureDesktopDeviceMidSync` behavior: the stable ID
//! lives in `{appConfigDir}/telemetry-state.json` under `deviceMid` (the same
//! file the CLI and telemetry share), so ARMS/OTLP correlation stays consistent
//! across processes. Existing values are never rewritten; a missing value is
//! generated once and merged into the current JSON object atomically.
//!
//! Any filesystem/JSON failure falls back to the freshly generated UUID instead
//! of blocking startup, matching the Electron contract.

use std::path::Path;
use std::sync::Mutex;

use serde_json::{Map, Value};

const STATE_FILE_NAME: &str = "telemetry-state.json";
const DEVICE_MID_KEY: &str = "deviceMid";

/// Managed Tauri state: caches the resolved ID so repeated synchronous lookups
/// never re-read the file and stay stable even if the disk write failed.
#[derive(Default)]
pub struct DeviceIdState {
    cached: Mutex<Option<String>>,
}

impl DeviceIdState {
    fn resolve(&self, config_dir: &Path) -> String {
        let mut guard = match self.cached.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(value) = guard.as_ref() {
            return value.clone();
        }
        let value = ensure_device_mid(config_dir, || uuid::Uuid::new_v4().to_string());
        *guard = Some(value.clone());
        value
    }
}

fn read_state(path: &Path) -> Map<String, Value> {
    let Ok(bytes) = std::fs::read(path) else {
        return Map::new();
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn write_state(path: &Path, state: &Map<String, Value>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let content =
        serde_json::to_vec_pretty(&Value::Object(state.clone())).map_err(|e| e.to_string())?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, &content).map_err(|error| error.to_string())?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows cannot rename over an existing file.
            let _ = std::fs::remove_file(path);
            std::fs::rename(&temp, path).map_err(|error| {
                let _ = std::fs::remove_file(&temp);
                error.to_string()
            })
        }
    }
}

fn ensure_device_mid(config_dir: &Path, create_id: impl FnOnce() -> String) -> String {
    let state_file = config_dir.join(STATE_FILE_NAME);
    let mut state = read_state(&state_file);
    if let Some(existing) = state
        .get(DEVICE_MID_KEY)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return existing.to_string();
    }
    let device_mid = create_id();
    state.insert(
        DEVICE_MID_KEY.to_string(),
        Value::String(device_mid.clone()),
    );
    // A failed write must not block the window: the generated ID is returned
    // and cached for this session, and the next launch retries persistence.
    let _ = write_state(&state_file, &state);
    device_mid
}

/// `IPlatformService.getDeviceId` (desktop semantics: persistent per profile).
#[tauri::command]
pub fn get_device_id(state: tauri::State<'_, DeviceIdState>) -> String {
    let paths = nc_code_service_core::AppPaths::from_env();
    state.resolve(&paths.app_config_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-device-id-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn creates_state_file_and_preserves_other_fields() {
        let dir = temp_dir("create");
        let state_file = dir.join(STATE_FILE_NAME);
        std::fs::write(
            &state_file,
            br#"{"lastDailyActiveDate":"2026-01-01","dailyActiveInFlight":true}"#,
        )
        .unwrap();
        let id = ensure_device_mid(&dir, || "device-1".to_string());
        assert_eq!(id, "device-1");
        let state = read_json(&state_file);
        assert_eq!(state[DEVICE_MID_KEY], "device-1");
        assert_eq!(state["lastDailyActiveDate"], "2026-01-01");
        assert_eq!(state["dailyActiveInFlight"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn returns_existing_id_without_generating() {
        let dir = temp_dir("existing");
        std::fs::write(dir.join(STATE_FILE_NAME), br#"{"deviceMid":"stable-id"}"#).unwrap();
        let id = ensure_device_mid(&dir, || panic!("must not generate a new id"));
        assert_eq!(id, "stable-id");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn regenerates_missing_or_empty_ids() {
        let dir = temp_dir("empty");
        std::fs::write(dir.join(STATE_FILE_NAME), br#"{"deviceMid":"   "}"#).unwrap();
        let id = ensure_device_mid(&dir, || "fresh-id".to_string());
        assert_eq!(id, "fresh-id");
        assert_eq!(
            read_json(&dir.join(STATE_FILE_NAME))[DEVICE_MID_KEY],
            "fresh-id"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovers_from_corrupt_state_file() {
        let dir = temp_dir("corrupt");
        let state_file = dir.join(STATE_FILE_NAME);
        std::fs::write(&state_file, b"not json").unwrap();
        let id = ensure_device_mid(&dir, || "recovered-id".to_string());
        assert_eq!(id, "recovered-id");
        let state = read_json(&state_file);
        assert_eq!(state[DEVICE_MID_KEY], "recovered-id");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_missing_config_directory() {
        let dir = temp_dir("missing");
        let nested = dir.join("v2");
        let id = ensure_device_mid(&nested, || "nested-id".to_string());
        assert_eq!(id, "nested-id");
        assert!(nested.join(STATE_FILE_NAME).is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generated_ids_are_valid_uuids() {
        let dir = temp_dir("uuid");
        let id = ensure_device_mid(&dir, || uuid::Uuid::new_v4().to_string());
        assert!(uuid::Uuid::parse_str(&id).is_ok());
        assert_eq!(ensure_device_mid(&dir, || "other".to_string()), id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_cache_is_stable_after_first_resolution() {
        let dir = temp_dir("cache");
        let state = DeviceIdState::default();
        let first = state.resolve(&dir);
        let second = state.resolve(&dir);
        assert_eq!(first, second);
        assert!(uuid::Uuid::parse_str(&first).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
