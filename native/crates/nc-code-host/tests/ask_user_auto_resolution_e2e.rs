//! AskUserQuestion's auto-convergence countdown: when nobody answers, the turn closes by itself, and after a snooze it no longer closes.
//!
//! Both are behavioral assertions, not field assertions:
//! 1. **at the deadline an empty answer is accepted automatically**. The assertion lands on the second model request body — the model must receive
//!    TS's "the user did not answer" sentence, instead of the turn hanging forever on a question nobody looks at, nor a fabricated preference.
//! 2. **snooze is permanent**. After a snooze, even when the deadline has long passed, the interaction must still be pending
//!    and the turn must still not have settled; a real answer still goes back to the model normally.
//!
//! The timing thresholds are scaled by TS's identically named test hook (`nc_code_ENV=test` +
//! `nc_code_E2E_ASK_USER_QUESTION_CLOCK_SCALE`, applied only to the **subprocess** environment); the production values
//! 60s/300s are pinned by `interaction.rs`'s unit tests.

mod common;

use common::{
    command_envelope, deltas_from_frame, text_sse, tool_use_sse_with_id, Client, HostProcess,
    ScriptedResponder,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn ask_input() -> Value {
    json!({"questions": [
        {"question": "Which cache library should we use?", "header": "Library",
         "multiSelect": false,
         "options": [
            {"label": "swr", "description": "hooks-first"},
            {"label": "react-query", "description": "cache-first"}
        ]}
    ]})
}

fn temp_base(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("zcode-host-ask-auto-{name}-{}", std::process::id()));
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
    clock_scale: &str,
) -> (HostProcess, Client) {
    let mut process = HostProcess::spawn_with_env(
        workspace,
        data_dir,
        config_path,
        false,
        &[
            ("ZCODE_ENV", "test"),
            ("ZCODE_E2E_ASK_USER_QUESTION_CLOCK_SCALE", clock_scale),
        ],
    );
    let client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    (process, client)
}

fn open_session(client: &mut Client) -> String {
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
        json!({
            "sessionId": session_id,
            "connectionId": "conn-1",
            "clientMode": "desktop-continuous"
        }),
    );
    client.response(id);
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
    assert_eq!(
        client.response(id)["result"]["status"],
        json!("accepted"),
        "{command_id} must be admitted"
    );
}

/// Wait for a userInput interaction and return (interactionId, the whole pendingInteraction).
fn await_user_input(client: &mut Client, session_id: &str) -> (String, Value) {
    let sid = format!("conn-1:conversation/{session_id}");
    for _ in 0..1200 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        for delta in deltas_from_frame(&line) {
            let Some(items) = delta["patch"]["pendingInteractions"].as_array() else {
                continue;
            };
            let Some(item) = items
                .iter()
                .find(|item| item["kind"].as_str() == Some("userInput"))
            else {
                continue;
            };
            return (
                item["interactionId"]
                    .as_str()
                    .expect("interactionId")
                    .to_string(),
                item.clone(),
            );
        }
    }
    panic!("no userInput pendingInteraction was ever projected");
}

/// Wait until this interaction's state becomes `expected` (snoozed / gone).
fn await_interaction_state(
    client: &mut Client,
    session_id: &str,
    interaction_id: &str,
    expected: &str,
) -> Value {
    let sid = format!("conn-1:conversation/{session_id}");
    for _ in 0..1200 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        for delta in deltas_from_frame(&line) {
            let Some(items) = delta["patch"]["pendingInteractions"].as_array() else {
                continue;
            };
            let Some(item) = items
                .iter()
                .find(|item| item["interactionId"].as_str() == Some(interaction_id))
            else {
                continue;
            };
            if item["autoResolution"]["state"].as_str() == Some(expected) {
                return item.clone();
            }
        }
    }
    panic!("interaction {interaction_id} never reached autoResolution {expected}");
}

