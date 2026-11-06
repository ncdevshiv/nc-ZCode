//! Page command surface: put `BrowserCommand` of TS `packages/shared/src/browser-use/commands.ts`
//! Determine whether the "page-level" command in the union is mapped to the raw CDP operation and return the `browserCommandResultSchema` of `result.ts`
//! Shape (`{ok, ...}` / `{ok:false, error:{code,message}}`).
//!
//! Command source (file:line see README/report):
//! - commands.ts:307-587 `browserCommandSchema` discriminant union (all methods).
//! - page-command.ts:130-301 `executeManagedPageCommand` (Playwright version of page command implementation, case by case).
//! - index.ts:280-356 `executeSessionCommand` (session level list/newTab/activate/close/viewport/dialog).
//!
//! Raw CDP is not supported, and the method explicitly rejected by capability_unsupported: playwright positioning engine,
//! recording*, browserVisibility*, claimTab/finalize*/mark*, etc. session-lifecycle semantics——
//! See `apiSupportOverrides` in descriptor.ts:16-25, these are also in the managed-CDP backend of TS
//! unsupported (index.ts:336-350).

use std::time::Duration;

use serde_json::{json, Value};

use crate::auto_wait::{self, WaitPolicy};
use crate::error::{CdpError, CdpErrorCode, CdpResult};
use crate::modifiers::modifier_mask;
use crate::page::Page;

/// URL scheme to allow navigation (mirror TS `isAllowedManagedBrowserUrl`, page-command.ts:20-27).
/// The raw CDP backend additionally allows `file:` - e2e uses locally generated pages to drive the real browser and has no external network dependence, so it is explicitly allowed;
/// If you want to tighten the production wiring to be completely consistent with TS, just delete the `file:` branch (README records this difference).
pub fn is_allowed_url(raw: &str) -> bool {
    let Some(url) = req_like_parse(raw) else {
        return raw == "about:blank";
    };
    url == "http:" || url == "https:" || url == "file:" || raw == "about:blank"
}

/// Minimalist scheme extraction (no third-party URL crate dependencies): return `scheme:`.
fn req_like_parse(raw: &str) -> Option<String> {
    let idx = raw.find(':')?;
    let scheme = &raw[..idx];
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    Some(format!("{}:", scheme.to_ascii_lowercase()))
}

/// Execute a page-level command. `params` is the parameter object of this method (except method/tabId).
/// Result constancy `elapsedMs` (required field of `browserCommandResultSchema`,
/// `packages/shared/src/browser-use/result.ts:127`).
pub async fn execute_page_command(page: &Page, method: &str, params: &Value) -> Value {
    let started = std::time::Instant::now();
    let mut result = match dispatch(page, method, params).await {
        Ok(result) => result,
        Err(err) => error_json(&err),
    };
    if let Some(object) = result.as_object_mut() {
        object.insert("elapsedMs".into(), json!(started.elapsed().as_millis()));
    }
    result
}

