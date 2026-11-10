//! Native application menu for the Tauri shell.
//!
//! Mirrors the Electron File/Edit/View/Window/Help surface: File actions emit
//! the same renderer commands, Edit uses native predefined items, View owns
//! fullscreen/zoom, and Help routes to shell actions. Labels are localized
//! (zh-CN/en-US) and menu accelerators honor `shortcutBindings` from
//! `AppSettings`, rebuilding on `syncAppSettings`.
//!
//! `menu.new-tab` emits `zcode:new-tab`, the shell surface that was previously
//! missing for `IPlatformService.onNewTab`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

const DEFAULT_LOCALE: &str = "zh-CN";
const SETTING_FILE_LOCALE_KEY: &str = "locale";
const EMBEDDED_APP_CONFIG: &str = include_str!("../../../../config/default.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    DesktopCommand(&'static str),
    NewTab,
    ZoomIn,
    ZoomOut,
    ResetZoom,
    ExportLogs,
    ToggleDevTools,
    ClearAllData,
    ResourceManager,
    About,
    Feedback,
}

/// Map a menu item id to the action the shell performs.
pub fn action_for_menu_id(id: &str) -> Option<MenuAction> {
    Some(match id {
        "menu.new-task" => MenuAction::DesktopCommand("newTask"),
        "menu.new-tab" => MenuAction::NewTab,
        "menu.open-workspace" => MenuAction::DesktopCommand("openWorkspace"),
        "menu.close-window" => MenuAction::DesktopCommand("closeActiveContext"),
        "menu.zoom-in" => MenuAction::ZoomIn,
        "menu.zoom-out" => MenuAction::ZoomOut,
        "menu.reset-zoom" => MenuAction::ResetZoom,
        "menu.export-logs" => MenuAction::ExportLogs,
        "menu.toggle-devtools" => MenuAction::ToggleDevTools,
        "menu.clear-data" => MenuAction::ClearAllData,
        "menu.resource-manager" => MenuAction::ResourceManager,
        "menu.about" => MenuAction::About,
        "menu.feedback" => MenuAction::Feedback,
        _ => return None,
    })
}

pub struct MenuState {
    locale: Mutex<String>,
    shortcut_bindings: Mutex<BTreeMap<String, Vec<String>>>,
}

