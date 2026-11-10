//! workspace hook 执行 e2e：真实工作区 + 真实 node hook 进程 + 真实 turn。
//!
//! 覆盖的产品事实：
//! 1. 受信任的 PreToolUse hook 返回 deny → **工具不执行**（文件不存在），错误结果
//!    （`tool.permissionDenied`）与 hook 上下文都回灌给模型；
//! 2. SessionStart 的 additionalContext 在重启后（hook 已受信任）注入下一次模型请求；
//! 3. 每次事件都投影一条 `hookInvocation` 行（didExecute/outcome 是可读事实）。

mod common;

use common::{
    command_envelope, text_sse, Client, HostProcess, ScriptedProvider, ScriptedResponder,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

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

const DENY_SCRIPT: &str = r#"
process.stdin.resume();
setTimeout(() => {
  process.stdout.write(JSON.stringify({
    hookSpecificOutput: {
      hookEventName: 'PreToolUse',
      permissionDecision: 'deny',
      permissionDecisionReason: 'no writes allowed'
    },
    additionalContext: 'policy: no writes'
  }));
}, 20);
"#;

const CONTEXT_SCRIPT: &str = r#"
process.stdin.resume();
setTimeout(() => {
  process.stdout.write(JSON.stringify({ additionalContext: 'remember the workspace rules' }));
}, 20);
"#;

fn make_workspace(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base =
        std::env::temp_dir().join(format!("zcode-host-hook-exec-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(workspace.join(".zcode")).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    std::fs::write(workspace.join("deny.js"), DENY_SCRIPT).expect("deny script");
    std::fs::write(workspace.join("ctx.js"), CONTEXT_SCRIPT).expect("ctx script");
    std::fs::write(
        workspace.join(".zcode").join("config.json"),
        r#"{
  "hooks": {
    "enabled": true,
    "events": {
      "PreToolUse": [
        {"matcher": "Write", "hooks": [{"type": "process", "command": "node", "args": ["deny.js"]}]}
      ],
      "SessionStart": [
        {"hooks": [{"type": "process", "command": "node", "args": ["ctx.js"]}]}
      ]
    }
  }
}
"#,
    )
    .expect("config");
    (base, workspace, data_dir)
}

/// 每次 spawn 都重新落一份 handover 文件：host 按"一次性密钥文件"契约在启动后
/// 删除 `--provider-config`（`main.rs` `remove_file`），重启复用同一路径会拿到
/// `provider=unconfigured`。生产侧桌面也是每次 spawn 新写一份，测试照同一语义。
fn start(
    workspace: &Path,
    data_dir: &Path,
    config_path: &Path,
    port: u16,
) -> (HostProcess, Client) {
    provider_config(port, config_path);
    let mut process = HostProcess::spawn(workspace, data_dir, config_path, false);
    let client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    (process, client)
}

fn snapshot_state(client: &mut Client, session_id: &str) -> Value {
    let id = client.send(
        "v4/conversation/subscribe",
        json!({
            "sessionId": session_id,
            "connectionId": "conn-hook-exec",
            "clientMode": "desktop-continuous",
        }),
    );
    let _ = client.response(id);
    let mut skipped = std::collections::VecDeque::new();
    for _ in 0..400 {
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

fn settle(client: &mut Client, session_id: &str, command_id: &str) -> Value {
    for _ in 0..300 {
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
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("command {command_id} never settled");
}

/// 一次性信任评审里的全部条目（generation 会因部分授权而前进）。
fn trust_all_hooks(client: &mut Client, session_id: &str) {
    for _ in 0..5 {
        let snapshot = snapshot_state(client, session_id);
        let interactions = snapshot["pendingInteractions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let Some(review) = interactions
            .iter()
            .find(|interaction| interaction["kind"] == json!("workspaceHookReview"))
            .map(|interaction| interaction["payload"].clone())
        else {
            return;
        };
        let pending_ids: Vec<Value> = review["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|item| {
                matches!(
                    item["trustState"].as_str(),
                    Some("pending_trust") | Some("stale_digest") | Some("revoked")
                )
            })
            .map(|item| item["reviewItemId"].clone())
            .collect();
        if pending_ids.is_empty() {
            return;
        }
        let command_id = format!("cmd-trust-{}", review["generation"]);
        let id = client.send(
            "v4/command",
            json!({
                "type": "respondWorkspaceHookReview",
                "commandId": command_id,
                "clientId": "e2e",
                "sessionId": session_id,
                "payload": {
                    "sessionId": session_id,
                    "taskId": review["taskId"],
                    "runId": review["runId"],
                    "workspaceIdentity": review["workspaceIdentity"],
                    "bundleDigest": review["bundleDigest"],
                    "reviewFlowId": review["reviewFlowId"],
                    "generation": review["generation"],
                    "interactionId": review["interactionId"],
                    "decision": {"action": "trust_selected", "reviewItemIds": pending_ids},
                },
                "issuedAt": 1_700_000_000_000.0,
            }),
        );
        let _ = client.response(id);
        let settled = settle(client, session_id, &command_id);
        assert_eq!(settled["status"], json!("accepted"), "{settled}");
    }
}

#[test]
fn a_trusted_pre_tool_use_hook_can_block_a_tool_and_inject_context() {
    let (provider, port) = ScriptedProvider::spawn();
    let (base, workspace, data_dir) = make_workspace("deny");
    let config_path = base.join("provider.json");
    let (process, mut client) = start(&workspace, &data_dir, &config_path, port);

    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    trust_all_hooks(&mut client, &session_id);

    // 第一轮：模型要求 Write → PreToolUse hook 拒绝 → 文件不能出现。
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-write",
            Some(&session_id),
            json!({"text": "write the file"}),
        ),
    );
    let _ = client.response(id);
    let settled = settle(&mut client, &session_id, "cmd-write");
    assert_eq!(settled["status"], json!("accepted"), "{settled}");
    assert!(
        !workspace.join("native-e2e.txt").exists(),
        "被 hook 拒绝的工具绝不能落盘"
    );

    // 模型第二次请求看到的是 hook 的附加上下文。
    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(
        bodies[1].contains("policy: no writes"),
        "PreToolUse additionalContext 必须回灌给模型：{}",
        bodies[1]
    );
    assert!(
        bodies[1].contains("blocked by workspace hook"),
        "工具结果必须说明是被 hook 拒绝的：{}",
        bodies[1]
    );

    // 时间线：PreToolUse 的 hookInvocation 行（真的执行过，outcome=blocked）。
    let snapshot = snapshot_state(&mut client, &session_id);
    let rows = snapshot["rows"]["window"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let invocation = rows
        .iter()
        .find(|row| {
            row["kind"] == json!("hookInvocation") && row["hookEventName"] == json!("PreToolUse")
        })
        .expect("a PreToolUse hookInvocation row");
    assert_eq!(invocation["state"], json!("completed"), "{invocation}");
    assert_eq!(invocation["executions"][0]["didExecute"], json!(true));
    assert_eq!(invocation["executions"][0]["outcome"], json!("blocked"));
    assert_eq!(invocation["lane"], json!("toolBefore"));
    let tool_row = rows
        .iter()
        .rev()
        .find(|row| row["kind"] == json!("toolCall"))
        .expect("tool row");
    assert_eq!(
        tool_row["error"]["code"],
        json!("tool.permissionDenied"),
        "{tool_row}"
    );

    // 重启后 SessionStart hook（此时已受信任）把上下文注入下一次用户轮。
    drop(process);
    drop(client);
    let (_process2, mut restarted) = start(&workspace, &data_dir, &config_path, port);
    let snapshot = snapshot_state(&mut restarted, &session_id);
    let session_rows = snapshot["rows"]["window"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    // 第一次 SessionStart（未受信任）留下的 skipped 行仍在历史里；取最新一条。
    let session_start = session_rows
        .iter()
        .rev()
        .find(|row| {
            row["kind"] == json!("hookInvocation") && row["hookEventName"] == json!("SessionStart")
        })
        .expect("SessionStart invocation row after resume");
    assert_eq!(session_start["executions"][0]["didExecute"], json!(true));

    let id = restarted.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-hello",
            Some(&session_id),
            json!({"text": "hello again"}),
        ),
    );
    let _ = restarted.response(id);
    let settled = settle(&mut restarted, &session_id, "cmd-hello");
    assert_eq!(settled["status"], json!("accepted"), "{settled}");
    let bodies = provider.request_bodies();
    let last = bodies.last().expect("a request after resume");
    assert!(
        last.contains("remember the workspace rules"),
        "SessionStart additionalContext 必须注入第一条用户消息：{last}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// Stop hook 请求"继续"时，轮结束后跑一层受控续跑（system-reminder 输入）。
#[test]
fn a_stop_hook_can_request_one_controlled_continuation() {
    let (provider, port) =
        ScriptedResponder::spawn(vec![text_sse("first answer"), text_sse("second answer")]);
    let base = std::env::temp_dir().join(format!("zcode-host-hook-stop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(workspace.join(".zcode")).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    std::fs::write(
        workspace.join("stop.js"),
        "process.stdin.resume();setTimeout(() => {process.stdout.write(JSON.stringify({continue: true, stopReason: 'keep going', additionalContext: 'keep going context'}));}, 20);",
    )
    .expect("stop script");
    std::fs::write(
        workspace.join(".zcode").join("config.json"),
        r#"{"hooks":{"enabled":true,"events":{"Stop":[{"hooks":[{"type":"process","command":"node","args":["stop.js"]}]}]}}}"#,
    )
    .expect("config");
    let config_path = base.join("provider.json");
    let (process, mut client) = start(&workspace, &data_dir, &config_path, port);

    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    trust_all_hooks(&mut client, &session_id);

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-stop",
            Some(&session_id),
            json!({"text": "do the thing"}),
        ),
    );
    let _ = client.response(id);
    let settled = settle(&mut client, &session_id, "cmd-stop");
    assert_eq!(settled["status"], json!("accepted"), "{settled}");

    // Stop hook 在命令 settle 之后才运行（轮已收口），所以这里等第二个模型请求，
    // 而不是立刻读——否则会在续跑轮还没起跑时断言。
    // TS `MAX_STOP_HOOK_CONTINUATIONS = 3`：同一轮最多被 Stop hook 延长 3 次，
    // 所以总共 4 次模型请求（首次 + 3 次续跑）。
    let mut bodies = provider.request_bodies();
    for _ in 0..200 {
        if bodies.len() >= 4 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
        bodies = provider.request_bodies();
    }
    assert_eq!(
        bodies.len(),
        4,
        "Stop hook continue:true 最多续跑 3 层: {bodies:?}"
    );
    assert!(
        bodies[1].contains("keep going context"),
        "续跑轮的输入必须带上 hook 的 additionalContext: {}",
        bodies[1]
    );
    assert!(
        bodies[3].contains("Stop hook additional context:"),
        "续跑上下文带 TS 同款标题: {}",
        bodies[3]
    );
    let snapshot = snapshot_state(&mut client, &session_id);
    let rows = snapshot["rows"]["window"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    // 与 TS 一致：续跑发生在**同一个 product turn 内**（不新增 turnHeader）。
    let headers: Vec<&Value> = rows
        .iter()
        .filter(|row| row["kind"] == json!("turnHeader"))
        .collect();
    assert_eq!(headers.len(), 1, "Stop 续跑不新增轮次：{headers:?}");
    assert_eq!(headers[0]["origin"], json!("userInput"));
    assert_eq!(headers[0]["state"], json!("completedSuccess"));
    assert_eq!(
        rows.iter()
            .filter(|row| row["kind"] == json!("hookInvocation")
                && row["hookEventName"] == json!("Stop"))
            .count(),
        3,
        "Stop hook 每次续跑都留一条 hookInvocation 行"
    );

    let _ = std::fs::remove_dir_all(&base);
    drop(process);
}

/// PermissionRequest hook 的 deny 是最终决定：不弹 permission、工具不执行。
#[test]
fn a_permission_request_hook_can_deny_instead_of_asking_the_user() {
    let (_provider, port) = ScriptedProvider::spawn();
    let base =
        std::env::temp_dir().join(format!("zcode-host-hook-permission-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(workspace.join(".zcode")).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    std::fs::write(
        workspace.join("permission.js"),
        "process.stdin.resume();setTimeout(() => {process.stdout.write(JSON.stringify({hookSpecificOutput:{hookEventName:'PermissionRequest',decision:{behavior:'deny',message:'no writes by policy'}}}));}, 20);",
    )
    .expect("permission script");
    std::fs::write(
        workspace.join(".zcode").join("config.json"),
        r#"{"hooks":{"enabled":true,"events":{"PermissionRequest":[{"matcher":"Write","hooks":[{"type":"process","command":"node","args":["permission.js"]}]}]}}}"#,
    )
    .expect("config");
    let config_path = base.join("provider.json");
    let (process, mut client) = start(&workspace, &data_dir, &config_path, port);

    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    trust_all_hooks(&mut client, &session_id);

    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-write",
            Some(&session_id),
            json!({"text": "write the file"}),
        ),
    );
    let _ = client.response(id);
    let settled = settle(&mut client, &session_id, "cmd-write");
    assert_eq!(settled["status"], json!("accepted"), "{settled}");
    assert!(
        !workspace.join("native-e2e.txt").exists(),
        "PermissionRequest hook deny 后工具绝不能执行"
    );
    let snapshot = snapshot_state(&mut client, &session_id);
    let rows = snapshot["rows"]["window"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !rows
            .iter()
            .any(|row| row["kind"] == json!("permissionRequest")),
        "hook 已拒绝，不应再弹用户 permission"
    );
    let tool_row = rows
        .iter()
        .rev()
        .find(|row| row["kind"] == json!("toolCall"))
        .expect("tool row");
    assert_eq!(tool_row["error"]["code"], json!("tool.permissionDenied"));
    assert!(rows.iter().any(|row| row["kind"] == json!("hookInvocation")
        && row["hookEventName"] == json!("PermissionRequest")));

    let _ = std::fs::remove_dir_all(&base);
    drop(process);
}
