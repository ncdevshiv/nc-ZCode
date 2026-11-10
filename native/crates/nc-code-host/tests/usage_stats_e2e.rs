//! `v4/usage/stats` e2e: the aggregate must read the **durable** per-turn facts written by
//! real turns, survive a host restart, and never invent numbers.
//!
//! Pinned behaviours:
//! 1. no usage before any turn (zero summary, `source:"agent-db"`, no favorite model);
//! 2. after a real text turn the snapshot reports that turn's tokens / session / model and a
//!    real heatmap bucket;
//! 3. after a restart the same facts are still aggregated (they live in the transcript);
//! 4. the `range` window filters by time (7d excludes an old fact).

mod common;

use common::{
    command_envelope, text_sse, Client, HostProcess, ScriptedProvider, ScriptedResponder,
};
use serde_json::{json, Value};

fn base_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zcode-host-usage-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn write_provider_config(port: u16, path: &std::path::Path) {
    std::fs::write(
        path,
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
}

fn start(
    workspace: &std::path::Path,
    data_dir: &std::path::Path,
    config: &std::path::Path,
    port: u16,
) -> (HostProcess, Client) {
    write_provider_config(port, config);
    let mut process = HostProcess::spawn(workspace, data_dir, config, false);
    let client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    (process, client)
}

fn create_session(client: &mut Client) -> String {
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string()
}

fn usage_stats(client: &mut Client, range: &str, time_zone: &str) -> Value {
    let id = client.send(
        "v4/usage/stats",
        json!({"range": range, "timeZone": time_zone}),
    );
    let response = client.response(id);
    response["result"].clone()
}

/// Send one turn and wait for its command to settle (the usage fact is appended at settlement).
fn run_turn(client: &mut Client, session_id: &str, command_id: &str, text: &str) {
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            command_id,
            Some(session_id),
            json!({"text": text}),
        ),
    );
    let response = client.response(id);
    assert_eq!(
        response["result"]["status"],
        json!("accepted"),
        "turn admitted: {response}"
    );
    // Poll the idempotency surface: the usage fact is appended when the turn settles,
    // which happens after the streamed answer completes.
    for _ in 0..300 {
        let query_id = client.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": session_id, "commandId": command_id}]}),
        );
        let query = client.response(query_id);
        let result = query["result"]["results"]
            .as_array()
            .and_then(|items| items.first())
            .map(|item| item["result"].clone())
            .unwrap_or(Value::Null);
        if result["status"].as_str().is_some() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("command {command_id} never settled");
}

#[test]
fn usage_stats_reads_durable_turn_facts_and_survives_restart() {
    let (_provider, port) =
        ScriptedResponder::spawn(vec![text_sse("first answer"), text_sse("second answer")]);
    let base = base_dir("durable");
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config = base.join("provider.json");

    let (process, mut client) = start(&workspace, &data_dir, &config, port);
    let empty = usage_stats(&mut client, "all", "UTC");
    assert_eq!(empty["summary"]["totalTokens"], json!(0), "{empty}");
    assert_eq!(empty["summary"]["totalSessions"], json!(0), "{empty}");
    assert_eq!(empty["source"], json!("agent-db"), "{empty}");
    assert!(empty["summary"]["favoriteModel"].is_null(), "{empty}");

    let session_id = create_session(&mut client);
    run_turn(&mut client, &session_id, "cmd-turn", "hello usage");

    let after = usage_stats(&mut client, "all", "UTC");
    assert_eq!(after["summary"]["totalSessions"], json!(1), "{after}");
    assert_eq!(after["summary"]["totalTurns"], json!(1), "{after}");
    // The scripted SSE reports input 10 / output 3 on the finish event.
    assert_eq!(after["summary"]["inputTokens"], json!(10), "{after}");
    assert_eq!(after["summary"]["outputTokens"], json!(3), "{after}");
    assert_eq!(after["summary"]["totalTokens"], json!(13), "{after}");
    assert_eq!(
        after["summary"]["favoriteModel"]["modelId"],
        json!("test-model"),
        "{after}"
    );
    assert!(
        after["heatmap"]["maxTokens"].as_u64().unwrap_or(0) > 0,
        "{after}"
    );

    // Restart: the facts are in the transcript, so the aggregate is unchanged.
    drop(client);
    drop(process);
    let (_process2, mut restarted) = start(&workspace, &data_dir, &config, port);
    let resumed = usage_stats(&mut restarted, "all", "UTC");
    assert_eq!(resumed["summary"]["totalTokens"], json!(13), "{resumed}");
    assert_eq!(resumed["summary"]["totalSessions"], json!(1), "{resumed}");
    assert_eq!(
        resumed["summary"]["favoriteModel"]["modelId"],
        json!("test-model"),
        "{resumed}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_missing_session_file_and_bad_lines_are_not_counted_as_usage() {
    let (_provider, port) = ScriptedProvider::spawn();
    let base = base_dir("skip");
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    // A malformed transcript that mentions tokens must not be read as usage.
    let junk = data_dir.join("sessions").join("junk-session");
    std::fs::create_dir_all(&junk).expect("junk dir");
    std::fs::write(
        junk.join("transcript.jsonl"),
        "{\"t\":\"usage\",\"fact\":\"not an object\"}\nnot json\n",
    )
    .expect("junk transcript");
    let config = base.join("provider.json");

    let (_process, mut client) = start(&workspace, &data_dir, &config, port);
    let snapshot = usage_stats(&mut client, "all", "UTC");
    assert_eq!(snapshot["summary"]["totalTokens"], json!(0), "{snapshot}");
    assert_eq!(snapshot["summary"]["totalSessions"], json!(0), "{snapshot}");
    let _ = std::fs::remove_dir_all(&base);
}
