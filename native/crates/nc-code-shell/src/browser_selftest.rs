//! `--browser-selftest`: real WebView2 round-trip for the native Browser port.
//!
//! This is a diagnostic mode, not a product surface: it builds the shell with a
//! hidden helper page, creates a real child webview, and runs the same
//! `ShellBrowserPort` the agent uses (`navigate` → `snapshot` → `click` →
//! `fill` → `evaluate` → `screenshot` → `getState`). Results are printed as JSON
//! lines on stderr and the process exits 0/1.
//!
//! It exists because the browser port's failure modes (WebView2 debugger
//! attachment, execution-context churn during navigation, ref resolution) cannot
//! be exercised by unit tests without a live webview.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::Manager;

use nc_code_host::tools::browser::{BrowserControlPort, BrowserTool};
use nc_code_host::tools::{AsyncTool, ToolContext};

use crate::browser_control::ShellBrowserPort;
use crate::browser_view::{BrowserViewBounds, BrowserViewCreatePayload, BrowserViewRegistry};

const SELFTEST_KEY: &str = "browser-selftest";
const STARTUP_DELAY: Duration = Duration::from_millis(800);

pub fn run() -> i32 {
    let registry = BrowserViewRegistry::default();
    let setup_registry = registry.clone();
    let exit_code = Arc::new(AtomicI32::new(1));
    let thread_exit_code = Arc::clone(&exit_code);
    let app = tauri::Builder::default()
        .manage(setup_registry)
        .setup(move |app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(STARTUP_DELAY);
                let code = match run_sequence(&handle) {
                    Ok(lines) => {
                        for line in lines {
                            eprintln!("[browser-selftest] {line}");
                        }
                        eprintln!("[browser-selftest] PASS");
                        0
                    }
                    Err(error) => {
                        eprintln!("[browser-selftest] FAIL: {error}");
                        1
                    }
                };
                thread_exit_code.store(code, Ordering::SeqCst);
                handle.exit(code);
            });
            Ok(())
        })
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            eprintln!("[browser-selftest] FAIL: app build: {error}");
            return 1;
        }
    };
    app.run(|_app, _event| {});
    exit_code.load(Ordering::SeqCst)
}

