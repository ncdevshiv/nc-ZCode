//! Real pseudoterminal end-to-end tests for the `terminal` channel.
//!
//! These spawn an actual shell through the OS PTY (ConPTY on Windows, `forkpty` on
//! macOS/Linux) — the same shell the TypeScript service picks — and pin the facts the
//! frontend depends on: output really arrives as a string event, `write` reaches the child,
//! `resize` reaches the terminal size, the exit event fires exactly once with the real
//! status, and `dispose` kills and reaps the child.

use std::sync::Arc;
use std::time::Duration;

use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, ClientMode, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::AppPaths;
use nc_code_services_terminal::TerminalService;
use serde_json::{json, Value};
use tokio::time::{timeout, Instant};

/// A shell has to print its prompt within this budget or the test fails loudly.
const EVENT_WAIT: Duration = Duration::from_secs(20);
/// Short quiet period used to let the start-up banner land before typing.
const WARM_UP: Duration = Duration::from_millis(1500);

fn context() -> ChannelContext {
    ChannelContext::new("pty-e2e", ClientMode::DesktopContinuous)
}

fn service() -> Arc<TerminalService> {
    TerminalService::new(AppPaths::new(std::env::temp_dir(), std::env::temp_dir()))
}

async fn call(
    service: &Arc<TerminalService>,
    command: &str,
    params: Value,
) -> Result<WireValue, ChannelFailure> {
    service
        .call(
            &context(),
            command,
            vec![WireValue::Json(params)],
            CancelToken::new(),
        )
        .await
}

async fn create(service: &Arc<TerminalService>, cols: u32, rows: u32, cwd: Option<&str>) -> Value {
    let mut params = json!({ "cols": cols, "rows": rows });
    if let Some(cwd) = cwd {
        params["cwd"] = Value::String(cwd.to_string());
    }
    let created = call(service, "create", params)
        .await
        .expect("create must succeed with a real shell");
    match created {
        WireValue::Json(value) => value,
        other => panic!("create must return an object, got {other:?}"),
    }
}

async fn listen(service: &Arc<TerminalService>, event: &str, id: &str) -> EventSubscription {
    service
        .listen(&context(), event, WireValue::String(id.to_string()))
        .await
        .unwrap_or_else(|error| panic!("{event} subscription must succeed: {error:?}"))
}

/// Reads events until the deadline, returning everything that arrived.
async fn collect(subscription: &mut EventSubscription, budget: Duration) -> String {
    let stop = Instant::now() + budget;
    let mut accumulated = String::new();
    loop {
        let remaining = stop.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return accumulated;
        }
        match timeout(remaining, subscription.next()).await {
            Ok(Some(WireValue::String(text))) => accumulated.push_str(&text),
            Ok(Some(other)) => panic!("onDynamicData must deliver a string, got {other:?}"),
            Ok(None) => return accumulated,
            Err(_) => return accumulated,
        }
    }
}

/// Reads events until `predicate` accepts the accumulated output.
async fn wait_for(
    subscription: &mut EventSubscription,
    budget: Duration,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let stop = Instant::now() + budget;
    let mut accumulated = String::new();
    loop {
        if predicate(&accumulated) {
            return accumulated;
        }
        let remaining = stop.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return accumulated;
        }
        match timeout(remaining, subscription.next()).await {
            Ok(Some(WireValue::String(text))) => accumulated.push_str(&text),
            Ok(Some(other)) => panic!("onDynamicData must deliver a string, got {other:?}"),
            Ok(None) => return accumulated,
            Err(_) => return accumulated,
        }
    }
}

/// ConPTY asks the terminal for a cursor-position report (`ESC[6n`) right after the
/// console starts, and the shell blocks until it gets the answer. A real client is xterm.js,
/// which replies automatically; these tests are the terminal client, so they must reply too —
/// otherwise every keystroke is typed into a shell that has not finished starting.
const CURSOR_POSITION_QUERY: &str = "\u{1b}[6n";
const CURSOR_POSITION_REPORT: &str = "\u{1b}[1;1R";

fn answer_cursor_position(
    service: &Arc<TerminalService>,
    id: &str,
    accumulated: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    if !accumulated.contains(CURSOR_POSITION_QUERY) {
        return None;
    }
    let service = Arc::clone(service);
    let params = json!({ "id": id, "data": CURSOR_POSITION_REPORT });
    Some(tokio::spawn(async move {
        let _ = call(&service, "write", params).await;
    }))
}

fn preview(text: &str) -> String {
    text.chars().take(400).collect::<String>()
}