impl MenuState {
    /// Read the persisted locale (`~/.zcode/v2/setting.json`) so the first menu
    /// matches the renderer's language; fall back to the shared default.
    pub fn from_settings() -> Self {
        let paths = nc_code_service_core::AppPaths::from_env();
        let locale = std::fs::read(paths.settings_file())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| {
                value
                    .get(SETTING_FILE_LOCALE_KEY)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .map(|value| normalize_locale(Some(&value)).to_string())
            .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
        Self {
            locale: Mutex::new(locale),
            shortcut_bindings: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn locale(&self) -> String {
        match self.locale.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn bindings(&self) -> BTreeMap<String, Vec<String>> {
        match self.shortcut_bindings.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Apply locale/shortcut changes and rebuild the native menu. Returns an
    /// explicit error when the menu cannot be rebuilt.
    pub fn apply(
        &self,
        app: &AppHandle,
        locale: Option<&str>,
        shortcut_bindings: Option<&BTreeMap<String, Vec<String>>>,
    ) -> Result<(), String> {
        if let Some(locale) = locale {
            let normalized = normalize_locale(Some(locale)).to_string();
            match self.locale.lock() {
                Ok(mut guard) => *guard = normalized,
                Err(poisoned) => *poisoned.into_inner() = normalized,
            }
        }
        if let Some(bindings) = shortcut_bindings {
            match self.shortcut_bindings.lock() {
                Ok(mut guard) => *guard = bindings.clone(),
                Err(poisoned) => *poisoned.into_inner() = bindings.clone(),
            }
        }
        rebuild(app, self)
    }
}

fn normalize_locale(value: Option<&str>) -> &'static str {
    match value.map(str::trim) {
        Some(locale) if locale.to_ascii_lowercase().starts_with("zh") => "zh-CN",
        Some(locale) if !locale.is_empty() => "en-US",
        _ => DEFAULT_LOCALE,
    }
}

/// Normalize an Electron-style binding (`CmdOrCtrl+Shift+K`) to a Tauri
/// accelerator. Only the menu-relevant subset is accepted; anything else
/// returns `None` so the item keeps working without a shortcut instead of
/// crashing the menu build.
pub fn normalize_accelerator(binding: &str) -> Option<String> {
    let mut modifiers: Vec<String> = Vec::new();
    let mut key: Option<String> = None;
    for part in binding.split('+').map(str::trim) {
        if part.is_empty() {
            return None;
        }
        match part.to_ascii_lowercase().as_str() {
            "cmdorctrl" | "commandorcontrol" => modifiers.push("CmdOrCtrl".to_string()),
            "cmd" | "command" | "super" => modifiers.push("Cmd".to_string()),
            "ctrl" | "control" => modifiers.push("Ctrl".to_string()),
            "alt" | "option" => modifiers.push("Alt".to_string()),
            "shift" => modifiers.push("Shift".to_string()),
            _ if key.is_none() => {
                key = Some(normalize_accelerator_key(part)?);
            }
            // AltGr and unknown modifiers are not representable in Tauri.
            _ => return None,
        }
    }
    let key = key?;
    if modifiers.is_empty() {
        return None;
    }
    // Canonical modifier order (matches the shared binding serializer).
    let order = ["CmdOrCtrl", "Cmd", "Ctrl", "Alt", "Shift"];
    modifiers.sort_by_key(|modifier| {
        order
            .iter()
            .position(|candidate| candidate == modifier)
            .unwrap_or(usize::MAX)
    });
    modifiers.dedup();
    modifiers.push(key);
    Some(modifiers.join("+"))
}

fn normalize_accelerator_key(raw: &str) -> Option<String> {
    if raw.chars().count() == 1 {
        let character = raw.chars().next()?;
        if character.is_ascii_alphabetic() {
            return Some(character.to_ascii_uppercase().to_string());
        }
        if character.is_ascii_digit() {
            return Some(character.to_string());
        }
        return match character {
            '=' => Some("Plus".to_string()),
            '-' => Some("-".to_string()),
            _ => None,
        };
    }
    let named = match raw.to_ascii_lowercase().as_str() {
        "plus" => "Plus",
        "minus" => "-",
        "enter" | "return" => "Enter",
        "tab" => "Tab",
        "space" => "Space",
        "escape" | "esc" => "Escape",
        "backspace" => "Backspace",
        "delete" | "del" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "up" | "arrowup" => "ArrowUp",
        "down" | "arrowdown" => "ArrowDown",
        "left" | "arrowleft" => "ArrowLeft",
        "right" | "arrowright" => "ArrowRight",
        "f1" => "F1",
        "f2" => "F2",
        "f3" => "F3",
        "f4" => "F4",
        "f5" => "F5",
        "f6" => "F6",
        "f7" => "F7",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "f11" => "F11",
        "f12" => "F12",
        _ => return None,
    };
    Some(named.to_string())
}

/// Resolve a command's accelerator: an explicit override wins (empty list
/// disables it); invalid overrides fall back to the default, matching Electron's
/// `resolveMenuAccelerator`.
pub fn resolve_accelerator(
    bindings: &BTreeMap<String, Vec<String>>,
    command_id: &str,
    fallback: &str,
) -> Option<String> {
    match bindings.get(command_id) {
        Some(overrides) if overrides.is_empty() => None,
        Some(overrides) => overrides
            .iter()
            .find_map(|binding| normalize_accelerator(binding))
            .or_else(|| normalize_accelerator(fallback)),
        None => normalize_accelerator(fallback),
    }
}

struct Labels {
    app: &'static str,
    file: &'static str,
    edit: &'static str,
    view: &'static str,
    window: &'static str,
    help: &'static str,
    new_task: &'static str,
    new_tab: &'static str,
    open_workspace: &'static str,
    close_window: &'static str,
    undo: &'static str,
    redo: &'static str,
    cut: &'static str,
    copy: &'static str,
    paste: &'static str,
    select_all: &'static str,
    fullscreen: &'static str,
    zoom_in: &'static str,
    zoom_out: &'static str,
    actual_size: &'static str,
    minimize: &'static str,
    about: &'static str,
    feedback: &'static str,
    export_logs: &'static str,
    clear_data: &'static str,
    resource_manager: &'static str,
    devtools: &'static str,
    quit: &'static str,
}

fn labels(locale: &str) -> Labels {
    if locale == "zh-CN" {
        Labels {
            app: "NC-Code",
            file: "文件",
            edit: "编辑",
            view: "视图",
            window: "窗口",
            help: "帮助",
            new_task: "新建任务",
            new_tab: "新建标签页",
            open_workspace: "打开工作区",
            close_window: "关闭窗口",
            undo: "撤销",
            redo: "重做",
            cut: "剪切",
            copy: "复制",
            paste: "粘贴",
            select_all: "全选",
            fullscreen: "切换全屏",
            zoom_in: "放大",
            zoom_out: "缩小",
            actual_size: "实际大小",
            minimize: "最小化",
            about: "关于 NC-Code",
            feedback: "问题上报",
            export_logs: "导出日志",
            clear_data: "清除所有数据",
            resource_manager: "资源管理器",
            devtools: "切换开发者工具",
            quit: "退出 NC-Code",
        }
    } else {
        Labels {
            app: "NC-Code",
            file: "File",
            edit: "Edit",
            view: "View",
            window: "Window",
            help: "Help",
            new_task: "New task",
            new_tab: "New tab",
            open_workspace: "Open workspace",
            close_window: "Close window",
            undo: "Undo",
            redo: "Redo",
            cut: "Cut",
            copy: "Copy",
            paste: "Paste",
            select_all: "Select all",
            fullscreen: "Toggle full screen",
            zoom_in: "Zoom in",
            zoom_out: "Zoom out",
            actual_size: "Actual size",
            minimize: "Minimize",
            about: "About NC-Code",
            feedback: "Feedback",
            export_logs: "Export logs",
            clear_data: "Clear all data",
            resource_manager: "Resource manager",
            devtools: "Toggle developer tools",
            quit: "Quit NC-Code",
        }
    }
}

fn accelerator_item(
    app: &AppHandle,
    id: &str,
    label: &str,
    accelerator: Option<String>,
) -> tauri::Result<tauri::menu::MenuItem<tauri::Wry>> {
    let mut builder = MenuItemBuilder::with_id(id, label);
    if let Some(accelerator) = accelerator {
        builder = builder.accelerator(accelerator);
    }
    builder.build(app)
}

fn build_menu(app: &AppHandle, state: &MenuState) -> tauri::Result<Menu<tauri::Wry>> {
    let locale = state.locale();
    let labels = labels(&locale);
    let bindings = state.bindings();

    let mut menu = MenuBuilder::new(app);
    if cfg!(target_os = "macos") {
        let app_menu = SubmenuBuilder::new(app, labels.app)
            .item(&PredefinedMenuItem::about(app, Some(labels.about), None)?)
            .separator()
            .item(&PredefinedMenuItem::quit(app, Some(labels.quit))?)
            .build()?;
        menu = menu.item(&app_menu);
    }

    let file = SubmenuBuilder::new(app, labels.file)
        .item(&accelerator_item(
            app,
            "menu.new-task",
            labels.new_task,
            resolve_accelerator(&bindings, "newTask", "CmdOrCtrl+N"),
        )?)
        .item(&accelerator_item(
            app,
            "menu.new-tab",
            labels.new_tab,
            resolve_accelerator(&bindings, "newTab", "CmdOrCtrl+T"),
        )?)
        .item(&accelerator_item(
            app,
            "menu.open-workspace",
            labels.open_workspace,
            resolve_accelerator(&bindings, "openWorkspace", "CmdOrCtrl+O"),
        )?)
        .separator()
        .item(&accelerator_item(
            app,
            "menu.close-window",
            labels.close_window,
            resolve_accelerator(&bindings, "closeActiveContext", "CmdOrCtrl+W"),
        )?)
        .build()?;

    let edit = SubmenuBuilder::new(app, labels.edit)
        .item(&PredefinedMenuItem::undo(app, Some(labels.undo))?)
        .item(&PredefinedMenuItem::redo(app, Some(labels.redo))?)
        .separator()
        .item(&PredefinedMenuItem::cut(app, Some(labels.cut))?)
        .item(&PredefinedMenuItem::copy(app, Some(labels.copy))?)
        .item(&PredefinedMenuItem::paste(app, Some(labels.paste))?)
        .separator()
        .item(&PredefinedMenuItem::select_all(
            app,
            Some(labels.select_all),
        )?)
        .build()?;

    let view = SubmenuBuilder::new(app, labels.view)
        .item(&PredefinedMenuItem::fullscreen(
            app,
            Some(labels.fullscreen),
        )?)
        .separator()
        .item(&accelerator_item(
            app,
            "menu.zoom-in",
            labels.zoom_in,
            resolve_accelerator(&bindings, "zoomIn", "CmdOrCtrl+="),
        )?)
        .item(&accelerator_item(
            app,
            "menu.zoom-out",
            labels.zoom_out,
            resolve_accelerator(&bindings, "zoomOut", "CmdOrCtrl+-"),
        )?)
        .item(&accelerator_item(
            app,
            "menu.reset-zoom",
            labels.actual_size,
            resolve_accelerator(&bindings, "resetZoom", "CmdOrCtrl+0"),
        )?)
        .build()?;

    let window = SubmenuBuilder::new(app, labels.window)
        .item(&PredefinedMenuItem::minimize(app, Some(labels.minimize))?)
        .build()?;

    let help = SubmenuBuilder::new(app, labels.help)
        .item(&accelerator_item(app, "menu.about", labels.about, None)?)
        .item(&accelerator_item(
            app,
            "menu.feedback",
            labels.feedback,
            None,
        )?)
        .item(&accelerator_item(
            app,
            "menu.export-logs",
            labels.export_logs,
            None,
        )?)
        .item(&accelerator_item(
            app,
            "menu.resource-manager",
            labels.resource_manager,
            None,
        )?)
        .separator()
        .item(&accelerator_item(
            app,
            "menu.toggle-devtools",
            labels.devtools,
            None,
        )?)
        .separator()
        .item(&accelerator_item(
            app,
            "menu.clear-data",
            labels.clear_data,
            None,
        )?)
        .build()?;

    menu.item(&file)
        .item(&edit)
        .item(&view)
        .item(&window)
        .item(&help)
        .build()
}

pub fn install(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<MenuState>();
    let menu = build_menu(app, &state).map_err(|error| error.to_string())?;
    app.set_menu(menu).map_err(|error| error.to_string())?;
    Ok(())
}

fn rebuild(app: &AppHandle, state: &MenuState) -> Result<(), String> {
    let menu = build_menu(app, state).map_err(|error| error.to_string())?;
    app.set_menu(menu).map_err(|error| error.to_string())?;
    Ok(())
}

fn embedded_feedback_url() -> Option<String> {
    let config: serde_json::Value = serde_json::from_str(EMBEDDED_APP_CONFIG).ok()?;
    config
        .get("feedback_url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Execute a menu selection. Predefined Edit/Window/Fullscreen items are
/// handled natively and never reach this function.
pub fn handle_menu_event(app: &AppHandle, id: &str) {
    match action_for_menu_id(id) {
        Some(MenuAction::DesktopCommand(command)) => {
            let _ = app.emit("zcode:desktop-command", command);
        }
        Some(MenuAction::NewTab) => {
            let _ = app.emit("zcode:new-tab", ());
        }
        Some(MenuAction::ZoomIn) => {
            let _ = crate::step_zoom(app, 1);
        }
        Some(MenuAction::ZoomOut) => {
            let _ = crate::step_zoom(app, -1);
        }
        Some(MenuAction::ResetZoom) => {
            let _ = crate::reset_zoom(app);
        }
        Some(MenuAction::ExportLogs) => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let _ = crate::export_logs::export_logs(app).await;
            });
        }
        Some(MenuAction::ToggleDevTools) => {
            if let Some(window) = app.get_webview_window("main") {
                if window.is_devtools_open() {
                    window.close_devtools();
                } else {
                    window.open_devtools();
                }
            }
        }
        Some(MenuAction::ClearAllData) => {
            crate::clear_data::confirm_and_clear(app);
        }
        Some(MenuAction::ResourceManager) => {
            if let Err(error) = crate::resource_manager::open(app) {
                eprintln!("[resource-manager] {error}");
            }
        }
        Some(MenuAction::About) => {
            app.dialog()
                .message(format!(
                    "NC-Code {} ({})",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS
                ))
                .title("About NC-Code")
                .kind(MessageDialogKind::Info)
                .show(|_| {});
        }
        Some(MenuAction::Feedback) => {
            if let Some(url) = embedded_feedback_url() {
                let _ = tauri_plugin_opener::open_url(url, None::<&str>);
            }
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(key, values)| {
                (
                    key.to_string(),
                    values.iter().map(|value| value.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn normalizes_electron_accelerators() {
        assert_eq!(
            normalize_accelerator("CmdOrCtrl+k").as_deref(),
            Some("CmdOrCtrl+K")
        );
        assert_eq!(
            normalize_accelerator("Shift+Ctrl+p").as_deref(),
            Some("Ctrl+Shift+P")
        );
        assert_eq!(
            normalize_accelerator("CmdOrCtrl+=").as_deref(),
            Some("CmdOrCtrl+Plus")
        );
        assert_eq!(
            normalize_accelerator("CmdOrCtrl+-").as_deref(),
            Some("CmdOrCtrl+-")
        );
        assert_eq!(
            normalize_accelerator("Ctrl+F11").as_deref(),
            Some("Ctrl+F11")
        );
        assert_eq!(normalize_accelerator("AltGr+e"), None);
        assert_eq!(normalize_accelerator("Ctrl+"), None);
        assert_eq!(normalize_accelerator("Ctrl+unknownkey"), None);
        assert_eq!(normalize_accelerator("k"), None);
    }

    #[test]
    fn resolves_overrides_and_fallbacks() {
        let empty = BTreeMap::new();
        assert_eq!(
            resolve_accelerator(&empty, "newTask", "CmdOrCtrl+N").as_deref(),
            Some("CmdOrCtrl+N")
        );
        let overridden = bindings(&[("newTask", &["CmdOrCtrl+Shift+n"])]);
        assert_eq!(
            resolve_accelerator(&overridden, "newTask", "CmdOrCtrl+N").as_deref(),
            Some("CmdOrCtrl+Shift+N")
        );
        let disabled = bindings(&[("newTask", &[])]);
        assert_eq!(
            resolve_accelerator(&disabled, "newTask", "CmdOrCtrl+N"),
            None
        );
        let invalid = bindings(&[("newTask", &["AltGr+e"])]);
        assert_eq!(
            resolve_accelerator(&invalid, "newTask", "CmdOrCtrl+N").as_deref(),
            Some("CmdOrCtrl+N")
        );
    }

    #[test]
    fn normalizes_locales() {
        assert_eq!(normalize_locale(Some("zh-CN")), "zh-CN");
        assert_eq!(normalize_locale(Some("zh-TW")), "zh-CN");
        assert_eq!(normalize_locale(Some("en-US")), "en-US");
        assert_eq!(normalize_locale(Some("ja-JP")), "en-US");
        assert_eq!(normalize_locale(None), DEFAULT_LOCALE);
    }

    #[test]
    fn menu_ids_map_to_actions() {
        assert_eq!(
            action_for_menu_id("menu.new-task"),
            Some(MenuAction::DesktopCommand("newTask"))
        );
        assert_eq!(action_for_menu_id("menu.new-tab"), Some(MenuAction::NewTab));
        assert_eq!(
            action_for_menu_id("menu.export-logs"),
            Some(MenuAction::ExportLogs)
        );
        assert_eq!(
            action_for_menu_id("menu.toggle-devtools"),
            Some(MenuAction::ToggleDevTools)
        );
        assert_eq!(
            action_for_menu_id("menu.clear-data"),
            Some(MenuAction::ClearAllData)
        );
        assert_eq!(
            action_for_menu_id("menu.resource-manager"),
            Some(MenuAction::ResourceManager)
        );
        assert_eq!(action_for_menu_id("menu.unknown"), None);
    }

    #[test]
    fn menu_labels_are_localized() {
        assert_eq!(labels("zh-CN").file, "文件");
        assert_eq!(labels("zh-CN").new_tab, "新建标签页");
        assert_eq!(labels("zh-CN").resource_manager, "资源管理器");
        assert_eq!(labels("en-US").file, "File");
        assert_eq!(labels("en-US").new_tab, "New tab");
        assert_eq!(labels("en-US").resource_manager, "Resource manager");
    }

    #[test]
    fn embedded_config_has_feedback_url() {
        assert!(embedded_feedback_url().is_some());
    }
}
