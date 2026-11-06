//! nc-code-protocol (non-v4) method e2e: `session/debug`, `session/compact`,
//! `session/goal`, `session/subagents`, `workspace/generateText`.
//!
//! Real binary + scripted provider: what is asserted is **product fact** (real usage counted into the debug ledger,
//! Task-derived child sessions appearing in `session/subagents`, a goal in plan mode only landing in storage and not continuing the run),
//! not "the method exists". All waits carry a timeout.

mod common;

use common::{command_envelope, text_sse, tool_use_sse, Client, HostProcess, ScriptedResponder};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn provider_config(port: u16, path: &Path) {
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

struct Fixture {
    _process: HostProcess,
    client: Client,
    base: PathBuf,
}

fn spawn(script: Vec<String>, full_access: bool, tag: &str) -> Fixture {
    let (responder, port) = ScriptedResponder::spawn(script);
    // The responder is held by the process until the test ends; after the client disconnects, the connection thread exits naturally.
    std::mem::forget(responder);
    boot_at(port, full_access, tag)
}

/// Start the host on the given provider port (the delayed-provider scenario is wired up by the caller itself).
fn boot_at(port: u16, full_access: bool, tag: &str) -> Fixture {
    let base =
        std::env::temp_dir().join(format!("zcode-host-protocol-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base.join("provider.json");
    provider_config(port, &config_path);
    let mut process = HostProcess::spawn(&workspace, data_dir.as_path(), &config_path, full_access);
    let client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    Fixture {
        _process: process,
        client,
        base,
    }
}

impl Fixture {
    fn create_session(&mut self) -> String {
        let id = self.client.send(
            "v4/command",
            command_envelope("createSession", "create-1", None, json!({})),
        );
        let response = self.client.response(id);
        response["result"]["result"]["sessionId"]
            .as_str()
            .expect("createSession result carries sessionId")
            .to_string()
    }

    fn settle(&mut self, session_id: &str, command_id: &str) {
        for _ in 0..200 {
            let id = self.client.send(
                "v4/commands/query",
                json!({"commands": [{"sessionId": session_id, "commandId": command_id}]}),
            );
            let response = self.client.response(id);
            let result = response["result"]["results"][0]["result"].clone();
            if result != json!("unknown") {
                let status = result
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                assert!(
                    matches!(status, "accepted" | "noop" | "duplicate"),
                    "command {command_id} settled as {result}"
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("command {command_id} never settled");
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.client.send(method, params);
        self.client.response(id)
    }

    /// Subscribe (the initial snapshot is subscribe's post-response notification) and return the authoritative revision.
    /// Along the way this makes the client a subscriber of the session — a real UI subscribes before it sends a command.
    fn subscribe_snapshot(&mut self, session_id: &str) -> (i64, Vec<Value>) {
        let id = self.client.send(
            "v4/conversation/subscribe",
            json!({
                "sessionId": session_id,
                "connectionId": "conn-protocol",
                "clientMode": "desktop-continuous",
            }),
        );
        let _ = self.client.response(id);
        for _ in 0..40 {
            let line = self.client.next_line();
            let payload = &line["params"]["frame"]["payload"];
            if payload["kind"] == json!("snapshot")
                && payload["snapshot"]["sessionId"] == json!(session_id)
            {
                return (
                    payload["snapshot"]["revision"].as_i64().unwrap_or(0),
                    payload["snapshot"]["rows"]["window"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                );
            }
        }
        panic!("no snapshot frame after subscribe for {session_id}");
    }
}

#[test]
fn session_debug_reports_rounds_and_network_entries_from_real_usage() {
    let mut fixture = spawn(vec![text_sse("done")], false, "debug");
    let session_id = fixture.create_session();
    let id = fixture.client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-debug-1",
            Some(&session_id),
            json!({"text": "hello"}),
        ),
    );
    let _ = fixture.client.response(id);
    fixture.settle(&session_id, "cmd-debug-1");

    let debug = fixture.call("session/debug", json!({"sessionId": session_id}));
    let snapshot = &debug["result"];
    assert_eq!(snapshot["sessionId"], json!(session_id));
    let rounds = snapshot["rounds"].as_array().expect("rounds array");
    assert_eq!(rounds.len(), 1, "one completed model request");
    assert_eq!(rounds[0]["usage"]["inputTokens"], json!(10));
    assert_eq!(rounds[0]["usage"]["outputTokens"], json!(3));
    let entries = snapshot["networkEntries"]
        .as_array()
        .expect("network entries");
    assert!(
        entries.len() >= 2,
        "started + completed entries: {entries:?}"
    );
    let status_types: Vec<&str> = entries
        .iter()
        .filter_map(|entry| entry["statusType"].as_str())
        .collect();
    assert!(status_types.contains(&"model_request_started"));
    assert!(status_types.contains(&"model_request_completed"));
    assert_eq!(entries[0]["statusCode"], json!(200));
    // With no cached usage the cumulative hit rate must be null (not 0, and not a guessed value).
    assert_eq!(snapshot["cache"]["hitRate"], Value::Null);
    assert_eq!(snapshot["cache"]["totalInputTokens"], json!(10));

    let _ = std::fs::remove_dir_all(&fixture.base);
}

#[test]
fn workspace_generate_text_runs_a_real_model_call_and_cancel_reports_missing_operation() {
    let mut fixture = spawn(vec![text_sse("generated title")], false, "generate");
    let session_id = fixture.create_session();
    // generateText needs no session, but the host needs a provider and a workspace (guaranteeing a real workspace configuration).
    let _ = session_id;
    let response = fixture.call(
        "workspace/generateText",
        json!({
            "workspacePath": fixture.base.join("workspace").to_string_lossy(),
            "workspaceIdentity": Value::Null,
            "selection": {"providerId": "personal:1", "modelId": "test-model"},
            "prompt": "name this workspace",
            "querySource": "workspace_generate_text",
            "maxOutputTokens": 128,
        }),
    );
    let result = &response["result"];
    assert_eq!(result["text"], json!("generated title"));
    assert_eq!(result["selection"]["modelId"], json!("test-model"));
    assert_eq!(result["usage"]["inputTokens"], json!(10));
    assert!(result.get("finishReason").is_some());

    let cancel = fixture.call(
        "workspace/cancelGenerateText",
        json!({"operationId": "never-started"}),
    );
    assert_eq!(cancel["result"]["cancelled"], json!(false));

    let _ = std::fs::remove_dir_all(&fixture.base);
}

#[test]
fn cancel_generate_text_stops_an_in_flight_request() {
    // Delayed provider: cancelling while the request is in flight must really take effect (return a cancellation error, not a late body).
    let (responder, port) = ScriptedResponder::spawn_with_delay(
        vec![text_sse("answer that arrived too late")],
        Duration::from_millis(1_500),
    );
    let mut fixture = boot_at(port, false, "cancel");
    let session_id = fixture.create_session();
    let _ = session_id;
    let id = fixture.client.send(
        "workspace/generateText",
        json!({
            "workspacePath": fixture.base.join("workspace").to_string_lossy(),
            "selection": {"providerId": "personal:1", "modelId": "test-model"},
            "prompt": "write a title",
            "querySource": "workspace_generate_text",
            "operationId": "op-cancel",
        }),
    );
    // Wait until the provider really received the request (otherwise the cancellation targets an operation that had not started yet).
    let mut landed = false;
    for _ in 0..200 {
        if !responder.request_bodies().is_empty() {
            landed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(landed, "the model request must reach the provider");

    let cancel = fixture.call(
        "workspace/cancelGenerateText",
        json!({"operationId": "op-cancel"}),
    );
    assert_eq!(
        cancel["result"]["cancelled"],
        json!(true),
        "Operation in progress must be claimed and canceled: {cancel}"
    );

    let response = fixture.client.response(id);
    let text = response["result"]["text"].as_str().unwrap_or_default();
    assert!(
        response.get("error").is_some() || !text.contains("too late"),
        "Late text may not be used as a result after cancellation: {response}"
    );
    let _ = std::fs::remove_dir_all(&fixture.base);
}

#[test]
fn goal_in_plan_mode_persists_without_continuing_and_clears() {
    let mut fixture = spawn(vec![], false, "goal");
    let session_id = fixture.create_session();
    let (revision, _rows) = fixture.subscribe_snapshot(&session_id);
    let id = fixture.client.send(
        "v4/command",
        json!({
            "type": "switchCollaborationMode",            "commandId": "mode-plan",
            "clientId": "e2e",
            "sessionId": session_id,
            "baseRevision": revision,
            "payload": {"mode": "plan"},
            "issuedAt": 1_700_000_000_000.0,
        }),
    );
    let _ = fixture.client.response(id);
    fixture.settle(&session_id, "mode-plan");

    let show = fixture.call(
        "session/goal",
        json!({"sessionId": session_id, "action": "show"}),
    );
    assert_eq!(
        show["result"]["response"],
        json!("No goal is set. Use /goal <objective> to set one.")
    );
    assert_eq!(show["result"]["startedTurn"], json!(false));

    let set = fixture.call(
        "session/goal",
        json!({
            "sessionId": session_id,
            "action": "set",
            "objective": "ship the port",
        }),
    );
    let response = set["result"]["response"].as_str().expect("response string");
    assert!(response.starts_with("Goal active"), "response: {response}");
    assert!(response.contains("Objective: ship the port"));
    assert!(
        response.contains("Plan mode"),
        "plan mode must explain why no continuation started: {response}"
    );
    assert_eq!(set["result"]["startedTurn"], json!(false));

    let show = fixture.call(
        "session/goal",
        json!({"sessionId": session_id, "action": "show"}),
    );
    let summary = show["result"]["response"].as_str().expect("summary");
    assert!(summary.starts_with("Goal active"), "summary: {summary}");
    assert!(summary.contains("Objective: ship the port"));

    let clear = fixture.call(
        "session/goal",
        json!({"sessionId": session_id, "action": "clear"}),
    );
    assert_eq!(clear["result"]["response"], json!("Goal cleared."));
    let show = fixture.call(
        "session/goal",
        json!({"sessionId": session_id, "action": "show"}),
    );
    assert_eq!(
        show["result"]["response"],
        json!("No goal is set. Use /goal <objective> to set one.")
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

#[test]
fn task_tool_runs_a_child_session_and_lists_it_as_an_ended_subagent() {
    let mut fixture = spawn(
        vec![
            tool_use_sse(
                "Agent",
                &json!({
                    "description": "child work",
                    "prompt": "say hello from the child",
                }),
            ),
            text_sse("hello from child"),
            text_sse("parent done"),
        ],
        true,
        "task",
    );
    let session_id = fixture.create_session();

    let empty = fixture.call(
        "session/subagents",
        json!({"sessionId": session_id, "endedLimit": 20}),
    );
    assert_eq!(empty["result"]["ended"]["total"], json!(0));
    assert_eq!(empty["result"]["running"], json!([]));

    let id = fixture.client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-task-1",
            Some(&session_id),
            json!({"text": "delegate this"}),
        ),
    );
    let _ = fixture.client.response(id);
    fixture.settle(&session_id, "cmd-task-1");

    let listed = fixture.call(
        "session/subagents",
        json!({"sessionId": session_id, "endedLimit": 20}),
    );
    let result = &listed["result"];
    assert_eq!(result["running"], json!([]));
    let ended = &result["ended"];
    assert_eq!(ended["total"], json!(1), "one subagent run: {ended:?}");
    let item = &ended["items"][0];
    assert_eq!(item["status"], json!("success"));
    assert_eq!(item["title"], json!("child work"));
    assert_eq!(item["subagentType"], json!("general-purpose"));
    let child_id = item["childSessionId"].as_str().expect("child session id");
    assert!(item["startedAt"].as_u64().is_some());
    assert!(item["endedAt"].as_u64().is_some());
    assert_eq!(result["childSessionIds"], json!([child_id]));

    // The child session really exists and has its own body (not a fabricated directory entry).
    let id = fixture.client.send(
        "v4/conversation/subscribe",
        json!({
            "sessionId": child_id,
            "connectionId": "conn-child",
            "clientMode": "desktop-continuous",
        }),
    );
    let _ = fixture.client.response(id);
    let mut child_text = String::new();
    for _ in 0..40 {
        let line = fixture.client.next_line();
        let payload = &line["params"]["frame"]["payload"];
        if payload["kind"] == json!("snapshot")
            && payload["snapshot"]["sessionId"] == json!(child_id)
        {
            for row in payload["snapshot"]["rows"]["window"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                if row["kind"] == json!("assistantText") {
                    child_text.push_str(row["text"].as_str().unwrap_or_default());
                }
            }
            break;
        }
    }
    assert!(
        child_text.contains("hello from child"),
        "child transcript must hold the subagent answer: {child_text}"
    );

    let _ = std::fs::remove_dir_all(&fixture.base);
}

#[test]
fn compact_rejects_while_a_turn_is_running() {
    // Delayed provider: compact inside the in-flight turn window must be -32010 (not queued, not a fake success).
    let (responder, port) =
        ScriptedResponder::spawn_with_delay(vec![text_sse("slow")], Duration::from_millis(600));
    std::mem::forget(responder);
    let base = std::env::temp_dir().join(format!(
        "zcode-host-protocol-compact-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data");
    let config_path = base.join("provider.json");
    provider_config(port, &config_path);
    let mut process = HostProcess::spawn(&workspace, data_dir.as_path(), &config_path, false);
    let mut client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "create-1", None, json!({})),
    );
    let response = client.response(id);
    let session_id = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-slow-1",
            Some(&session_id),
            json!({"text": "slow turn"}),
        ),
    );
    let _ = client.response(id);
    let started = Instant::now();
    let mut rejected = false;
    while started.elapsed() < Duration::from_secs(2) {
        let id = client.send("session/compact", json!({"sessionId": session_id}));
        let response = client.response(id);
        if response["error"]["code"].as_i64() == Some(-32010) {
            rejected = true;
            break;
        }
        let compact = &response["result"]["compact"];
        if compact["state"] == json!("accepted") || compact["state"] == json!("already_running") {
            panic!("compact must not be admitted while a prompt is running: {response}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(rejected, "compact while running must be -32010");

    let _ = std::fs::remove_dir_all(&base);
}
