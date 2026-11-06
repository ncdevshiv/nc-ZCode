//! 工作区文件回滚 e2e：`applyFileRewind` 真的把本轮写出去的文件收回去，
//! 并且外部改动 / 重复回滚都被门禁挡住。
//!
//! 这里刻意用**真实写工具**（provider 脚本回 tool_use(Write)，host 真的往 workspace
//! 落盘），因为被测的是"台账记的是不是磁盘上真实发生过的事"——用假工具就什么都证明不了。
//! 断言覆盖四件事：
//! 1. 轮收口后 turnHeader 带 `fileChanges{additions,deletions,files,state:"active"}`
//!    与 `actions.canRewindFiles`；
//! 2. 文件被用户手改/手删 → preview 判 `external_modified`，整笔拒绝且**零副作用**；
//! 3. 内容回到 host 写出的那一版 → applied：本轮新建的文件被删除，行标成 `reverted`；
//! 4. 第二次 rewind → `guard.actionUnavailable`（不能反复覆盖同一批文件）。

mod common;

use common::{
    authoritative_snapshot, cas_command_envelope, command_envelope, pump_until, Client,
    HostProcess, ScriptedProvider,
};
use serde_json::{json, Value};
use std::time::Duration;

const WRITTEN_CONTENT: &str = "written by the native agent";
const WRITTEN_FILE: &str = "native-e2e.txt";

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

fn turn_header(rows: &[Value]) -> Value {
    rows.iter()
        .find(|row| row["kind"] == json!("turnHeader"))
        .cloned()
        .expect("the session has a turnHeader row")
}

