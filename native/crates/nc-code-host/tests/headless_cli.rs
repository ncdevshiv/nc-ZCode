//! e2e for the headless entry point (`nc-code-host --prompt`): for the first time the native agent can be run
//! **outside the desktop**. All three assertions are properties users actually rely on:
//!
//! 1. **stdout carries only the answer**. This is the dividing line for "can it be piped": if any one of diagnostics, tool notifications, protocol frames
//!    leaks into stdout, `… | wc -l` is no longer the answer.
//! 2. **fail-closed when the provider is unavailable**: no model request is sent, exit non-zero, and say clearly what is missing.
//!    A silent "succeeded but no output" is the hardest failure to investigate.
//! 3. **headless's interaction semantics are still fail-closed**: with no subscriber able to answer, a file-writing tool
//!    is refused (not hung), and the answer still comes back.

mod common;

use common::{run_headless, text_sse, tool_use_sse, ScriptedResponder};
use serde_json::json;
use std::path::{Path, PathBuf};

fn temp_base(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("zcode-host-headless-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn layout(name: &str, port: u16) -> (PathBuf, PathBuf, PathBuf) {
    let base_dir = temp_base(name);
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = base_dir.join("provider.json");
    std::fs::write(
        &config_path,
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
    (workspace, data_dir, config_path)
}

#[test]
fn headless_prompt_prints_only_the_answer_and_exits_zero() {
    let (_provider, port) =
        ScriptedResponder::spawn(vec![text_sse("HEADLESS-ANSWER line one\nline two")]);
    let (workspace, data_dir, config_path) = layout("answer", port);

    let output = run_headless(
        &workspace,
        &data_dir,
        Some(&config_path),
        false,
        "say the magic words",
    );
    assert!(
        output.status.success(),
        "a successful turn must exit 0; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(
        stdout.trim_end(),
        "HEADLESS-ANSWER line one\nline two",
        "stdout must carry exactly the assistant text"
    );
    // Diagnostics go to stderr: the ready line, resumed-sessions etc. must not enter stdout.
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("ready"),
        "diagnostics belong to stderr: {stderr}"
    );
    assert!(
        !stdout.contains("zcode-host]") && !stdout.contains("{\"jsonrpc\""),
        "no diagnostics and no protocol frames may reach stdout: {stdout}"
    );
}

#[test]
fn headless_without_a_provider_fails_closed_and_says_why() {
    let base_dir = temp_base("no-provider");
    let workspace = base_dir.join("workspace");
    let data_dir = base_dir.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let missing: Option<&Path> = None;

    let output = run_headless(&workspace, &data_dir, missing, false, "anything");
    assert_ne!(
        output.status.code().unwrap_or(0),
        0,
        "no provider must not report success"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("needs a model") && stderr.contains("--provider-config"),
        "the failure must name what is missing: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "nothing may be printed as an answer: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn headless_declines_unapproved_writes_and_still_answers() {
    // The model first wants to write a file, then wrap up. In headless there is no subscriber able to answer, so permission must
    // **refuse rather than hang** — this test exists precisely to pin that distinction.
    let (provider, port) = ScriptedResponder::spawn(vec![
        tool_use_sse(
            "Write",
            &json!({"file_path": "sneaky.txt", "content": "written"}),
        ),
        text_sse("I could not write without approval"),
    ]);
    let (workspace, data_dir, config_path) = layout("declined", port);

    let output = run_headless(
        &workspace,
        &data_dir,
        Some(&config_path),
        false,
        "write a file for me",
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        stdout.contains("I could not write without approval"),
        "the answer must still come back. stdout={stdout:?} status={:?} stderr={:?}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !workspace.join("sneaky.txt").exists(),
        "an unapproved write must not happen just because a human is at a terminal"
    );
    let bodies = provider.request_bodies();
    assert_eq!(
        bodies.len(),
        2,
        "the denial must feed back into the conversation: {bodies:?}"
    );
    assert!(
        bodies[1].contains("no interactive client is attached")
            || bodies[1].contains("permissionDenied"),
        "the model must see why the tool failed:\n{}",
        bodies[1]
    );
}
