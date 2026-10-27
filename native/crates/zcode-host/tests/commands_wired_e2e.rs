//! e2e stdio：已接线命令的状态投影面。
//!
//! 覆盖 `setFollowupMode` / `switchCollaborationMode` / `switchModelConfig` /
//! `setAutoDrain`：这些命令不是纯 settle——必须向订阅者投影 `state.updated`
//! 帧，patch 携带对应字段（config.followupMode / config.mode / config.provider 等
//! 与 queue.autoDrain）。断言「真的从 stdio 面上能看到状态变化」，而不只是 ack。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde_json::{json, Value};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

struct HostProcess {
    child: Child,
}

impl HostProcess {
    fn spawn(data_dir: &std::path::Path) -> Self {
        let bin = env!("CARGO_BIN_EXE_zcode-host");
        let mut command = Command::new(bin);
        command
            .arg("--workspace")
            .arg(env!("CARGO_MANIFEST_DIR"))
            .arg("--data-dir")
            .arg(data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let child = command.spawn().expect("spawn zcode-host");
        Self { child }
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    id_counter: u64,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    pending: std::collections::VecDeque<Value>,
}

impl Client {
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.id_counter += 1;
        let id = self.id_counter;
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let line = serde_json::to_string(&request).expect("request");
        self.stdin.write_all(line.as_bytes()).expect("write");
        self.stdin.write_all(b"\n").expect("newline");
        self.stdin.flush().expect("flush");
        id
    }

    fn next_line(&mut self) -> Value {
        if let Some(buffered) = self.pending.pop_front() {
            return buffered;
        }
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read line");
            assert!(read > 0, "stdout closed before a protocol line arrived");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return serde_json::from_str(trimmed).expect("protocol line must be JSON");
        }
    }

    fn response(&mut self, id: u64) -> Value {
        // 先取 pending 再读新行；读到的通知帧按原序放回，**绝不能**把刚弹出的帧再塞回
        // 队尾（next_line 会再次弹出它 → 死循环）。
        let mut skipped = std::collections::VecDeque::new();
        loop {
            let message = match self.pending.pop_front() {
                Some(buffered) => buffered,
                None => self.next_line(),
            };
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                for line in skipped.into_iter().rev() {
                    self.pending.push_front(line);
                }
                return message;
            }
            skipped.push_back(message);
        }
    }

    /// 读到下一个 deltas 帧的 delta 列表（跳过 snapshot/其他帧）。
    fn next_deltas(&mut self) -> Vec<Value> {
        loop {
            let line = self.next_line();
            let payload = &line["params"]["frame"]["payload"];
            if payload["kind"] == json!("deltas") {
                return payload["deltas"].as_array().cloned().unwrap_or_default();
            }
        }
    }
}

fn command_envelope(
    command_type: &str,
    command_id: &str,
    session_id: &str,
    base_revision: i64,
    payload: Value,
) -> Value {
    // setFollowupMode/switchModelConfig/switchCollaborationMode/setAutoDrain 都在
    // COMMANDS_REQUIRING_BASE_REVISION 里（CAS 命令）→ 必须带 baseRevision，且
    // 配置变更会推进 revision，后续命令必须带新值。
    json!({
        "type": command_type,
        "commandId": command_id,
        "clientId": "e2e-client",
        "sessionId": session_id,
        "baseRevision": base_revision,
        "payload": payload,
        "issuedAt": 1_700_000_000_000.0,
    })
}

fn find_state_patch(deltas: &[Value], predicate: impl Fn(&Value) -> bool) -> Option<Value> {
    deltas
        .iter()
        .find(|delta| delta["op"] == json!("state.updated") && predicate(&delta["patch"]))
        .map(|delta| delta["patch"].clone())
}

