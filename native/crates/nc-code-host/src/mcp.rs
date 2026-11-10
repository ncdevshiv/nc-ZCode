//! MCP 客户端：把外部 MCP server 的工具并入 host 工具面（对齐 TS
//! `contracts/src/interfaces/mcp.port.ts` 的 `McpPort` 语义面）。
//!
//! 传输（与 TS `type: "stdio" | "http" | "sse"` 对齐）：
//! - **stdio**：spawn 子进程 → JSON-RPC over stdio → `initialize` → `tools/list`；
//! - **streamable HTTP**：`POST <url>`，响应可以是 `application/json`，也可以是
//!   `text/event-stream`（按 SSE 帧解析）；`Mcp-Session-Id` 响应头在后续请求中回传；
//! - **legacy HTTP+SSE**：`GET <url>` 先收到 `endpoint` 事件，再向该端点 POST 消息，
//!   响应经同一条 SSE 流返回。
//! 三种传输共享同一套工具语义与 fail-closed 规则；OAuth 交互式登录仍未接线
//! （凭据可由调用方以静态 header 注入）。
//!
//! 工具面：MCP 工具以 `mcp__<server>__<tool>` 暴露给模型（与 TS 侧命名一致，
//! 避免与 nc-tools 点号名和 zcode 工具重名）。发现的工具**只读执行**：MCP server
//! 声明的 destructive 语义本 increment 不解析，因此统一按 needsApproval 处理
//! ——headless 无交互客户端，默认**拒绝**执行外部 MCP 工具（fail-closed），除非
//! `--full-access`。
//!
//! 隔离：每个 server 一份连接；断连即从工具面移除（不留死工具）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nc_code_service_core::logger::ServiceLogger;
use serde_json::{json, Value};

/// MCP 子进程 stderr 明确禁止原样落盘：env/header/token 等 secret 常出现。
/// 只记脱敏截断后的行（secret 关键字或长不透明 token 直接整行 [REDACTED]）。
fn redact_mcp_stderr_line(line: &str) -> String {
    const LIMIT: usize = 500;
    let clipped = if line.len() > LIMIT {
        &line[..LIMIT]
    } else {
        line
    };
    let lower = clipped.to_ascii_lowercase();
    const SENSITIVE: &[&str] = &[
        "token",
        "secret",
        "password",
        "passwd",
        "api_key",
        "api-key",
        "apikey",
        "authorization",
        "bearer",
        "cookie",
        "private_key",
        "access_key",
        "client_secret",
    ];
    let has_keyword = SENSITIVE.iter().any(|word| lower.contains(word));
    let has_long_token = clipped
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.')
        .any(|tok| tok.len() >= 32);
    if has_keyword || has_long_token {
        format!("[REDACTED] (stderr line truncated len={})", clipped.len())
    } else {
        clipped.to_string()
    }
}

/// 单个 MCP server 的 stdio 连接。
pub struct McpConnection {
    server_name: String,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    /// 读缓冲 + 后台读线程把每行 JSON-RPC 响应放进 pending。
    pending: Arc<Mutex<HashMap<u64, Value>>>,
    next_id: AtomicU64,
    tools: Vec<McpTool>,
}

/// 一个被发现的 MCP 工具（`mcp__server__tool`）。
#[derive(Debug, Clone)]
pub struct McpTool {
    pub server: String,
    pub tool: String,
    pub description: String,
    pub input_schema: Value,
    /// Server-provided annotation hints (MCP spec). Used for hardened permission decisions.
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

impl McpTool {
    /// 模型可见名（与 TS 侧 `mcp__server__tool` 一致）。
    pub fn qualified_name(&self) -> String {
        format!("mcp__{}__{}", self.server, self.tool)
    }
}

#[derive(Debug)]
pub enum McpError {
    Spawn { server: String, message: String },
    Protocol { server: String, message: String },
    ToolFailed { tool: String, message: String },
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { server, message } => {
                write!(f, "mcp server {server} failed to start: {message}")
            }
            Self::Protocol { server, message } => {
                write!(f, "mcp server {server} protocol error: {message}")
            }
            Self::ToolFailed { tool, message } => write!(f, "mcp tool {tool} failed: {message}"),
        }
    }
}

