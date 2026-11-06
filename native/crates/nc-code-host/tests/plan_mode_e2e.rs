//! plan mode e2e: the gate really blocks writes, approval really lifts the gate, and the plan really lands on disk.
//!
//! None of the three are display-layer assertions:
//! 1. **after EnterPlanMode, writing a file must fail**. The verdict looks at that tool result inside **the next model request body**
//!    plus "the file really does not exist" — asserting only `config.planEnabled == true` would mean that when the gate breaks,
//!    the UI still shows "plan mode" while the file has already been written.
//! 2. **ExitPlanMode's approval is an interaction**: `pendingInteraction(kind=userInput)` +
//!    `schema.interaction == "plan_approval"`; after approve, the mode is restored, the plan is written into the workspace,
//!    and `v4/conversation/plans` can read this terminal catalog entry.
//! 3. **headless does not grant it**: with nobody approving, the mode must stay in plan and the plan file must not exist.

mod common;

use common::{
    command_envelope, deltas_from_frame, text_sse, tool_use_sse_with_id, Client, HostProcess,
    ScriptedResponder,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PLAN: &str = "# Plan\n1. Extract the pane shell\n2. Wire the tabs\n";
const APPROVE_REASON: &str = "ExitPlanMode changes session mode after user plan approval";

fn temp_base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zcode-host-plan-mode-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn write_provider_config(port: u16, path: &Path) {
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

fn connect(
    workspace: &Path,
    data_dir: &Path,
    config_path: &Path,
    full_access: bool,
) -> (HostProcess, Client) {
    let mut process = HostProcess::spawn(workspace, data_dir, config_path, full_access);
    let client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    (process, client)
}

fn open_session(client: &mut Client, subscribe: bool) -> String {
    let id = client.send(
        "v4/command",
        command_envelope("createSession", "cmd-create", None, json!({})),
    );
    let session_id = client.response(id)["result"]["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    if subscribe {
        let id = client.send(
            "v4/conversation/subscribe",
            json!({
                "sessionId": session_id,
                "connectionId": "conn-1",
                "clientMode": "desktop-continuous"
            }),
        );
        client.response(id);
    }
    session_id
}

fn submit(client: &mut Client, session_id: &str, command_id: &str, text: &str) {
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            command_id,
            Some(session_id),
            json!({"text": text}),
        ),
    );
    let response = client.response(id);
    assert_eq!(
        response["result"]["status"],
        json!("accepted"),
        "{command_id} must be admitted: {response}"
    );
}

