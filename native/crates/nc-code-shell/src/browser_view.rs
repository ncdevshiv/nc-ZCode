//! Tauri child-webview browser guest for the embedded browser pane.
//!
//! The Electron renderer used `<webview>` guests and attached CDP to them. The
//! Tauri shell replaces the guest with a child [`tauri::Webview`]: it renders the
//! page natively, is positioned over the renderer container, and exposes
//! navigation/eval/zoom/devtools through commands. The renderer facade in
//! `packages/ui/src/browser-use/tauriBrowserGuest.ts` maps these commands onto the
//! `ElectronWebviewTag` subset that `UnifiedBrowserView` already consumes.
//!
//! CDP attachment (`browserViewAttachGuest`) is not part of this module: the
//! TypeScript host still owns the agent control plane, so the Tauri adapter does
//! not pretend a CDP session exists.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{
    webview::PageLoadEvent, Emitter, LogicalPosition, LogicalSize, Manager, Url, Webview,
    WebviewUrl, Window,
};
#[cfg(windows)]
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_4;
#[cfg(windows)]
use windows::core::{Interface, HSTRING};

const EVENT_NAME: &str = "zcode:browser-view:event";
const MAX_HISTORY: usize = 100;
const EVAL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EVAL_RESULT_BYTES: usize = 1_000_000;

const PAGE_METADATA_SCRIPT: &str = r#"JSON.stringify({
  title: document.title || "",
  favicon: (document.querySelector('link[rel~="icon"]') || {}).href || ""
})"#;

#[derive(Default, Clone)]
pub struct BrowserViewRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    downloads: Arc<Mutex<HashMap<String, BrowserDownloadRecord>>>,
}

#[derive(Clone)]
struct BrowserDownloadRecord {
    path: PathBuf,
    tab_id: String,
}

#[derive(Default)]
struct RegistryInner {
    next_id: u64,
    entries: HashMap<String, BrowserViewEntry>,
}