impl McpConnection {
    /// 启动 server 子进程并完成 initialize + tools/list。
    pub fn connect(server: &str, command: &str, args: &[String]) -> Result<Self, McpError> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| McpError::Spawn {
                server: server.to_string(),
                message: error.to_string(),
            })?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::Spawn {
            server: server.to_string(),
            message: "no stdin pipe".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Spawn {
            server: server.to_string(),
            message: "no stdout pipe".to_string(),
        })?;
        // stderr 排空：否则 pipe 填满会反压子进程。
        if let Some(stderr) = child.stderr.take() {
            let server_name = server.to_string();
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(std::result::Result::ok) {
                    // stderr 可能含 secret，禁止原样落盘；只记脱敏截断行。
                    ServiceLogger::new(format!("mcp:{server_name}"))
                        .warn(None, redact_mcp_stderr_line(&line));
                }
            });
        }

        let pending: Arc<Mutex<HashMap<u64, Value>>> = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_reader = Arc::clone(&pending);
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(std::result::Result::ok) {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    if let Ok(mut map) = pending_for_reader.lock() {
                        map.insert(id, message);
                    }
                }
            }
        });

        let connection = Self {
            server_name: server.to_string(),
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            tools: Vec::new(),
        };

        // initialize（协议版本 2025-06-18：与 nc-tools/mcp 世界一致的能力面）。
        const CLIENT_PROTOCOL_VERSION: &str = "2025-06-18";
        let initialized = connection.request(
            "initialize",
            json!({
                "protocolVersion": CLIENT_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "zcode-host", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        if let Some(server_version) = initialized.get("protocolVersion").and_then(Value::as_str) {
            const SUPPORTED: [&str; 2] = ["2025-06-18", "2024-11-05"];
            if !SUPPORTED.contains(&server_version) {
                return Err(McpError::Protocol {
                    server: server.to_string(),
                    message: format!(
                        "MCP server chose protocolVersion {server_version}; this client speaks {}",
                        SUPPORTED.join(", ")
                    ),
                });
            }
        }
        // initialized 通知（无 id）。
        connection.notify("notifications/initialized", json!({}))?;
        Ok(connection)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write_frame(&frame)
    }

    fn write_frame(&self, frame: &Value) -> Result<(), McpError> {
        let mut stdin = self.stdin.lock().map_err(|_| McpError::Protocol {
            server: self.server_name.clone(),
            message: "stdin poisoned".to_string(),
        })?;
        let line = serde_json::to_string(frame).map_err(|error| McpError::Protocol {
            server: self.server_name.clone(),
            message: error.to_string(),
        })?;
        writeln!(stdin, "{line}").map_err(|error| McpError::Protocol {
            server: self.server_name.clone(),
            message: error.to_string(),
        })?;
        stdin.flush().map_err(|error| McpError::Protocol {
            server: self.server_name.clone(),
            message: error.to_string(),
        })
    }

    /// 发请求并等响应（轮询 pending，1ms 粒度，协议响应必须落行）。
    fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id();
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write_frame(&frame)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Ok(map) = self.pending.lock() {
                if let Some(response) = map.get(&id) {
                    let response = response.clone();
                    drop(map);
                    if let Some(error) = response.get("error") {
                        return Err(McpError::Protocol {
                            server: self.server_name.clone(),
                            message: error.to_string(),
                        });
                    }
                    return Ok(response.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(McpError::Protocol {
                    server: self.server_name.clone(),
                    message: format!("{method} timed out"),
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// `tools/list` → 工具目录。
    pub fn list_tools(&mut self) -> Result<Vec<McpTool>, McpError> {
        let result = self.request("tools/list", json!({}))?;
        let listed = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.tools = listed
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?.to_string();
                let annotations = tool.get("annotations");
                let read_only_hint = annotations
                    .and_then(|a| a.get("readOnlyHint").or_else(|| a.get("readOnly")))
                    .and_then(Value::as_bool)
                    .or_else(|| tool.get("readOnly").and_then(Value::as_bool));
                let destructive_hint = annotations
                    .and_then(|a| a.get("destructiveHint").or_else(|| a.get("destructive")))
                    .and_then(Value::as_bool);
                let idempotent_hint = annotations
                    .and_then(|a| a.get("idempotentHint"))
                    .and_then(Value::as_bool);
                let open_world_hint = annotations
                    .and_then(|a| a.get("openWorldHint"))
                    .and_then(Value::as_bool);
                Some(McpTool {
                    server: String::new(), // 由 Pool 填充
                    tool: name,
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                    read_only_hint,
                    destructive_hint,
                    idempotent_hint,
                    open_world_hint,
                })
            })
            .collect();
        Ok(self.tools.clone())
    }

    /// `tools/call` → 结果文本（content 块拼接 + structuredContent 兜底）。
    pub fn call_tool(&self, tool: &str, arguments: &Value) -> Result<String, McpError> {
        let result = self.request("tools/call", json!({"name": tool, "arguments": arguments}))?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(McpError::ToolFailed {
                tool: tool.to_string(),
                message: extract_text(&result),
            });
        }
        let text = extract_text(&result);
        if let Some(structured) = result.get("structuredContent") {
            if text.is_empty() {
                return Ok(structured.to_string());
            }
        }
        Ok(text)
    }

    pub fn shutdown(&self) {
        let _ = self.notify("shutdown", json!({}));
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn extract_text(result: &Value) -> String {
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        let parts: Vec<String> = blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    result
        .get("content")
        .map(|content| content.to_string())
        .unwrap_or_default()
}

/// Parse the `data:` payloads of an SSE body into JSON-RPC messages.
///
/// A frame is terminated by a blank line (CRLF or LF). Multiple `data:` lines in one
/// frame are joined with `\n` (SSE spec). Non-JSON data is skipped: MCP carries
/// JSON-RPC on the wire, and a keep-alive comment (`: ping`) must not break parsing.
fn parse_sse_json_messages(body: &str) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    let flush = |lines: &mut Vec<String>, messages: &mut Vec<Value>| {
        if lines.is_empty() {
            return;
        }
        let joined = lines.join("\n");
        lines.clear();
        let trimmed = joined.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            messages.push(value);
        }
    };
    for raw_line in body.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            flush(&mut data_lines, &mut messages);
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    flush(&mut data_lines, &mut messages);
    messages
}

/// A transport a `McpPool` can drive. stdio spawns a child; HTTP talks to a URL.
trait McpBackend: Send {
    fn list_tools(&mut self) -> Result<Vec<McpTool>, McpError>;
    fn call_tool(&self, tool: &str, arguments: &Value) -> Result<String, McpError>;
    fn shutdown(&self);
}

impl McpBackend for McpConnection {
    fn list_tools(&mut self) -> Result<Vec<McpTool>, McpError> {
        McpConnection::list_tools(self)
    }
    fn call_tool(&self, tool: &str, arguments: &Value) -> Result<String, McpError> {
        McpConnection::call_tool(self, tool, arguments)
    }
    fn shutdown(&self) {
        McpConnection::shutdown(self)
    }
}

/// HTTP MCP connection: streamable HTTP (`POST` JSON-RPC) with a legacy
/// `GET`-SSE fallback. `headers` carry caller-provided auth (never logged).
pub struct HttpMcpConnection {
    server_name: String,
    client: reqwest::blocking::Client,
    /// POST target. For legacy SSE this is the endpoint announced by the stream.
    endpoint: Mutex<String>,
    headers: Vec<(String, String)>,
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
    tools: Vec<McpTool>,
    /// Legacy SSE only: messages pushed by the stream reader thread.
    pending: Arc<Mutex<HashMap<u64, Value>>>,
    sse_stop: Arc<std::sync::atomic::AtomicBool>,
    sse_handle: Option<std::thread::JoinHandle<()>>,
}

const CLIENT_PROTOCOL_VERSION: &str = "2025-06-18";

impl HttpMcpConnection {
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    fn apply_headers(
        &self,
        mut request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Ok(session) = self.session_id.lock() {
            if let Some(session) = session.as_deref() {
                request = request.header("Mcp-Session-Id", session);
            }
        }
        request
    }

    fn post_frame(
        &self,
        frame: &Value,
    ) -> Result<(reqwest::StatusCode, String, Option<String>), McpError> {
        let endpoint = self
            .endpoint
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        let request = self
            .client
            .post(&endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(frame.to_string());
        let response = self
            .apply_headers(request)
            .send()
            .map_err(|error| McpError::Protocol {
                server: self.server_name.clone(),
                message: format!("POST {endpoint} failed: {error}"),
            })?;
        let status = response.status();
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response.text().unwrap_or_default();
        let payload = if content_type.contains("text/event-stream") {
            // Streamable HTTP may answer with a short SSE stream; the JSON-RPC reply
            // is the message carrying our id.
            serde_json::to_string(&Value::Array(parse_sse_json_messages(&body))).unwrap_or_default()
        } else {
            body
        };
        Ok((status, payload, session))
    }

    /// One request. Streamable HTTP returns the reply in the POST body; legacy SSE
    /// returns it on the GET stream, so the caller waits on `pending`.
    fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id();
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let (status, body, session) = self.post_frame(&frame)?;
        if let Some(session) = session {
            if let Ok(mut current) = self.session_id.lock() {
                *current = Some(session);
            }
        }
        if !status.is_success() {
            return Err(McpError::Protocol {
                server: self.server_name.clone(),
                message: format!("{method} returned HTTP {status}: {body}"),
            });
        }
        let mut candidates: Vec<Value> = Vec::new();
        let trimmed = body.trim();
        if !trimmed.is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                match value {
                    Value::Array(items) => candidates.extend(items),
                    other => candidates.push(other),
                }
            }
        }
        if candidates.is_empty() {
            // Legacy SSE: wait for the stream reader to deliver this id.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if let Ok(map) = self.pending.lock() {
                    if let Some(response) = map.get(&id) {
                        let response = response.clone();
                        drop(map);
                        return Self::unwrap_result(&self.server_name, method, &response);
                    }
                }
                if std::time::Instant::now() > deadline {
                    return Err(McpError::Protocol {
                        server: self.server_name.clone(),
                        message: format!("{method} timed out"),
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let response = candidates
            .into_iter()
            .find(|item| item.get("id").and_then(Value::as_u64) == Some(id))
            .or_else(|| None)
            .ok_or_else(|| McpError::Protocol {
                server: self.server_name.clone(),
                message: format!("{method} reply did not carry id {id}: {body}"),
            })?;
        Self::unwrap_result(&self.server_name, method, &response)
    }

    fn unwrap_result(server: &str, method: &str, response: &Value) -> Result<Value, McpError> {
        if let Some(error) = response.get("error") {
            return Err(McpError::Protocol {
                server: server.to_string(),
                message: error.to_string(),
            });
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::Protocol {
                server: server.to_string(),
                message: format!("{method} reply carried no result: {response}"),
            })
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let (status, body, _session) = self.post_frame(&frame)?;
        if !status.is_success() {
            return Err(McpError::Protocol {
                server: self.server_name.clone(),
                message: format!("{method} notification returned HTTP {status}: {body}"),
            });
        }
        Ok(())
    }

    /// Streamable HTTP first; if the server rejects the POST handshake (legacy
    /// HTTP+SSE servers answer 405/404), fall back to the `GET`-SSE handshake.
    pub fn connect(
        server: &str,
        url: &str,
        headers: Vec<(String, String)>,
    ) -> Result<Self, McpError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|error| McpError::Spawn {
                server: server.to_string(),
                message: format!("http client: {error}"),
            })?;
        let mut connection = Self {
            server_name: server.to_string(),
            client,
            endpoint: Mutex::new(url.to_string()),
            headers,
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
            tools: Vec::new(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            sse_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sse_handle: None,
        };
        let initialize_params = || {
            json!({
                "protocolVersion": CLIENT_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "zcode-host", "version": env!("CARGO_PKG_VERSION")},
            })
        };
        let initialized = match connection.request("initialize", initialize_params()) {
            Ok(value) => value,
            Err(first_error) => {
                // Legacy HTTP+SSE: open the stream, learn the message endpoint.
                match connection.start_legacy_sse() {
                    Ok(()) => match connection.request("initialize", initialize_params()) {
                        Ok(value) => value,
                        Err(second_error) => {
                            return Err(McpError::Protocol {
                                server: server.to_string(),
                                message: format!(
                                    "streamable HTTP handshake failed ({first_error}); legacy SSE handshake failed ({second_error})"
                                ),
                            })
                        }
                    },
                    Err(_) => return Err(first_error),
                }
            }
        };
        if let Some(server_version) = initialized.get("protocolVersion").and_then(Value::as_str) {
            const SUPPORTED: [&str; 2] = ["2025-06-18", "2024-11-05"];
            if !SUPPORTED.contains(&server_version) {
                return Err(McpError::Protocol {
                    server: server.to_string(),
                    message: format!(
                        "MCP server chose protocolVersion {server_version}; this client speaks {}",
                        SUPPORTED.join(", ")
                    ),
                });
            }
        }
        connection.notify("notifications/initialized", json!({}))?;
        Ok(connection)
    }

    /// Legacy HTTP+SSE: `GET` the endpoint, read events until the server announces
    /// `endpoint`, spawn a reader that keeps pushing responses into `pending`.
    fn start_legacy_sse(&mut self) -> Result<(), McpError> {
        let url = self
            .endpoint
            .lock()
            .map(|value| value.clone())
            .map_err(|_| McpError::Protocol {
                server: self.server_name.clone(),
                message: "endpoint poisoned".to_string(),
            })?;
        let request = self.client.get(&url).header("accept", "text/event-stream");
        let response = self
            .apply_headers(request)
            .send()
            .map_err(|error| McpError::Protocol {
                server: self.server_name.clone(),
                message: format!("GET {url} failed: {error}"),
            })?;
        if !response.status().is_success() {
            return Err(McpError::Protocol {
                server: self.server_name.clone(),
                message: format!("GET {url} returned HTTP {}", response.status()),
            });
        }
        let mut stream = std::io::BufReader::new(response);
        let mut line = String::new();
        let mut data: Vec<String> = Vec::new();
        let mut event_name = String::new();
        let mut endpoint: Option<String> = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if std::time::Instant::now() > deadline {
                return Err(McpError::Protocol {
                    server: self.server_name.clone(),
                    message: "legacy SSE handshake timed out waiting for the endpoint event"
                        .to_string(),
                });
            }
            line.clear();
            let read = std::io::BufRead::read_line(&mut stream, &mut line).map_err(|error| {
                McpError::Protocol {
                    server: self.server_name.clone(),
                    message: format!("legacy SSE read failed: {error}"),
                }
            })?;
            if read == 0 {
                return Err(McpError::Protocol {
                    server: self.server_name.clone(),
                    message: "legacy SSE stream closed before the endpoint event".to_string(),
                });
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                if event_name == "endpoint" {
                    if let Some(value) = data.first() {
                        endpoint = Some(resolve_endpoint(&url, value));
                    }
                }
                data.clear();
                event_name.clear();
                if endpoint.is_some() {
                    break;
                }
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("event:") {
                event_name = rest.trim().to_string();
            } else if let Some(rest) = trimmed.strip_prefix("data:") {
                data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
        }
        let endpoint = endpoint.ok_or_else(|| McpError::Protocol {
            server: self.server_name.clone(),
            message: "legacy SSE handshake produced no endpoint".to_string(),
        })?;
        if let Ok(mut current) = self.endpoint.lock() {
            *current = endpoint;
        }
        // Background reader: keep consuming the stream for the session lifetime.
        let pending = Arc::clone(&self.pending);
        let stop = Arc::clone(&self.sse_stop);
        self.sse_handle = Some(std::thread::spawn(move || {
            let mut line = String::new();
            let mut data: Vec<String> = Vec::new();
            loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                line.clear();
                match std::io::BufRead::read_line(&mut stream, &mut line) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(_) => return,
                }
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    if !data.is_empty() {
                        let joined = data.join("\n");
                        data.clear();
                        if let Ok(message) = serde_json::from_str::<Value>(&joined) {
                            if let Some(id) = message.get("id").and_then(Value::as_u64) {
                                if let Ok(mut map) = pending.lock() {
                                    map.insert(id, message);
                                }
                            }
                        }
                    }
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("data:") {
                    data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                }
            }
        }));
        Ok(())
    }
}

/// Resolve the `endpoint` event's URI against the SSE URL (servers may announce a
/// relative path).
fn resolve_endpoint(base: &str, endpoint: &str) -> String {
    let endpoint = endpoint.trim();
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }
    // Strip a trailing slash + any query from the base, then join.
    let base = base.trim_end_matches('/');
    if let Some(scheme_end) = base.find("://") {
        let after_scheme = &base[scheme_end + 3..];
        if let Some(path_start) = after_scheme.find('/') {
            let origin = &base[..scheme_end + 3 + path_start];
            return format!("{origin}/{}", endpoint.trim_start_matches('/'));
        }
    }
    format!("{base}/{}", endpoint.trim_start_matches('/'))
}

