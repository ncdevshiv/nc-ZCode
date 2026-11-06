//! Auto-updater integration for Tauri shell.
//!
//! Provides update checking, downloading, and installation with state events
//! matching the platform interface expectations.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum UpdateState {
    #[serde(rename = "idle")]
    Idle { enabled: bool },
    #[serde(rename = "checking")]
    Checking { enabled: bool },
    #[serde(rename = "update-available")]
    UpdateAvailable {
        enabled: bool,
        version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        release_notes: Option<serde_json::Value>,
    },
    #[serde(rename = "download-progress")]
    DownloadProgress {
        enabled: bool,
        progress: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        transferred_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        release_notes: Option<serde_json::Value>,
    },
    #[serde(rename = "update-downloaded")]
    UpdateDownloaded {
        enabled: bool,
        version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        release_notes: Option<serde_json::Value>,
    },
}

#[derive(Clone)]
pub struct UpdaterState {
    pub inner: Arc<Mutex<UpdateState>>,
}

impl Default for UpdaterState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(UpdateState::Idle { enabled: true })),
        }
    }
}

impl UpdaterState {
    pub fn get(&self) -> UpdateState {
        self.inner.lock().unwrap().clone()
    }

    pub fn set(&self, state: UpdateState) {
        *self.inner.lock().unwrap() = state;
    }
}

pub fn emit_update_state(app: &AppHandle, state: &UpdateState) {
    let _ = app.emit("zcode:update-state-changed", state);
}

#[tauri::command]
pub async fn get_update_state(app: AppHandle) -> Result<UpdateState, String> {
    let state = app.state::<UpdaterState>();
    Ok(state.get())
}

#[tauri::command]
pub async fn check_for_updates(app: AppHandle) -> Result<(), String> {
    let updater_state = app.state::<UpdaterState>();
    updater_state.set(UpdateState::Checking { enabled: true });
    emit_update_state(&app, &updater_state.get());

    #[cfg(desktop)]
    {
        use tauri_plugin_updater::UpdaterExt;
        if let Ok(updater) = app.updater() {
            match updater.check().await {
                Ok(Some(update)) => {
                    let state = UpdateState::UpdateAvailable {
                        enabled: true,
                        version: update.version.clone(),
                        channel: None,
                        release_notes: None,
                    };
                    updater_state.set(state.clone());
                    emit_update_state(&app, &state);
                    let _ = app.emit(
                        "zcode:update-check-result",
                        serde_json::json!({
                            "kind": "available",
                            "version": update.version
                        }),
                    );
                    return Ok(());
                }
                Ok(None) => {
                    let state = UpdateState::Idle { enabled: true };
                    updater_state.set(state.clone());
                    emit_update_state(&app, &state);
                    let _ = app.emit(
                        "zcode:update-check-result",
                        serde_json::json!({
                            "kind": "up-to-date",
                            "currentVersion": env!("CARGO_PKG_VERSION")
                        }),
                    );
                    return Ok(());
                }
                Err(e) => {
                    let state = UpdateState::Idle { enabled: true };
                    updater_state.set(state.clone());
                    emit_update_state(&app, &state);
                    let _ = app.emit(
                        "zcode:update-check-result",
                        serde_json::json!({
                            "kind": "error",
                            "message": e.to_string()
                        }),
                    );
                    return Err(e.to_string());
                }
            }
        }
    }

    let state = UpdateState::Idle { enabled: true };
    updater_state.set(state.clone());
    emit_update_state(&app, &state);
    let _ = app.emit(
        "zcode:update-check-result",
        serde_json::json!({
            "kind": "up-to-date",
            "currentVersion": env!("CARGO_PKG_VERSION")
        }),
    );
    Ok(())
}

#[tauri::command]
pub async fn download_update(app: AppHandle) -> Result<(), String> {
    let updater_state = app.state::<UpdaterState>();

    #[cfg(desktop)]
    {
        use tauri_plugin_updater::UpdaterExt;
        if let Ok(updater) = app.updater() {
            if let Ok(Some(update)) = updater.check().await {
                let version = update.version.clone();
                let state = UpdateState::DownloadProgress {
                    enabled: true,
                    progress: "0%".to_string(),
                    transferred_bytes: Some(0),
                    total_bytes: None,
                    version: Some(version.clone()),
                    channel: None,
                    release_notes: None,
                };
                updater_state.set(state.clone());
                emit_update_state(&app, &state);

                match update.download_and_install(|_, _| {}, || {}).await {
                    Ok(_) => {
                        let downloaded_state = UpdateState::UpdateDownloaded {
                            enabled: true,
                            version,
                            channel: None,
                            release_notes: None,
                        };
                        updater_state.set(downloaded_state.clone());
                        emit_update_state(&app, &downloaded_state);
                        let _ = app.emit("zcode:update-ready", serde_json::json!({}));
                        return Ok(());
                    }
                    Err(e) => {
                        let state = UpdateState::Idle { enabled: true };
                        updater_state.set(state.clone());
                        emit_update_state(&app, &state);
                        return Err(e.to_string());
                    }
                }
            }
        }
    }

    Err("No update available".to_string())
}

#[tauri::command]
pub async fn cancel_update_download(_app: AppHandle) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
pub async fn quit_and_install_update(app: AppHandle) -> Result<(), String> {
    #[cfg(desktop)]
    {
        // After download_and_install, restart is handled by the update process
        let _ = app;
        return Ok(());
    }
    #[allow(unused)]
    let _ = app;
    Ok(())
}

#[tauri::command]
pub async fn skip_update_version(_app: AppHandle, _version: String) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
pub async fn acknowledge_post_update_release_notes(_app: AppHandle) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_state_default_is_idle() {
        let state = UpdaterState::default();
        match state.get() {
            UpdateState::Idle { enabled } => assert!(enabled),
            _ => panic!("expected idle state"),
        }
    }

    #[test]
    fn updater_state_set_and_get() {
        let state = UpdaterState::default();
        state.set(UpdateState::Checking { enabled: true });
        match state.get() {
            UpdateState::Checking { enabled } => assert!(enabled),
            _ => panic!("expected checking state"),
        }
    }

    #[test]
    fn updater_state_can_set_update_available() {
        let state = UpdaterState::default();
        state.set(UpdateState::UpdateAvailable {
            enabled: true,
            version: "1.2.3".to_string(),
            channel: None,
            release_notes: None,
        });
        match state.get() {
            UpdateState::UpdateAvailable { version, .. } => assert_eq!(version, "1.2.3"),
            _ => panic!("expected update-available state"),
        }
    }
}
