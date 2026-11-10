//! `nc-code-shell`：S4 Tauri 2 桌面壳脚手架。
//!
//! 交付面（spec `specs/rust-native-crates.md` S4 验收）：
//! - 编译通过（`cargo check` / `cargo build`），映射 `IPlatformService`
//!   （`packages/shared/src/platform.ts`）的**已声明子集**——完整成员映射与
//!   延迟项见 crate README 的 R4 对照表；
//! - tray 菜单 Show/Quit，图标为运行期程序生成的 16x16 RGBA（不携带二进制图标）；
//! - UI 资源经 `build.rs` staged 到 `$OUT_DIR/web`（dev）与 `bundle.resources`
//!   （packaged，`../../../packages/web/dist` → `web`）；`backend.rs` 按
//!   `ZCODE_WEB_DIST` → staged → resource → checkout fallback 解析。
//!
//! CDP 桥：Windows 经 `browser_view_cdp` → `ShellBrowserPort` → Rust host
//! `Browser` 工具（`--browser-selftest` 真实 WebView2 验证，2026-11-04 PASS）；
//! macOS/Linux 无 WebView2 CDP，显式 `not-webview` / `browser_view_cdp_unsupported`，
//! 绝不伪装成功。窗口后端自检：`--window-selftest`（headless HTTP 验证）。

mod app_menu;
mod backend;
mod browser_control;
mod browser_control_scripts;
mod browser_playwright;
mod browser_recording;
mod browser_recording_selftest;
mod browser_selftest;
mod browser_use_bridge;
mod browser_view;
mod chrome_import;
mod clear_data;
mod computer_control;
mod computer_selftest;
mod device_id;
mod editors;
mod export_logs;
mod mcp_config;
mod notifications;
mod power_save;
#[cfg(any(test, target_os = "macos", target_os = "linux"))]
mod print_pdf;
mod remote_discovery;
mod resource_manager;
mod telemetry;
mod temp_attachment;
mod updater;
mod window_screenshot;
mod window_selftest;
mod window_sync;

use chrome_import::import_chrome_browser_data;
use device_id::{get_device_id, DeviceIdState};
use editors::{get_installed_editors, open_in_editor};
use export_logs::export_logs;
use mcp_config::{
    load_mcp_from_user_directory, migrate_legacy_common_mcp, save_mcp_to_user_directory,
};
use notifications::NotificationClickRegistry;
use power_save::PowerSaveManager;
use resource_manager::{resource_manager_snapshot, ResourceManagerState};
use temp_attachment::create_temp_text_attachment;
use updater::{
    acknowledge_post_update_release_notes, cancel_update_download, check_for_updates,
    download_update, get_update_state, quit_and_install_update, skip_update_version, UpdaterState,
};
use window_screenshot::capture_window_screenshot;
use window_sync::{
    sync_active_task_session, sync_app_settings, sync_window_tabs, sync_window_unread_count,
    WindowSyncState,
};

use browser_view::{
    browser_view_attach_guest, browser_view_cdp, browser_view_close, browser_view_create,
    browser_view_detach_guest, browser_view_eval, browser_view_go_back, browser_view_go_forward,
    browser_view_navigate, browser_view_open_devtools, browser_view_reload,
    browser_view_set_bounds, browser_view_set_occluded, browser_view_set_visible,
    browser_view_set_zoom, browser_view_state,
};

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use reqwest::Url;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, WebviewWindow};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;

/// 平台信息（os/arch）；`IPlatformService` 无同名成员，renderer 现走 navigator，
/// 此命令为壳层显式能力，供 Tauri adapter 未来统一读取。
#[derive(serde::Serialize)]
struct PlatformInfo {
    os: &'static str,
    arch: &'static str,
}

/// 应用版本（= `tauri.conf.json` version = 仓库根 `package.json` version，测试守护）。
/// `IPlatformService` 更新族成员（`onUpdateReady` 等）的 version 载荷来源之一；
/// 完整自动更新是 deferred（tauri-plugin-updater + port，§7 R4）。
#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// `IPlatformService.openExternal`：用系统默认应用打开 URL（OAuth 跳浏览器同路径）。
#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string())
}

/// `IPlatformService.openExternalFile`：用系统默认应用打开本地路径。
#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    tauri_plugin_opener::open_path(path, None::<&str>).map_err(|e| e.to_string())
}

/// Reveal a file (or open a directory) in the OS file manager, selecting the file when
/// the platform supports it — Electron parity for `openInFileManager` (`explorer
/// /select,` on Windows, `open -R` on macOS, `xdg-open` on Linux).
#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    let target = std::path::PathBuf::from(&path);
    if !target.exists() {
        return Err(format!("path does not exist: {path}"));
    }
    let (program, args) = reveal_command_for(std::env::consts::OS, &target);
    std::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("{program} failed: {error}"))
}

