//! `--browser-recording-selftest`: real WebView2 recording round-trip.
//!
//! This diagnostic creates a real child webview, opens a local selftest page,
//! and has that page capture the child over CDP, encode WebM with
//! `MediaRecorder`, and write it through `save_browser_recording`. It verifies
//! the resulting file (EBML magic + byte count) before exiting 0/1.
//!
//! It exists because the recording pipeline's failure modes (MediaRecorder
//! codec support, canvas capture, CDP screenshot cadence, workspace write)
//! cannot be exercised by unit tests without a live webview. It is gated behind
//! an explicit CLI flag and is not a product surface.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use crate::browser_view::{
    create_webview, BrowserViewBounds, BrowserViewCreatePayload, BrowserViewRegistry,
};

const SELFTEST_KEY: &str = "recording-selftest-child";
const STARTUP_DELAY: Duration = Duration::from_millis(1200);
const RESULT_TIMEOUT: Duration = Duration::from_secs(40);
const WEBM_EBML_MAGIC: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];

pub struct RecordingSelftestState {
    key: String,
    workspace_path: PathBuf,
    relative_path: String,
    sender: Mutex<Option<mpsc::Sender<Value>>>,
}

#[tauri::command]
fn recording_selftest_config(state: tauri::State<'_, RecordingSelftestState>) -> Value {
    json!({
        "key": state.key,
        "workspacePath": state.workspace_path.to_string_lossy(),
        "relativePath": state.relative_path,
    })
}

#[tauri::command]
fn recording_selftest_result(
    state: tauri::State<'_, RecordingSelftestState>,
    result: Value,
) -> bool {
    let sender = state.sender.lock().ok().and_then(|mut guard| guard.take());
    sender
        .map(|sender| sender.send(result).is_ok())
        .unwrap_or(false)
}

pub fn run() -> i32 {
    let workspace = std::env::temp_dir().join(format!(
        "zcode-recording-selftest-ws-{}",
        std::process::id()
    ));
    if let Err(error) = std::fs::create_dir_all(&workspace) {
        eprintln!("[browser-recording-selftest] FAIL: workspace: {error}");
        return 1;
    }
    let registry = BrowserViewRegistry::default();
    let (sender, receiver) = mpsc::channel();
    let exit_code = Arc::new(AtomicI32::new(1));
    let thread_exit_code = Arc::clone(&exit_code);
    let app = tauri::Builder::default()
        .manage(registry.clone())
        .manage(RecordingSelftestState {
            key: SELFTEST_KEY.to_string(),
            workspace_path: workspace.clone(),
            relative_path: "recordings/selftest.webm".to_string(),
            sender: Mutex::new(Some(sender)),
        })
        .invoke_handler(tauri::generate_handler![
            recording_selftest_config,
            recording_selftest_result,
            crate::browser_view::browser_view_cdp,
            crate::save_browser_recording
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(STARTUP_DELAY);
                let code = match run_sequence(&handle, &registry, &workspace, receiver) {
                    Ok(lines) => {
                        for line in lines {
                            eprintln!("[browser-recording-selftest] {line}");
                        }
                        eprintln!("[browser-recording-selftest] PASS");
                        0
                    }
                    Err(error) => {
                        eprintln!("[browser-recording-selftest] FAIL: {error}");
                        1
                    }
                };
                thread_exit_code.store(code, Ordering::SeqCst);
                if std::env::var("ZCODE_RECORDING_SELFTEST_KEEP")
                    .ok()
                    .as_deref()
                    != Some("1")
                {
                    let _ = std::fs::remove_dir_all(&workspace);
                }
                handle.exit(code);
            });
            Ok(())
        })
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            eprintln!("[browser-recording-selftest] FAIL: app build: {error}");
            return 1;
        }
    };
    app.run(|_app, _event| {});
    exit_code.load(Ordering::SeqCst)
}

