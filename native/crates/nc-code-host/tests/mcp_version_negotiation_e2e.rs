//! MCP version negotiation against a real server subprocess.
//!
//! `mcp_e2e.rs` covers the happy path with a server that echoes the version we offer. This file
//! covers the two branches a pinned `protocolVersion` used to hide: a server that selects an
//! older version this client also implements, and one that selects a version it does not. The
//! refusal happens during `initialize`, before any tool is discovered, so the point of the test
//! is that a mismatched wire format is refused **loudly** instead of being run anyway.

use nc_code_host::mcp::{McpError, McpPool};
use serde_json::json;

/// A minimal MCP server whose `initialize` reply is chosen by the argument after the script:
/// `older` answers a version this client implements, `newer` answers one it does not, and
/// `silent` omits the field the way pre-spec servers do.
const SERVER_JS: &str = r#"
const readline = require('node:readline');
const mode = process.argv[1] || 'older';
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (msg.method === 'initialize') {
    const result = {
      capabilities: { tools: {} },
      serverInfo: { name: 'negotiating-server', version: '0.1.0' },
    };
    if (mode === 'older') result.protocolVersion = '2024-11-05';
    if (mode === 'newer') result.protocolVersion = '2099-01-01';
    send({ jsonrpc: '2.0', id: msg.id, result });
    return;
  }
  if (msg.method === 'tools/list') {
    send({ jsonrpc: '2.0', id: msg.id, result: { tools: [
      { name: 'ping', description: 'Ping', inputSchema: { type: 'object', properties: {} } },
    ] } });
    return;
  }
  if (msg.method === 'tools/call') {
    send({ jsonrpc: '2.0', id: msg.id, result: { content: [{ type: 'text', text: 'pong' }] } });
    return;
  }
});
function send(obj) { process.stdout.write(JSON.stringify(obj) + '\n'); }
setInterval(() => {}, 1 << 30);
"#;

fn connect(pool: &McpPool, server: &str, mode: &str) -> Result<usize, McpError> {
    pool.connect_stdio(
        server,
        "node",
        &["-e".to_string(), SERVER_JS.to_string(), mode.to_string()],
    )
}

#[test]
fn a_server_choosing_an_older_supported_version_still_connects() {
    let pool = McpPool::new();
    let discovered = connect(&pool, "older", "older")
        .expect("2024-11-05 is implemented, so the server's choice is adopted");
    assert_eq!(
        discovered, 1,
        "the negotiation must not eat the tool surface"
    );
    pool.disconnect("older");
}

#[test]
fn a_pre_spec_server_keeps_the_version_the_client_offered() {
    let pool = McpPool::new();
    assert_eq!(
        connect(&pool, "silent", "silent")
            .expect("a reply without protocolVersion is a pre-spec server, not a refusal"),
        1
    );
    pool.disconnect("silent");
}

#[test]
fn a_server_choosing_an_unimplemented_version_is_refused_by_name() {
    let pool = McpPool::new();
    let error = connect(&pool, "newer", "newer")
        .expect_err("a version this client does not speak must refuse the connection");
    match error {
        McpError::Protocol { server, message } => {
            assert_eq!(server, "newer");
            assert!(
                message.contains("2099-01-01") && message.contains("2025-06-18"),
                "the refusal must name what the server picked and what the client speaks: {message}"
            );
        }
        other => panic!("expected a protocol refusal, got {other:?}"),
    }
    pool.disconnect("newer");
}

#[test]
fn the_negotiated_connection_is_the_one_tools_are_called_through() {
    // The refusal above must not be a general "this server is broken": the same script, answering
    // with a supported version, reaches tools/call.
    let pool = McpPool::new();
    connect(&pool, "call", "older").expect("connect");
    assert_eq!(
        pool.call_tool("mcp__call__ping", &json!({})).expect("ping"),
        "pong"
    );
    pool.disconnect("call");
}