/// `(program, args)` for revealing `path`, per target OS. Pure so the argv shape is
/// pinned by a unit test on every platform.
fn reveal_command_for(platform: &str, path: &std::path::Path) -> (String, Vec<String>) {
    let rendered = path.to_string_lossy().to_string();
    match platform {
        "windows" => ("explorer".to_string(), vec![format!("/select,{rendered}")]),
        "macos" => ("open".to_string(), vec!["-R".to_string(), rendered]),
        _ => {
            // Linux/BSD file managers have no portable "select" argument: open the
            // containing directory for a file, or the directory itself.
            let target = if path.is_dir() {
                rendered
            } else {
                path.parent()
                    .map(|parent| parent.to_string_lossy().to_string())
                    .unwrap_or(rendered)
            };
            ("xdg-open".to_string(), vec![target])
        }
    }
}

/// `IPlatformService.executeDesktopCommand("minimizeWindow")`。
#[tauri::command]
fn window_minimize(window: WebviewWindow) -> Result<(), String> {
    window.minimize().map_err(|e| e.to_string())
}

/// `IPlatformService.executeDesktopCommand("toggleMaximizeWindow")`。
#[tauri::command]
fn window_toggle_maximize(window: WebviewWindow) -> Result<(), String> {
    // tauri 2 核心窗口没有 toggle_maximize 方法（只有 JS 插件命令），
    // 按 is_maximized 手动切换以对齐 Electron 的 toggle 语义。
    if window.is_maximized().map_err(|e| e.to_string())? {
        window.unmaximize().map_err(|e| e.to_string())
    } else {
        window.maximize().map_err(|e| e.to_string())
    }
}

/// `IPlatformService.executeDesktopCommand("closeWindow")`。
#[tauri::command]
fn window_close(window: WebviewWindow) -> Result<(), String> {
    window.close().map_err(|e| e.to_string())
}

/// 壳层能力（tray 需要）：隐藏窗口；`IPlatformService` 无对应成员。
#[tauri::command]
fn window_hide(window: WebviewWindow) -> Result<(), String> {
    window.hide().map_err(|e| e.to_string())
}

/// 壳层能力（tray "Show"）：还原并聚焦窗口；`IPlatformService` 无对应成员。
#[tauri::command]
fn window_show(window: WebviewWindow) -> Result<(), String> {
    window
        .show()
        .and_then(|_| window.unminimize())
        .and_then(|_| window.set_focus())
        .map_err(|e| e.to_string())
}

/// `IPlatformService` 无同名成员；见结构体注释。
#[tauri::command]
fn platform_info() -> PlatformInfo {
    PlatformInfo {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    }
}

/// 程序生成的 16x16 tray 图标：透明外圈 + 蓝底白色 X 形，避免仓库携带
/// `.ico`/`.png` 二进制资产；与 electron 正式图标的对等验证属开放项（见 README）。
fn generate_tray_rgba() -> Vec<u8> {
    let mut pixels = Vec::with_capacity(16 * 16 * 4);
    for y in 0..16u32 {
        for x in 0..16u32 {
            let edge = x == 0 || y == 0 || x == 15 || y == 15;
            let diagonal = x == y || x + y == 15;
            let rgba: [u8; 4] = if edge {
                [0, 0, 0, 0] // 外圈透明，托盘视觉更干净
            } else if diagonal {
                [255, 255, 255, 255]
            } else {
                [37, 99, 235, 255]
            };
            pixels.extend_from_slice(&rgba);
        }
    }
    pixels
}

/// 注册 tray：菜单 Show（还原并聚焦主窗口）/ Quit（退出进程）。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn emit_desktop_command(app: &tauri::AppHandle, command: &str) {
    let _ = app.emit("zcode:desktop-command", command);
}

/// Deep links carried by a second instance's argv (`zcode://…`). The launcher may pass
/// other arguments; only the scheme the app registers is forwarded, so a random argv
/// value can never be mistaken for an app route.
fn deep_links_from_argv(argv: &[String]) -> Vec<String> {
    argv.iter()
        .map(|arg| arg.trim())
        .filter(|arg| arg.to_ascii_lowercase().starts_with("zcode://"))
        .map(str::to_string)
        .collect()
}

/// A second launch must reuse the running app (Electron `requestSingleInstanceLock`):
/// focus the window and replay the deep links this process was launched with.
fn handle_second_instance(app: &tauri::AppHandle, argv: &[String]) {
    show_main_window(app);
    for url in deep_links_from_argv(argv) {
        let _ = app.emit("zcode:deep-link", url);
    }
}

