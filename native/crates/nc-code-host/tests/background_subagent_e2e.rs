//! Background sub-agent e2e: the model calls `Agent(run_in_background:true)` and immediately gets async_launched,
//! the sub-agent finishes in the background and writes its result into outputFile, and the parent session reads it back on turn two with `TaskOutput`.
//!
//! Uses the real stdio binary + a scripted provider; assertions land on the **row projection** and **the output file on disk**,
//! not on internal structure. Note on script order: the parent turn's and the child turn's model requests share the same provider,
//! so scripts 2 and 3 use the same text (whichever arrives first is fine), and script 4 is reserved for turn two's TaskOutput.

mod common;

use common::{
    apply_to_rows, command_envelope, deltas_from_frame, text_sse, tool_use_sse_with_id, Client,
    HostProcess, ScriptedResponder,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const CHILD_TEXT: &str = "child finished the analysis";

fn temp_base(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("zcode-host-bg-agent-{name}-{}", std::process::id()));
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

fn poll_rows(client: &mut Client, session_id: &str, want: &str) -> Vec<Value> {
    let sid = format!("conn-1:conversation/{session_id}");
    let mut rows: Vec<Value> = Vec::new();
    for _ in 0..800 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        let deltas = deltas_from_frame(&line);
        apply_to_rows(&mut rows, &deltas);
        if rows.iter().any(|row| {
            row["kind"] == json!("toolCall")
                && row["toolName"] == json!(want)
                && matches!(row["status"].as_str(), Some("success") | Some("error"))
        }) {
            return rows;
        }
    }
    panic!("no terminal {want} row in {rows:?}");
}