fn wait_turn_settled(client: &mut Client, session_id: &str, command_id: &str) {
    for _ in 0..2000 {
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
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("command {command_id} never settled");
}

fn command_settled(client: &mut Client, session_id: &str, command_id: &str) -> bool {
    let id = client.send(
        "v4/commands/query",
        json!({"commands": [{"sessionId": session_id, "commandId": command_id}]}),
    );
    let entry = client.response(id)["result"]["results"]
        .as_array()
        .and_then(|items| items.first())
        .cloned()
        .unwrap_or(Value::Null);
    entry["result"] != json!("unknown") && entry["result"] != Value::Null
}

fn escaped_in_json_body(text: &str) -> String {
    let rendered = json!(text).to_string();
    rendered[1..rendered.len() - 1].to_string()
}

#[test]
fn an_unanswered_question_auto_resolves_with_the_explicit_zero_answers() {
    // scale=60 → hiddenGrace 1s, deadline 5s. Leaving a wide enough gap is so that "opened", "advanced to
    // visibleCountdown" and "expired, closed" each become an observable projection — squeezing the deadline down to tens of
    // milliseconds would merge them into one delta batch, and the test would then measure batching speed rather than the state machine.
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse_with_id("toolu_ask", "AskUserQuestion", &ask_input()),
        text_sse("proceeded after the countdown"),
    ]);
    let base_dir = temp_base("deadline");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let (_process, mut client) = connect(&workspace, &data_dir, &config_path, "60");
    let session_id = open_session(&mut client);
    submit(
        &mut client,
        &session_id,
        "cmd-auto",
        "pick a library, nobody will answer",
    );

    // 1. Already carrying the countdown at registration (both hiddenGrace/visibleCountdown compute correctly, depending on tick speed),
    //    and all three timestamps are present — the client relies on them to render "how much time is left".
    let (interaction_id, item) = await_user_input(&mut client, &session_id);
    let auto = &item["autoResolution"];
    assert!(
        matches!(
            auto["state"].as_str(),
            Some("hiddenGrace") | Some("visibleCountdown")
        ),
        "state must be a countdown state, not snoozed: {item}"
    );
    assert!(auto["startedAt"].is_number(), "{item}");
    assert!(auto["visibleAt"].is_number(), "{item}");
    assert!(auto["deadlineAt"].is_number(), "{item}");

    // 2. At visibleAt it must advance the state to visibleCountdown and **re-project**: the client starts
    //    showing the remaining time precisely on this change (nothing is shown during hiddenGrace).
    await_interaction_state(
        &mut client,
        &session_id,
        &interaction_id,
        "visibleCountdown",
    );

    // 3. Nobody answers: the turn must settle (this is exactly the "wait forever" this test is meant to prevent).
    wait_turn_settled(&mut client, &session_id, "cmd-auto");

    // 4. What auto-closing hands the model is still TS's explicit zero-answer wording, not a fabricated preference.
    let bodies = provider.request_bodies();
    assert!(
        bodies.len() >= 2,
        "the turn must resume with a second request: {bodies:?}"
    );
    assert!(
        bodies[1].contains(&escaped_in_json_body(
            "The user did not provide answers to these questions. Continue using your best judgment; do not treat this as a rejection or invent a user preference."
        )),
        "auto-resolution must deliver the TS zero-answer wording:\n{}",
        bodies[1]
    );
    assert!(
        !bodies[1].contains("\"Which cache library should we use?\"="),
        "no answer may be invented for a question nobody answered:\n{}",
        bodies[1]
    );

    // 5. After closing, the interaction must disappear from the pending surface (otherwise the UI keeps a question it no longer waits on).
    let sid = format!("conn-1:conversation/{session_id}");
    for _ in 0..400 {
        let line = client.next_line();
        if line["params"]["frame"]["subscriptionId"].as_str() != Some(sid.as_str()) {
            continue;
        }
        for delta in deltas_from_frame(&line) {
            if let Some(items) = delta["patch"]["pendingInteractions"].as_array() {
                if items
                    .iter()
                    .all(|item| item["interactionId"].as_str() != Some(interaction_id.as_str()))
                {
                    return;
                }
            }
        }
    }
    panic!("the auto-resolved interaction was never cleared from pendingInteractions");
}

#[test]
fn snoozing_a_question_stops_the_countdown_for_good() {
    // scale=60 → hiddenGrace 1s, deadline 5s. After the snooze, wait past the 5s: the interaction must still be there
    // and the turn must still not have settled — this is the only credible way to test "snooze is permanent": really cross the deadline.
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse_with_id("toolu_ask", "AskUserQuestion", &ask_input()),
        text_sse("answered after a snooze"),
    ]);
    let base_dir = temp_base("snooze");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let (_process, mut client) = connect(&workspace, &data_dir, &config_path, "60");
    let session_id = open_session(&mut client);
    submit(
        &mut client,
        &session_id,
        "cmd-snooze",
        "pick one, I will answer later",
    );

    let (interaction_id, _) = await_user_input(&mut client, &session_id);
    let id = client.send(
        "v4/command",
        command_envelope(
            "snoozeInteractionAutoResolution",
            "cmd-snooze-cmd",
            Some(&session_id),
            json!({"interactionId": interaction_id}),
        ),
    );
    assert_eq!(
        client.response(id)["result"]["status"],
        json!("accepted"),
        "snooze must be admitted"
    );
    let snoozed = await_interaction_state(&mut client, &session_id, &interaction_id, "snoozed");
    assert!(
        snoozed["autoResolution"]["snoozedAt"].is_number(),
        "{snoozed}"
    );
    assert!(
        snoozed["autoResolution"].get("deadlineAt").is_none(),
        "snoozed shape carries startedAt/snoozedAt only (TS convertToSnoozed): {snoozed}"
    );

    // Cross the original deadline (deadline = startedAt + 5s), leaving 2s of slack for scheduling and the wait loop's 100ms checkpoints.
    std::thread::sleep(Duration::from_millis(7_000));
    assert!(
        !command_settled(&mut client, &session_id, "cmd-snooze"),
        "a snoozed question must never auto-resolve"
    );

    // A real answer still goes back to the model normally.
    let id = client.send(
        "v4/command",
        command_envelope(
            "resolveInteraction",
            "cmd-answer",
            Some(&session_id),
            json!({
                "interactionId": interaction_id,
                "answer": {"action": "accept", "content": {"answer": "swr"}}
            }),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));
    wait_turn_settled(&mut client, &session_id, "cmd-snooze");
    let bodies = provider.request_bodies();
    assert!(
        bodies
            .last()
            .is_some_and(|body| body.contains(&escaped_in_json_body(
                "\"Which cache library should we use?\"=\"swr\""
            ))),
        "the real answer must still reach the model:\n{bodies:?}"
    );
}