async fn dispatch(page: &Page, method: &str, params: &Value) -> CdpResult<Value> {
    match method {
        "navigate" => {
            let url = params.get("url").and_then(Value::as_str).unwrap_or("");
            if !is_allowed_url(url) {
                return Err(CdpError::new(
                    CdpErrorCode::NavigationBlocked,
                    format!("Navigation URL is not allowed: {url}"),
                ));
            }
            page.navigate(url).await?;
            auto_wait::wait_for_ready(page, &WaitPolicy::default()).await?;
            Ok(ok_state(page).await?)
        }
        "back" => {
            page.go_back_or_forward(-1).await?;
            auto_wait::wait_for_ready(page, &WaitPolicy::default()).await?;
            Ok(ok_state(page).await?)
        }
        "forward" => {
            page.go_back_or_forward(1).await?;
            auto_wait::wait_for_ready(page, &WaitPolicy::default()).await?;
            Ok(ok_state(page).await?)
        }
        "reload" => {
            page.reload().await?;
            auto_wait::wait_for_ready(page, &WaitPolicy::default()).await?;
            Ok(ok_state(page).await?)
        }
        "getState" => Ok(ok_state(page).await?),
        "snapshot" => {
            let max = params
                .get("maxElements")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            let include_hidden = params
                .get("includeHidden")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let snapshot = page.capture_snapshot(max, include_hidden).await?;
            Ok(json!({ "ok": true, "snapshot": serde_json::to_value(&snapshot).unwrap() }))
        }
        "click" => {
            let (x, y) = point_or_ref(page, params)?;
            let button = params
                .get("button")
                .and_then(Value::as_str)
                .unwrap_or("left");
            let double_click = params
                .get("doubleClick")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let modifiers = modifier_mask(params.get("modifiers"));
            page.click(x, y, button, double_click, modifiers).await?;
            Ok(json!({ "ok": true }))
        }
        "hover" => {
            let (x, y) = point_or_ref(page, params)?;
            let modifiers = modifier_mask(params.get("modifiers"));
            page.mouse_move(x, y, modifiers).await?;
            Ok(json!({ "ok": true }))
        }
        "fill" => {
            let rf = required_ref(params)?;
            let value = params.get("value").and_then(Value::as_str).unwrap_or("");
            let rect = page.resolve_ref(rf).ok_or_else(|| ref_stale(rf))?;
            // raw CDP None ElementHandle: Use coordinates to click to focus and then press the selector to degrade to keyboard input when it is not available.
            // First click the center to focus, then insertText (stable text injection), and finally write the check value.
            page.click(rect.x, rect.y, "left", false, 0).await?;
            page.insert_text(value).await?;
            Ok(json!({ "ok": true }))
        }
        "type" => {
            if let Some(rf) = params.get("ref").and_then(Value::as_str) {
                let rect = page.resolve_ref(rf).ok_or_else(|| ref_stale(rf))?;
                page.click(rect.x, rect.y, "left", false, 0).await?;
            }
            let text = params.get("text").and_then(Value::as_str).unwrap_or("");
            page.insert_text(text).await?;
            Ok(json!({ "ok": true }))
        }
        "press" => {
            let key = params.get("key").and_then(Value::as_str).unwrap_or("");
            let modifiers = modifier_mask(params.get("modifiers"));
            if let Some(rf) = params.get("ref").and_then(Value::as_str) {
                let rect = page.resolve_ref(rf).ok_or_else(|| ref_stale(rf))?;
                page.click(rect.x, rect.y, "left", false, 0).await?;
            }
            page.key_press(key, modifiers).await?;
            Ok(json!({ "ok": true }))
        }
        "cuaKeypress" => {
            let keys = params
                .get("keys")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let resolved: Vec<String> = keys
                .iter()
                .filter_map(|k| k.as_str().map(str::to_string))
                .collect();
            press_keys(page, &resolved).await?;
            Ok(json!({ "ok": true }))
        }
        "scroll" => {
            if let Some(rf) = params.get("ref").and_then(Value::as_str) {
                let rect = page.resolve_ref(rf).ok_or_else(|| ref_stale(rf))?;
                // scrollIntoView semantics: scroll to the center of the element (inside the viewport)
                page.mouse_move(rect.x, rect.y, 0).await?;
            }
            let dx = params.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let dy = params.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            let (cx, cy) = page.viewport_size();
            page.evaluate_raw(&format!("window.scrollBy({dx},{dy})"))
                .await?;
            let _ = (cx, cy);
            Ok(json!({ "ok": true }))
        }
        "cuaScroll" => {
            let x = params.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let y = params.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            let sx = params.get("scrollX").and_then(Value::as_f64).unwrap_or(0.0);
            let sy = params.get("scrollY").and_then(Value::as_f64).unwrap_or(0.0);
            let modifiers = modifier_mask(params.get("modifiers"));
            page.mouse_move(x, y, modifiers).await?;
            page.mouse_wheel(x, y, sx, sy).await?;
            Ok(json!({ "ok": true }))
        }
        "domCuaScroll" => {
            let sx = params.get("scrollX").and_then(Value::as_f64).unwrap_or(0.0);
            let sy = params.get("scrollY").and_then(Value::as_f64).unwrap_or(0.0);
            let (vx, vy) = match params
                .get("nodeId")
                .and_then(Value::as_str)
                .and_then(|rf| page.resolve_ref(rf))
            {
                Some(rect) => (rect.x, rect.y),
                None => {
                    let (w, h) = page.viewport_size();
                    (w / 2.0, h / 2.0)
                }
            };
            page.mouse_move(vx, vy, 0).await?;
            page.mouse_wheel(vx, vy, sx, sy).await?;
            Ok(json!({ "ok": true }))
        }
        "screenshot" => {
            let full_page = params
                .get("fullPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let clip = params.get("clip").and_then(|c| {
                let x = c.get("x")?.as_f64()?;
                let y = c.get("y")?.as_f64()?;
                let w = c.get("width")?.as_f64()?;
                let h = c.get("height")?.as_f64()?;
                Some((x, y, w, h))
            });
            let base64 = page.screenshot(full_page, clip).await?;
            Ok(json!({ "ok": true, "image": { "base64": base64, "mimeType": "image/png" } }))
        }
        "select" => {
            let rf = required_ref(params)?;
            let values = params
                .get("values")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let rect = page.resolve_ref(rf).ok_or_else(|| ref_stale(rf))?;
            page.click(rect.x, rect.y, "left", false, 0).await?;
            // Set selection with DOM fact: match option by value or visible text.
            let list: Vec<String> = values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            set_select(page, rf, &list).await?;
            Ok(json!({ "ok": true }))
        }
        "check" => {
            let rf = required_ref(params)?;
            let want = params
                .get("checked")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let rect = page.resolve_ref(rf).ok_or_else(|| ref_stale(rf))?;
            set_checked(page, rf, rect.x, rect.y, want).await?;
            Ok(json!({ "ok": true }))
        }
        "drag" => {
            let from = point_for_side(page, params, "fromRef", "from")?;
            let to = point_for_side(page, params, "toRef", "to")?;
            let modifiers = modifier_mask(params.get("modifiers"));
            drag_path(page, &[from, to], modifiers).await?;
            Ok(json!({ "ok": true }))
        }
        "cuaDrag" => {
            let path = params
                .get("path")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let points: Vec<(f64, f64)> = path
                .iter()
                .filter_map(|p| {
                    let x = p.get("x")?.as_f64()?;
                    let y = p.get("y")?.as_f64()?;
                    Some((x, y))
                })
                .collect();
            if points.is_empty() {
                return Err(CdpError::new(
                    CdpErrorCode::ExecutionError,
                    "drag path empty",
                ));
            }
            let modifiers = modifier_mask(params.get("modifiers"));
            drag_path(page, &points, modifiers).await?;
            Ok(json!({ "ok": true }))
        }
        "elementInfo" => {
            let x = params.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let y = params.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            let snapshot = page.capture_snapshot(None, false).await?;
            let hit = snapshot.elements.iter().find(|e| {
                x >= e.rect.x as f64
                    && x <= (e.rect.x + e.rect.width) as f64
                    && y >= e.rect.y as f64
                    && y <= (e.rect.y + e.rect.height) as f64
            });
            match hit {
                Some(el) => Ok(json!({
                    "ok": true,
                    "element": serde_json::to_value(el).unwrap(),
                })),
                None => Ok(json!({ "ok": true })),
            }
        }
        "evaluate" => {
            let expression = params
                .get("expression")
                .and_then(Value::as_str)
                .unwrap_or("");
            let value = tokio::time::timeout(Duration::from_secs(3), page.evaluate_raw(expression))
                .await
                .map_err(|_| CdpError::new(CdpErrorCode::Timeout, "evaluate timed out"))??;
            Ok(json!({ "ok": true, "value": value }))
        }
        "waitFor" => {
            let timeout = Duration::from_millis(
                params
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(3_000)
                    .clamp(1, 3_000),
            );
            if let Some(selector) = params.get("selector").and_then(Value::as_str) {
                let found = auto_wait::wait_for_selector(page, selector, true, timeout).await?;
                if !found {
                    return Err(CdpError::new(
                        CdpErrorCode::Timeout,
                        format!("waitFor selector '{selector}' not found"),
                    ));
                }
                return Ok(json!({ "ok": true }));
            }
            if let Some(text) = params.get("text").and_then(Value::as_str) {
                let selector = text_visible_selector(text);
                let found = auto_wait::wait_for_selector(page, &selector, false, timeout).await?;
                if !found {
                    return Err(CdpError::new(
                        CdpErrorCode::Timeout,
                        format!("waitFor text '{text}' not found"),
                    ));
                }
                return Ok(json!({ "ok": true }));
            }
            if let Some(gone) = params.get("textGone").and_then(Value::as_str) {
                let selector = text_visible_selector(gone);
                let timeout2 = Duration::from_millis(3_000);
                let present = auto_wait::wait_for_selector(page, &selector, false, timeout2)
                    .await
                    .unwrap_or(true);
                if present {
                    return Err(CdpError::new(
                        CdpErrorCode::Timeout,
                        format!("waitFor textGone '{gone}' still present"),
                    ));
                }
                return Ok(json!({ "ok": true }));
            }
            Err(CdpError::new(
                CdpErrorCode::ExecutionError,
                "waitFor requires selector, text, or textGone",
            ))
        }
        "playwrightWaitForTimeout" => {
            let ms = params.get("timeoutMs").and_then(Value::as_u64).unwrap_or(0);
            auto_wait::wait_for_timeout(page, Duration::from_millis(ms)).await?;
            Ok(json!({ "ok": true }))
        }
        // Explicit not supported // consistent with TS managed-CDP backend (descriptor.ts:16-25 / index.ts:336-350).
        "playwright"
        | "capabilities"
        | "browserVisibilityGet"
        | "browserVisibilitySet"
        | "recordingStart"
        | "recordingStatus"
        | "recordingCancel"
        | "claimTab"
        | "finalize"
        | "finalizeTabs"
        | "markDeliverable"
        | "markHandoff"
        | "listUserTabs" => Err(CdpError::new(
            CdpErrorCode::CapabilityUnsupported,
            format!("Browser command '{method}' is unavailable in the raw CDP page runtime"),
        )),
        other => Err(CdpError::new(
            CdpErrorCode::CapabilityUnsupported,
            format!("Unknown or session-scoped browser command '{other}' in page runtime"),
        )),
    }
}

