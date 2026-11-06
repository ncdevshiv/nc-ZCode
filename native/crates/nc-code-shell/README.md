# zcode-shell — Tauri 2 桌面壳脚手架（S4）

Spec：`specs/rust-pure-native-roadmap.md` S4 + §7 R4 表；`specs/rust-native-crates.md`
S4 验收（**编译通过 + IPlatformService 形状的命令面**；交互窗口验证与 BrowserView 设计门
保持 OPEN）。productName `ZCode` / identifier `dev.zcode.app` 与 electron-builder 生产
identity 同族（`packages/desktop/scripts/desktop-product-identity.mjs`），版本 =
仓库根 `package.json` `version`（`tests/config_smoke.rs` 做漂移守护）。

## invoke 命令面（已实现子集）

`app_version`、`open_external_url`、`open_path`、`window_minimize`、
`window_toggle_maximize`、`window_close`、`window_hide`、`window_show`、
`platform_info`、`select_directory`、`select_file`、`select_files`、`save_file`、
`clipboard_write_text`、`clipboard_read_text`、`show_task_notification`、
`get_desktop_window_chrome_state`、`deep_link_current`、`desktop_command`、
`get_desktop_zoom_level`、`resource_manager_snapshot`、`print_page_to_pdf`、`is_docker_available`、
`list_wsl_distros`、`list_docker_containers`、`list_ssh_config_aliases`、
`report_telemetry_event`、`report_arms_custom_event`、`report_renderer_action_trace_batch`、
`report_renderer_heap_sample`、`report_local_ttft_batch`、`import_chrome_browser_data`、
`export_logs`、`capture_window_screenshot`、`get_installed_editors`、`open_in_editor`、
`load_mcp_from_user_directory`、`save_mcp_to_user_directory`、`migrate_legacy_common_mcp`、
`create_temp_text_attachment`、`get_device_id`、`sync_window_tabs`、`sync_window_unread_count`、`sync_active_task_session`、`sync_app_settings`、`browser_view_create`、
`browser_view_set_bounds`、`browser_view_set_visible`、`browser_view_set_occluded`、`browser_view_navigate`、
`browser_view_reload`、`browser_view_go_back`、`browser_view_go_forward`、
`browser_view_eval`、`browser_view_set_zoom`、`browser_view_open_devtools`、
`browser_view_state`、`browser_view_close`、`browser_view_attach_guest`、
`browser_view_detach_guest`、`browser_view_cdp`、`browser_use_lifecycle_result`、
`browser_use_lifecycle_probe`（仅 `ZCODE_SHELL_BROWSER_DEBUG=1`）、
`save_browser_recording`（workspace 内 WebM 落盘，校验相对路径/EBML/原子写）。

## ACL / loopback capability

主窗口加载的是 Rust 后端在 `http://127.0.0.1:<port>` 提供的真实 Web UI；Tauri 2
把它视为 **remote origin**，默认拒绝所有 app command。`capabilities/default.json`
+ `permissions/shell.toml` 显式放行：

- `remote.urls`: `http://127.0.0.1:*` / `http://localhost:*`；
- `webviews`: 只包含 `main` / `resource-manager`（child browser webview 不获得
  IPC 权限）；
- `permissions`: `core:default` + app 权限 `shell`（`commands.allow` 列出
  `generate_handler!` 的全部命令）。

`build.rs` 通过 `tauri_build::AppManifest` 注册 app ACL；`main.rs` 的
`command_permission_list_matches_invoke_handler` 测试保证权限清单与
`generate_handler!` 不漂移。

## Browser-use 生命周期桥

`listUserTabs` / `activateTab` / `claimTab` / `newTab` / `finalize` /
`finalizeTabs` / `markDeliverable` / `markHandoff` / `nameSession` /
`closeSession` / `recordingStart` / `recordingStatus` / `recordingCancel` /
`close` 由 renderer 的 side-pane 状态机拥有：
`ShellBrowserPort` 通过 `zcode:browser-use-lifecycle-request` 事件把命令发给
main webview，renderer 调用 `getBrowserUseLifecycleController()` 处理后用
`browser_use_lifecycle_result` 命令回传同一 `requestId`。请求表上限 32、10s
超时；未登录/无工作区时 `listUserTabs` 返回空列表，其余生命周期命令返回
`backend_unavailable`。`claimTab` 每个 tab 只能成功一次；`newTab` 由 renderer
创建 `browser-<uuid>` tab（child webview 在挂载时创建）；`finalize` 是单 tab
生命周期（默认 `deliverable`，`deliverable:false` 重置为 `active`）；
`nameSession` 保存 scope 名称；`closeSession` 释放 scope 内全部 tab 的
lifecycle/claim（tab 保持打开）并清除名称；`finalizeTabs` 只标记 `keep` 中的
tab（`deliverable` 释放回用户、`handoff` 保持 agent 所有），不关闭其余 tab；
`markDeliverable` 不释放 claim。`browser_use_lifecycle_probe` 是仅调试环境可用的
端到端探针。