#[tokio::test]
async fn create_streams_real_output_and_reports_the_shell_shape() {
    let service = service();
    let result = create(&service, 100, 30, None).await;

    let id = result["id"].as_str().expect("id").to_string();
    assert_eq!(id, "0", "ids start at 0 like the TypeScript counter");
    let shell = result["shell"].as_str().expect("shell");
    assert!(!shell.trim().is_empty(), "a real shell must be resolved");

    let family = result["fontFamily"].as_str().expect("fontFamily");
    assert!(!family.is_empty());
    let source = result["fontFamilySource"].as_str().expect("source");
    assert!(
        ["custom", "system", "fallback"].contains(&source),
        "unexpected fontFamilySource {source:?}"
    );

    #[cfg(windows)]
    {
        let windows_pty = &result["windowsPty"];
        assert_eq!(windows_pty["backend"], json!("conpty"));
        let build = windows_pty["buildNumber"]
            .as_u64()
            .unwrap_or_else(|| panic!("a real Windows build number, got {windows_pty}"));
        assert!(build > 10000, "implausible build number {build}");
        let lowered = shell.to_ascii_lowercase();
        assert!(
            lowered.ends_with("pwsh.exe")
                || lowered.ends_with("powershell.exe")
                || lowered.ends_with("cmd.exe"),
            "unexpected Windows shell {shell}"
        );
    }
    #[cfg(not(windows))]
    assert!(
        result.get("windowsPty").is_none(),
        "windowsPty is undefined off Windows, so the key must be absent"
    );

    // Real output: an interactive shell prints a prompt without any input from us.
    let mut data = listen(&service, "onDynamicData", &id).await;
    let output = wait_for(&mut data, EVENT_WAIT, |text| !text.is_empty()).await;
    assert!(!output.is_empty(), "the shell produced no output at all");

    call(&service, "dispose", json!({ "id": id }))
        .await
        .expect("dispose");
}

#[tokio::test]
async fn write_reaches_the_shell_resize_reaches_the_pty_and_exit_fires_once() {
    let service = service();
    let result = create(&service, 90, 24, None).await;
    let id = result["id"].as_str().expect("id").to_string();

    let mut data = listen(&service, "onDynamicData", &id).await;
    let mut exit = listen(&service, "onDynamicExit", &id).await;
    let warm_up = collect(&mut data, WARM_UP).await;
    if let Some(handle) = answer_cursor_position(&service, &id, &warm_up) {
        let _ = handle.await;
    }

    // `write` sends keystrokes; the PTY echoes them and the shell runs the command, so the
    // marker appears at least twice (line echo + command output).
    let marker = format!("ZCODEPTY{}", std::process::id());
    call(
        &service,
        "write",
        json!({ "id": &id, "data": format!("echo {marker}\r") }),
    )
    .await
    .expect("write");

    let mut output = wait_for(&mut data, EVENT_WAIT, |text| {
        text.matches(&marker).count() >= 2
    })
    .await;
    // A fresh console can re-query the cursor position after the first output.
    if let Some(handle) = answer_cursor_position(&service, &id, &output) {
        let _ = handle.await;
        output = wait_for(&mut data, EVENT_WAIT, |text| {
            text.matches(&marker).count() >= 2
        })
        .await;
    }
    assert!(
        output.matches(&marker).count() >= 2,
        "the shell never echoed/executed the marker; got {:?}",
        preview(&output)
    );

    // `resize` must land on the terminal itself, not just in our bookkeeping.
    call(
        &service,
        "resize",
        json!({ "id": &id, "cols": 132, "rows": 43 }),
    )
    .await
    .expect("resize");
    assert_eq!(
        service.terminal_size(&id),
        Some((132, 43)),
        "the PTY keeps the resized window"
    );

    // A real exit: `exit` ends the shell, so the exit event carries its status once.
    call(&service, "write", json!({ "id": &id, "data": "exit\r" }))
        .await
        .expect("write exit");
    let code = timeout(EVENT_WAIT, exit.next())
        .await
        .expect("the exit event must arrive")
        .expect("the exit payload");
    let WireValue::Int(code) = code else {
        panic!("onDynamicExit must deliver a number (tag 4), got {code:?}");
    };
    assert_eq!(code, 0, "`exit` ends the shell with status 0");

    // Exactly once, then both subscriptions close because the emitters are gone.
    assert_eq!(
        timeout(Duration::from_secs(3), exit.next()).await.ok(),
        Some(None),
        "onDynamicExit must fire exactly once"
    );
    // ConPTY can deliver one last control frame while the console tears down (for example a
    // window-size report); drain it, then the emitter must really be closed.
    let drain_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("the output subscription must close after the child exits");
        }
        match timeout(remaining, data.next()).await {
            Ok(None) => break,
            Ok(Some(WireValue::String(_))) => continue,
            Ok(Some(other)) => panic!("onDynamicData must deliver a string, got {other:?}"),
            Err(_) => panic!("the output subscription must close after the child exits"),
        }
    }
    assert_eq!(
        service.open_count(),
        0,
        "the exited terminal leaves the map"
    );

    let error = call(&service, "write", json!({ "id": &id, "data": "x" }))
        .await
        .expect_err("the terminal is gone");
    assert_eq!(
        error.payload().expect("payload").message,
        format!("Terminal not found: {id}")
    );
}