fn spawn_selftest_server() -> Result<String, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let request = String::from_utf8_lossy(&buffer);
            let path = request
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .split('?')
                .next()
                .unwrap_or("/");
            let response = match path {
                "/" | "/index.html" => html_response(SELFTEST_PAGE),
                "/frame" => html_response(FRAME_PAGE),
                "/download" => {
                    let body = "selftest-download";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=\"selftest-download.txt\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                }
                _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    Ok(format!("http://127.0.0.1:{port}/"))
}

fn html_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn run_sequence(handle: &tauri::AppHandle) -> Result<Vec<String>, String> {
    let window = handle
        .get_window("main")
        .ok_or_else(|| "main window is missing".to_string())?;
    let registry = handle.state::<BrowserViewRegistry>().inner().clone();
    let page_url = spawn_selftest_server()?;
    let download_url = format!("{page_url}download");

    // Webview creation must run on the main thread; the event loop is live.
    let (created_tx, created_rx) = mpsc::channel::<Result<(), String>>();
    let window_for_create = window.clone();
    let registry_for_create = registry.clone();
    let create_url = page_url.clone();
    handle
        .run_on_main_thread(move || {
            let result = crate::browser_view::create_webview(
                &window_for_create,
                &registry_for_create,
                BrowserViewCreatePayload {
                    key: SELFTEST_KEY.to_string(),
                    url: create_url,
                    bounds: BrowserViewBounds {
                        x: 0.0,
                        y: 0.0,
                        width: 800.0,
                        height: 600.0,
                    },
                    visible: Some(true),
                },
            )
            .map(|_| ());
            let _ = created_tx.send(result);
        })
        .map_err(|error| error.to_string())?;
    created_rx
        .recv_timeout(Duration::from_secs(15))
        .map_err(|_| "timed out creating the child webview".to_string())??;

    let registry_for_tool = registry.clone();
    let port = ShellBrowserPort::new(registry.clone());
    let mut lines = Vec::new();

    let navigate = block_on(port.execute(&json!({ "method": "navigate", "url": page_url })))?;
    lines.push(format!("navigate: {}", summary(&navigate)));
    ensure_ok("navigate", &navigate)?;

    let initial = registry
        .state(SELFTEST_KEY)
        .ok_or_else(|| "selftest webview state is missing".to_string())?;
    if !initial.visible || initial.occluded || !initial.effective_visible {
        return Err(format!("initial visibility is wrong: {initial:?}"));
    }
    let hidden = registry.apply_occluded(SELFTEST_KEY, true)?;
    if !hidden.occluded || hidden.effective_visible || hidden.visible != initial.visible {
        return Err(format!("occluded state is wrong: {hidden:?}"));
    }
    #[cfg(windows)]
    if crate::browser_view::native_visible(&registry, SELFTEST_KEY)? {
        return Err("native webview is still visible while occluded".to_string());
    }
    let restored = registry.apply_occluded(SELFTEST_KEY, false)?;
    if restored.occluded || !restored.effective_visible {
        return Err(format!("restored state is wrong: {restored:?}"));
    }
    #[cfg(windows)]
    if !crate::browser_view::native_visible(&registry, SELFTEST_KEY)? {
        return Err("native webview did not reappear after occlusion cleared".to_string());
    }
    lines.push(format!(
        "occlusion: hidden={:?} restored={:?}",
        hidden.effective_visible, restored.effective_visible
    ));

    let snapshot = block_on(port.execute(&json!({ "method": "snapshot" })))?;
    ensure_ok("snapshot", &snapshot)?;
    let button_ref =
        find_ref(&snapshot, "button").ok_or_else(|| "button ref missing".to_string())?;
    let input_ref = find_ref(&snapshot, "input").ok_or_else(|| "input ref missing".to_string())?;
    lines.push(format!(
        "snapshot: elements={} button={button_ref} input={input_ref}",
        snapshot
            .pointer("/snapshot/elements")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0)
    ));

    let (button_x, button_y) =
        element_center(&snapshot, &button_ref).ok_or_else(|| "button rect missing".to_string())?;
    let element_info =
        block_on(port.execute(&json!({ "method": "elementInfo", "x": button_x, "y": button_y })))?;
    ensure_ok("elementInfo", &element_info)?;
    let hit_tag = element_info
        .pointer("/element/tag")
        .and_then(Value::as_str)
        .unwrap_or("");
    lines.push(format!("elementInfo: tag={hit_tag:?}"));
    if hit_tag != "button" {
        return Err(format!("elementInfo hit {hit_tag:?}, expected button"));
    }

    let click = block_on(port.execute(&json!({ "method": "click", "ref": button_ref })))?;
    ensure_ok("click", &click)?;
    let title = click
        .pointer("/state/title")
        .and_then(Value::as_str)
        .unwrap_or("");
    lines.push(format!("click: title={title:?}"));
    if title != "clicked" {
        return Err(format!(
            "click did not run the page handler (title={title:?})"
        ));
    }

    let fill =
        block_on(port.execute(&json!({ "method": "fill", "ref": input_ref, "value": "hello" })))?;
    ensure_ok("fill", &fill)?;
    let value = block_on(port.execute(
        &json!({ "method": "evaluate", "expression": "document.querySelector('#name').value" }),
    ))?;
    ensure_ok("evaluate", &value)?;
    let filled = value.get("value").and_then(Value::as_str).unwrap_or("");
    lines.push(format!("fill/evaluate: value={filled:?}"));
    if filled != "hello" {
        return Err(format!("fill did not reach the input (value={filled:?})"));
    }

    let locator_count = block_on(port.execute(&json!({
        "method": "playwright",
        "action": { "name": "locator", "selector": "#go", "operation": "count" }
    })))?;
    ensure_ok("playwright count", &locator_count)?;
    let count = locator_count
        .get("value")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    lines.push(format!("playwright count: {count}"));
    if count != 1 {
        return Err(format!("playwright count expected 1, got {count}"));
    }

    let locator_text = block_on(port.execute(&json!({
        "method": "playwright",
        "action": { "name": "locator", "selector": "#go", "operation": "textContent" }
    })))?;
    ensure_ok("playwright textContent", &locator_text)?;
    let text = locator_text
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    lines.push(format!("playwright textContent: {text:?}"));
    if text != "Go" {
        return Err(format!(
            "playwright textContent expected 'Go', got {text:?}"
        ));
    }

    let locator_fill = block_on(port.execute(&json!({
        "method": "playwright",
        "action": {
            "name": "locator",
            "selector": "#name",
            "operation": "fill",
            "value": "locator"
        }
    })))?;
    ensure_ok("playwright fill", &locator_fill)?;
    let locator_value = block_on(port.execute(&json!({
        "method": "evaluate",
        "expression": "document.querySelector('#name').value"
    })))?;
    ensure_ok("playwright fill evaluate", &locator_value)?;
    let locator_value = locator_value
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or("");
    lines.push(format!("playwright fill: value={locator_value:?}"));
    if locator_value != "locator" {
        return Err(format!(
            "playwright fill did not reach the input (value={locator_value:?})"
        ));
    }

    let locator_click = block_on(port.execute(&json!({
        "method": "playwright",
        "action": { "name": "locator", "selector": "#go", "operation": "click" }
    })))?;
    ensure_ok("playwright click", &locator_click)?;
    let locator_state = block_on(port.execute(&json!({ "method": "getState" })))?;
    ensure_ok("playwright click getState", &locator_state)?;
    let locator_title = locator_state
        .pointer("/state/title")
        .and_then(Value::as_str)
        .unwrap_or("");
    lines.push(format!("playwright click: title={locator_title:?}"));
    if locator_title != "clicked" {
        return Err(format!(
            "playwright click did not run the page handler (title={locator_title:?})"
        ));
    }

    for (label, selector) in [
        ("xpath", "xpath=//button[@id='go']"),
        ("chain", "body >> #go"),
        ("nth", "#go >> nth=0"),
    ] {
        let result = block_on(port.execute(&json!({
            "method": "playwright",
            "action": { "name": "locator", "selector": selector, "operation": "count" }
        })))?;
        ensure_ok(&format!("playwright {label} count"), &result)?;
        let count = result.get("value").and_then(Value::as_u64).unwrap_or(0);
        lines.push(format!("playwright {label} count: {count}"));
        if count != 1 {
            return Err(format!(
                "playwright {label} selector expected 1, got {count}"
            ));
        }
    }

    let element_screenshot = block_on(port.execute(&json!({
        "method": "playwright",
        "action": { "name": "elementScreenshot", "x": button_x, "y": button_y }
    })))?;
    ensure_ok("playwright elementScreenshot", &element_screenshot)?;
    let element_image_bytes = element_screenshot
        .pointer("/image/base64")
        .and_then(Value::as_str)
        .map(str::len)
        .unwrap_or(0);
    lines.push(format!(
        "playwright elementScreenshot: base64_bytes={element_image_bytes}"
    ));
    if element_image_bytes < 100 {
        return Err("playwright elementScreenshot returned an empty image".to_string());
    }

    for (label, selector, expected) in [
        ("internal role", "internal:role=button", 1_u64),
        (
            "internal role name",
            "internal:role=button[name=\"Go\"i]",
            1,
        ),
        ("internal text strict", "internal:text=\"Go\"s", 1),
        (
            "internal testid",
            "internal:testid=[data-testid=\"go-button\"s]",
            1,
        ),
        ("internal attr", "internal:attr=[placeholder=\"Name\"i]", 1),
        ("internal label", "internal:label=\"Name\"i", 1),
        ("internal and", "#go >> internal:and=\"button\"", 1),
        ("internal or", "#go >> internal:or=\"#name\"", 2),
        ("internal has", "body >> internal:has=\"#go\"", 1),
        (
            "internal has-not",
            "body >> internal:has-not=\"#missing\"",
            1,
        ),
        ("internal has-text", "body >> internal:has-text=\"Go\"i", 1),
        (
            "internal frame",
            "#frame >> internal:control=enter-frame >> #frame-go",
            1,
        ),
    ] {
        let result = block_on(port.execute(&json!({
            "method": "playwright",
            "action": {
                "name": "locator",
                "selector": selector,
                "operation": "count",
                "timeoutMs": 5_000
            }
        })))?;
        ensure_ok(&format!("playwright {label} count"), &result)?;
        let count = result.get("value").and_then(Value::as_u64).unwrap_or(0);
        lines.push(format!("playwright {label} count: {count}"));
        if count != expected {
            return Err(format!(
                "playwright {label} selector expected {expected}, got {count}"
            ));
        }
    }

    let (download_id, download_path) = tauri::async_runtime::block_on(async {
        let wait_port = port.clone();
        let wait = tauri::async_runtime::spawn(async move {
            wait_port
                .execute(&json!({
                    "method": "playwright",
                    "action": {
                        "name": "waitForEvent",
                        "event": "download",
                        "timeoutMs": 10_000
                    }
                }))
                .await
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        port.cdp(None, "Page.navigate", json!({ "url": download_url }))
            .await
            .map_err(|(code, message)| format!("download navigate failed: {code}: {message}"))?;
        let wait = wait.await.map_err(|error| error.to_string())??;
        ensure_ok("playwright waitForEvent download", &wait)?;
        let id = wait
            .pointer("/value/id")
            .and_then(Value::as_str)
            .ok_or_else(|| "waitForEvent returned no download id".to_string())?
            .to_string();
        let path = port
            .execute(&json!({
                "method": "playwright",
                "action": {
                    "name": "downloadPath",
                    "downloadId": id,
                    "timeoutMs": 10_000
                }
            }))
            .await?;
        ensure_ok("playwright downloadPath", &path)?;
        let path = path
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| "downloadPath returned no path".to_string())?
            .to_string();
        Ok::<_, String>((id, path))
    })?;
    let download_bytes = std::fs::read(&download_path).map_err(|error| error.to_string())?;
    let download_text = String::from_utf8_lossy(&download_bytes);
    lines.push(format!(
        "playwright download: id={download_id} bytes={} text={download_text:?}",
        download_bytes.len()
    ));
    if download_text != "selftest-download" {
        return Err(format!("download content mismatch: {download_text:?}"));
    }

    let screenshot = block_on(port.execute(&json!({ "method": "screenshot" })))?;
    ensure_ok("screenshot", &screenshot)?;
    let image_bytes = screenshot
        .pointer("/image/base64")
        .and_then(Value::as_str)
        .map(str::len)
        .unwrap_or(0);
    lines.push(format!("screenshot: base64_bytes={image_bytes}"));
    if image_bytes < 100 {
        return Err("screenshot returned an empty image".to_string());
    }

    let state = block_on(port.execute(&json!({ "method": "getState" })))?;
    ensure_ok("getState", &state)?;
    lines.push(format!(
        "getState: url={:?}",
        state
            .pointer("/state/url")
            .and_then(Value::as_str)
            .unwrap_or("")
    ));

    // Exercise the host-facing tool wrapper, not just the port.
    let tool_ctx = ToolContext {
        workspace_root: std::env::temp_dir(),
        full_access: false,
        interactive: false,
        trace_id: "browser-selftest".to_string(),
        turn_id: "browser-selftest".to_string(),
        read_state: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        todos: nc_code_host::tools::todo::new_todo_store(),
        provider_config: None,
        session_id: "browser-selftest".to_string(),
        model_ref: None,
        tool_call_id: None,
        plan_mode: false,
        ask_user: None,
        plan: None,
        subagent: None,
        background: None,
        sessions: None,
        workflow: None,
        automation: None,
        browser: Some(Arc::new(ShellBrowserPort::new(registry_for_tool))),
        computer: None,
        offpeak: None,
    };
    let tool_output = tauri::async_runtime::block_on(
        BrowserTool.execute(&json!({ "method": "getState" }), &tool_ctx),
    )
    .map_err(|error| format!("Browser tool failed: {}", error.message))?;
    let tool_json: Value = serde_json::from_str(&tool_output)
        .map_err(|error| format!("Browser tool output is not JSON: {error}"))?;
    ensure_ok("Browser tool getState", &tool_json)?;
    lines.push(format!(
        "tool getState: ok=true bytes={}",
        tool_output.len()
    ));

    Ok(lines)
}

fn block_on(
    future: impl std::future::Future<Output = Result<Value, String>>,
) -> Result<Value, String> {
    tauri::async_runtime::block_on(future)
}

fn ensure_ok(step: &str, result: &Value) -> Result<(), String> {
    if result.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    let code = result
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = result
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    Err(format!("{step} failed: {code}: {message}"))
}

fn find_ref(result: &Value, tag: &str) -> Option<String> {
    result
        .pointer("/snapshot/elements")?
        .as_array()?
        .iter()
        .find(|element| element.get("tag").and_then(Value::as_str) == Some(tag))
        .and_then(|element| element.get("ref"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn element_center(result: &Value, reference: &str) -> Option<(f64, f64)> {
    let element = result
        .pointer("/snapshot/elements")?
        .as_array()?
        .iter()
        .find(|element| element.get("ref").and_then(Value::as_str) == Some(reference))?;
    let rect = element.get("rect")?;
    let x = rect.get("x")?.as_f64()?;
    let y = rect.get("y")?.as_f64()?;
    let width = rect.get("width")?.as_f64()?;
    let height = rect.get("height")?.as_f64()?;
    Some((x + width / 2.0, y + height / 2.0))
}

fn summary(result: &Value) -> String {
    format!(
        "ok={} elapsedMs={}",
        result.get("ok").and_then(Value::as_bool).unwrap_or(false),
        result.get("elapsedMs").and_then(Value::as_u64).unwrap_or(0)
    )
}

const SELFTEST_PAGE: &str = r#"<!doctype html>
<html>
  <head><meta charset="utf-8" /><title>selftest</title></head>
  <body>
    <h1>ZCode browser selftest</h1>
    <button id="go" data-testid="go-button" onclick="document.title='clicked'; document.getElementById('out').textContent='clicked';">
      Go
    </button>
    <label for="name">Name</label>
    <input id="name" placeholder="Name" />
    <iframe id="frame" src="/frame" width="320" height="120"></iframe>
    <div id="out"></div>
  </body>
</html>
"#;

const FRAME_PAGE: &str = r#"<!doctype html>
<html>
  <head><meta charset="utf-8" /><title>selftest frame</title></head>
  <body>
    <button id="frame-go">Frame</button>
  </body>
</html>
"#;
