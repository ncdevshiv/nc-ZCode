//! Native browser-control port backed by the Tauri child webview + CDP.
//!
//! The host's `Browser` tool sends a `BrowserCommand` JSON; this port maps it to
//! WebView2 `CallDevToolsProtocolMethod` calls on the active (or `tabId`-named)
//! child webview and returns a `BrowserCommandResult`-shaped JSON. macOS/Linux
//! have no CDP endpoint, so they return the explicit `capability_unsupported`
//! result instead of pretending the page was driven.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nc_code_host::tools::browser::BrowserControlPort;
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::browser_control_scripts as scripts;
use crate::browser_use_bridge::BrowserUseBridgeState;
use crate::browser_view::BrowserViewRegistry;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_MAX_ELEMENTS: usize = 200;
const MAX_ELEMENTS: usize = 1_000;
const POLL_INTERVAL: Duration = Duration::from_millis(150);
const BACKEND_ID: &str = "iab:shell";

#[derive(Clone)]
pub struct ShellBrowserPort {
    registry: BrowserViewRegistry,
    app: Option<AppHandle>,
    bridge: Option<Arc<BrowserUseBridgeState>>,
}

impl ShellBrowserPort {
    pub fn new(registry: BrowserViewRegistry) -> Self {
        Self {
            registry,
            app: None,
            bridge: None,
        }
    }

    /// Attach the renderer lifecycle bridge (used by the real shell; the
    /// `--browser-selftest` port stays registry-only).
    pub fn with_lifecycle(
        registry: BrowserViewRegistry,
        app: AppHandle,
        bridge: Arc<BrowserUseBridgeState>,
    ) -> Self {
        Self {
            registry,
            app: Some(app),
            bridge: Some(bridge),
        }
    }

    pub(crate) fn download_ids(&self, key: Option<&str>) -> Vec<String> {
        let tab_id = key
            .map(str::to_string)
            .or_else(|| self.registry.active_key())
            .unwrap_or_default();
        self.registry.download_ids(&tab_id)
    }

    pub(crate) fn download_path_for(
        &self,
        id: &str,
        key: Option<&str>,
    ) -> Option<std::path::PathBuf> {
        let tab_id = key
            .map(str::to_string)
            .or_else(|| self.registry.active_key())
            .unwrap_or_default();
        self.registry.download_path(id, &tab_id)
    }

    fn target_key(command: &Value) -> Option<String> {
        command
            .get("tabId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    fn timeout(command: &Value) -> Duration {
        let millis = command
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(100, MAX_TIMEOUT_MS);
        Duration::from_millis(millis)
    }

    pub(crate) async fn cdp(
        &self,
        key: Option<String>,
        method: &str,
        params: Value,
    ) -> Result<Value, (String, String)> {
        let registry = self.registry.clone();
        let method = method.to_string();
        let raw = tokio::task::spawn_blocking(move || {
            registry.dispatch_cdp(key.as_deref(), &method, &params)
        })
        .await
        .map_err(|error| {
            (
                "execution_error".to_string(),
                format!("browser_cdp_join_failed: {error}"),
            )
        })?
        .map_err(map_cdp_error)?;
        if raw.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&raw).map_err(|error| {
            (
                "execution_error".to_string(),
                format!("browser_cdp_bad_json: {error}"),
            )
        })
    }