`recordingStart` / `recordingStatus` / `recordingCancel` 由 renderer 的录制端口
执行：通过 `browser_view_cdp` 轮询 `Page.captureScreenshot`，canvas +
`MediaRecorder` 编码 WebM，完成后经 `save_browser_recording` 写入 workspace
（默认 `browser-recordings/<job>.webm`；`recordingStatus.outputPath` 可指定
workspace 相对路径）。`recordingStart` 同 tab 二次启动返回 `execution_error`；
没有录制端口（Web/Electron preload/远程 workspace）返回
`capability_unsupported`；脚本化 `options.actions` 显式拒绝；`showCursor`
暂不绘制光标覆盖层。`close`/`closeSession` 会取消对应运行中的录制。

Tray 菜单与 Electron 对齐（Open ZCode / New Task / Open Workspace / About ZCode /
Clear All Data / Quit）；图标为运行期程序生成的 16x16 RGBA（不携带二进制图标）。

## Browser 自检（`--browser-selftest`）

`zcode-shell.exe --browser-selftest` 是真实 WebView2 的诊断模式（不是产品入口）：
创建一个隐藏 helper 页面的 child webview，并调用与 agent 相同的
`ShellBrowserPort` 依次执行 `navigate` → `snapshot` → `elementInfo` → `click` →
`fill` → `evaluate` → `screenshot` → `getState`，最后再走一遍 host 的
`Browser` 工具封装。结果以 JSON 行打印到 stderr，PASS/FAIL 决定退出码。

2026-11-04 Windows 实测：`navigate ok`（179ms）、`snapshot elements=2`
（button=e1 / input=e2）、`elementInfo tag="button"`、`click title="clicked"`、
`fill/evaluate value="hello"`、`screenshot base64_bytes=12720`、
`tool getState ok bytes=348`、`PASS`、`EXITCODE=0`。

## 应用菜单

File / Edit / View / Window / Help 原生菜单（zh-CN / en-US）：

- File：New Task、New Tab（发 `zcode:new-tab`）、Open Workspace、Close Window；
- Edit：Undo/Redo/Cut/Copy/Paste/Select All（Tauri predefined role）；
- View：Toggle Full Screen、Zoom In/Out/Actual Size（菜单直接调用壳层 zoom）；
- Window：Minimize；
- Help：About（原生消息框）、Feedback（内置 `config/default.json` 的 `feedback_url`）、
  Export Logs（调用 `export_logs`）、Resource Manager（原生子窗口，见下）、
  Toggle Developer Tools、Clear All Data
  （确认后删除 `~/.zcode/v2`、清 webview 浏览数据并重启；失败时显示具体错误且不重启）。

## Resource Manager

`openResourceManager`（Help 菜单或 renderer 的 `executeDesktopCommand`）打开
`resource-manager.html` 原生子窗口；窗口每 2 秒调用 `resource_manager_snapshot`：

- 系统：OS / 版本 / 内核 / 主机名、逻辑与物理核心数、总内存/已用内存、全局 CPU 使用率；
- 进程：名称匹配 `zcode` / `nc-code` 的进程（含当前壳进程），按内存降序，最多 50 条，
  返回 `{pid, name, memoryBytes, cpuUsagePercent}`。

采样基于 `sysinfo`，不依赖 Electron 的 `app.getAppMetrics()`；读取失败时窗口显示具体错误，
不伪造数据。

