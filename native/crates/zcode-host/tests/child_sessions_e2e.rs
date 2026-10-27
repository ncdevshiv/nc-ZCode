//! 子会话命令 e2e：`forkAssistant` 带内容分叉、`createSelectionSideSession` 派生副屏。
//!
//! 被测的产品事实：
//! 1. fork 出来的 child 只看到父会话**到目标行为止**的行与模型历史（不是全量复制，
//!    也不是空会话），并且复制来的行**不带** `canRewindFiles`——fork 只接管对话，
//!    父会话在共享 workspace 里写的文件仍归父会话回滚；
//! 2. fork 之后父会话的行集、队列与 revision 一字不变（TS 的"不 stop parent"）；
//! 3. 目标不是轮尾 completed assistant 的行被 `guard.forkTargetNotStable` 拒绝，
//!    且拒绝不留下一个半成品 child；
//! 4. side session 的 `firstInput` 落到 **child 自己的命令账本**，父队列不被污染。

mod common;

use common::{
    authoritative_snapshot, cas_command_envelope, command_envelope, pump_until, Client,
    HostProcess, ScriptedProvider,
};
use serde_json::{json, Value};
use std::time::Duration;

const COPIED_ROWS: usize = 3;

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

fn subscribe(client: &mut Client, session_id: &str, connection_id: &str) {
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": connection_id, "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
}

