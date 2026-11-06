//! The end-to-end surface of background bash: the model hands one command to the background → the turn closes immediately → the client polls
//! `v4/conversation/backgroundBashOutput` for the real output.
//!
//! Pins three things that each break separately in production:
//! 1. **the handoff on the row**: the terminal toolCall row carries `backgrounded: true` + `workId`
//!    (`rows.ts:229-230`). The client can only start a query once it got the workId from the row;
//! 2. **the read model returns real output**: `kind: "output"` and `output` holds the text the command really printed,
//!    and the state advances from running to terminal (not "queried once, then frozen");
//! 3. **the observation surface revives nothing**: unknown workId → `unavailable`; missing parameter → protocol error,
//!    never a `kind: "output"` that merely looks like empty output.

mod common;

use common::{
    apply_to_rows, command_envelope, deltas_from_frame, text_sse, tool_use_sse, Client,
    HostProcess, ScriptedResponder,
};
use serde_json::{json, Value};
use std::time::Duration;

const PROBE: &str = "bg-e2e-probe-line";

fn temp_base(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zcode-host-bg-{name}-{}", std::process::id()));
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

fn start_session(client: &mut Client) -> String {
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string()
}

/// Runs one turn of "the model calls Bash(run_in_background)" and returns the terminal toolCall row.
fn terminal_bash_row(client: &mut Client, session_id: &str) -> Value {
    let sid = format!("conn-1:conversation/{session_id}");
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-bg",
            Some(session_id),
            json!({"text": "start the probe in the background"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let mut rows: Vec<Value> = Vec::new();
    for _ in 0..600 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        let deltas = deltas_from_frame(&line);
        apply_to_rows(&mut rows, &deltas);
        if let Some(row) = rows
            .iter()
            .find(|row| row["kind"] == json!("toolCall") && row["status"] == json!("success"))
        {
            return row.clone();
        }
    }
    panic!("the terminal toolCall row never arrived");
}

fn query_output(client: &mut Client, session_id: &str, work_id: &str) -> Value {
    let id = client.send(
        "v4/conversation/backgroundBashOutput",
        json!({"sessionId": session_id, "workId": work_id}),
    );
    client.response(id)["result"].clone()
}

#[test]
fn a_backgrounded_command_projects_its_work_id_and_streams_its_output_back() {
    let (_provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse(
            "Bash",
            &json!({"command": format!("echo {PROBE}"), "run_in_background": true}),
        ),
        text_sse("started in the background"),
    ]);
    let base_dir = temp_base("projection");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, true);
    let mut client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    let session_id = start_session(&mut client);
    let id = client.send(
        "v4/conversation/subscribe",
        json!({
            "sessionId": session_id,
            "connectionId": "conn-1",
            "clientMode": "desktop-continuous"
        }),
    );
    client.response(id);

    let row = terminal_bash_row(&mut client, &session_id);
    assert_eq!(
        row["backgrounded"],
        json!(true),
        "the row must say the command was handed off: {row}"
    );
    let work_id = row["workId"].as_str().unwrap_or_default().to_string();
    assert!(!work_id.is_empty(), "row must carry the workId: {row}");
    // The same id is in the tool text: the model uses it (and outputPath) to keep observing this command.
    let output_text = row["output"]["text"].as_str().unwrap_or_default();
    assert!(
        output_text.contains(&work_id),
        "the model-visible result and the row must name the same work: {row}"
    );

    // The output file is advanced by the process itself: poll until the line the command printed shows up.
    let mut snapshot = Value::Null;
    for _ in 0..60 {
        snapshot = query_output(&mut client, &session_id, &work_id);
        if snapshot["output"]
            .as_str()
            .is_some_and(|text| text.contains(PROBE))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(snapshot["kind"], json!("output"), "{snapshot}");
    assert_eq!(snapshot["workId"], json!(work_id));
    assert_eq!(snapshot["truncated"], json!(false));
    assert!(
        snapshot["output"]
            .as_str()
            .unwrap_or_default()
            .contains(PROBE),
        "the detail pane must show what the command printed: {snapshot}"
    );
    assert!(
        snapshot["outputPath"]
            .as_str()
            .unwrap_or_default()
            .ends_with(&format!("{work_id}.log"))
            && snapshot["outputPath"]
                .as_str()
                .unwrap_or_default()
                .contains("background"),
        "outputPath points at the session's background dir: {snapshot}"
    );

    // An unregistered workId: the observation surface answers "there is no such task" and invents no empty output.
    let settled = {
        let mut current = snapshot.clone();
        for _ in 0..60 {
            current = query_output(&mut client, &session_id, &work_id);
            if current["status"] != json!("running") {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        current
    };
    assert_ne!(
        settled["status"],
        json!("running"),
        "the process watcher must advance the status on its own: {settled}"
    );

    assert_eq!(
        query_output(&mut client, &session_id, "bash-nope-1"),
        json!({"kind": "unavailable", "workId": "bash-nope-1"})
    );
}

#[test]
fn the_read_rejects_a_missing_work_id_and_an_unknown_session_is_unavailable() {
    let (_provider, port) = ScriptedResponder::spawn(vec![text_sse("unused")]);
    let base_dir = temp_base("guards");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, true);
    let mut client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };

    let id = client.send(
        "v4/conversation/backgroundBashOutput",
        json!({"sessionId": "ses-whatever"}),
    );
    let response = client.response(id);
    assert_eq!(
        response["error"]["code"],
        json!(-32602),
        "a missing params.workId is an invalid request, not an empty answer: {response}"
    );

    // A cold session (this process never created it) must likewise not be revived by a single observation query: same ending as an unknown workId.
    let id = client.send(
        "v4/conversation/backgroundBashOutput",
        json!({"sessionId": "ses-cold", "workId": "bash-ses-cold-1"}),
    );
    assert_eq!(
        client.response(id)["result"],
        json!({"kind": "unavailable", "workId": "bash-ses-cold-1"})
    );
}
