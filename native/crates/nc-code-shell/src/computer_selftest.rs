//! `--computer-selftest`: real Windows computer-use round-trip.
//!
//! This diagnostic creates a local Tauri window with a known layout, then uses
//! the same `ShellComputerControl` the `Computer` tool uses to list windows,
//! activate the window, click/type/press/scroll/drag through `SendInput`, and
//! capture the window as PNG. It verifies the page state after each step.
//!
//! It is gated behind an explicit CLI flag and is not a product surface.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use crate::computer_control::ShellComputerControl;

const WINDOW_TITLE: &str = "ZCode computer selftest";
const STARTUP_DELAY: Duration = Duration::from_millis(1200);
const STEP_TIMEOUT: Duration = Duration::from_secs(8);

fn execute(command: Value) -> Result<Value, String> {
    let port = ShellComputerControl;
    tauri::async_runtime::block_on(async move {
        use nc_code_host::tools::computer::ComputerControlPort;
        port.execute(&command).await
    })
}

fn require_ok(step: &str, result: &Value) -> Result<(), String> {
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

fn eval(window: &tauri::WebviewWindow, script: &str) -> Result<String, String> {
    let (sender, receiver) = mpsc::channel();
    window
        .eval_with_callback(script, move |value| {
            let _ = sender.send(value);
        })
        .map_err(|error| error.to_string())?;
    receiver
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| format!("eval timed out: {script}"))
}

