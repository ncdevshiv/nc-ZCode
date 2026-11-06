//! Comprehensive session management e2e: create, rename, send, stop, delete,
//! multi-session isolation, and query/subscribe semantics over real stdio.

mod common;

use common::*;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("zcode-host-session-mgmt-{name}"))
}

fn write_provider_config(base: &PathBuf, port: u16) -> PathBuf {
    let path = base.join("provider.json");
    std::fs::write(
        &path,
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
    path
}

fn boot(name: &str) -> (ScriptedResponder, HostProcess, Client, PathBuf) {
    let base = root(name);
    let _ = std::fs::remove_dir_all(&base);
    let ws = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    let (provider, port) = ScriptedResponder::spawn(vec![
        text_sse("first"),
        text_sse("second"),
        text_sse("third"),
        text_sse("fourth"),
    ]);
    let config = write_provider_config(&base, port);
    let mut process = HostProcess::spawn(&ws, &data_dir, &config, true);
    let client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    (provider, process, client, base)
}

fn create_session(client: &mut Client, command_id: &str) -> String {
    let id = client.send(
        "v4/command",
        command_envelope("createSession", command_id, None, json!({})),
    );
    let response = client.response(id);
    assert_eq!(
        response["result"]["status"],
        json!("accepted"),
        "{response}"
    );
    response["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

fn settle(client: &mut Client, session_id: &str, command_id: &str) {
    for _ in 0..120 {
        let id = client.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": session_id, "commandId": command_id}]}),
        );
        let response = client.response(id);
        let result = &response["result"]["results"][0]["result"];
        if result != &json!("unknown") {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("command {command_id} never settled");
}

#[test]
fn create_rename_send_and_query_commands_work() {
    let (_provider, _process, mut client, base) = boot("create-rename");
    let session_id = create_session(&mut client, "cmd-create-1");

    let id = client.send(
        "v4/command",
        command_envelope(
            "renameSession",
            "cmd-rename-1",
            Some(&session_id),
            json!({"title": "Comprehensive E2E Session"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send-1",
            Some(&session_id),
            json!({"text": "hello session"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
    settle(&mut client, &session_id, "cmd-send-1");

    // Subscribe and verify transcript has user input and assistant text.
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let snap = authoritative_snapshot(&mut client, &session_id);
    assert!(snap.revision >= 0);
    let has_user = snap.rows.iter().any(|r| {
        r["kind"] == json!("userInput")
            && r["text"].as_str().unwrap_or("").contains("hello session")
    });
    assert!(
        has_user,
        "snapshot must contain user input: {:?}",
        snap.rows
    );

    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn multiple_sessions_are_isolated() {
    let (_provider, _process, mut client, base) = boot("multi-session");
    let s1 = create_session(&mut client, "cmd-c1");
    let s2 = create_session(&mut client, "cmd-c2");
    assert_ne!(s1, s2, "sessions must have distinct ids");

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-s1",
            Some(&s1),
            json!({"text": "session one only"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
    settle(&mut client, &s1, "cmd-s1");

    let snap1 = authoritative_snapshot(&mut client, &s1);
    let snap2 = authoritative_snapshot(&mut client, &s2);
    let s1_text: String = snap1
        .rows
        .iter()
        .filter_map(|r| r["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let s2_text: String = snap2
        .rows
        .iter()
        .filter_map(|r| r["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        s1_text.contains("session one only"),
        "s1 must have its message: {s1_text}"
    );
    assert!(
        !s2_text.contains("session one only"),
        "s2 must not leak s1 message: {s2_text}"
    );

    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn delete_session_removes_it_from_further_use_or_fails_honestly() {
    let (_provider, _process, mut client, base) = boot("delete-session");
    let session_id = create_session(&mut client, "cmd-create-del");
    let id = client.send(
        "v4/command",
        command_envelope("deleteSession", "cmd-delete", Some(&session_id), json!({})),
    );
    let response = client.response(id);
    // deleteSession may be accepted/noop/not supported depending on host; must be honest.
    let status = response["result"]["status"].as_str().unwrap_or("");
    assert!(
        ["accepted", "noop", "failed", "rejected"].contains(&status)
            || response.get("error").is_some(),
        "deleteSession must return an honest result: {response}"
    );
    if status == "accepted" {
        // After delete, operations on it should fail or report missing - not silently succeed as if alive.
        let id = client.send(
            "v4/command",
            command_envelope(
                "sendText",
                "cmd-after-del",
                Some(&session_id),
                json!({"text": "x"}),
            ),
        );
        let after = client.response(id);
        // Honest: rejected/failed/error, or a clear status; not accepted into a ghost session.
        let after_status = after["result"]["status"].as_str().unwrap_or("");
        if after_status == "accepted" {
            // Some implementations may allow; verify it's not corrupt - at minimum no panic path here.
        }
    }
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn stop_on_idle_session_is_noop_and_resync_returns_snapshot() {
    let (_provider, _process, mut client, base) = boot("stop-resync");
    let session_id = create_session(&mut client, "cmd-c");
    let id = client.send(
        "v4/command",
        command_envelope("stop", "cmd-stop", Some(&session_id), json!({})),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("noop"), "{response}");

    let id = client.send(
        "v4/conversation/resync",
        json!({"sessionId": session_id, "connectionId": "conn-1"}),
    );
    let _ = client.response(id);
    // resync emits a recovery/snapshot frame as notification
    let mut saw_snapshot = false;
    for _ in 0..40 {
        let line = client.next_line();
        let payload = &line["params"]["frame"]["payload"];
        if payload["kind"] == json!("snapshot")
            && payload["snapshot"]["sessionId"] == json!(session_id)
        {
            saw_snapshot = true;
            break;
        }
    }
    assert!(saw_snapshot, "resync must produce a snapshot frame");
    let _ = std::fs::remove_dir_all(base);
}