struct BrowserViewEntry {
    webview: Webview,
    /// Creation order; the highest visible id is the active tab.
    created_id: u64,
    history: Vec<String>,
    history_index: usize,
    title: String,
    visible: bool,
    /// A DOM overlay currently covers this webview; effective visibility is
    /// `visible && !occluded` because the Tauri child webview is a native layer
    /// that cannot be painted under DOM.
    occluded: bool,
    cdp_attached: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserViewBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserViewCreatePayload {
    pub key: String,
    pub url: String,
    pub bounds: BrowserViewBounds,
    #[serde(default)]
    pub visible: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserViewCreateResult {
    pub webview_id: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserViewState {
    pub url: String,
    pub title: String,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    /// The renderer requested this tab to be shown.
    pub visible: bool,
    /// A DOM overlay currently covers the webview.
    pub occluded: bool,
    /// Whether the native webview is actually shown (`visible && !occluded`).
    pub effective_visible: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserGuestAttachResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_requested: Option<bool>,
}

impl BrowserGuestAttachResult {
    fn attached(generation: u64) -> Self {
        Self {
            ok: true,
            guest_generation: Some(generation),
            reason: None,
            recovery_requested: None,
        }
    }

    fn rejected(reason: &str) -> Self {
        Self {
            ok: false,
            guest_generation: None,
            reason: Some(reason.to_string()),
            recovery_requested: Some(false),
        }
    }
}

impl BrowserViewRegistry {
    fn webview(&self, key: &str) -> Option<Webview> {
        self.inner
            .lock()
            .ok()?
            .entries
            .get(key)
            .map(|entry| entry.webview.clone())
    }

    /// The tab the user is currently looking at: the newest visible entry, or the
    /// newest entry when nothing is marked visible. The agent's `Browser` tool
    /// targets this tab because the renderer owns tab ids and the host cannot.
    pub fn register_download(&self, tab_id: &str, path: PathBuf) -> String {
        let id = format!("iab-download:{}", uuid::Uuid::new_v4());
        if let Ok(mut guard) = self.downloads.lock() {
            guard.insert(
                id.clone(),
                BrowserDownloadRecord {
                    path,
                    tab_id: tab_id.to_string(),
                },
            );
        }
        id
    }

    pub fn download_ids(&self, tab_id: &str) -> Vec<String> {
        self.downloads
            .lock()
            .map(|guard| {
                guard
                    .iter()
                    .filter(|(_, record)| record.tab_id == tab_id)
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn download_path(&self, id: &str, tab_id: &str) -> Option<PathBuf> {
        self.downloads
            .lock()
            .ok()
            .and_then(|guard| guard.get(id).cloned())
            .filter(|record| record.tab_id == tab_id)
            .map(|record| record.path)
    }

    pub fn active_key(&self) -> Option<String> {
        let inner = self.inner.lock().ok()?;
        inner
            .entries
            .iter()
            .filter(|(_, entry)| entry.visible)
            .max_by_key(|(_, entry)| entry.created_id)
            .or_else(|| {
                inner
                    .entries
                    .iter()
                    .max_by_key(|(_, entry)| entry.created_id)
            })
            .map(|(key, _)| key.clone())
    }

    /// The active tab's creation id (stable per webview); used as the browser
    /// backend generation.
    pub fn active_id(&self) -> Option<u64> {
        let inner = self.inner.lock().ok()?;
        inner
            .entries
            .iter()
            .filter(|(_, entry)| entry.visible)
            .max_by_key(|(_, entry)| entry.created_id)
            .or_else(|| {
                inner
                    .entries
                    .iter()
                    .max_by_key(|(_, entry)| entry.created_id)
            })
            .map(|(_, entry)| entry.created_id)
    }

    /// Dispatch a raw CDP method to the active (or named) child webview.
    /// Windows uses WebView2 `CallDevToolsProtocolMethod`; other platforms have no
    /// CDP endpoint and return `browser_view_cdp_unsupported`.
    pub fn dispatch_cdp(
        &self,
        key: Option<&str>,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<String, String> {
        let key = match key {
            Some(key) => key.to_string(),
            None => self
                .active_key()
                .ok_or_else(|| "browser_view_not_found".to_string())?,
        };
        let webview = self.webview(&key).ok_or("browser_view_not_found")?;
        #[cfg(windows)]
        {
            call_cdp(&webview, method.to_string(), params.to_string())
        }
        #[cfg(not(windows))]
        {
            let _ = webview;
            Err("browser_view_cdp_unsupported".to_string())
        }
    }

    fn insert(&self, key: String, entry: BrowserViewEntry) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.entries.insert(key, entry);
        }
    }

    fn remove(&self, key: &str) -> Option<Webview> {
        self.inner
            .lock()
            .ok()?
            .entries
            .remove(key)
            .map(|entry| entry.webview)
    }

    fn reserve_id(&self, key: &str) -> Result<(u64, String), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "browser_view_state_poisoned".to_string())?;
        if inner.entries.contains_key(key) {
            return Err("browser_view_exists".to_string());
        }
        inner.next_id += 1;
        let id = inner.next_id;
        Ok((id, format!("browser-view-{id}")))
    }

    fn record_navigation(&self, key: &str, url: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let Some(entry) = inner.entries.get_mut(key) else {
            return;
        };
        if entry.history.get(entry.history_index).map(String::as_str) == Some(url) {
            return;
        }
        if entry
            .history
            .get(entry.history_index + 1)
            .map(String::as_str)
            == Some(url)
        {
            entry.history_index += 1;
            return;
        }
        if entry.history_index > 0
            && entry
                .history
                .get(entry.history_index - 1)
                .map(String::as_str)
                == Some(url)
        {
            entry.history_index -= 1;
            return;
        }
        entry.history.truncate(entry.history_index + 1);
        entry.history.push(url.to_string());
        if entry.history.len() > MAX_HISTORY {
            entry.history.remove(0);
        }
        entry.history_index = entry.history.len() - 1;
    }

    fn set_title(&self, key: &str, title: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(entry) = inner.entries.get_mut(key) {
                entry.title = title.to_string();
            }
        }
    }

    fn set_visible(&self, key: &str, visible: bool) -> Option<bool> {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(entry) = inner.entries.get_mut(key) {
                entry.visible = visible;
                return Some(entry.visible && !entry.occluded);
            }
        }
        None
    }

    fn set_occluded(&self, key: &str, occluded: bool) -> Option<bool> {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(entry) = inner.entries.get_mut(key) {
                entry.occluded = occluded;
                return Some(entry.visible && !entry.occluded);
            }
        }
        None
    }