`syncAppSettings` 的 `locale` / `shortcutBindings` 会重建菜单：标签切换语言，
accelerator 按 `shortcutBindings` 覆盖（空数组禁用，非法绑定回退默认）。

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
| tray / 原生菜单 / 窗口 chrome 细节                                                                                                                                                                                                  | ✅ 已实现      | tray 菜单与 Electron 对齐；窗口 chrome 状态子集；macOS 圆角门槛仍 deferred                               |
| `onUpdateReady` / `downloadUpdate` / `quitAndInstallUpdate` / `getUpdateState` / `skipUpdateVersion` …                                                                                                                              | ⏳ blocked     | `tauri-plugin-updater` + 策略移植；需更新 feed URL 与签名密钥（用户提供）                                |
| `registerOAuthState` / `onOAuthCallback` / `onPaymentCallback` / `onShareImport` / `notifyRendererReady`（deep link `zcode://`）                                                                                                    | ✅ 已实现      | `tauri-plugin-deep-link` + renderer 路由过滤/OAuth state 校验；安装包 scheme 注册由 installer 负责        |
| `showTaskNotification`                                                                                                                                                                                                              | ✅ 已实现      | `tauri-plugin-notification` → `show_task_notification`                                                   |
| clipboard get/set                                                                                                                                                                                                                   | ✅ 已实现      | `tauri-plugin-clipboard-manager` → `clipboard_read_text` / `clipboard_write_text`                        |
| `selectDirectory` / `selectFile` / `selectFiles` / `saveFile`                                                                                                                                                                       | ✅ 已实现      | `tauri-plugin-dialog`；`save_file` 含 50 MiB 上限与公网地址校验（重定向/钉 DNS）                          |
| `getSystemLocale` / `setApplicationLocale`                                                                                                                                                                                          | ⏳ deferred    | locale/菜单语言同步属 S4 集成，未实现                                                                    |
| `getDesktopWindowChromeState`（含 macOS 圆角门槛）                                                                                                                                                                                  | ⚠️ 部分        | `isMaximized` / `isFullscreen` 可取；macOS 版本门槛逻辑需移植                                             |
| `reportTelemetryEvent` / `reportArmsCustomEvent`                                                                                                                                                                                    | ✅ 已实现      | `report_telemetry_event` / `report_arms_custom_event` → Rust OTLP/HTTP JSON（`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` 或 `OTEL_EXPORTER_OTLP_ENDPOINT` + `/v1/logs`）；未配置 endpoint 时丢弃，不落盘 |
| renderer trace / heap sample / local TTFT                                                                                                                                                                                          | ✅ 已实现      | `report_renderer_action_trace_batch` → OTLP traces；`report_renderer_heap_sample` → `renderer.heap.used` gauge；`report_local_ttft_batch` → `renderer.ttft` gauge；信号 endpoint 各自独立 |
| `importChromeBrowserData` / Chrome 凭据发现                                                                                                                                                                                         | ⚠️ 部分        | Windows：复制 Chrome profile 的 `Network/Cookies`，DPAPI 解 `Local State` AES key + `v10`/`v11` AES-GCM，CDP `Network.setCookie` 导入；Local Storage 扫描 `META:<origin>`，临时 profile + Chrome helper `DOMStorage.getDOMStorageItems` 后 CDP 写回。macOS：Keychain `Chrome Safe Storage` + PBKDF2-SHA1(1003) + AES-128-CBC(`v10`, 16 空格 IV) + Tauri `set_cookie`；Linux：同一 CBC 路径，Chrome 文档回退口令 `peanuts`（1 次迭代）+ `set_cookie`；Chrome schema ≥24 的 32 字节 host digest 会剥离。macOS/Linux 的 Local Storage 无跨源写入 API（WKWebView/WebKitGTK 无 CDP `DOMStorage`），显式返回 `chrome_local_storage_import_failed` 并计入 `originsFailed`；App-Bound key 返回 elevation/app-bound 错误 |
| CUA 成员（`openCuaPermissionOnboarding` 等）                                                                                                                                                                                        | ⏳ deferred    | 「Defer」桶（CUA 现为 fail-closed 关闭）                                                                 |
| BrowserView 家族（`browserView*` / `onBrowserView*` / `onOpenBrowserUrl` …）                                                                                                                                                        | ⚠️ 部分        | Tauri 2 child webview 已实现 human 浏览（create/bounds/visible/navigate/reload/back/forward/eval/zoom/devtools/close + 事件）；`browser_view_attach_guest` / `browser_view_detach_guest` / `browser_view_cdp` 在 Windows 走 WebView2 `CallDevToolsProtocolMethod`，macOS/Linux 显式 `not-webview` / `browser_view_cdp_unsupported`；**agent 侧已由 Rust host 的 `Browser` 工具消费**（`ShellBrowserPort` → `browser_view_cdp`，核心命令 navigate/snapshot/click/fill/type/press/scroll/waitFor/evaluate/screenshot/getState/list），TS host 不再参与 Tauri 运行 |
| 事件订阅族（`onFocusTab` / `onNewTab` / `onNewTask` / `onOpenWorkspace` / `onTaskNotificationClick` …）                                                                                                                             | ⚠️ 部分        | `onNewTask` / `onOpenWorkspace` / zoom / deep link / `onFocusTab`（`zcode:focus-tab`）/ `onNewTab`（应用菜单 New Tab → `zcode:new-tab`）/ `onTaskNotificationClick`（`zcode:task-notification-click`，通知 id→taskId 注册表）已接入；其余事件桥未移植 |
| 同步族（`syncWindowTabs` / `syncWindowUnreadCount` / `syncActiveTaskSession` / `syncAppSettings` …）                                                                                                                                | ⚠️ 部分        | `syncWindowTabs` 记录窗口 tab 集（去重/上限 200）；`syncWindowUnreadCount` 在 macOS/Linux 调 `set_badge_count`（Windows 与 Electron 一致不做 badge）；`syncActiveTaskSession` 记录 trim 后 ≤255 字符的 active session；`syncAppSettings` 应用 `closeToTrayOnWindows`、`keepAwakeWhileRunning`（Windows `SetThreadExecutionState` / macOS `caffeinate` / Linux `systemd-inhibit`）以及 `locale`/`shortcutBindings`（重建应用菜单标签与 accelerator）；其余设置键由 renderer 持久化 |
| `isDockerAvailable` / `listWSLDistros` / `listDockerContainers` / `listSSHConfigAliases`                                                                                                                                            | ✅ 已实现      | Rust 原生探测（docker CLI、`wsl.exe -l -v`、`~/.ssh/config` 含 Include）；解析器有单元测试                |
| `connectRemote` / `cancelPendingRemoteConnection` / `bindRemoteWorkspaceSessionContext` / `disposeRemoteSession`                                                                                                                     | ⏳ deferred    | 远程 session host 仍在 TypeScript host；Tauri adapter 显式返回 unsupported，不伪装成功                    |
| `getDeviceId` / `exportLogs` / `captureWindowScreenshot` / `getInstalledEditors` / `openInEditor` / `openFeedback` / `openCommunity` / `getPathForFile` / `printPageToPdf` / `createTempTextAttachment` / MCP 目录读写               | ⚠️ 部分        | `exportLogs`、`captureWindowScreenshot`（Windows WebView2 `CapturePreview` / macOS `WKWebView takeSnapshot` / Linux WebKitGTK snapshot → base64 PNG；失败返回 null）、`printPageToPdf`（Windows WebView2 `PrintToPdf` 不变；macOS `WKWebView.createPDFWithConfiguration`，macOS 11 以下显式 `print_unsupported`；Linux `WebKitPrintOperation` + GTK `Print to File`/`output-uri`/pdf；三条路径都校验 `%PDF-` 魔数，空/非 PDF 返回 `print_failed`）、`getInstalledEditors`/`openInEditor`、MCP 目录读写 + `migrateLegacyCommonMcp`、`createTempTextAttachment`（`~/.zcode/tmp/paste-attachments/<date>/`，32 MiB 上限，文件名净化 + 唯一后缀）、`getDeviceId`（`~/.zcode/v2/telemetry-state.json` 的 `deviceMid`，与 CLI/telemetry 共用，首渲染前 prime）、`openFeedback`/`openCommunity`（沿用 web 配置解析，经 Rust opener 打开系统浏览器）、原生文件拖放（`IPlatformService.onFileDropPaths` ← Tauri `onDragDropEvent`，按窗口物理坐标命中 pane 后进入 composer `addAttachmentLocalPaths`）已实现；Tauri 拦截 OS drop 后 DOM 无 `File`，因此 `getPathForFile` 在 Tauri 不适用，Web/Electron 仍走 File bytes 兜底 |

