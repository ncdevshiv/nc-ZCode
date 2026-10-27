//! workspace hook 评审 e2e：真实二进制 + 真实工作区配置 + 真实磁盘台账。
//!
//! 覆盖的产品事实：
//! 1. 会话启动时发现工作区 hook，`workspaceHookAdmission` 显示待审数，
//!    `pendingInteractions` 带一条 `workspaceHookReview`（interactionId 与 payload 一致）；
//! 2. `respondWorkspaceHookReview` 真的把授权写进 `<data-dir>/security/` 的台账，
//!    横幅与评审交互同时消失；
//! 3. `revokeWorkspaceHookTrust` 删除授权并重开评审（generation 前进）；
//! 4. `toggleWorkspaceHookReviewItem` 真的改 `.zcode/config.json` 的 `enabled`，
//!    且开关改变 bundle 摘要 → 评审重新打开；
//! 5. `requestWorkspaceHookReview` 对过期 bundle 拒绝；
//! 6. 重启后信任仍在（台账是 durable 的）。

mod common;

use common::{command_envelope, Client, HostProcess};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn provider_config(port: u16, path: &Path) {
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

const HOOKS_CONFIG: &str = r#"{
  "theme": "dark",
  "hooks": {
    "enabled": true,
    "events": {
      "PreToolUse": [
        {"matcher": "Bash", "hooks": [{"type": "command", "command": "node check.js"}]}
      ],
      "Stop": [
        {"hooks": [{"type": "process", "command": "git", "args": ["status"]}]}
      ]
    }
  }
}
"#;

fn make_workspace(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "zcode-host-hook-review-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(workspace.join(".zcode")).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    std::fs::write(workspace.join(".zcode").join("config.json"), HOOKS_CONFIG).expect("config");
    let config_path = base.join("provider.json");
    provider_config(9, &config_path);
    (base, workspace, data_dir)
}

fn start(workspace: &Path, data_dir: &Path, config_path: &Path) -> (HostProcess, Client) {
    let mut process = HostProcess::spawn(workspace, data_dir, config_path, false);
    let client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    (process, client)
}

fn create_session(client: &mut Client) -> String {
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string()
}

/// 重新订阅取权威快照（含 hook 准入/交互），丢弃其它帧。
fn snapshot_state(client: &mut Client, session_id: &str) -> Value {
    let id = client.send(
        "v4/conversation/subscribe",
        json!({
            "sessionId": session_id,
            "connectionId": "conn-hooks",
            "clientMode": "desktop-continuous",
        }),
    );
    let _ = client.response(id);
    let mut skipped = std::collections::VecDeque::new();
    for _ in 0..300 {
        let line = client.next_line();
        let payload = &line["params"]["frame"]["payload"];
        if payload["kind"] == json!("snapshot")
            && payload["snapshot"]["sessionId"] == json!(session_id)
        {
            for skipped_line in skipped.into_iter().rev() {
                client.pending.push_front(skipped_line);
            }
            return payload["snapshot"].clone();
        }
        skipped.push_back(line);
    }
    panic!("no snapshot frame for {session_id}");
}