fn send_text(client: &mut Client, command_id: &str, session_id: &str, text: &str) {
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            command_id,
            Some(session_id),
            json!({"text": text}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
}

#[test]
fn a_background_agent_finishes_on_its_own_and_task_output_reads_its_result() {
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse_with_id(
            "toolu-agent-bg",
            "Agent",
            &json!({
                "description": "background analysis",
                "prompt": "analyse the repo and report",
                "run_in_background": true,
            }),
        ),
        // The parent turn's closing answer and the child turn's model answer share the same text (the request order is decided by the race).
        text_sse(CHILD_TEXT),
        text_sse(CHILD_TEXT),
    ]);
    let base_dir = temp_base("flow");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, true);
    let mut client = client_for(&mut process);
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

    send_text(
        &mut client,
        "cmd-bg",
        &session_id,
        "start a background analysis",
    );
    let rows = poll_rows(&mut client, &session_id, "Agent");
    let agent_row = rows
        .iter()
        .find(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!("Agent"))
        .cloned()
        .expect("agent row");
    assert_eq!(agent_row["status"], json!("success"), "{agent_row}");
    let output: Value =
        serde_json::from_str(agent_row["output"]["text"].as_str().unwrap_or_default())
            .expect("async_launched json");
    assert_eq!(output["status"], json!("async_launched"), "{output}");
    assert_eq!(output["isAsync"], json!(true));
    assert_eq!(output["canReadOutputFile"], json!(true));
    let agent_id = output["agentId"].as_str().expect("agentId").to_string();
    assert_eq!(output["backgroundTaskId"], json!(agent_id));
    assert_eq!(output["childSessionId"], json!(agent_id));
    assert_eq!(agent_row["backgrounded"], json!(true));
    assert_eq!(agent_row["workId"], json!(agent_id));

    // The sub-agent finishes by itself in the background: wait for session/subagents to report success, and check the output file.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut status = String::new();
    while Instant::now() < deadline {
        let id = client.send("session/subagents", json!({"sessionId": session_id}));
        let response = client.response(id);
        // The real shape: `running: []` + `ended: {total, items: []}` (protocol_methods.rs).
        let mut runs = response["result"]["running"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        runs.extend(
            response["result"]["ended"]["items"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
        if let Some(run) = runs.iter().find(|run| run["agentId"] == json!(agent_id)) {
            status = run["status"].as_str().unwrap_or_default().to_string();
            if status == "success" {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        status, "success",
        "the background agent must settle on its own"
    );

    let output_file = output["outputFile"]
        .as_str()
        .expect("outputFile")
        .to_string();
    let written = std::fs::read_to_string(&output_file).expect("output file");
    assert!(
        written.contains(CHILD_TEXT),
        "the child's final text must be on disk: {written}"
    );

    // Turn two: the model reads back the background sub-agent's result with TaskOutput.
    provider.reset_script(vec![
        tool_use_sse_with_id(
            "toolu-read-agent",
            "TaskOutput",
            &json!({"task_id": agent_id, "block": true, "timeout": 5000}),
        ),
        text_sse("read the result"),
    ]);
    send_text(
        &mut client,
        "cmd-read",
        &session_id,
        "read the background result",
    );
    let rows = poll_rows(&mut client, &session_id, "TaskOutput");
    let read_row = rows
        .iter()
        .find(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!("TaskOutput"))
        .cloned()
        .expect("task output row");
    assert_eq!(read_row["status"], json!("success"), "{read_row}");
    let read_output: Value =
        serde_json::from_str(read_row["output"]["text"].as_str().unwrap_or_default())
            .expect("task output json");
    assert_eq!(
        read_output["retrieval_status"],
        json!("success"),
        "{read_output}"
    );
    assert_eq!(read_output["task"]["task_type"], json!("local_agent"));
    assert_eq!(read_output["task"]["status"], json!("completed"));
    assert!(
        read_output["task"]["output"]
            .as_str()
            .unwrap_or_default()
            .contains(CHILD_TEXT),
        "{read_output}"
    );

    // Turn three: the model resumes the already-finished sub-agent with SendMessage; after the background run completes, the output file is overwritten.
    const RESUMED_TEXT: &str = "child did the follow-up work";
    provider.reset_script(vec![
        tool_use_sse_with_id(
            "toolu-send-message",
            "SendMessage",
            &json!({"to": agent_id, "summary": "follow up", "message": "do one more pass"}),
        ),
        text_sse(RESUMED_TEXT),
        text_sse(RESUMED_TEXT),
    ]);
    send_text(
        &mut client,
        "cmd-send",
        &session_id,
        "ask the agent to continue",
    );
    let rows = poll_rows(&mut client, &session_id, "SendMessage");
    let send_row = rows
        .iter()
        .find(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!("SendMessage"))
        .cloned()
        .expect("send message row");
    assert_eq!(send_row["status"], json!("success"), "{send_row}");
    let send_output: Value =
        serde_json::from_str(send_row["output"]["text"].as_str().unwrap_or_default())
            .expect("send message json");
    assert_eq!(send_output["success"], json!(true), "{send_output}");
    assert!(
        send_output["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Resumed agent"),
        "{send_output}"
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut resumed = String::new();
    while Instant::now() < deadline {
        resumed = std::fs::read_to_string(&output_file).unwrap_or_default();
        if resumed.contains(RESUMED_TEXT) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        resumed.contains(RESUMED_TEXT),
        "the resumed agent must overwrite the output file: {resumed}"
    );

    let _ = std::fs::remove_dir_all(&base_dir);
}

/// Named profile e2e: the project `.zcode/agents/reviewer.md` is loaded at host startup, and the `Agent(
/// subagent_type:"reviewer")` child request must carry only `Read` (the profile's tools whitelist), use
/// the profile's model and body; an unknown type names the available profiles in the error.
#[test]
fn a_named_profile_shapes_the_child_toolset_model_and_system_prompt() {
    const MARKER: &str = "PROFILE SYSTEM MARKER: review carefully.";
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse_with_id(
            "toolu-agent-profile",
            "Agent",
            &json!({
                "description": "review the change",
                "prompt": "review the diff under review",
                "subagent_type": "reviewer",
            }),
        ),
        text_sse("reviewed the code"),
        tool_use_sse_with_id(
            "toolu-agent-unknown",
            "Agent",
            &json!({
                "description": "unknown type",
                "prompt": "do something",
                "subagent_type": "nope",
            }),
        ),
        text_sse("final answer"),
    ]);
    let base_dir = temp_base("profile");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    let agents_dir = workspace.join(".zcode").join("agents");
    std::fs::create_dir_all(&agents_dir).expect("agents dir");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    std::fs::write(
        agents_dir.join("reviewer.md"),
        format!(
            "---\nname: reviewer\ndescription: reviews code changes\ntools: Read\nmodel: test-model\nmaxTurns: 3\n---\n{MARKER}\n"
        ),
    )
    .expect("profile");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, true);
    let mut client = client_for(&mut process);
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
    send_text(&mut client, "cmd-profile", &session_id, "review my change");

    let sid = format!("conn-1:conversation/{session_id}");
    let mut rows: Vec<Value> = Vec::new();
    let mut agent_rows: Vec<Value> = Vec::new();
    for _ in 0..800 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        apply_to_rows(&mut rows, &deltas_from_frame(&line));
        agent_rows = rows
            .iter()
            .filter(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!("Agent"))
            .cloned()
            .collect();
        if agent_rows.len() == 2
            && agent_rows
                .iter()
                .all(|row| matches!(row["status"].as_str(), Some("success") | Some("error")))
        {
            break;
        }
    }
    assert_eq!(agent_rows.len(), 2, "two Agent rows: {rows:?}");
    let profile_run = agent_rows
        .iter()
        .find(|row| row["input"]["subagent_type"] == json!("reviewer"))
        .expect("reviewer call row");
    assert_eq!(profile_run["status"], json!("success"), "{profile_run}");
    let profile_output: Value = serde_json::from_str(
        profile_run["output"]["text"]
            .as_str()
            .expect("tool output text"),
    )
    .expect("agent output json");
    assert_eq!(profile_output["agentType"], json!("reviewer"));

    let unknown = agent_rows
        .iter()
        .find(|row| row["input"]["subagent_type"] == json!("nope"))
        .expect("unknown call row");
    assert_eq!(unknown["status"], json!("error"), "{unknown}");
    let error_text = serde_json::to_string(unknown).expect("unknown row json");
    assert!(error_text.contains("agent_unknown_type"), "{unknown}");
    assert!(
        error_text.contains("general-purpose") && error_text.contains("Explore"),
        "the error must list available profiles: {unknown}"
    );

    // The child request's facts: the tool surface has only the profile's whitelist, system carries the profile's body, and the model is the resolved id.
    let bodies = provider.request_bodies();
    let child_body = bodies
        .iter()
        .filter_map(|body| serde_json::from_str::<Value>(body).ok())
        .find(|body| {
            body["tools"]
                .as_array()
                .is_some_and(|tools| tools.len() == 1 && tools[0]["name"] == json!("Read"))
        })
        .expect("the child request carries exactly the profile allowlist");
    assert!(
        child_body
            .to_string()
            .contains("review the diff under review"),
        "the child request carries the Agent prompt: {child_body}"
    );
    let tool_names: Vec<&str> = child_body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(tool_names, vec!["Read"], "{child_body}");
    assert!(
        child_body.to_string().contains(MARKER),
        "the profile body belongs in the child system prompt: {child_body}"
    );
    assert_eq!(child_body["model"], json!("test-model"), "{child_body}");

    let _ = std::fs::remove_dir_all(&base_dir);
}
