//! 分支重放 e2e：`retryTurn` / `editUserQuery` 真的截断最后一轮并重跑。
//!
//! 覆盖的不是"命令被接受了"（那只证明 admission 通了），而是四条产品事实：
//! 1. 行集收到 `row.removed{fromRowId}`，客户端侧的行集回到该轮之前；
//! 2. 重跑轮的 `turnHeader.origin == "editRerun"`，输入行带正确文本
//!    （retry = 原 prompt，edit = newText）；
//! 3. **模型历史真的被切回轮起点**——重跑请求体里只有那一条 user 消息，
//!    没有重复堆叠（rewind 最容易做错的地方：只删行不删历史）；
//! 4. revision 与客户端收到的权威投影同步前移，CAS 命令不会 stale-by-one
//!    （终态帧发出之后才 +1 的话，所有行定位命令都会失败）。
//!
//! 外加三个守卫：未知 rowId → `fault.command.executionFailed`；空 newText →
//! `proto.invalidPayload` 且不动分支；历史轮 retry → `guard.latestAssistantRetryOnly`。

mod common;

use common::{
    authoritative_snapshot, cas_command_envelope, command_envelope, pump_until, Client,
    HostProcess, ScriptedProvider,
};
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

fn rows_of_kind(rows: &[Value], kind: &str) -> Vec<Value> {
    rows.iter()
        .filter(|row| row["kind"] == json!(kind))
        .cloned()
        .collect()
}

fn user_texts(rows: &[Value]) -> Vec<String> {
    rows_of_kind(rows, "userInput")
        .iter()
        .filter_map(|row| row["text"].as_str().map(str::to_string))
        .collect()
}

/// provider 收到的请求体里的 messages（历史是否被截断的直接证据）。
fn request_messages(body: &str) -> Vec<Value> {
    let parsed: Value = serde_json::from_str(body).expect("provider request body is json");
    parsed["messages"].as_array().cloned().unwrap_or_default()
}

/// 轮询命令账本直到该 commandId 落定终态 ack（admitted 命令的终态由执行体写入）。
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
    panic!("command {command_id} never reached a settled ack");
}

/// 等到某一轮以 completedSuccess 收口（turnHeader 终态 upsert）。
/// `sink` 只是帧的落地容器：本文件断言的是"服务端事实"，所以权威数据一律取快照。
fn pump_turn_completed(client: &mut Client, sink: &mut Vec<Value>, revision: &mut i64) {
    assert!(
        pump_until(client, sink, revision, |deltas| {
            deltas.iter().any(|delta| {
                delta["op"] == json!("row.upserted")
                    && delta["row"]["kind"] == json!("turnHeader")
                    && delta["row"]["state"] == json!("completedSuccess")
            })
        }),
        "a turn must complete with a terminal turnHeader upsert"
    );
}