fn wait_turn_settled(client: &mut Client, session_id: &str, command_id: &str) {
    for _ in 0..400 {
        let id = client.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": session_id, "commandId": command_id}]}),
        );
        let entry = client.response(id)["result"]["results"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        if entry["result"] != json!("unknown") && entry["result"] != Value::Null {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("command {command_id} never settled");
}

/// Observe the two projections of plan mode: the one carrying `config.planEnabled`, and the one carrying the plan approval interaction.
///
/// The two observations must be collected in the **same pump**, not written as two loops waiting one after the other: the host's flush merges
/// the same batch of deltas into one frame (the merge granularity depends on timing), so the loop waiting for `planEnabled` first would
/// **drop the same batch's pendingInteractions** in the frame it hits, and the second loop would never see that frame —
/// under parallel load this reproduced reliably as a blocking read. Bookkeeping both within the merged batch is the correct way to read.
fn observe_plan_state(
    client: &mut Client,
    session_id: &str,
    want_plan_enabled: bool,
    need_approval: bool,
) -> (Value, Option<(String, Value)>) {
    let sid = format!("conn-1:conversation/{session_id}");
    let mut config = Value::Null;
    let mut approval: Option<(String, Value)> = None;
    for _ in 0..1200 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        for delta in deltas_from_frame(&line) {
            let patch = &delta["patch"];
            if config.is_null()
                && patch["config"]["planEnabled"].as_bool() == Some(want_plan_enabled)
            {
                config = patch["config"].clone();
            }
            if approval.is_none() {
                if let Some(items) = patch["pendingInteractions"].as_array() {
                    if let Some(item) = items.iter().find(|item| {
                        item["payload"]["schema"]["interaction"].as_str() == Some("plan_approval")
                    }) {
                        approval = Some((
                            item["interactionId"]
                                .as_str()
                                .expect("interactionId")
                                .to_string(),
                            item.clone(),
                        ));
                    }
                }
            }
        }
        let have_config = !config.is_null();
        let have_approval = !need_approval || approval.is_some();
        if have_config && have_approval {
            return (config, approval);
        }
    }
    panic!("plan observation incomplete (config={config} approval={approval:?}) for {session_id}");
}

fn resolve(
    client: &mut Client,
    session_id: &str,
    command_id: &str,
    interaction_id: &str,
    answer: Value,
) {
    let id = client.send(
        "v4/command",
        command_envelope(
            "resolveInteraction",
            command_id,
            Some(session_id),
            json!({"interactionId": interaction_id, "answer": answer}),
        ),
    );
    let response = client.response(id);
    assert_eq!(
        response["result"]["status"],
        json!("accepted"),
        "resolveInteraction must be admitted: {response}"
    );
}

/// The catalog of `v4/conversation/plans` (a read-only query surface).
fn plans_directory(client: &mut Client, session_id: &str) -> Value {
    let id = client.send("v4/conversation/plans", json!({"sessionId": session_id}));
    let response = client.response(id);
    assert!(
        response["error"].is_null(),
        "plans query must not error: {response}"
    );
    response["result"].clone()
}

/// The request body is JSON: the quotes in the text are escaped, so assertions must compare against the same escaped form.
fn escaped_in_json_body(text: &str) -> String {
    let rendered = json!(text).to_string();
    rendered[1..rendered.len() - 1].to_string()
}

/// The plan body on disk (the session id in the file name is sanitized; do not guess the format: the directory should hold only one .md).
fn persisted_plan(workspace: &Path) -> Option<String> {
    let dir = workspace.join(".zcode").join("plans");
    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            return std::fs::read_to_string(&path).ok();
        }
    }
    None
}

