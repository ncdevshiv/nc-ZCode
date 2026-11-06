//! Local TTFT facts e2e: real binary + scripted provider.
//!
//! Asserts that the `v4/telemetry/local-ttft` notification really carries the **measured** stage timestamps (receipt →
//! admission → execution → request → first output → terminal state), and that:
//! - with no `ttft` context, not a single fact is emitted (not observing = not emitting fake facts);
//! - the stage timestamps are monotonic and never go backwards;
//! - `observationId`/`commandId` match the command (the renderer aligns on these two keys).

mod common;

use common::{text_sse, HostProcess, ScriptedResponder};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

struct Reader {
    stdin: std::process::ChildStdin,
    rx: std::sync::mpsc::Receiver<Value>,
    next_id: u64,
}

impl Reader {
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let line = serde_json::to_string(
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )
        .expect("serialize");
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .expect("write");
        id
    }

    fn recv(&self, what: &str) -> Value {
        self.rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| panic!("{what} never arrived: {error}"))
    }

    fn response(&mut self, id: u64, what: &str) -> Value {
        loop {
            let line = self.recv(what);
            if line.get("id").and_then(Value::as_u64) == Some(id) {
                return line;
            }
        }
    }
}

fn boot(script: Vec<String>, tag: &str) -> (HostProcess, Reader, PathBuf) {
    let (responder, port) = ScriptedResponder::spawn(script);
    std::mem::forget(responder);
    let base = std::env::temp_dir().join(format!("zcode-host-ttft-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base.join("provider.json");
    provider_config(port, &config_path);
    let mut process = HostProcess::spawn(&workspace, data_dir.as_path(), &config_path, false);
    let stdin = process.stdin();
    let stdout = process.stdout();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in stdout.lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if tx.send(parsed).is_err() {
                return;
            }
        }
    });
    (
        process,
        Reader {
            stdin,
            rx,
            next_id: 0,
        },
        base,
    )
}

fn create_session(reader: &mut Reader) -> String {
    let id = reader.send(
        "v4/command",
        json!({
            "type": "createSession",
            "commandId": "create-1",
            "clientId": "e2e",
            "sessionId": Value::Null,
            "payload": {},
            "issuedAt": 1_700_000_000_000.0,
        }),
    );
    let response = reader.response(id, "createSession response");
    response["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

#[test]
fn observed_command_emits_measured_stage_facts() {
    let observation_id = "6f1d2a3b-4c5d-4e6f-8a9b-0c1d2e3f4a5b";
    let (_process, mut reader, base) = boot(vec![text_sse("a measured answer")], "observed");
    let session_id = create_session(&mut reader);

    let id = reader.send(
        "v4/command",
        json!({
            "type": "sendText",
            "commandId": "cmd-ttft-1",
            "clientId": "e2e",
            "sessionId": session_id,
            "payload": {"text": "hello"},
            "issuedAt": 1_700_000_000_000.0,
            "ttft": {"version": 1, "observationId": observation_id},
        }),
    );
    let ack = reader.response(id, "sendText ack");
    assert_eq!(ack["result"]["status"], json!("accepted"));

    // Collect until the terminal fact (the checkpoints of one and the same record increase by revision).
    let mut facts: Vec<Value> = Vec::new();
    for _ in 0..400 {
        let line = reader.recv("ttft notification");
        if line["method"] != json!("v4/telemetry/local-ttft") {
            continue;
        }
        let params = line["params"].clone();
        assert_eq!(params["observationId"], json!(observation_id));
        assert_eq!(params["commandId"], json!("cmd-ttft-1"));
        assert_eq!(params["version"], json!(1));
        assert!(
            params["instanceId"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "instanceId must be the real instance ID of host: {params}"
        );
        facts.push(params);
        if facts
            .last()
            .is_some_and(|fact| fact.get("terminal").is_some())
        {
            break;
        }
    }
    assert!(
        !facts.is_empty(),
        "observed command must produce checkpoints"
    );
    let last = facts.last().expect("at least one fact");
    assert_eq!(last["terminal"], json!("completed"));
    assert_eq!(last["sessionId"], json!(session_id));
    assert_eq!(last["sendMode"], json!("idle"));
    assert_eq!(last["model"], json!("test-model"));
    let received = last["receivedAt"].as_u64().expect("receivedAt");
    let admitted = last["admittedAt"].as_u64().expect("admittedAt");
    let execution = last["executionAt"].as_u64().expect("executionAt");
    let request = last["requestAt"].as_u64().expect("requestAt");
    let output = last["outputAt"].as_u64().expect("outputAt");
    assert!(
        received <= admitted && admitted <= execution && execution <= request && request <= output,
        "stage timestamps must be monotonic: {last}"
    );
    assert_eq!(last["outputKind"], json!("text"));
    assert_eq!(last["requestId"], last["details"][0]["requestId"]);
    // revision only moves forward on real change (multiple checkpoints = multiple stages observed).
    assert!(
        last["revision"].as_u64().unwrap_or(0) >= 4,
        "expected stage-by-stage checkpoints, got {last}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn unobserved_command_emits_no_facts() {
    // No ttft context = not observed: not a single fact may be emitted (emitting one would be fabricating an alignment key).
    let (_process, mut reader, base) = boot(vec![text_sse("quiet answer")], "unobserved");
    let session_id = create_session(&mut reader);
    // Only a subscription yields frames (consistent with a real client); what this test waits for is exactly the terminal row of "this turn finished".
    let id = reader.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-ttft", "clientMode": "desktop-continuous"}),
    );
    let _ = reader.response(id, "subscribe ack");
    let id = reader.send(
        "v4/command",
        json!({
            "type": "sendText",
            "commandId": "cmd-ttft-2",
            "clientId": "e2e",
            "sessionId": session_id,
            "payload": {"text": "hello"},
            "issuedAt": 1_700_000_000_000.0,
        }),
    );
    let ack = reader.response(id, "sendText ack");
    assert_eq!(ack["result"]["status"], json!("accepted"));
    // Wait until the turn really ran to completion (a terminal state in the session frames), then confirm the pipe contains no local-ttft notification.
    for _ in 0..200 {
        let line = reader.recv("frames after unobserved turn");
        let payload = &line["params"]["frame"]["payload"];
        if payload["kind"] == json!("deltas") {
            let settled = payload["deltas"].as_array().is_some_and(|deltas| {
                deltas.iter().any(|delta| {
                    delta["row"]["kind"] == json!("turnHeader")
                        && delta["row"]["state"] == json!("completedSuccess")
                })
            });
            if settled {
                break;
            }
        }
    }
    // Give in-flight notifications some arrival time, then confirm the pipe contains no local-ttft notification at all.
    std::thread::sleep(Duration::from_millis(150));
    while let Ok(line) = reader.rx.try_recv() {
        assert_ne!(
            line["method"],
            json!("v4/telemetry/local-ttft"),
            "unobserved command must not emit facts: {line}"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}
