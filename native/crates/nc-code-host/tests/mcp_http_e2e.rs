//! MCP HTTP transports: streamable HTTP and legacy HTTP+SSE against real sockets.
//!
//! A tiny HTTP/1.1 server is built here (test-only) so the production client talks to
//! genuine TCP + HTTP frames; there is no mock transport in the product code.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nc_code_host::mcp::McpPool;
use serde_json::{json, Value};

/// Streamable-HTTP MCP server: one JSON reply per POST, session id header on initialize.
struct StreamableServer {
    port: u16,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    session_headers: Arc<Mutex<Vec<Option<String>>>>,
}

impl StreamableServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let session_headers = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);
        let headers_for_thread = Arc::clone(&session_headers);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let requests = Arc::clone(&requests_for_thread);
                let headers = Arc::clone(&headers_for_thread);
                std::thread::spawn(move || {
                    handle_streamable(stream, requests, headers);
                });
            }
        });
        Self {
            port,
            requests,
            session_headers,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }
}

/// `(request line, lower-cased headers, body)` of one parsed HTTP/1.1 request.
type ParsedRequest = (String, Vec<(String, String)>, String);

fn read_request(stream: &mut TcpStream) -> Option<ParsedRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some((
        request_line.trim().to_string(),
        headers,
        String::from_utf8_lossy(&body).to_string(),
    ))
}

fn write_json(stream: &mut TcpStream, status: &str, body: &str, extra: &[(&str, &str)]) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn reply_for_request(request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    match request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "streamable-test", "version": "1.0.0"},
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": [
                {
                    "name": "echo",
                    "description": "echo text",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
                    "annotations": {"readOnlyHint": true}
                },
                {"name": "fail", "description": "always errors", "inputSchema": {"type": "object"}}
            ]}
        }),
        "tools/call" => {
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let text = request
                .pointer("/params/arguments/text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name == "fail" {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"isError": true, "content": [{"type": "text", "text": "boom"}]}
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"content": [{"type": "text", "text": format!("echo:{text}")}]}
                })
            }
        }
        "notifications/initialized" | "notifications/cancelled" => json!({}),
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("method not found: {other}")}
        }),
    }
}