/// 注册 tray：与 Electron 托盘一致的入口（Open / New Task / Open Workspace /
/// About / Clear All Data / Quit）。菜单命令发到 renderer，窗口动作留在壳层。
fn setup_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open NC-Code", true, None::<&str>)?;
    let new_task = MenuItem::with_id(app, "new-task", "New Task", true, None::<&str>)?;
    let open_workspace =
        MenuItem::with_id(app, "open-workspace", "Open Workspace", true, None::<&str>)?;
    let about = MenuItem::with_id(app, "about", "About NC-Code", true, None::<&str>)?;
    let clear_data = MenuItem::with_id(app, "clear-data", "Clear All Data", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &new_task,
            &open_workspace,
            &about,
            &clear_data,
            &quit,
        ],
    )?;

    // Use the packaged product icon when the bundle provides one (tauri.conf icons),
    // falling back to the generated glyph only for builds without it.
    let icon = app.default_window_icon().cloned();
    let mut tray = TrayIconBuilder::with_id("main-tray").tooltip("NC-Code");
    tray = match icon {
        Some(icon) => tray.icon(icon),
        None => tray.icon(Image::new_owned(generate_tray_rgba(), 16, 16)),
    };
    tray.menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.0.as_str() {
            "open" => show_main_window(app),
            "new-task" => {
                show_main_window(app);
                emit_desktop_command(app, "newTask");
            }
            "open-workspace" => {
                show_main_window(app);
                emit_desktop_command(app, "openWorkspace");
            }
            "about" => {
                show_main_window(app);
                emit_desktop_command(app, "showAbout");
            }
            "clear-data" => {
                show_main_window(app);
                clear_data::confirm_and_clear(app);
            }
            "quit" => window_sync::quit(app),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// `IPlatformService.selectDirectory`: native folder picker (Tauri dialog plugin).
#[tauri::command]
fn select_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    app.dialog()
        .file()
        .blocking_pick_folder()
        .map(|path| {
            path.into_path()
                .map(|value| value.to_string_lossy().to_string())
                .map_err(|error| error.to_string())
        })
        .transpose()
}

/// `IPlatformService.selectFile`: native file picker (single file).
#[tauri::command]
fn select_file(app: tauri::AppHandle) -> Result<Option<String>, String> {
    app.dialog()
        .file()
        .blocking_pick_file()
        .map(|path| {
            path.into_path()
                .map(|value| value.to_string_lossy().to_string())
                .map_err(|error| error.to_string())
        })
        .transpose()
}

/// `IPlatformService.selectFiles`: native file picker (multi-select).
#[tauri::command]
fn select_files(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let mut selected = Vec::new();
    if let Some(paths) = app.dialog().file().blocking_pick_files() {
        for path in paths {
            selected.push(
                path.into_path()
                    .map(|value| value.to_string_lossy().to_string())
                    .map_err(|error| error.to_string())?,
            );
        }
    }
    Ok(selected)
}

/// `IPlatformService` clipboard members (Tauri clipboard plugin).
#[tauri::command]
fn clipboard_write_text(app: tauri::AppHandle, text: String) -> Result<(), String> {
    app.clipboard()
        .write_text(text)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn clipboard_read_text(app: tauri::AppHandle) -> Result<String, String> {
    app.clipboard()
        .read_text()
        .map_err(|error| error.to_string())
}

/// `IPlatformService.showTaskNotification` (Tauri notification plugin).
/// The numeric notification id is registered so a click can be mapped back to
/// the renderer task id and emitted as `zcode:task-notification-click`.
#[tauri::command]
fn show_task_notification(
    app: tauri::AppHandle,
    registry: tauri::State<'_, NotificationClickRegistry>,
    task_id: Option<String>,
    title: String,
    body: String,
) -> Result<(), String> {
    let notification_id = registry.register(task_id.as_deref());
    app.notification()
        .builder()
        .id(notification_id)
        .title(title)
        .body(body)
        .show()
        .map_err(|error| error.to_string())
}

/// `IPlatformService.getDesktopWindowChromeState` (the subset Tauri exposes).
#[tauri::command]
fn get_desktop_window_chrome_state(window: WebviewWindow) -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "isMaximized": window.is_maximized().map_err(|error| error.to_string())?,
        "isFullscreen": window.is_fullscreen().map_err(|error| error.to_string())?,
    }))
}

const DESKTOP_ZOOM_MIN_LEVEL: i32 = -3;
const DESKTOP_ZOOM_MAX_LEVEL: i32 = 5;
const DESKTOP_ZOOM_FACTOR_STEP: f64 = 1.1;
static DESKTOP_ZOOM_LEVEL: AtomicI32 = AtomicI32::new(0);

