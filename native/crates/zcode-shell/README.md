# zcode-shell — Tauri 2 桌面壳脚手架（S4）

Spec：`specs/rust-pure-native-roadmap.md` S4 + §7 R4 表；`specs/rust-native-crates.md`
S4 验收（**编译通过 + IPlatformService 形状的命令面**；交互窗口验证与 BrowserView 设计门
保持 OPEN）。productName `ZCode` / identifier `dev.zcode.app` 与 electron-builder 生产
identity 同族（`packages/desktop/scripts/desktop-product-identity.mjs`），版本 =
仓库根 `package.json` `version`（`tests/config_smoke.rs` 做漂移守护）。

## invoke 命令面（已实现子集）

`app_version`、`open_external_url`、`open_path`、`window_minimize`、
`window_toggle_maximize`、`window_close`、`window_hide`、`window_show`、
`platform_info`；tray 菜单 Show/Quit（16x16 RGBA 运行期程序生成，无二进制图标）。

## IPlatformService → R4 映射表（诚实版：未实现即 deferred）

| `IPlatformService` 成员（`packages/shared/src/platform.ts`）                                                                                                                                                                        | 状态           | 说明 / roadmap 桶（§7 R4）                                                                               |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------- | -------------------------------------------------------------------------------------------------------- |
| `openExternal(url)`                                                                                                                                                                                                                 | ✅ 已实现      | `open_external_url` → `tauri-plugin-opener::open_url`                                                    |
| `openExternalFile(path)`                                                                                                                                                                                                            | ✅ 已实现      | `open_path` → opener `open_path`（无「选择应用打开」扩展）                                               |
| `openInFileManager(path)`                                                                                                                                                                                                           | ⚠️ 部分        | 仅目录经 `open_path` 打开文件管理器；「在父目录选中文件」（win `explorer /select`、mac `-R`）deferred    |
| `executeDesktopCommand("minimizeWindow"/"toggleMaximizeWindow"/"closeWindow")`                                                                                                                                                      | ✅ 已实现      | 映射为 `window_minimize` / `window_toggle_maximize` / `window_close`                                     |
| （窗口 hide/show）                                                                                                                                                                                                                  | ✅ 壳层附加    | `IPlatformService` 无对应成员；tray Show/Quit 依赖                                                       |
| （app 版本）                                                                                                                                                                                                                        | ✅ 壳层附加    | 更新族成员的 version 载荷来源；见下一行 updater deferred                                                 |
| （平台 os/arch）                                                                                                                                                                                                                    | ✅ 壳层附加    | `platform_info`；`IPlatformService` 无同名成员（renderer 走 navigator）                                  |
| tray / 原生菜单 / 窗口 chrome 细节                                                                                                                                                                                                  | ⏳ deferred    | 「Tauri plugins + port」桶；本 crate 仅 tray Show/Quit 最小实现                                          |
| `onUpdateReady` / `downloadUpdate` / `quitAndInstallUpdate` / `getUpdateState` / `skipUpdateVersion` …                                                                                                                              | ⏳ deferred    | `tauri-plugin-updater` + 策略移植（`forceUpdateGuard` 等），R4 桶                                        |
| `registerOAuthState` / `onOAuthCallback` / `onPaymentCallback` / `onShareImport` / `notifyRendererReady`（deep link `zcode://`）                                                                                                    | ⏳ deferred    | `tauri-plugin-deep-link` + port，R4 桶；electron-builder 已声明 scheme `zcode`                           |
| `showTaskNotification`                                                                                                                                                                                                              | ⏳ deferred    | `tauri-plugin-notification` + port，R4 桶                                                                |
| clipboard get/set                                                                                                                                                                                                                   | ⏳ deferred    | Tauri 2 核心无 clipboard API → 需 `tauri-plugin-clipboard-manager` + port                                |
| `selectDirectory` / `selectFile` / `selectFiles` / `saveFile`                                                                                                                                                                       | ⏳ deferred    | `tauri-plugin-dialog` + port                                                                             |
| `getSystemLocale` / `setApplicationLocale`                                                                                                                                                                                          | ⏳ deferred    | locale/菜单语言同步属 S4 集成，未实现                                                                    |
| `getDesktopWindowChromeState`（含 macOS 圆角门槛）                                                                                                                                                                                  | ⏳ deferred    | 仅 `is_maximized` 可取；macOS 版本门槛逻辑需移植                                                         |
| `reportTelemetryEvent` / `reportArmsCustomEvent` / renderer trace / heap sample                                                                                                                                                     | ⏳ deferred    | 「Replace with OTLP」桶，先审计 ARMS-only 特性                                                           |
| `importChromeBrowserData` / Chrome 凭据发现                                                                                                                                                                                         | ⏳ deferred    | 「Rewrite (Rust upgrade)」桶：`windows` crate DPAPI                                                      |
| CUA 成员（`openCuaPermissionOnboarding` 等）                                                                                                                                                                                        | ⏳ deferred    | 「Defer」桶（CUA 现为 fail-closed 关闭）                                                                 |
| BrowserView 家族（`browserView*` / `onBrowserView*` / `onOpenBrowserUrl` …）                                                                                                                                                        | 🚪 设计门 OPEN | R4「Design gate before S4」：Tauri 无 BrowserView，child-window vs CDP 复合是产品决策，**S4 前必须先定** |
| 事件订阅族（`onFocusTab` / `onNewTab` / `onNewTask` / `onOpenWorkspace` / `onTaskNotificationClick` …）                                                                                                                             | ⏳ deferred    | renderer↔shell 事件桥未接入（S4 集成开放）                                                               |
| 同步族（`syncWindowTabs` / `syncWindowUnreadCount` / `syncActiveTaskSession` / `syncAppSettings` …）                                                                                                                                | ⏳ deferred    | 同上，未实现                                                                                             |
| `getDeviceId` / `exportLogs` / `captureWindowScreenshot` / `getInstalledEditors` / `openInEditor` / `openFeedback` / `openCommunity` / `getPathForFile` / `printPageToPdf` / `createTempTextAttachment` / MCP 目录读写 / 远程连接族 | ⏳ deferred    | 逐项移植未开始；远程连接族依赖 S3 host 接线                                                              |

**不声明任何未列出的对等能力。** 已实现 = 上表 ✅ 行；其余一律 deferred/OPEN。

## 开放项（S4 未完成）

- 真实 UI 资源接入（`packages/web` 构建 → `build.frontendDist`），当前是 `dist/index.html` 占位页；
- 交互窗口验证（headless 环境未运行 GUI，仅 `cargo check`/`cargo build`）；
- deep-link / updater / notification / clipboard / dialog 插件接入与策略移植；
- R4 BrowserView 设计门（产品决策，OPEN）；
- 与 Electron 窗口行为（托盘左键、关闭到托盘、AUMID）的逐项对等验证。

## 验证

```sh
cargo check --manifest-path native/Cargo.toml -p zcode-shell
cargo build --manifest-path native/Cargo.toml -p zcode-shell
cargo clippy --manifest-path native/Cargo.toml -p zcode-shell --all-targets -- -D warnings
cargo test  --manifest-path native/Cargo.toml -p zcode-shell
```

依赖变更需按 spec 重新生成 cargo 三方许可
（`node scripts/third-party-cargo.mjs --write && node scripts/licenses.mjs notices`）。
