//! NWS on the authoring surface, against the real stack.
//!
//! These tests are the proof behind the tool description: a script written in the native
//! language launches from `CreateWorkflow`, runs to a real settlement and publishes real
//! artifacts **without any Node process**. That is what makes it safe to tell a model to write
//! NWS at all — the TS-stack guard (`stack_available`) is deliberately absent here, so the file
//! still runs on a machine with no node and no `dynamic-workflow` dist.
mod common;
use common::{text_sse, tool_use_sse, ScriptedResponder};
use nc_code_host::provider_config::ProviderConfig;
use nc_code_host::sessions::SessionSeed;
use nc_code_host::tools::workflow::authoring::{CreateWorkflowTool, SaveWorkflowTool};
use nc_code_host::tools::workflow::service_port::HostWorkflowPort;
use nc_code_host::tools::{AsyncTool, ToolContext};
use nc_code_host::workflow_service::{SnapshotStatus, WorkflowService};
use nc_code_host::HostServer;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
/// A script that uses only the native surface: a typed ask, a world read, a report, one
/// content artifact and a return value. No `args`, so the inline source needs no argument map.
const NWS_SCRIPT: &str = r#"schema Plan {
  steps: string[]
  rationale: string
}
workflow "nws-authoring-e2e" {
  phase "plan";
  let planner = agent("planner", "You produce short, concrete plans.");
  let plan: Plan = await planner.ask("Draft a plan for the demo repository");
  let readme = await files.read("README.md");
  report({ step: plan.steps[0], state: "planned" });
  await artifact.markdown("plan", plan.rationale);
  log("plan published");
  return { steps: plan.steps, rationale: plan.rationale, readmeLength: readme.length };
}
"#;
struct Harness {
    base: PathBuf,
    workspace: PathBuf,
    host: Arc<HostServer>,
    ctx: ToolContext,
    _responder: ScriptedResponder,
}
impl Harness {
    fn new(tag: &str, script: Vec<String>) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!(
            "zcode-nws-authoring-{tag}-{}-{unique}",
            std::process::id()
        ));
        let workspace = base.join("workspace");
        let data_dir = base.join("data");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        std::fs::write(workspace.join("README.md"), "# demo\n\nreadme body\n").expect("readme");
        let (responder, port) = ScriptedResponder::spawn(script);
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
        let session = host.open_session(SessionSeed::blank()).expect("session");
        let workflow_port = Arc::new(HostWorkflowPort::new(data_dir.clone(), workspace.clone()));
        workflow_port.attach(Arc::downgrade(&host));
        let service = host.workflow_service().expect("workflow service wired");
        workflow_port.set_service(service);
        let mut ctx = base_ctx(&workspace);
        ctx.session_id = session.id.clone();
        ctx.workflow = Some(workflow_port as nc_code_host::tools::workflow::WorkflowPort);
        Self {
            base,
            workspace,
            host,
            ctx,
            _responder: responder,
        }
    }
    fn service(&self) -> Arc<WorkflowService> {
        self.host.workflow_service().expect("workflow service")
    }

    /// The sandbox child writes its lowered module under this directory. Its absence is the
    /// no-Node proof: an NWS run never reaches the code that would create it.
    fn assert_no_node_artifacts(&self) {
        assert!(
            !self.workspace.join(".zcode/workflow-runs").exists(),
            "an NWS run must not produce a sandbox module"
        );
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}
fn base_ctx(root: &std::path::Path) -> ToolContext {
    ToolContext {
        workspace_root: root.to_path_buf(),
        full_access: true,
        interactive: false,
        trace_id: "t".to_string(),
        turn_id: "turn".to_string(),
        session_id: "session-1".to_string(),
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
fn parse(text: Result<String, nc_code_host::tools::ToolError>) -> Value {
    let text = text.unwrap_or_else(|error| panic!("tool failed: {}", error.message));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("tool output not JSON: {error}: {text}"))
}
async fn async_parse(
    fut: impl std::future::Future<Output = Result<String, nc_code_host::tools::ToolError>>,
) -> Value {
    parse(fut.await)
}
async fn wait_terminal(
    service: &WorkflowService,
    run_id: &str,
) -> nc_code_host::workflow_service::RunSnapshot {
    for _ in 0..600 {
        if let Some(snapshot) = service.get_run(run_id) {
            if snapshot.status != SnapshotStatus::Running {
                return snapshot;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run {run_id} never reached a terminal state");
}
/// The actor turn: hand back the typed result, then a text round that closes the exchange
/// (otherwise the scripted model re-emits the same submit forever).
fn submit_plan_script() -> Vec<String> {
    vec![
        tool_use_sse(
            "submit_result",
            &json!({"result": {"steps": ["s1", "s2", "s3"], "rationale": "e2e rationale"}}),
        ),
        text_sse("done"),
    ]
}
#[tokio::test]
async fn create_workflow_runs_a_native_script_and_never_needs_node() {
    let harness = Harness::new("create", submit_plan_script());
    let created = async_parse(CreateWorkflowTool.execute(
        &json!({"script": NWS_SCRIPT, "name": "nws-authoring"}),
        &harness.ctx,
    ))
    .await;
    assert_eq!(created["ok"], json!(true), "{created}");
    assert_eq!(created["status"], json!("backgrounded"));
    let graph = created
        .get("causalityGraph")
        .expect("the NWS projection feeds the same preview card the TypeScript path does");
    let ids: Vec<String> = graph["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| step["id"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(ids, vec!["ask#1", "world-read#1"], "{graph}");
    assert_eq!(graph["steps"][0]["lane"], "actor#1");
    assert_eq!(
        graph["sink"][0], "ask#1",
        "the returned value came from the ask"
    );
    let run_id = created["backgroundTaskId"]
        .as_str()
        .expect("backgroundTaskId")
        .to_string();
    let snapshot = wait_terminal(&harness.service(), &run_id).await;
    assert_eq!(snapshot.status, SnapshotStatus::Completed, "{snapshot:?}");
    let readme = std::fs::read_to_string(harness.workspace.join("README.md")).expect("readme");
    assert_eq!(
        snapshot.output,
        Some(json!({
            "steps": ["s1", "s2", "s3"],
            "rationale": "e2e rationale",
            "readmeLength": readme.encode_utf16().count(),
        })),
        "the script's return value is the run's output"
    );
    let artifact_ids: Vec<&str> = snapshot
        .artifacts
        .iter()
        .map(|artifact| artifact.id.as_str())
        .collect();
    assert_eq!(artifact_ids, vec!["plan"], "artifact.markdown published");
    assert_eq!(
        snapshot.reports.len(),
        1,
        "the report call journaled one finding"
    );
    harness.assert_no_node_artifacts();
    harness.service().close().await;
}
#[tokio::test]
async fn a_saved_definition_with_a_native_body_launches_through_the_same_path() {
    let harness = Harness::new("saved", submit_plan_script());
    let saved = async_parse(SaveWorkflowTool.execute(
        &json!({
            "name": "nws-saved",
            "description": "Plan the demo repository with one planner.",
            "scope": "project",
            "script": NWS_SCRIPT,
        }),
        &harness.ctx,
    ))
    .await;
    assert_eq!(saved["ok"], json!(true), "{saved}");
    let created = async_parse(CreateWorkflowTool.execute(
        &json!({"saved": {"name": "nws-saved"}, "name": "nws-saved-run"}),
        &harness.ctx,
    ))
    .await;
    assert_eq!(created["ok"], json!(true), "{created}");
    let run_id = created["backgroundTaskId"]
        .as_str()
        .expect("backgroundTaskId")
        .to_string();
    let snapshot = wait_terminal(&harness.service(), &run_id).await;
    assert_eq!(snapshot.status, SnapshotStatus::Completed, "{snapshot:?}");
    harness.assert_no_node_artifacts();
    harness.service().close().await;
}