#[test]
fn wired_state_commands_project_real_deltas() {
    let data_dir = std::env::temp_dir().join(format!("zcode-host-wired-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.child.stdin.take().expect("stdin");
    let stdout = BufReader::new(process.child.stdout.take().expect("stdout"));
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };

    // createSession + subscribe
    let id = client.send(
        "v4/command",
        json!({
            "type": "createSession", "commandId": "c-create", "clientId": "e2e",
            "sessionId": Value::Null, "payload": {}, "issuedAt": 1_700_000_000_000.0
        }),
    );
    let response = client.response(id);
    let session_id = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot

    // 配置变更推进 revision（CAS）：每次命令后从 patch 取新 revision 供下一条使用。
    let mut rev = 0i64;

    // setFollowupMode → config.followupMode = guide
    let id = client.send(
        "v4/command",
        command_envelope(
            "setFollowupMode",
            "c-follow",
            &session_id,
            rev,
            json!({"mode": "guide"}),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));
    let patch = find_state_patch(&client.next_deltas(), |p| p.get("config").is_some())
        .expect("setFollowupMode must project a config patch");
    assert_eq!(patch["config"]["followupMode"], json!("guide"));
    rev = patch["revision"].as_i64().expect("patch revision");

    // switchCollaborationMode → config.mode = plan
    let id = client.send(
        "v4/command",
        command_envelope(
            "switchCollaborationMode",
            "c-mode",
            &session_id,
            rev,
            json!({"mode": "plan"}),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));
    let patch = find_state_patch(&client.next_deltas(), |p| p.get("config").is_some())
        .expect("switchCollaborationMode must project a config patch");
    assert_eq!(patch["config"]["mode"], json!("plan"));
    // followupMode 保留（config 是全量投影，不是稀疏增量）
    assert_eq!(patch["config"]["followupMode"], json!("guide"));
    rev = patch["revision"].as_i64().expect("patch revision");

    // switchModelConfig → config.provider/model/thought
    let id = client.send(
        "v4/command",
        command_envelope(
            "switchModelConfig",
            "c-model",
            &session_id,
            rev,
            json!({"provider": "anthropic", "model": "claude", "thought": "high"}),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));
    let patch = find_state_patch(&client.next_deltas(), |p| p.get("config").is_some())
        .expect("switchModelConfig must project a config patch");
    assert_eq!(patch["config"]["provider"], json!("anthropic"));
    assert_eq!(patch["config"]["model"], json!("claude"));
    assert_eq!(patch["config"]["thought"], json!("high"));
    rev = patch["revision"].as_i64().expect("patch revision");

    // setAutoDrain → queue.autoDrain = false
    let id = client.send(
        "v4/command",
        command_envelope(
            "setAutoDrain",
            "c-drain",
            &session_id,
            rev,
            json!({"autoDrain": false}),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));
    let patch = find_state_patch(&client.next_deltas(), |p| p.get("queue").is_some())
        .expect("setAutoDrain must project a queue patch");
    assert_eq!(patch["queue"]["autoDrain"], json!(false));

    // 重启后 config 必须在 snapshot 中保留（状态不只在 delta 里）
    drop(client);
    drop(process);
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.child.stdin.take().expect("stdin");
    let stdout = BufReader::new(process.child.stdout.take().expect("stdout"));
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-2", "clientMode": "web-remote-replayable"}),
    );
    let _ = client.response(id);
    let snapshot = client.next_line();
    let config = &snapshot["params"]["frame"]["payload"]["snapshot"]["config"];
    // config 持久化到 meta.json，冷恢复必须原样带回（否则命令效果跨重启丢失）。
    assert_eq!(config["followupMode"], json!("guide"));
    assert_eq!(config["mode"], json!("plan"));
    assert_eq!(config["provider"], json!("anthropic"));
    assert_eq!(config["model"], json!("claude"));
    assert_eq!(config["thought"], json!("high"));

    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 把 deltas 应用到行集（row.appended 追加；row.upserted 按 rowId 就地替换）。
fn apply_rows(rows: &mut Vec<Value>, deltas: &[Value]) {
    for delta in deltas {
        match delta["op"].as_str() {
            Some("row.appended") => {
                if let Some(row) = delta.get("row") {
                    rows.push(row.clone());
                }
            }
            Some("row.upserted") => {
                if let Some(row) = delta.get("row") {
                    let row_id = row["rowId"].as_u64();
                    match rows
                        .iter_mut()
                        .find(|candidate| row_id.is_some() && candidate["rowId"].as_u64() == row_id)
                    {
                        Some(slot) => *slot = row.clone(),
                        // upsert 到不存在的行 = 追加（首次投影可能走 upsert 路径）。
                        None => rows.push(row.clone()),
                    }
                }
            }
            _ => {}
        }
    }
}

#[test]
fn assistant_feedback_command_upserts_row_and_fails_closed_on_bad_target() {
    let data_dir = std::env::temp_dir().join(format!("zcode-host-feedback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.child.stdin.take().expect("stdin");
    let stdout = BufReader::new(process.child.stdout.take().expect("stdout"));
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };

    let id = client.send(
        "v4/command",
        json!({
            "type": "createSession", "commandId": "c-create", "clientId": "e2e",
            "sessionId": Value::Null, "payload": {}, "issuedAt": 1_700_000_000_000.0
        }),
    );
    let response = client.response(id);
    let session_id = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let snapshot = client.next_line();
    let snapshot = &snapshot["params"]["frame"]["payload"]["snapshot"];
    let log_epoch = snapshot["logEpoch"].as_str().expect("logEpoch").to_string();
    let mut rows: Vec<Value> = snapshot["rows"]["window"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    // sendText（无 provider → fail-closed）产生 assistantText 行。
    let id = client.send(
        "v4/command",
        json!({
            "type": "sendText", "commandId": "c-send", "clientId": "e2e",
            "sessionId": session_id, "payload": {"text": "hi"}, "issuedAt": 1_700_000_000_000.0
        }),
    );
    let _ = client.response(id);
    let assistant_row_id = loop {
        let deltas = client.next_deltas();
        apply_rows(&mut rows, &deltas);
        // 等 turn 真正收口（turnHeader 终态）再发下一条命令：否则 admission 会
        // 排在仍在跑的 turn 后面。
        let header_done = rows
            .iter()
            .any(|row| row["kind"] == json!("turnHeader") && row["state"] != json!("running"));
        if header_done {
            if let Some(row) = rows
                .iter()
                .find(|row| row["kind"] == json!("assistantText"))
            {
                break row["rowId"].as_u64().expect("rowId");
            }
        }
        assert!(rows.len() < 50, "no assistantText row projected");
    };

    // 失败 turn 推进 revision 到 1。
    let rev = 1i64;
    let feedback_envelope = |command_id: &str, target_row_id: u64, feedback: Value| {
        json!({
            "type": "setAssistantFeedback", "commandId": command_id, "clientId": "e2e",
            "sessionId": session_id,
            "baseRevision": rev, "baseLogEpoch": log_epoch,
            "payload": {"target": {"rowId": target_row_id, "entityId": "resp-c-send"}, "feedback": feedback},
            "issuedAt": 1_700_000_000_000.0
        })
    };

    // like → row.upserted 携带 feedback
    let id = client.send(
        "v4/command",
        feedback_envelope("c-fb-like", assistant_row_id, json!("like")),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));
    let deltas = client.next_deltas();
    apply_rows(&mut rows, &deltas);
    let row = rows
        .iter()
        .find(|row| row["rowId"] == json!(assistant_row_id))
        .expect("assistant row");
    assert_eq!(row["feedback"], json!("like"));

    // null → 清除反馈
    let id = client.send(
        "v4/command",
        feedback_envelope("c-fb-clear", assistant_row_id, Value::Null),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));
    let deltas = client.next_deltas();
    apply_rows(&mut rows, &deltas);
    let row = rows
        .iter()
        .find(|row| row["rowId"] == json!(assistant_row_id))
        .expect("assistant row");
    assert!(row.get("feedback").is_none(), "feedback must be cleared");

    // 不存在的 rowId → 首响应是 admission ack（accepted），终态经幂等重提可观察。
    let id = client.send(
        "v4/command",
        feedback_envelope("c-fb-bad", 9999, json!("like")),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));
    // 重提同 commandId → 回放已持久化的 failed 终态。
    let id = client.send(
        "v4/command",
        feedback_envelope("c-fb-bad", 9999, json!("like")),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("failed"));
    assert_eq!(
        response["result"]["reasonCode"],
        json!("fault.command.executionFailed")
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn stale_stop_with_expected_execution_id_is_a_noop_guard() {
    let data_dir =
        std::env::temp_dir().join(format!("zcode-host-stopguard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.child.stdin.take().expect("stdin");
    let stdout = BufReader::new(process.child.stdout.take().expect("stdout"));
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };
    let id = client.send(
        "v4/command",
        json!({
            "type": "createSession", "commandId": "c-create", "clientId": "e2e",
            "sessionId": Value::Null, "payload": {}, "issuedAt": 1_700_000_000_000.0
        }),
    );
    let response = client.response(id);
    let session_id = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // 空闲 + 携带 expectedForegroundExecutionId → 迟到 stop，必须 guard.stopTargetChanged。
    let id = client.send(
        "v4/command",
        json!({
            "type": "stop", "commandId": "c-stop-stale", "clientId": "e2e",
            "sessionId": session_id,
            "payload": {"expectedForegroundExecutionId": "some-old-execution"},
            "issuedAt": 1_700_000_000_000.0
        }),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("noop"));
    assert_eq!(
        response["result"]["reasonCode"],
        json!("guard.stopTargetChanged")
    );

    // 空闲 + 无 expected → 仍是 guard.nothingToStop。
    let id = client.send(
        "v4/command",
        json!({
            "type": "stop", "commandId": "c-stop-idle", "clientId": "e2e",
            "sessionId": session_id, "payload": {},
            "issuedAt": 1_700_000_000_000.0
        }),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("noop"));
    assert_eq!(
        response["result"]["reasonCode"],
        json!("guard.nothingToStop")
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn delete_session_closes_runtime_but_retains_history() {
    let data_dir = std::env::temp_dir().join(format!("zcode-host-delete-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.child.stdin.take().expect("stdin");
    let stdout = BufReader::new(process.child.stdout.take().expect("stdout"));
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };

    // create + 产生历史（sendText 无 provider 也会投影行并落 transcript）
    let id = client.send(
        "v4/command",
        json!({
            "type": "createSession", "commandId": "c-create", "clientId": "e2e",
            "sessionId": Value::Null, "payload": {}, "issuedAt": 1_700_000_000_000.0
        }),
    );
    let response = client.response(id);
    let session_id = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot
    let id = client.send(
        "v4/command",
        json!({
            "type": "sendText", "commandId": "c-send", "clientId": "e2e",
            "sessionId": session_id, "payload": {"text": "keep me"}, "issuedAt": 1_700_000_000_000.0
        }),
    );
    let _ = client.response(id);
    let mut rows: Vec<Value> = Vec::new();
    loop {
        let deltas = client.next_deltas();
        apply_rows(&mut rows, &deltas);
        if rows
            .iter()
            .any(|row| row["kind"] == json!("turnHeader") && row["state"] != json!("running"))
        {
            break;
        }
        assert!(rows.len() < 50, "turn did not converge");
    }

    // deleteSession = close，不是真删
    let id = client.send(
        "v4/command",
        json!({
            "type": "deleteSession", "commandId": "c-delete", "clientId": "e2e",
            "sessionId": session_id, "payload": {}, "issuedAt": 1_700_000_000_000.0
        }),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));

    // 关闭后 runtime 不再认识该 session
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-2", "clientMode": "desktop-continuous"}),
    );
    let response = client.response(id);
    assert_eq!(
        response["error"]["data"]["reasonCode"],
        json!("proto.sessionNotFound")
    );

    // 但 transcript 必须还在（TS：历史仍留在库里）
    let transcript = data_dir
        .join("sessions")
        .join(&session_id)
        .join("transcript.jsonl");
    assert!(
        transcript.exists(),
        "deleteSession must not erase the transcript (close, not purge)"
    );

    // 重启后历史仍可恢复（resume 重新注册该 session）
    drop(client);
    drop(process);
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.child.stdin.take().expect("stdin");
    let stdout = BufReader::new(process.child.stdout.take().expect("stdout"));
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-3", "clientMode": "web-remote-replayable"}),
    );
    let response = client.response(id);
    assert!(
        response.get("error").is_none(),
        "restart must resume the closed session's history: {response}"
    );
    let snapshot = client.next_line();
    let resumed = snapshot["params"]["frame"]["payload"]["snapshot"]["rows"]["window"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        resumed.iter().any(|row| row["kind"] == json!("userInput")),
        "resumed history must contain the retained user input"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn rename_session_projects_meta_delta_to_subscribers() {
    let data_dir = std::env::temp_dir().join(format!("zcode-host-rename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.child.stdin.take().expect("stdin");
    let stdout = BufReader::new(process.child.stdout.take().expect("stdout"));
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };
    let id = client.send(
        "v4/command",
        json!({
            "type": "createSession", "commandId": "c-create", "clientId": "e2e",
            "sessionId": Value::Null, "payload": {}, "issuedAt": 1_700_000_000_000.0
        }),
    );
    let response = client.response(id);
    let session_id = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot

    let id = client.send(
        "v4/command",
        json!({
            "type": "renameSession", "commandId": "c-rename", "clientId": "e2e",
            "sessionId": session_id, "payload": {"title": "Renamed Session"},
            "issuedAt": 1_700_000_000_000.0
        }),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));
    let patch = find_state_patch(&client.next_deltas(), |p| p.get("meta").is_some())
        .expect("renameSession must project a meta patch");
    assert_eq!(patch["meta"]["title"], json!("Renamed Session"));
    assert_eq!(patch["meta"]["titleSource"], json!("custom"));
    // revision 必须前移（配置/元信息变更参与 CAS）
    assert!(patch["revision"].as_i64().unwrap_or(0) >= 1);

    let _ = std::fs::remove_dir_all(&data_dir);
}
