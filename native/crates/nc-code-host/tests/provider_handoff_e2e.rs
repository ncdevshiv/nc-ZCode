//! Desktop provider handoff e2e: a `nc-code-host` spawned **without** `--provider-config`
//! resolves its provider from the provider-store file the desktop already injects into
//! every agent spawn (`nc_code_PERSONAL_PROVIDER_CONFIG_FILE`), then completes a real model
//! round against a scripted HTTP provider.
//!
//! This is the seam a default flip depends on: no TypeScript-side secret marshalling, no
//! second config format — the host reads the same store files the TS CLI reads.

mod common;

use common::{
    apply_to_rows, command_envelope, deltas_from_frame, text_sse, Client, HostProcess,
    ScriptedResponder,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn temp_base(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zcode-host-provider-handoff-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn the_desktop_provider_store_hands_a_model_to_the_native_host() {
    let (provider, port) = ScriptedResponder::spawn(vec![text_sse("handoff reply")]);
    let base = temp_base("flow");
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let store_path = base.join("provider_config.json");
    std::fs::write(
        &store_path,
        json!({
            "schemaVersion": 3,
            "config": {
                "providerOrder": ["personal:0"],
                "providerConfigRules": {
                    "providerRules": [{
                        "providerId": "personal:0",
                        "config": {
                            "access": {"type": "api-key", "apiKey": "test-key-not-a-real-credential"},
                            "api": {
                                "type": "anthropic-messages",
                                "baseUrl": format!("http://127.0.0.1:{port}")
                            }
                        }
                    }]
                },
                "modelConfigRules": {
                    "providerModelRules": [{
                        "providerId": "personal:0",
                        "modelId": "test-model",
                        "config": {"enabled": true}
                    }],
                    "manualProviderModelRules": []
                },
                "defaultModelSelection": {"providerId": "personal:0", "modelId": "test-model"}
            }
        })
        .to_string(),
    )
    .expect("provider store");
    let store = store_path.to_string_lossy().to_string();
    let mut process = HostProcess::spawn_without_provider_config(
        &workspace,
        &data_dir,
        false,
        &[("ZCODE_PERSONAL_PROVIDER_CONFIG_FILE", store.as_str())],
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
    let _ = client.next_line(); // initial snapshot
    let id = client.send(
        "v4/command",
        command_envelope(
            "sendText",
            "cmd-handoff",
            Some(&session_id),
            json!({"text": "say hello through the handoff"}),
        ),
    );
    assert_eq!(client.response(id)["result"]["status"], json!("accepted"));

    let started = Instant::now();
    let mut rows: Vec<Value> = Vec::new();
    let mut replied = false;
    while started.elapsed() < Duration::from_secs(30) {
        let line = client.next_line();
        apply_to_rows(&mut rows, &deltas_from_frame(&line));
        replied = rows.iter().any(|row| {
            row["kind"] == json!("assistantText")
                && row["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("handoff reply")
        });
        if replied {
            break;
        }
    }
    assert!(
        replied,
        "the host must complete a model round from the store handoff: {rows:?}"
    );
    let bodies = provider.request_bodies();
    assert!(
        !bodies.is_empty(),
        "the scripted provider must receive the request (no fake local reply)"
    );
    assert!(
        bodies.iter().any(|body| body.contains("test-model")),
        "the request must use the model selected in the provider store: {bodies:?}"
    );

    drop(provider);
    let _ = std::fs::remove_dir_all(&base);
}
