//! Turn-level e2e for ReadSessionContext: content said in session A is read back by the model through a tool call in session B.
//!
//! Runs the real HostServer (in-process) + real transcript persistence + a scripted provider;
//! `SessionContextPort` is implemented by HostServer itself (reading the transcript), with no stand-in.
//! The `sessionId` in the script needs session A's real id, so first create A, then start the provider, then run two turns.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use nc_code_host::provider_config::ProviderConfig;
use nc_code_host::HostServer;
use serde_json::{json, Value};

fn text_sse(text: &str) -> String {
    let text = serde_json::to_string(text).unwrap();
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{text}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn tool_use(id: &str, name: &str, input: &Value) -> String {
    let id = serde_json::to_string(id).unwrap();
    let name = serde_json::to_string(name).unwrap();
    let input = serde_json::to_string(&input.to_string()).unwrap();
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":{id},\"name\":{name},\"input\":{{}}}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{input}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":5}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn spawn_provider(script: Vec<String>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let script = Arc::new(script);
    let counter = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let script = Arc::clone(&script);
            let counter = Arc::clone(&counter);
            thread::spawn(move || serve(&mut stream, &script, &counter));
        }
    });
    port
}

fn serve(stream: &mut TcpStream, script: &Arc<Vec<String>>, counter: &Arc<AtomicUsize>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut length = 0usize;
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
                    length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let index = counter.fetch_add(1, Ordering::SeqCst);
        let payload = &script[index.min(script.len() - 1)];
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

fn envelope(
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

async fn send(server: &Arc<HostServer>, id: u64, method: &str, params: Value) -> Option<Value> {
    let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let (response, _) = server.handle_line(&line).await;
    response
        .and_then(|line| serde_json::from_str::<Value>(&line).ok())
        .filter(|value| value.get("error").is_none())
}

async fn wait_for<F>(server: &Arc<HostServer>, session_id: &str, predicate: F) -> Vec<Value>
where
    F: Fn(&[Value]) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(view) = server.session_transcript_view(session_id) {
            let rows = view["rows"].as_array().cloned().unwrap_or_default();
            if predicate(&rows) {
                return rows;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for rows in {session_id}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_session_reads_the_first_sessions_context_through_a_model_tool_call() {
    let base = std::env::temp_dir().join(format!("zcode-rsc-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // Session A is created with a host that has **no provider**: createSession needs no model,
    // and the task argument in the script needs A's real id, so we must get it before starting the provider.
    let (minter, _) =
        HostServer::new(workspace.clone(), data_dir.clone(), None, true, vec![]).unwrap();
    let response = send(
        &minter,
        1,
        "v4/command",
        envelope("createSession", "cmd-a", None, json!({})),
    )
    .await
    .expect("session A response");
    let session_a = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("session A id")
        .to_string();
    drop(minter);

    let port = spawn_provider(vec![
        text_sse("noted"),
        tool_use(
            "toolu-read",
            "ReadSessionContext",
            &json!({"sessionId": session_a, "query": "passphrase"}),
        ),
        text_sse("found it"),
    ]);
    let config_path = base.join("provider.json");
    std::fs::write(
        &config_path,
        json!({
            "kind": "anthropic",
            "baseUrl": format!("http://127.0.0.1:{port}"),
            "apiKey": "test-key-not-a-real-credential",
            "model": "test-model",
            "maxTokens": 1024,
        })
        .to_string(),
    )
    .unwrap();
    let provider = ProviderConfig::load(&config_path).expect("provider config");

    // The same workspace/data-dir: A's transcript is already on disk, and resume_sessions reads it back.
    let (server, _outbox) =
        HostServer::new(workspace, data_dir, Some(provider), true, vec![]).unwrap();
    server.resume_sessions();

    let _ = send(
        &server,
        2,
        "v4/command",
        envelope(
            "sendText",
            "cmd-a-text",
            Some(&session_a),
            json!({"text": "the passphrase is ORANGE-42, remember it"}),
        ),
    )
    .await;
    wait_for(&server, &session_a, |rows| {
        rows.iter().any(|row| {
            row["kind"] == json!("assistantText")
                && row["text"].as_str().unwrap_or_default().contains("noted")
        })
    })
    .await;

    let response = send(
        &server,
        3,
        "v4/command",
        envelope("createSession", "cmd-b", None, json!({})),
    )
    .await
    .expect("session B response");
    let session_b = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("session B id")
        .to_string();
    let _ = send(
        &server,
        4,
        "v4/command",
        envelope(
            "sendText",
            "cmd-b-text",
            Some(&session_b),
            json!({"text": "what was the passphrase from the other session?"}),
        ),
    )
    .await;

    let rows = wait_for(&server, &session_b, |rows| {
        rows.iter().any(|row| {
            row["kind"] == json!("toolCall")
                && row["toolName"] == json!("ReadSessionContext")
                && row["status"] == json!("success")
        })
    })
    .await;
    let tool_row = rows
        .iter()
        .find(|row| {
            row["kind"] == json!("toolCall") && row["toolName"] == json!("ReadSessionContext")
        })
        .cloned()
        .expect("tool row");
    let output = tool_row["output"]["text"].as_str().unwrap_or_default();
    assert!(output.contains("\"source\":\"local\""), "{output}");
    assert!(
        output.contains("ORANGE-42"),
        "session B must read what session A said: {output}"
    );

    let _ = std::fs::remove_dir_all(&base);
}