#[tokio::test]
async fn dispose_kills_and_reaps_the_child_without_an_exit_event() {
    let service = service();
    let result = create(&service, 80, 20, None).await;
    let id = result["id"].as_str().expect("id").to_string();
    let reaped_before = service.reaped_count();

    let mut exit = listen(&service, "onDynamicExit", &id).await;
    let mut data = listen(&service, "onDynamicData", &id).await;
    collect(&mut data, WARM_UP).await;

    // Start something long-running inside the shell so `dispose` has real work to do.
    let shell = result["shell"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let command = if cfg!(windows) {
        if shell.contains("powershell") {
            "Start-Sleep -Seconds 120\r"
        } else {
            // cmd.exe has no Start-Sleep; `choice` blocks until input.
            "choice /c X /d 1 /t 120 >nul\r"
        }
    } else {
        "sleep 120\n"
    };
    call(&service, "write", json!({ "id": &id, "data": command }))
        .await
        .expect("write command");
    // Give the shell a moment to actually start the child it is waiting on.
    collect(&mut data, Duration::from_millis(300)).await;

    call(&service, "dispose", json!({ "id": &id }))
        .await
        .expect("dispose");
    assert_eq!(service.open_count(), 0);
    assert_eq!(service.terminal_process_id(&id), None);

    // The kill happened: the child is reaped off the OS thread, not left as a zombie.
    let deadline = Instant::now() + EVENT_WAIT;
    while service.reaped_count() == reaped_before && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        service.reaped_count(),
        reaped_before + 1,
        "dispose must terminate AND reap the child"
    );

    // TypeScript disposes the emitters inside `cleanupTerminal`, so a killed terminal
    // reports no exit code; the subscription just closes.
    assert_eq!(
        timeout(EVENT_WAIT, exit.next()).await.ok(),
        Some(None),
        "dispose must not deliver an exit event"
    );

    // Disposing an already removed terminal is a no-op, like the TypeScript early return.
    call(&service, "dispose", json!({ "id": &id }))
        .await
        .expect("second dispose");
}

#[tokio::test]
async fn dispose_all_terminates_every_live_terminal() {
    let service = service();
    let reaped_before = service.reaped_count();
    let first = create(&service, 80, 20, None).await;
    let second = create(&service, 80, 20, None).await;
    assert_eq!(first["id"], json!("0"));
    assert_eq!(second["id"], json!("1"));
    assert_eq!(service.open_count(), 2);

    let mut first_data = listen(&service, "onDynamicData", "0").await;
    let mut second_exit = listen(&service, "onDynamicExit", "1").await;

    call(&service, "disposeAll", json!({}))
        .await
        .expect("disposeAll");
    assert_eq!(
        service.open_count(),
        0,
        "no terminal may outlive disposeAll"
    );

    let deadline = Instant::now() + EVENT_WAIT;
    while service.reaped_count() < reaped_before + 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        service.reaped_count() >= reaped_before + 2,
        "both children must be reaped, got {}",
        service.reaped_count()
    );
    assert_eq!(
        timeout(EVENT_WAIT, first_data.next()).await.ok(),
        Some(None),
        "output subscriptions close with the terminal"
    );
    assert_eq!(
        timeout(EVENT_WAIT, second_exit.next()).await.ok(),
        Some(None),
        "killed terminals report no exit code"
    );
}

#[tokio::test]
async fn create_falls_back_when_the_requested_directory_is_gone() {
    let service = service();
    let stale = std::env::temp_dir().join("zcode-terminal-missing-dir-e2e");
    let result = create(&service, 80, 24, stale.to_str()).await;
    let id = result["id"].as_str().expect("id").to_string();
    // The terminal must still be able to get up (return to HOME) when the directory is invalid, and the output must be truly usable.
    let mut data = listen(&service, "onDynamicData", &id).await;
    let output = wait_for(&mut data, EVENT_WAIT, |text| !text.is_empty()).await;
    assert!(
        !output.is_empty(),
        "a stale cwd must not break terminal startup"
    );
    call(&service, "dispose", json!({ "id": id }))
        .await
        .expect("dispose");
}
