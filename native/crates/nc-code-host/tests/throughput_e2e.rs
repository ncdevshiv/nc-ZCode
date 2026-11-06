//! Throughput (tok/s) e2e: the assistant terminal row must carry throughput measured from the real SSE block arrival times.
//!
//! Why this deserves its own pin: TPS is a measured number the product footer displays. If the window came from "after the event vector is returned"
//! (rather than the transport's block arrival times), the measurement is always 0 and the footer would miss this chip forever —
//! this assertion pins that distinction in the protocol shape.

mod common;

use common::*;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

/// An anthropic SSE that flushes block by block: three text deltas, 40ms between blocks, usage in the tail frame.
fn spawn_slow_provider(texts: &[&'static str], gap: Duration, output_tokens: u64) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider");
    let port = listener.local_addr().expect("addr").port();
    let texts: Vec<String> = texts.iter().map(|value| (*value).to_string()).collect();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let texts = texts.clone();
            thread::spawn(move || {
                serve_slow_connection(stream, texts, gap, output_tokens);
            });
        }
    });
    port
}

fn serve_slow_connection(
    mut stream: TcpStream,
    texts: Vec<String>,
    gap: Duration,
    output_tokens: u64,
) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
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
    if !body.is_empty() {
        let _ = reader.read_exact(&mut body);
    }
    // Chunked flush: only when the bytes really go out segment by segment do the host-side block arrival times have any span.
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n";
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.flush();
    let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_tps\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n";
    let _ = stream.write_all(start.as_bytes());
    let _ = stream.flush();
    thread::sleep(gap);
    let block = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
    let _ = stream.write_all(block.as_bytes());
    for text in &texts {
        let escaped = serde_json::to_string(text).expect("escape");
        let delta = format!("event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{escaped}}}}}\n\n");
        let _ = stream.write_all(delta.as_bytes());
        let _ = stream.flush();
        thread::sleep(gap);
    }
    let stop = "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
    let _ = stream.write_all(stop.as_bytes());
    let tail = format!("event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":{output_tokens}}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n");
    let _ = stream.write_all(tail.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

fn assistant_rows(client: &mut Client, session_id: &str, budget: u32) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for _ in 0..budget {
        let line = client.next_line();
        if line.get("method").and_then(Value::as_str) != Some("v4/conversation/frame") {
            continue;
        }
        let Some(params) = line.get("params") else {
            continue;
        };
        let frame = params.get("frame").cloned().unwrap_or(Value::Null);
        let deltas = frame["payload"]["deltas"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        apply_to_rows(&mut rows, &deltas);
        let done = rows
            .iter()
            .any(|row| row["kind"] == json!("assistantText") && row["state"] == json!("complete"));
        if done {
            return rows;
        }
    }
    let _ = session_id;
    rows
}

#[test]
fn assistant_final_row_carries_measured_throughput() {
    let port = spawn_slow_provider(
        &["Hello ", "from ", "the scripted provider."],
        Duration::from_millis(40),
        24,
    );
    let base = std::env::temp_dir().join(format!("zcode-host-tps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data");
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

    let mut process = HostProcess::spawn(&workspace, &data_dir, &provider_config, true);
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
        json!({"sessionId": session_id, "connectionId": "conn-tps", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-turn",
            Some(&session_id),
            json!({"text": "greet me"}),
        ),
    );
    let _ = client.response(id);

    let rows = assistant_rows(&mut client, &session_id, 80);
    let assistant = rows
        .iter()
        .find(|row| row["kind"] == json!("assistantText") && row["state"] == json!("complete"))
        .expect("completed assistantText row");
    let throughput = assistant.get("throughput").cloned().unwrap_or(Value::Null);
    assert!(
        throughput.is_object(),
        "assistant final row must carry measured throughput: {assistant}"
    );
    assert_eq!(throughput["completionTokens"], json!(24));
    assert!(
        throughput["tokensPerSecond"].as_f64().unwrap_or(0.0) > 0.0,
        "tokensPerSecond must be a positive measured value: {throughput}"
    );
    assert_eq!(throughput["windowSource"], json!("generation"));
    assert!(
        throughput["elapsedMs"].as_u64().unwrap_or(0) >= 80,
        "three deltas at 40ms spacing must span >=80ms: {throughput}"
    );
    assert_eq!(throughput["estimated"], json!(false));
    let _ = std::fs::remove_dir_all(base);
}
