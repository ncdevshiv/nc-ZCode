//! Set revised e2e (in-process): The controlOnly start wheel of `startSavedWorkflow` has been
//! `workflow_v4_e2e` coverage; here only three things about setting the path are nailed -
//! 1. The precursor script file is still the same byte at this moment ⇒ Use the same draft without rewriting;
//! 2. Set the difference between the `amend` metadata and model text in the settings controlOnly wheel;
//! 3. When nothing is changed (no script, no settings), ACK fails and no new run is created.
//!
//! No provider: The script under test only does world read, and run does not require a model.
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
use std::time::Duration;
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
) {
    let envelope = json!({
        "type": command_type,        "commandId": command_id,
        "clientId": "e2e",
        "sessionId": session_id,
        "payload": payload,
        "issuedAt": 1_700_000_000_000.0,
    });
    let line =
        json!({"jsonrpc": "2.0", "id": 1, "method": "v4/command", "params": envelope}).to_string();
    let (response, _) = host.handle_line(&line).await;
    let value: Value = serde_json::from_str(&response.expect("command response")).expect("json");
    assert_eq!(value["result"]["status"], json!("accepted"), "{value}");
}
async fn final_ack(host: &Arc<HostServer>, session_id: &str, command_id: &str) -> Value {
    for _ in 0..400 {
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
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("command {command_id} never settled");
}
async fn wait_completed(host: &Arc<HostServer>, run_id: &str) {
    let service = host.workflow_service().expect("workflow service");
    for _ in 0..600 {
        if let Some(snapshot) = service.get_run(run_id) {
            if snapshot.status != SnapshotStatus::Running {
                assert_eq!(
                    snapshot.status,
                    SnapshotStatus::Completed,
                    "run {run_id} must complete"
                );
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {run_id} never settled");
}
async fn launch_event(host: &Arc<HostServer>, run_id: &str) -> Value {
    host.workflow_service()
        .expect("workflow service")
        .list_events(run_id, None, Some(1000))
        .events
        .into_iter()
        .find(|stored| stored.event.type_name() == "run-launched")
        .map(|stored| serde_json::to_value(&stored.event).expect("serialize"))
        .expect("run-launched event")
}
#[tokio::test(flavor = "multi_thread")]
async fn settings_amend_reuses_the_script_file_and_records_the_change() {
    let base = std::env::temp_dir().join(format!("zcode-host-settings-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    std::fs::write(workspace.join("README.md"), "# settings e2e\n").expect("readme");
    let (host, _outbox) =
        HostServer::new(workspace.clone(), data_dir.clone(), None, true, vec![]).expect("host");
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
                "name": "settings-probe",
                "description": "reads the readme",
                "scope": "project",
                "script": "phase(\"plan\");\nconst readme = await files.read(\"README.md\");\nphase(\"execute\");\nreturn { bytes: readme.length };",
            }),
            &save_ctx,
        )
        .await
        .expect("SaveWorkflow");
    let saved = resolve_saved_workflow(
        &workspace,
        "settings-probe",
        Some(SavedWorkflowScope::Project),
        None,
    );
    let SavedWorkflowResolveResult::Ok(saved) = saved else {
        panic!("saved definition must resolve");
    };
    dispatch(
        &host,
        &session.id,
        "startSavedWorkflow",
        "cmd-start",
        json!({"name": "settings-probe"}),
    )
    .await;
    let start_ack = final_ack(&host, &session.id, "cmd-start").await;
    assert_eq!(start_ack["status"], json!("accepted"), "{start_ack}");
    let run_id = start_ack["result"]["runId"]
        .as_str()
        .expect("run id")
        .to_string();
    wait_completed(&host, &run_id).await;
    let launch = launch_event(&host, &run_id).await;
    let draft = launch["scriptPath"]
        .as_str()
        .expect("launch draft")
        .to_string();
    // Setting changes + same script ⇒ Keep the draft and note the model changes.
    dispatch(
        &host,
        &session.id,
        "amendWorkflowRunSettings",
        "cmd-settings",
        json!({"runId": run_id, "script": saved.script, "subagentModel": "test-model"}),
    )
    .await;
    let settings_ack = final_ack(&host, &session.id, "cmd-settings").await;
    assert_eq!(settings_ack["status"], json!("accepted"), "{settings_ack}");
    let successor_id = settings_ack["result"]["runId"]
        .as_str()
        .expect("successor id")
        .to_string();
    wait_completed(&host, &successor_id).await;
    let successor_launch = launch_event(&host, &successor_id).await;
    assert_eq!(
        successor_launch["scriptPath"],
        json!(draft),
        "an unchanged script must keep its file: {successor_launch}"
    );
    assert_eq!(successor_launch["subagentModel"], json!("test-model"));
    assert_eq!(
        successor_launch["phaseNames"],
        json!(["plan", "execute"]),
        "{successor_launch}"
    );
    let view = host
        .session_transcript_view(&session.id)
        .expect("transcript view");
    let rows = view["rows"].as_array().expect("rows");
    let settings_input = rows
        .iter()
        .find(|row| {
            row["kind"] == json!("userInput")
                && row["origin"] == json!("workflowLaunch")
                && row["workflowLaunch"]["amend"]["predecessorRunId"] == json!(run_id)
        })
        .unwrap_or_else(|| panic!("no settings controlOnly turn: {rows:?}"));
    let amend = &settings_input["workflowLaunch"]["amend"];
    assert_eq!(amend["subagentModel"]["to"], json!("test-model"), "{amend}");
    assert!(amend["subagentModel"].get("from").is_none(), "{amend}");
    assert_eq!(
        amend["ceiling"],
        json!(host
            .workflow_service()
            .expect("service")
            .concurrency_ceiling()),
        "{amend}"
    );
    assert_eq!(
        settings_input["workflowLaunch"]["runId"],
        json!(successor_id)
    );
    let text = settings_input["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("its subagents now run on test-model"),
        "{text}"
    );
    let settings_header = rows
        .iter()
        .find(|row| {
            row["kind"] == json!("turnHeader")
                && row["origin"] == json!("workflowLaunch")
                && row["workflowLaunch"]["runId"] == json!(successor_id)
        })
        .expect("settings turnHeader");
    assert_eq!(settings_header["executionKind"], json!("controlOnly"));
    // Empty revision: no script, no setting differences ⇒ failed ACK, and no new additions to the run list.
    let runs_before = host
        .workflow_service()
        .expect("service")
        .list_runs_limited(Some(&session.id), Some(usize::MAX))
        .len();
    dispatch(
        &host,
        &session.id,
        "amendWorkflowRunSettings",
        "cmd-noop",
        json!({"runId": successor_id}),
    )
    .await;
    let noop = final_ack(&host, &session.id, "cmd-noop").await;
    assert_eq!(noop["status"], json!("failed"), "{noop}");
    assert!(
        noop["message"]
            .as_str()
            .is_some_and(|message| message.contains("Nothing changed")),
        "{noop}"
    );
    assert_eq!(
        host.workflow_service()
            .expect("service")
            .list_runs_limited(Some(&session.id), Some(usize::MAX))
            .len(),
        runs_before,
        "a no-op amend must not create a run"
    );
    host.workflow_service().expect("service").close().await;
    let _ = std::fs::remove_dir_all(&base);
}