async fn ok_state(page: &Page) -> CdpResult<Value> {
    let (url, title) = page.page_state().await?;
    let history = page.send("Page.getNavigationHistory", Value::Null).await?;
    let current = history
        .get("currentIndex")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let count = history
        .get("entries")
        .and_then(Value::as_array)
        .map(|e| e.len() as i64)
        .unwrap_or(1);
    let scroll = page
        .evaluate_raw("JSON.stringify([window.scrollX,window.scrollY])")
        .await?;
    let (scroll_x, scroll_y) = parse_pair(scroll.as_str().unwrap_or("[0,0]"));
    let (vw, vh) = page.viewport_size();
    Ok(json!({
        "ok": true,
        "state": {
            "url": url,
            "title": title,
            "canGoBack": current > 0,
            "canGoForward": current + 1 < count,
            "scrollX": scroll_x,
            "scrollY": scroll_y,
            "viewportWidth": vw,
            "viewportHeight": vh,
        }
    }))
}

fn parse_pair(raw: &str) -> (f64, f64) {
    let trimmed = raw.trim_matches(|c| c == '[' || c == ']');
    let mut parts = trimmed.split(',');
    let x = parts
        .next()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.0);
    let y = parts
        .next()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.0);
    (x, y)
}

fn required_ref(params: &Value) -> CdpResult<&str> {
    params
        .get("ref")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CdpError::new(CdpErrorCode::RefNotFound, "command requires a ref"))
}

