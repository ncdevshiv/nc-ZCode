//! e2e stdio：驱动真实二进制完成一个完整会话生命周期。
//!
//! 覆盖 S3 验收面：admission ack → 投影帧（snapshot/deltas）→ fail-closed 错误
//! 终态 → resync recovery snapshot 行集一致 → 进程重启后 resume 行不丢 → 幂等
//! 重提回放同一 ack。无 provider 配置（本 checkout 无凭据）：模型调用 fail-closed
//! 路径即是被测路径。

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

    fn stdin(&mut self) -> std::process::ChildStdin {
        self.child.stdin.take().expect("stdin")
    }

    fn stdout(&mut self) -> BufReader<std::process::ChildStdout> {
        BufReader::new(self.child.stdout.take().expect("stdout"))
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 一个 clientMode 连接：写请求、读下一行协议输出。
/// 通知帧可能先于任何响应到达（turn 帧与响应共管），response 查找时把非目标行
/// 缓存进 pending，绝不吞帧。
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
        self.stdin
            .write_all(line.as_bytes())
            .expect("write request");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush");
        id
    }

    /// 读下一行协议输出（pending 优先）。协议保证每个请求都有响应；stdout 静默
    /// = 缺陷，直接失败。
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
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zcode-host-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn command_envelope(
    command_type: &str,
    command_id: &str,
    session_id: Option<&str>,
    payload: Value,
) -> Value {
    json!({
        "type": command_type,
        "commandId": command_id,
        "clientId": "e2e-client",
        "sessionId": session_id,
        "payload": payload,
        "issuedAt": 1_700_000_000_000.0,
    })
}

fn rows_from_frame(line: &Value) -> Vec<Value> {
    let payload = &line["params"]["frame"]["payload"];
    match payload["kind"].as_str() {
        Some("snapshot") => payload["snapshot"]["rows"]["window"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        Some("deltas") => Vec::new(),
        _ => Vec::new(),
    }
}

fn deltas_from_frame(line: &Value) -> Vec<Value> {
    let payload = &line["params"]["frame"]["payload"];
    match payload["kind"].as_str() {
        Some("deltas") => payload["deltas"].as_array().cloned().unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn apply_to_rows(rows: &mut Vec<Value>, deltas: &[Value]) {
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
                    if let Some(slot) = rows
                        .iter_mut()
                        .find(|candidate| row_id.is_some() && candidate["rowId"].as_u64() == row_id)
                    {
                        *slot = row.clone();
                    }
                }
            }
            _ => {}
        }
    }
}

