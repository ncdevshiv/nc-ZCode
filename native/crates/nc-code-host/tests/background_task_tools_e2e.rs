//! The model-side closed loop for background tasks: the model starts a background command → reads it with `TaskOutput` →
//! stops it with `TaskStop`; all three steps run on real stdio + a real scripted provider.
//!
//! Pins three things:
//! 1. **the two tools really are in the model's tool surface** (the request body sent to the provider shows
//!    `TaskOutput`/`TaskStop`, not something only a Rust unit test can reach);
//! 2. **the row carries a result-derived display**: the `task_output` and `task_stop` variants are built from this call's result
//!    (TS `createToolResultDisplay`), and the client renders the card from it;
//! 3. **stopping is a real stop**: after `TaskStop` the detail surface steadily reports `cancelled`, not merely a flipped flag.
//!
//! TaskStop's **no-ask gate** (TS `alwaysAllowPatternSources:["toolName"]`) is
//! asserted against the real gate in `tools::background_tools::tests` — that decision needs no provider.
//! Here we use `--full-access`: the command in this case is a compound command (`echo … && sleep …`), which under
//! the host's conservative argv classification is not read-only, and without authorization it would first open a permission interaction,
//! blending "did the closed loop run through" and "did the gate let it pass" into the same assertion.
//!
//! workId uses the deterministic `bash-<sessionId>-<n>` format, and the sessionId is known right after the session is created,
//! so the script can substitute it in before the first model request (`ScriptedResponder::set_script`).

mod common;

use common::{
    apply_to_rows, command_envelope, deltas_from_frame, text_sse, tool_use_sse_with_id, Client,
    HostProcess, ScriptedResponder,
};
use serde_json::{json, Value};

const PROBE: &str = "bg-tool-loop-probe";

fn temp_base(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("zcode-host-bg-tools-{name}-{}", std::process::id()));
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

fn client_for(process: &mut HostProcess) -> Client {
    Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    }
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

fn subscribe(client: &mut Client, session_id: &str) {
    let id = client.send(
        "v4/conversation/subscribe",
        json!({
            "sessionId": session_id,
            "connectionId": "conn-1",
            "clientMode": "desktop-continuous"
        }),
    );
    client.response(id);
}

fn query_output(client: &mut Client, session_id: &str, work_id: &str) -> Value {
    let id = client.send(
        "v4/conversation/backgroundBashOutput",
        json!({"sessionId": session_id, "workId": work_id}),
    );
    client.response(id)["result"].clone()
}

/// Collect the terminal projections of the three tool rows (the same row across appended→upserted).
fn terminal_tool_rows(client: &mut Client, session_id: &str, want: &[&str]) -> Vec<Value> {
    let sid = format!("conn-1:conversation/{session_id}");
    let mut rows: Vec<Value> = Vec::new();
    let mut found: Vec<Value> = Vec::new();
    for _ in 0..600 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        let deltas = deltas_from_frame(&line);
        apply_to_rows(&mut rows, &deltas);
        found = rows
            .iter()
            .filter(|row| {
                row["kind"] == json!("toolCall")
                    && want.contains(&row["toolName"].as_str().unwrap_or_default())
                    && matches!(
                        row["status"].as_str(),
                        Some("success") | Some("error") | Some("cancelled")
                    )
            })
            .cloned()
            .collect();
        if found.len() == want.len() {
            return found;
        }
    }
    panic!("terminal rows for {want:?} never all arrived; got {found:?}");
}

#[test]
fn the_model_starts_reads_and_stops_a_background_command() {
    // Start from an empty script: the sessionId is only known once the session is created, and TaskOutput/TaskStop's arguments need the workId.
    let (provider, port) = ScriptedResponder::spawn(vec![]);
    let base_dir = temp_base("loop");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, true);
    let mut client = client_for(&mut process);
    let session_id = create_session(&mut client);
    subscribe(&mut client, &session_id);
    let work_id = format!("bash-{session_id}-1");

    // A command that is certainly still alive (on Windows time it with ping; a container does not necessarily have sleep):
    // that way TaskOutput reads running, and TaskStop also catches running.
    let long = if cfg!(windows) {
        "ping -n 20 127.0.0.1"
    } else {
        "sleep 20"
    };
    provider.set_script(vec![
        tool_use_sse_with_id(
            "toolu-bg-start",
            "Bash",
            &json!({"command": format!("echo {PROBE} && {long}"), "run_in_background": true}),
        ),
        tool_use_sse_with_id(
            "toolu-bg-read",
            "TaskOutput",
            &json!({"task_id": work_id, "block": false, "timeout": 0}),
        ),
        tool_use_sse_with_id("toolu-bg-stop", "TaskStop", &json!({"task_id": work_id})),
        text_sse("stopped the watcher"),
    ]);

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-loop",
            Some(&session_id),
            json!({"text": "start a watcher, read it, then stop it"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let rows = terminal_tool_rows(
        &mut client,
        &session_id,
        &["Bash", "TaskOutput", "TaskStop"],
    );
    let row_of = |name: &str| -> Value {
        rows.iter()
            .find(|row| row["toolName"] == json!(name))
            .cloned()
            .unwrap_or_else(|| panic!("no {name} row in {rows:?}"))
    };

    let start = row_of("Bash");
    assert_eq!(start["status"], json!("success"), "{start}");
    assert_eq!(start["backgrounded"], json!(true));
    assert_eq!(start["workId"], json!(work_id));

    let read = row_of("TaskOutput");
    assert_eq!(read["status"], json!("success"), "{read}");
    assert_eq!(read["display"]["kind"], json!("task_output"), "{read}");
    assert_eq!(
        read["display"]["retrievalStatus"],
        json!("not_ready"),
        "the command is still running and block=false, so the honest answer is not_ready"
    );
    assert_eq!(read["display"]["taskStatus"], json!("running"));

    let stop = row_of("TaskStop");
    assert_eq!(stop["status"], json!("success"), "{stop}");
    assert_eq!(stop["display"]["kind"], json!("task_stop"), "{stop}");
    assert_eq!(stop["display"]["taskId"], json!(work_id));
    assert_eq!(stop["display"]["taskType"], json!("local_bash"));

    // Stopping really stopped the process: the detail surface steadily reports cancelled.
    let detail = query_output(&mut client, &session_id, &work_id);
    assert_eq!(detail["kind"], json!("output"), "{detail}");
    assert_eq!(detail["status"], json!("cancelled"), "{detail}");

    // Exposed on the tool surface to the model: all three names appear in the request bodies sent to the provider (by now the whole turn
    // has finished and all request bodies are recorded, so no timing gamble here).
    let bodies = provider.request_bodies();
    assert!(
        bodies.len() >= 3,
        "the loop made {}/4 scripted model requests: {bodies:?}",
        bodies.len()
    );
    for name in ["TaskOutput", "TaskStop"] {
        assert!(
            bodies[0].contains(name),
            "{name} must be part of the model's tool surface"
        );
    }
    let _ = base_dir;
}