/// 等命令真正落定（admission ack 先回，执行是异步的）；返回账本里的终态 ack。
fn settle(client: &mut Client, session_id: &str, command_id: &str) -> Value {
    for _ in 0..200 {
        let id = client.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": session_id, "commandId": command_id}]}),
        );
        let response = client.response(id);
        let result = response["result"]["results"]
            .as_array()
            .and_then(|items| items.first())
            .map(|item| item["result"].clone())
            .unwrap_or(Value::Null);
        if result["status"].as_str().is_some() {
            return result;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("command {command_id} never settled");
}

fn review_interactions(snapshot: &Value) -> Vec<Value> {
    snapshot["pendingInteractions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|interaction| interaction["kind"] == json!("workspaceHookReview"))
        .collect()
}

#[test]
fn hook_review_banner_grant_revoke_toggle_and_restart() {
    let (base, workspace, data_dir) = make_workspace("lifecycle");
    let config_path = base.join("provider.json");
    let (process, mut client) = start(&workspace, &data_dir, &config_path);
    let session_id = create_session(&mut client);

    // ── 1. 启动即发现 hook，横幅与评审交互就位 ──
    let snapshot = snapshot_state(&mut client, &session_id);
    let admission = &snapshot["workspaceHookAdmission"];
    assert_eq!(
        admission["pendingCount"],
        json!(2),
        "两条 hook 都待审：{snapshot}"
    );
    assert_eq!(
        admission["workspaceIdentity"],
        json!(workspace.to_string_lossy().to_string())
    );
    let reviews = review_interactions(&snapshot);
    assert_eq!(reviews.len(), 1, "{snapshot}");
    let review = reviews[0]["payload"].clone();
    assert_eq!(review["kind"], json!("workspaceHookReview"));
    assert_eq!(
        reviews[0]["interactionId"], review["interactionId"],
        "interactionId 必须与 payload 一致"
    );
    assert_eq!(review["summary"]["hookCount"], json!(2));
    assert_eq!(review["summary"]["pendingCount"], json!(2));
    assert_eq!(review["warningCode"], json!("workspace_hooks_execute_code"));
    assert_eq!(review["items"][0]["displayCommand"], json!("node check.js"));
    assert_eq!(
        review["items"][0]["displayName"],
        json!("PreToolUse · Bash")
    );
    assert_eq!(review["items"][1]["type"], json!("process"));
    assert_eq!(review["items"][1]["displayCommand"], json!("git status"));

    // ── 2. 授权：台账落盘，横幅消失 ──
    let respond = command_envelope(
        "respondWorkspaceHookReview",
        "cmd-respond",
        Some(&session_id),
        json!({
            "sessionId": session_id,
            "taskId": review["taskId"],
            "runId": review["runId"],
            "workspaceIdentity": review["workspaceIdentity"],
            "bundleDigest": review["bundleDigest"],
            "reviewFlowId": review["reviewFlowId"],
            "generation": review["generation"],
            "interactionId": review["interactionId"],
            "decision": {
                "action": "trust_selected",
                "reviewItemIds": [review["items"][0]["reviewItemId"]],
            },
        }),
    );
    let id = client.send("v4/command", respond);
    let _ = client.response(id);
    let settled = settle(&mut client, &session_id, "cmd-respond");
    assert_eq!(settled["status"], json!("accepted"), "{settled}");
    let snapshot = snapshot_state(&mut client, &session_id);
    assert_eq!(
        snapshot["workspaceHookAdmission"]["pendingCount"],
        json!(1),
        "只授权了一条：{snapshot}"
    );
    let store_path = data_dir
        .join("security")
        .join("workspace-hook-trust-v1.json");
    let store: Value = serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap())
        .expect("trust store json");
    assert_eq!(store["schemaVersion"], json!(1));
    assert_eq!(store["records"].as_array().unwrap().len(), 1);
    let trusted_digest = store["records"][0]["hookDeclarationDigest"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        review_interactions(&snapshot)[0]["payload"]["items"][0]["trustState"],
        json!("trusted_persistent"),
        "已授权条目的状态必须反映在重开的评审里"
    );

    // ── 3. 撤销：记录消失、条目变 revoked、评审重开 ──
    let next_review = review_interactions(&snapshot)[0]["payload"].clone();
    let revoke = command_envelope(
        "revokeWorkspaceHookTrust",
        "cmd-revoke",
        Some(&session_id),
        json!({
            "sessionId": session_id,
            "workspaceIdentity": next_review["workspaceIdentity"],
            "bundleDigest": next_review["bundleDigest"],
            "reviewFlowId": next_review["reviewFlowId"],
            "generation": next_review["generation"],
            "interactionId": next_review["interactionId"],
            "reviewItemIds": [next_review["items"][0]["reviewItemId"]],
        }),
    );
    let id = client.send("v4/command", revoke);
    let _ = client.response(id);
    let settled = settle(&mut client, &session_id, "cmd-revoke");
    assert_eq!(settled["status"], json!("accepted"), "{settled}");
    let snapshot = snapshot_state(&mut client, &session_id);
    assert_eq!(snapshot["workspaceHookAdmission"]["pendingCount"], json!(2));
    let store: Value = serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap())
        .expect("trust store json");
    assert!(
        store["records"].as_array().unwrap().is_empty(),
        "撤销必须删除持久记录"
    );
    let reopened = review_interactions(&snapshot)[0]["payload"].clone();
    assert!(
        reopened["generation"].as_u64().unwrap() > next_review["generation"].as_u64().unwrap(),
        "撤销后评审必须换 generation 重开"
    );
    assert_eq!(reopened["items"][0]["trustState"], json!("revoked"));

    // ── 4. 开关：真的写回 `.zcode/config.json`，并因 bundle 变化重开评审 ──
    let toggle_target = reopened["items"][1]["reviewItemId"].clone();
    let toggle = command_envelope(
        "toggleWorkspaceHookReviewItem",
        "cmd-toggle",
        Some(&session_id),
        json!({
            "sessionId": session_id,
            "taskId": reopened["taskId"],
            "runId": reopened["runId"],
            "workspaceIdentity": reopened["workspaceIdentity"],
            "bundleDigest": reopened["bundleDigest"],
            "reviewFlowId": reopened["reviewFlowId"],
            "generation": reopened["generation"],
            "interactionId": reopened["interactionId"],
            "reviewItemId": toggle_target,
            "enabled": false,
        }),
    );
    let id = client.send("v4/command", toggle);
    let _ = client.response(id);
    let settled = settle(&mut client, &session_id, "cmd-toggle");
    assert_eq!(settled["status"], json!("accepted"), "{settled}");
    let config: Value = serde_json::from_str(
        &std::fs::read_to_string(workspace.join(".zcode").join("config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(config["theme"], json!("dark"), "无关键保留");
    assert_eq!(
        config["hooks"]["events"]["Stop"][0]["hooks"][0]["enabled"],
        json!(false)
    );
    let snapshot = snapshot_state(&mut client, &session_id);
    let after_toggle = review_interactions(&snapshot)[0]["payload"].clone();
    assert!(
        after_toggle["generation"].as_u64().unwrap() > reopened["generation"].as_u64().unwrap()
    );
    assert_eq!(after_toggle["items"][1]["configuredEnabled"], json!(false));

    // ── 5. 过期 bundle 的按需评审必须拒绝 ──
    let stale = command_envelope(
        "requestWorkspaceHookReview",
        "cmd-request-stale",
        Some(&session_id),
        json!({
            "sessionId": session_id,
            "workspaceIdentity": after_toggle["workspaceIdentity"],
            "bundleDigest": "0".repeat(64),
        }),
    );
    let id = client.send("v4/command", stale);
    let _ = client.response(id);
    let settled = settle(&mut client, &session_id, "cmd-request-stale");
    assert_eq!(settled["status"], json!("rejected"), "{settled}");
    assert_eq!(
        settled["reasonCode"],
        json!("workspace_hooks_snapshot_mismatch")
    );
    // 当前 bundle 的按需评审是幂等成功（复用已有 pending flow）。
    let current = command_envelope(
        "requestWorkspaceHookReview",
        "cmd-request-current",
        Some(&session_id),
        json!({
            "sessionId": session_id,
            "workspaceIdentity": after_toggle["workspaceIdentity"],
            "bundleDigest": after_toggle["bundleDigest"],
        }),
    );
    let id = client.send("v4/command", current);
    let _ = client.response(id);
    let settled = settle(&mut client, &session_id, "cmd-request-current");
    assert_eq!(settled["status"], json!("accepted"), "{settled}");

    // ── 6. 重启：信任 durable；撤销只影响本进程（TS 同：撤销后就是"没有记录"） ──
    let digest = trusted_digest.clone();
    let respawn = command_envelope(
        "respondWorkspaceHookReview",
        "cmd-respond-2",
        Some(&session_id),
        json!({
            "sessionId": session_id,
            "taskId": after_toggle["taskId"],
            "runId": after_toggle["runId"],
            "workspaceIdentity": after_toggle["workspaceIdentity"],
            "bundleDigest": after_toggle["bundleDigest"],
            "reviewFlowId": after_toggle["reviewFlowId"],
            "generation": after_toggle["generation"],
            "interactionId": after_toggle["interactionId"],
            "decision": {
                "action": "trust_selected",
                "reviewItemIds": [after_toggle["items"][0]["reviewItemId"]],
            },
        }),
    );
    let id = client.send("v4/command", respawn);
    let _ = client.response(id);
    let settled = settle(&mut client, &session_id, "cmd-respond-2");
    assert_eq!(settled["status"], json!("accepted"), "{settled}");
    let store: Value = serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap())
        .expect("trust store json");
    assert_eq!(store["records"].as_array().unwrap().len(), 1);
    assert_eq!(
        store["records"][0]["hookDeclarationDigest"],
        json!(digest),
        "重新授权同一个声明写回同一条记录"
    );

    drop(process);
    drop(client);
    let (_restarted_process, mut restarted) = start(&workspace, &data_dir, &config_path);
    let snapshot = snapshot_state(&mut restarted, &session_id);
    assert_eq!(
        snapshot["workspaceHookAdmission"]["pendingCount"],
        json!(1),
        "重启后只剩未授权的那条待审：{snapshot}"
    );
    assert_eq!(
        review_interactions(&snapshot)[0]["payload"]["items"][0]["trustState"],
        json!("trusted_persistent")
    );

    let _ = std::fs::remove_dir_all(&base);
}