    pub(crate) async fn eval(
        &self,
        key: Option<String>,
        expression: &str,
    ) -> Result<Value, (String, String)> {
        let result = self
            .cdp(
                key,
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true,
                }),
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            let message = exception
                .get("exception")
                .and_then(|value| value.get("description"))
                .and_then(Value::as_str)
                .or_else(|| exception.get("text").and_then(Value::as_str))
                .unwrap_or("browser_eval_failed");
            return Err(("execution_error".to_string(), message.to_string()));
        }
        Ok(result
            .get("result")
            .and_then(|value| value.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn wait_for_ready(&self, key: Option<String>, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.eval(key.clone(), scripts::READY_SCRIPT).await {
                Ok(Value::Bool(true)) => return,
                Ok(_) => tokio::time::sleep(POLL_INTERVAL).await,
                // Navigation may briefly invalidate the execution context.
                Err(_) => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }

    async fn state(&self, key: Option<String>) -> Result<Value, (String, String)> {
        let raw = self.eval(key.clone(), scripts::STATE_SCRIPT).await?;
        let mut state = match raw {
            Value::String(text) => serde_json::from_str::<Value>(&text).map_err(|error| {
                (
                    "execution_error".to_string(),
                    format!("browser_state_bad_json: {error}"),
                )
            })?,
            other => other,
        };
        let key = key.or_else(|| self.registry.active_key());
        if let Some((_, registry_state)) = key
            .as_deref()
            .and_then(|key| self.registry.state(key).map(|state| (key, state)))
        {
            if let Some(object) = state.as_object_mut() {
                object.insert("canGoBack".into(), json!(registry_state.can_go_back));
                object.insert("canGoForward".into(), json!(registry_state.can_go_forward));
            }
        }
        Ok(state)
    }

    fn meta(&self, key: &Option<String>) -> Value {
        let mut meta = json!({
            "browserUse": true,
            "backendType": "iab",
            "browserId": BACKEND_ID,
            "browserGeneration": 1,
            "openTabIds": key.iter().cloned().collect::<Vec<_>>(),
        });
        if let (Some(object), Some(key)) = (meta.as_object_mut(), key.as_ref()) {
            object.insert("tabId".into(), json!(key));
        }
        meta
    }

    async fn navigate(&self, key: Option<String>, command: &Value, timeout: Duration) -> Value {
        let url = match command.get("url").and_then(Value::as_str) {
            Some(url) => url,
            None => return error_result("invalid_input", "navigate requires `url`"),
        };
        if let Err(error) = crate::browser_view::validate_navigation_url(url) {
            return error_result("navigation_blocked", &error);
        }
        if let Err((code, message)) = self
            .cdp(key.clone(), "Page.navigate", json!({ "url": url }))
            .await
        {
            return error_result(&code, &message);
        }
        self.wait_for_ready(key.clone(), timeout).await;
        match self.state(key.clone()).await {
            Ok(state) => ok_result(json!({ "state": state, "meta": self.meta(&key) })),
            Err((code, message)) => error_result(&code, &message),
        }
    }

    async fn history(&self, key: Option<String>, expression: &str, timeout: Duration) -> Value {
        if let Err((code, message)) = self.eval(key.clone(), expression).await {
            return error_result(&code, &message);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        self.wait_for_ready(key.clone(), timeout).await;
        match self.state(key.clone()).await {
            Ok(state) => ok_result(json!({ "state": state, "meta": self.meta(&key) })),
            Err((code, message)) => error_result(&code, &message),
        }
    }

    pub(crate) async fn snapshot(&self, key: Option<String>, command: &Value) -> Value {
        let max = command
            .get("maxElements")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_MAX_ELEMENTS)
            .clamp(1, MAX_ELEMENTS);
        let include_hidden = command
            .get("includeHidden")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let script = scripts::SNAPSHOT_SCRIPT
            .replace("__MAX__", &max.to_string())
            .replace(
                "__INCLUDE_HIDDEN__",
                if include_hidden { "true" } else { "false" },
            );
        let raw = match self.eval(key.clone(), &script).await {
            Ok(value) => value,
            Err((code, message)) => return error_result(&code, &message),
        };
        let snapshot = match raw {
            Value::String(text) => match serde_json::from_str::<Value>(&text) {
                Ok(value) => value,
                Err(error) => {
                    return error_result(
                        "execution_error",
                        &format!("browser_snapshot_bad_json: {error}"),
                    )
                }
            },
            other => other,
        };
        ok_result(json!({ "snapshot": snapshot, "meta": self.meta(&key) }))
    }

    async fn screenshot(&self, key: Option<String>, command: &Value) -> Value {
        let full_page = command
            .get("fullPage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let result = match self
            .cdp(
                key.clone(),
                "Page.captureScreenshot",
                json!({
                    "format": "png",
                    "fromSurface": true,
                    "captureBeyondViewport": full_page,
                }),
            )
            .await
        {
            Ok(value) => value,
            Err((code, message)) => return error_result(&code, &message),
        };
        let Some(data) = result.get("data").and_then(Value::as_str) else {
            return error_result("execution_error", "captureScreenshot returned no data");
        };
        ok_result(json!({
            "image": { "base64": data, "mimeType": "image/png" },
            "meta": self.meta(&key),
        }))
    }

    async fn click(&self, key: Option<String>, command: &Value, timeout: Duration) -> Value {
        let (x, y) = match command.get("ref").and_then(Value::as_str) {
            Some(reference) => {
                if !scripts::is_valid_ref(reference) {
                    return error_result("invalid_input", "invalid element ref");
                }
                let script =
                    scripts::REF_POINT_SCRIPT.replace("__REF__", &format!("e{}", &reference[1..]));
                let raw = match self.eval(key.clone(), &script).await {
                    Ok(value) => value,
                    Err((code, message)) => return error_result(&code, &message),
                };
                let point = match raw {
                    Value::String(text) => {
                        serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)
                    }
                    other => other,
                };
                if point.get("ok").and_then(Value::as_bool) != Some(true) {
                    return error_result("ref_not_found", "element ref is not in the page");
                }
                (
                    point.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                    point.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                )
            }
            None => match (
                command.get("x").and_then(Value::as_f64),
                command.get("y").and_then(Value::as_f64),
            ) {
                (Some(x), Some(y)) => (x, y),
                _ => return error_result("invalid_input", "click requires `ref` or `x`/`y`"),
            },
        };
        let click_count = if command
            .get("doubleClick")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            2
        } else {
            1
        };
        let button = command
            .get("button")
            .and_then(Value::as_str)
            .unwrap_or("left");
        let modifiers = modifier_mask(command);
        for event_type in ["mousePressed", "mouseReleased"] {
            if let Err((code, message)) = self
                .cdp(
                    key.clone(),
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": event_type,
                        "x": x,
                        "y": y,
                        "button": button,
                        "clickCount": click_count,
                        "modifiers": modifiers,
                    }),
                )
                .await
            {
                return error_result(&code, &message);
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        self.wait_for_ready(key.clone(), timeout).await;
        match self.state(key.clone()).await {
            Ok(state) => ok_result(json!({ "state": state, "meta": self.meta(&key) })),
            Err((code, message)) => error_result(&code, &message),
        }
    }

    async fn fill(&self, key: Option<String>, command: &Value) -> Value {
        let Some(reference) = command.get("ref").and_then(Value::as_str) else {
            return error_result("invalid_input", "fill requires `ref`");
        };
        if !scripts::is_valid_ref(reference) {
            return error_result("invalid_input", "invalid element ref");
        }
        let Some(value) = command.get("value").and_then(Value::as_str) else {
            return error_result("invalid_input", "fill requires `value`");
        };
        let script = scripts::FILL_SCRIPT
            .replace("__REF__", &format!("e{}", &reference[1..]))
            .replace("__VALUE__", &scripts::js_string(value));
        let raw = match self.eval(key.clone(), &script).await {
            Ok(value) => value,
            Err((code, message)) => return error_result(&code, &message),
        };
        let parsed = match raw {
            Value::String(text) => serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
            other => other,
        };
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            return error_result("ref_not_found", "element ref is not in the page");
        }
        ok_result(json!({ "meta": self.meta(&key) }))
    }

    async fn type_text(&self, key: Option<String>, command: &Value) -> Value {
        let Some(text) = command.get("text").and_then(Value::as_str) else {
            return error_result("invalid_input", "type requires `text`");
        };
        if let Some(reference) = command.get("ref").and_then(Value::as_str) {
            if !scripts::is_valid_ref(reference) {
                return error_result("invalid_input", "invalid element ref");
            }
            let focus = scripts::FOCUS_SCRIPT.replace("__REF__", &scripts::js_string(reference));
            if let Err((code, message)) = self.eval(key.clone(), &focus).await {
                return error_result(&code, &message);
            }
        }
        match self
            .cdp(key.clone(), "Input.insertText", json!({ "text": text }))
            .await
        {
            Ok(_) => ok_result(json!({ "meta": self.meta(&key) })),
            Err((code, message)) => error_result(&code, &message),
        }
    }

    async fn press(&self, key: Option<String>, command: &Value) -> Value {
        let Some(raw_key) = command.get("key").and_then(Value::as_str) else {
            return error_result("invalid_input", "press requires `key`");
        };
        let Some((code, virtual_key, key_name)) = scripts::key_fields(raw_key) else {
            return error_result("invalid_input", &format!("unsupported key: {raw_key}"));
        };
        let modifiers = modifier_mask(command);
        for event_type in ["keyDown", "keyUp"] {
            if let Err((error_code, message)) = self
                .cdp(
                    key.clone(),
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": event_type,
                        "key": key_name,
                        "code": code,
                        "windowsVirtualKeyCode": virtual_key,
                        "nativeVirtualKeyCode": virtual_key,
                        "modifiers": modifiers,
                    }),
                )
                .await
            {
                return error_result(&error_code, &message);
            }
        }
        ok_result(json!({ "meta": self.meta(&key) }))
    }

    async fn scroll(&self, key: Option<String>, command: &Value) -> Value {
        let x = command.get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let y = command.get("y").and_then(Value::as_f64).unwrap_or(600.0);
        let script = format!("window.scrollBy({x}, {y}); true");
        match self.eval(key.clone(), &script).await {
            Ok(_) => ok_result(json!({ "meta": self.meta(&key) })),
            Err((code, message)) => error_result(&code, &message),
        }
    }

    pub(crate) async fn element_info(&self, key: Option<String>, command: &Value) -> Value {
        let (Some(x), Some(y)) = (
            command.get("x").and_then(Value::as_f64),
            command.get("y").and_then(Value::as_f64),
        ) else {
            return error_result("invalid_input", "elementInfo requires `x` and `y`");
        };
        let script = format!(
            r#"(() => {{
  const el = document.elementFromPoint({x}, {y});
  if (!el) return JSON.stringify({{ ok: false }});
  const rect = el.getBoundingClientRect();
  return JSON.stringify({{
    ok: true,
    tag: el.tagName.toLowerCase(),
    role: el.getAttribute('role') || undefined,
    name: (el.getAttribute('aria-label') || el.getAttribute('name') || '').slice(0, 200) || undefined,
    text: (el.innerText || el.value || '').trim().slice(0, 300) || undefined,
    selector: el.id ? '#' + CSS.escape(el.id) : undefined,
    rect: {{ x: rect.x, y: rect.y, width: rect.width, height: rect.height }},
    inViewport: rect.top < window.innerHeight && rect.bottom > 0 && rect.left < window.innerWidth && rect.right > 0,
  }});
}})()"#
        );
        let raw = match self.eval(key.clone(), &script).await {
            Ok(value) => value,
            Err((code, message)) => return error_result(&code, &message),
        };
        let parsed = match raw {
            Value::String(text) => serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
            other => other,
        };
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            return ok_result(json!({ "element": Value::Null, "meta": self.meta(&key) }));
        }
        ok_result(json!({ "element": parsed, "meta": self.meta(&key) }))
    }

    async fn wait_for(&self, key: Option<String>, command: &Value, timeout: Duration) -> Value {
        let selector = command.get("selector").and_then(Value::as_str);
        let text = command.get("text").and_then(Value::as_str);
        let text_gone = command.get("textGone").and_then(Value::as_str);
        if selector.is_none() && text.is_none() && text_gone.is_none() {
            return error_result(
                "invalid_input",
                "waitFor requires `selector`, `text` or `textGone`",
            );
        }
        let script = scripts::WAIT_SCRIPT
            .replace(
                "__SELECTOR__",
                &selector
                    .map(scripts::js_string)
                    .unwrap_or_else(|| "null".to_string()),
            )
            .replace(
                "__TEXT__",
                &text
                    .map(scripts::js_string)
                    .unwrap_or_else(|| "null".to_string()),
            )
            .replace(
                "__TEXT_GONE__",
                &text_gone
                    .map(scripts::js_string)
                    .unwrap_or_else(|| "null".to_string()),
            );
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.eval(key.clone(), &script).await {
                Ok(raw) => {
                    let parsed = match raw {
                        Value::String(text) => {
                            serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)
                        }
                        other => other,
                    };
                    let satisfied = parsed.get("selector").and_then(Value::as_bool) == Some(true)
                        && parsed.get("text").and_then(Value::as_bool) == Some(true)
                        && parsed.get("textGone").and_then(Value::as_bool) == Some(true);
                    if satisfied {
                        return ok_result(json!({ "meta": self.meta(&key) }));
                    }
                }
                Err((code, message)) => return error_result(&code, &message),
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        error_result(
            "timeout",
            "waitFor condition was not met before the deadline",
        )
    }
}

