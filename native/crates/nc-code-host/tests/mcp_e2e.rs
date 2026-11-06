//! MCP e2e：host 的 MCP 客户端对**真实 MCP server 子进程**完成
//! initialize → tools/list → tools/call 全链路。
//!
//! server double 是测试进程内的真实子进程（不是 mock：它按 MCP JSON-RPC 协议
//! 应答），验证的是 host 客户端的协议实现而不是对象交互。

use nc_code_host::mcp::McpPool;
use serde_json::json;

const TEST_SERVER_JS: &str = r#"
const readline = require('node:readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (msg.method === 'initialize') {
    send({ jsonrpc: '2.0', id: msg.id, result: {
      protocolVersion: '2025-06-18',
      capabilities: { tools: {} },
      serverInfo: { name: 'test-server', version: '1.0.0' },
    }});
    return;
  }
  if (msg.method === 'tools/list') {
    send({ jsonrpc: '2.0', id: msg.id, result: { tools: [
      { name: 'echo', description: 'Echo the input back', inputSchema: {
        type: 'object', properties: { text: { type: 'string' } }, required: ['text'] } },
      { name: 'add', description: 'Add two numbers', inputSchema: {
        type: 'object', properties: { a: { type: 'number' }, b: { type: 'number' } }, required: ['a','b'] } },
    ] } });
    return;
  }
  if (msg.method === 'tools/call') {
    const name = msg.params && msg.params.name;
    const args = (msg.params && msg.params.arguments) || {};
    if (name === 'echo') {
      send({ jsonrpc: '2.0', id: msg.id, result: { content: [{ type: 'text', text: 'echo: ' + args.text }] } });
    } else if (name === 'add') {
      send({ jsonrpc: '2.0', id: msg.id, result: { content: [{ type: 'text', text: String(args.a + args.b) }] } });
    } else {
      send({ jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: 'unknown tool ' + name } });
    }
    return;
  }
  // notifications/*：无响应
});
function send(obj) { process.stdout.write(JSON.stringify(obj) + '\n'); }
setInterval(() => {}, 1 << 30);
"#;

#[test]
fn mcp_client_discovers_and_calls_real_server_tools() {
    // 用独立 node 进程池（断开即杀子进程）；pool 放在栈上，析构前必须 disconnect，
    // 否则 server 子进程持管道不放，调用方永远等不到测试结束。
    let pool = McpPool::new();
    let count = pool
        .connect_stdio(
            "test",
            "node",
            &["-e".to_string(), TEST_SERVER_JS.to_string()],
        )
        .expect("MCP server must connect");
    assert_eq!(count, 2, "two tools must be discovered");

    let tools = pool.tools();
    let names: Vec<String> = tools.iter().map(|tool| tool.qualified_name()).collect();
    assert!(
        names.contains(&"mcp__test__echo".to_string()),
        "tools: {names:?}"
    );
    assert!(
        names.contains(&"mcp__test__add".to_string()),
        "tools: {names:?}"
    );

    // 描述与 schema 来自 server（模型要据此正确调用）。
    let echo = tools.iter().find(|tool| tool.tool == "echo").expect("echo");
    assert_eq!(echo.description, "Echo the input back");
    assert_eq!(echo.input_schema["type"], json!("object"));
    assert_eq!(echo.input_schema["required"], json!(["text"]));

    // 真实调用：echo 与 add 都要拿到 server 的文本结果。
    let echoed = pool
        .call_tool("mcp__test__echo", &json!({"text": "hello native"}))
        .expect("echo must succeed");
    assert_eq!(echoed, "echo: hello native");

    let sum = pool
        .call_tool("mcp__test__add", &json!({"a": 2, "b": 3}))
        .expect("add must succeed");
    assert_eq!(sum, "5");

    // 未注册的工具必须报 typed error，不静默。
    let missing = pool.call_tool("mcp__test__nope", &json!({}));
    assert!(missing.is_err(), "unknown tool must fail");

    pool.disconnect("test");
    assert!(pool.tools().is_empty(), "disconnect must remove the tools");
}

#[test]
fn mcp_tools_survive_concurrent_calls() {
    let pool = std::sync::Arc::new(McpPool::new());
    pool.connect_stdio(
        "test",
        "node",
        &["-e".to_string(), TEST_SERVER_JS.to_string()],
    )
    .expect("connect");

    let handles: Vec<_> = (0..4)
        .map(|index| {
            let pool = std::sync::Arc::clone(&pool);
            std::thread::spawn(move || {
                pool.call_tool("mcp__test__add", &json!({"a": index, "b": 100}))
                    .map(|text| text.parse::<i64>().expect("numeric result"))
            })
        })
        .collect();
    let mut results: Vec<i64> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread").expect("call"))
        .collect();
    results.sort_unstable();
    assert_eq!(results, vec![100, 101, 102, 103]);
    // 关键：断开 pool 才能杀掉它 spawn 的 server 子进程——否则测试进程退出时
    // 子进程仍持有 stdout 管道，调用方（cargo / shell）会一直等管道关闭。
    pool.disconnect("test");
    assert!(pool.tools().is_empty());
}