#[test]
fn full_session_lifecycle_over_stdio() {
    let data_dir = temp_dir("lifecycle");
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.stdin();
    let stdout = process.stdout();
    let mut client = Client {
        id_counter: 0,
        stdin,
        stdout,
        pending: std::collections::VecDeque::new(),
    };

    // 1) createSession（sessionId: null，global bucket）
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));
    let session_id = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // 2) subscribe（continuous profile）→ ack + initial snapshot frame
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["ack"]["mode"], json!("snapshot"));
    let snapshot_line = client.next_line();
    assert_eq!(snapshot_line["method"], json!("v4/conversation/frame"));
    assert_eq!(snapshot_line["params"]["deliveryKind"], json!("initial"));
    let mut rows = rows_from_frame(&snapshot_line);
    assert!(rows.is_empty(), "fresh session has no rows");

    // 3) sendText → admission ack + turn 投影帧（fail-closed：无 provider 配置）
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send-1",
            Some(&session_id),
            json!({"text": "hello native host"}),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));

    // 逐帧 apply：每帧的 delta 只应用一次（row.appended 重复 apply 会重复入集）。
    let mut saw_last_error = false;
    loop {
        let line = client.next_line();
        let frame_deltas = deltas_from_frame(&line);
        for delta in &frame_deltas {
            if delta["op"] == json!("state.updated") {
                let code = delta["patch"]["control"]["lastError"]["code"].as_str();
                if code == Some("fault.model.providerNotConfigured") {
                    saw_last_error = true;
                }
            }
        }
        apply_to_rows(&mut rows, &frame_deltas);
        // turnHeader failed + userInput + lastError 均已入集 → 轮次结束。
        let has_user = rows.iter().any(|row| {
            row["kind"] == json!("userInput") && row["text"] == json!("hello native host")
        });
        let header_failed = rows
            .iter()
            .any(|row| row["kind"] == json!("turnHeader") && row["state"] == json!("failed"));
        if has_user && header_failed && saw_last_error {
            break;
        }
        assert!(
            rows.len() < 50,
            "projection did not converge; rows so far: {}",
            serde_json::to_string_pretty(&rows).unwrap()
        );
    }
    assert!(rows.iter().any(|row| row["kind"] == json!("turnHeader")));
    assert!(rows.iter().any(|row| row["kind"] == json!("userInput")));
    assert!(rows.iter().any(|row| row["kind"] == json!("assistantText")));
    let assistant = rows
        .iter()
        .find(|row| row["kind"] == json!("assistantText"))
        .expect("assistant row");
    assert_eq!(assistant["state"], json!("failed"));
    // 失败不得伪造模型正文：错误只进 sessionErrorInfo，assistant 正文保持空。
    assert_eq!(assistant["text"], json!(""));
    assert!(
        saw_last_error,
        "fail-closed error must surface as sessionErrorInfo, not be swallowed"
    );

    // 4) resync → recovery snapshot 必须与当前行集一致（终态一致性）
    let id = client.send(
        "v4/conversation/resync",
        json!({"sessionId": session_id, "connectionId": "conn-1"}),
    );
    let _ = client.response(id);
    let recovery = client.next_line();
    assert_eq!(recovery["params"]["deliveryKind"], json!("recovery"));
    let recovered = rows_from_frame(&recovery);
    assert_eq!(
        recovered.len(),
        rows.len(),
        "recovery snapshot must carry the same row set"
    );
    for row in &recovered {
        assert!(
            rows.iter().any(|candidate| candidate == row),
            "recovery row mismatch: {row}"
        );
    }

    // 5) 幂等重提：同 commandId 重发 sendText → 回放已持久化的终态。失败命令按
    //    inbox 的 retry_ack 语义原样返回 failed（不是 duplicate，见 zcode-core）。
    let rows_before = rows.len();
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send-1",
            Some(&session_id),
            json!({"text": "hello native host"}),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("failed"));
    assert_eq!(
        response["result"]["reasonCode"],
        json!("fault.model.providerNotConfigured")
    );

    // 6) rename → title 落 meta
    let id = client.send(
        "v4/command",
        command_envelope(
            "renameSession",
            "cmd-rename-1",
            Some(&session_id),
            json!({"title": "native e2e"}),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));

    // 读干净缓冲（无订阅者后 stop 等仍应有响应）：stop 空闲会话 = noop
    let id = client.send(
        "v4/command",
        command_envelope("stop", "cmd-stop-1", Some(&session_id), json!({})),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("noop"));

    drop(client);
    drop(process);

    // 7) 重启进程 → resume：行与幂等事实必须恢复
    let mut process = HostProcess::spawn(&data_dir);
    let stdin = process.stdin();
    let stdout = process.stdout();
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
    let response = client.response(id);
    assert_eq!(response["result"]["ack"]["mode"], json!("snapshot"));
    let snapshot_line = client.next_line();
    let resumed_rows = rows_from_frame(&snapshot_line);
    assert_eq!(
        resumed_rows.len(),
        rows_before,
        "resume must restore every projected row"
    );
    assert!(
        resumed_rows
            .iter()
            .any(|row| row["kind"] == json!("userInput")),
        "resumed transcript must contain the user input"
    );

    // resume 后同 commandId 再提 → 回放持久化的 failed 终态（transcript 回源）
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send-1",
            Some(&session_id),
            json!({"text": "hello native host"}),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("failed"));
    assert_eq!(
        response["result"]["reasonCode"],
        json!("fault.model.providerNotConfigured")
    );

    // meta 持久化校验（title 跨重启存活）
    let meta_raw = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(&session_id)
            .join("meta.json"),
    )
    .expect("meta.json");
    let meta: Value = serde_json::from_str(&meta_raw).expect("meta json");
    assert_eq!(meta["title"], json!("native e2e"));

    let _ = std::fs::remove_dir_all(&data_dir);
}
