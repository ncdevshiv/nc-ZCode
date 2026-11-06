//! Background-task completion notification e2e (in-process): after a bash background command ends, the host
//! **starts a turn itself** in the parent session as a `backgroundResult` turn, and the model sees the `<task-notification>` XML and responds.
//!
//! Use HostServer's in-process entry point rather than stdio: the crux of this chain is "no one sends another command,
//! the host must run the notifications sitting in the queue into a turn by itself", and in-process we can assert the row projection directly; blocking reads also won't
//! disguise a failure as a timeout.

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
    response.and_then(|line| serde_json::from_str::<Value>(&line).ok())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_background_command_starts_a_background_result_turn() {
    let port = spawn_provider(vec![
        tool_use(
            "toolu-bg",
            "Bash",
            &json!({"command": "echo notify-probe", "run_in_background": true, "description": "notify probe"}),
        ),
        // The parent turn wraps up.
        text_sse("started the probe"),
        // Notification turn: the input the model sees is exactly the `<task-notification>` XML.
        text_sse("notification received"),
    ]);
    let base = std::env::temp_dir().join(format!("zcode-bg-notify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
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
    let (server, _outbox) =
        HostServer::new(workspace, data_dir, Some(provider), true, vec![]).unwrap();

    let response = send(
        &server,
        1,
        "v4/command",
        envelope("createSession", "cmd-create", None, json!({})),
    )
    .await
    .expect("createSession");
    let session_id = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    let _ = send(
        &server,
        2,
        "v4/command",
        envelope(
            "sendText",
            "cmd-start",
            Some(&session_id),
            json!({"text": "start the probe"}),
        ),
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut rows: Vec<Value> = Vec::new();
    while Instant::now() < deadline {
        if let Some(view) = server.session_transcript_view(&session_id) {
            rows = view["rows"].as_array().cloned().unwrap_or_default();
            let notification_input = rows.iter().any(|row| {
                row["kind"] == json!("userInput")
                    && row["origin"] == json!("backgroundResult")
                    && row["text"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("<task-notification>")
            });
            let notification_turn = rows.iter().any(|row| {
                row["kind"] == json!("turnHeader") && row["origin"] == json!("backgroundResult")
            });
            let answered = rows.iter().any(|row| {
                row["kind"] == json!("assistantText")
                    && row["text"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("notification received")
            });
            if notification_input && notification_turn && answered {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let notification_input = rows
        .iter()
        .find(|row| row["kind"] == json!("userInput") && row["origin"] == json!("backgroundResult"))
        .cloned()
        .unwrap_or_else(|| panic!("no backgroundResult input row in {rows:?}"));
    let text = notification_input["text"].as_str().unwrap_or_default();
    assert!(text.contains("<task-notification>"), "{text}");
    assert!(text.contains("<status>completed</status>"), "{text}");
    assert!(
        text.contains("Background command \"notify probe\" completed"),
        "the summary must name the description: {text}"
    );
    assert!(
        text.contains("<output-file>"),
        "the model needs the output file to read more: {text}"
    );
    assert!(
        rows.iter()
            .any(|row| row["kind"] == json!("turnHeader")
                && row["origin"] == json!("backgroundResult")),
        "the notification must be a real turnHeader: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| {
            row["kind"] == json!("assistantText")
                && row["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("notification received")
        }),
        "the model must answer the notification turn: {rows:?}"
    );
    let notification_inputs = rows
        .iter()
        .filter(|row| {
            row["kind"] == json!("userInput") && row["origin"] == json!("backgroundResult")
        })
        .count();
    assert_eq!(notification_inputs, 1, "duplicate notification: {rows:?}");

    let _ = std::fs::remove_dir_all(&base);
}
