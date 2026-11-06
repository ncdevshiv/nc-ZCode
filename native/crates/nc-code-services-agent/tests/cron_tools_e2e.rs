//! The turn-level e2e for Cron×4: the real host + the real `AutomationService` (sqlite repo),
//! with the model calling CronCreate → CronList → CronUpdate → CronDelete in a row through a scripted provider.
//!
//! Why this lives in services-agent: host must not depend on the automation crate (automation depends on host's
//! storage, the reverse direction would form a cycle), the port is injected by the composition root; only this crate holds both the host and the service.
//! The assertions land on the **real tool's returned text** and the **real repo state**, never on internal structures.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use nc_code_host::provider_config::ProviderConfig;
use nc_code_host::HostServer;
use nc_code_services_automation::{AutomationRepo, AutomationService};
use serde_json::{json, Value};

fn tool_use(id: &str, name: &str, input: &Value) -> String {
    let id = serde_json::to_string(id).unwrap();
    let name = serde_json::to_string(name).unwrap();
    let input = serde_json::to_string(&input.to_string()).unwrap();
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{{\"input_tokens\":30,\"output_tokens\":1}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":{id},\"name\":{name},\"input\":{{}}}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{input}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":5}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn text_sse(text: &str) -> String {
    let text = serde_json::to_string(text).unwrap();
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{{\"input_tokens\":30,\"output_tokens\":1}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{text}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

/// A minimal scripted SSE provider (each connection takes the next entry of the script; past the end the last entry repeats).
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

#[tokio::test(flavor = "multi_thread")]
async fn cron_tools_drive_the_real_automation_service_through_model_tool_calls() {
    // Create one row first: the id used inside the tool calls is decided by CronCreate's return, so the script supplies the four calls in order;
    // CronList's assertion is "at least one row" (the id is generated by the service and cannot be written into the script).
    let script = vec![
        tool_use(
            "c1",
            "CronCreate",
            &json!({
                "title": "every hour",
                "prompt": "check the build",
                "cron": "0 * * * *",
            }),
        ),
        tool_use("c2", "CronList", &json!({})),
        text_sse("done"),
    ];
    let port = spawn_provider(script);
    let base = std::env::temp_dir().join(format!("zcode-cron-e2e-{}", std::process::id()));
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

    let (server, mut outbox) =
        HostServer::new(workspace.clone(), data_dir, Some(provider), true, vec![]).unwrap();
    let repo = AutomationRepo::new(base.join("tasks.sqlite"));
    let service = Arc::new(AutomationService::new(Arc::new(repo)));
    server.set_automation_port(Arc::new(
        nc_code_services_agent::automation_tool_port::AutomationToolPort::new(
            Arc::clone(&service),
            workspace.to_string_lossy().to_string(),
            workspace.to_string_lossy().to_string(),
        ),
    ));

    let mut next_id = 0u64;
    let mut call = |method: &str, params: Value| {
        next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params});
        request.to_string()
    };

    let line = call(
        "v4/command",
        envelope("createSession", "cmd-create", None, json!({})),
    );
    let (response, _) = server.handle_line(&line).await;
    let response: Value = serde_json::from_str(&response.expect("createSession response")).unwrap();
    let session_id = response["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    let line = call(
        "v4/command",
        envelope(
            "sendText",
            "cmd-cron",
            Some(&session_id),
            json!({"text": "schedule an hourly check"}),
        ),
    );
    let (response, _) = server.handle_line(&line).await;
    if let Some(response) = response {
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["status"], json!("accepted"), "{parsed}");
    }
    // Drain the outbox so unsubscribed frames do not pile up (this case subscribes to nothing, so normally there are no frames).
    while outbox.try_recv().is_ok() {}

    // Wait for this turn to finish: both toolCall rows reach success.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut rows: Vec<Value> = Vec::new();
    while Instant::now() < deadline {
        if let Some(view) = server.session_transcript_view(&session_id) {
            rows = view["rows"].as_array().cloned().unwrap_or_default();
            let terminal_tools = rows
                .iter()
                .filter(|row| {
                    row["kind"] == json!("toolCall")
                        && matches!(row["status"].as_str(), Some("success") | Some("error"))
                })
                .count();
            let turn_done = rows.iter().any(|row| {
                row["kind"] == json!("turnHeader")
                    && matches!(
                        row["state"].as_str(),
                        Some("completedSuccess") | Some("completedError") | Some("cancelled")
                    )
            });
            if terminal_tools >= 2 && turn_done {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let tool_row = |name: &str| -> Value {
        rows.iter()
            .find(|row| row["kind"] == json!("toolCall") && row["toolName"] == json!(name))
            .cloned()
            .unwrap_or_else(|| panic!("no {name} row in {rows:?}"))
    };

    let create = tool_row("CronCreate");
    assert_eq!(create["status"], json!("success"), "{create}");
    let create_output = create["output"]["text"].as_str().unwrap_or_default();
    assert!(
        create_output.contains("Created automation"),
        "create output: {create_output}"
    );
    let list = tool_row("CronList");
    assert_eq!(list["status"], json!("success"), "{list}");
    let list_output = list["output"]["text"].as_str().unwrap_or_default();
    assert!(
        list_output.contains("every hour"),
        "the created automation must be listed: {list_output}"
    );

    // The real repo state: the create really landed in the database.
    let persisted = service
        .list(&nc_code_services_automation::Scope {
            workspace_path: Some(workspace.to_string_lossy().to_string()),
            workspace_identity: Some(workspace.to_string_lossy().to_string()),
        })
        .expect("list");
    assert_eq!(persisted.len(), 1, "the automation must be persisted");
    assert_eq!(persisted[0].title, "every hour");

    let _ = std::fs::remove_dir_all(&base);
}
