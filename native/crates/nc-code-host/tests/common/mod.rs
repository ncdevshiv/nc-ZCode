#![allow(dead_code)]
//! 集成测试共享夹具：真实二进制进程 + stdio client + 脚本化 SSE provider。
//! 产品代码零测试替身；这里只是测试进程内的传输/夹具。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub const TOOL_USE_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"planning the write\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_native_1\",\"name\":\"Write\",\"input\":{}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file_path\\\":\\\"native-e2e.txt\\\",\\\"content\\\":\\\"written by the native agent\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

pub const FINAL_TEXT_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"File written.\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// 脚本化 provider double：按请求序回复；记录所有请求体。
pub struct ScriptedProvider {
    bodies: Arc<Mutex<Vec<String>>>,
}

impl ScriptedProvider {
    pub fn spawn() -> (Self, u16) {
        Self::spawn_with_delay(Duration::from_millis(0))
    }

    /// `delay` = 每个模型请求的响应前等待（用于制造「turn 在跑」的窗口）。
    pub fn spawn_with_delay(delay: Duration) -> (Self, u16) {
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
                    handle_connection(&mut stream, bodies, counter, delay);
                });
            }
        });
        (Self { bodies }, port)
    }

    pub fn request_bodies(&self) -> Vec<String> {
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
    delay: Duration,
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

        if !delay.is_zero() {
            thread::sleep(delay);
        }
        let index = counter.fetch_add(1, Ordering::SeqCst);
        // 无延迟 = 既有的 tool-loop 脚本；有延迟 = 队列场景，始终回终态文本。
        let payload = if delay.is_zero() && index == 0 {
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

/// 纯文本应答（无工具）的 SSE：goal 续跑轮与 verifier 都是这种形状。
pub fn text_sse(text: &str) -> String {
    let escaped = serde_json::to_string(text).expect("escape text");
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_text\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{escaped}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":3}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

/// Anthropic SSE for one assistant turn that calls a tool. The whole input JSON is
/// emitted as one `input_json_delta` (the provider assembler accepts it).
pub fn tool_use_sse_with_id(id: &str, name: &str, input: &Value) -> String {
    let input_json = serde_json::to_string(input).expect("tool input json");
    let escaped = serde_json::to_string(&input_json).expect("escape input");
    let name = serde_json::to_string(name).expect("escape tool name");
    let id = serde_json::to_string(id).expect("escape tool id");
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_tool\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":{id},\"name\":{name},\"input\":{{}}}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{escaped}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":3}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

pub fn tool_use_sse(name: &str, input: &Value) -> String {
    tool_use_sse_with_id("toolu_e2e", name, input)
}

/// Run the headless entry point and return its process output.
pub fn run_headless(
    workspace: &Path,
    data_dir: &Path,
    provider_config: Option<&Path>,
    full_access: bool,
    prompt: &str,
) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_zcode-host");
    let mut command = Command::new(bin);
    command
        .arg("--workspace")
        .arg(workspace)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--prompt")
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(config) = provider_config {
        command.arg("--provider-config").arg(config);
    }
    if full_access {
        command.arg("--full-access");
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command.output().expect("run zcode-host --prompt")
}

/// 按请求序回放的 provider：第 N 个请求回 `script[N]`（越界复用最后一条）。
/// goal e2e 用它脚本化「续跑轮 → 裁判 → 续跑轮 → 裁判」的完整序列。
pub struct ScriptedResponder {
    bodies: Arc<Mutex<Vec<String>>>,
    script: Arc<Mutex<Vec<String>>>,
    /// Global request index. `reset_script` resets it so a fresh script really
    /// starts from its first entry (the old index would clamp to the last entry
    /// and silently replay the wrong response).
    counter: Arc<AtomicUsize>,
}

impl ScriptedResponder {
    pub fn spawn(script: Vec<String>) -> (Self, u16) {
        Self::spawn_with_delay(script, Duration::from_millis(0))
    }

    /// `delay` 制造「模型请求在途」的窗口（stop/pause 打断、verifying 投影断言用）。
    pub fn spawn_with_delay(script: Vec<String>, delay: Duration) -> (Self, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_for_thread = Arc::clone(&bodies);
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_thread = Arc::clone(&counter);
        let script = Arc::new(Mutex::new(script));
        let script_for_thread = Arc::clone(&script);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let bodies = Arc::clone(&bodies_for_thread);
                let counter = Arc::clone(&counter_for_thread);
                let script = Arc::clone(&script_for_thread);
                thread::spawn(move || {
                    handle_scripted_connection(&mut stream, bodies, counter, script, delay);
                });
            }
        });
        (
            Self {
                bodies,
                script,
                counter,
            },
            port,
        )
    }

    /// Replace the remaining script (a background subagent run finishes after the
    /// parent's next request, so the next response must be scripted mid-test).
    pub fn set_script(&self, script: Vec<String>) {
        self.reset_script(script);
    }

    /// Append one more response to the remaining script.
    pub fn push(&self, response: String) {
        if let Ok(mut current) = self.script.lock() {
            current.push(response);
        }
    }

    pub fn reset_script(&self, script: Vec<String>) {
        if let Ok(mut current) = self.script.lock() {
            *current = script;
        }
        // A fresh script starts from its first entry, not from the old counter.
        self.counter.store(0, Ordering::SeqCst);
    }

    pub fn request_bodies(&self) -> Vec<String> {
        self.bodies
            .lock()
            .map(|bodies| bodies.clone())
            .unwrap_or_default()
    }
}