fn run_sequence(
    handle: &tauri::AppHandle,
    registry: &BrowserViewRegistry,
    workspace: &Path,
    receiver: mpsc::Receiver<Value>,
) -> Result<Vec<String>, String> {
    let window = handle
        .get_window("main")
        .ok_or_else(|| "main window is missing".to_string())?;
    let page_path = std::env::temp_dir().join(format!(
        "zcode-recording-selftest-child-{}.html",
        std::process::id()
    ));
    std::fs::write(&page_path, CHILD_PAGE).map_err(|error| error.to_string())?;
    let page_url = tauri::Url::from_file_path(&page_path)
        .map_err(|_| "selftest page path is not a file URL".to_string())?
        .to_string();

    let (created_tx, created_rx) = mpsc::channel::<Result<(), String>>();
    let window_for_create = window.clone();
    let registry_for_create = registry.clone();
    handle
        .run_on_main_thread(move || {
            let result = create_webview(
                &window_for_create,
                &registry_for_create,
                BrowserViewCreatePayload {
                    key: SELFTEST_KEY.to_string(),
                    url: page_url,
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

    let (window_tx, window_rx) = mpsc::channel::<Result<(), String>>();
    let handle_for_window = handle.clone();
    handle
        .run_on_main_thread(move || {
            let result = WebviewWindowBuilder::new(
                &handle_for_window,
                "recording-selftest",
                WebviewUrl::App("recording-selftest.html".into()),
            )
            .title("ZCode recording selftest")
            .inner_size(900.0, 700.0)
            .build()
            .map(|_| ())
            .map_err(|error| error.to_string());
            let _ = window_tx.send(result);
        })
        .map_err(|error| error.to_string())?;
    window_rx
        .recv_timeout(Duration::from_secs(15))
        .map_err(|_| "timed out creating the selftest window".to_string())??;

    let result = receiver
        .recv_timeout(RESULT_TIMEOUT)
        .map_err(|_| "timed out waiting for the renderer selftest result".to_string())?;
    let _ = std::fs::remove_file(&page_path);
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("renderer selftest failed");
        return Err(error.to_string());
    }
    let path = result
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "selftest result has no artifact path".to_string())?;
    let bytes = result
        .get("bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| "selftest result has no byte count".to_string())?;
    let target = PathBuf::from(path);
    if !target.starts_with(workspace) {
        return Err(format!("artifact escaped the workspace: {path}"));
    }
    let data = std::fs::read(&target).map_err(|error| error.to_string())?;
    if data.len() as u64 != bytes {
        return Err(format!(
            "artifact size mismatch: reported {bytes}, read {}",
            data.len()
        ));
    }
    if !data.starts_with(&WEBM_EBML_MAGIC) {
        return Err("artifact does not start with the WebM EBML magic".to_string());
    }
    if data.len() < 1_000 {
        return Err(format!(
            "artifact is too small to be a real recording: {}",
            data.len()
        ));
    }
    let frames = result.get("frames").and_then(Value::as_u64).unwrap_or(0);
    let mime = result
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("video/webm");
    let actions = result.get("actions").and_then(Value::as_u64).unwrap_or(0);
    if actions != 3 {
        return Err(format!("expected 3 scripted actions, got {actions}"));
    }
    if result.get("cursorVisible").and_then(Value::as_bool) != Some(true) {
        return Err("cursor overlay was not visible in the captured frame".to_string());
    }
    Ok(vec![
        format!("capture: {frames} frames, {mime}, {actions} scripted actions"),
        format!("cursor: visible (pixel {:?})", result.get("cursorPixel")),
        format!("artifact: {path} ({} bytes)", data.len()),
    ])
}

const CHILD_PAGE: &str = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <title>recording selftest child</title>
    <style>
      body { margin: 0; font-family: system-ui, sans-serif; background: #0f1115; color: #e6e8ee; }
      main { padding: 32px; }
      .dot { width: 64px; height: 64px; border-radius: 50%; background: #3b82f6; animation: pulse 1s infinite alternate; }
      @keyframes pulse { from { transform: translateX(0); opacity: .4; } to { transform: translateX(200px); opacity: 1; } }
    </style>
  </head>
  <body>
    <main>
      <h1>Recording selftest child</h1>
      <div class="dot"></div>
      <p>This page is captured by the recording selftest.</p>
    </main>
  </body>
</html>
"#;