#[async_trait]
impl BrowserControlPort for ShellBrowserPort {
    async fn list(&self) -> Result<Value, String> {
        let active = self.registry.active_key();
        let generation = self.registry.active_id().unwrap_or(1).max(1);
        Ok(json!([{
            "id": BACKEND_ID,
            "generation": generation,
            "type": "iab",
            "name": "ZCode In-app Browser",
            "capabilities": {
                "browser": [{
                    "id": "visibility",
                    "description": "Use to show or hide the browser to the user.",
                }],
                "tab": [],
            },
            "metadata": active.map(|key| json!({ "activeTabId": key })).unwrap_or_else(|| json!({})),
        }]))
    }

    async fn execute(&self, command: &Value) -> Result<Value, String> {
        let started = Instant::now();
        let Some(method) = command.get("method").and_then(Value::as_str) else {
            return Err("browser command is missing `method`".to_string());
        };
        let key = Self::target_key(command);
        let timeout = Self::timeout(command);
        let result = match method {
            "navigate" => self.navigate(key.clone(), command, timeout).await,
            "back" => {
                self.history(key.clone(), "history.back(); true", timeout)
                    .await
            }
            "forward" => {
                self.history(key.clone(), "history.forward(); true", timeout)
                    .await
            }
            "reload" => {
                let reload = self
                    .cdp(key.clone(), "Page.reload", json!({ "ignoreCache": false }))
                    .await;
                match reload {
                    Ok(_) => {
                        self.wait_for_ready(key.clone(), timeout).await;
                        match self.state(key.clone()).await {
                            Ok(state) => {
                                ok_result(json!({ "state": state, "meta": self.meta(&key) }))
                            }
                            Err((code, message)) => error_result(&code, &message),
                        }
                    }
                    Err((code, message)) => error_result(&code, &message),
                }
            }
            "getState" => match self.state(key.clone()).await {
                Ok(state) => ok_result(json!({ "state": state, "meta": self.meta(&key) })),
                Err((code, message)) => error_result(&code, &message),
            },
            "snapshot" => self.snapshot(key.clone(), command).await,
            "evaluate" => {
                let Some(expression) = command.get("expression").and_then(Value::as_str) else {
                    return Ok(error_result(
                        "invalid_input",
                        "evaluate requires `expression`",
                    ));
                };
                match self.eval(key.clone(), expression).await {
                    Ok(value) => ok_result(json!({ "value": value, "meta": self.meta(&key) })),
                    Err((code, message)) => error_result(&code, &message),
                }
            }
            "screenshot" => self.screenshot(key.clone(), command).await,
            "click" => self.click(key.clone(), command, timeout).await,
            "fill" => self.fill(key.clone(), command).await,
            "type" => self.type_text(key.clone(), command).await,
            "press" => self.press(key.clone(), command).await,
            "scroll" => self.scroll(key.clone(), command).await,
            "waitFor" => self.wait_for(key.clone(), command, timeout).await,
            "elementInfo" => self.element_info(key.clone(), command).await,
            "playwright" => {
                let action = command.get("action").cloned().unwrap_or(Value::Null);
                match crate::browser_playwright::execute(self, key.clone(), &action, timeout).await
                {
                    Ok(value) => {
                        let mut result = json!({ "meta": self.meta(&key) });
                        if let Some(image) = value.get("image") {
                            result["image"] = image.clone();
                        } else {
                            result["value"] = value;
                        }
                        ok_result(result)
                    }
                    Err((code, message)) => error_result(&code, &message),
                }
            }
            "playwrightWaitForTimeout" => {
                let millis = command
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(90_000);
                tokio::time::sleep(Duration::from_millis(millis)).await;
                ok_result(json!({ "meta": self.meta(&key) }))
            }
            "list" => match self.list().await {
                Ok(browsers) => ok_result(json!({ "browsers": browsers })),
                Err(message) => error_result("backend_unavailable", &message),
            },
            // Renderer-owned tab lifecycle: the side-pane state is the source of
            // truth, so delegate through the request/response event bridge.
            "listUserTabs" | "activateTab" | "claimTab" | "newTab" | "finalize"
            | "finalizeTabs" | "markDeliverable" | "markHandoff" | "nameSession"
            | "closeSession" | "recordingStart" | "recordingStatus" | "recordingCancel"
            | "close" => {
                let (Some(bridge), Some(app)) = (self.bridge.as_ref(), self.app.as_ref()) else {
                    return Ok(error_result(
                        "backend_unavailable",
                        "renderer browser lifecycle bridge is not attached",
                    ));
                };
                match bridge.request(app, command.clone()).await {
                    Ok(result) => result,
                    Err(error) => error_result("backend_unavailable", &error),
                }
            }
            other => error_result(
                "capability_unsupported",
                &format!("browser method `{other}` is not supported by the Tauri shell"),
            ),
        };
        let mut result = result;
        if let Some(object) = result.as_object_mut() {
            object.insert(
                "elapsedMs".into(),
                json!(started.elapsed().as_millis() as u64),
            );
        }
        Ok(result)
    }
}

