//! The Todo list surviving across processes: TS's `TodoWrite` writes through `sessionStore.updateTodos`
//! (`handlers/todo.ts:65`), so the task list is still there after a restart; this host originally kept only process memory
//! (`Arc<Mutex<Vec<_>>>`), so a restart clear it silently. What this e2e pins is the **property users can perceive**:
//! swap in another host process, ask `TodoRead` once more against the same session, and what comes back must still be the original few entries.
//!
//! The same harness as `tool_matrix_e2e`: a real host process, real tool execution, real files on disk;
//! the provider double lives only in the test process (`HostProcess`'s `Drop` is what kills the subprocess).

mod common;

use common::*;
use serde_json::{json, Value};
use std::path::Path;

fn pump_to_terminal_row(client: &mut Client, tool: &str) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for _ in 0..200 {
        let line = client.next_line();
        apply_to_rows(&mut rows, &deltas_from_frame(&line));
        let settled = rows.iter().any(|row| {
            row["kind"] == json!("toolCall")
                && row["toolName"] == json!(tool)
                && row["status"] != json!("running")
        });
        let answered = rows.iter().any(|row| row["kind"] == json!("assistantText"));
        if settled && answered {
            return rows;
        }
    }
    panic!(
        "{tool} line did not enter final state: {}",
        serde_json::to_string(&rows).unwrap()
    );
}

fn write_config(path: &Path, port: u16) {
    std::fs::write(
        path,
        json!({
            "kind": "anthropic",
            "baseUrl": format!("http://127.0.0.1:{port}"),
            "apiKey": "test-key-not-a-real-credential",
            "model": "test-model",            "maxTokens": 1024,
        })
        .to_string(),
    )
    .expect("provider config");
}

fn tool_row(rows: &[Value], tool: &str) -> Value {
    rows.iter()
        .find(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!(tool))
        .unwrap_or_else(|| panic!("{tool} row missing"))
        .clone()
}

fn create_and_send(client: &mut Client, text: &str) -> String {
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
            "cmd-send-a",
            Some(&session_id),
            json!({"text": text}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
    session_id
}

#[test]
fn todo_write_lands_in_the_session_dir_for_the_next_process() {
    let base = std::env::temp_dir().join(format!("zcode-host-todo-disk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base.join("provider.json");

    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse(
            "TodoWrite",
            &json!({"todos": [{"content": "port the desktop shell", "status": "pending", "priority": "high"}]}),
        ),
        text_sse("queued"),
    ]);
    write_config(&config_path, port);
    let todos_file;
    {
        let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, true);
        let mut client = Client {
            id_counter: 0,
            stdin: process.stdin(),
            stdout: process.stdout(),
            pending: std::collections::VecDeque::new(),
        };
        let session_id = create_and_send(&mut client, "track the work");
        let rows = pump_to_terminal_row(&mut client, "TodoWrite");
        let write = tool_row(&rows, "TodoWrite");
        assert_eq!(write["status"], json!("success"), "{write}");
        todos_file = data_dir
            .join("sessions")
            .join(&session_id)
            .join("todos.json");
        drop(client);
        drop(process); // Drop kills the subprocess: the in-memory state is gone at this point
    }
    drop(provider);

    // The copy on disk is exactly "the only thing readable after a restart", and its content matches what the model wrote.
    let on_disk = std::fs::read_to_string(&todos_file).unwrap_or_else(|error| {
        panic!(
            "TodoWrite must be placed ({}): {error}",
            todos_file.display()
        )
    });
    assert!(on_disk.contains("port the desktop shell"), "{on_disk}");
    assert!(
        on_disk.contains("high") && on_disk.contains("pending"),
        "{on_disk}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A session recovered across processes must keep conversing: after the restart, subscribe to the same sessionId and the `sendText` turn must really/// go through a tool turn plus a final-answer turn, with the tool's terminal row and TodoRead's historical content both coming back to the client.
///
/// Regression background: a new turn after recovery reuses **the tool call id left behind by the previous process** (the scripted provider
/// returns the same id fixed; a real provider may also reuse one in replay/retry). The row's locating key is
/// `(turnId, toolCallId)` rather than a global `toolCallId` — otherwise the new turn's TodoRead terminal state would rewrite the
/// previous turn's TodoWrite row whole. `session::tests::reused_tool_call_ids_across_turns_get_separate_rows`
/// pins that invariant; this case pins the end-to-end restart path.
#[test]
fn a_resumed_session_can_still_run_a_turn() {
    let base = std::env::temp_dir().join(format!("zcode-host-todo-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base.join("provider.json");

    let (provider_a, port_a) = ScriptedResponder::spawn(vec![
        tool_use_sse(
            "TodoWrite",
            &json!({"todos": [{"content": "port the desktop shell", "status": "pending", "priority": "high"}]}),
        ),
        text_sse("queued"),
    ]);
    write_config(&config_path, port_a);
    let session_id;
    {
        let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, true);
        let mut client = Client {
            id_counter: 0,
            stdin: process.stdin(),
            stdout: process.stdout(),
            pending: std::collections::VecDeque::new(),
        };
        session_id = create_and_send(&mut client, "track the work");
        pump_to_terminal_row(&mut client, "TodoWrite");
        drop(client);
        drop(process);
    }
    drop(provider_a);

    let (provider_b, port_b) = ScriptedResponder::spawn(vec![
        tool_use_sse("TodoRead", &json!({})),
        text_sse("remembered"),
    ]);
    write_config(&config_path, port_b);
    let mut process_b = HostProcess::spawn(&workspace, &data_dir, &config_path, true);
    let mut client_b = Client {
        id_counter: 0,
        stdin: process_b.stdin(),
        stdout: process_b.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    let id = client_b.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-2", "clientMode": "desktop-continuous"}),
    );
    let _ = client_b.response(id);
    let id = client_b.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send-b",
            Some(&session_id),
            json!({"text": "what is on the list?"}),
        ),
    );
    assert_eq!(client_b.response(id)["result"]["status"], json!("accepted"));
    let mut rows = pump_to_terminal_row(&mut client_b, "TodoRead");
    // The terminal tool row arrives when the "tool turn" ends; the final-answer turn is a new model request after that. The test must not
    // assert the request count before the second request has been sent — wait for the assistantText final draft (the script's second response) to arrive.
    for _ in 0..200 {
        if provider_b.request_bodies().len() >= 2
            && rows.iter().any(|row| {
                row["kind"] == json!("assistantText") && row["text"] == json!("remembered")
            })
        {
            break;
        }
        let line = client_b.next_line();
        apply_to_rows(&mut rows, &deltas_from_frame(&line));
    }
    let output = tool_row(&rows, "TodoRead")["output"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        output.contains("port the desktop shell"),
        "TodoRead after restarting must read back the original task: {output}"
    );
    assert_eq!(
        provider_b.request_bodies().len(),
        2,
        "The round after restart is tool wheel + final answer wheel: two model requests"
    );
    assert!(
        rows.iter().any(|row| {
            row["kind"] == json!("assistantText") && row["text"] == json!("remembered")
        }),
        "the final answer row arrives after the tool round: {rows:?}"
    );
    drop(client_b);
    let _ = std::fs::remove_dir_all(&base);
}
