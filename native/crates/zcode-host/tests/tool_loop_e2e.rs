//! Tool-loop e2e：真实工具执行，由本地脚本化 SSE provider 驱动。
//!
//! 场景 A（full-access）：模型回合 1 返回 tool_use(Write) → host 执行真实写入 →
//! tool_result 回灌 → 模型回合 2 返回终态文本。断言：文件真实落盘、toolCall 行
//! success、assistant 终态文本、第二个请求体里出现 tool_result（回灌证据）。
//! 场景 B（无 full-access）：同一 tool_use 被权限闸门拒绝 → tool_result 携带
//! typed 拒绝 → 文件不存在（fail-closed 证据）。
//!
//! provider double 只活在测试进程里（Tcp listener + 静态 SSE 脚本），不是产品
//! 代码路径上的 mock；产品代码零测试替身。

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

const TOOL_USE_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_native_1\",\"name\":\"Write\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file_path\\\":\\\"native-e2e.txt\\\",\\\"content\\\":\\\"written by the native agent\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

const FINAL_TEXT_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"File written.\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// 脚本化 provider double：按请求序回复；记录所有请求体。
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

/// 最小 HTTP/1.1：读一个请求（含 body）→ 按请求序回复脚本 SSE。
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
        let body = String::from_utf8_lossy(&body).to_string();
        bodies.lock().expect("bodies lock").push(body);

        let index = counter.fetch_add(1, Ordering::SeqCst);
        let payload = if index == 0 {
            TOOL_USE_SSE
        } else {
            FINAL_TEXT_SSE
        };
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
    fn spawn(workspace: &Path, data_dir: &Path, provider_config: &Path, full_access: bool) -> Self {
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
        if full_access {
            command.arg("--full-access");
        }
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
        "clientId": "e2e",
        "sessionId": session_id,
        "payload": payload,
        "issuedAt": 1_700_000_000_000.0,
    })
}

fn deltas_from_frame(line: &Value) -> Vec<Value> {
    match line["params"]["frame"]["payload"]["kind"].as_str() {
        Some("deltas") => line["params"]["frame"]["payload"]["deltas"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
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

struct Scenario {
    name: &'static str,
    full_access: bool,
}

fn run_scenario(scenario: &Scenario) {
    let (provider, port) = ScriptedProvider::spawn();
    let base = std::env::temp_dir().join(format!(
        "zcode-host-toolloop-{}-{}",
        scenario.name,
        std::process::id()
    ));
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

    let mut process = HostProcess::spawn(
        &workspace,
        &data_dir,
        &provider_config,
        scenario.full_access,
    );
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
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot frame

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send-1",
            Some(&session_id),
            json!({"text": "write the marker file"}),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));

    let mut rows = Vec::new();
    let mut converged = false;
    for _ in 0..20 {
        let line = client.next_line();
        let deltas = deltas_from_frame(&line);
        apply_to_rows(&mut rows, &deltas);
        let header_done = rows
            .iter()
            .any(|row| row["kind"] == json!("turnHeader") && row["state"] != json!("running"));
        if header_done {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "turn did not converge: {}",
        serde_json::to_string_pretty(&rows).unwrap()
    );

    // 共同断言：toolCall 行投影存在。
    let tool_row = rows
        .iter()
        .find(|row| row["kind"] == json!("toolCall"))
        .expect("toolCall row must be projected");
    assert_eq!(tool_row["toolName"], json!("Write"));

    if scenario.full_access {
        // 场景 A：真实执行 + 回灌 + 终态文本。
        assert_eq!(
            tool_row["status"],
            json!("success"),
            "tool must succeed: {tool_row}"
        );
        let marker = workspace.join("native-e2e.txt");
        assert!(
            marker.exists(),
            "the tool must have really written the file"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read marker"),
            "written by the native agent"
        );
        let assistant = rows
            .iter()
            .find(|row| row["kind"] == json!("assistantText"))
            .expect("assistant row");
        assert_eq!(assistant["state"], json!("complete"));
        assert_eq!(assistant["text"], json!("File written."));
        assert_eq!(
            rows.iter()
                .filter(|row| row["kind"] == json!("turnHeader"))
                .count(),
            1
        );
        let header = rows
            .iter()
            .find(|row| row["kind"] == json!("turnHeader"))
            .expect("header");
        assert_eq!(header["state"], json!("completedSuccess"));

        // 回灌证据：第二个请求体必须包含 tool_result 与工具输出。
        let bodies = provider.request_bodies();
        assert_eq!(bodies.len(), 2, "exactly two model roundtrips expected");
        assert!(
            bodies[1].contains("tool_result"),
            "second request must carry the tool result"
        );
        assert!(
            bodies[1].contains("toolu_native_1"),
            "tool_call_id must correlate"
        );
        assert!(
            bodies[1].contains("wrote 27 bytes to native-e2e.txt"),
            "tool output must feed back; body was: {}",
            bodies[1]
        );
    } else {
        // 场景 B：fail-closed 拒绝，文件不存在，拒绝回灌给模型。
        assert_eq!(
            tool_row["status"],
            json!("error"),
            "permission denial must project as error"
        );
        let marker = workspace.join("native-e2e.txt");
        assert!(
            !marker.exists(),
            "no file may be written without full access"
        );
        let error = &tool_row["error"];
        assert_eq!(error["code"], json!("tool.permissionDenied"));
        let bodies = provider.request_bodies();
        assert!(
            bodies[1].contains("tool.permissionDenied"),
            "denial must reach the model"
        );
    }

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn tool_loop_executes_real_tools_and_feeds_results_back() {
    run_scenario(&Scenario {
        name: "full-access",
        full_access: true,
    });
}

#[test]
fn destructive_tools_fail_closed_without_full_access() {
    run_scenario(&Scenario {
        name: "denied",
        full_access: false,
    });
}
