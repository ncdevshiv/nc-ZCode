//! Comprehensive protocol flow e2e: v4 commands/queries, conversation subscribe modes,
//! error handling, and method dispatch over real stdio.

mod common;

use common::*;
use serde_json::json;
use std::path::{Path, PathBuf};

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("zcode-host-protocol-comp-{name}"))
}

fn write_provider_config(base: &Path, port: u16) -> PathBuf {
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

fn boot(name: &str, script: Vec<String>) -> (HostProcess, Client, PathBuf) {
    let base = root(name);
    let _ = std::fs::remove_dir_all(&base);
    let ws = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    let (provider, port) = ScriptedResponder::spawn(script);
    let config = write_provider_config(&base, port);
    let mut process = HostProcess::spawn(&ws, &data_dir, &config, true);
    let client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    std::mem::forget(provider);
    (process, client, base)
}

#[test]
fn unknown_method_returns_jsonrpc_method_not_found() {
    let (_process, mut client, base) = boot("unknown-method", vec![]);
    let id = client.send("v4/nonexistent/method", json!({"x": 1}));
    let response = client.response(id);
    assert_eq!(response["error"]["code"], json!(-32601), "{response}");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn subscribe_replayable_mode_returns_snapshot() {
    let (_process, mut client, base) = boot("replayable", vec![]);
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-c", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-r", "clientMode": "web-remote-replayable"}),
    );
    let response = client.response(id);
    assert_eq!(
        response["result"]["ack"]["mode"],
        json!("snapshot"),
        "{response}"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn commands_query_batch_preserves_order_and_reports_unknown() {
    let (_process, mut client, base) = boot("batch-query", vec![]);
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-known", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let id = client.send(
        "v4/commands/query",
        json!({"commands": [
            {"sessionId": null, "commandId": "cmd-known"},
            {"sessionId": session_id, "commandId": "cmd-missing"},
        ]}),
    );
    let response = client.response(id);
    let results = response["result"]["results"].as_array().expect("results");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["key"]["commandId"], json!("cmd-known"));
    assert_ne!(results[0]["result"], json!("unknown"));
    assert_eq!(results[1]["result"], json!("unknown"));
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn session_subagents_on_fresh_session_returns_empty() {
    let (_process, mut client, base) = boot("subagents-empty", vec![]);
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-c", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let id = client.send(
        "session/subagents",
        json!({"sessionId": session_id, "endedLimit": 10}),
    );
    let response = client.response(id);
    assert!(response["error"].is_null(), "{response}");
    assert_eq!(response["result"]["running"], json!([]));
    assert_eq!(response["result"]["ended"]["total"], json!(0));
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn session_debug_on_fresh_session_returns_empty_snapshot() {
    let (_process, mut client, base) = boot("debug-empty", vec![]);
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-c", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let id = client.send("session/debug", json!({"sessionId": session_id}));
    let response = client.response(id);
    assert!(response["error"].is_null(), "{response}");
    assert_eq!(response["result"]["sessionId"], json!(session_id));
    assert!(response["result"]["rounds"].is_array());
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn v4_command_invalid_payload_returns_protocol_error() {
    let (_process, mut client, base) = boot("invalid-cmd", vec![]);
    // Missing required envelope fields / malformed command
    let id = client.send("v4/command", json!({"type": "sendText"}));
    let response = client.response(id);
    // Should be a JSON-RPC error or rejected result - not a panic/crash.
    assert!(
        response.get("error").is_some()
            || response["result"]["status"] == json!("rejected")
            || response["result"]["status"] == json!("failed"),
        "malformed command must fail honestly: {response}"
    );
    let _ = std::fs::remove_dir_all(base);
}