fn wait_until(
    window: &tauri::WebviewWindow,
    script: &str,
    predicate: impl Fn(&str) -> bool,
    label: &str,
) -> Result<String, String> {
    let deadline = std::time::Instant::now() + STEP_TIMEOUT;
    loop {
        if let Ok(value) = eval(window, script) {
            if predicate(&value) {
                return Ok(value);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("timed out waiting for {label}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn run() -> i32 {
    let exit_code = Arc::new(AtomicI32::new(1));
    let thread_exit_code = Arc::clone(&exit_code);
    let app = tauri::Builder::default()
        .setup(move |app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(STARTUP_DELAY);
                let code = match run_sequence(&handle) {
                    Ok(lines) => {
                        for line in lines {
                            eprintln!("[computer-selftest] {line}");
                        }
                        eprintln!("[computer-selftest] PASS");
                        0
                    }
                    Err(error) => {
                        eprintln!("[computer-selftest] FAIL: {error}");
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
            eprintln!("[computer-selftest] FAIL: app build: {error}");
            return 1;
        }
    };
    app.run(|_app, _event| {});
    exit_code.load(Ordering::SeqCst)
}

fn run_sequence(handle: &tauri::AppHandle) -> Result<Vec<String>, String> {
    let (created_tx, created_rx) = mpsc::channel::<Result<(), String>>();
    let builder_handle = handle.clone();
    handle
        .run_on_main_thread(move || {
            let result = WebviewWindowBuilder::new(
                &builder_handle,
                "computer-selftest",
                WebviewUrl::App("computer-selftest.html".into()),
            )
            .title(WINDOW_TITLE)
            .inner_size(700.0, 500.0)
            .position(120.0, 120.0)
            .build()
            .map(|_| ())
            .map_err(|error| error.to_string());
            let _ = created_tx.send(result);
        })
        .map_err(|error| error.to_string())?;
    created_rx
        .recv_timeout(Duration::from_secs(15))
        .map_err(|_| "timed out creating the selftest window".to_string())??;

    let window = handle
        .get_webview_window("computer-selftest")
        .ok_or_else(|| "selftest window is missing".to_string())?;
    wait_until(
        &window,
        "document.readyState",
        |value| value.contains("complete"),
        "document.readyState=complete",
    )?;

    let mut lines = Vec::new();
    let apps = execute(json!({ "method": "listApps" }))?;
    require_ok("listApps", &apps)?;
    let app = apps
        .get("apps")
        .and_then(Value::as_array)
        .and_then(|apps| {
            apps.iter()
                .find(|app| app.get("title").and_then(Value::as_str) == Some(WINDOW_TITLE))
        })
        .cloned()
        .ok_or_else(|| "selftest window is not in listApps".to_string())?;
    let window_id = app
        .get("windowId")
        .and_then(Value::as_u64)
        .ok_or_else(|| "listApps entry has no windowId".to_string())?;
    let origin_x = app
        .pointer("/clientOrigin/x")
        .and_then(Value::as_i64)
        .ok_or_else(|| "listApps entry has no clientOrigin".to_string())?;
    let origin_y = app
        .pointer("/clientOrigin/y")
        .and_then(Value::as_i64)
        .ok_or_else(|| "listApps entry has no clientOrigin".to_string())?;
    lines.push(format!(
        "listApps: windowId={window_id} pid={} origin=({origin_x},{origin_y})",
        app.get("pid").and_then(Value::as_u64).unwrap_or(0)
    ));

    let scale: f64 = eval(&window, "window.devicePixelRatio")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1.0);
    let point = |x: f64, y: f64| (origin_x as f64 + x * scale, origin_y as f64 + y * scale);
    let (input_x, input_y) = point(24.0 + 160.0, 24.0 + 20.0);
    let (button_x, button_y) = point(24.0 + 60.0, 84.0 + 20.0);

    let mut activated = false;
    let mut last_error = String::new();
    for _ in 0..5 {
        let result = execute(json!({ "method": "activate", "windowId": window_id }))?;
        match require_ok("activate", &result) {
            Ok(()) => {
                activated = true;
                break;
            }
            Err(error) => {
                last_error = error;
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
    if !activated {
        return Err(last_error);
    }
    lines.push("activate: foreground granted".to_string());

    let state = execute(json!({ "method": "getState" }))?;
    require_ok("getState", &state)?;
    let foreground_id = state
        .pointer("/foreground/windowId")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if foreground_id != window_id {
        return Err(format!(
            "foreground window {foreground_id} does not match {window_id}"
        ));
    }
    lines.push(format!(
        "getState: cursor={:?} screen={:?}",
        state.get("cursor"),
        state.get("screen")
    ));

    let click = execute(json!({ "method": "click", "x": input_x, "y": input_y }))?;
    require_ok("click input", &click)?;
    let typed = execute(json!({ "method": "type", "text": "hello" }))?;
    require_ok("type", &typed)?;
    let value = wait_until(
        &window,
        "document.getElementById('input').value",
        |value| value.contains("hello"),
        "input value=hello",
    )?;
    lines.push(format!("type: input value={value:?}"));

    let click = execute(json!({ "method": "click", "x": button_x, "y": button_y }))?;
    require_ok("click button", &click)?;
    let clicked = wait_until(
        &window,
        "document.getElementById('button').dataset.clicked ?? ''",
        |value| value.contains('1'),
        "button click handler",
    )?;
    lines.push(format!("click: button clicked={clicked:?}"));

    let scroll = execute(json!({ "method": "scroll", "deltaY": 3 }))?;
    require_ok("scroll", &scroll)?;
    std::thread::sleep(Duration::from_millis(250));
    let probe = eval(
        &window,
        "JSON.stringify({y: window.scrollY, wheels: window.__wheels ?? -1, height: document.body.scrollHeight, inner: window.innerHeight})",
    )
    .unwrap_or_default();
    lines.push(format!("scroll probe: {probe}"));
    let scroll_y = wait_until(
        &window,
        "window.scrollY > 0 ? 'yes' : 'no'",
        |value| value.contains("yes"),
        "scrollY > 0",
    )
    .map_err(|error| format!("{error}; probe={probe}"))?;
    lines.push(format!("scroll: {probe} (yes={scroll_y})"));

    let press = execute(json!({ "method": "press", "key": "Tab" }))?;
    require_ok("press", &press)?;
    lines.push("press: Tab accepted".to_string());

    let screenshot = execute(json!({
        "method": "screenshot",
        "windowId": window_id,
        "maxBytes": 4_000_000
    }))?;
    require_ok("screenshot", &screenshot)?;
    let base64 = screenshot
        .pointer("/image/base64")
        .and_then(Value::as_str)
        .ok_or_else(|| "screenshot has no base64".to_string())?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64)
        .map_err(|error| error.to_string())?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("screenshot is not a PNG".to_string());
    }
    lines.push(format!(
        "screenshot: {} bytes, {}x{}",
        bytes.len(),
        screenshot
            .pointer("/image/width")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        screenshot
            .pointer("/image/height")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    ));

    let drag = execute(json!({
        "method": "drag",
        "fromX": input_x,
        "fromY": input_y,
        "toX": button_x,
        "toY": button_y
    }))?;
    require_ok("drag", &drag)?;
    let cursor = drag.get("cursor").cloned().unwrap_or(Value::Null);
    let cursor_x = cursor.get("x").and_then(Value::as_i64).unwrap_or(0);
    let cursor_y = cursor.get("y").and_then(Value::as_i64).unwrap_or(0);
    if (cursor_x - button_x.round() as i64).abs() > 2
        || (cursor_y - button_y.round() as i64).abs() > 2
    {
        return Err(format!(
            "drag ended at ({cursor_x},{cursor_y}), expected ({:.0},{:.0})",
            button_x, button_y
        ));
    }
    lines.push(format!("drag: cursor=({cursor_x},{cursor_y})"));

    let wait = execute(json!({ "method": "wait", "timeoutMs": 20 }))?;
    require_ok("wait", &wait)?;
    lines.push("wait: 20ms".to_string());

    Ok(lines)
}
