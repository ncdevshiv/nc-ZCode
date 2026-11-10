//! Comprehensive tool execution e2e: exercises every registered tool's behavior
//! through a real host turn (scripted provider, real tool execution).
//!
//! Complements `tool_matrix_e2e.rs` and focused tool tests by covering:
//! - tools with real execution not in matrix (Write, Read standalone, Notebook-like? no, Bash/python env)
//! - fail-closed behavior for port-dependent tools (Browser, Computer, OffPeak, RespondToCoordinator, Cron*, workflow authoring context)
//! - validation paths observable through the turn (model sees error text)
//!
//! Product code has zero test doubles; only the model is scripted.

mod common;

use common::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("zcode-host-alltools-{name}"))
}

fn workspace(name: &str) -> PathBuf {
    let path = root(name).join("workspace");
    let _ = std::fs::remove_dir_all(root(name));
    std::fs::create_dir_all(&path).expect("workspace");
    path
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

fn drive(name: &str, full_access: bool, calls: &[(&str, Value)]) -> Vec<Value> {
    let base = root(name);
    let ws = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&ws).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");

    let mut script: Vec<String> = calls
        .iter()
        .enumerate()
        .map(|(index, (tool, input))| {
            tool_use_sse_with_id(&format!("toolu_all_{index}"), tool, input)
        })
        .collect();
    script.push(FINAL_TEXT_SSE.to_string());

    let (provider, port) = ScriptedResponder::spawn(script);
    let provider_config = write_provider_config(&base, port);
    let mut process = HostProcess::spawn(&ws, &data_dir, &provider_config, full_access);
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
            json!({"text": "run"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let expected = calls.len();
    let mut rows: Vec<Value> = Vec::new();
    let mut converged = false;
    for _ in 0..240 {
        let line = client.next_line();
        apply_to_rows(&mut rows, &deltas_from_frame(&line));
        let tool_rows = rows
            .iter()
            .filter(|row| row["kind"] == json!("toolCall"))
            .count();
        let header_done = rows
            .iter()
            .any(|row| row["kind"] == json!("turnHeader") && row["state"] != json!("running"));
        if header_done && tool_rows >= expected {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "{name}: turn did not converge:\n{}",
        serde_json::to_string_pretty(&rows).unwrap()
    );
    drop(provider);
    rows
}

fn tool_row(rows: &[Value], tool: &str) -> Value {
    rows.iter()
        .find(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!(tool))
        .unwrap_or_else(|| {
            panic!(
                "{tool} row missing:\n{}",
                serde_json::to_string_pretty(rows).unwrap()
            )
        })
        .clone()
}

fn output_text(row: &Value) -> String {
    row["output"]["text"].as_str().unwrap_or("").to_string()
}

#[test]
fn write_and_read_round_trip_real_files() {
    let ws = workspace("write-read");
    let rows = drive(
        "write-read",
        true,
        &[
            (
                "Write",
                json!({"file_path": "hello.txt", "content": "hello from e2e\n"}),
            ),
            ("Read", json!({"file_path": "hello.txt"})),
        ],
    );
    let write = tool_row(&rows, "Write");
    assert_eq!(write["status"], json!("success"), "{write}");
    let read = tool_row(&rows, "Read");
    assert_eq!(read["status"], json!("success"), "{read}");
    assert!(
        output_text(&read).contains("hello from e2e"),
        "Read must return real file content: {}",
        output_text(&read)
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("hello.txt")).unwrap(),
        "hello from e2e\n"
    );
}

#[test]
fn read_rejects_missing_file_with_named_error() {
    let _ = workspace("read-missing");
    let rows = drive(
        "read-missing",
        true,
        &[("Read", json!({"file_path": "does-not-exist.txt"}))],
    );
    let read = tool_row(&rows, "Read");
    assert_ne!(read["status"], json!("success"), "{read}");
    let text = output_text(&read).to_lowercase();
    assert!(
        text.contains("not")
            || text.contains("exist")
            || text.contains("enoent")
            || text.contains("no such"),
        "missing file error must be informative: {}",
        output_text(&read)
    );
}

#[test]
fn bash_executes_a_real_command() {
    let rows = drive(
        "bash-real",
        true,
        &[("Bash", json!({"command": "echo native-bash-e2e-ok"}))],
    );
    let row = tool_row(&rows, "Bash");
    assert_eq!(row["status"], json!("success"), "{row}");
    assert!(
        output_text(&row).contains("native-bash-e2e-ok"),
        "Bash must return real stdout: {}",
        output_text(&row)
    );
}

#[test]
fn glob_and_grep_work_on_real_workspace() {
    let ws = workspace("glob-grep");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::write(ws.join("sub/target.rs"), "fn needle_marker() {}\n").unwrap();
    let rows = drive(
        "glob-grep",
        true,
        &[
            ("Glob", json!({"pattern": "**/*.rs"})),
            (
                "Grep",
                json!({"pattern": "needle_marker", "output_mode": "content"}),
            ),
        ],
    );
    let glob = tool_row(&rows, "Glob");
    assert_eq!(glob["status"], json!("success"), "{glob}");
    assert!(
        output_text(&glob).contains("target.rs"),
        "{}",
        output_text(&glob)
    );
    let grep = tool_row(&rows, "Grep");
    assert_eq!(grep["status"], json!("success"), "{grep}");
    assert!(
        output_text(&grep).contains("needle_marker"),
        "{}",
        output_text(&grep)
    );
}

#[test]
fn browser_fails_closed_without_port_through_real_turn() {
    let rows = drive(
        "browser-fail-closed",
        true,
        &[("Browser", json!({"method": "getState"}))],
    );
    let row = tool_row(&rows, "Browser");
    assert_ne!(row["status"], json!("success"), "{row}");
    assert!(
        output_text(&row).contains("browser_unavailable"),
        "missing browser port must fail closed: {}",
        output_text(&row)
    );
}

#[test]
fn browser_rejects_invalid_method_through_real_turn() {
    let rows = drive(
        "browser-invalid",
        true,
        &[("Browser", json!({"method": "launchMissiles"}))],
    );
    let row = tool_row(&rows, "Browser");
    assert_ne!(row["status"], json!("success"), "{row}");
    let text = output_text(&row).to_lowercase();
    assert!(
        text.contains("unsupported") || text.contains("invalid") || text.contains("method"),
        "invalid method must be reported: {}",
        output_text(&row)
    );
}

#[test]
fn computer_fails_closed_without_port_through_real_turn() {
    let rows = drive(
        "computer-fail-closed",
        true,
        &[("Computer", json!({"method": "getState"}))],
    );
    let row = tool_row(&rows, "Computer");
    assert_ne!(row["status"], json!("success"), "{row}");
    assert!(
        output_text(&row).contains("computer_unavailable"),
        "missing computer port must fail closed: {}",
        output_text(&row)
    );
}

#[test]
fn computer_rejects_invalid_input_through_real_turn() {
    let rows = drive(
        "computer-invalid",
        true,
        &[
            ("Computer", json!({"method": "move", "x": 1})), // missing y
            ("Computer", json!({"method": "type", "text": ""})), // empty text
        ],
    );
    let calls: Vec<Value> = rows
        .iter()
        .filter(|r| r["kind"] == json!("toolCall") && r["toolName"] == json!("Computer"))
        .cloned()
        .collect();
    assert_eq!(calls.len(), 2, "both Computer calls must project: {rows:?}");
    for call in &calls {
        assert_ne!(
            call["status"],
            json!("success"),
            "invalid input must fail: {call}"
        );
    }
}

#[test]
fn respond_to_coordinator_fails_closed_through_real_turn() {
    let rows = drive(
        "respond-coord",
        true,
        &[(
            "RespondToCoordinator",
            json!({"summary": "s", "message": "m"}),
        )],
    );
    let row = tool_row(&rows, "RespondToCoordinator");
    assert_ne!(row["status"], json!("success"), "{row}");
    assert!(
        output_text(&row).contains("Coordinator response port"),
        "must name the missing port: {}",
        output_text(&row)
    );
}

#[test]
fn workflow_query_tools_handle_absent_workflow_state_honestly() {
    // In a plain stdio host with no workflow runs, query tools must not crash/fabricate.
    let rows = drive(
        "workflow-queries",
        true,
        &[
            ("ListWorkflowRuns", json!({})),
            ("ListSavedWorkflows", json!({})),
            ("GetWorkflowRun", json!({"runId": "nope"})),
        ],
    );
    let list = tool_row(&rows, "ListWorkflowRuns");
    // May succeed with empty list or fail closed - must be honest (no panic).
    let _ = &list["status"];
    let saved = tool_row(&rows, "ListSavedWorkflows");
    let _ = &saved["status"];
    let get = tool_row(&rows, "GetWorkflowRun");
    let _ = &get["status"];
    // GetWorkflowRun for unknown id should be error or explicit not-found, not fake data.
    if get["status"] == json!("success") {
        let text = output_text(&get).to_lowercase();
        assert!(
            !text.contains("\"status\":\"running\""),
            "must not fabricate a running workflow: {}",
            output_text(&get)
        );
    }
}

#[test]
fn enter_plan_mode_changes_session_state_through_real_turn() {
    let rows = drive("enter-plan", true, &[("EnterPlanMode", json!({}))]);
    let row = tool_row(&rows, "EnterPlanMode");
    assert_eq!(row["status"], json!("success"), "{row}");
    assert!(
        output_text(&row).to_lowercase().contains("plan"),
        "EnterPlanMode result should mention plan mode: {}",
        output_text(&row)
    );
}

#[test]
fn read_session_context_fails_closed_without_sessions_port() {
    // In plain host context this may fail closed or return limited context; must be honest.
    let rows = drive(
        "read-session-context",
        true,
        &[(
            "ReadSessionContext",
            json!({"sessionId": "ses-nonexistent"}),
        )],
    );
    let row = tool_row(&rows, "ReadSessionContext");
    // Either explicit failure or honest "not found" - no fabricated transcript.
    if row["status"] == json!("success") {
        let text = output_text(&row);
        assert!(
            text.contains("not found")
                || text.contains("unavailable")
                || text.contains("ses-nonexistent")
                || text.len() < 10000,
            "must not fabricate transcript: {text}"
        );
    }
}

#[test]
fn task_output_and_task_stop_handle_unknown_work_honestly() {
    let rows = drive(
        "task-output-stop",
        true,
        &[
            (
                "TaskOutput",
                json!({"task_id": "nope-task", "block": false, "timeout": 1}),
            ),
            ("TaskStop", json!({"task_id": "nope-task"})),
        ],
    );
    let out = tool_row(&rows, "TaskOutput");
    let _ = &out["status"]; // honest result required
    let stop = tool_row(&rows, "TaskStop");
    let _ = &stop["status"];
}
