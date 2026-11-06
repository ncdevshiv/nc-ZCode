//! Cancelling a command that is running: `stop` must really kill the process and tell the model that it "was cancelled".
//!
//! Pins the two real defects ENTRY 135 fixed:
//! 1. the watchdog only watched the deadline and never read the turn cancellation flag — after the user pressed stop, the command still ran to natural completion
//!    or ran out the default 120 s;
//! 2. timeout/cancellation used to surface only as an `exit code`, so the model could not distinguish "the command crashed by itself" from "we killed it",
//!    and therefore reran the same command instead of raising the timeout or moving it to the background.
//!
//! The same harness as `tool_matrix_e2e`: a real host process + a real shell + real subprocesses,
//! with the provider double living only in the test process.

mod common;

use common::*;
use serde_json::{json, Value};
use std::time::Instant;

#[test]
fn stop_kills_a_running_command_and_the_model_is_told_why() {
    let base = std::env::temp_dir().join(format!("zcode-host-bash-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let provider_config = base.join("provider.json");
    // Two model responses: first it asks to run a command far longer than the test budget, then the closing text after cancellation.
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse(
            "Bash",
            &json!({"command": "ping -n 30 127.0.0.1", "timeout_ms": 60000}),
        ),
        text_sse("stopped early"),
    ]);
    std::fs::write(
        &provider_config,
        json!({
            "kind": "anthropic",
            "baseUrl": format!("http://127.0.0.1:{port}"),
            "apiKey": "test-key-not-a-real-credential",
            "model": "test-model",
            "maxTokens": 1024,
        })
        .to_string(),
    )
    .expect("provider config");

    let mut process = HostProcess::spawn(&workspace, &data_dir, &provider_config, true);
    let mut client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };

    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send",
            Some(&session_id),
            json!({"text": "ping for a long time"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let mut rows: Vec<Value> = Vec::new();
    let mut sent_stop = false;
    let mut terminal: Option<Value> = None;
    let started = Instant::now();
    // First wait for the Bash row to appear (the command is already running), then send stop; afterwards wait for it to reach a terminal state.
    for _ in 0..400 {
        let line = client.next_line();
        apply_to_rows(&mut rows, &deltas_from_frame(&line));
        let bash = rows
            .iter()
            .find(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!("Bash"));
        if !sent_stop {
            if let Some(row) = bash {
                let id = client.send(
                    "v4/command",
                    command_envelope("stop", "cmd-stop", Some(&session_id), json!({})),
                );
                let ack = client.response(id);
                assert_eq!(
                    ack["result"]["status"],
                    json!("accepted"),
                    "stop must be accepted while the turn runs: {ack}, row={row}"
                );
                sent_stop = true;
            }
        } else if let Some(row) = bash {
            if row["status"] != json!("running") && row["status"] != json!("pendingApproval") {
                // After stop the turn owes the model no sentence: TS's stop semantics end this very turn,
                // so what is awaited here is the fact that the command got killed, not assistant text.
                terminal = Some(row.clone());
                break;
            }
        }
    }

    let terminal = terminal.expect("the Bash row must reach a terminal state");
    assert!(
        sent_stop,
        "never observed the Bash row, so stop was never exercised: {rows:?}"
    );
    let output = terminal["output"]["text"].as_str().unwrap_or("");
    assert!(
        output.contains("cancelled by the turn"),
        "the model must be told this was a cancellation, not a crash: {terminal}"
    );
    assert!(
        !output.contains("killed after the 60000 ms timeout"),
        "Timeout prompt should not appear on cancel path: {output}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(25),
        "The command must be killed instead of waiting for it to finish: {:?}",
        started.elapsed()
    );
    let bodies = provider.request_bodies();

    assert!(
        !bodies.is_empty(),
        "There must be at least one real model request (provider double cannot be bypassed)"
    );
    drop(provider);
    let _ = std::fs::remove_dir_all(&base);
}