#[test]
fn file_rewind_uses_the_real_ledger_and_refuses_unsafe_cases() {
    // delay = 0 → 脚本第一个请求回 tool_use(Write)，第二个回终态文本。
    let (provider, port) = ScriptedProvider::spawn();
    let base = std::env::temp_dir().join(format!("zcode-host-rewind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base.join("provider.json");
    provider_config(port, &config_path);
    let written_path = workspace.join(WRITTEN_FILE);

    // full-access：写工具不需要 permission 应答，本轮能一路跑到收口。
    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, true);
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
    let _ = client.next_line();

    let mut sink: Vec<Value> = Vec::new();
    let mut revision = 0i64;
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-write",
            Some(&session_id),
            json!({"text": "write the file"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
    assert!(
        pump_until(&mut client, &mut sink, &mut revision, |deltas| {
            deltas.iter().any(|delta| {
                delta["op"] == json!("row.upserted")
                    && delta["row"]["kind"] == json!("turnHeader")
                    && delta["row"]["state"] == json!("completedSuccess")
            })
        }),
        "the tool turn must complete"
    );
    assert!(
        written_path.is_file(),
        "the Write tool must really have created the file"
    );
    assert_eq!(
        std::fs::read_to_string(&written_path).expect("read written file"),
        WRITTEN_CONTENT
    );

    let snapshot = authoritative_snapshot(&mut client, &session_id);
    let header = turn_header(&snapshot.rows);
    let row_id = header["rowId"].as_u64().expect("rowId");
    let turn_id = header["turnId"].as_str().expect("turnId").to_string();
    assert_eq!(
        header["fileChanges"]["state"].as_str(),
        Some("active"),
        "a turn that touched files advertises an active fileChanges summary: {header}"
    );
    assert_eq!(header["fileChanges"]["files"], json!(1));
    assert_eq!(header["actions"]["canRewindFiles"], json!(true));

    // ── 1b) 只读查询面：fileChanges（含 diff）与 fileRewindPreview ──
    let entity_id = header["entityId"].as_str().expect("entityId").to_string();
    let target = json!({"rowId": row_id, "entityId": entity_id});
    let changes = {
        let id = client.send(
            "v4/conversation/fileChanges",
            json!({
                "sessionId": session_id,
                "target": target,
                "baseRevision": snapshot.revision,
                "baseLogEpoch": snapshot.log_epoch,
            }),
        );
        client.response(id)["result"].clone()
    };
    assert_eq!(changes["files"], json!(1), "{changes}");
    assert_eq!(changes["state"], json!("active"), "{changes}");
    assert!(changes["additions"].as_u64().unwrap_or(0) > 0, "{changes}");
    assert_eq!(changes["items"][0]["path"], json!(WRITTEN_FILE));
    assert_eq!(changes["items"][0]["writeCount"], json!(1));
    let patch_lines = changes["items"][0]["patches"][0]["lines"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        patch_lines
            .iter()
            .any(|line| line.as_str().is_some_and(|text| text.starts_with('+'))),
        "新建文件的 patch 必须有 + 行: {changes}"
    );

    let usage = {
        let id = client.send("v4/conversation/usage", json!({"sessionId": session_id}));
        client.response(id)["result"].clone()
    };
    assert_eq!(usage["sessionId"], json!(session_id));
    assert!(
        usage["modelRequestCount"].as_u64().unwrap_or(0) >= 1,
        "本轮至少有一次模型请求: {usage}"
    );
    assert!(
        usage["totalTokens"].as_u64().unwrap_or(0) > 0,
        "脚本化 provider 报告了 token：{usage}"
    );

    let preview = {
        let id = client.send(
            "v4/conversation/fileRewindPreview",
            json!({
                "sessionId": session_id,
                "target": target,
                "baseRevision": snapshot.revision,
                "baseLogEpoch": snapshot.log_epoch,
            }),
        );
        client.response(id)["result"].clone()
    };
    assert_eq!(preview["canApply"], json!(true), "{preview}");
    assert_eq!(
        preview["safeFiles"][0]["action"],
        json!("delete"),
        "本轮新建的文件回滚动作是删除: {preview}"
    );
    assert_eq!(preview["safeFiles"][0]["path"], json!(WRITTEN_FILE));
    assert_eq!(preview["unsafeFiles"], json!([]));
    assert_eq!(preview["ignoredFiles"], json!([]));

    // 陈旧 epoch 必须整笔拒绝（rowId 只在 epoch 内有意义）。
    let id = client.send(
        "v4/conversation/fileChanges",
        json!({
            "sessionId": session_id,
            "target": target,
            "baseRevision": snapshot.revision,
            "baseLogEpoch": "epoch-that-does-not-exist",
        }),
    );
    let stale = client.response(id);
    assert_eq!(
        stale["error"]["data"]["reasonCode"],
        json!("proto.staleLogEpoch"),
        "{stale}"
    );

    let rewind = |client: &mut Client, command_id: &str, revision: i64| {
        let id = client.send(
            "v4/command",
            cas_command_envelope(
                "applyFileRewind",
                command_id,
                &session_id,
                revision,
                &snapshot.log_epoch,
                json!({"target": {"rowId": row_id, "entityId": turn_id}}),
            ),
        );
        let _ = client.response(id);
        ledger_ack(client, &session_id, command_id)
    };

    // ── 2) 用户手删文件 → external_modified，整笔拒绝且不留副作用 ──
    std::fs::remove_file(&written_path).expect("remove file by hand");
    let blocked = rewind(&mut client, "cmd-unsafe", snapshot.revision);
    assert_eq!(blocked["status"], json!("rejected"));
    assert_eq!(
        blocked["reasonCode"],
        json!("guard.workspaceRewindUnsafeFiles"),
        "{blocked}"
    );
    let unsafe_files = blocked["result"]["preview"]["unsafeFiles"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(unsafe_files.len(), 1, "{blocked}");
    assert_eq!(unsafe_files[0]["path"], json!(WRITTEN_FILE));
    assert_eq!(unsafe_files[0]["reason"], json!("external_modified"));
    assert_eq!(
        unsafe_files[0]["currentHash"].as_str(),
        Some("missing"),
        "缺失文件用协议哨兵值 missing 表达身份"
    );
    assert!(!written_path.exists(), "被拒绝的 rewind 不能把文件写回来");
    let after_reject = authoritative_snapshot(&mut client, &session_id);
    assert_eq!(
        turn_header(&after_reject.rows)["fileChanges"]["state"].as_str(),
        Some("active"),
        "拒绝不改变该轮的可回滚状态"
    );

    // ── 3) 内容回到 host 写出的那一版 → applied，本轮新建的文件被删除 ──
    std::fs::write(&written_path, WRITTEN_CONTENT).expect("restore tool content");
    let revision = after_reject.revision;
    let applied = rewind(&mut client, "cmd-applied", revision);
    assert_eq!(applied["status"], json!("accepted"), "{applied}");
    assert_eq!(applied["result"]["disposition"], json!("applied"));
    assert_eq!(applied["result"]["restoredFiles"], json!([WRITTEN_FILE]));
    assert!(
        !written_path.exists(),
        "本轮之前文件不存在 → 回滚就是删除它"
    );
    assert!(
        provider.request_bodies().len() >= 2,
        "the turn really ran a tool loop"
    );

    let after_apply = authoritative_snapshot(&mut client, &session_id);
    let header = turn_header(&after_apply.rows);
    assert_eq!(
        header["fileChanges"]["state"].as_str(),
        Some("reverted"),
        "回滚成功后必须标成 reverted，否则同一批文件会被反复覆盖"
    );
    assert!(
        header["actions"].get("canRewindFiles").is_none(),
        "TS 的 actions 只下发为 true 的键：已回滚的轮不再 advertise 回滚"
    );

    // ── 4) 第二次 rewind → guard.actionUnavailable ──
    let again = rewind(&mut client, "cmd-twice", after_apply.revision);
    assert_eq!(again["status"], json!("rejected"), "{again}");
    assert_eq!(again["reasonCode"], json!("guard.actionUnavailable"));
    assert!(!written_path.exists(), "重复回滚同样必须零副作用");

    let _ = std::fs::remove_dir_all(&base);
}