fn ok_result(body: Value) -> Value {
    let mut object = body.as_object().cloned().unwrap_or_default();
    object.insert("ok".into(), json!(true));
    Value::Object(object)
}

fn error_result(code: &str, message: &str) -> Value {
    json!({
        "ok": false,
        "error": { "code": code, "message": message },
    })
}

fn map_cdp_error(error: String) -> (String, String) {
    if error == "browser_view_cdp_unsupported" {
        return (
            "capability_unsupported".to_string(),
            "CDP is only available on Windows (WebView2); this platform has no debugger endpoint"
                .to_string(),
        );
    }
    if error == "browser_view_not_found" {
        return (
            "backend_unavailable".to_string(),
            "no browser tab is open in the shell".to_string(),
        );
    }
    if error == "browser_view_cdp_timeout" {
        return ("timeout".to_string(), "CDP call timed out".to_string());
    }
    ("execution_error".to_string(), error)
}

fn modifier_mask(command: &Value) -> u32 {
    let Some(modifiers) = command.get("modifiers").and_then(Value::as_array) else {
        return 0;
    };
    let mut mask = 0;
    for modifier in modifiers.iter().filter_map(Value::as_str) {
        mask |= match modifier {
            "Alt" => 1,
            "Control" | "ControlOrMeta" => 2,
            "Meta" => 4,
            "Shift" => 8,
            _ => 0,
        };
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_cdp_errors_to_stable_codes() {
        assert_eq!(
            map_cdp_error("browser_view_cdp_unsupported".to_string()).0,
            "capability_unsupported"
        );
        assert_eq!(
            map_cdp_error("browser_view_not_found".to_string()).0,
            "backend_unavailable"
        );
        assert_eq!(
            map_cdp_error("browser_view_cdp_timeout".to_string()).0,
            "timeout"
        );
        assert_eq!(
            map_cdp_error("boom".to_string()),
            ("execution_error".to_string(), "boom".to_string())
        );
    }

    #[test]
    fn modifier_mask_matches_cdp_bits() {
        assert_eq!(modifier_mask(&json!({})), 0);
        assert_eq!(modifier_mask(&json!({ "modifiers": ["Shift"] })), 8);
        assert_eq!(
            modifier_mask(&json!({ "modifiers": ["Alt", "Control", "Meta", "Shift"] })),
            15
        );
        assert_eq!(modifier_mask(&json!({ "modifiers": ["Bogus"] })), 0);
    }

    #[test]
    fn timeout_clamps_and_defaults() {
        assert_eq!(
            ShellBrowserPort::timeout(&json!({})),
            Duration::from_millis(30_000)
        );
        assert_eq!(
            ShellBrowserPort::timeout(&json!({ "timeoutMs": 1 })),
            Duration::from_millis(100)
        );
        assert_eq!(
            ShellBrowserPort::timeout(&json!({ "timeoutMs": 999_999 })),
            Duration::from_millis(120_000)
        );
    }

    #[test]
    fn result_shaping_is_structured() {
        let ok = ok_result(json!({ "value": 1 }));
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["value"], 1);
        let error = error_result("timeout", "late");
        assert_eq!(error["ok"], false);
        assert_eq!(error["error"]["code"], "timeout");
        assert_eq!(error["error"]["message"], "late");
    }
    #[test]
    fn tab_id_is_trimmed_and_optional() {
        assert_eq!(
            ShellBrowserPort::target_key(&json!({ "tabId": "  tab-1  " })),
            Some("tab-1".to_string())
        );
        assert_eq!(
            ShellBrowserPort::target_key(&json!({ "tabId": "   " })),
            None
        );
        assert_eq!(ShellBrowserPort::target_key(&json!({})), None);
    }
}
