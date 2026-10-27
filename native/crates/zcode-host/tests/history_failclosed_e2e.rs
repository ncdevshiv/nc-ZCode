//! 会话记忆 fail-closed 的**线上**证据（review round2 要求）。
//!
//! round2 用真实二进制证明了两个缺陷，它们**按构造就是单测看不见的**：
//!
//! - `FAULT_HISTORY_NOT_PERSISTED` 被当成 `AdapterError` 的 **message** 传入，而 settle
//!   映射按 `error.code` 选 fault，于是它被 `FAULT_MODEL_FAILED` 覆盖、永远到不了
//!   `reasonCode`。单测断言「`append_history` 返回 Err」看不到这一层。
//! - `transcript.jsonl` **整份缺失**时 `load_transcript` 直接 `Err`，`unwrap_or_default()`
//!   给出 `skipped_lines` 为空的默认值，于是降级诊断不响——同一个静默失忆的另一扇门。
//!
//! 所以这里只断言**协议上真实可见的东西**：settled ack 的 `reasonCode`、host stderr
//! 的降级诊断。provider double 活在测试进程里，产品代码零测试替身。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn final_text_sse(text: &str) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{{\"input_tokens\":5,\"output_tokens\":1}}}}}}\n\n\
         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":2}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

struct ScriptedProvider {
    bodies: Arc<Mutex<Vec<String>>>,
}

impl ScriptedProvider {
    fn spawn() -> (Self, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_for_thread = Arc::clone(&bodies);
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_thread = Arc::clone(&counter);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let bodies = Arc::clone(&bodies_for_thread);
                let counter = Arc::clone(&counter_for_thread);
                thread::spawn(move || handle_connection(&mut stream, bodies, counter));
            }
        });
        (Self { bodies }, port)
    }

    fn request_count(&self) -> usize {
        self.bodies.lock().map(|bodies| bodies.len()).unwrap_or(0)
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    bodies: Arc<Mutex<Vec<String>>>,
    counter: Arc<AtomicUsize>,
) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let trimmed = header.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        bodies
            .lock()
            .expect("bodies lock")
            .push(String::from_utf8_lossy(&body).to_string());
        let index = counter.fetch_add(1, Ordering::SeqCst);
        let payload = final_text_sse(&format!("reply {index}"));
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{}",
            payload.len(),
            payload
        );
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
    }
}

/// 收集 stderr 的线程：降级诊断只走 stderr，所以必须真读它。
fn drain_stderr(stderr: ChildStderr, sink: Arc<Mutex<String>>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(mut guard) = sink.lock() {
                guard.push_str(&line);
                guard.push('\n');
            }
        }
    })
}

struct HostProcess {
    child: Child,
}

