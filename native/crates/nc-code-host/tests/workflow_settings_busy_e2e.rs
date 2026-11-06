//! Settings revision on a busy session (in-process): `amendWorkflowRunSettings` arriving while
//! the `sendText` model turn is still in flight. TS's semantics are "settings commit immediately, the settings controlOnly turn is queued into
//! the runtime queue (priority next)"; this host relies on CommandInbox serial admission as the backstop. What is pinned here:
//! 1. the settings command must settle within bounded time (it must not hang with no output like the former combined long case did);
//! 2. the in-flight model turn completes normally by itself;
//! 3. the settings controlOnly turn lands in the row order after that turn.
mod common;
use nc_code_host::provider_config::ProviderConfig;
use nc_code_host::sessions::SessionSeed;
use nc_code_host::tools::workflow::authoring::SaveWorkflowTool;
use nc_code_host::tools::workflow::service_port::HostWorkflowPort;
use nc_code_host::tools::{AsyncTool, ToolContext};
use nc_code_host::workflow_service::saved_workflows::{
    resolve_saved_workflow, SavedWorkflowResolveResult, SavedWorkflowScope,
};
use nc_code_host::workflow_service::SnapshotStatus;
use nc_code_host::HostServer;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
const TURN_TEXT: &str = "busy-turn finished";
fn ctx(workspace: &std::path::Path) -> ToolContext {
    ToolContext {
        workspace_root: workspace.to_path_buf(),
        full_access: true,
        interactive: false,
        trace_id: "t".to_string(),
        turn_id: "turn".to_string(),
        session_id: "save-session".to_string(),
        model_ref: None,
        automation: None,
        browser: None,
        computer: None,
        offpeak: None,
        sessions: None,
        workflow: None,
        read_state: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        todos: nc_code_host::tools::todo::new_todo_store(),
        provider_config: None,
        tool_call_id: None,
        subagent: None,
        ask_user: None,
        plan_mode: false,
        plan: None,
        background: None,
    }
}
async fn dispatch(
    host: &Arc<HostServer>,
    session_id: &str,
    command_type: &str,
    command_id: &str,
    payload: Value,
) -> Value {
    let envelope = json!({
        "type": command_type,
        "commandId": command_id,
        "clientId": "e2e",
        "sessionId": session_id,
        "payload": payload,
        "issuedAt": 1_700_000_000_000.0,
    });
    let line =
        json!({"jsonrpc": "2.0", "id": 1, "method": "v4/command", "params": envelope}).to_string();
    let (response, _) = host.handle_line(&line).await;
    serde_json::from_str::<Value>(&response.expect("command response")).expect("json")
}
async fn final_ack(host: &Arc<HostServer>, session_id: &str, command_id: &str) -> Value {
    for _ in 0..2400 {
        let line = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "v4/commands/query",
            "params": {"commands": [{"sessionId": session_id, "commandId": command_id}]},
        })
        .to_string();
        let (response, _) = host.handle_line(&line).await;
        let value: Value = serde_json::from_str(&response.expect("query response")).expect("json");
        let result = value["result"]["results"][0]["result"].clone();
        if result != json!("unknown") && !result.is_null() {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("command {command_id} never settled (busy-session deadlock?)");
}
async fn wait_completed(host: &Arc<HostServer>, run_id: &str) {
    let service = host.workflow_service().expect("workflow service");
    for _ in 0..600 {
        if let Some(snapshot) = service.get_run(run_id) {
            if snapshot.status != SnapshotStatus::Running {
                assert_eq!(snapshot.status, SnapshotStatus::Completed);
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {run_id} never settled");
}
fn rows(host: &Arc<HostServer>, session_id: &str) -> Vec<Value> {
    host.session_transcript_view(session_id)
        .and_then(|view| view["rows"].as_array().cloned())
        .unwrap_or_default()
}
#[tokio::test(flavor = "multi_thread")]
async fn an_amend_while_a_model_turn_is_in_flight_settles_and_lands_after_it() {
    let base =
        std::env::temp_dir().join(format!("zcode-host-settings-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    std::fs::write(workspace.join("README.md"), "# busy settings\n").expect("readme");
    // The model turn is slow by 1.5 s: giving a deterministic window for "the command arrives mid-turn".
    let (responder, port) = common::ScriptedResponder::spawn_with_delay(
        vec![common::text_sse(TURN_TEXT)],
        Duration::from_millis(1_500),
    );
    let config_path = base.join("provider.json");
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
    let provider = ProviderConfig::load(&config_path).expect("provider config");
    let (host, _outbox) = HostServer::new(
        workspace.clone(),
        data_dir.clone(),
        Some(provider),
        true,
        vec![],
    )
    .expect("host");
    host.set_inline_responses(true);
    let session = host.open_session(SessionSeed::blank()).expect("session");
    let workflow_port = Arc::new(HostWorkflowPort::new(data_dir.clone(), workspace.clone()));
    workflow_port.attach(Arc::downgrade(&host));
    workflow_port.set_service(host.workflow_service().expect("workflow service"));
    let mut save_ctx = ctx(&workspace);
    save_ctx.session_id = session.id.clone();
    save_ctx.workflow = Some(workflow_port as nc_code_host::tools::workflow::WorkflowPort);
    SaveWorkflowTool
        .execute(
            &json!({
                "name": "busy-settings",
                "description": "reads the readme",
                "scope": "project",
                "script": "phase(\"plan\");\nconst readme = await files.read(\"README.md\");\nreturn { bytes: readme.length };",
            }),
            &save_ctx,
        )
        .await
        .expect("SaveWorkflow");
    let saved = resolve_saved_workflow(
        &workspace,
        "busy-settings",
        Some(SavedWorkflowScope::Project),
        None,
    );
    let SavedWorkflowResolveResult::Ok(saved) = saved else {
        panic!("saved definition must resolve");
    };
    let start = dispatch(
        &host,
        &session.id,
        "startSavedWorkflow",
        "cmd-start",
        json!({"name": "busy-settings"}),
    )
    .await;
    assert_eq!(start["result"]["status"], json!("accepted"), "{start}");
    let start_ack = final_ack(&host, &session.id, "cmd-start").await;
    let run_id = start_ack["result"]["runId"]
        .as_str()
        .expect("run id")
        .to_string();
    wait_completed(&host, &run_id).await;
    // The model turn takes off (the turn is already running when the admission ACK returns); send the settings revision right after.
    let turn = dispatch(
        &host,
        &session.id,
        "sendText",
        "cmd-busy-turn",
        json!({"text": "do something slow"}),
    )
    .await;
    assert_eq!(turn["result"]["status"], json!("accepted"), "{turn}");
    let started = Instant::now();
    let settings = dispatch(
        &host,
        &session.id,
        "amendWorkflowRunSettings",
        "cmd-busy-settings",
        json!({"runId": run_id, "script": saved.script, "subagentModel": "test-model"}),
    )
    .await;
    assert_eq!(
        settings["result"]["status"],
        json!("accepted"),
        "{settings}"
    );
    let settings_ack = final_ack(&host, &session.id, "cmd-busy-settings").await;
    assert_eq!(settings_ack["status"], json!("accepted"), "{settings_ack}");
    let successor_id = settings_ack["result"]["runId"]
        .as_str()
        .expect("successor id")
        .to_string();
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "the busy-session amend took {:?}",
        started.elapsed()
    );
    wait_completed(&host, &successor_id).await;
    // The in-flight turn completes by itself, and the settings turn lands after it.
    let mut all_rows = Vec::new();
    for _ in 0..1200 {
        all_rows = rows(&host, &session.id);
        if all_rows.iter().any(|row| {
            row["kind"] == json!("assistantText")
                && row["text"].as_str().unwrap_or_default().contains(TURN_TEXT)
        }) && all_rows.iter().any(|row| {
            row["kind"] == json!("userInput")
                && row["origin"] == json!("workflowLaunch")
                && row["workflowLaunch"]["amend"]["predecessorRunId"] == json!(run_id)
        }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let turn_header = all_rows
        .iter()
        .find(|row| {
            row["kind"] == json!("turnHeader") && row["sourceCommandId"] == json!("cmd-busy-turn")
        })
        .unwrap_or_else(|| panic!("no turnHeader for the in-flight turn: {all_rows:?}"))
        .clone();
    assert!(
        all_rows.iter().any(|row| {
            row["kind"] == json!("assistantText")
                && row["text"].as_str().unwrap_or_default().contains(TURN_TEXT)
        }),
        "the in-flight turn must complete: {all_rows:?}"
    );
    let settings_row = all_rows
        .iter()
        .find(|row| {
            row["kind"] == json!("userInput")
                && row["origin"] == json!("workflowLaunch")
                && row["workflowLaunch"]["amend"]["predecessorRunId"] == json!(run_id)
        })
        .unwrap_or_else(|| panic!("no settings controlOnly turn: {all_rows:?}"))
        .clone();
    assert!(
        settings_row["rowId"].as_u64() > turn_header["rowId"].as_u64(),
        "the settings turn must land after the in-flight turn: {settings_row} vs {turn_header}"
    );
    host.workflow_service().expect("service").close().await;
    drop(responder);
    let _ = std::fs::remove_dir_all(&base);
}
