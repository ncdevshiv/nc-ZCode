//! Session memory e2e：模型历史跨 turn 累积，并在重启后保持。
//!
//! 这是 `specs/rust-native-crates.md` S3 session memory 规则 1-4 的行为证据。
//! 旧实现每轮用 `vec![user text]` 重建历史，模型在第 2 轮就失忆；本文件用**真实
//! provider 请求体**证明修复，不靠代理标志：
//!
//! 1. turn 1 → turn 2：第二个请求体必须同时含第一轮问答与第二轮输入。
//! 2. 重启 → turn 3：重启后第一个请求体必须含前两轮的全部内容（durable）。
//!
//! provider double 只活在测试进程里（Tcp listener + 脚本化 SSE），不是产品代码
//! 路径上的 mock；产品代码零测试替身。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 每轮都返回终态纯文本的 SSE（无 tool_use，让 turn 一步收敛）。
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
                thread::spawn(move || {
                    handle_connection(&mut stream, bodies, counter);
                });
            }
        });
        (Self { bodies }, port)
    }

    fn request_bodies(&self) -> Vec<String> {
        self.bodies
            .lock()
            .map(|bodies| bodies.clone())
            .unwrap_or_default()
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
        // 每次回复都点明这是第几次请求，模型文本本身即可证明它看见了几轮上下文。
        let payload = final_text_sse(&format!("reply number {index}"));
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

struct HostProcess {
    child: Child,
}

impl HostProcess {
    fn spawn(workspace: &Path, data_dir: &Path, provider_config: &Path) -> Self {
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

    /// 发一条 sendText 并等到该轮 settle。
    ///
    /// 收敛判据必须**累积行集**再看 turnHeader 的 state：终态 header 是
    /// `row.upserted`（不是 `row.appended`），只看单帧的 append 会永远等不到——
    /// 这正是本文件第一版挂死的原因。
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
        // 本轮之前累积的行（subscribe 的 initial snapshot 已经读掉了，这里只从
        // delta 开始累积即可：终态 header 的 upsert 会带 rowId）。
        let mut rows: Vec<Value> = Vec::new();
        for _ in 0..40 {
            let line = self.next_line();
            let deltas = line["params"]["frame"]["payload"]["deltas"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            apply_to_rows(&mut rows, &deltas);
            let settled = rows
                .iter()
                .any(|row| row["kind"] == json!("turnHeader") && row["state"] != json!("running"));
            if settled {
                return response;
            }
        }
        panic!("turn {command_id} did not converge; rows: {rows:?}");
    }
}

fn apply_to_rows(rows: &mut Vec<Value>, deltas: &[Value]) {
    for delta in deltas {
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
}

/// 从请求体里取出 messages 数组（断言对象级形状，不做字符串 contains 匹配）。
fn messages_of(body: &str) -> Vec<Value> {
    let parsed: Value = serde_json::from_str(body).expect("provider request body is JSON");
    parsed["messages"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("request body must carry messages: {body}"))
}

/// anthropic 形状下 user/assistant 文本所在的位置。
fn text_of(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn body_carries(body: &str, needle: &str) -> bool {
    messages_of(body)
        .iter()
        .any(|message| text_of(message).contains(needle))
}

#[test]
fn model_history_survives_across_turns_and_across_restart() {
    let (provider, port) = ScriptedProvider::spawn();
    let base =
        std::env::temp_dir().join(format!("zcode-host-session-memory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");

    let provider_config = base.join("provider.json");
    std::fs::write(
        &provider_config,
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

    // ── 阶段一：同进程内跨 turn 累积 ──
    let session_id;
    {
        let mut process = HostProcess::spawn(&workspace, &data_dir, &provider_config);
        let mut client = Client::new(&mut process);
        let id = client.send(
            "v4/command",
            json!({
                "type": "createSession",
                "commandId": "cmd-create",
                "clientId": "e2e",
                "sessionId": null,
                "payload": {},
                "issuedAt": 1_700_000_000_000.0,
            }),
        );
        session_id = client.response(id)["result"]["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string();
        let id = client.send(
            "v4/conversation/subscribe",
            json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
        );
        let _ = client.response(id);
        let _ = client.next_line(); // initial snapshot frame

        client.run_turn(&session_id, "cmd-turn-1", "ALPHA first question");
        client.run_turn(&session_id, "cmd-turn-2", "BETA second question");
    }

    // ── 断言 1：turn 2 的请求体必须同时含 turn 1 与 turn 2 的内容 ──
    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 2, "two turns produce two model requests");
    let first = &bodies[0];
    assert!(body_carries(first, "ALPHA first question"));
    assert!(
        !body_carries(first, "BETA second question"),
        "turn 1 must not see the future"
    );
    let second = &bodies[1];
    assert!(
        body_carries(second, "ALPHA first question"),
        "turn 2 must still see turn 1's input: {second}"
    );
    assert!(body_carries(second, "BETA second question"));
    assert!(
        body_carries(second, "reply number 0"),
        "turn 2 must see turn 1's answer: {second}"
    );
    assert!(
        messages_of(second).len() > messages_of(first).len(),
        "history must grow across turns"
    );

    // ── 阶段二：重启后历史必须还在（durable）──
    {
        let mut process = HostProcess::spawn(&workspace, &data_dir, &provider_config);
        let mut client = Client::new(&mut process);
        let id = client.send(
            "v4/conversation/subscribe",
            json!({"sessionId": session_id, "connectionId": "conn-2", "clientMode": "desktop-continuous"}),
        );
        let _ = client.response(id);
        let _ = client.next_line(); // recovery snapshot frame

        client.run_turn(&session_id, "cmd-turn-3", "GAMMA third question");
    }

    // ── 断言 2：重启后的第一个请求体必须含前两轮全部内容 ──
    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 3, "third turn adds one more model request");
    let after_restart = &bodies[2];
    for needle in [
        "ALPHA first question",
        "reply number 0",
        "BETA second question",
        "reply number 1",
        "GAMMA third question",
    ] {
        assert!(
            body_carries(after_restart, needle),
            "restart must rehydrate history; missing {needle} in: {after_restart}"
        );
    }

    let _ = std::fs::remove_dir_all(&base);
}