fn handle_streamable(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    session_headers: Arc<Mutex<Vec<Option<String>>>>,
) {
    let Some((line, headers, body)) = read_request(&mut stream) else {
        return;
    };
    let session = headers
        .iter()
        .find(|(name, _)| name == "mcp-session-id")
        .map(|(_, value)| value.clone());
    if let Ok(mut seen) = session_headers.lock() {
        seen.push(session);
    }
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if let Ok(mut seen) = requests.lock() {
        seen.push((line.clone(), parsed.clone()));
    }
    if parsed.get("method").and_then(Value::as_str) == Some("notifications/initialized") {
        // Notifications have no id; the streamable transport answers 202 with no body.
        let _ = stream
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let reply = reply_for_request(&parsed);
    let is_initialize = parsed.get("method").and_then(Value::as_str) == Some("initialize");
    let extra: Vec<(&str, &str)> = if is_initialize {
        vec![("Mcp-Session-Id", "sess-1")]
    } else {
        vec![]
    };
    write_json(&mut stream, "200 OK", &reply.to_string(), &extra);
}

#[test]
fn streamable_http_connect_lists_tools_and_calls_them() {
    let server = StreamableServer::start();
    let pool = McpPool::new();
    let count = pool
        .connect_http("remote", &server.url(), Vec::new())
        .expect("connect_http");
    assert_eq!(count, 2, "both tools discovered");

    let tools = pool.tools();
    let names: Vec<&str> = tools.iter().map(|tool| tool.tool.as_str()).collect();
    assert!(
        names.contains(&"echo") && names.contains(&"fail"),
        "{names:?}"
    );
    let echo = tools
        .iter()
        .find(|tool| tool.tool == "echo")
        .expect("echo tool");
    assert_eq!(echo.qualified_name(), "mcp__remote__echo");
    assert_eq!(echo.read_only_hint, Some(true));

    let text = pool
        .call_tool("mcp__remote__echo", &json!({"text": "hi"}))
        .expect("call echo");
    assert_eq!(text, "echo:hi");

    // isError results are typed failures, not silent successes.
    let error = pool
        .call_tool("mcp__remote__fail", &json!({}))
        .expect_err("fail tool must error");
    assert!(format!("{error}").contains("boom"), "{error}");

    // The session id from the initialize response was replayed on later requests.
    let headers = server.session_headers.lock().expect("headers");
    let had_session = headers.iter().flatten().any(|value| value == "sess-1");
    assert!(had_session, "Mcp-Session-Id must be replayed: {headers:?}");
    let methods: Vec<String> = server
        .requests
        .lock()
        .expect("requests")
        .iter()
        .filter_map(|(_, body)| {
            body.get("method")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert!(methods.contains(&"initialize".to_string()));
    assert!(methods.contains(&"notifications/initialized".to_string()));
    assert!(methods.contains(&"tools/list".to_string()));
    assert!(methods.contains(&"tools/call".to_string()));
}

#[test]
fn unreachable_http_endpoint_fails_closed() {
    let pool = McpPool::new();
    let error = pool
        .connect_http("dead", "http://127.0.0.1:1/mcp", Vec::new())
        .expect_err("unreachable endpoint must fail");
    let text = format!("{error}");
    assert!(
        text.contains("failed") || text.contains("HTTP"),
        "unexpected error: {text}"
    );
    assert!(pool.tools().is_empty(), "no tools from a dead server");
}

// ── Legacy HTTP+SSE ─────────────────────────────────────────────────────────────

/// Legacy SSE server: `GET /sse` announces `endpoint: /messages`, answers to POSTs are
/// written onto the live stream, exactly as the 2024-11-05 transport specifies.
struct SseServer {
    port: u16,
    posts: Arc<AtomicUsize>,
}

impl SseServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let posts = Arc::new(AtomicUsize::new(0));
        let posts_for_thread = Arc::clone(&posts);
        std::thread::spawn(move || {
            let stream_writer: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let writer = Arc::clone(&stream_writer);
                let posts = Arc::clone(&posts_for_thread);
                std::thread::spawn(move || {
                    handle_sse(stream, writer, posts);
                });
            }
        });
        Self { port, posts }
    }

    fn sse_url(&self) -> String {
        format!("http://127.0.0.1:{}/sse", self.port)
    }
}

fn handle_sse(
    mut stream: TcpStream,
    writer: Arc<Mutex<Option<TcpStream>>>,
    posts: Arc<AtomicUsize>,
) {
    let Some((line, _headers, body)) = read_request(&mut stream) else {
        return;
    };
    if line.starts_with("GET /sse") {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(b"event: endpoint\ndata: /messages\n\n");
        let _ = stream.flush();
        if let Ok(mut writer) = writer.lock() {
            *writer = Some(stream);
        }
        // Keep the thread alive until the client closes the stream (short bound: the
        // assertion below completes long before this).
        std::thread::sleep(std::time::Duration::from_secs(3));
        return;
    }
    if line.starts_with("POST /messages") {
        posts.fetch_add(1, Ordering::SeqCst);
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if parsed.get("method").and_then(Value::as_str) == Some("notifications/initialized") {
            let _ = stream.write_all(
                b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        }
        let reply = reply_for_request(&parsed);
        let _ = stream
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = stream.flush();
        if let Ok(writer) = writer.lock() {
            if let Some(live) = writer.as_ref() {
                let mut live = live;
                let _ = live.write_all(format!("event: message\ndata: {reply}\n\n").as_bytes());
                let _ = live.flush();
            }
        }
        return;
    }
    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
}

#[test]
fn legacy_sse_transport_handshakes_and_answers_over_the_stream() {
    let server = SseServer::start();
    let pool = McpPool::new();
    let count = pool
        .connect_http("legacy", &server.sse_url(), Vec::new())
        .expect("legacy sse connect");
    assert_eq!(count, 2, "legacy server exposes both tools");
    assert_eq!(pool.tools().len(), 2);
    let text = pool
        .call_tool("mcp__legacy__echo", &json!({"text": "over sse"}))
        .expect("call over sse");
    assert_eq!(text, "echo:over sse");
    assert!(
        server.posts.load(Ordering::SeqCst) >= 3,
        "initialize + tools/list + tools/call must POST to the announced endpoint"
    );
}