**不声明任何未列出的对等能力。** 已实现 = 上表 ✅ 行；其余一律 deferred/OPEN。

## 开放项（S4 未完成）

- updater（需更新 feed URL + 签名密钥，用户提供后才能实现/验证）；
- 远程 session host（`connectRemote` 等）仍在 TypeScript host，Rust 侧只做本地目标探测；
- CUA；
- BrowserView agent 控制面：Windows 已由 Rust host 的 `Browser` 工具 → `ShellBrowserPort` → `browser_view_cdp` 打通核心命令；macOS/Linux 无 CDP（显式 `capability_unsupported`）；Playwright locator/recording/claimTab 等完整 browser-client 面未移植；
- BrowserView native-layer 遮挡：`browser_view_set_occluded` 把“渲染器请求可见”与“DOM 遮挡”分开记录，有效可见性 = `visible && !occluded`；renderer 的 `browserUseOcclusion.ts` 监听 Radix popper/menu/dialog/tooltip/toast 与显式 `data-browser-occluder`，与浏览器 pane 相交时隐藏 child webview，关闭后恢复；`--browser-selftest` 通过原生 `IsWindowVisible` 验证隐藏/恢复；
- 交互窗口验证（headless 环境未运行 GUI，仅 `cargo check`/`cargo build`/单测）；Browser 的 `--browser-selftest` 已补真实 WebView2 验证；
- printPageToPdf 的平台验证：Windows 路径保持原样；Linux 分支已在 WSL（Ubuntu 26.04，WebKitGTK 2.52.6 / GTK 3.24）用真实 `webkit2gtk 2.0.2` + `gtk 0.18` 编译通过（Tauri 最小 shim）；完整 Linux app 构建因缺少 `libayatana-appindicator3-dev`/`librsvg2-dev`/`libxdo-dev`/`libssl-dev`（需 sudo）未执行；macOS 分支无 Apple SDK，仅按缓存 crate 源码做 API 核对与 `rustfmt` 解析；
- Chrome import 的平台验证：Windows 路径保持原样（DPAPI/AES-GCM + CDP）；macOS/Linux cookie 路径已实现（Keychain/`peanuts` + PBKDF2-SHA1 + AES-128-CBC + `set_cookie`），Linux 分支已在 WSL 用真实 `rusqlite`/`cookie`/`aes`/`cbc` 编译通过；macOS Keychain 分支仅 API 核对；macOS/Linux 的 Local Storage 无跨源写入 API，显式返回 `chrome_local_storage_import_failed`；本机未用真实 Chrome profile 做运行时导入验证；
- nc-code-host 全量测试：最新 `nc-host-full-test.log` 记录 50 个 test target、0 failed（含 doc-tests 收尾）；早期日志的 3 个失败（throughput/tool-progress/workflow fixture）在后续运行中已全绿；
- R4 BrowserView 设计门已选 Tauri 2 child webview，命令面待移植；
- 与 Electron 窗口行为（托盘左键、关闭到托盘、AUMID）的逐项对等验证。