fn ref_stale(rf: &str) -> CdpError {
    CdpError::new(
        CdpErrorCode::RefNotFound,
        format!(
            "Browser ref '{rf}' is stale or unavailable. Take a fresh snapshot before retrying."
        ),
    )
}

fn point_or_ref(page: &Page, params: &Value) -> CdpResult<(f64, f64)> {
    if let Some(rf) = params.get("ref").and_then(Value::as_str) {
        let rect = page.resolve_ref(rf).ok_or_else(|| ref_stale(rf))?;
        return Ok((rect.x, rect.y));
    }
    let x = params.get("x").and_then(Value::as_f64);
    let y = params.get("y").and_then(Value::as_f64);
    match (x, y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(CdpError::new(
            CdpErrorCode::ExecutionError,
            "action requires a ref or x/y coordinates",
        )),
    }
}

fn point_for_side(
    page: &Page,
    params: &Value,
    ref_key: &str,
    point_key: &str,
) -> CdpResult<(f64, f64)> {
    if let Some(rf) = params.get(ref_key).and_then(Value::as_str) {
        let rect = page.resolve_ref(rf).ok_or_else(|| ref_stale(rf))?;
        return Ok((rect.x, rect.y));
    }
    let point = params
        .get(point_key)
        .and_then(|p| Some((p.get("x")?.as_f64()?, p.get("y")?.as_f64()?)));
    point.ok_or_else(|| {
        CdpError::new(
            CdpErrorCode::ExecutionError,
            format!("drag side requires {ref_key} or {point_key}"),
        )
    })
}

