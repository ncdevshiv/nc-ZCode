//! MCP 客户端：把外部 MCP server 的工具并入 host 工具面（对齐 TS
//! `contracts/src/interfaces/mcp.port.ts` 的 `McpPort` 语义面）。
//!
//! 本 increment 只做 **stdio 传输**（`type: "stdio"`，最常见形态，也是唯一能在
//! host 里无额外运行时依赖承载的形态）：spawn 子进程 → JSON-RPC over stdio →
//! `initialize` → `tools/list` → `tools/call`。http/sse 与 OAuth 传输**未接线**
//! （README 差异表记录；需要 HTTP 客户端 + 凭据处理，语义面不同）。
//!
//! 工具面：MCP 工具以 `mcp__<server>__<tool>` 暴露给模型（与 TS 侧命名一致，
//! 避免与 nc-tools 点号名和 zcode 工具重名）。发现的工具**只读执行**：MCP server
//! 声明的 destructive 语义本 increment 不解析，因此统一按 needsApproval 处理
//! ——headless 无交互客户端，默认**拒绝**执行外部 MCP 工具（fail-closed），除非
//! `--full-access`。
//!
//! 隔离：每个 server 一个子进程，一个会话一份；断连即从工具面移除（不留死工具）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

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
                    eprintln!("[mcp:{server_name}] {line}");
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
        let _ = connection.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "zcode-host", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
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

/// server 名 → 连接 + 工具目录。
#[derive(Default)]
pub struct McpPool {
    connections: Mutex<HashMap<String, Arc<Mutex<McpConnection>>>>,
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
        if let Ok(mut tools) = self.tools.lock() {
            tools.retain(|tool| tool.server != server);
            tools.extend(discovered.clone());
        }
        if let Ok(mut connections) = self.connections.lock() {
            if let Some(existing) = connections.remove(server) {
                if let Ok(existing) = existing.lock() {
                    existing.shutdown();
                }
            }
            connections.insert(server.to_string(), Arc::new(Mutex::new(connection)));
        }
        Ok(count)
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
        };
        assert_eq!(tool.qualified_name(), "mcp__github__create_issue");
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
}
