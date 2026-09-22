//! `zcode-shell`：S4 Tauri 2 桌面壳脚手架。
//!
//! 交付面（spec `specs/rust-native-crates.md` S4 验收）：
//! - 编译通过（`cargo check` / `cargo build`），映射 `IPlatformService`
//!   （`packages/shared/src/platform.ts`）的**已声明子集**——完整成员映射与
//!   延迟项见 crate README 的 R4 对照表；
//! - tray 菜单 Show/Quit，图标为运行期程序生成的 16x16 RGBA（不携带二进制图标）；
//! - UI 资源是 `dist/index.html` 脚手架页（如实标注来源）；接入 `packages/web`
//!   构建产物是开放项。
//!
//! 交互窗口验证与 R4 BrowserView 设计门保持 OPEN（roadmap §7）。

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WebviewWindow};

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
fn setup_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id("main-tray")
        .icon(Image::new_owned(generate_tray_rgba(), 16, 16))
        .tooltip("ZCode")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.0.as_str() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            app_version,
            open_external_url,
            open_path,
            window_minimize,
            window_toggle_maximize,
            window_close,
            window_hide,
            window_show,
            platform_info
        ])
        .setup(|app| {
            setup_tray(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("zcode-shell 启动失败");
}