    pub(crate) fn apply_occluded(
        &self,
        key: &str,
        occluded: bool,
    ) -> Result<BrowserViewState, String> {
        let webview = self.webview(key).ok_or("browser_view_not_found")?;
        let effective = self
            .set_occluded(key, occluded)
            .ok_or("browser_view_not_found")?;
        apply_webview_visibility(&webview, effective)?;
        self.state(key)
            .ok_or_else(|| "browser_view_not_found".to_string())
    }

    fn set_cdp_attached(&self, key: &str, attached: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(entry) = inner.entries.get_mut(key) {
                entry.cdp_attached = attached;
            }
        }
    }

    pub(crate) fn state(&self, key: &str) -> Option<BrowserViewState> {
        let inner = self.inner.lock().ok()?;
        let entry = inner.entries.get(key)?;
        let url = entry
            .webview
            .url()
            .map(|url| url.to_string())
            .ok()
            .filter(|url| !url.is_empty())
            .or_else(|| entry.history.get(entry.history_index).cloned())
            .unwrap_or_default();
        Some(BrowserViewState {
            url,
            title: entry.title.clone(),
            can_go_back: entry.history_index > 0,
            can_go_forward: entry.history_index + 1 < entry.history.len(),
            visible: entry.visible,
            occluded: entry.occluded,
            effective_visible: effective_visible(entry.visible, entry.occluded),
        })
    }
}

fn effective_visible(visible: bool, occluded: bool) -> bool {
    visible && !occluded
}

#[cfg(test)]
mod effective_visible_test {
    use super::effective_visible;
    #[test]
    fn truth_table() {
        assert!(effective_visible(true, false));
        assert!(!effective_visible(true, true));
        assert!(!effective_visible(false, false));
        assert!(!effective_visible(false, true));
    }
}

#[allow(dead_code)]
fn effective_visible_unchecked(visible: bool, occluded: bool) -> bool {
    visible && !occluded
}

fn is_allowed_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "file") || url.as_str() == "about:blank"
}

fn parse_allowed_url(value: &str) -> Result<Url, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("browser_view_invalid_url".to_string());
    }
    let url = Url::parse(trimmed).map_err(|_| "browser_view_invalid_url".to_string())?;
    if !is_allowed_url(&url) {
        return Err("browser_view_url_not_allowed".to_string());
    }
    Ok(url)
}

/// Navigation gate shared by the renderer command and the agent `Browser` tool.
pub(crate) fn validate_navigation_url(value: &str) -> Result<String, String> {
    parse_allowed_url(value).map(|url| url.to_string())
}

fn emit_event(app: &tauri::AppHandle, key: &str, payload: serde_json::Value) {
    let mut body = payload;
    if let Some(object) = body.as_object_mut() {
        object.insert("key".to_string(), json!(key));
    }
    let _ = app.emit(EVENT_NAME, body);
}

fn parse_page_metadata(raw: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    // `eval_with_callback` serializes the JS result; a script returning a string
    // yields a JSON string containing the JSON object.
    let value = match value {
        serde_json::Value::String(inner) => serde_json::from_str(&inner).ok()?,
        other => other,
    };
    let title = value
        .get("title")
        .and_then(|item| item.as_str())
        .unwrap_or_default()
        .to_string();
    let favicon = value
        .get("favicon")
        .and_then(|item| item.as_str())
        .unwrap_or_default()
        .to_string();
    Some((title, favicon))
}

/// Create a child webview for `payload.key`, positioned over the renderer
/// container. Re-creating an existing key is rejected so a stale guest cannot be
/// silently reused.
#[tauri::command]
pub async fn browser_view_create(
    window: Window,
    state: tauri::State<'_, BrowserViewRegistry>,
    payload: BrowserViewCreatePayload,
) -> Result<BrowserViewCreateResult, String> {
    create_webview(&window, &state, payload)
}

