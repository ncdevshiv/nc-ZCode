//! Tool-loop e2e：真实工具执行，由本地脚本化 SSE provider 驱动。
//!
//! 场景 A（full-access）：模型回合 1 返回 tool_use(Write) → host 执行真实写入 →
//! tool_result 回灌 → 模型回合 2 返回终态文本。断言：文件真实落盘、toolCall 行
//! success、assistant 终态文本、第二个请求体里出现 tool_result（回灌证据）。
//! 场景 B（无 full-access）：同一 tool_use 被权限闸门拒绝 → tool_result 携带
//! typed 拒绝 → 文件不存在（fail-closed 证据）。
//!
//! provider double 只活在测试进程里（Tcp listener + 静态 SSE 脚本），不是产品
//! 代码路径上的 mock；产品代码零测试替身。

mod common;

use common::*;
use serde_json::{json, Value};
use std::time::Duration;

struct Scenario {
    name: &'static str,
    full_access: bool,
    /// Some(optionId)：收到 permission 交互时的应答（deny / allowOnce）。
    resolve_permission: Option<&'static str>,
}

fn run_scenario(scenario: &Scenario) {
    let (provider, port) = ScriptedProvider::spawn();
    let base = std::env::temp_dir().join(format!(
        "zcode-host-toolloop-{}-{}",
        scenario.name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");

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

    let mut process = HostProcess::spawn(
        &workspace,
        &data_dir,
        &provider_config,
        scenario.full_access,
    );
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
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot frame

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-send-1",
            Some(&session_id),
            json!({"text": "write the marker file"}),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));

    let mut rows = Vec::new();
    let mut converged = false;
    let mut resolved_permission = false;
    let mut saw_permission_interaction = false;
    let mut saw_permission_cleared = false;
    for _ in 0..40 {
        let line = client.next_line();
        let deltas = deltas_from_frame(&line);
        // permission 交互：按场景应答（deny / allowOnce），否则 turn 会一直等。
        if !resolved_permission {
            let pending = deltas.iter().find_map(|delta| {
                delta["patch"]["pendingInteractions"]
                    .as_array()
                    .filter(|items| !items.is_empty())
            });
            if let (Some(pending), Some(option)) = (pending, scenario.resolve_permission) {
                let interaction_id = pending[0]["interactionId"]
                    .as_str()
                    .expect("interactionId")
                    .to_string();
                let id = client.send(
                    "v4/command",
                    command_envelope(
                        "resolveInteraction",
                        "cmd-resolve-1",
                        Some(&session_id),
                        json!({
                            "interactionId": interaction_id,
                            "answer": {"optionId": option},
                        }),
                    ),
                );
                let ack = client.response(id);
                assert_eq!(ack["result"]["status"], json!("accepted"));
                resolved_permission = true;
            }
        }
        // pendingInteractions 生命周期：先出现 permission（带 anchorRowId/options），
        // 应答后清零。
        if let Some(items) = deltas.iter().find_map(|delta| {
            delta["patch"]
                .get("pendingInteractions")
                .and_then(Value::as_array)
        }) {
            if items.iter().any(|item| {
                item["kind"] == json!("permission")
                    && item["anchorRowId"].is_number()
                    && item["payload"]["options"]
                        .as_array()
                        .is_some_and(|options| options.len() >= 2)
            }) {
                saw_permission_interaction = true;
            }
            if saw_permission_interaction && items.is_empty() {
                saw_permission_cleared = true;
            }
        }
        apply_to_rows(&mut rows, &deltas);
        let header_done = rows
            .iter()
            .any(|row| row["kind"] == json!("turnHeader") && row["state"] != json!("running"));
        let tool_terminal = rows
            .iter()
            .any(|row| row["kind"] == json!("toolCall") && row["status"] != json!("running"));
        // 等待 turnHeader 收口**且** toolCall 行进入终态，避免帧顺序导致的
        // 误判（tool success upsert 晚于 header upsert 的边界）。
        if header_done && (tool_terminal || !rows.iter().any(|r| r["kind"] == json!("toolCall"))) {
            converged = true;
            break;
        }
    }
    if scenario.resolve_permission.is_some() {
        assert!(
            saw_permission_interaction,
            "permission interaction must be projected with anchor + options"
        );
        assert!(
            saw_permission_cleared,
            "permission interaction must clear after resolve"
        );
    }
    assert!(
        converged,
        "turn did not converge: {}",
        serde_json::to_string_pretty(&rows).unwrap()
    );

    // 共同断言：toolCall 行投影存在。
    let tool_row = rows
        .iter()
        .find(|row| row["kind"] == json!("toolCall"))
        .expect("toolCall row must be projected");
    assert_eq!(tool_row["toolName"], json!("Write"));

    // thinking block 必须投影为 reasoning 行（open → delta → complete），
    // 不能像旧实现那样被 `_ => {}` 静默丢弃。
    let reasoning = rows
        .iter()
        .find(|row| row["kind"] == json!("reasoning"))
        .expect("reasoning row must be projected");
    assert_eq!(reasoning["state"], json!("complete"));
    assert_eq!(reasoning["text"], json!("planning the write"));
    assert!(
        reasoning["durationMs"].as_i64().is_some(),
        "terminal reasoning row must carry durationMs"
    );

    let expect_success = scenario.full_access || scenario.resolve_permission == Some("allowOnce");
    if expect_success {
        // 场景 A：真实执行 + 回灌 + 终态文本。
        assert_eq!(
            tool_row["status"],
            json!("success"),
            "tool must succeed: {tool_row}"
        );
        let marker = workspace.join("native-e2e.txt");
        assert!(
            marker.exists(),
            "the tool must have really written the file"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read marker"),
            "written by the native agent"
        );
        let assistant = rows
            .iter()
            .find(|row| row["kind"] == json!("assistantText"))
            .expect("assistant row");
        assert_eq!(assistant["state"], json!("complete"));
        assert_eq!(assistant["text"], json!("File written."));
        assert_eq!(
            rows.iter()
                .filter(|row| row["kind"] == json!("turnHeader"))
                .count(),
            1
        );
        let header = rows
            .iter()
            .find(|row| row["kind"] == json!("turnHeader"))
            .expect("header");
        assert_eq!(header["state"], json!("completedSuccess"));

        // 回灌证据：第二个请求体必须包含 tool_result 与工具输出。
        let bodies = provider.request_bodies();
        assert_eq!(bodies.len(), 2, "exactly two model roundtrips expected");
        assert!(
            bodies[1].contains("tool_result"),
            "second request must carry the tool result"
        );
        assert!(
            bodies[1].contains("toolu_native_1"),
            "tool_call_id must correlate"
        );
        assert!(
            bodies[1].contains("wrote 27 bytes to native-e2e.txt"),
            "tool output must feed back; body was: {}",
            bodies[1]
        );
    } else {
        // 场景 B：fail-closed 拒绝，文件不存在，拒绝回灌给模型。
        assert_eq!(
            tool_row["status"],
            json!("error"),
            "permission denial must project as error"
        );
        let marker = workspace.join("native-e2e.txt");
        assert!(
            !marker.exists(),
            "no file may be written without full access"
        );
        let error = &tool_row["error"];
        assert_eq!(error["code"], json!("tool.permissionDenied"));
        let bodies = provider.request_bodies();
        assert!(
            bodies[1].contains("tool.permissionDenied"),
            "denial must reach the model"
        );
    }

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn tool_loop_executes_real_tools_and_feeds_results_back() {
    run_scenario(&Scenario {
        name: "full-access",
        full_access: true,
        resolve_permission: None,
    });
}

#[test]
fn destructive_tools_fail_closed_without_full_access() {
    run_scenario(&Scenario {
        name: "denied",
        full_access: false,
        resolve_permission: Some("deny"),
    });
}

/// 队列 e2e：turn 在跑时第二条 sendText 必须入队（delivery=queue + queue 投影），
/// 当前 turn 收口后按 autoDrain 自动提升，第二条以**正常 turn 路径**执行
/// （第二个模型请求体携带第一轮历史）。
#[test]
fn queued_input_drains_after_active_turn() {
    let (provider, port) = ScriptedProvider::spawn_with_delay(Duration::from_millis(400));
    let base = std::env::temp_dir().join(format!("zcode-host-queue-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
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

    let mut process = HostProcess::spawn(&workspace, &data_dir, &provider_config, false);
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
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot

    // 第一条：立即 startNow（turn 开始，provider 会延迟 400ms 才回）
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-first",
            Some(&session_id),
            json!({"text": "first message"}),
        ),
    );
    let ack = client.response(id);
    assert_eq!(ack["result"]["status"], json!("accepted"));

    // 第二条：turn 在跑 → 入队
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-second",
            Some(&session_id),
            json!({"text": "second message"}),
        ),
    );
    let ack = client.response(id);
    assert_eq!(ack["result"]["status"], json!("accepted"));
    assert_eq!(
        ack["result"]["result"]["delivery"],
        json!("queue"),
        "busy sendText must be queued: {ack}"
    );
    assert_eq!(ack["result"]["result"]["inputId"], json!("cmd-second"));

    let mut rows: Vec<Value> = Vec::new();
    let mut saw_queued = false;
    let mut saw_drained = false;
    let mut completed_turns = 0usize;
    for _ in 0..200 {
        let line = client.next_line();
        let deltas = deltas_from_frame(&line);
        for delta in &deltas {
            if delta["op"] == json!("state.updated") {
                if let Some(items) = delta["patch"]["queue"]["items"].as_array() {
                    if items
                        .iter()
                        .any(|item| item["sourceCommandId"] == json!("cmd-second"))
                    {
                        saw_queued = true;
                    }
                    if saw_queued && items.is_empty() {
                        saw_drained = true;
                    }
                }
            }
        }
        apply_to_rows(&mut rows, &deltas);
        completed_turns = rows
            .iter()
            .filter(|row| {
                row["kind"] == json!("turnHeader") && row["state"] == json!("completedSuccess")
            })
            .count();
        if saw_queued && saw_drained && completed_turns >= 2 {
            break;
        }
    }
    assert!(saw_queued, "queue projection must show the queued input");
    assert!(
        saw_drained,
        "autoDrain must clear the queue after promotion"
    );
    assert_eq!(
        completed_turns,
        2,
        "both turns must complete: {}",
        serde_json::to_string_pretty(&rows).unwrap()
    );
    let user_texts: Vec<&str> = rows
        .iter()
        .filter(|row| row["kind"] == json!("userInput"))
        .filter_map(|row| row["text"].as_str())
        .collect();
    assert!(user_texts.contains(&"first message"), "{user_texts:?}");
    assert!(user_texts.contains(&"second message"), "{user_texts:?}");

    // drain 走真实 turn 路径：两个模型请求，第二个携带第一轮历史。
    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 2, "expected two model roundtrips");
    assert!(
        bodies[1].contains("first message"),
        "drained turn must reuse session history: {}",
        bodies[1]
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// 无 --full-access 时，用户 allowOnce 必须真正放行工具（permission broker 生效）。
#[test]
fn permission_allow_once_executes_tool_without_full_access() {
    run_scenario(&Scenario {
        name: "allow-once",
        full_access: false,
        resolve_permission: Some("allowOnce"),
    });
}