fn ledger_ack(client: &mut Client, session_id: &str, command_id: &str) -> Value {
    for _ in 0..100 {
        let id = client.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": session_id, "commandId": command_id}]}),
        );
        let response = client.response(id);
        let item = response["result"]["results"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        if item["result"] != json!("unknown") && item["result"] != Value::Null {
            return item["result"].clone();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("command {command_id} never settled in session {session_id}");
}

fn turn_completed(client: &mut Client, sink: &mut Vec<Value>, revision: &mut i64) {
    assert!(
        pump_until(client, sink, revision, |deltas| {
            deltas.iter().any(|delta| {
                delta["op"] == json!("row.upserted")
                    && delta["row"]["kind"] == json!("turnHeader")
                    && delta["row"]["state"] == json!("completedSuccess")
            })
        }),
        "a turn must complete"
    );
}

#[test]
fn fork_and_side_session_create_independent_children() {
    let (provider, port) = ScriptedProvider::spawn_with_delay(Duration::from_millis(1));
    let base = std::env::temp_dir().join(format!("zcode-host-child-{}", std::process::id()));
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
    let parent = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    subscribe(&mut client, &parent, "conn-parent");
    let mut sink: Vec<Value> = Vec::new();
    let mut revision = 0i64;

    for (command_id, text) in [("cmd-t1", "first question"), ("cmd-t2", "second question")] {
        let id = client.send(
            "v4/command",
            command_envelope("sendText", command_id, Some(&parent), json!({"text": text})),
        );
        assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
        turn_completed(&mut client, &mut sink, &mut revision);
    }
    let parent_snapshot = authoritative_snapshot(&mut client, &parent);
    assert_eq!(parent_snapshot.rows.len(), 6, "two turns × three rows");
    // turnId 就是命令 id（本 host 用 sourceCommandId 作 turn 身份）。
    let first_turn_assistant = parent_snapshot
        .rows
        .iter()
        .find(|row| {
            row["kind"] == json!("assistantText")
                && row["turnId"]
                    .as_str()
                    .is_some_and(|id| id.contains("cmd-t1"))
        })
        .cloned()
        .expect("turn 1 has an assistant row");
    let target_row = first_turn_assistant["rowId"].as_u64().expect("rowId");
    let target_entity = first_turn_assistant["entityId"]
        .as_str()
        .expect("entityId")
        .to_string();

    // ── 3) 先验拒绝：指到 userInput 行不是可分叉的 assistant 段 ──
    let user_row = parent_snapshot
        .rows
        .iter()
        .find(|row| row["kind"] == json!("userInput"))
        .and_then(|row| row["rowId"].as_u64())
        .expect("userInput row");
    let id = client.send(
        "v4/command",
        cas_command_envelope(
            "forkAssistant",
            "cmd-fork-bad",
            &parent,
            parent_snapshot.revision,
            &parent_snapshot.log_epoch,
            json!({"target": {"rowId": user_row, "entityId": "resp-cmd-t1"}}),
        ),
    );
    let _ = client.response(id);
    let bad = ledger_ack(&mut client, &parent, "cmd-fork-bad");
    assert_eq!(bad["status"], json!("rejected"), "{bad}");
    assert_eq!(bad["reasonCode"], json!("guard.forkTargetNotStable"));

    // ── 1)+2) 真正的 fork ──
    let id = client.send(
        "v4/command",
        cas_command_envelope(
            "forkAssistant",
            "cmd-fork",
            &parent,
            parent_snapshot.revision,
            &parent_snapshot.log_epoch,
            json!({"target": {"rowId": target_row, "entityId": target_entity}}),
        ),
    );
    let admission = client.response(id);
    assert_eq!(
        admission["result"]["status"].as_str(),
        Some("accepted"),
        "fork must be admitted; got {admission}"
    );
    let fork_ack = ledger_ack(&mut client, &parent, "cmd-fork");
    assert_eq!(fork_ack["status"], json!("accepted"), "{fork_ack}");
    let child = fork_ack["result"]["sessionId"]
        .as_str()
        .expect("child session id")
        .to_string();
    assert_ne!(child, parent, "fork creates a separate session");
    assert_eq!(fork_ack["result"]["copiedRows"], json!(COPIED_ROWS));
    assert_eq!(fork_ack["result"]["copiedHistory"], json!(2));

    subscribe(&mut client, &child, "conn-child");
    let child_snapshot = authoritative_snapshot(&mut client, &child);
    assert_eq!(
        child_snapshot.rows.len(),
        COPIED_ROWS,
        "child 只到父会话的分叉边界为止"
    );
    assert_ne!(
        child_snapshot.log_epoch, parent_snapshot.log_epoch,
        "child 有自己的 logEpoch：父会话的行定位凭证不能拿来指认子会话的行"
    );
    assert!(
        child_snapshot
            .rows
            .iter()
            .all(|row| row.get("actions").is_none()),
        "复制来的行不得带上 canRewindFiles：child 没接管工作区"
    );
    assert!(
        child_snapshot.rows.iter().any(|row| {
            row["kind"] == json!("userInput") && row["text"] == json!("first question")
        }),
        "child 带着父会话那一轮的输入"
    );
    let parent_after = authoritative_snapshot(&mut client, &parent);
    assert_eq!(
        parent_after.rows.len(),
        parent_snapshot.rows.len(),
        "fork 不能改动父会话行集: parent_after={:?} child={child} parent={parent}",
        parent_after
            .rows
            .iter()
            .map(|r| (&r["kind"], &r["turnId"]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        parent_after.revision, parent_snapshot.revision,
        "fork 不改父投影，因此不推进父 revision"
    );

    // ── 4) side session + firstInput 落在 child 账本 ──
    let id = client.send(
        "v4/command",
        command_envelope(
            "createSelectionSideSession",
            "cmd-side",
            Some(&parent),
            json!({"firstInput": {"text": "asked only in the side session"}}),
        ),
    );
    let admission = client.response(id);
    assert_eq!(
        admission["result"]["status"].as_str(),
        Some("accepted"),
        "side session must be admitted; got {admission}"
    );
    // 走 inbox 的命令，响应只有 admission ack；子会话 id 在**终态** ack 里（账本回源）。
    let admission_status = admission["result"]["status"].clone();
    assert_eq!(
        admission_status,
        json!("accepted"),
        "side session must be admitted; got {admission}"
    );
    let parent_side_ack = ledger_ack(&mut client, &parent, "cmd-side");
    let side = parent_side_ack["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("side session id missing in {parent_side_ack}"))
        .to_string();
    assert_eq!(
        ledger_ack(&mut client, &side, "cmd-side")["status"],
        json!("accepted"),
        "首条输入的终态必须进 child 自己的命令账本"
    );
    subscribe(&mut client, &side, "conn-side");
    let side_snapshot = authoritative_snapshot(&mut client, &side);
    assert!(
        side_snapshot.rows.iter().any(|row| {
            row["kind"] == json!("userInput")
                && row["text"] == json!("asked only in the side session")
        }),
        "side session 真的跑了那条输入: {:?}",
        side_snapshot.rows
    );
    let parent_after_side = authoritative_snapshot(&mut client, &parent);
    assert_eq!(
        parent_after_side.rows.len(),
        parent_snapshot.rows.len(),
        "子会话的输入不得进父行集或父队列"
    );
    assert_eq!(
        parent_after_side.queue["items"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        0,
        "父队列必须保持为空"
    );
    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 3, "两轮父会话 + 一次 side 首条输入");
    assert!(
        bodies[2].contains("asked only in the side session"),
        "child 的请求体带着自己的问题"
    );
    assert!(
        !bodies[2].contains("second question"),
        "child 的历史不继承兄弟 fork 的内容（side session 从空开始）"
    );

    let _ = std::fs::remove_dir_all(&base);
}