fn apply_desktop_zoom(window: &WebviewWindow, level: i32) -> Result<(), String> {
    let level = level.clamp(DESKTOP_ZOOM_MIN_LEVEL, DESKTOP_ZOOM_MAX_LEVEL);
    let factor = DESKTOP_ZOOM_FACTOR_STEP.powi(level);
    window.set_zoom(factor).map_err(|error| error.to_string())?;
    DESKTOP_ZOOM_LEVEL.store(level, Ordering::SeqCst);
    let _ = window.emit(
        "zcode:desktop-zoom-changed",
        serde_json::json!({ "zoomLevel": level }),
    );
    Ok(())
}

/// Menu entry point: step the main window's zoom level.
pub(crate) fn step_zoom(app: &tauri::AppHandle, delta: i32) -> Result<(), String> {
    let Some(window) = app.get_webview_window("main") else {
        return Err("main_window_missing".to_string());
    };
    apply_desktop_zoom(&window, DESKTOP_ZOOM_LEVEL.load(Ordering::SeqCst) + delta)
}

/// Menu entry point: reset the main window's zoom level.
pub(crate) fn reset_zoom(app: &tauri::AppHandle) -> Result<(), String> {
    let Some(window) = app.get_webview_window("main") else {
        return Err("main_window_missing".to_string());
    };
    apply_desktop_zoom(&window, 0)
}

/// `IPlatformService.getDesktopZoomLevel`.
#[tauri::command]
fn get_desktop_zoom_level() -> serde_json::Value {
    serde_json::json!({ "zoomLevel": DESKTOP_ZOOM_LEVEL.load(Ordering::SeqCst) })
}

/// `IPlatformService.executeDesktopCommand`: window/zoom commands the shell owns.
#[tauri::command]
fn desktop_command(window: WebviewWindow, command: String) -> Result<(), String> {
    match command.as_str() {
        "minimizeWindow" => window.minimize().map_err(|error| error.to_string()),
        "toggleMaximizeWindow" => {
            if window.is_maximized().map_err(|error| error.to_string())? {
                window.unmaximize().map_err(|error| error.to_string())
            } else {
                window.maximize().map_err(|error| error.to_string())
            }
        }
        "closeWindow" => window.close().map_err(|error| error.to_string()),
        "toggleFullScreen" => {
            let fullscreen = window.is_fullscreen().map_err(|error| error.to_string())?;
            window
                .set_fullscreen(!fullscreen)
                .map_err(|error| error.to_string())
        }
        "resetWindowSize" => {
            window
                .set_size(tauri::LogicalSize::new(1280.0, 840.0))
                .map_err(|error| error.to_string())?;
            window.center().map_err(|error| error.to_string())
        }
        "zoomIn" => apply_desktop_zoom(&window, DESKTOP_ZOOM_LEVEL.load(Ordering::SeqCst) + 1),
        "zoomOut" => apply_desktop_zoom(&window, DESKTOP_ZOOM_LEVEL.load(Ordering::SeqCst) - 1),
        "resetZoom" => apply_desktop_zoom(&window, 0),
        "openResourceManager" => resource_manager::open(window.app_handle()),
        _ => Ok(()),
    }
}

const MAX_SAVE_FILE_BYTES: usize = 50 * 1024 * 1024;
const REMOTE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REMOTE_REDIRECTS: usize = 5;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveFileRequest {
    suggested_name: String,
    #[serde(default)]
    data: Option<Vec<u8>>,
    #[serde(default)]
    source_url: Option<String>,
}

#[derive(Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SaveFileResult {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    canceled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl SaveFileResult {
    fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(message.into()),
            ..Default::default()
        }
    }
}

