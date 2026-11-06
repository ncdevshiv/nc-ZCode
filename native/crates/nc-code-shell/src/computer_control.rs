//! Native computer-use port for the Tauri shell.
//!
//! The Electron product used the external `computer-use` plugin (MCP server +
//! native Helper). The Rust shell has no plugin runtime, so it implements the
//! same observation/input surface directly: window enumeration, foreground and
//! cursor state, PNG screen capture, and `SendInput` mouse/keyboard events.
//!
//! Windows is the implemented target. Other platforms return an explicit
//! `capability_unsupported` result instead of pretending the desktop was
//! observed or driven.

use async_trait::async_trait;
use nc_code_host::tools::computer::ComputerControlPort;
use serde_json::{json, Value};

pub struct ShellComputerControl;

#[async_trait]
impl ComputerControlPort for ShellComputerControl {
    async fn execute(&self, command: &Value) -> Result<Value, String> {
        let command = command.clone();
        #[cfg(windows)]
        {
            return tokio::task::spawn_blocking(move || windows_impl::execute(&command))
                .await
                .map_err(|error| format!("computer_task_join_failed: {error}"));
        }
        #[cfg(not(windows))]
        {
            let method = command
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Ok(error_result(
                "capability_unsupported",
                &format!(
                    "computer.{method} requires Windows (SendInput/GDI); this platform has no native computer-use backend"
                ),
            ))
        }
    }
}

pub(crate) fn error_result(code: &str, message: &str) -> Value {
    json!({
        "ok": false,
        "error": { "code": code, "message": message }
    })
}

#[cfg(windows)]
mod windows_impl {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use std::path::PathBuf;

    use super::error_result;
    use serde_json::{json, Value};
    use windows::core::{BOOL, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        GetDC, GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        CAPTUREBLT, DIB_RGB_COLORS, HGDIOBJ, SRCCOPY,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetCursorPos, GetForegroundWindow, GetSystemMetrics,
        GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible, SetCursorPos, SetForegroundWindow, ShowWindow, SM_CXVIRTUALSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE,
    };

    const MAX_WINDOWS: usize = 200;
    const MAX_CAPTURE_PIXELS: u64 = 64 * 1024 * 1024;

    #[derive(Clone)]
    struct WindowInfo {
        window_id: u64,
        pid: u32,
        name: String,
        title: String,
        bounds: (i32, i32, i32, i32),
        client_origin: (i32, i32),
        client_size: (i32, i32),
        foreground: bool,
    }

    pub fn execute(command: &Value) -> Value {
        let Some(method) = command.get("method").and_then(Value::as_str) else {
            return error_result("invalid_input", "computer command is missing `method`");
        };
        match method {
            "listApps" => list_apps(),
            "getState" => get_state(),
            "screenshot" => screenshot(command),
            "activate" => activate(command),
            "move" => move_cursor(command),
            "click" => click(command),
            "scroll" => scroll(command),
            "type" => type_text(command),
            "press" => press_key(command),
            "drag" => drag(command),
            "wait" => {
                let millis = command
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(30_000);
                std::thread::sleep(std::time::Duration::from_millis(millis));
                ok(json!({ "waitedMs": millis }))
            }
            other => error_result(
                "invalid_input",
                &format!("unsupported computer method `{other}`"),
            ),
        }
    }

    fn ok(value: Value) -> Value {
        match value {
            Value::Object(mut object) => {
                object.insert("ok".to_string(), Value::Bool(true));
                Value::Object(object)
            }
            other => json!({ "ok": true, "value": other }),
        }
    }

    fn windows_list() -> Vec<WindowInfo> {
        let mut list: Vec<WindowInfo> = Vec::new();
        let pointer = &mut list as *mut Vec<WindowInfo>;
        unsafe {
            let _ = EnumWindows(Some(enum_window), LPARAM(pointer as isize));
        }
        let foreground = unsafe { GetForegroundWindow() };
        for window in &mut list {
            window.foreground = window.window_id == hwnd_value(foreground);
        }
        list.truncate(MAX_WINDOWS);
        list
    }