/// Shared by the renderer command and the `--browser-selftest` diagnostic.
pub(crate) fn create_webview(
    window: &Window,
    state: &BrowserViewRegistry,
    payload: BrowserViewCreatePayload,
) -> Result<BrowserViewCreateResult, String> {
    let url = parse_allowed_url(&payload.url)?;
    let (webview_id, label) = state.reserve_id(&payload.key)?;
    let key = payload.key.clone();
    let app = window.app_handle().clone();
    let page_load_key = key.clone();
    let page_load_app = app.clone();
    let webview = window
        .add_child(
            tauri::WebviewBuilder::new(label, WebviewUrl::External(url.clone()))
                .on_navigation(is_allowed_url)
                .on_page_load(move |webview, payload| {
                    let url = payload.url().to_string();
                    match payload.event() {
                        PageLoadEvent::Started => {
                            emit_event(
                                &page_load_app,
                                &page_load_key,
                                json!({ "type": "did-start-loading", "url": url }),
                            );
                        }
                        PageLoadEvent::Finished => {
                            if let Some(registry) = page_load_app.try_state::<BrowserViewRegistry>()
                            {
                                registry.record_navigation(&page_load_key, &url);
                            }
                            emit_event(
                                &page_load_app,
                                &page_load_key,
                                json!({ "type": "dom-ready", "url": url }),
                            );
                            emit_event(
                                &page_load_app,
                                &page_load_key,
                                json!({ "type": "did-navigate", "url": url }),
                            );
                            emit_event(
                                &page_load_app,
                                &page_load_key,
                                json!({ "type": "did-stop-loading", "url": url }),
                            );
                            let metadata_app = page_load_app.clone();
                            let metadata_key = page_load_key.clone();
                            let _ = webview.eval_with_callback(PAGE_METADATA_SCRIPT, move |raw| {
                                let Some((title, favicon)) = parse_page_metadata(&raw) else {
                                    return;
                                };
                                if let Some(registry) =
                                    metadata_app.try_state::<BrowserViewRegistry>()
                                {
                                    registry.set_title(&metadata_key, &title);
                                }
                                emit_event(
                                    &metadata_app,
                                    &metadata_key,
                                    json!({ "type": "page-title-updated", "title": title }),
                                );
                                if !favicon.is_empty() {
                                    emit_event(
                                        &metadata_app,
                                        &metadata_key,
                                        json!({
                                            "type": "page-favicon-updated",
                                            "favicons": [favicon],
                                        }),
                                    );
                                }
                            });
                        }
                    }
                }),
            LogicalPosition::new(payload.bounds.x, payload.bounds.y),
            LogicalSize::new(
                payload.bounds.width.max(1.0),
                payload.bounds.height.max(1.0),
            ),
        )
        .map_err(|error| format!("browser_view_create_failed: {error}"))?;
    #[cfg(windows)]
    {
        let download_registry = state.clone();
        let download_key = key.clone();
        let debug = std::env::var("ZCODE_SHELL_DOWNLOAD_DEBUG").ok().as_deref() == Some("1");
        let _ = webview.with_webview(move |platform| unsafe {
            let controller = platform.controller();
            let Ok(core) = controller.CoreWebView2() else {
                return;
            };
            let Ok(core4) = core.cast::<ICoreWebView2_4>() else {
                return;
            };
            let handler = webview2_com::DownloadStartingEventHandler::create(Box::new(
                move |_sender, args| {
                    if let Some(args) = args {
                        let dir = std::env::temp_dir().join("zcode-browser-downloads");
                        let _ = std::fs::create_dir_all(&dir);
                        let path = dir.join(format!("{}-download.bin", uuid::Uuid::new_v4()));
                        let _ =
                            args.SetResultFilePath(&HSTRING::from(path.to_string_lossy().as_ref()));
                        download_registry.register_download(&download_key, path.clone());
                        if debug {
                            eprintln!("[download-debug] registered {}", path.display());
                        }
                    }
                    Ok(())
                },
            ));
            let mut token = 0;
            if let Err(error) = core4.add_DownloadStarting(&handler, &mut token) {
                if debug {
                    eprintln!("[download-debug] add_DownloadStarting failed: {error}");
                }
            } else if debug {
                eprintln!("[download-debug] add_DownloadStarting ok token={token}");
            }
        });
    }
    let visible = payload.visible.unwrap_or(true);
    if !visible {
        let _ = webview.hide();
    }
    state.insert(
        key,
        BrowserViewEntry {
            webview,
            created_id: webview_id,
            history: vec![url.to_string()],
            history_index: 0,
            title: String::new(),
            visible,
            occluded: false,
            cdp_attached: false,
        },
    );
    Ok(BrowserViewCreateResult { webview_id })
}

