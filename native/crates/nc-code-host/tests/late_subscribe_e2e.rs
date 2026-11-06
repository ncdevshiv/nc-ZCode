//! 迟到订阅者 e2e：会话**已经跑过一轮**之后才第一次 subscribe，也必须拿到
//! response + initial snapshot。
//!
//! WHY 这个场景单独成文：分支截断 e2e 在开发中先命中过这条路径——turn 已收口、
//! 订阅者列表为空时，第一个 subscribe 的响应必须仍然出现。手机远控与桌面重连都是
//! "迟到订阅者"，拿不到快照等于会话白跑。
//!
//! 读取带超时：被测路径哪天阻塞的话，测试必须失败，而不是把整个套件挂住。

mod common;

use common::{command_envelope, Client, HostProcess, ScriptedProvider};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn provider_config(port: u16, path: &std::path::Path) {
    std::fs::write(
        path,
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
}

/// stdout 交给读线程（管道读做不到带超时），主线程保留 stdin 发请求，
/// 从通道里按超时收协议行。
fn spawn_line_reader(
    stdout: std::io::BufReader<std::process::ChildStdout>,
) -> mpsc::Receiver<Value> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let stdout = stdout;
        for line in stdout.lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if tx.send(parsed).is_err() {
                return;
            }
        }
    });
    rx
}

fn send_line(stdin: &mut std::process::ChildStdin, request: &Value) {
    let line = serde_json::to_string(request).expect("serialize request");
    stdin
        .write_all(line.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .expect("write request");
}

fn recv_within(rx: &mpsc::Receiver<Value>, what: &str) -> Value {
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(line) => line,
        Err(error) => panic!("{what} never arrived within 5s: {error}"),
    }
}

#[test]
fn a_subscriber_that_arrives_after_the_first_turn_still_gets_its_snapshot() {
    let (provider, port) = ScriptedProvider::spawn_with_delay(Duration::from_millis(1));
    let base = std::env::temp_dir().join(format!("zcode-host-late-sub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base.join("provider.json");
    provider_config(port, &config_path);

    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, false);
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

    // 无订阅者的第一轮：命令仍要受理并回 ack（帧没处发不是拒绝的理由）。
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-t1",
            Some(&session_id),
            json!({"text": "ran without a subscriber"}),
        ),
    );
    let ack = client.response(id);
    assert_eq!(ack["result"]["status"], json!("accepted"));
    // 先等这一轮真正收口（账本里出现终态 ack），否则迟到订阅者的响应会排在在途
    // eager 帧之后，被测的"迟到"条件就变成"中途订阅"了。
    let mut settled = Value::Null;
    for _ in 0..100 {
        let id = client.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": session_id, "commandId": "cmd-t1"}]}),
        );
        let response = client.response(id);
        settled = response["result"]["results"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        if settled["result"] != json!("unknown") && settled["result"] != Value::Null {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        settled["result"]["status"].is_string(),
        "a turn must settle into the command ledger with zero subscribers: {settled}"
    );
    assert_eq!(provider.request_bodies().len(), 1, "the turn really ran");

    // ── 迟到订阅者：从这里开始按超时收帧 ──
    let mut stdin = client.stdin;
    let rx = spawn_line_reader(client.stdout);
    send_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 900,
            "method": "v4/conversation/subscribe",
            "params": {
                "sessionId": session_id,
                "connectionId": "conn-late",
                "clientMode": "desktop-continuous"
            }
        }),
    );
    let response = recv_within(&rx, "late subscribe response");
    assert_eq!(
        response["id"].as_u64(),
        Some(900),
        "the late subscriber's own response must come back: {response}"
    );
    assert!(
        response["result"]["ack"]["subscriptionId"].is_string(),
        "subscribe must answer with its ack: {response}"
    );
    let frame = recv_within(&rx, "late initial snapshot");
    let snapshot = &frame["params"]["frame"]["payload"]["snapshot"];
    assert_eq!(
        snapshot["sessionId"].as_str(),
        Some(session_id.as_str()),
        "the late subscriber gets the snapshot of the session it asked for"
    );
    assert!(
        snapshot["revision"].as_i64().unwrap_or(0) >= 1,
        "the snapshot carries the authoritative revision the next CAS command needs"
    );
    let window = snapshot["rows"]["window"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        window.iter().any(|row| {
            row["kind"] == json!("turnHeader") && row["state"] == json!("completedSuccess")
        }),
        "the completed turn must be visible to a subscriber that arrived afterwards: {window:?}"
    );

    let _ = std::fs::remove_dir_all(&base);
}