    unsafe extern "system" fn enum_window(window: HWND, lparam: LPARAM) -> BOOL {
        let list = unsafe { &mut *(lparam.0 as *mut Vec<WindowInfo>) };
        if list.len() >= MAX_WINDOWS {
            return BOOL(0);
        }
        if !unsafe { IsWindowVisible(window) }.as_bool() {
            return BOOL(1);
        }
        let title = window_title(window);
        if title.trim().is_empty() {
            return BOOL(1);
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        if pid == 0 {
            return BOOL(1);
        }
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(window, &mut rect) }.is_err() {
            return BOOL(1);
        }
        let mut client = RECT::default();
        let _ = unsafe { GetClientRect(window, &mut client) };
        let mut origin = POINT { x: 0, y: 0 };
        let _ = unsafe { ClientToScreen(window, &mut origin) };
        list.push(WindowInfo {
            window_id: hwnd_value(window),
            pid,
            name: process_name(pid).unwrap_or_else(|| format!("pid-{pid}")),
            title,
            bounds: (
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
            ),
            client_origin: (origin.x, origin.y),
            client_size: (client.right - client.left, client.bottom - client.top),
            foreground: false,
        });
        BOOL(1)
    }

    fn hwnd_value(window: HWND) -> u64 {
        window.0 as usize as u64
    }

    fn hwnd_from(value: u64) -> HWND {
        HWND(value as usize as *mut c_void)
    }

    fn window_title(window: HWND) -> String {
        let length = unsafe { GetWindowTextLengthW(window) };
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let read = unsafe { GetWindowTextW(window, &mut buffer) };
        String::from_utf16_lossy(&buffer[..read.max(0) as usize])
    }