/// Resize/reposition the child webview; values are logical CSS pixels relative
/// to the window content area (the renderer container's bounding rect).
#[tauri::command]
pub fn browser_view_set_bounds(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
    bounds: BrowserViewBounds,
) -> Result<(), String> {
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    webview
        .set_position(LogicalPosition::new(bounds.x, bounds.y))
        .map_err(|error| error.to_string())?;
    webview
        .set_size(LogicalSize::new(
            bounds.width.max(1.0),
            bounds.height.max(1.0),
        ))
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn browser_view_set_visible(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
    visible: bool,
) -> Result<(), String> {
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    let effective = state
        .set_visible(&key, visible)
        .ok_or("browser_view_not_found")?;
    apply_webview_visibility(&webview, effective)
}

/// Hide/show a browser child webview because a DOM overlay covers it.
///
/// The Tauri child webview is a native layer above the DOM, so DOM menus and
/// dialogs cannot paint over it. The renderer reports overlap; this command
/// hides the webview without changing the tab's requested `visible` state, and
/// clearing the flag restores the effective visibility.
#[tauri::command]
pub fn browser_view_set_occluded(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
    occluded: bool,
) -> Result<BrowserViewState, String> {
    state.apply_occluded(&key, occluded)
}

fn apply_webview_visibility(webview: &Webview, visible: bool) -> Result<(), String> {
    let result = if visible {
        webview.show()
    } else {
        webview.hide()
    };
    result.map_err(|error| error.to_string())
}

/// Native window visibility (Windows). Used by the selftest to prove that the
/// child webview is really hidden, not just flagged.
#[cfg(windows)]
pub(crate) fn native_visible(state: &BrowserViewRegistry, key: &str) -> Result<bool, String> {
    use std::sync::mpsc;
    use std::time::Duration;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

    let webview = state.webview(key).ok_or("browser_view_not_found")?;
    let (sender, receiver) = mpsc::channel();
    webview
        .with_webview(move |platform| unsafe {
            let mut hwnd = HWND(std::ptr::null_mut());
            let visible = platform
                .controller()
                .ParentWindow(&mut hwnd)
                .map(|()| IsWindowVisible(hwnd).as_bool())
                .unwrap_or(false);
            let _ = sender.send(visible);
        })
        .map_err(|error| error.to_string())?;
    receiver
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "native_visible_timeout".to_string())
}