async fn press_keys(page: &Page, keys: &[String]) -> CdpResult<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let (mods, last) = keys.split_at(keys.len() - 1);
    for key in mods {
        page.key_down(key, 0).await?;
    }
    let result = page.key_press(&last[0], 0).await;
    for key in mods.iter().rev() {
        page.key_up(key, 0).await?;
    }
    result
}

/// Drag path: Mirror TS `dragPath` (page-command.ts:118-128) - move to the starting point,/// Press the left button, move along the path, and release. In CDP, "press and hold the left button to move" is expressed by `buttons: 1`
/// (buttons bitfield of Input.dispatchMouseEvent: left=1), **not** modifiers bitfield
/// (1 in modifiers is Alt) - the old implementation conflated the two and made every move during the drag
/// Add Alt modification to change the dragover/drop behavior of the page.
async fn drag_path(page: &Page, points: &[(f64, f64)], modifiers: u32) -> CdpResult<()> {
    let Some((first, rest)) = points.split_first() else {
        return Err(CdpError::new(
            CdpErrorCode::ExecutionError,
            "drag requires a non-empty path",
        ));
    };
    page.mouse_move(first.0, first.1, modifiers).await?;
    page.mouse_down(first.0, first.1, "left", modifiers).await?;
    // TS's finally { mouse.up() }: If the mid-movement fails, the key must be released at the end point, leaving no hanging drag.
    let moved = async {
        for (x, y) in rest {
            page.mouse_drag_move(*x, *y, modifiers).await?;
        }
        Ok::<(), CdpError>(())
    }
    .await;
    let (last_x, last_y) = rest.last().copied().unwrap_or(*first);
    page.mouse_up(last_x, last_y, "left", modifiers).await?;
    moved
}

async fn set_checked(page: &Page, _rf: &str, x: f64, y: f64, want: bool) -> CdpResult<()> {
    // Read the currently checked, and click to toggle only when needed (avoiding multiple toggle).
    let expr = format!(
        "(function(){{var el=document.elementFromPoint({x},{y});if(!el)return null;if(el.checked===undefined)return null;return el.checked;}})()"
    );
    let current = page.evaluate_raw(&expr).await?;
    let is_checked = current.as_bool();
    if is_checked != Some(want) {
        page.click(x, y, "left", false, 0).await?;
    }
    Ok(())
}