#[test]
fn plan_mode_blocks_writes_until_the_user_approves_the_plan() {
    // The session carries `--full-access`: this is exactly the point this test hits — TS places the plan gate
    // **before** `mode.yolo`/global allowance, so a "session granted everything" still cannot write files after entering plan mode.
    // After approval the same write must **succeed**, otherwise "exiting plan mode" is only a display-level mode label.
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse_with_id("toolu_enter", "EnterPlanMode", &json!({})),
        tool_use_sse_with_id(
            "toolu_write_before",
            "Write",
            &json!({"file_path": "premature.txt", "content": "nope"}),
        ),
        tool_use_sse_with_id("toolu_exit", "ExitPlanMode", &json!({"plan": PLAN})),
        tool_use_sse_with_id(
            "toolu_write_after",
            "Write",
            &json!({"file_path": "after-approval.txt", "content": "implemented"}),
        ),
        text_sse("implementing the approved plan"),
    ]);
    let base_dir = temp_base("approve");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let (_process, mut client) = connect(&workspace, &data_dir, &config_path, true);
    let session_id = open_session(&mut client, true);
    submit(
        &mut client,
        &session_id,
        "cmd-plan",
        "restructure the settings pane",
    );

    // 1+2. Enter plan mode (mode and planEnabled are projected together, and the renderer's plan toggle reads the latter),
    // and take the approval interaction within the same pump — if the approval is never answered, this turn waits there forever (and never settles).
    let (config, approval) = observe_plan_state(&mut client, &session_id, true, true);
    assert_eq!(config["mode"], json!("plan"), "{config}");
    let (interaction_id, item) = approval.expect("plan_approval interaction");
    assert!(
        item["payload"]["input"]["plan"].is_string(),
        "the approval carries the plan body: {}",
        item["payload"]
    );
    resolve(
        &mut client,
        &session_id,
        "cmd-approve",
        &interaction_id,
        json!({"action": "accept", "content": {"answer": "approve"}}),
    );
    wait_turn_settled(&mut client, &session_id, "cmd-plan");

    // 3. The gate blocked the write **before execution** (the file does not exist), and after approval the same write landed on disk.
    assert!(
        !workspace.join("premature.txt").exists(),
        "plan mode denied the write, so the file must not exist"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("after-approval.txt"))
            .ok()
            .as_deref(),
        Some("implemented"),
        "approval must actually release the gate, not just relabel the session"
    );
    let bodies = provider.request_bodies();
    assert_eq!(
        bodies.len(),
        5,
        "enter → denied write → approved exit → allowed write → final text: {bodies:?}"
    );
    assert!(
        bodies[2].contains("Plan mode only allows read-only, non-destructive tools"),
        "the model must be told WHY the write failed:\n{}",
        bodies[2]
    );
    assert!(
        bodies[3].contains(&escaped_in_json_body(
            "User has approved your plan. You can now start coding. Start with updating your todo list if applicable.\n\n## Approved Plan:\n# Plan\n1. Extract the pane shell\n2. Wire the tabs\n"
        )),
        "the approved plan must come back to the model verbatim:\n{}",
        bodies[3]
    );
    assert!(
        bodies[4].contains("wrote 11 bytes to after-approval.txt"),
        "the post-approval write must go through:\n{}",
        bodies[4]
    );

    // 4. The plan lands on disk (TS's order: persist first, then exit).
    assert_eq!(
        persisted_plan(&workspace).as_deref(),
        Some(PLAN),
        "the approved plan must be written into the workspace"
    );

    // 5. The `v4/conversation/plans` catalog entry comes from that terminal toolCall row.
    let directory = plans_directory(&mut client, &session_id);
    let plans = directory["plans"]
        .as_array()
        .unwrap_or_else(|| panic!("plans must be an array: {directory}"));
    assert_eq!(plans.len(), 1, "one terminal ExitPlanMode row: {plans:?}");
    assert_eq!(plans[0]["toolName"], json!("ExitPlanMode"));
    assert_eq!(plans[0]["status"], json!("success"));
    assert_eq!(
        plans[0]["input"]["plan"],
        json!(PLAN),
        "the read model needs row.input.plan (getPlans filter)"
    );
    assert!(directory["atSeq"].is_number(), "{directory}");
    assert!(
        directory["atLogEpoch"]
            .as_str()
            .is_some_and(|epoch| !epoch.is_empty()),
        "{directory}"
    );
}

