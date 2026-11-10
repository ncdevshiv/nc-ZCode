//! Window tab, unread-count, active-session and runtime-settings sync for the
//! Tauri shell.
//!
//! The Electron main process tracked each window's open workspace tabs,
//! forwarded the summed unread count to the OS badge, routed the active CUA PiP
//! session, and applied immediate app-settings changes. The Tauri shell has a
//! single main window today; it records the same facts and implements the
//! runtime effects that exist here (close-to-tray on Windows).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::Deserialize;
use std::collections::BTreeMap;
use tauri::{Emitter, WebviewWindow, Window};

use crate::app_menu::MenuState;
use crate::power_save::PowerSaveManager;

const MAX_TRACKED_TABS: usize = 200;
const MAX_ACTIVE_SESSION_CHARS: usize = 255;

static EXPLICIT_QUIT: AtomicBool = AtomicBool::new(false);

/// Last fullscreen state emitted to the renderer. Electron fires
/// `enter-full-screen`/`leave-full-screen`; this shell derives the same fact from window
/// resize events and only emits on a real change (resize fires continuously).
static LAST_FULLSCREEN: AtomicBool = AtomicBool::new(false);

/// Emit `zcode:window-fullscreen` when the fullscreen state actually changed.
pub fn emit_fullscreen_if_changed(window: &Window, fullscreen: bool) {
    if LAST_FULLSCREEN.swap(fullscreen, Ordering::SeqCst) != fullscreen {
        let _ = window.emit("zcode:window-fullscreen", fullscreen);
    }
}

pub struct WindowSyncState {
    tabs: Mutex<Vec<String>>,
    active_session: Mutex<Option<String>>,
    close_to_tray: AtomicBool,
}

impl Default for WindowSyncState {
    fn default() -> Self {
        Self {
            tabs: Mutex::default(),
            active_session: Mutex::default(),
            // AppSettings defaults `closeToTrayOnWindows` to true; it only has
            // an effect on Windows.
            close_to_tray: AtomicBool::new(cfg!(windows)),
        }
    }
}

impl WindowSyncState {
    fn set_tabs(&self, paths: Vec<String>) {
        let mut guard = match self.tabs.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = paths;
    }

    fn set_active_session(&self, session_id: Option<String>) {
        let mut guard = match self.active_session.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = session_id;
    }

    pub fn close_to_tray(&self) -> bool {
        self.close_to_tray.load(Ordering::SeqCst)
    }

    fn set_close_to_tray(&self, enabled: bool) {
        self.close_to_tray.store(enabled, Ordering::SeqCst);
    }
}

/// Trim the active session id; empty and overlong values are dropped instead of
/// propagating a value the CUA PiP router would reject.
fn sanitize_active_session(session_id: Option<&str>) -> Option<String> {
    let trimmed = session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if trimmed.chars().count() > MAX_ACTIVE_SESSION_CHARS {
        return None;
    }
    Some(trimmed.to_string())
}

/// A close request hides to tray only when the setting is enabled, the platform
/// has a tray (Windows), and the user did not choose Quit.
pub fn should_hide_on_close(close_to_tray: bool, explicit_quit: bool) -> bool {
    cfg!(windows) && close_to_tray && !explicit_quit
}
pub fn explicit_quit() -> bool {
    EXPLICIT_QUIT.load(Ordering::SeqCst)
}

/// Mark the process as an explicit quit (tray Quit) so the close handler lets
/// the window exit instead of hiding it.
pub fn quit(app: &tauri::AppHandle) {
    EXPLICIT_QUIT.store(true, Ordering::SeqCst);
    app.exit(0);
}

/// Trim, drop empty/duplicate entries and cap the list. Paths are opaque
/// identity strings here; no filesystem normalization is applied.
fn sanitize_tab_paths(paths: Vec<String>) -> Vec<String> {
    let mut result = Vec::new();
    for path in paths {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        if result.iter().any(|existing| existing == trimmed) {
            continue;
        }
        result.push(trimmed.to_string());
        if result.len() >= MAX_TRACKED_TABS {
            break;
        }
    }
    result
}

/// `IPlatformService.syncWindowTabs`: replace the window's tracked tab set.
#[tauri::command]
pub fn sync_window_tabs(state: tauri::State<'_, WindowSyncState>, paths: Vec<String>) {
    state.set_tabs(sanitize_tab_paths(paths));
}

/// `IPlatformService.syncActiveTaskSession`: record the active CUA PiP session
/// for this window (CUA itself remains fail-closed until the helper is ported).
#[tauri::command]
pub fn sync_active_task_session(
    state: tauri::State<'_, WindowSyncState>,
    session_id: Option<String>,
) {
    state.set_active_session(sanitize_active_session(session_id.as_deref()));
}

/// The subset of `Partial<AppSettings>` the shell applies immediately. Unknown
/// keys are ignored; settings that only affect the renderer are persisted by the
/// renderer's setting service and need no shell action.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncAppSettingsPatch {
    #[serde(default)]
    pub close_to_tray_on_windows: Option<bool>,
    #[serde(default)]
    pub keep_awake_while_running: Option<bool>,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub shortcut_bindings: Option<BTreeMap<String, Vec<String>>>,
}