#[tauri::command]
pub fn browser_view_navigate(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
    url: String,
) -> Result<(), String> {
    let parsed = parse_allowed_url(&url)?;
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    webview.navigate(parsed).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn browser_view_reload(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
) -> Result<(), String> {
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    webview.reload().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn browser_view_go_back(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
) -> Result<(), String> {
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    webview
        .eval("history.back()")
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn browser_view_go_forward(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
) -> Result<(), String> {
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    webview
        .eval("history.forward()")
        .map_err(|error| error.to_string())
}

/// Evaluate JS in the child webview and return the JSON-serialized result.
/// `eval_with_callback` is supported on WebView2, WKWebView and WebKitGTK.
#[tauri::command]
pub async fn browser_view_eval(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
    script: String,
) -> Result<String, String> {
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    let (sender, receiver) = tokio::sync::oneshot::channel::<String>();
    let sender = Mutex::new(Some(sender));
    webview
        .eval_with_callback(script, move |result| {
            let result = if result.len() > MAX_EVAL_RESULT_BYTES {
                result.chars().take(MAX_EVAL_RESULT_BYTES).collect()
            } else {
                result
            };
            if let Some(sender) = sender.lock().ok().and_then(|mut guard| guard.take()) {
                let _ = sender.send(result);
            }
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(EVAL_TIMEOUT, receiver)
        .await
        .map_err(|_| "browser_view_eval_timeout".to_string())?
        .map_err(|_| "browser_view_eval_failed".to_string())
}

#[tauri::command]
pub fn browser_view_set_zoom(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
    factor: f64,
) -> Result<(), String> {
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    webview
        .set_zoom(factor.clamp(0.25, 5.0))
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn browser_view_open_devtools(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
) -> Result<(), String> {
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    webview.open_devtools();
    Ok(())
}

#[tauri::command]
pub fn browser_view_state(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
) -> Result<BrowserViewState, String> {
    state
        .state(&key)
        .ok_or_else(|| "browser_view_not_found".to_string())
}

#[tauri::command]
pub fn browser_view_close(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
) -> Result<(), String> {
    let Some(webview) = state.remove(&key) else {
        return Ok(());
    };
    webview.close().map_err(|error| error.to_string())
}

#[cfg(windows)]
fn call_cdp(webview: &Webview, method: String, params: String) -> Result<String, String> {
    use std::sync::mpsc;
    use std::sync::Arc;
    use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
    use windows::core::HSTRING;

    let (sender, receiver) = mpsc::channel::<Result<String, String>>();
    let sender = Arc::new(Mutex::new(Some(sender)));
    let handler_sender = Arc::clone(&sender);
    let error_sender = Arc::clone(&sender);
    webview
        .with_webview(move |platform_webview| {
            let started = (|| -> Result<(), String> {
                let controller = platform_webview.controller();
                let core =
                    unsafe { controller.CoreWebView2() }.map_err(|error| error.to_string())?;
                let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
                    move |error_code, json| {
                        let result = if error_code.is_ok() {
                            Ok(json)
                        } else {
                            Err("browser_view_cdp_failed".to_string())
                        };
                        if let Some(sender) = handler_sender
                            .lock()
                            .ok()
                            .and_then(|mut guard| guard.take())
                        {
                            let _ = sender.send(result);
                        }
                        Ok(())
                    },
                ));
                unsafe {
                    core.CallDevToolsProtocolMethod(
                        &HSTRING::from(method),
                        &HSTRING::from(params),
                        &handler,
                    )
                }
                .map_err(|error| error.to_string())
            })();
            if let Err(error) = started {
                if let Some(sender) = error_sender.lock().ok().and_then(|mut guard| guard.take()) {
                    let _ = sender.send(Err(error));
                }
            }
        })
        .map_err(|error| error.to_string())?;
    receiver
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| "browser_view_cdp_timeout".to_string())?
}

/// `IPlatformService.browserViewAttachGuest`: enables CDP on the child webview.
/// Windows uses WebView2's `CallDevToolsProtocolMethod`; macOS/Linux have no CDP
/// endpoint, so they return the explicit `not-webview` rejection instead of
/// pretending the guest is controllable.
#[tauri::command]
pub async fn browser_view_attach_guest(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
    web_contents_id: Option<u64>,
) -> Result<BrowserGuestAttachResult, String> {
    let _ = web_contents_id;
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    #[cfg(windows)]
    {
        let result = tokio::task::spawn_blocking(move || {
            call_cdp(&webview, "Runtime.enable".to_string(), "{}".to_string())
        })
        .await
        .map_err(|error| error.to_string())?;
        match result {
            Ok(_) => {
                state.set_cdp_attached(&key, true);
                Ok(BrowserGuestAttachResult::attached(1))
            }
            Err(_) => Ok(BrowserGuestAttachResult::rejected("not-webview")),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = webview;
        Ok(BrowserGuestAttachResult::rejected("not-webview"))
    }
}

/// `IPlatformService.browserViewDetachGuest`: releases the CDP attachment state.
/// WebView2 has no explicit debugger detach; clearing the registry flag is the
/// observable contract. Non-Windows returns `false` because no CDP was attached.
#[tauri::command]
pub fn browser_view_detach_guest(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
    web_contents_id: Option<u64>,
) -> Result<bool, String> {
    let _ = web_contents_id;
    if state.webview(&key).is_none() {
        return Ok(false);
    }
    state.set_cdp_attached(&key, false);
    Ok(cfg!(windows))
}

/// Dispatch a raw CDP method to the child webview. Windows only; other platforms
/// return `browser_view_cdp_unsupported`.
#[tauri::command]
pub async fn browser_view_cdp(
    state: tauri::State<'_, BrowserViewRegistry>,
    key: String,
    method: String,
    params: Option<String>,
) -> Result<String, String> {
    let webview = state.webview(&key).ok_or("browser_view_not_found")?;
    #[cfg(windows)]
    {
        let params = params.unwrap_or_else(|| "{}".to_string());
        tokio::task::spawn_blocking(move || call_cdp(&webview, method, params))
            .await
            .map_err(|error| error.to_string())?
    }
    #[cfg(not(windows))]
    {
        let _ = (webview, method, params);
        Err("browser_view_cdp_unsupported".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_web_file_and_blank_urls() {
        for value in [
            "https://example.com",
            "http://localhost:3000/app",
            "file:///C:/tmp/page.html",
            "about:blank",
        ] {
            assert!(parse_allowed_url(value).is_ok(), "{value} must be allowed");
        }
    }

    #[test]
    fn rejects_inline_and_malformed_urls() {
        for value in [
            "javascript:alert(1)",
            "data:text/html,hi",
            "about:config",
            "not a url",
            "",
        ] {
            assert!(
                parse_allowed_url(value).is_err(),
                "{value} must be rejected"
            );
        }
    }

    #[test]
    fn parses_eval_page_metadata_in_both_shapes() {
        let inner = r#"{"title":"Docs","favicon":"https://example.com/icon.png"}"#;
        let direct =
            serde_json::json!({ "title": "Docs", "favicon": "https://example.com/icon.png" });
        assert_eq!(
            parse_page_metadata(&direct.to_string()),
            Some((
                "Docs".to_string(),
                "https://example.com/icon.png".to_string()
            ))
        );
        assert_eq!(
            parse_page_metadata(&serde_json::Value::String(inner.to_string()).to_string()),
            Some((
                "Docs".to_string(),
                "https://example.com/icon.png".to_string()
            ))
        );
        assert_eq!(parse_page_metadata("not json"), None);
    }

    #[test]
    fn serializes_guest_attach_results_for_the_renderer_union() {
        let attached = serde_json::to_value(BrowserGuestAttachResult::attached(1)).unwrap();
        assert_eq!(attached["ok"], true);
        assert_eq!(attached["guestGeneration"], 1);
        assert!(attached.get("reason").is_none());
        let rejected =
            serde_json::to_value(BrowserGuestAttachResult::rejected("not-webview")).unwrap();
        assert_eq!(rejected["ok"], false);
        assert_eq!(rejected["reason"], "not-webview");
        assert_eq!(rejected["recoveryRequested"], false);
        assert!(rejected.get("guestGeneration").is_none());
    }

    #[test]
    fn occlusion_hides_only_while_requested_visible() {
        assert!(effective_visible(true, false));
        assert!(!effective_visible(true, true));
        assert!(!effective_visible(false, false));
        assert!(!effective_visible(false, true));
    }

    #[test]
    fn download_registry_is_tab_scoped_and_ids_are_unique() {
        let registry = BrowserViewRegistry::default();
        let first = registry.register_download("tab-a", PathBuf::from("/tmp/a.bin"));
        let second = registry.register_download("tab-a", PathBuf::from("/tmp/b.bin"));
        let other = registry.register_download("tab-b", PathBuf::from("/tmp/c.bin"));

        assert!(first.starts_with("iab-download:"));
        assert_ne!(first, second);
        assert_ne!(second, other);

        let mut tab_a = registry.download_ids("tab-a");
        tab_a.sort();
        let mut expected = vec![first.clone(), second.clone()];
        expected.sort();
        assert_eq!(tab_a, expected);
        assert_eq!(registry.download_ids("tab-b"), vec![other.clone()]);
        assert!(registry.download_ids("tab-missing").is_empty());

        assert_eq!(
            registry.download_path(&first, "tab-a"),
            Some(PathBuf::from("/tmp/a.bin"))
        );
        assert_eq!(registry.download_path(&first, "tab-b"), None);
        assert_eq!(registry.download_path("missing", "tab-a"), None);
    }
}