fn handle_scripted_connection(
    stream: &mut TcpStream,
    bodies: Arc<Mutex<Vec<String>>>,
    counter: Arc<AtomicUsize>,
    script: Arc<Mutex<Vec<String>>>,
    delay: Duration,
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
        if !delay.is_zero() {
            thread::sleep(delay);
        }
        let index = counter.fetch_add(1, Ordering::SeqCst);
        let payload = {
            let script = script.lock().expect("script lock");
            assert!(
                !script.is_empty(),
                "scripted responder needs at least one response"
            );
            script[index.min(script.len().saturating_sub(1))].clone()
        };
        let payload = &payload;
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

pub struct HostProcess {
    child: Child,
}

impl HostProcess {
    pub fn spawn(
        workspace: &Path,
        data_dir: &Path,
        provider_config: &Path,
        full_access: bool,
    ) -> Self {
        Self::spawn_with(workspace, data_dir, Some(provider_config), full_access, &[])
    }

    /// Host without `--provider-config` (the desktop injects provider stores via env
    /// instead; this is the handoff path).
    pub fn spawn_without_provider_config(
        workspace: &Path,
        data_dir: &Path,
        full_access: bool,
        env: &[(&str, &str)],
    ) -> Self {
        Self::spawn_with(workspace, data_dir, None, full_access, env)
    }

    pub fn spawn_with_env(
        workspace: &Path,
        data_dir: &Path,
        provider_config: &Path,
        full_access: bool,
        env: &[(&str, &str)],
    ) -> Self {
        Self::spawn_with(workspace, data_dir, Some(provider_config), full_access, env)
    }

    fn spawn_with(
        workspace: &Path,
        data_dir: &Path,
        provider_config: Option<&Path>,
        full_access: bool,
        env: &[(&str, &str)],
    ) -> Self {
        let bin = env!("CARGO_BIN_EXE_zcode-host");
        let mut command = Command::new(bin);
        command
            .arg("--workspace")
            .arg(workspace)
            .arg("--data-dir")
            .arg(data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(provider_config) = provider_config {
            command.arg("--provider-config").arg(provider_config);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        if full_access {
            command.arg("--full-access");
        }
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let mut child = command.spawn().expect("spawn zcode-host");
        // 常驻读 stderr：既避免管道写满阻塞 host，也让 host panic/诊断直接出现在测试输出。
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    eprintln!("[host] {line}");
                }
            });
        }
        Self { child }
    }

    pub fn stdin(&mut self) -> std::process::ChildStdin {
        self.child.stdin.take().expect("stdin")
    }

    pub fn stdout(&mut self) -> BufReader<std::process::ChildStdout> {
        BufReader::new(self.child.stdout.take().expect("stdout"))
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Client {
    pub id_counter: u64,
    pub stdin: std::process::ChildStdin,
    pub stdout: BufReader<std::process::ChildStdout>,
    pub pending: std::collections::VecDeque<Value>,
}

impl Client {
    pub fn send(&mut self, method: &str, params: Value) -> u64 {
        self.id_counter += 1;
        let id = self.id_counter;
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let line = serde_json::to_string(&request).expect("request");
        self.stdin.write_all(line.as_bytes()).expect("write");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush");
        id
    }

    fn read_raw(&mut self) -> Value {
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

    pub fn next_line(&mut self) -> Value {
        match self.pending.pop_front() {
            Some(buffered) => buffered,
            None => self.read_raw(),
        }
    }

    /// 等待指定 id 的响应；途中读到的通知帧按原序放回 pending。
    /// 注意：必须先取 pending 再读新行，且**不得**把刚弹出的帧再塞回队尾——
    /// 否则 next_line 会再次弹出同一帧，形成死循环。
    pub fn response(&mut self, id: u64) -> Value {
        let mut skipped = std::collections::VecDeque::new();
        loop {
            let message = match self.pending.pop_front() {
                Some(buffered) => buffered,
                None => self.read_raw(),
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

pub fn command_envelope(
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

/// CAS / 行定位命令的信封：必须携带 baseRevision 与 baseLogEpoch，
/// 否则 `parse_command_envelope` 直接以 `proto.invalidPayload` 拒绝。
pub fn cas_command_envelope(
    command_type: &str,
    command_id: &str,
    session_id: &str,
    base_revision: i64,
    base_log_epoch: &str,
    payload: Value,
) -> Value {
    json!({
        "type": command_type,
        "commandId": command_id,
        "clientId": "e2e",
        "sessionId": session_id,
        "baseRevision": base_revision,
        "baseLogEpoch": base_log_epoch,
        "payload": payload,
        "issuedAt": 1_700_000_000_000.0,
    })
}

/// 权威快照：重新订阅取服务端当前的 revision / rows / logEpoch。
///
/// CAS 命令的 `baseRevision` 必须来自客户端**最后看到**的权威投影；靠累加帧来猜
/// 水位是脆弱的（终态帧与 revision 的先后正是被测性质之一）。真实客户端重连时走的
/// 就是这条路径，所以测试也用它。
pub struct Snapshot {
    pub revision: i64,
    pub log_epoch: String,
    pub rows: Vec<Value>,
    /// `queueStateSchema` 原样带出：子会话命令不得污染父队列，这条断言要读它。
    pub queue: Value,
}

pub fn authoritative_snapshot(client: &mut Client, session_id: &str) -> Snapshot {
    let id = client.send(
        "v4/conversation/subscribe",
        json!({
            "sessionId": session_id,
            "connectionId": "conn-1",
            "clientMode": "desktop-continuous"
        }),
    );
    let _ = client.response(id);
    // 订阅之后可能先收到别的会话的帧或遗留的 delta 帧（`pump_until` 看到目标就返回，
    // 会留下在途帧）。所以既要求 `kind == "snapshot"`，也要求 `snapshot.sessionId`
    // 就是本次订阅的会话——否则会拿兄弟会话的快照冒充本会话的（实测会把 child 的行集
    // 当成 parent 的，看起来像 host 掉了行）。
    let mut skipped = std::collections::VecDeque::new();
    let snapshot_line = loop {
        let line = client.next_line();
        let frame = &line["params"]["frame"]["payload"];
        if frame["kind"] == json!("snapshot") && frame["snapshot"]["sessionId"] == json!(session_id)
        {
            break line;
        }
        skipped.push_back(line);
        if skipped.len() > 200 {
            panic!("no snapshot frame for {session_id} within 200 lines after subscribe");
        }
    };
    for line in skipped.into_iter().rev() {
        client.pending.push_front(line);
    }
    let snapshot = &snapshot_line["params"]["frame"]["payload"]["snapshot"];
    Snapshot {
        revision: snapshot["revision"].as_i64().expect("snapshot revision"),
        log_epoch: snapshot["logEpoch"].as_str().expect("logEpoch").to_string(),
        rows: snapshot["rows"]["window"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        queue: snapshot["queue"].clone(),
    }
}

pub fn deltas_from_frame(line: &Value) -> Vec<Value> {
    match line["params"]["frame"]["payload"]["kind"].as_str() {
        Some("deltas") => line["params"]["frame"]["payload"]["deltas"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

pub fn apply_to_rows(rows: &mut Vec<Value>, deltas: &[Value]) {
    for delta in deltas {
        match delta["op"].as_str() {
            Some("row.appended") => rows.push(delta["row"].clone()),
            Some("row.removed") => {
                // 客户端语义（`delta.ts`）：删除 fromRowId **及其后所有**行。
                // 分支截断（editUserQuery / retryTurn）靠这一个 op 回退行集。
                let from = delta["fromRowId"].as_u64();
                rows.retain(|row| {
                    from.is_some_and(|from| row["rowId"].as_u64().unwrap_or(0) < from)
                });
            }
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

/// 读帧直到谓词满足；顺带把 state.updated 的 revision 记为当前水位。
pub fn pump_until(
    client: &mut Client,
    rows: &mut Vec<Value>,
    revision: &mut i64,
    mut done: impl FnMut(&[Value]) -> bool,
) -> bool {
    for _ in 0..200 {
        let line = client.next_line();
        let deltas = deltas_from_frame(&line);
        for delta in &deltas {
            if let Some(revision_value) = delta["patch"]["revision"].as_i64() {
                *revision = (*revision).max(revision_value);
            }
        }
        apply_to_rows(rows, &deltas);
        if done(&deltas) {
            return true;
        }
    }
    false
}
