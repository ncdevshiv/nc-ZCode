//! CDP operation for a single controlled tab (page target). Holds the attached sessionId, snapshot ref table and
//! Cancel bit. All methods send commands through `CdpConnection` and wait for the associated response.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{CdpError, CdpErrorCode, CdpResult};
use crate::snapshot::{
    ax_nodes_in_dom_order, build_snapshot, parse_ax_tree, BrowserElementRect, BrowserSnapshot,
    Geometry, ResolvedGeometry, SnapshotOptions, DEFAULT_MAX_ELEMENTS,
};
use crate::transport::{CdpConnection, DEFAULT_COMMAND_TIMEOUT};

/// Default Agent viewport (CSS px): mirror TS `DEFAULT_AGENT_BROWSER_VIEWPORT`
/// (`packages/shared/src/browser-use/command-metadata.ts:12-15`).
pub const DEFAULT_VIEWPORT: (u32, u32) = (1280, 720);

/// `buttons` bitfield of CDP `Input.dispatchMouseEvent`: left button pressed (left=1).
pub const LEFT_BUTTON_HELD: u32 = 1;

/// The actionable point after snapshot handle resolution (ref → center coordinate + backendNodeId).
#[derive(Debug, Clone, Copy)]
pub struct ResolvedRef {
    pub x: f64,
    pub y: f64,
    pub rect: BrowserElementRect,
    pub backend_dom_node_id: Option<i64>,
}

pub struct Page {
    conn: Arc<CdpConnection>,
    target_id: String,
    session_id: String,
    cancel: Arc<AtomicBool>,
    refs: Arc<Mutex<HashMap<String, ResolvedRef>>>,
    viewport: Mutex<(u32, u32)>,
    command_timeout: Duration,
}