/// The same private/loopback/link-local/multicast blocklist the Electron main
/// process uses: a renderer URL must not turn into a main-process intranet read.
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let value = u32::from_be_bytes(v4.octets());
            let in_subnet = |network: [u8; 4], prefix: u32| {
                let mask = if prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - prefix)
                };
                value & mask == u32::from_be_bytes(network) & mask
            };
            in_subnet([0, 0, 0, 0], 8)
                || in_subnet([10, 0, 0, 0], 8)
                || in_subnet([100, 64, 0, 0], 10)
                || in_subnet([127, 0, 0, 0], 8)
                || in_subnet([169, 254, 0, 0], 16)
                || in_subnet([172, 16, 0, 0], 12)
                || in_subnet([192, 0, 0, 0], 24)
                || in_subnet([192, 0, 2, 0], 24)
                || in_subnet([192, 168, 0, 0], 16)
                || in_subnet([198, 18, 0, 0], 15)
                || in_subnet([198, 51, 100, 0], 24)
                || in_subnet([203, 0, 113, 0], 24)
                || in_subnet([224, 0, 0, 0], 4)
                || in_subnet([240, 0, 0, 0], 4)
        }
        IpAddr::V6(v6) => {
            let value = u128::from_be_bytes(v6.octets());
            let in_subnet = |network: [u8; 16], prefix: u32| {
                let mask = if prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - prefix)
                };
                value & mask == u128::from_be_bytes(network) & mask
            };
            in_subnet([0; 16], 128)
                || in_subnet([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1], 128)
                || in_subnet([0xfc, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 7)
                || in_subnet([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 10)
                || in_subnet([0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8)
                || in_subnet(
                    [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    32,
                )
        }
    }
}

async fn download_public_file(initial: Url) -> Result<Vec<u8>, String> {
    let mut current = initial;
    for redirect in 0..=MAX_REMOTE_REDIRECTS {
        let host = current
            .host_str()
            .ok_or_else(|| "download_failed".to_string())?
            .to_string();
        if host.eq_ignore_ascii_case("localhost")
            || host.to_ascii_lowercase().ends_with(".localhost")
        {
            return Err("remote_address_not_allowed".to_string());
        }
        let port = current
            .port_or_known_default()
            .ok_or_else(|| "download_failed".to_string())?;
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|_| "download_failed".to_string())?
            .collect();
        if addresses.is_empty() || addresses.iter().any(|address| is_blocked_ip(address.ip())) {
            return Err("remote_address_not_allowed".to_string());
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REMOTE_DOWNLOAD_TIMEOUT)
            // Pin the connection to the address we validated, closing the DNS
            // rebinding window between the check and the actual connect.
            .resolve(&host, addresses[0])
            .build()
            .map_err(|_| "download_failed".to_string())?;
        let mut response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|_| "download_failed".to_string())?;
        if response.status().is_redirection() {
            if redirect == MAX_REMOTE_REDIRECTS {
                return Err("download_failed".to_string());
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "download_failed".to_string())?;
            current = current
                .join(location)
                .map_err(|_| "download_failed".to_string())?;
            if !matches!(current.scheme(), "http" | "https") {
                return Err("download_failed".to_string());
            }
            continue;
        }
        if !response.status().is_success() {
            return Err("download_failed".to_string());
        }
        if let Some(length) = response.content_length() {
            if length as usize > MAX_SAVE_FILE_BYTES {
                return Err("file_too_large".to_string());
            }
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "download_failed".to_string())?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_SAVE_FILE_BYTES {
                return Err("file_too_large".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Err("invalid_file_payload".to_string());
        }
        return Ok(bytes);
    }
    Err("download_failed".to_string())
}

/// The deep link that launched the app (or the most recent one). The renderer
/// calls this from `notifyRendererReady` so a cold-start deep link is not lost.
#[tauri::command]
fn deep_link_current(app: tauri::AppHandle) -> Option<String> {
    app.deep_link()
        .get_current()
        .ok()
        .flatten()
        .and_then(|urls| urls.into_iter().next())
        .map(|url| url.to_string())
}