## 验证

```sh
cargo check --manifest-path native/Cargo.toml -p nc-code-shell
cargo build --manifest-path native/Cargo.toml -p nc-code-shell
cargo clippy --manifest-path native/Cargo.toml -p nc-code-shell --all-targets -- -D warnings
cargo test  --manifest-path native/Cargo.toml -p nc-code-shell
```

依赖变更需按 spec 重新生成 cargo 三方许可
（`node scripts/third-party-cargo.mjs --write && node scripts/licenses.mjs notices`）。

## Recording selftest (`--browser-recording-selftest`)

`zcode-shell.exe --browser-recording-selftest` creates a real child webview and
opens the local `recording-selftest.html` page. The page captures the child over
`browser_view_cdp`, encodes WebM with `MediaRecorder`, writes through
`save_browser_recording`, and reports the artifact back to the shell; the shell
verifies the workspace path, byte count and EBML magic before PASS.

2026-11-04 Windows run (WebView2): 15 frames, `video/webm;codecs=vp9`, 3 scripted CDP actions, cursor overlay visible (pixel 255,73,82), 23,294 bytes; `ffprobe` reports `vp9`, 800x600, `matroska,webm`, ~1.95s.
Set `ZCODE_RECORDING_SELFTEST_KEEP=1` to keep the workspace for inspection.

## Playwright locator subset (`playwright`)