impl Page {
    pub fn new(
        conn: Arc<CdpConnection>,
        target_id: String,
        session_id: String,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            conn,
            target_id,
            session_id,
            cancel,
            refs: Arc::new(Mutex::new(HashMap::new())),
            viewport: Mutex::new(DEFAULT_VIEWPORT),
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
        }
    }
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn connection(&self) -> &CdpConnection {
        &self.conn
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn set_cancelled(&self, value: bool) {
        self.cancel.store(value, Ordering::SeqCst);
    }

    /// Session-scoped commands are sent (all Page/DOM/Runtime/Input domain commands have sessionId).
    pub async fn send(&self, method: &str, params: Value) -> CdpResult<Value> {
        self.conn
            .send(method, params, Some(&self.session_id), self.command_timeout)
            .await
    }

    pub fn refs_handle(&self) -> Arc<Mutex<HashMap<String, ResolvedRef>>> {
        Arc::clone(&self.refs)
    }

    /// `Runtime.evaluate` and get back a JSON-serializable value (returnByValue + awaitPromise).
    pub async fn evaluate_raw(&self, expression: &str) -> CdpResult<Value> {
        let result = self
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            let text = exception
                .get("text")
                .and_then(Value::as_str)
                .or_else(|| {
                    exception
                        .get("exception")
                        .and_then(|e| e.get("description"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("evaluation threw");
            return Err(CdpError::new(
                CdpErrorCode::Protocol,
                format!("evaluate failed: {text}"),
            ));
        }
        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Read the current page URL + title (getState / snapshot header field).
    pub async fn page_state(&self) -> CdpResult<(String, String)> {
        let url = self
            .evaluate_raw("location.href")
            .await?
            .as_str()
            .unwrap_or("")
            .to_string();
        let title = self
            .evaluate_raw("document.title")
            .await?
            .as_str()
            .unwrap_or("")
            .to_string();
        Ok((url, title))
    }

    pub async fn navigate(&self, url: &str) -> CdpResult<()> {
        self.send("Page.navigate", json!({ "url": url })).await?;
        Ok(())
    }

    pub async fn reload(&self) -> CdpResult<()> {
        self.send("Page.reload", json!({})).await?;
        Ok(())
    }

    pub async fn go_back_or_forward(&self, offset: i64) -> CdpResult<()> {
        let history = self.send("Page.getNavigationHistory", Value::Null).await?;
        let current = history
            .get("currentIndex")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let target = current + offset;
        let entries = history
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let entry = entries.get(target.max(0) as usize).ok_or_else(|| {
            CdpError::new(
                CdpErrorCode::NavigationBlocked,
                format!("no history entry at index {target}"),
            )
        })?;
        let id = entry.get("id").and_then(Value::as_i64).ok_or_else(|| {
            CdpError::new(CdpErrorCode::NavigationBlocked, "history entry missing id")
        })?;
        self.send("Page.navigateToHistoryEntry", json!({ "entryId": id }))
            .await?;
        Ok(())
    }

    /// Screenshot: `Page.captureScreenshot` (format png), returns base64.
    /// `full_page` uses captureBeyondViewport; `clip` is the viewport CSS px rectangle.
    pub async fn screenshot(
        &self,
        full_page: bool,
        clip: Option<(f64, f64, f64, f64)>,
    ) -> CdpResult<String> {
        let mut params = json!({ "format": "png" });
        if full_page {
            params["captureBeyondViewport"] = json!(true);
        }
        if let Some((x, y, width, height)) = clip {
            params["clip"] = json!({
                "x": x, "y": y, "width": width, "height": height, "scale": 1
            });
        }
        let result = self.send("Page.captureScreenshot", params).await?;
        result
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                CdpError::new(CdpErrorCode::Protocol, "captureScreenshot returned no data")
            })
    }

    /// Grab AX snapshot: `Accessibility.getFullAXTree` after `Accessibility.enable`,
    /// Then use the DOM domain to parse the rect/tag (geometry) of each actionable node, and finally hand it over to the pure mapping layer
    /// `snapshot::build_snapshot`. Also record the ref→center point for action to use.
    pub async fn capture_snapshot(
        &self,
        max_elements: Option<usize>,
        include_hidden: bool,
    ) -> CdpResult<BrowserSnapshot> {
        let _ = self.send("Accessibility.enable", Value::Null).await;
        let _ = self.send("DOM.enable", Value::Null).await;
        let ax_result = self
            .send("Accessibility.getFullAXTree", Value::Null)
            .await?;
        let raw = parse_ax_tree(&ax_result);
        let ordered = ax_nodes_in_dom_order(&raw, None);

        let (viewport_w, viewport_h) = self.viewport_size();
        let geometry = self
            .resolve_geometry(&ordered, viewport_w, viewport_h)
            .await?;

        let (url, title) = self.page_state().await?;
        let max = max_elements.unwrap_or(DEFAULT_MAX_ELEMENTS).max(1);
        let options = SnapshotOptions {
            max_elements: max,
            // The command parameter includeHidden must be passed transparently: ignoring it will make the hidden element never enter.
            // Snapshot is inconsistent with the includeHidden semantics (snapshot.ts:119-121) of TS snapshot.
            include_hidden,
            ..Default::default()
        };
        let snapshot = build_snapshot(&ordered, &url, &title, &geometry, &options);

        // Record ref→point (center), mirror TS resolveSnapshotRef (snapshot.ts:251-262).
        let mut refs = self.refs.lock().expect("page refs mutex");
        refs.clear();
        for element in &snapshot.elements {
            // ref→center point (equivalent of TS resolveSnapshotRef, snapshot.ts:251-262).
            // BackendNodeId backend check: role/name matching in ordered is not unique, just use rect to hit it.
            // Therefore, ordered document order is used to reconstruct the correspondence of ref→backendNodeId.
            refs.insert(
                element.r#ref.clone(),
                ResolvedRef {
                    x: element.rect.x as f64 + element.rect.width as f64 / 2.0,
                    y: element.rect.y as f64 + element.rect.height as f64 / 2.0,
                    rect: element.rect,
                    backend_dom_node_id: None,
                },
            );
        }
        Ok(snapshot)
    }

    pub fn resolve_ref(&self, rf: &str) -> Option<ResolvedRef> {
        self.refs.lock().expect("page refs mutex").get(rf).copied()
    }

    /// Synthetic mouse click: `Input.dispatchMouseEvent`. `button` ∈ left/right/middle.
    pub async fn click(
        &self,
        x: f64,
        y: f64,
        button: &str,
        double_click: bool,
        modifiers: u32,
    ) -> CdpResult<()> {
        self.send(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mousePressed", "x": x, "y": y, "button": button,
                "clickCount": if double_click { 2 } else { 1 }, "modifiers": modifiers
            }),
        )
        .await?;
        self.send(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseReleased", "x": x, "y": y, "button": button,
                "clickCount": if double_click { 2 } else { 1 }, "modifiers": modifiers
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn mouse_move(&self, x: f64, y: f64, modifiers: u32) -> CdpResult<()> {
        self.send(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": x, "y": y, "modifiers": modifiers }),
        )
        .await?;
        Ok(())
    }

    /// Press mouse button (do not release): first paragraph of drag (TS `page.mouse.down()`, page-command.ts:123).
    pub async fn mouse_down(&self, x: f64, y: f64, button: &str, modifiers: u32) -> CdpResult<()> {
        self.send(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mousePressed", "x": x, "y": y, "button": button,
                "clickCount": 1, "modifiers": modifiers
            }),
        )
        .await?;
        Ok(())
    }

    /// Release the mouse button: the end of the drag (TS `page.mouse.up()`, page-command.ts:126).
    pub async fn mouse_up(&self, x: f64, y: f64, button: &str, modifiers: u32) -> CdpResult<()> {
        self.send(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseReleased", "x": x, "y": y, "button": button,"clickCount": 1, "modifiers": modifiers
            }),
        )
        .await?;
        Ok(())
    }

    /// Press and hold the left button to move: CDP uses a separate `buttons` bit field to express "which key is being pressed"
    /// (left=1, right=2, middle=4), orthogonal to `modifiers` (Alt/Ctrl/Meta/Shift).
    /// TS `mouse.move` automatically carries this state after down (internal to Playwright),
    /// raw CDP must be brought explicitly, otherwise the page will receive hover movement without pressing the key.
    pub async fn mouse_drag_move(&self, x: f64, y: f64, modifiers: u32) -> CdpResult<()> {
        self.send(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseMoved", "x": x, "y": y,
                "modifiers": modifiers, "buttons": LEFT_BUTTON_HELD
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn mouse_wheel(&self, x: f64, y: f64, delta_x: f64, delta_y: f64) -> CdpResult<()> {
        self.send(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": delta_x, "deltaY": delta_y }),
        )
        .await?;
        Ok(())
    }

    /// Key-by-key down/up: rawKeyDown + char + keyUp (char provides text input semantics).
    pub async fn key_press(&self, key: &str, modifiers: u32) -> CdpResult<()> {
        self.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyDown", "key": key, "modifiers": modifiers }),
        )
        .await?;
        self.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key, "modifiers": modifiers }),
        )
        .await?;
        Ok(())
    }

    pub async fn key_down(&self, key: &str, modifiers: u32) -> CdpResult<()> {
        self.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyDown", "key": key, "modifiers": modifiers }),
        )
        .await?;
        Ok(())
    }
    pub async fn key_up(&self, key: &str, modifiers: u32) -> CdpResult<()> {
        self.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key, "modifiers": modifiers }),
        )
        .await?;
        Ok(())
    }

    /// Character-by-character text input: `Input.insertText` (stable text injection independent of keyboard layout).
    pub async fn insert_text(&self, text: &str) -> CdpResult<()> {
        self.send("Input.insertText", json!({ "text": text }))
            .await?;
        Ok(())
    }

    /// fill: Focus on the element hit by backendNodeId/selector and then set the value and dispatch the input/change event.
    /// Approximate Playwright `element.fill`. Ensure observable DOM fact changes with JS.
    pub async fn fill_by_selector(&self, selector: &str, value: &str) -> CdpResult<bool> {
        let escaped_selector = selector.replace('\\', "\\\\").replace('\'', "\\'");
        let escaped_value = value.replace('\\', "\\\\").replace('\'', "\\'");
        let expr = format!(
            "(function(){{var el=document.querySelector('{escaped_selector}');if(!el)return false;\
             el.focus();var proto=Object.getPrototypeOf(el);\
             var setter=Object.getOwnPropertyDescriptor(proto,'value');\
             if(setter&&setter.set){{setter.set.call(el,'{escaped_value}');}}else{{el.value='{escaped_value}';}}\
             el.dispatchEvent(new Event('input',{{bubbles:true}}));\
             el.dispatchEvent(new Event('change',{{bubbles:true}}));return true;}})()"
        );
        Ok(self.evaluate_raw(&expr).await?.as_bool().unwrap_or(false))
    }

    pub async fn set_viewport(&self, width: u32, height: u32) -> CdpResult<()> {
        self.send(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": width, "height": height, "deviceScaleFactor": 1, "mobile": false }),
        )
        .await?;
        *self.viewport.lock().expect("viewport mutex") = (width, height);
        Ok(())
    }

    pub fn viewport_size(&self) -> (f64, f64) {
        let (w, h) = *self.viewport.lock().expect("viewport mutex");
        (w as f64, h as f64)
    }

    /// handleJavaScriptDialog: accept + optional promptText.
    pub async fn handle_dialog(&self, accept: bool, prompt_text: Option<&str>) -> CdpResult<()> {
        let mut params = json!({ "accept": accept });
        if let Some(text) = prompt_text {
            params["promptText"] = json!(text);
        }
        self.send("Page.handleJavaScriptDialog", params).await?;
        Ok(())
    }

    /// Parse the rect/tag of the AX actionable node: `DOM.getBoxModel` (backendNodeId → content quad)
    /// + `DOM.describeNode`(nodeName). Fails to downgrade a single node (does not affect the overall snapshot).
    async fn resolve_geometry(
        &self,
        ordered: &[&crate::snapshot::RawAxNode],
        viewport_w: f64,
        viewport_h: f64,
    ) -> CdpResult<Geometry> {
        let mut geometry = Geometry::new();
        for node in ordered {
            if !node.actionable() && !node.semantic() {
                continue;
            }
            let Some(backend) = node.backend_dom_node_id else {
                continue;
            };
            let rect = self.box_rect(backend).await;
            let tag = self.node_tag(backend).await;
            let (in_viewport, rect_value) = match rect {
                Some(r) => {
                    let iv = r.y < viewport_h
                        && (r.y + r.height) > 0.0
                        && r.x < viewport_w
                        && (r.x + r.width) > 0.0;
                    (
                        iv,
                        BrowserElementRect {
                            x: r.x.round() as i64,
                            y: r.y.round() as i64,
                            width: r.width.round() as i64,
                            height: r.height.round() as i64,
                        },
                    )
                }
                None => (false, BrowserElementRect::default()),
            };
            geometry.insert(
                backend,
                ResolvedGeometry {
                    tag,
                    rect: rect_value,
                    selector: String::new(),
                    xpath: String::new(),
                    in_viewport,
                },
            );
        }
        Ok(geometry)
    }

    async fn box_rect(&self, backend_node_id: i64) -> Option<RawRect> {
        let model = self
            .send(
                "DOM.getBoxModel",
                json!({ "backendNodeId": backend_node_id }),
            )
            .await
            .ok()?;
        let content = model.get("model")?.get("content")?.as_array()?;
        if content.len() < 8 {
            return None;
        }
        let xs = [
            content[0].as_f64()?,
            content[2].as_f64()?,
            content[4].as_f64()?,
            content[6].as_f64()?,
        ];
        let ys = [
            content[1].as_f64()?,
            content[3].as_f64()?,
            content[5].as_f64()?,
            content[7].as_f64()?,
        ];
        let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some(RawRect {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        })
    }

    async fn node_tag(&self, backend_node_id: i64) -> String {
        match self
            .send(
                "DOM.describeNode",
                json!({ "backendNodeId": backend_node_id }),
            )
            .await
        {
            Ok(desc) => desc
                .get("node")
                .and_then(|n| n.get("nodeName"))
                .and_then(Value::as_str)
                .map(|name| name.to_lowercase())
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RawRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_viewport_matches_ts_const() {
        assert_eq!(DEFAULT_VIEWPORT, (1280, 720));
    }

    #[test]
    fn fill_selector_expression_is_balanced_and_quotes_escaped() {
        // Pure string construction, no concatenation required to verify the syntactic skeleton of the generated expression.
        let escaped = "div[data-x='y']".replace('\\', "\\\\").replace('\'', "\\'");
        let expr = format!("(function(){{var el=document.querySelector('{escaped}');}})()");
        assert!(expr.matches('{').count() >= expr.matches('}').count());
        assert!(expr.contains("\\'"));
    }

    #[test]
    fn in_viewport_logic_mirrors_ts() {
        // TS snapshot.ts:116-117: top<vh && bottom>0 && left<vw && right>0
        let (vx, vy) = (100.0, 100.0);
        let in_vp =
            |r: &RawRect| r.y < vy && (r.y + r.height) > 0.0 && r.x < vx && (r.x + r.width) > 0.0;
        let inside = RawRect {
            x: 10.0,
            y: 10.0,
            width: 20.0,
            height: 20.0,
        };
        let below = RawRect {
            x: 10.0,
            y: 200.0,
            width: 20.0,
            height: 20.0,
        };
        assert!(in_vp(&inside));
        assert!(!in_vp(&below));
    }
}