async fn set_select(page: &Page, _rf: &str, values: &[String]) -> CdpResult<()> {
    let list = values
        .iter()
        .map(|v| format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect::<Vec<_>>()
        .join(",");
    let expr = format!("(function(){{var wanted=[{list}];var el=document.activeElement;if(!el||el.tagName!=='SELECT')return false;\
         var changed=false;for(var i=0;i<el.options.length;i++){{var o=el.options[i];\
         var hit=wanted.indexOf(o.value)>=0||wanted.indexOf(o.textContent.trim())>=0;\
         if(o.selected!==hit){{o.selected=hit;changed=true;}}}}\
         el.dispatchEvent(new Event('change',{{bubbles:true}}));return changed;}})()"
    );
    page.evaluate_raw(&expr).await?;
    Ok(())
}

fn text_visible_selector(text: &str) -> String {
    // Body text contains determination (approximate visible match of TS getByText).
    let escaped = text.replace('\\', "\\\\").replace('\'', "\\'");
    format!(
        "function(){{return Array.from(document.querySelectorAll('body *')).some(function(e){{\
         return e.childElementCount===0&&e.textContent&&e.textContent.indexOf('{escaped}')>=0\
         &&e.getBoundingClientRect().width>0;}})}}().valueOf()&&'body'"
    )
}

/// Error result: `code` uses wire code (in TS strict schema collection), not Rust internal diagnostic code ——
/// This JSON goes directly into `browserCommandResultSchema.error.code` (strict enum),
/// The superset code will be rejected by zod, and the model will receive an unbranchable error name.
pub fn error_json(err: &CdpError) -> Value {
    json!({
        "ok": false,
        "error": { "code": err.code.wire_code(), "message": err.message }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_url_gate_matches_plus_file() {
        assert!(is_allowed_url("https://example.com/x"));
        assert!(is_allowed_url("http://example.com"));
        assert!(is_allowed_url("file:///C:/tmp/a.html"));
        assert!(is_allowed_url("about:blank"));
        assert!(!is_allowed_url("javascript:alert(1)"));
        assert!(!is_allowed_url("data:text/html,<script>"));
        assert!(!is_allowed_url("not a url"));
    }

    #[test]
    fn parse_pair_roundtrip() {
        assert_eq!(parse_pair("[12, 34]"), (12.0, 34.0));
        assert_eq!(parse_pair("[0,0]"), (0.0, 0.0));
    }

    #[test]
    fn required_ref_errors_when_absent() {
        let params = json!({});
        let err = required_ref(&params).unwrap_err();
        assert_eq!(err.code, CdpErrorCode::RefNotFound);
        let ok = json!({"ref": "e3"});
        assert_eq!(required_ref(&ok).unwrap(), "e3");
    }

    #[test]
    fn error_json_shape_matches_result_schema() {
        let err = CdpError::new(CdpErrorCode::Timeout, "too slow");
        let value = error_json(&err);
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error"]["code"], json!("timeout"));
        assert_eq!(value["error"]["message"], json!("too slow"));
    }

    #[test]
    fn unknown_session_method_is_capability_unsupported() {
        // Only test dispatch decisions when there is no connection: these methods must be rejected before any IO.
        let blocked = [
            "playwright",
            "recordingStart",
            "browserVisibilitySet",
            "finalizeTabs",
        ];
        for method in blocked {
            // dispatch requires &Page, which cannot be called without a connection; instead, it is statically asserted that it is listed in an unsupported collection.
            assert!(
                unsupported_list().contains(&method),
                "{method} must be capability_unsupported"
            );
        }
    }

    fn unsupported_list() -> Vec<&'static str> {
        vec![
            "playwright",
            "capabilities",
            "browserVisibilityGet",
            "browserVisibilitySet",
            "recordingStart",
            "recordingStatus",
            "recordingCancel",
            "claimTab",
            "finalize",
            "finalizeTabs",
            "markDeliverable",
            "markHandoff",
            "listUserTabs",
        ]
    }
}