`browser_control.rs` routes `playwright` / `playwrightWaitForTimeout` to
`browser_playwright.rs`. It implements the locator operation surface over a
documented selector subset: CSS, `css=`, `id=`, `data-testid=`, `text=`
(substring; quoted exact), `role=<role>[name="..."]`, `xpath=`, `nth=` and `>>`
chaining, plus the `internal:` selectors the TS client emits:
`internal:role=`, `internal:text=` (`i` = case-insensitive substring, `s`/quoted
= normalized whole-text equality), `internal:label=`,
`internal:attr=[name=matcher]`, `internal:testid=[data-testid=…s]`,
`internal:and="sel"`, `internal:or="sel"`, `internal:has="sel"`,
`internal:has-not="sel"`, `internal:has-text=…`, `internal:has-not-text=…`, and
`internal:control=enter-frame` (same-origin iframes). Unknown `internal:`
selectors and cross-origin frame entry fail explicitly; `fileChooserSetFiles`
stays `capability_unsupported` (IAB file uploads are unsupported by the TS
product too). `elementScreenshot` is implemented via a clipped
`Page.captureScreenshot`.

Downloads: every Windows child webview registers
`ICoreWebView2_4::add_DownloadStarting`; the handler forces the destination into
`%TEMP%\zcode-browser-downloads` and records `{id, tab, path}` in
`BrowserViewRegistry`. `waitForEvent("download")` returns the new id and
`downloadPath` polls until the file exists. Non-Windows platforms report
`capability_unsupported` because there is no WebView2 handler.
`ZCODE_SHELL_DOWNLOAD_DEBUG=1` prints registration diagnostics to stderr.

Locator operations: `count`, `allTextContents`, `isVisible`, `isEnabled`,
`textContent`, `innerText`, `getAttribute`, `evaluate`, `waitFor`, `click`,
`dblclick`, `fill`, `press`, `setChecked`, `selectOption`, `downloadMedia`.
Actions use trusted CDP input/evaluation and poll actionability up to the
Playwright default 3s timeout.

## Window selftest (`--window-selftest`)

`zcode-shell.exe --window-selftest` is the headless interactive-window backend
gate: it resolves the static dir with the same precedence as `backend.rs`
(`ZCODE_WEB_DIST` → build-staged `$OUT_DIR/web` → resource `web`), builds the
real registry via `nc_code_app::build`, serves on an ephemeral loopback port,
and fetches `/` + `/api/server-info` over real HTTP. PASS proves the window
can load and boot against the embedded Rust backend with no TS runtime.

2026-11-06 run (dev checkout, `packages/web/dist` present):
`[window-selftest] PASS: origin=http://127.0.0.1:50910/ channels=39`.

2026-11-04 `--browser-selftest` live result: `playwright count: 1`,
`playwright textContent: "Go"`, `playwright fill: value="locator"`,
`playwright click: title="clicked"`, `playwright xpath count: 1`,
`playwright chain count: 1`, `playwright nth count: 1`,
`playwright elementScreenshot: base64_bytes=600`; internal role/name/text/
testid/attr/label/and/or/has/has-not/has-text/frame counts all match;
`playwright download: id=iab-download:… bytes=17 text="selftest-download"`,
PASS/EXITCODE=0.

## Computer use (`--computer-selftest`)

`computer_control.rs` implements the host `ComputerControlPort`:

- `listApps` / `getState` / `screenshot` / `activate` / `move` / `click` /
  `scroll` / `type` / `press` / `drag` / `wait`;
- Windows: `EnumWindows` + process names, `GetForegroundWindow`/`GetCursorPos`/
  `GetSystemMetrics`, GDI `BitBlt` + PNG encoding, and `SendInput` for mouse,
  wheel and keyboard (UTF-16 unicode for `type`, modifier chords for `press`);
- non-Windows: explicit `capability_unsupported`;
- `activate` reports `foreground_denied` when Windows refuses;
- screenshots are capped at 64 MP and `maxBytes` (default 8 MiB).

`zcode-shell.exe --computer-selftest` creates a real local window, then drives
it through the same port and verifies page state:

```
[computer-selftest] listApps: windowId=15075744 pid=31244 origin=(128,151)
[computer-selftest] activate: foreground granted
[computer-selftest] type: input value="hello"
[computer-selftest] click: button clicked="1"
[computer-selftest] scroll probe: {"y":300,"wheels":1,"height":2000,"inner":500}
[computer-selftest] press: Tab accepted
[computer-selftest] screenshot: 17344 bytes, 716x539
[computer-selftest] drag: cursor=(212,255)
[computer-selftest] wait: 20ms
[computer-selftest] PASS
```