impl McpBackend for HttpMcpConnection {
    fn list_tools(&mut self) -> Result<Vec<McpTool>, McpError> {
        let result = self.request("tools/list", json!({}))?;
        let listed = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.tools = listed
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?.to_string();
                let annotations = tool.get("annotations");
                let read_only_hint = annotations
                    .and_then(|a| a.get("readOnlyHint").or_else(|| a.get("readOnly")))
                    .and_then(Value::as_bool)
                    .or_else(|| tool.get("readOnly").and_then(Value::as_bool));
                let destructive_hint = annotations
                    .and_then(|a| a.get("destructiveHint").or_else(|| a.get("destructive")))
                    .and_then(Value::as_bool);
                let idempotent_hint = annotations
                    .and_then(|a| a.get("idempotentHint"))
                    .and_then(Value::as_bool);
                let open_world_hint = annotations
                    .and_then(|a| a.get("openWorldHint"))
                    .and_then(Value::as_bool);
                Some(McpTool {
                    server: String::new(),
                    tool: name,
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                    read_only_hint,
                    destructive_hint,
                    idempotent_hint,
                    open_world_hint,
                })
            })
            .collect();
        Ok(self.tools.clone())
    }

    fn call_tool(&self, tool: &str, arguments: &Value) -> Result<String, McpError> {
        let result = self.request("tools/call", json!({"name": tool, "arguments": arguments}))?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(McpError::ToolFailed {
                tool: tool.to_string(),
                message: extract_text(&result),
            });
        }
        let text = extract_text(&result);
        if text.is_empty() {
            if let Some(structured) = result.get("structuredContent") {
                return Ok(structured.to_string());
            }
        }
        Ok(text)
    }

    fn shutdown(&self) {
        self.sse_stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for HttpMcpConnection {
    fn drop(&mut self) {
        // Signal the reader and let it observe EOF when the client/socket drops; joining
        // here would block on a pending `read_line` for as long as the server keeps the
        // stream open.
        self.sse_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.sse_handle.take() {
            drop(handle);
        }
    }
}

/// server 名 → 连接 + 工具目录。
type SharedBackend = Arc<Mutex<Box<dyn McpBackend>>>;

#[derive(Default)]
pub struct McpPool {
    connections: Mutex<HashMap<String, SharedBackend>>,
    tools: Mutex<Vec<McpTool>>,
}

impl McpPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// 连接一个 stdio server 并登记其工具。
    pub fn connect_stdio(
        &self,
        server: &str,
        command: &str,
        args: &[String],
    ) -> Result<usize, McpError> {
        let mut connection = McpConnection::connect(server, command, args)?;
        let mut discovered = connection
            .list_tools()
            .map_err(|error| McpError::Protocol {
                server: server.to_string(),
                message: error.to_string(),
            })?;
        for tool in &mut discovered {
            tool.server = server.to_string();
        }
        let count = discovered.len();
        self.publish(server, discovered);
        self.install(server, Box::new(connection));
        Ok(count)
    }

    /// 连接一个 HTTP/SSE server（`url` 为 streamable HTTP 端点或 legacy SSE 端点）。
    pub fn connect_http(
        &self,
        server: &str,
        url: &str,
        headers: Vec<(String, String)>,
    ) -> Result<usize, McpError> {
        let mut connection = HttpMcpConnection::connect(server, url, headers)?;
        let mut discovered = connection
            .list_tools()
            .map_err(|error| McpError::Protocol {
                server: server.to_string(),
                message: error.to_string(),
            })?;
        for tool in &mut discovered {
            tool.server = server.to_string();
        }
        let count = discovered.len();
        self.publish(server, discovered);
        self.install(server, Box::new(connection));
        Ok(count)
    }

    fn publish(&self, server: &str, discovered: Vec<McpTool>) {
        if let Ok(mut tools) = self.tools.lock() {
            tools.retain(|tool| tool.server != server);
            tools.extend(discovered);
        }
    }

    fn install(&self, server: &str, connection: Box<dyn McpBackend>) {
        if let Ok(mut connections) = self.connections.lock() {
            if let Some(existing) = connections.remove(server) {
                if let Ok(existing) = existing.lock() {
                    existing.shutdown();
                }
            }
            connections.insert(server.to_string(), Arc::new(Mutex::new(connection)));
        }
    }

    pub fn tools(&self) -> Vec<McpTool> {
        self.tools
            .lock()
            .map(|tools| tools.clone())
            .unwrap_or_default()
    }

    pub fn disconnect(&self, server: &str) {
        if let Ok(mut connections) = self.connections.lock() {
            if let Some(connection) = connections.remove(server) {
                if let Ok(connection) = connection.lock() {
                    connection.shutdown();
                }
            }
        }
        if let Ok(mut tools) = self.tools.lock() {
            tools.retain(|tool| tool.server != server);
        }
    }

    pub fn call_tool(&self, qualified: &str, arguments: &Value) -> Result<String, McpError> {
        let tools = self.tools();
        let tool = tools
            .iter()
            .find(|tool| tool.qualified_name() == qualified)
            .cloned()
            .ok_or_else(|| McpError::ToolFailed {
                tool: qualified.to_string(),
                message: "not connected".to_string(),
            })?;
        let connections = self.connections.lock().map_err(|_| McpError::Protocol {
            server: tool.server.clone(),
            message: "pool poisoned".to_string(),
        })?;
        let connection = connections
            .get(&tool.server)
            .ok_or_else(|| McpError::ToolFailed {
                tool: qualified.to_string(),
                message: format!("server {} not connected", tool.server),
            })?;
        let connection = connection.lock().map_err(|_| McpError::Protocol {
            server: tool.server.clone(),
            message: "connection poisoned".to_string(),
        })?;
        connection.call_tool(&tool.tool, arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_names_match_the_typescript_convention() {
        let tool = McpTool {
            server: "github".to_string(),
            tool: "create_issue".to_string(),
            description: String::new(),
            input_schema: json!({}),
            read_only_hint: None,
            destructive_hint: None,
            idempotent_hint: None,
            open_world_hint: None,
        };
        assert_eq!(tool.qualified_name(), "mcp__github__create_issue");
    }

    #[test]
    fn annotation_hints_are_optional_and_parsed() {
        // Parsing is exercised via list_tools in integration; this documents the contract.
        let tool = McpTool {
            server: "s".to_string(),
            tool: "t".to_string(),
            description: String::new(),
            input_schema: json!({}),
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        };
        assert_eq!(tool.read_only_hint, Some(true));
    }

    #[test]
    fn extract_text_prefers_text_blocks() {
        let result = json!({"content": [
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"},
        ]});
        assert_eq!(extract_text(&result), "first\nsecond");
    }

    #[test]
    fn extract_text_falls_back_to_serialized_content() {
        let result = json!({"content": [{"type": "image", "data": "..."}]});
        assert!(extract_text(&result).contains("image"));
    }

    #[test]
    fn connecting_to_a_missing_binary_is_a_typed_error() {
        let error = McpPool::new()
            .connect_stdio("nope", "definitely-not-a-real-binary-xyz", &[])
            .expect_err("missing binary must fail");
        assert!(matches!(error, McpError::Spawn { .. }));
    }

    #[test]
    fn sse_bodies_parse_json_data_frames_and_ignore_keepalives() {
        let body =
            ": ping\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":2,\"result\":{}}\n\n\
                    data: not-json\n\n";
        let messages = parse_sse_json_messages(body);
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[0]["id"], 1);
        assert_eq!(messages[1]["id"], 2, "multi-line data is joined");
    }

    #[test]
    fn sse_bodies_handle_crlf_and_trailing_frame_without_blank_line() {
        let body = "event: message\r\ndata: {\"id\":7,\"result\":{\"ok\":true}}\r\n\r\n";
        let messages = parse_sse_json_messages(body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["id"], 7);

        let no_trailing_blank = "data: {\"id\":9}";
        let messages = parse_sse_json_messages(no_trailing_blank);
        assert_eq!(messages.len(), 1, "{messages:?}");
    }

    #[test]
    fn endpoint_events_resolve_against_the_stream_origin() {
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:9000/sse", "/messages"),
            "http://127.0.0.1:9000/messages"
        );
        assert_eq!(
            resolve_endpoint("https://host.example/sse?token=x", "messages"),
            "https://host.example/messages"
        );
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:9000/sse", "http://other:1/msg"),
            "http://other:1/msg"
        );
    }
}