impl HostProcess {
    fn spawn(
        workspace: &Path,
        data_dir: &Path,
        provider_config: &Path,
        stderr_sink: Arc<Mutex<String>>,
    ) -> Self {
        let bin = env!("CARGO_BIN_EXE_zcode-host");
        let mut command = Command::new(bin);
        command
            .arg("--workspace")
            .arg(workspace)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--provider-config")
            .arg(provider_config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let mut child = command.spawn().expect("spawn zcode-host");
        let stderr = child.stderr.take().expect("stderr");
        // 句柄移交给读线程；JoinHandle 故意丢弃（进程结束时它自然结束）。
        drop(drain_stderr(stderr, stderr_sink));
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

struct Client {
    id_counter: u64,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    pending: std::collections::VecDeque<Value>,
}

impl Client {
    fn new(process: &mut HostProcess) -> Self {
        Self {
            id_counter: 0,
            stdin: process.stdin(),
            stdout: process.stdout(),
            pending: std::collections::VecDeque::new(),
        }
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.id_counter += 1;
        let id = self.id_counter;
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let line = serde_json::to_string(&request).expect("request");
        self.stdin.write_all(line.as_bytes()).expect("write");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush");
        id
    }

    fn next_line(&mut self) -> Value {
        if let Some(buffered) = self.pending.pop_front() {
            return buffered;
        }
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read");
            assert!(read > 0, "stdout closed early");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return serde_json::from_str(trimmed).expect("json line");
        }
    }

    fn response(&mut self, id: u64) -> Value {
        loop {
            let message = self.next_line();
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message;
            }
            self.pending.push_back(message);
        }
    }

    fn create_session(&mut self, command_id: &str) -> String {
        let id = self.send(
            "v4/command",
            json!({
                "type": "createSession",
                "commandId": command_id,
                "clientId": "e2e",
                "sessionId": null,
                "payload": {},
                "issuedAt": 1_700_000_000_000.0,
            }),
        );
        self.response(id)["result"]["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string()
    }

    fn subscribe(&mut self, session_id: &str, connection_id: &str) {
        let id = self.send(
            "v4/conversation/subscribe",
            json!({
                "sessionId": session_id,
                "connectionId": connection_id,
                "clientMode": "desktop-continuous",
            }),
        );
        let _ = self.response(id);
        let _ = self.next_line(); // initial/recovery snapshot frame
    }

    /// 发 sendText 并读到该轮 settle（turnHeader 离开 running）。
    fn run_turn(&mut self, session_id: &str, command_id: &str, text: &str) -> Value {
        let id = self.send(
            "v4/command",
            json!({
                "type": "sendText",
                "commandId": command_id,
                "clientId": "e2e",
                "sessionId": session_id,
                "payload": {"text": text},
                "issuedAt": 1_700_000_000_000.0,
            }),
        );
        let response = self.response(id);
        assert_eq!(
            response["result"]["status"],
            json!("accepted"),
            "turn must be admitted: {response}"
        );
        let mut rows: Vec<Value> = Vec::new();
        for _ in 0..40 {
            let line = self.next_line();
            let deltas = line["params"]["frame"]["payload"]["deltas"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for delta in &deltas {
                match delta["op"].as_str() {
                    Some("row.appended") => rows.push(delta["row"].clone()),
                    Some("row.upserted") => {
                        let row_id = delta["row"]["rowId"].as_u64();
                        if let Some(slot) = rows
                            .iter_mut()
                            .find(|row| row_id.is_some() && row["rowId"].as_u64() == row_id)
                        {
                            *slot = delta["row"].clone();
                        }
                    }
                    _ => {}
                }
            }
            if rows
                .iter()
                .any(|row| row["kind"] == json!("turnHeader") && row["state"] != json!("running"))
            {
                return response;
            }
        }
        panic!("turn {command_id} did not converge; rows: {rows:?}");
    }

    /// 从 `v4/commands/query` 回读**已落定**的命令 ack（settled fact，不是 response）。
    /// 形状按 `commands_query.rs`：`params.commands[]`，结果在 `results[0].result`
    /// （`commandQueryItemSchema` = `{key, result}`，未落定时 result 是 `"unknown"`）。
    fn settled_ack(&mut self, session_id: &str, command_id: &str) -> Value {
        let id = self.send(
            "v4/commands/query",
            json!({
                "commands": [{"sessionId": session_id, "commandId": command_id}],
            }),
        );
        let response = self.response(id);
        let result = response
            .pointer("/result/results/0/result")
            .cloned()
            .unwrap_or_else(|| panic!("commands/query returned no item: {response}"));
        assert_ne!(
            result,
            json!("unknown"),
            "the command must have settled before we read it back: {response}"
        );
        result
    }
}

fn workspace_and_data(base: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    (workspace, data_dir)
}

fn write_provider_config(base: &Path, port: u16) -> std::path::PathBuf {
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

/// MAJOR-1 的线上证据：历史写失败时，settled ack 的 `reasonCode` 必须是
/// `fault.turn.historyNotPersisted`，**不是** `fault.model.requestFailed`。
#[test]
fn settled_reason_code_is_history_not_persisted_not_a_generic_model_fault() {
    let (provider, port) = ScriptedProvider::spawn();
    let base = std::env::temp_dir().join(format!(
        "zcode-host-failclosed-reasoncode-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let (workspace, data_dir) = workspace_and_data(&base);
    let provider_config = write_provider_config(&base, port);
    let stderr_sink = Arc::new(Mutex::new(String::new()));

    let session_id;
    {
        let mut process = HostProcess::spawn(
            &workspace,
            &data_dir,
            &provider_config,
            Arc::clone(&stderr_sink),
        );
        let mut client = Client::new(&mut process);
        session_id = client.create_session("cmd-create");
        client.subscribe(&session_id, "conn-1");

        // 制造真实的写失败：删掉 transcript.jsonl。`append_entry` 用 `.append(true)`
        // 且没有 `.create(true)`，所以此后每次写都失败。
        let transcript = data_dir
            .join("sessions")
            .join(&session_id)
            .join("transcript.jsonl");
        assert!(
            transcript.exists(),
            "transcript must exist before we break it"
        );
        std::fs::remove_file(&transcript).expect("remove transcript");

        // turn 仍然被 admission 接受（协议层正常），但模型**不应**被调用。
        let before = provider.request_count();
        let response = client.run_turn(&session_id, "cmd-send-1", "does the model see this?");
        assert_eq!(response["result"]["status"], json!("accepted"));

        // 等 settle 落盘后回读已落定 ack。
        let ack = client.settled_ack(&session_id, "cmd-send-1");
        let status = ack
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("settled ack has no status: {ack}"));
        let reason_code = ack
            .get("reasonCode")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_ne!(status, "accepted", "the turn must not succeed: {ack}");
        assert_eq!(
            reason_code, "fault.turn.historyNotPersisted",
            "the fault must reach the wire with its own vocabulary entry, not be swallowed \
             by the generic model fault: {ack}"
        );
        assert_eq!(
            provider.request_count(),
            before,
            "fail-closed means no model call happens at all"
        );
    }

    let stderr = stderr_sink.lock().map(|s| s.clone()).unwrap_or_default();
    assert!(
        stderr.contains("fault.turn.historyNotPersisted"),
        "the fault must be visible in diagnostics too: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// MAJOR-2 第二扇门的线上证据：`transcript.jsonl` 整份缺失后重启，host 必须在
/// stderr 报出降级恢复，而不是静默带着空记忆 resume。
#[test]
fn restart_with_missing_transcript_reports_degraded_resume() {
    let (_provider, port) = ScriptedProvider::spawn();
    let base = std::env::temp_dir().join(format!(
        "zcode-host-failclosed-missing-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let (workspace, data_dir) = workspace_and_data(&base);
    let provider_config = write_provider_config(&base, port);

    let session_id;
    {
        let sink = Arc::new(Mutex::new(String::new()));
        let mut process = HostProcess::spawn(&workspace, &data_dir, &provider_config, sink);
        let mut client = Client::new(&mut process);
        session_id = client.create_session("cmd-create");
        client.subscribe(&session_id, "conn-1");
        client.run_turn(&session_id, "cmd-send-1", "remember me");
    }

    // 删掉整份 transcript，再重启。
    let transcript = data_dir
        .join("sessions")
        .join(&session_id)
        .join("transcript.jsonl");
    std::fs::remove_file(&transcript).expect("remove whole transcript");

    let stderr_sink = Arc::new(Mutex::new(String::new()));
    {
        let mut process = HostProcess::spawn(
            &workspace,
            &data_dir,
            &provider_config,
            Arc::clone(&stderr_sink),
        );
        let mut client = Client::new(&mut process);
        client.subscribe(&session_id, "conn-2");
        // 降级恢复后仍可正常发一轮（功能不该被阻断），但缺失必须被报出来。
        let _ = client.run_turn(&session_id, "cmd-send-2", "after the loss");
    }

    // stderr 是异步写的，等它落盘。
    let mut stderr = String::new();
    for _ in 0..50 {
        stderr = stderr_sink.lock().map(|s| s.clone()).unwrap_or_default();
        if stderr.contains("degraded resume") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        stderr.contains("degraded resume"),
        "a missing transcript must produce a degraded-resume diagnostic, not a silent \
         amnesia: {stderr}"
    );
    assert!(
        stderr.contains(&session_id),
        "the diagnostic must name the affected session: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&base);
}