/// `IPlatformService.syncAppSettings`: apply the runtime subset.
#[tauri::command]
pub fn sync_app_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, WindowSyncState>,
    power: tauri::State<'_, PowerSaveManager>,
    menu: tauri::State<'_, MenuState>,
    patch: SyncAppSettingsPatch,
) {
    if let Some(enabled) = patch.close_to_tray_on_windows {
        state.set_close_to_tray(enabled);
    }
    if let Some(enabled) = patch.keep_awake_while_running {
        if power.is_enabled() != enabled {
            if let Err(error) = power.set_enabled(enabled) {
                // The renderer method is fire-and-forget; keep the failure
                // visible on the shell stderr instead of pretending the
                // assertion exists.
                eprintln!("[keep-awake] {error}");
            }
        }
    }
    if patch.locale.is_some() || patch.shortcut_bindings.is_some() {
        if let Err(error) = menu.apply(
            &app,
            patch.locale.as_deref(),
            patch.shortcut_bindings.as_ref(),
        ) {
            eprintln!("[app-menu] {error}");
        }
    }
}

fn badge_value(count: u64) -> Option<i64> {
    if count == 0 {
        return None;
    }
    Some(count.min(i64::MAX as u64) as i64)
}

fn apply_unread_badge(window: &WebviewWindow, count: u64) -> Result<(), String> {
    // Windows has no taskbar badge count in Tauri; Electron also skipped the
    // badge there (`syncAppUnreadBadge` supports darwin/linux only).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        return window
            .set_badge_count(badge_value(count))
            .map_err(|error| error.to_string());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (window, badge_value(count));
        Ok(())
    }
}

/// `IPlatformService.syncWindowUnreadCount`: apply the OS badge count.
#[tauri::command]
pub fn sync_window_unread_count(window: WebviewWindow, count: u64) -> Result<(), String> {
    apply_unread_badge(&window, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_tabs_and_drops_duplicates() {
        let paths = vec![
            "  /work/a  ".to_string(),
            "".to_string(),
            "   ".to_string(),
            "/work/a".to_string(),
            "/work/b".to_string(),
        ];
        assert_eq!(
            sanitize_tab_paths(paths),
            vec!["/work/a".to_string(), "/work/b".to_string()]
        );
    }

    #[test]
    fn caps_tracked_tabs() {
        let paths: Vec<String> = (0..MAX_TRACKED_TABS + 25)
            .map(|index| format!("/work/{index}"))
            .collect();
        let sanitized = sanitize_tab_paths(paths);
        assert_eq!(sanitized.len(), MAX_TRACKED_TABS);
        assert_eq!(sanitized[0], "/work/0");
        assert_eq!(
            sanitized[MAX_TRACKED_TABS - 1],
            format!("/work/{}", MAX_TRACKED_TABS - 1)
        );
    }

    #[test]
    fn badge_value_clears_zero_and_keeps_counts() {
        assert_eq!(badge_value(0), None);
        assert_eq!(badge_value(1), Some(1));
        assert_eq!(badge_value(42), Some(42));
    }

    #[test]
    fn state_replaces_the_tab_set() {
        let state = WindowSyncState::default();
        state.set_tabs(vec!["/a".to_string()]);
        state.set_tabs(vec!["/b".to_string(), "/c".to_string()]);
        let guard = state.tabs.lock().unwrap();
        assert_eq!(guard.as_slice(), ["/b".to_string(), "/c".to_string()]);
    }

    #[test]
    fn sanitizes_active_session_ids() {
        assert_eq!(sanitize_active_session(None), None);
        assert_eq!(sanitize_active_session(Some("   ")), None);
        assert_eq!(
            sanitize_active_session(Some("  session-1  ")),
            Some("session-1".to_string())
        );
        let overlong = "x".repeat(MAX_ACTIVE_SESSION_CHARS + 1);
        assert_eq!(sanitize_active_session(Some(&overlong)), None);
        let at_limit = "y".repeat(MAX_ACTIVE_SESSION_CHARS);
        assert_eq!(
            sanitize_active_session(Some(&at_limit)),
            Some(at_limit.clone())
        );
    }

    #[test]
    fn active_session_state_can_be_set_and_cleared() {
        let state = WindowSyncState::default();
        state.set_active_session(Some("session-1".to_string()));
        assert_eq!(
            state.active_session.lock().unwrap().as_deref(),
            Some("session-1")
        );
        state.set_active_session(None);
        assert!(state.active_session.lock().unwrap().is_none());
    }

    #[test]
    fn close_to_tray_defaults_to_windows_setting() {
        let state = WindowSyncState::default();
        assert_eq!(state.close_to_tray(), cfg!(windows));
        state.set_close_to_tray(false);
        assert!(!state.close_to_tray());
    }

    #[test]
    fn hides_on_close_only_when_enabled_and_not_quitting() {
        assert_eq!(
            should_hide_on_close(true, false),
            cfg!(windows),
            "close-to-tray is a Windows-only behavior"
        );
        assert!(!should_hide_on_close(true, true));
        assert!(!should_hide_on_close(false, false));
    }

    #[test]
    fn settings_patch_accepts_only_the_shell_subset() {
        let patch: SyncAppSettingsPatch = serde_json::from_str(
            r#"{"closeToTrayOnWindows":false,"keepAwakeWhileRunning":true,"locale":"en-US","shortcutBindings":{"newTask":["CmdOrCtrl+Shift+n"]},"theme":"dark"}"#,
        )
        .unwrap();
        assert_eq!(patch.close_to_tray_on_windows, Some(false));
        assert_eq!(patch.keep_awake_while_running, Some(true));
        assert_eq!(patch.locale.as_deref(), Some("en-US"));
        assert_eq!(
            patch
                .shortcut_bindings
                .as_ref()
                .and_then(|bindings| bindings.get("newTask"))
                .map(Vec::as_slice),
            Some(&["CmdOrCtrl+Shift+n".to_string()][..])
        );
        let empty: SyncAppSettingsPatch = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(empty.close_to_tray_on_windows, None);
        assert_eq!(empty.keep_awake_while_running, None);
        assert!(empty.locale.is_none());
        assert!(empty.shortcut_bindings.is_none());
    }
}
