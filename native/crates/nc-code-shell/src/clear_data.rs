//! Clear-all-data flow for the Tauri shell.
//!
//! Mirrors the Electron command: confirm with a native warning dialog, delete
//! the app config dir (`~/.zcode/v2`: settings, credentials, logs), clear the
//! webview's browsing data (cookies/localStorage), then restart the app.
//!
//! Deletion is best-effort but not silent: if the directory cannot be removed
//! (for example a locked file), the shell shows the concrete error and does not
//! restart, instead of pretending the wipe succeeded.

use std::path::Path;

use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

struct ClearDataLabels {
    title: &'static str,
    message: &'static str,
    confirm: &'static str,
    cancel: &'static str,
    error_title: &'static str,
}

fn labels(locale: &str) -> ClearDataLabels {
    if locale == "zh-CN" {
        ClearDataLabels {
            title: "清除所有数据",
            message: "将删除 ~/.zcode/v2（配置、凭据、日志）并清除浏览器数据。操作不可恢复，清除后应用将自动重启。",
            confirm: "清除所有数据",
            cancel: "取消",
            error_title: "清除数据失败",
        }
    } else {
        ClearDataLabels {
            title: "Clear All Data",
            message: "This deletes ~/.zcode/v2 (config, credentials, logs) and clears browser data. This cannot be undone; the app will restart.",
            confirm: "Clear All Data",
            cancel: "Cancel",
            error_title: "Failed to clear data",
        }
    }
}

/// Remove the config directory (or a stray file at that path). A missing path
/// is already clear and counts as success.
pub fn remove_app_config_dir(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            std::fs::remove_dir_all(path).map_err(|error| error.to_string())
        }
        Ok(_) => std::fs::remove_file(path).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// Ask for confirmation, then clear data and restart. The dialog callback runs
/// off the main thread, so the blocking deletion cannot freeze the UI.
pub fn confirm_and_clear(app: &AppHandle) {
    let locale = app.state::<crate::app_menu::MenuState>().locale();
    let labels = labels(&locale);
    let app = app.clone();
    app.dialog()
        .message(labels.message)
        .title(labels.title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            labels.confirm.to_string(),
            labels.cancel.to_string(),
        ))
        .show(move |confirmed| {
            if !confirmed {
                return;
            }
            let paths = nc_code_service_core::AppPaths::from_env();
            if let Err(error) = remove_app_config_dir(&paths.app_config_dir()) {
                app.dialog()
                    .message(format!("{}: {error}", labels.error_title))
                    .title(labels.title)
                    .kind(MessageDialogKind::Error)
                    .show(|_| {});
                return;
            }
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.clear_all_browsing_data();
            }
            app.restart();
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-clear-data-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn removes_nested_config_directory() {
        let root = temp_dir("nested");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("setting.json"), b"{}").unwrap();
        std::fs::write(root.join("sub").join("log.txt"), b"log").unwrap();
        remove_app_config_dir(&root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn missing_path_is_already_clear() {
        let root = temp_dir("missing");
        let target = root.join("does-not-exist");
        remove_app_config_dir(&target).unwrap();
        assert!(!target.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn removes_a_file_at_the_config_path() {
        let root = temp_dir("file");
        let target = root.join("setting.json");
        std::fs::write(&target, b"{}").unwrap();
        remove_app_config_dir(&target).unwrap();
        assert!(!target.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn labels_cover_both_locales() {
        assert_eq!(labels("zh-CN").title, "清除所有数据");
        assert_eq!(labels("zh-CN").cancel, "取消");
        assert_eq!(labels("en-US").title, "Clear All Data");
        assert_eq!(labels("en-US").cancel, "Cancel");
        assert_eq!(labels("ja-JP").title, "Clear All Data");
    }
}
