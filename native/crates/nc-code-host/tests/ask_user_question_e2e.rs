//! AskUserQuestion e2e: the model really waits for a human answer inside the turn, and the answer really lands back in the model context.
//!
//! Neither path is at the display layer:
//! 1. **with a subscriber**: the tool call suspends → the session projects `pendingInteraction(kind=userInput)`
//!    (the payload carries `questions`, only then can the UI render a choice question) → the client calls `resolveInteraction`
//!    with `answer.content.answers` → the tool result text appears in **the next model request body**.
//!    The assertion lands on the request body, because "the panel displayed a question" is not the same as "the model got the answer".
//! 2. **headless** (no subscriber): it must not suspend and must not invent user preferences — following TS's zero-answer semantics
//!    it tells the model plainly that "the user did not answer", and this turn keeps closing.

mod common;

use common::{
    command_envelope, deltas_from_frame, text_sse, tool_use_sse, Client, HostProcess,
    ScriptedResponder,
};
use serde_json::{json, Value};
use std::time::Duration;

fn temp_base(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("zcode-host-ask-user-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn write_provider_config(port: u16, path: &std::path::Path) {
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

fn ask_input() -> Value {
    json!({"questions": [
        {"question": "Which cache library should we use?", "header": "Library",
         "multiSelect": false,
         "options": [
            {"label": "swr", "description": "hooks-first", "preview": "<div>useSWR(key)</div>"},
            {"label": "react-query", "description": "cache-first"}
         ]},
        {"question": "Which themes should ship?", "header": "Themes", "multiSelect": true,
         "options": [
            {"label": "dark", "description": "default now"},
            {"label": "light", "description": "follow-up"}
         ]}
    ]})
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

/// Takes the interactionId of one userInput interaction out of the frame stream, validating the payload shape along the way.
fn await_user_input_interaction(client: &mut Client, session_id: &str) -> (String, Value) {
    let sid = format!("conn-1:conversation/{session_id}");
    for _ in 0..400 {
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
            let interaction_id = item["interactionId"]
                .as_str()
                .expect("interactionId")
                .to_string();
            return (interaction_id, item.clone());
        }
    }
    panic!("no userInput pendingInteraction was ever projected");
}

fn wait_turn_settled(client: &mut Client, session_id: &str, command_id: &str) {
    for _ in 0..200 {
        let id = client.send(
            "v4/commands/query",
            json!({"commands": [{"sessionId": session_id, "commandId": command_id}]}),
        );
        let response = client.response(id);
        let entry = response["result"]["results"]
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

/// The request body is JSON: the quotes in the tool result text are escaped, so assertions must compare against the same escaped form,
/// otherwise what is being tested is only "the escaping failed to apply".
fn escaped_in_json_body(text: &str) -> String {
    let rendered = json!(text).to_string();
    rendered[1..rendered.len() - 1].to_string()
}

#[test]
fn ask_user_question_waits_for_the_answer_and_returns_it_to_the_model() {
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse("AskUserQuestion", &ask_input()),
        text_sse("shipping swr with dark + light"),
    ]);
    let base_dir = temp_base("answered");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, false);
    let mut client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    let session_id = open_session(&mut client, true);

    submit(&mut client, &session_id, "cmd-ask", "pick the stack for me");
    let (interaction_id, item) = await_user_input_interaction(&mut client, &session_id);

    // The payload shape (specs/rust-native-crates.md S3 rule 1): for the client to render a choice question
    // it must receive questions; anchorRowId points at that toolCall row.
    let payload = &item["payload"];
    assert_eq!(payload["kind"], json!("userInput"));
    assert_eq!(payload["toolName"], json!("AskUserQuestion"));
    assert_eq!(payload["freeText"], json!(true));
    assert_eq!(
        payload["prompt"].as_str(),
        Some("AskUserQuestion pauses execution to collect answers from the user")
    );
    assert!(
        payload["toolCallId"].is_string(),
        "toolCallId lets the client bind the answer back: {payload}"
    );
    assert_eq!(
        payload["schema"]["toolName"],
        json!("AskUserQuestion"),
        "TS carries schema: {{toolName}}"
    );
    let questions = payload["questions"]
        .as_array()
        .expect("questions must be projected for the UI");
    assert_eq!(questions.len(), 2, "{questions:?}");
    assert_eq!(questions[0]["header"], json!("Library"));
    assert_eq!(
        questions[0]["options"][0]["value"],
        json!("swr"),
        "option value falls back to label (TS normalizeAskUserQuestionPayloadOption)"
    );
    assert_eq!(
        questions[0]["options"][0]["preview"],
        json!("<div>useSWR(key)</div>")
    );
    assert_eq!(
        questions[0].get("multiSelect"),
        None,
        "TS emits multiSelect only when it is true"
    );
    assert_eq!(questions[1]["multiSelect"], json!(true));
    assert!(
        item["anchorRowId"].is_number(),
        "the interaction must anchor the toolCall row: {item}"
    );

    // The answer: a single-question choice plus a multi-question combination (array), carrying an annotation.
    let id = client.send(
        "v4/command",
        command_envelope(
            "resolveInteraction",
            "cmd-resolve",
            Some(&session_id),
            json!({
                "interactionId": interaction_id,
                "answer": {
                    "content": {
                        "answers": {
                            "Which cache library should we use?": "swr",
                            "Which themes should ship?": ["dark", " ", "light"]
                        },
                        "annotations": {
                            "Which cache library should we use?": {"notes": "cheaper egress"}
                        }
                    }
                }
            }),
        ),
    );
    let response = client.response(id);
    assert_eq!(response["result"]["status"], json!("accepted"));

    wait_turn_settled(&mut client, &session_id, "cmd-ask");

    // The key assertion: the answers entered the **second model request**, rather than only updating the panel.
    let bodies = provider.request_bodies();
    assert_eq!(
        bodies.len(),
        2,
        "the turn must resume with a second model request after the answer"
    );
    let expected = concat!(
        "User has answered your questions: ",
        "\"Which cache library should we use?\"=\"swr\" user notes: cheaper egress, ",
        "\"Which themes should ship?\"=\"dark, light\". ",
        "You can now continue with the user's answers in mind."
    );
    assert!(
        bodies[1].contains(&escaped_in_json_body(expected)),
        "the second request body must carry the tool result text:\n{expected}\ngot:\n{}",
        bodies[1]
    );

    // After the answer, the interaction must disappear from the registry (otherwise the UI would keep showing an already-answered question).
    let mut cleared = false;
    for _ in 0..200 {
        let line = client.next_line();
        for delta in deltas_from_frame(&line) {
            if let Some(items) = delta["patch"]["pendingInteractions"].as_array() {
                assert!(
                    items
                        .iter()
                        .all(|item| item["interactionId"].as_str() != Some(interaction_id.as_str())),
                    "a resolved interaction must never be re-projected: {items:?}"
                );
                cleared = true;
            }
        }
        if cleared {
            break;
        }
    }
    assert!(
        cleared,
        "no pendingInteractions update arrived after resolve"
    );
}

#[test]
fn headless_ask_user_question_answers_with_zero_answers_instead_of_hanging() {
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse("AskUserQuestion", &ask_input()),
        text_sse("proceeded on its own judgment"),
    ]);
    let base_dir = temp_base("headless");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    write_provider_config(port, &config_path);

    let mut process = HostProcess::spawn(&workspace, &data_dir, &config_path, false);
    let mut client = Client {
        id_counter: 0,
        stdin: process.stdin(),
        stdout: process.stdout(),
        pending: std::collections::VecDeque::new(),
    };
    // No subscribe: there is nobody who can answer.
    let session_id = open_session(&mut client, false);

    submit(
        &mut client,
        &session_id,
        "cmd-headless",
        "pick the stack, nobody is watching",
    );
    wait_turn_settled(&mut client, &session_id, "cmd-headless");

    let bodies = provider.request_bodies();
    assert_eq!(
        bodies.len(),
        2,
        "the turn must continue, not stall: {bodies:?}"
    );
    assert!(
        bodies[1].contains(
            "The user did not provide answers to these questions. Continue using your best \
             judgment; do not treat this as a rejection or invent a user preference."
        ),
        "headless must tell the model there were no answers:\n{}",
        bodies[1]
    );
    assert!(
        !bodies[1].contains("\"Which cache library should we use?\"="),
        "no answer may be invented for a question nobody answered:\n{}",
        bodies[1]
    );
}
