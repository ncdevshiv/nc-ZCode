//! 队列变更命令 e2e：edit / reorder / delete / sendQueuedNow（CAS 门禁 + 真实生效）。
//!
//! 场景：turn 在跑时排队 A、B → 旧 revision 的 edit 被拒 → edit A → reorder B 到队首
//! → sendQueuedNow 缺失项被拒 → delete B → 仅 A（编辑后的文本）被 autoDrain 执行。
//!
//! 使用共享夹具 `common`（真实二进制 + stdio；脚本化 SSE 只在测试进程内）。

mod common;

use common::{command_envelope, pump_until, Client, HostProcess, ScriptedProvider};
use serde_json::{json, Value};
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

#[test]
fn queue_mutations_are_cas_gated_and_take_effect() {
    let (provider, port) = ScriptedProvider::spawn_with_delay(Duration::from_millis(600));
    let base = std::env::temp_dir().join(format!("zcode-host-queue-mut-{}", std::process::id()));
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
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot
                                // 起一轮（600ms），随后排队 A、B
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-first",
            Some(&session_id),
            json!({"text": "first message"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
    // 等待 turn 真正进入 running（turnHeader running 行）后再排队，避免与启动竞态。
    let mut rows: Vec<Value> = Vec::new();
    let mut revision = 0i64;
    assert!(
        pump_until(&mut client, &mut rows, &mut revision, |deltas| {
            deltas.iter().any(|delta| {
                delta["row"]["kind"] == json!("turnHeader")
                    && delta["row"]["state"] == json!("running")
            })
        }),
        "turn must be running before queueing"
    );
    for (command_id, text) in [("cmd-a", "alpha"), ("cmd-b", "beta")] {
        let id = client.send(
            "v4/command",
            command_envelope(
                "sendText",
                command_id,
                Some(&session_id),
                json!({"text": text}),
            ),
        );
        let ack = client.response(id);
        assert_eq!(ack["result"]["result"]["delivery"], json!("queue"));
    }
    assert!(
        pump_until(&mut client, &mut rows, &mut revision, |deltas| {
            deltas.iter().any(|delta| {
                delta["op"] == json!("state.updated")
                    && delta["patch"]["queue"]["items"]
                        .as_array()
                        .is_some_and(|items| items.len() == 2)
            })
        }),
        "both queued items must be projected"
    );
    assert!(revision >= 2, "queue changes are revision-bearing");

    let queue_envelope =
        |command_type: &str, command_id: &str, base_revision: i64, payload: Value| {
            let mut envelope =
                command_envelope(command_type, command_id, Some(&session_id), payload);
            envelope["baseRevision"] = json!(base_revision);
            envelope
        };
    // 旧 revision 必须被拒（CAS）
    let id = client.send(
        "v4/command",
        queue_envelope(
            "editQueueItem",
            "cmd-stale",
            revision - 1,
            json!({"queueItemId": "queue_cmd-a", "newText": "stale edit"}),
        ),
    );
    let stale = client.response(id);
    assert_eq!(stale["result"]["status"], json!("stale"));
    assert_eq!(stale["result"]["reasonCode"], json!("proto.staleRevision"));
    // edit A
    let id = client.send(
        "v4/command",
        queue_envelope(
            "editQueueItem",
            "cmd-edit",
            revision,
            json!({"queueItemId": "queue_cmd-a", "newText": "alpha-edited"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
    assert!(
        pump_until(&mut client, &mut rows, &mut revision, |deltas| {
            deltas.iter().any(|delta| {
                delta["op"] == json!("state.updated")
                    && delta["patch"]["queue"]["items"]
                        .as_array()
                        .is_some_and(|items| {
                            items
                                .iter()
                                .any(|item| item["text"] == json!("alpha-edited"))
                        })
            })
        }),
        "edit must project the new text"
    );
    // reorder B before A
    let id = client.send(
        "v4/command",
        queue_envelope(
            "reorderQueueItem",
            "cmd-reorder",
            revision,
            json!({"queueItemId": "queue_cmd-b", "beforeQueueItemId": "queue_cmd-a"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
    assert!(
        pump_until(&mut client, &mut rows, &mut revision, |deltas| {
            deltas.iter().any(|delta| {
                delta["op"] == json!("state.updated")
                    && delta["patch"]["queue"]["items"]
                        .as_array()
                        .and_then(|items| items.first())
                        .is_some_and(|item| item["sourceCommandId"] == json!("cmd-b"))
            })
        }),
        "reorder must move B to the front"
    );
    // sendQueuedNow 缺失项 → rejected
    let id = client.send(
        "v4/command",
        queue_envelope(
            "sendQueuedNow",
            "cmd-missing",
            revision,
            json!({"queueItemId": "queue_missing"}),
        ),
    );
    let missing = client.response(id);
    assert_eq!(missing["result"]["status"], json!("rejected"));
    assert_eq!(
        missing["result"]["reasonCode"],
        json!("fault.command.executionFailed")
    );
    // delete B
    let id = client.send(
        "v4/command",
        queue_envelope(
            "deleteQueueItem",
            "cmd-delete",
            revision,
            json!({"queueItemId": "queue_cmd-b"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
    assert!(
        pump_until(&mut client, &mut rows, &mut revision, |deltas| {
            deltas.iter().any(|delta| {
                delta["op"] == json!("state.updated")
                    && delta["patch"]["queue"]["items"]
                        .as_array()
                        .is_some_and(|items| items.len() == 1)
            })
        }),
        "delete must remove B from the queue"
    );
    // turn1 + drain(A) 完成；B 不应执行。
    let mut completed_turns = 0usize;
    assert!(
        pump_until(&mut client, &mut rows, &mut revision, |deltas| {
            for delta in deltas {
                if delta["op"] == json!("row.upserted")
                    && delta["row"]["kind"] == json!("turnHeader")
                    && delta["row"]["state"] == json!("completedSuccess")
                {
                    completed_turns += 1;
                }
            }
            completed_turns >= 2
        }),
        "turn 1 + drained A must complete"
    );
    let user_texts: Vec<&str> = rows
        .iter()
        .filter(|row| row["kind"] == json!("userInput"))
        .filter_map(|row| row["text"].as_str())
        .collect();
    assert!(user_texts.contains(&"first message"), "{user_texts:?}");
    assert!(
        user_texts.contains(&"alpha-edited"),
        "edited text must be executed: {user_texts:?}"
    );
    assert!(
        !user_texts.contains(&"beta"),
        "deleted item must not run: {user_texts:?}"
    );
    assert_eq!(
        provider.request_bodies().len(),
        2,
        "only turn 1 and drained A may hit the model"
    );

    let _ = std::fs::remove_dir_all(&base);
}