    fn process_name(pid: u32) -> Option<String> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buffer = vec![0u16; 1024];
            let mut size = buffer.len() as u32;
            let result = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut size,
            );
            let _ = CloseHandle(handle);
            result.ok()?;
            let path = String::from_utf16_lossy(&buffer[..size as usize]);
            PathBuf::from(path)
                .file_stem()
                .map(|name| name.to_string_lossy().to_string())
        }
    }

    fn window_json(window: &WindowInfo) -> Value {
        json!({
            "windowId": window.window_id,
            "pid": window.pid,
            "name": window.name,
            "title": window.title,
            "bounds": {
                "x": window.bounds.0,
                "y": window.bounds.1,
                "width": window.bounds.2,
                "height": window.bounds.3,
            },
            "clientOrigin": { "x": window.client_origin.0, "y": window.client_origin.1 },
            "clientSize": { "width": window.client_size.0, "height": window.client_size.1 },
            "foreground": window.foreground,
        })
    }

    fn list_apps() -> Value {
        let windows = windows_list();
        ok(json!({
            "apps": windows.iter().map(window_json).collect::<Vec<_>>(),
            "count": windows.len(),
        }))
    }

    fn get_state() -> Value {
        let windows = windows_list();
        let foreground = windows.iter().find(|window| window.foreground);
        let mut cursor = POINT { x: 0, y: 0 };
        let cursor_ok = unsafe { GetCursorPos(&mut cursor) }.is_ok();
        let screen = unsafe {
            json!({
                "x": GetSystemMetrics(SM_XVIRTUALSCREEN),
                "y": GetSystemMetrics(SM_YVIRTUALSCREEN),
                "width": GetSystemMetrics(SM_CXVIRTUALSCREEN),
                "height": GetSystemMetrics(SM_CYVIRTUALSCREEN),
            })
        };
        ok(json!({
            "foreground": foreground.map(window_json),
            "cursor": if cursor_ok { json!({ "x": cursor.x, "y": cursor.y }) } else { Value::Null },
            "screen": screen,
            "apps": windows.iter().map(window_json).collect::<Vec<_>>(),
        }))
    }

    fn find_window(windows: &[WindowInfo], command: &Value) -> Option<WindowInfo> {
        if let Some(id) = command.get("windowId").and_then(Value::as_u64) {
            return windows
                .iter()
                .find(|window| window.window_id == id)
                .cloned();
        }
        if let Some(pid) = command.get("pid").and_then(Value::as_u64) {
            return windows
                .iter()
                .find(|window| u64::from(window.pid) == pid)
                .cloned();
        }
        windows.iter().find(|window| window.foreground).cloned()
    }

    fn activate(command: &Value) -> Value {
        let windows = windows_list();
        let Some(window) = find_window(&windows, command) else {
            return error_result("window_not_found", "no matching window is visible");
        };
        let hwnd = hwnd_from(window.window_id);
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        let foreground = unsafe { GetForegroundWindow() };
        if hwnd_value(foreground) != window.window_id {
            return error_result(
                "foreground_denied",
                "Windows refused to bring the window to the foreground",
            );
        }
        ok(json!({ "window": window_json(&window) }))
    }

    fn move_cursor(command: &Value) -> Value {
        let (x, y) = match point(command) {
            Ok(point) => point,
            Err(error) => return error,
        };
        if let Err(error) = unsafe { SetCursorPos(x, y) } {
            return error_result("input_failed", &format!("SetCursorPos failed: {error}"));
        }
        cursor_json()
    }

    fn cursor_json() -> Value {
        let mut cursor = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return error_result("input_failed", "GetCursorPos failed");
        }
        ok(json!({ "cursor": { "x": cursor.x, "y": cursor.y } }))
    }

    fn point(command: &Value) -> Result<(i32, i32), Value> {
        let x = command.get("x").and_then(Value::as_f64);
        let y = command.get("y").and_then(Value::as_f64);
        match (x, y) {
            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => {
                Ok((x.round() as i32, y.round() as i32))
            }
            _ => Err(error_result(
                "invalid_input",
                "computer command requires finite `x` and `y`",
            )),
        }
    }

    fn mouse_flags(button: &str, down: bool) -> Option<MOUSE_EVENT_FLAGS> {
        Some(match (button, down) {
            ("left", true) => MOUSEEVENTF_LEFTDOWN,
            ("left", false) => MOUSEEVENTF_LEFTUP,
            ("right", true) => MOUSEEVENTF_RIGHTDOWN,
            ("right", false) => MOUSEEVENTF_RIGHTUP,
            ("middle", true) => MOUSEEVENTF_MIDDLEDOWN,
            ("middle", false) => MOUSEEVENTF_MIDDLEUP,
            _ => return None,
        })
    }

    fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn send(inputs: &[INPUT]) -> bool {
        if inputs.is_empty() {
            return true;
        }
        unsafe { SendInput(inputs, size_of::<INPUT>() as i32) == inputs.len() as u32 }
    }

    fn click(command: &Value) -> Value {
        let (x, y) = match point(command) {
            Ok(point) => point,
            Err(error) => return error,
        };
        let button = command
            .get("button")
            .and_then(Value::as_str)
            .unwrap_or("left");
        let clicks = command
            .get("clicks")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 3);
        let Some(down) = mouse_flags(button, true) else {
            return error_result("invalid_input", "button must be left, right or middle");
        };
        let Some(up) = mouse_flags(button, false) else {
            return error_result("invalid_input", "button must be left, right or middle");
        };
        if unsafe { SetCursorPos(x, y) }.is_err() {
            return error_result("input_failed", "SetCursorPos failed");
        }
        let inputs = [
            mouse_input(down, 0),
            mouse_input(up, 0),
            mouse_input(down, 0),
            mouse_input(up, 0),
            mouse_input(down, 0),
            mouse_input(up, 0),
        ];
        let count = (clicks as usize) * 2;
        if !send(&inputs[..count]) {
            return error_result("input_failed", "SendInput did not accept the click");
        }
        cursor_json()
    }

    fn scroll(command: &Value) -> Value {
        if let (Some(x), Some(y)) = (
            command.get("x").and_then(Value::as_f64),
            command.get("y").and_then(Value::as_f64),
        ) {
            let _ = unsafe { SetCursorPos(x.round() as i32, y.round() as i32) };
        }
        let delta_x = command.get("deltaX").and_then(Value::as_f64).unwrap_or(0.0);
        let delta_y = command.get("deltaY").and_then(Value::as_f64).unwrap_or(0.0);
        let mut inputs = Vec::new();
        if delta_y != 0.0 {
            // Positive deltaY scrolls down, matching the browser/CUA convention.
            inputs.push(mouse_input(
                MOUSEEVENTF_WHEEL,
                -(delta_y * 120.0).round() as i32,
            ));
        }
        if delta_x != 0.0 {
            inputs.push(mouse_input(
                MOUSEEVENTF_HWHEEL,
                (delta_x * 120.0).round() as i32,
            ));
        }
        if !send(&inputs) {
            return error_result("input_failed", "SendInput did not accept the wheel event");
        }
        cursor_json()
    }

    fn key_input(key: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn unicode_input(unit: u16) -> [INPUT; 2] {
        [
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
        ]
    }

    fn type_text(command: &Value) -> Value {
        let Some(text) = command.get("text").and_then(Value::as_str) else {
            return error_result("invalid_input", "computer.type requires `text`");
        };
        if text.is_empty() {
            return error_result("invalid_input", "computer.type `text` must not be empty");
        }
        let mut inputs = Vec::new();
        for unit in text.encode_utf16() {
            inputs.extend_from_slice(&unicode_input(unit));
        }
        if !send(&inputs) {
            return error_result("input_failed", "SendInput did not accept the text");
        }
        ok(json!({ "typed": text.chars().count() }))
    }

    fn press_key(command: &Value) -> Value {
        let Some(value) = command.get("key").and_then(Value::as_str) else {
            return error_result("invalid_input", "computer.press requires `key`");
        };
        let mut parts: Vec<&str> = value
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        let Some(key_name) = parts.pop() else {
            return error_result("invalid_input", "computer.press requires a key");
        };
        let mut modifiers = Vec::new();
        for modifier in parts {
            let virtual_key = match modifier {
                "Alt" => VIRTUAL_KEY(0x12),
                "Control" | "ControlOrMeta" => VIRTUAL_KEY(0x11),
                "Meta" => VIRTUAL_KEY(0x5B),
                "Shift" => VIRTUAL_KEY(0x10),
                other => {
                    return error_result(
                        "invalid_input",
                        &format!("unsupported press modifier: {other}"),
                    )
                }
            };
            modifiers.push(virtual_key);
        }
        let Some((_code, virtual_key, _key)) = crate::browser_control_scripts::key_fields(key_name)
        else {
            return error_result("invalid_input", &format!("unsupported key: {key_name}"));
        };
        let mut inputs = Vec::new();
        for modifier in &modifiers {
            inputs.push(key_input(*modifier, KEYBD_EVENT_FLAGS(0)));
        }
        inputs.push(key_input(
            VIRTUAL_KEY(virtual_key as u16),
            KEYBD_EVENT_FLAGS(0),
        ));
        inputs.push(key_input(VIRTUAL_KEY(virtual_key as u16), KEYEVENTF_KEYUP));
        for modifier in modifiers.iter().rev() {
            inputs.push(key_input(*modifier, KEYEVENTF_KEYUP));
        }
        if !send(&inputs) {
            return error_result("input_failed", "SendInput did not accept the key press");
        }
        ok(json!({ "key": value }))
    }

    fn drag(command: &Value) -> Value {
        let from = (command.get("fromX"), command.get("fromY"));
        let to = (command.get("toX"), command.get("toY"));
        let (
            Some(Value::Number(from_x)),
            Some(Value::Number(from_y)),
            Some(Value::Number(to_x)),
            Some(Value::Number(to_y)),
        ) = (from.0, from.1, to.0, to.1)
        else {
            return error_result(
                "invalid_input",
                "computer.drag requires fromX/fromY/toX/toY",
            );
        };
        let (from_x, from_y, to_x, to_y) = (
            from_x.as_f64().unwrap_or(0.0),
            from_y.as_f64().unwrap_or(0.0),
            to_x.as_f64().unwrap_or(0.0),
            to_y.as_f64().unwrap_or(0.0),
        );
        if !(from_x.is_finite() && from_y.is_finite() && to_x.is_finite() && to_y.is_finite()) {
            return error_result("invalid_input", "computer.drag coordinates must be finite");
        }
        if unsafe { SetCursorPos(from_x.round() as i32, from_y.round() as i32) }.is_err() {
            return error_result("input_failed", "SetCursorPos failed");
        }
        let mut inputs = vec![mouse_input(MOUSEEVENTF_LEFTDOWN, 0)];
        let steps = 20;
        for step in 1..=steps {
            let progress = f64::from(step) / f64::from(steps);
            let x = from_x + (to_x - from_x) * progress;
            let y = from_y + (to_y - from_y) * progress;
            let _ = unsafe { SetCursorPos(x.round() as i32, y.round() as i32) };
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        inputs.push(mouse_input(MOUSEEVENTF_LEFTUP, 0));
        if !send(&inputs) {
            return error_result("input_failed", "SendInput did not accept the drag");
        }
        cursor_json()
    }

    fn screenshot(command: &Value) -> Value {
        let windows = windows_list();
        let (x, y, width, height) =
            if command.get("windowId").is_some() || command.get("pid").is_some() {
                let Some(window) = find_window(&windows, command) else {
                    return error_result("window_not_found", "no matching window is visible");
                };
                (
                    window.bounds.0,
                    window.bounds.1,
                    window.bounds.2,
                    window.bounds.3,
                )
            } else if let (Some(x), Some(y), Some(width), Some(height)) = (
                command.get("x").and_then(Value::as_f64),
                command.get("y").and_then(Value::as_f64),
                command.get("width").and_then(Value::as_f64),
                command.get("height").and_then(Value::as_f64),
            ) {
                (
                    x.round() as i32,
                    y.round() as i32,
                    width.round() as i32,
                    height.round() as i32,
                )
            } else {
                unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN),
                    )
                }
            };
        if width <= 0 || height <= 0 {
            return error_result("invalid_input", "capture region must be non-empty");
        }
        if u64::from(width as u32) * u64::from(height as u32) > MAX_CAPTURE_PIXELS {
            return error_result("invalid_input", "capture region is too large");
        }
        match capture_png(x, y, width, height) {
            Ok((png, width, height)) => {
                let max_bytes = command
                    .get("maxBytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(8 * 1024 * 1024) as usize;
                if png.len() > max_bytes {
                    return error_result(
                        "image_too_large",
                        &format!("capture is {} bytes (limit {max_bytes})", png.len()),
                    );
                }
                ok(json!({
                    "image": {
                        "base64": base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &png,
                        ),
                        "mimeType": "image/png",
                        "width": width,
                        "height": height,
                        "bytes": png.len(),
                    },
                    "region": { "x": x, "y": y, "width": width, "height": height },
                }))
            }
            Err(message) => error_result("capture_failed", &message),
        }
    }

    fn capture_png(x: i32, y: i32, width: i32, height: i32) -> Result<(Vec<u8>, u32, u32), String> {
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return Err("GetDC returned an invalid handle".to_string());
            }
            let memory = CreateCompatibleDC(Some(screen));
            if memory.is_invalid() {
                ReleaseDC(None, screen);
                return Err("CreateCompatibleDC failed".to_string());
            }
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            if bitmap.is_invalid() {
                let _ = DeleteDC(memory);
                ReleaseDC(None, screen);
                return Err("CreateCompatibleBitmap failed".to_string());
            }
            let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
            let blit = BitBlt(
                memory,
                0,
                0,
                width,
                height,
                Some(screen),
                x,
                y,
                SRCCOPY | CAPTUREBLT,
            );
            if blit.is_err() {
                let _ = SelectObject(memory, previous);
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(memory);
                ReleaseDC(None, screen);
                return Err("BitBlt failed".to_string());
            }
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..zeroed()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
            let lines = GetDIBits(
                memory,
                bitmap,
                0,
                height as u32,
                Some(pixels.as_mut_ptr() as *mut c_void),
                &mut info,
                DIB_RGB_COLORS,
            );
            let _ = SelectObject(memory, previous);
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
            if lines == 0 {
                return Err("GetDIBits failed".to_string());
            }
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            let image = image::RgbaImage::from_raw(width as u32, height as u32, pixels)
                .ok_or_else(|| "RGBA buffer size mismatch".to_string())?;
            let mut png = Vec::new();
            image
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .map_err(|error| error.to_string())?;
            Ok((png, width as u32, height as u32))
        }
    }
}