#[test]
fn plan_approval_is_a_user_input_interaction_and_feedback_is_not_approval() {
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse_with_id("toolu_enter", "EnterPlanMode", &json!({})),
        tool_use_sse_with_id("toolu_exit", "ExitPlanMode", &json!({"plan": PLAN})),
        tool_use_sse_with_id(
            "toolu_write_after_feedback",
            "Write",
            &json!({"file_path": "premature.txt", "content": "nope"}),
        ),
        text_sse("revising after feedback"),
    ]);
    let base_dir = temp_base("payload");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let (_process, mut client) = connect(&workspace, &data_dir, &config_path, true);
    let session_id = open_session(&mut client, true);
    submit(&mut client, &session_id, "cmd-payload", "plan it, then ask");

    let (config, approval) = observe_plan_state(&mut client, &session_id, true, true);
    assert_eq!(config["mode"], json!("plan"), "{config}");
    let (interaction_id, item) = approval.expect("plan_approval interaction");

    // The payload shape aligns field by field with TS (`product-projection.ts:3111-3131`).
    let payload = &item["payload"];
    assert_eq!(payload["kind"], json!("userInput"));
    assert_eq!(payload["toolName"], json!("ExitPlanMode"));
    assert_eq!(payload["freeText"], json!(true));
    assert_eq!(payload["prompt"], json!(APPROVE_REASON));
    assert_eq!(
        payload["schema"]["toolName"],
        json!("ExitPlanMode"),
        "{payload}"
    );
    assert_eq!(
        payload["input"]["plan"],
        json!(PLAN),
        "the plan body rides on payload.input.plan"
    );
    assert_eq!(
        payload["questions"][0]["question"],
        json!(APPROVE_REASON),
        "the question text is the reason, not the plan"
    );
    assert_eq!(payload["questions"][0]["header"], json!("Plan"));
    assert_eq!(
        payload["questions"][0]["options"][0]["value"],
        json!("approve")
    );
    assert_eq!(
        payload["questions"][0]["options"][0]["label"],
        json!("Approve")
    );
    assert!(
        item["anchorRowId"].is_number(),
        "the approval must anchor its toolCall row: {item}"
    );

    // Free text = revision feedback: no approval, nothing persisted, and the mode stays in plan.
    resolve(
        &mut client,
        &session_id,
        "cmd-resolve",
        &interaction_id,
        json!({"action": "accept", "content": {"answer": "split step 2 first"}}),
    );
    wait_turn_settled(&mut client, &session_id, "cmd-payload");

    assert!(
        persisted_plan(&workspace).is_none(),
        "feedback is not an approval"
    );
    let bodies = provider.request_bodies();
    assert!(
        bodies[2].contains(&escaped_in_json_body(
            "The user asked for changes before approving: split step 2 first. Revise the plan and call ExitPlanMode again."
        )),
        "the feedback must reach the model:\n{}",
        bodies[2]
    );
    // The gate was not lifted: the write after the feedback is still refused by plan mode (the session is full-access, so the refusal
    // can only come from the plan check), and the file still does not exist.
    assert!(
        bodies[3].contains("Plan mode only allows read-only, non-destructive tools"),
        "the gate stays on after feedback:\n{}",
        bodies[3]
    );
    assert!(
        !workspace.join("premature.txt").exists(),
        "a declined plan must not unlock writes"
    );
}

#[test]
fn headless_exit_plan_mode_is_declined_and_stays_in_plan_mode() {
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse_with_id("toolu_enter", "EnterPlanMode", &json!({})),
        tool_use_sse_with_id("toolu_exit", "ExitPlanMode", &json!({"plan": PLAN})),
        text_sse("staying in plan mode"),
    ]);
    let base_dir = temp_base("headless");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let (_process, mut client) = connect(&workspace, &data_dir, &config_path, false);
    // No subscribe: there is nobody who can approve.
    let session_id = open_session(&mut client, false);
    submit(
        &mut client,
        &session_id,
        "cmd-headless",
        "plan it and exit, nobody watching",
    );
    wait_turn_settled(&mut client, &session_id, "cmd-headless");

    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 3, "the turn must not stall: {bodies:?}");
    assert!(
        bodies[2].contains("The user did not approve exiting plan mode"),
        "headless must report a decline, not an approval:\n{}",
        bodies[2]
    );
    assert!(
        !bodies[2].contains("User has approved your plan"),
        "an unattended plan is never approved:\n{}",
        bodies[2]
    );
    assert!(
        persisted_plan(&workspace).is_none(),
        "no plan file without an approval"
    );
}

#[test]
fn exit_plan_mode_is_denied_outside_plan_mode() {
    // TS `resolvePlanModeTransitionPermission`: `mode.plan.exitOnly` is ranked ahead of every other check,
    // so in a non-plan session ExitPlanMode does not even open an approval interaction.
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse_with_id("toolu_exit", "ExitPlanMode", &json!({"plan": PLAN})),
        text_sse("no plan mode to exit"),
    ]);
    let base_dir = temp_base("exit-only");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let (_process, mut client) = connect(&workspace, &data_dir, &config_path, false);
    let session_id = open_session(&mut client, true);
    submit(
        &mut client,
        &session_id,
        "cmd-exit-only",
        "just exit plan mode please",
    );
    wait_turn_settled(&mut client, &session_id, "cmd-exit-only");

    let bodies = provider.request_bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(
        bodies[1].contains("ExitPlanMode can only be used while plan mode is active"),
        "the deny reason must reach the model:\n{}",
        bodies[1]
    );
    assert!(
        persisted_plan(&workspace).is_none(),
        "a denied transition must not persist anything"
    );
}
