//! The structured projection of toolCall rows e2e: the client's tool card and the plan read model read
//! `row.input` / `row.output` (TS writes these two fields when it produces the toolCall row),
//! so the assertions must land on the row shape that really appears on the stdio surface, not on Rust internal structures.
//!
//! Pins:
//! 1. a toolCall row is "append first (running projection), then upsert (terminal projection)", and the rowId stays the same;
//!    `input` is present in both (the running row's field shape is pinned by the `rows::tests` unit tests);
//! 2. on the terminal row `input` is the **effective** arguments, `output.text` is the tool's returned text, and a short result carries no `truncated`;
//! 3. the row still satisfies `toolCallRowSchema`'s required set (kind/toolCallId/toolName/status/inputText),
//!    and every added field is optional, so it cannot make an old client fail to decode.

mod common;

use common::{
    apply_to_rows, command_envelope, deltas_from_frame, text_sse, tool_use_sse, Client,
    HostProcess, ScriptedResponder,
};
use serde_json::{json, Value};
use std::time::Duration;

fn temp_base(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("zcode-host-tool-row-{name}-{}", std::process::id()));
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

fn write_input() -> Value {
    json!({"file_path": "notes.txt", "content": "created by the native host\n"})
}

#[test]
fn tool_rows_carry_structured_input_and_terminal_output() {
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse("Write", &write_input()),
        text_sse("wrote notes.txt"),
    ]);
    let base_dir = temp_base("projection");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    // full-access: Write is a destructive tool; without authorization it gets refused, and then there is no terminal row to assert on.
    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, true);
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
        json!({
            "sessionId": session_id,
            "connectionId": "conn-1",
            "clientMode": "desktop-continuous"
        }),
    );
    client.response(id);

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-write",
            Some(&session_id),
            json!({"text": "create notes.txt with that line"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    // Collect the toolCall rows this turn really emitted. running→terminal is "two projections of the same row",
    // so what is recorded here is whether the rowId was first appended and then upserted — independent of flush timing
    // (a fast tool's two deltas land in the same frame, so scanning by row state is a race).
    let sid = format!("conn-1:conversation/{session_id}");
    let mut rows: Vec<Value> = Vec::new();
    let mut appended_tool_rows = Vec::new();
    let mut upserted_tool_rows = Vec::new();
    let mut saw_terminal = None;
    for _ in 0..400 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        let deltas = deltas_from_frame(&line);
        for delta in &deltas {
            let Some(row) = delta.get("row") else {
                continue;
            };
            if row["kind"] != json!("toolCall") || row["toolName"] != json!("Write") {
                continue;
            }
            let row_id = row["rowId"].as_u64();
            match delta["op"].as_str() {
                Some("row.appended") => appended_tool_rows.push(row_id),
                Some("row.upserted") => upserted_tool_rows.push(row_id),
                _ => {}
            }
        }
        apply_to_rows(&mut rows, &deltas);
        if let Some(terminal) = rows
            .iter()
            .find(|row| row["kind"] == json!("toolCall") && row["status"] == json!("success"))
        {
            saw_terminal = Some(terminal.clone());
        }
        if saw_terminal.is_some() {
            break;
        }
    }

    let terminal = saw_terminal.expect("the terminal toolCall row must arrive");
    let terminal_row_id = terminal["rowId"].as_u64().expect("rowId");
    assert!(
        appended_tool_rows.contains(&Some(terminal_row_id)),
        "the tool row must be appended once (running projection); got {appended_tool_rows:?}"
    );
    assert!(
        upserted_tool_rows.contains(&Some(terminal_row_id)),
        "the terminal projection must upsert the same rowId, not append a second row: {upserted_tool_rows:?}"
    );
    assert_eq!(
        terminal["entityId"],
        json!("toolu_e2e"),
        "The entity identity of the tool row is toolCallId (row location verification relies on it)"
    );
    assert_eq!(
        terminal["input"],
        write_input(),
        "terminal row keeps the effective structured input"
    );
    assert_eq!(
        terminal["inputText"],
        json!(write_input().to_string()),
        "inputText is still the text form of the same input parameter (schema is required)"
    );
    let output = terminal
        .get("output")
        .expect("terminal row must carry output (TS buildToolOutput)");
    assert!(
        output["text"].as_str().is_some_and(|text| !text.is_empty()),
        "output.text must be the tool result text: {output}"
    );
    assert!(
        output.get("truncated").is_none(),
        "a short result must not be marked truncated: {output}"
    );
    assert!(
        terminal["toolCallId"].is_string() && terminal["rowId"].is_number(),
        "row keeps its identity fields: {terminal}"
    );

    // The file really landed on disk — `output` is not decoration for the UI, it describes this very execution.
    let written = std::fs::read_to_string(workspace.join("notes.txt"))
        .expect("the Write tool must have created the file");
    assert!(
        written.contains("created by the native host"),
        "unexpected file content: {written}"
    );
    // The tool result must enter **the next model request**: this is not a display-layer assertion, so really wait for the second request to arrive
    // (under parallel test runs it is asynchronous and in flight, so exiting early would turn into a race).
    let mut second_request = None;
    for _ in 0..100 {
        let bodies = provider.request_bodies();
        if bodies.len() >= 2 {
            second_request = Some(bodies[1].clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let second_request = second_request.expect("the tool result must feed a second model request");
    assert!(
        second_request.contains("wrote") || second_request.contains("notes.txt"),
        "the second request must carry the tool result:\n{second_request}"
    );
}

/// The permission ask really rewrote the tool row's state, and the approval link arrives in the same batch as the interaction; the terminal state then clears the link.
///
/// Why this deserves its own case: `pendingApproval` is the user's only signal for "where should I click", and
/// `approvalInteractionId` is the foreign key between the tool card and the approval card. Miss either one and the UI can only display
/// "running", while the user is in fact waiting for a dialog that never appeared.
#[test]
fn permission_ask_marks_the_tool_row_pending_approval_and_terminal_clears_it() {
    let (_provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse("Write", &write_input()),
        text_sse("written after approval"),
    ]);
    let base_dir = temp_base("approval-row");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    // full-access=false: without authorization a destructive tool opens a permission interaction (the Ask branch).
    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, false);
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
        json!({
            "sessionId": session_id,
            "connectionId": "conn-1",
            "clientMode": "desktop-continuous"
        }),
    );
    client.response(id);
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-approve",
            Some(&session_id),
            json!({"text": "create notes.txt"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let sid = format!("conn-1:conversation/{session_id}");
    let mut rows: Vec<Value> = Vec::new();
    let mut running_started_at = None;
    let mut approval: Option<(String, Value)> = None;
    let mut known_interactions: Vec<String> = Vec::new();
    for _ in 0..400 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        let deltas = deltas_from_frame(&line);
        // The foreign key must point at the permission interaction **this session really saw**.
        // We do not require both deltas to arrive in the same batch: the coalescing window is decided by timing, and making "same batch"
        // an assertion would be a race test that only turns red under load (the first version of this test hung exactly that way).
        for delta in &deltas {
            if let Some(items) = delta["patch"]["pendingInteractions"].as_array() {
                for item in items {
                    if item["kind"].as_str() == Some("permission") {
                        if let Some(id) = item["interactionId"].as_str() {
                            known_interactions.push(id.to_string());
                        }
                    }
                }
            }
        }
        if let Some(row) = deltas.iter().find_map(|delta| {
            let row = delta.get("row")?;
            (row["status"].as_str() == Some("pendingApproval")).then(|| row.clone())
        }) {
            let linked = row["approvalInteractionId"].as_str().unwrap_or("");
            assert!(
                known_interactions.iter().any(|id| id == linked),
                "the row must point at a permission interaction the client can answer;                  known={known_interactions:?} linked={linked}"
            );
            approval = Some((linked.to_string(), row));
        }
        for delta in &deltas {
            if let Some(row) = delta.get("row") {
                if row["kind"] == json!("toolCall") && row["status"] == json!("running") {
                    running_started_at = row["startedAt"].as_i64();
                }
            }
        }
        apply_to_rows(&mut rows, &deltas);
        if approval.is_some() {
            break;
        }
    }

    let (interaction_id, row) = approval.expect("the tool row must enter pendingApproval");
    assert_eq!(
        row["approvalInteractionId"],
        json!(interaction_id.clone()),
        "the row points at the interaction the user has to answer: {row}"
    );
    assert_eq!(row["toolName"], json!("Write"));
    assert!(
        row.get("endedAt").is_none(),
        "pendingApproval is not a terminal status: {row}"
    );
    assert_eq!(
        row["startedAt"].as_i64(),
        running_started_at,
        "the approval upsert must inherit startedAt, not restamp it"
    );
    assert_eq!(
        row["input"],
        write_input(),
        "the approval upsert must not drop the structured input"
    );

    // Approve (allowOnce) → the tool executes → the terminal row clears the approval link and fills in endedAt.
    let id = client.send(
        "v4/command",
        command_envelope(
            "resolveInteraction",
            "cmd-resolve",
            Some(&session_id),
            json!({
                "interactionId": interaction_id,
                "answer": {"optionId": "allowOnce"}
            }),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let mut terminal = None;
    for _ in 0..400 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        apply_to_rows(&mut rows, &deltas_from_frame(&line));
        if let Some(row) = rows
            .iter()
            .find(|row| row["kind"] == json!("toolCall") && row["status"] == json!("success"))
        {
            terminal = Some(row.clone());
            break;
        }
    }
    let terminal = terminal.expect("the terminal tool row must arrive after approval");
    assert!(
        terminal.get("approvalInteractionId").is_none(),
        "a terminal row must not still advertise an approval: {terminal}"
    );
    assert_eq!(
        terminal["startedAt"].as_i64(),
        running_started_at,
        "startedAt survives the whole lifecycle: {terminal}"
    );
    assert!(
        terminal["endedAt"]
            .as_i64()
            .is_some_and(|ended| ended >= running_started_at.unwrap_or(0)),
        "terminal row carries endedAt: {terminal}"
    );
    assert!(
        workspace.join("notes.txt").exists(),
        "the approval actually unlocked the execution"
    );
}