/// `IPlatformService.saveFile`: native save dialog + data write or guarded
/// remote download (50 MiB cap, public-address check on every redirect).
#[tauri::command]
async fn save_file(app: tauri::AppHandle, payload: SaveFileRequest) -> SaveFileResult {
    let suggested: String = Path::new(payload.suggested_name.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect();
    if suggested.is_empty() {
        return SaveFileResult::error("invalid_file_payload");
    }
    let data = payload.data.filter(|bytes| !bytes.is_empty());
    let source = payload
        .source_url
        .as_deref()
        .and_then(|value| Url::parse(value).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"));
    if data.is_none() && source.is_none() {
        return SaveFileResult::error("invalid_file_payload");
    }
    if data
        .as_ref()
        .is_some_and(|bytes| bytes.len() > MAX_SAVE_FILE_BYTES)
    {
        return SaveFileResult::error("file_too_large");
    }
    let picked = app
        .dialog()
        .file()
        .set_file_name(&suggested)
        .blocking_save_file();
    let Some(path) = picked else {
        return SaveFileResult {
            success: false,
            canceled: Some(true),
            ..Default::default()
        };
    };
    let path = match path.into_path() {
        Ok(path) => path,
        Err(error) => return SaveFileResult::error(error.to_string()),
    };
    let bytes = match (data, source) {
        (Some(data), _) => data,
        (None, Some(url)) => match download_public_file(url).await {
            Ok(bytes) => bytes,
            Err(error) => return SaveFileResult::error(error),
        },
        (None, None) => return SaveFileResult::error("invalid_file_payload"),
    };
    match std::fs::write(&path, bytes) {
        Ok(()) => SaveFileResult {
            success: true,
            path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        },
        Err(error) => SaveFileResult::error(format!("write_failed: {error}")),
    }
}

#[cfg(windows)]
fn print_page_to_pdf_windows(window: &WebviewWindow) -> Result<Vec<u8>, String> {
    use std::sync::mpsc;
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_7;
    use webview2_com::PrintToPdfCompletedHandler;
    use windows::core::{Interface, HSTRING};

    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, String>>();
    let path = std::env::temp_dir().join(format!("zcode-print-{}.pdf", std::process::id()));
    let path_for_handler = path.clone();
    let handler_tx = tx.clone();
    window
        .with_webview(move |webview| {
            let started = (|| -> Result<(), String> {
                let controller = webview.controller();
                let core =
                    unsafe { controller.CoreWebView2() }.map_err(|error| error.to_string())?;
                let core7: ICoreWebView2_7 = core.cast().map_err(|error| error.to_string())?;
                let handler =
                    PrintToPdfCompletedHandler::create(Box::new(move |error_code, success| {
                        let result = if error_code.is_ok() && success {
                            std::fs::read(&path_for_handler).map_err(|error| error.to_string())
                        } else {
                            Err("print_failed".to_string())
                        };
                        let _ = handler_tx.send(result);
                        Ok(())
                    }));
                unsafe {
                    core7.PrintToPdf(
                        &HSTRING::from(path.to_string_lossy().to_string()),
                        None,
                        &handler,
                    )
                }
                .map_err(|error| error.to_string())
            })();
            if let Err(error) = started {
                let _ = tx.send(Err(error));
            }
        })
        .map_err(|error| error.to_string())?;
    rx.recv_timeout(Duration::from_secs(30))
        .map_err(|_| "print_failed".to_string())?
}

/// Persist a renderer-produced WebM recording inside the workspace.
#[tauri::command]
async fn save_browser_recording(
    workspace_path: String,
    relative_path: String,
    data: Vec<u8>,
) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        browser_recording::write_recording(
            std::path::Path::new(&workspace_path),
            &relative_path,
            &data,
        )
        .map(|path| {
            let value = path.to_string_lossy();
            value.strip_prefix(r"\\?\").unwrap_or(&value).to_string()
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

/// `IPlatformService.printPageToPdf`: Windows uses WebView2's PrintToPdfAsync
/// (the same Chromium print pipeline the Electron build used). Other platforms
/// report `print_failed` explicitly rather than fabricating a PDF.
#[tauri::command]
fn print_page_to_pdf(window: WebviewWindow) -> serde_json::Value {
    #[cfg(windows)]
    {
        match print_page_to_pdf_windows(&window) {
            Ok(bytes) => serde_json::json!({ "success": true, "data": bytes }),
            Err(error) => serde_json::json!({ "success": false, "error": error }),
        }
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        match print_pdf::print_page_to_pdf(&window) {
            Ok(bytes) => serde_json::json!({ "success": true, "data": bytes }),
            Err(error) => serde_json::json!({ "success": false, "error": error }),
        }
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = window;
        serde_json::json!({ "success": false, "error": "print_failed" })
    }
}

/// `IPlatformService.isDockerAvailable`: asks the local Docker CLI for a server
/// version; a missing CLI or stopped daemon reports `false`.
#[tauri::command]
async fn is_docker_available() -> bool {
    remote_discovery::is_docker_available().await
}

/// `IPlatformService.listWSLDistros`: parses `wsl.exe -l -v`; non-Windows and
/// missing WSL report an empty list.
#[tauri::command]
async fn list_wsl_distros() -> Vec<remote_discovery::WslDistro> {
    remote_discovery::list_wsl_distros().await
}

/// `IPlatformService.listDockerContainers`: parses `docker ps` JSON lines.
#[tauri::command]
async fn list_docker_containers() -> Vec<remote_discovery::DockerContainerInfo> {
    remote_discovery::list_docker_containers().await
}

/// `IPlatformService.listSSHConfigAliases`: parses `~/.ssh/config` (including
/// `Include` files) and reports the aliases that are safe to connect to.
#[tauri::command]
fn list_ssh_config_aliases() -> Vec<remote_discovery::SshConfigAliasOption> {
    remote_discovery::list_ssh_config_aliases()
}

/// `IPlatformService.reportTelemetryEvent`: queue a renderer event for the
/// OTLP/HTTP exporter (disabled unless an OTLP endpoint is configured).
#[tauri::command]
fn report_telemetry_event(
    exporter: tauri::State<'_, telemetry::TelemetryExporter>,
    payload: telemetry::RendererTelemetryEvent,
) {
    exporter.record(telemetry::TelemetryRecord::Event(payload));
}

/// `IPlatformService.reportArmsCustomEvent`: ARMS-only call sites are mapped to
/// the same OTLP log stream instead of the vendor SDK.
#[tauri::command]
fn report_arms_custom_event(
    exporter: tauri::State<'_, telemetry::TelemetryExporter>,
    payload: telemetry::ArmsCustomEvent,
) {
    exporter.record(telemetry::TelemetryRecord::Arms(payload));
}

/// `IPlatformService.reportRendererActionTraceBatch`: renderer action spans are
/// exported as OTLP traces (`/v1/traces`).
#[tauri::command]
fn report_renderer_action_trace_batch(
    exporter: tauri::State<'_, telemetry::TelemetryExporter>,
    batch: telemetry::RendererActionTraceBatch,
) {
    exporter.record(telemetry::TelemetryRecord::ActionTrace(batch));
}

/// `IPlatformService.reportRendererHeapSample`: exported as the
/// `renderer.heap.used` OTLP gauge.
#[tauri::command]
fn report_renderer_heap_sample(
    exporter: tauri::State<'_, telemetry::TelemetryExporter>,
    sample: telemetry::RendererHeapSample,
) {
    exporter.record(telemetry::TelemetryRecord::HeapSample(sample));
}

/// `IPlatformService.reportLocalTtftBatch`: exported as the `renderer.ttft`
/// OTLP gauge with per-record stage/outcome attributes.
#[tauri::command]
fn report_local_ttft_batch(
    exporter: tauri::State<'_, telemetry::TelemetryExporter>,
    batch: telemetry::LocalTtftBatch,
) {
    exporter.record(telemetry::TelemetryRecord::LocalTtft(batch));
}

fn main() {
    if std::env::args().any(|argument| argument == "--window-selftest") {
        std::process::exit(window_selftest::run());
    }
    if std::env::args().any(|argument| argument == "--browser-recording-selftest") {
        std::process::exit(browser_recording_selftest::run());
    }
    if std::env::args().any(|argument| argument == "--computer-selftest") {
        std::process::exit(computer_selftest::run());
    }
    if std::env::args().any(|argument| argument == "--browser-selftest") {
        std::process::exit(browser_selftest::run());
    }
    tauri::Builder::default()
        // Must be the first plugin: a second launch has to be intercepted before any
        // window or backend is created (Electron `requestSingleInstanceLock` parity).
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            handle_second_instance(app, &argv);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(telemetry::start(env!("CARGO_PKG_VERSION")))
        .manage(browser_view::BrowserViewRegistry::default())
        .manage(DeviceIdState::default())
        .manage(NotificationClickRegistry::default())
        .manage(WindowSyncState::default())
        .manage(UpdaterState::default())
        .manage(PowerSaveManager::default())
        .manage(ResourceManagerState::default())
        .manage(app_menu::MenuState::from_settings())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let close_to_tray = window
                    .app_handle()
                    .try_state::<WindowSyncState>()
                    .map(|state| state.close_to_tray())
                    .unwrap_or(false);
                if window_sync::should_hide_on_close(close_to_tray, window_sync::explicit_quit()) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_version,
            open_external_url,
            open_path,
            reveal_path,
            window_minimize,
            window_toggle_maximize,
            window_close,
            window_hide,
            window_show,
            platform_info,
            select_directory,
            select_file,
            select_files,
            clipboard_write_text,
            clipboard_read_text,
            show_task_notification,
            sync_window_tabs,
            sync_window_unread_count,
            sync_active_task_session,
            sync_app_settings,
            get_desktop_window_chrome_state,
            save_file,
            deep_link_current,
            desktop_command,
            get_desktop_zoom_level,
            resource_manager_snapshot,
            print_page_to_pdf,
            save_browser_recording,
            is_docker_available,
            list_wsl_distros,
            list_docker_containers,
            list_ssh_config_aliases,
            report_telemetry_event,
            report_arms_custom_event,
            report_renderer_action_trace_batch,
            report_renderer_heap_sample,
            report_local_ttft_batch,
            import_chrome_browser_data,
            export_logs,
            capture_window_screenshot,
            get_installed_editors,
            open_in_editor,
            load_mcp_from_user_directory,
            save_mcp_to_user_directory,
            migrate_legacy_common_mcp,
            create_temp_text_attachment,
            get_device_id,
            get_update_state,
            check_for_updates,
            download_update,
            cancel_update_download,
            quit_and_install_update,
            skip_update_version,
            acknowledge_post_update_release_notes,
            browser_view_create,
            browser_view_set_bounds,
            browser_view_set_visible,
            browser_view_set_occluded,
            browser_view_navigate,
            browser_view_reload,
            browser_view_go_back,
            browser_view_go_forward,
            browser_view_eval,
            browser_view_set_zoom,
            browser_view_open_devtools,
            browser_view_state,
            browser_view_close,
            browser_view_attach_guest,
            browser_view_detach_guest,
            browser_view_cdp,
            browser_use_bridge::browser_use_lifecycle_result,
            browser_use_bridge::browser_use_lifecycle_probe
        ])
        .setup(|app| {
            setup_tray(app)?;
            app_menu::install(app.handle()).map_err(Box::<dyn std::error::Error>::from)?;
            app.on_menu_event(|app, event| {
                app_menu::handle_menu_event(app, event.id().0.as_str());
            });
            // Forward zcode:// deep links to the renderer; the web adapter filters
            // OAuth/payment/share routes and keeps the registered OAuth state set.
            let deep_link_handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    let _ = deep_link_handle.emit("zcode:deep-link", url.to_string());
                }
            });
            // Dev builds have no installer to register the scheme; packaged builds
            // still call this (it is idempotent on Windows/Linux).
            let _ = app.deep_link().register_all();
            // Route notification clicks back to the renderer with the task id
            // registered when the notification was shown.
            let notification_app = app.handle().clone();
            app.notification()
                .on_action(move |performed| {
                    if !notifications::is_click_action(performed.action_id()) {
                        return;
                    }
                    let Some(notification) = performed.notification() else {
                        return;
                    };
                    let task_id = notification_app
                        .try_state::<NotificationClickRegistry>()
                        .and_then(|registry| registry.take(notification.id()));
                    let Some(task_id) = task_id else {
                        return;
                    };
                    show_main_window(&notification_app);
                    let _ = notification_app.emit("zcode:task-notification-click", task_id);
                })
                .map_err(|error| Box::<dyn std::error::Error>::from(error.to_string()))?;
            // Start the real Rust backend and navigate the window to it.
            backend::start(app).map_err(Box::<dyn std::error::Error>::from)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("zcode-shell 启动失败");
}

#[cfg(test)]
mod tests {
    use super::{deep_links_from_argv, is_blocked_ip, reveal_command_for};

    #[test]
    fn reveal_command_selects_the_file_per_platform() {
        let file = std::path::PathBuf::from("C:/work/app/main.ts");
        let (program, args) = reveal_command_for("windows", &file);
        assert_eq!(program, "explorer");
        assert_eq!(args, vec!["/select,C:/work/app/main.ts".to_string()]);
        let (program, args) = reveal_command_for("macos", &file);
        assert_eq!(program, "open");
        assert_eq!(
            args,
            vec!["-R".to_string(), "C:/work/app/main.ts".to_string()]
        );
        // Linux has no portable select: a file reveals its parent directory.
        let (program, args) = reveal_command_for("linux", &file);
        assert_eq!(program, "xdg-open");
        assert_eq!(args, vec!["C:/work/app".to_string()]);
    }

    #[test]
    fn reveal_command_opens_a_directory_itself_on_linux() {
        let dir = std::env::temp_dir();
        let (_, args) = reveal_command_for("linux", &dir);
        assert_eq!(args, vec![dir.to_string_lossy().to_string()]);
    }

    #[test]
    fn second_instance_argv_forwards_only_zcode_deep_links() {
        let argv: Vec<String> = [
            "C:/Program Files/NC-Code/zcode-shell.exe",
            "--open-workspace",
            "C:/work",
            "zcode://oauth/callback?code=abc",
            " ZCODE://share/import?code=x ",
        ]
        .iter()
        .map(|value| value.to_string())
        .collect();
        let links = deep_links_from_argv(&argv);
        assert_eq!(
            links,
            vec![
                "zcode://oauth/callback?code=abc".to_string(),
                "ZCODE://share/import?code=x".to_string()
            ],
            "only the registered scheme is forwarded; case/whitespace tolerated"
        );
        assert!(deep_links_from_argv(&[]).is_empty());
        assert!(deep_links_from_argv(&["--foo".to_string()]).is_empty());
    }

    #[test]
    fn blocks_private_loopback_and_documentation_addresses() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "172.16.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
        ] {
            assert!(
                is_blocked_ip(address.parse().expect("ip")),
                "{address} must be blocked"
            );
        }
    }

    #[test]
    fn allows_public_addresses() {
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ] {
            assert!(
                !is_blocked_ip(address.parse().expect("ip")),
                "{address} must be allowed"
            );
        }
    }

    fn permission_commands() -> Vec<String> {
        let toml = include_str!("../permissions/shell.toml");
        let start = toml.find("commands.allow").expect("commands.allow");
        let rest = &toml[start..];
        let open = rest.find('[').expect("open list");
        let close = rest[open..].find(']').expect("close list") + open;
        rest[open + 1..close]
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.trim_matches('"').to_string())
            .collect()
    }

    fn invoke_handler_commands() -> Vec<String> {
        let source = include_str!("main.rs");
        let start = source.find("generate_handler![").expect("handler");
        let rest = &source[start + "generate_handler![".len()..];
        let end = rest.find("])").expect("handler end");
        rest[..end]
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.rsplit("::").next().unwrap_or(value).to_string())
            .collect()
    }

    #[test]
    fn command_permission_list_matches_invoke_handler() {
        let mut permissions = permission_commands();
        let mut handlers = invoke_handler_commands();
        permissions.sort();
        handlers.sort();
        assert_eq!(
            handlers, permissions,
            "permissions/shell.toml must list exactly the generate_handler! commands"
        );
    }
}