#[test]
fn branch_rerun_truncates_the_latest_turn_and_replays_it() {
    // 1ms 延迟：脚本恒回终态文本，turn 不需要 permission 交互即可收口。
    let (provider, port) = ScriptedProvider::spawn_with_delay(Duration::from_millis(1));
    let base = std::env::temp_dir().join(format!("zcode-host-branch-{}", std::process::id()));
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
    let mut revision = 0i64;
    let mut sink: Vec<Value> = Vec::new();
    // 先建立订阅：turn 的帧只有在有订阅者时才按投影顺序流出。
    let id = client.send(
        "v4/conversation/subscribe",
        json!({"sessionId": session_id, "connectionId": "conn-1", "clientMode": "desktop-continuous"}),
    );
    let _ = client.response(id);
    let _ = client.next_line(); // initial snapshot

    // ── 第一轮：普通输入 ──
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-t1",
            Some(&session_id),
            json!({"text": "first message"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
    pump_turn_completed(&mut client, &mut sink, &mut revision);

    // 权威快照：CAS 参数与行定位都以服务端事实为准（真实客户端重连就是这么取）。
    let snapshot = authoritative_snapshot(&mut client, &session_id);
    let log_epoch = snapshot.log_epoch.clone();
    let assistant = rows_of_kind(&snapshot.rows, "assistantText")
        .last()
        .cloned()
        .expect("turn 1 has an assistant row");
    let assistant_row_id = assistant["rowId"].as_u64().expect("rowId");
    let assistant_entity = assistant["assistantResponseId"]
        .as_str()
        .expect("assistantResponseId")
        .to_string();
    let header_row_id = rows_of_kind(&snapshot.rows, "turnHeader")[0]["rowId"]
        .as_u64()
        .expect("header rowId");

    // ── retryTurn：重跑最后一轮，用原 prompt ──
    let id = client.send(
        "v4/command",
        cas_command_envelope(
            "retryTurn",
            "cmd-retry",
            &session_id,
            snapshot.revision,
            &log_epoch,
            json!({"target": {"rowId": assistant_row_id, "entityId": assistant_entity}}),
        ),
    );
    let admission = client.response(id);
    assert_eq!(
        admission["result"]["status"].as_str(),
        Some("accepted"),
        "retryTurn must be admitted at the revision the client last saw; got {admission}"
    );
    assert!(
        pump_until(&mut client, &mut sink, &mut revision, |deltas| {
            deltas.iter().any(|delta| {
                delta["op"] == json!("row.removed") && delta["fromRowId"] == json!(header_row_id)
            })
        }),
        "retry must project row.removed for the truncated turn"
    );
    pump_turn_completed(&mut client, &mut sink, &mut revision);
    let snapshot = authoritative_snapshot(&mut client, &session_id);
    let rerun_headers = rows_of_kind(&snapshot.rows, "turnHeader");
    assert_eq!(
        rerun_headers.last().expect("header")["origin"],
        json!("editRerun"),
        "editRerun origin tells the UI this is a replay, not a new input"
    );
    assert_eq!(
        user_texts(&snapshot.rows),
        vec!["first message".to_string()],
        "retry replays the ORIGINAL prompt, and the truncated turn's rows are gone"
    );
    assert_eq!(
        ledger_ack(&mut client, &session_id, "cmd-retry")["status"],
        json!("accepted"),
        "a successful rerun settles its own ack"
    );
    // 模型历史真的切回轮起点：第二次请求只有那一条 user 消息。
    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 2, "turn 1 + retry each hit the model once");
    assert_eq!(
        request_messages(&bodies[1]).len(),
        1,
        "history must be cut back to the turn start, not appended to"
    );
    assert!(bodies[1].contains("first message"));

    // ── editUserQuery：换成新文本重跑 ──
    let user_row = rows_of_kind(&snapshot.rows, "userInput")
        .last()
        .cloned()
        .expect("userInput row");
    let user_row_id = user_row["rowId"].as_u64().expect("rowId");
    // target.entityId 必须是该行**当前**的实体身份。编一个值过去会被 host 的实体校验
    // 判成过期快照（`fault.command.executionFailed`）——那是被测行为，不是被测输入。
    let user_entity = user_row["entityId"].as_str().expect("entityId").to_string();
    let id = client.send(
        "v4/command",
        cas_command_envelope(
            "editUserQuery",
            "cmd-edit",
            &session_id,
            snapshot.revision,
            &log_epoch,
            json!({"target": {"rowId": user_row_id, "entityId": user_entity}, "newText": "edited question"}),
        ),
    );
    let admission = client.response(id);
    assert_eq!(
        admission["result"]["status"].as_str(),
        Some("accepted"),
        "editUserQuery must be admitted at the current revision; got {admission}"
    );
    pump_turn_completed(&mut client, &mut sink, &mut revision);
    let snapshot = authoritative_snapshot(&mut client, &session_id);
    assert_eq!(
        user_texts(&snapshot.rows),
        vec!["edited question".to_string()],
        "edit replaces the branch: only the edited query survives"
    );
    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 3, "turn 1 + retry + edit");
    assert!(
        !bodies[2].contains("first message"),
        "the edited branch must not carry the superseded prompt"
    );
    assert!(bodies[2].contains("edited question"));

    // ── 守卫 1：未知 rowId 解析不到实体（TS V4RowTranslationError）──
    let id = client.send(
        "v4/command",
        cas_command_envelope(
            "retryTurn",
            "cmd-ghost",
            &session_id,
            snapshot.revision,
            &log_epoch,
            json!({"target": {"rowId": 999_999, "entityId": "resp-ghost"}}),
        ),
    );
    let _ = client.response(id);
    let ghost = ledger_ack(&mut client, &session_id, "cmd-ghost");
    assert_eq!(ghost["status"], json!("failed"));
    assert_eq!(
        ghost["reasonCode"],
        json!("fault.command.executionFailed"),
        "an unresolvable row target fails closed with a typed fault"
    );

    // ── 守卫 2：空输入在截断之前就被拒（TS 的 hasPromptInput 校验）──
    let empty_target = rows_of_kind(&snapshot.rows, "userInput")
        .last()
        .cloned()
        .expect("userInput row");
    // 同样必须用该行当前的实体身份，否则 host 会先按"过期快照"拒绝，
    // 被测的空输入门禁就没有机会执行。
    let id = client.send(
        "v4/command",
        cas_command_envelope(
            "editUserQuery",
            "cmd-empty",
            &session_id,
            snapshot.revision,
            &log_epoch,
            json!({"target": {"rowId": empty_target["rowId"], "entityId": empty_target["entityId"]}, "newText": "   "}),
        ),
    );
    let _ = client.response(id);
    let empty = ledger_ack(&mut client, &session_id, "cmd-empty");
    assert_eq!(empty["status"], json!("failed"));
    assert_eq!(
        empty["reasonCode"],
        json!("proto.invalidPayload"),
        "an empty edit must be refused, and it must not truncate anything"
    );
    assert_eq!(
        user_texts(&authoritative_snapshot(&mut client, &session_id).rows),
        vec!["edited question".to_string()],
        "a refused edit leaves the branch untouched"
    );

    // ── 守卫 3：历史轮不可 retry（会静默回退 active branch）──
    let prior_assistant = rows_of_kind(&snapshot.rows, "assistantText")
        .last()
        .cloned()
        .expect("assistant row of the edited turn");
    let prior_row_id = prior_assistant["rowId"].as_u64().expect("rowId");
    let prior_entity = prior_assistant["assistantResponseId"]
        .as_str()
        .expect("assistantResponseId")
        .to_string();
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-t4",
            Some(&session_id),
            json!({"text": "a newer turn"}),
        ),
    );
    let _ = client.response(id);
    pump_turn_completed(&mut client, &mut sink, &mut revision);
    let snapshot = authoritative_snapshot(&mut client, &session_id);
    let id = client.send(
        "v4/command",
        cas_command_envelope(
            "retryTurn",
            "cmd-history",
            &session_id,
            snapshot.revision,
            &log_epoch,
            json!({"target": {"rowId": prior_row_id, "entityId": prior_entity}}),
        ),
    );
    let _ = client.response(id);
    let history = ledger_ack(&mut client, &session_id, "cmd-history");
    assert_eq!(history["status"], json!("failed"));
    assert_eq!(
        history["reasonCode"],
        json!("guard.latestAssistantRetryOnly"),
        "retrying a historical turn would rewind the active branch — it must be refused"
    );
    assert_eq!(
        user_texts(&authoritative_snapshot(&mut client, &session_id).rows),
        vec!["edited question".to_string(), "a newer turn".to_string()],
        "a refused rerun must not truncate or add rows"
    );

    let _ = std::fs::remove_dir_all(&base);
}
