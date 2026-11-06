//! `outputPreview` during tool execution e2e: real host + real shell + real subprocesses.
//!
//! Pins the two boundaries of TS's `projectToolActivity`:
//! 1. a running Bash row receives a bounded tail-read preview after 2 s (`text/fullText/totalLines/totalBytes/
//!    linesEstimated`), so the client can see half-finished output without polling the background read model;
//! 2. after the terminal row is rebuilt whole, the preview disappears (TS `clearSettledOutputPreviews`); a terminal row must not keep
//!    carrying a preview that says "still running".
//!
//! The same harness as `bash_cancellation_e2e`: the host subprocess goes over stdio, and the provider double lives only
//! in the test process.

mod common;

use common::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[test]
fn a_running_bash_row_gets_an_output_preview_and_the_terminal_row_drops_it() {
    preview_roundtrip("desktop-continuous");
}

/// The same preview channel must also hold on the replayable profile (row upsert does not filter by profile; the journal records the very same
/// delta, so a reconnect replay never sees the live row in some other shape).
#[test]
fn the_same_preview_reaches_a_replayable_client() {
    preview_roundtrip("web-remote-replayable");
}

fn preview_roundtrip(client_mode: &str) {
    let base = std::env::temp_dir().join(format!(
        "zcode-host-tool-progress-{}-{client_mode}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let provider_config = base.join("provider.json");
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse_with_id(
            "toolu-progress",
            "Bash",
            &json!({
                "command": "node -e \"console.log('preview-live'); setTimeout(() => console.log('done'), 3500)\"",
                "description": "progress probe"
            }),
        ),
        text_sse("command finished"),
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
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": client_mode}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send",
            Some(&session_id),
            json!({"text": "run the progress probe"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let mut rows: Vec<Value> = Vec::new();
    let mut preview: Option<Value> = None;
    let mut terminal: Option<Value> = None;
    let mut answered = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let line = client.next_line();
        apply_to_rows(&mut rows, &deltas_from_frame(&line));
        if let Some(row) = rows
            .iter()
            .find(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!("Bash"))
        {
            if row["status"] == json!("running") {
                if let Some(candidate) = row.get("outputPreview") {
                    if candidate["totalBytes"].as_u64().unwrap_or(0) > 0 {
                        preview = Some(candidate.clone());
                    }
                }
            } else if row["status"] != json!("pendingApproval") {
                terminal = Some(row.clone());
            }
        }
        answered |= rows.iter().any(|row| {
            row["kind"] == json!("assistantText")
                && row["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("command finished")
        });
        if terminal.is_some() && answered {
            break;
        }
    }

    let preview = preview.unwrap_or_else(|| {
        panic!("no running Bash row ever carried a non-empty outputPreview: {rows:?}")
    });
    assert!(
        preview["text"]
            .as_str()
            .is_some_and(|text| text.contains("preview-live")),
        "the short preview must show real output: {preview}"
    );
    assert!(
        preview["fullText"]
            .as_str()
            .is_some_and(|text| text.contains("preview-live")),
        "fullText carries the bounded tail: {preview}"
    );
    assert!(
        preview["totalLines"].as_u64().unwrap_or(0) >= 1,
        "line count is a fact, not zero: {preview}"
    );
    assert_eq!(
        preview["linesEstimated"],
        json!(false),
        "the whole file fit in the tail read: {preview}"
    );

    let terminal = terminal.unwrap_or_else(|| panic!("Bash row never settled: {rows:?}"));
    assert_eq!(terminal["status"], json!("success"), "{terminal}");
    assert!(
        terminal.get("outputPreview").is_none(),
        "a settled row must not keep a live preview: {terminal}"
    );
    assert!(
        terminal["output"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("done")),
        "the terminal row keeps the full result: {terminal}"
    );

    let bodies = provider.request_bodies();
    assert!(
        bodies.len() >= 2,
        "the tool round and the final answer must both reach the provider"
    );
    drop(provider);
    let _ = std::fs::remove_dir_all(&base);
}
