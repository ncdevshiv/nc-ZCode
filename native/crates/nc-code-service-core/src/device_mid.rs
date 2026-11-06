//! Device MID (machine/device id) state (port of `packages/services/src/device/deviceMid.ts`).
//!
//! Stores a stable `deviceMid` in `telemetry-state.json` under the app config dir (or provided home).
//! Uses a lock file to avoid concurrent initialization races across processes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

const LOCK_RETRY_DELAY_MS: u64 = 10;
const LOCK_RETRY_COUNT: usize = 200;
const LOCK_STALE_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceState {
    pub device_mid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceStateLockOwner {
    pid: u32,
    created_at: u64,
}

pub fn resolve_device_state_file(app_config_dir: &Path, home_dir: Option<&Path>) -> PathBuf {
    if let Some(home) = home_dir {
        home.join(".zcode").join("v2").join("telemetry-state.json")
    } else {
        app_config_dir.join("telemetry-state.json")
    }
}

pub fn resolve_device_state_lock_file(app_config_dir: &Path, home_dir: Option<&Path>) -> PathBuf {
    if let Some(home) = home_dir {
        home.join(".zcode").join("v2").join("telemetry-state.lock")
    } else {
        app_config_dir.join("telemetry-state.lock")
    }
}

async fn read_device_state(path: &Path) -> DeviceState {
    match fs::read_to_string(path).await {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => DeviceState::default(),
    }
}

async fn write_device_state(path: &Path, state: &DeviceState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let content = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".into());
    let mut f = fs::File::create(path).await?;
    f.write_all(content.as_bytes()).await?;
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn is_process_alive(_pid: u32) -> bool {
    // Conservative: without platform APIs, avoid stealing recent locks.
    // Staleness is primarily determined by mtime in `remove_stale_lock_if_needed`.
    true
}

async fn read_lock_owner(lock_file: &Path) -> Option<DeviceStateLockOwner> {
    let raw = fs::read_to_string(lock_file).await.ok()?;
    serde_json::from_str(&raw).ok()
}

async fn remove_stale_lock_if_needed(lock_file: &Path) -> bool {
    let Ok(meta) = fs::metadata(lock_file).await else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let age_ms = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        .as_millis() as u64;
    if age_ms < LOCK_STALE_MS {
        if let Some(owner) = read_lock_owner(lock_file).await {
            if is_process_alive(owner.pid) {
                return false;
            }
        } else {
            return false;
        }
    }
    let _ = fs::remove_file(lock_file).await;
    true
}

pub fn create_uuid() -> String {
    // Simple UUID v4-ish without adding uuid dep; sufficient for device mid.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!(
        "{nanos:x}-{pid:x}-{:x}-{:x}",
        nanos.rotate_left(13) & 0xffff,
        nanos.rotate_left(29) & 0xffff_ffff
    )
}

pub async fn ensure_device_mid_in_locked_state(
    state: &mut DeviceState,
    state_file: &Path,
) -> Result<String, std::io::Error> {
    if let Some(ref mid) = state.device_mid {
        return Ok(mid.clone());
    }
    let mid = create_uuid();
    state.device_mid = Some(mid.clone());
    write_device_state(state_file, state).await?;
    Ok(mid)
}

pub async fn ensure_device_mid(
    app_config_dir: &Path,
    home_dir: Option<&Path>,
) -> Result<String, String> {
    let state_file = resolve_device_state_file(app_config_dir, home_dir);
    let lock_file = resolve_device_state_lock_file(app_config_dir, home_dir);
    if let Some(parent) = lock_file.parent() {
        let _ = fs::create_dir_all(parent).await;
    }

    for _attempt in 0..LOCK_RETRY_COUNT {
        // Try to create lock exclusively
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_file)
            .await
        {
            Ok(mut handle) => {
                let owner = DeviceStateLockOwner {
                    pid: std::process::id(),
                    created_at: now_ms(),
                };
                let _ = handle
                    .write_all(serde_json::to_string(&owner).unwrap_or_default().as_bytes())
                    .await;
                drop(handle);
                let mut state = read_device_state(&state_file).await;
                let result = ensure_device_mid_in_locked_state(&mut state, &state_file).await;
                let _ = fs::remove_file(&lock_file).await;
                return result.map_err(|e| e.to_string());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if remove_stale_lock_if_needed(&lock_file).await {
                    continue;
                }
                sleep(Duration::from_millis(LOCK_RETRY_DELAY_MS)).await;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("Device state lock timeout".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn ensure_device_mid_creates_and_reuses() {
        let tmp = TempDir::new().unwrap();
        let mid1 = ensure_device_mid(tmp.path(), None).await.unwrap();
        assert!(!mid1.is_empty());
        let mid2 = ensure_device_mid(tmp.path(), None).await.unwrap();
        assert_eq!(mid1, mid2);
    }
}
