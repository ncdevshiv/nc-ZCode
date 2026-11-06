//! E2e for the ten model-facing dynamic-workflow tools: the **real stack** (no doubles).
//!
//! A real `HostServer` (including the single run service + a real SQLite journal + a real compile kernel + a real sandbox subprocess),
//! the real production port `HostWorkflowPort` (the same implementation the tools get inside a turn), and real tool handlers. Only the
//! provider is scripted — it only plays the model, replacing none of the host's layers; the tools' **permission gate** is not on this path (handler
//! level e2e), and the gate itself is covered by the registry/metadata gate and the host's existing tests.
//!
//! When the prerequisites (node / the dynamic-workflow dist) are missing, the test skips explicitly and prints the reason.
mod common;
use common::{text_sse, tool_use_sse, ScriptedResponder};
use nc_code_host::provider_config::ProviderConfig;
use nc_code_host::sessions::SessionSeed;
use nc_code_host::tools::workflow::authoring::{
    AmendWorkflowTool, CreateWorkflowTool, EvalWorkflowSnippetTool, SaveWorkflowTool,
};
use nc_code_host::tools::workflow::runs::{
    GetWorkflowRunTool, ListSavedWorkflowsTool, ListWorkflowRunsTool, ResolveWorkflowQuestionTool,
    ResumeWorkflowRunTool,
};
use nc_code_host::tools::workflow::service_port::HostWorkflowPort;
use nc_code_host::tools::{AsyncTool, Tool, ToolContext};
use nc_code_host::workflow_service::{SnapshotStatus, WorkflowService};
use nc_code_host::HostServer;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
fn stack_available() -> bool {
    let node = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !node {
        eprintln!("skipping workflow tools e2e: node executable not found (set nc_code_NODE_PATH)");
        return false;
    }
    let dist = nc_code_workflow::compile::default_dist_entry();
    if !dist.is_file() {
        eprintln!(
            "skipping workflow tools e2e: dynamic-workflow dist not found at {} (build apps/nc-code-cli/packages/dynamic-workflow)",
            dist.display()
        );
        return false;
    }
    true
}
fn fixture_script() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../nc-code-workflow/tests/fixtures/workflow_linear.ts"
    ))
    .expect("workflow_linear.ts fixture")
}
/// A script that hangs forever: the run stays in running, giving amend / resume a real predecessor.
const HANGING_SCRIPT: &str = "await new Promise(() => {}); return 1;";
struct Harness {
    base: PathBuf,
    workspace: PathBuf,
    host: Arc<HostServer>,
    ctx: ToolContext,
    // Keep-alive: the responder's script handle is held by it.
    _responder: ScriptedResponder,
}
impl Harness {
    fn new(tag: &str, script: Vec<String>) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!(
            "zcode-wf-tools-{tag}-{}-{unique}",
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
        // Production port wiring (the same set as inside HostServer::new: build a new port + attach + inject the same service).
        let workflow_port = Arc::new(HostWorkflowPort::new(data_dir.clone(), workspace.clone()));
        workflow_port.attach(Arc::downgrade(&host));
        let service = host.workflow_service().expect("workflow service wired");
        workflow_port.set_service(Arc::clone(&service));
        let mut ctx = base_ctx(&workspace);
        ctx.session_id = session.id.clone();
        ctx.workflow = Some(workflow_port as nc_code_host::tools::workflow::WorkflowPort);
        let _ = data_dir;
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
    async fn close(&self) {
        self.service().close().await;
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
/// The actor turn's script: first hand back the typed result, then a pure-text turn that closes this ask's exchange
/// (otherwise the scripted provider would keep emitting the same submit forever and the turn would never end).
/// Look only at the **project profile** entries: the list also reports global definitions under the real home directory, and a test must not assume empty globals.
fn project_entries(value: &Value) -> Vec<Value> {
    value["workflows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|row| row["scope"] == json!("project"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}
fn submit_plan_script() -> Vec<String> {
    vec![
        tool_use_sse(
            "submit_result",
            &json!({"result": {"steps": ["s1", "s2", "s3"], "rationale": "e2e rationale"}}),
        ),
        text_sse("done"),
    ]
}
// ————————————————————————————— CreateWorkflow + GetWorkflowRun —————————————————————————————
#[tokio::test]
async fn create_workflow_starts_the_linear_fixture_and_get_workflow_run_reports_the_result() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new("create", submit_plan_script());
    let created = async_parse(CreateWorkflowTool.execute(
        &json!({"script": fixture_script(), "name": "linear-e2e"}),
        &harness.ctx,
    ))
    .await;
    assert_eq!(created["ok"], json!(true));
    assert_eq!(created["status"], json!("backgrounded"));
    // The display graph enters the tool row together with the output: real analyzer → bounded causalityGraph → create_workflow card.
    let graph = created
        .get("causalityGraph")
        .expect("create output must carry the display graph");
    assert!(
        graph["steps"]
            .as_array()
            .is_some_and(|steps| !steps.is_empty()),
        "the linear script has steps: {graph}"
    );
    let display = CreateWorkflowTool
        .result_display_for(&json!({}), &created.to_string())
        .expect("create display");
    assert_eq!(display["kind"], json!("create_workflow"));
    assert_eq!(display["causalityGraph"], *graph);
    let run_id = created["backgroundTaskId"]
        .as_str()
        .expect("run id")
        .to_string();
    let snapshot = wait_terminal(&harness.service(), &run_id).await;
    assert_eq!(
        snapshot.status,
        SnapshotStatus::Completed,
        "linear run completes"
    );
    assert_eq!(
        snapshot.output,
        Some(json!({"rationale": "e2e rationale", "steps": ["s1", "s2", "s3"]}))
    );
    // The stage table is taken from the real analysis graph and lands verbatim in `run-launched`: the sidebar mini-track reads only this event.
    let launched = harness
        .service()
        .list_events(&run_id, None, Some(1000))
        .events
        .into_iter()
        .find(|stored| stored.event.type_name() == "run-launched")
        .map(|stored| serde_json::to_value(&stored.event).expect("serialize event"))
        .expect("run-launched event");
    assert_eq!(launched["phaseNames"], json!(["plan", "execute"]));
    assert!(
        launched.get("phaseAlongside").is_none(),
        "the linear fixture has no parallel phases: {launched}"
    );
    // phases is read from the session's workflowRuns reducer state; the live fan-out is an async task, so wait for it to land in the state.
    let mut detail = Value::Null;
    for _ in 0..200 {
        detail = parse(GetWorkflowRunTool.execute(&json!({"run_id": run_id}), &harness.ctx));
        if detail
            .get("phases")
            .and_then(Value::as_array)
            .is_some_and(|phases| !phases.is_empty())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(detail["runId"], json!(run_id));
    let phases = detail["phases"].as_array().expect("phase roster");
    assert!(
        phases.iter().any(|phase| phase["name"] == json!("plan")),
        "declared plan phase: {phases:?}"
    );
    assert!(
        phases.iter().any(|phase| phase["name"] == json!("execute")),
        "entered execute phase: {phases:?}"
    );
    assert!(
        phases.iter().all(|phase| phase["state"] != json!("ahead")),
        "the linear fixture entered every phase: {phases:?}"
    );
    assert_eq!(detail["status"], json!("completed"));
    // `result` is the bounded textualization of the engine's settled value (consistent with TS's detail page); parse it back before comparing.
    let result_text = detail["result"].as_str().expect("result text");
    let result_value: Value = serde_json::from_str(result_text).expect("result JSON");
    assert_eq!(
        result_value,
        json!({"rationale": "e2e rationale", "steps": ["s1", "s2", "s3"]})
    );
    let artifacts = detail["artifacts"].as_array().expect("artifacts array");
    assert!(
        artifacts
            .iter()
            .any(|artifact| artifact["id"] == json!("summary")),
        "the script's markdown artifact is listed: {artifacts:?}"
    );
    let display = GetWorkflowRunTool
        .result_display_for(&json!({"run_id": run_id}), &detail.to_string())
        .expect("get_workflow_run display");
    assert_eq!(display["kind"], json!("get_workflow_run"));
    assert_eq!(display["runId"], json!(run_id));
    assert!(
        display["phases"]
            .as_array()
            .is_some_and(|phases| !phases.is_empty()),
        "the card carries the phase roster: {display}"
    );
    harness.close().await;
}
// ————————————————————————————— live workflowRuns projection —————————————————————————————
/// A real run's RunEvents must be reduced into the parent session's `workflowRuns` state through the host sink:
/// this is the data source of the v4 live panel, and one of the facts that must be pinned before the native host becomes the default path.
#[tokio::test]
async fn live_progress_projects_into_the_parent_session_workflow_runs_state() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new("projection", submit_plan_script());
    let created = async_parse(CreateWorkflowTool.execute(
        &json!({"script": fixture_script(), "name": "projection-e2e"}),
        &harness.ctx,
    ))
    .await;
    assert_eq!(created["ok"], json!(true));
    let run_id = created["backgroundTaskId"]
        .as_str()
        .expect("run id")
        .to_string();
    let snapshot = wait_terminal(&harness.service(), &run_id).await;
    assert_eq!(snapshot.status, SnapshotStatus::Completed);
    let session = harness
        .host
        .load_session(&harness.ctx.session_id)
        .expect("parent session");
    // The live fan-out runs in an independent forwarding task; wait for the terminal state to land in the state key (only a task-scheduling window away from settle).
    let mut state = None;
    for _ in 0..200 {
        state = session.workflow_runs_state();
        let settled = state
            .as_ref()
            .and_then(|state| state["runs"].as_array())
            .and_then(|runs| runs.iter().find(|run| run["runId"] == json!(run_id)))
            .and_then(|run| run["status"].as_str())
            == Some("completed");
        if settled {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = state.expect("the host sink must populate workflowRuns");
    let runs = state["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 1, "{state}");
    let run = &runs[0];
    assert_eq!(run["runId"], json!(run_id));
    assert_eq!(run["status"], json!("completed"), "{run}");
    // The stage table on `run-launched` enters the client state together with the projection (the sidebar mini-track's data source).
    assert_eq!(run["phaseNames"], json!(["plan", "execute"]), "{run}");
    let actors = run["actors"].as_array().expect("actors");
    assert!(!actors.is_empty(), "actor-created is projected: {run}");
    assert!(
        actors
            .iter()
            .any(|actor| actor["sessionId"].as_str().is_some_and(|id| !id.is_empty())),
        "the driver's actor session id reaches the client state: {run}"
    );
    let nodes = run["nodes"].as_array().expect("nodes");
    assert!(!nodes.is_empty(), "node lifecycle is projected: {run}");
    assert!(
        nodes.iter().any(|node| node["phase"] == json!("settled")),
        "settled node is projected: {run}"
    );
    assert!(
        run["lastEventSequence"].as_u64().unwrap_or(0) > 0,
        "the journal sequence reaches the live state: {run}"
    );
    harness.close().await;
}
// ————————————————————————————— cold rebuild —————————————————————————————
/// Cold-start rebuild: create a new host on the same data-dir (with no old in-memory state), and after loading the sessions
/// `ensure_workflow_runs` must replay the complete `workflowRuns` from the journal events — this is where the reconnect snapshot
/// gets its "no run lost" guarantee.
#[tokio::test]
async fn a_new_host_rebuilds_workflow_runs_from_the_journal() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new("cold-rebuild", submit_plan_script());
    let created = async_parse(CreateWorkflowTool.execute(
        &json!({"script": fixture_script(), "name": "cold-rebuild-e2e"}),
        &harness.ctx,
    ))
    .await;
    assert_eq!(created["ok"], json!(true));
    let run_id = created["backgroundTaskId"]
        .as_str()
        .expect("run id")
        .to_string();
    let snapshot = wait_terminal(&harness.service(), &run_id).await;
    assert_eq!(snapshot.status, SnapshotStatus::Completed);
    harness.close().await;
    let data_dir = harness.base.join("data");
    let (host2, _outbox2) =
        HostServer::new(harness.workspace.clone(), data_dir, None, true, vec![])
            .expect("second host over the same data dir");
    let session = host2
        .load_session(&harness.ctx.session_id)
        .expect("session loads from disk");
    host2.ensure_workflow_runs(&session);
    let state = session
        .workflow_runs_state()
        .expect("the journal fold rebuilds workflowRuns");
    let runs = state["runs"].as_array().expect("runs");
    let run = runs
        .iter()
        .find(|run| run["runId"] == json!(run_id))
        .unwrap_or_else(|| panic!("run {run_id} missing from rebuilt state: {state}"));
    assert_eq!(run["status"], json!("completed"), "{run}");
    assert!(
        run["actors"].as_array().is_some_and(|actors| actors
            .iter()
            .any(|actor| actor["sessionId"].as_str().is_some_and(|id| !id.is_empty()))),
        "the journal actor row supplies the session id on cold rebuild: {run}"
    );
    assert!(
        run["nodes"]
            .as_array()
            .is_some_and(|nodes| nodes.iter().any(|node| node["phase"] == json!("settled"))),
        "node lifecycle survives the rebuild: {run}"
    );
    if let Some(service) = host2.workflow_service() {
        service.close().await;
    }
}
// ————————————————————————————— AmendWorkflow —————————————————————————————
#[tokio::test]
async fn amend_supersedes_a_running_predecessor_and_the_successor_completes() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new("amend", submit_plan_script());
    // The predecessor: hangs inside a real sandbox (no model request), and the row stays at running.
    let created = async_parse(CreateWorkflowTool.execute(
        &json!({"script": HANGING_SCRIPT, "name": "hanging-predecessor"}),
        &harness.ctx,
    ))
    .await;
    let predecessor = created["backgroundTaskId"]
        .as_str()
        .expect("run id")
        .to_string();
    let service = harness.service();
    assert_eq!(
        wait_running(&service, &predecessor).await.status,
        SnapshotStatus::Running
    );
    let amended = async_parse(AmendWorkflowTool.execute(
        &json!({"run_id": predecessor, "script": fixture_script(), "name": "amended-successor"}),
        &harness.ctx,
    ))
    .await;
    let successor = amended["backgroundTaskId"]
        .as_str()
        .expect("successor id")
        .to_string();
    assert_ne!(successor, predecessor);
    let successor_snapshot = wait_terminal(&service, &successor).await;
    assert_eq!(successor_snapshot.status, SnapshotStatus::Completed);
    let predecessor_snapshot = wait_terminal(&service, &predecessor).await;
    assert_eq!(predecessor_snapshot.status, SnapshotStatus::Cancelled);
    assert_eq!(
        predecessor_snapshot.run_status,
        Some(nc_code_workflow::types::RunStatus::Stopped)
    );
    assert_eq!(
        predecessor_snapshot.superseded_by.as_deref(),
        Some(successor.as_str()),
        "lineage points at the successor"
    );
    harness.close().await;
}
async fn wait_running(
    service: &WorkflowService,
    run_id: &str,
) -> nc_code_host::workflow_service::RunSnapshot {
    for _ in 0..200 {
        if let Some(snapshot) = service.get_run(run_id) {
            if snapshot.status == SnapshotStatus::Running {
                return snapshot;
            }
            if snapshot.status != SnapshotStatus::Running && snapshot.run_status.is_some() {
                panic!(
                    "run {run_id} settled before it could be amended: {:?}",
                    snapshot.status
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {run_id} never reported running");
}
// ————————————————————————————— SaveWorkflow + ListSavedWorkflows —————————————————————————————
#[tokio::test]
async fn save_workflow_writes_the_definition_and_list_shows_it() {
    let harness = Harness::new("save", vec![]);
    let empty = parse(ListSavedWorkflowsTool.execute(&json!({}), &harness.ctx));
    assert!(
        project_entries(&empty).is_empty(),
        "a fresh project lists nothing: {empty}"
    );
    let saved = async_parse(SaveWorkflowTool.execute(
        &json!({
            "name": "review-pr",
            "description": "Reviews a PR",
            "scope": "project",
            "script": "return { ok: true };",
        }),
        &harness.ctx,
    ))
    .await;
    assert_eq!(saved["ok"], json!(true));
    let path = harness.workspace.join(".zcode/workflows/review-pr.dwf.ts");
    let text = std::fs::read_to_string(&path).expect("saved definition on disk");
    assert!(
        text.starts_with("/* nc-code-workflow"),
        "frontmatter block: {text}"
    );
    assert!(text.contains("description: Reviews a PR"));
    let listed = parse(ListSavedWorkflowsTool.execute(&json!({}), &harness.ctx));
    let project: Vec<Value> = listed["workflows"]
        .as_array()
        .expect("workflows array")
        .iter()
        .filter(|row| row["scope"] == json!("project"))
        .cloned()
        .collect();
    assert_eq!(
        project.len(),
        1,
        "exactly the saved project definition: {listed}"
    );
    assert_eq!(project[0]["name"], json!("review-pr"));
    assert_eq!(project[0]["description"], json!("Reviews a PR"));
    harness.close().await;
}
#[tokio::test]
async fn list_saved_workflows_starts_empty_and_lists_a_saved_definition() {
    let harness = Harness::new("list-saved", vec![]);
    let empty = parse(ListSavedWorkflowsTool.execute(&json!({}), &harness.ctx));
    assert!(project_entries(&empty).is_empty(), "fresh project: {empty}");
    async_parse(SaveWorkflowTool.execute(
        &json!({
            "name": "daily-digest",
            "description": "Summarises the day",
            "scope": "project",
            "script": "return 1;",
        }),
        &harness.ctx,
    ))
    .await;
    let listed = parse(ListSavedWorkflowsTool.execute(&json!({}), &harness.ctx));
    assert!(
        project_entries(&listed)
            .iter()
            .any(|entry| entry["name"] == json!("daily-digest")),
        "the saved definition is listed: {listed}"
    );
    harness.close().await;
}
// ————————————————————————————— EvalWorkflowSnippet —————————————————————————————
#[tokio::test]
async fn eval_workflow_snippet_reads_the_workspace_and_returns_its_value() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new("snippet", vec![]);
    let result = async_parse(EvalWorkflowSnippetTool.execute(
        &json!({
            "code": "const readme = await files.read(\"README.md\"); return { bytes: readme.length };"
        }),
        &harness.ctx,
    ))
    .await;
    assert_eq!(result["ok"], json!(true), "snippet result: {result}");
    let expected = std::fs::read_to_string(harness.workspace.join("README.md"))
        .expect("readme")
        .len();
    let response = result["response"].as_str().expect("response text");
    assert!(
        response.contains(&format!("\"bytes\": {expected}"))
            || response.contains(&format!("\"bytes\":{expected}")),
        "snippet read the REAL workspace README (expected {expected} bytes): {response}"
    );
    harness.close().await;
}
// ————————————————————————————— ListWorkflowRuns —————————————————————————————
#[tokio::test]
async fn list_workflow_runs_reports_the_created_run_and_the_status_filter() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new("list-runs", submit_plan_script());
    let created = async_parse(CreateWorkflowTool.execute(
        &json!({"script": fixture_script(), "name": "listed-run"}),
        &harness.ctx,
    ))
    .await;
    let run_id = created["backgroundTaskId"]
        .as_str()
        .expect("run id")
        .to_string();
    let service = harness.service();
    wait_terminal(&service, &run_id).await;
    let all = parse(ListWorkflowRunsTool.execute(&json!({}), &harness.ctx));
    let runs = all["runs"].as_array().expect("runs array");
    let row = runs
        .iter()
        .find(|row| row["runId"] == json!(run_id))
        .unwrap_or_else(|| panic!("created run listed: {all}"));
    assert_eq!(row["status"], json!("completed"));
    let completed_only =
        parse(ListWorkflowRunsTool.execute(&json!({"statuses": ["completed"]}), &harness.ctx));
    assert!(completed_only["runs"]
        .as_array()
        .expect("runs array")
        .iter()
        .any(|row| row["runId"] == json!(run_id)));
    let running_only =
        parse(ListWorkflowRunsTool.execute(&json!({"statuses": ["running"]}), &harness.ctx));
    assert!(
        !running_only["runs"]
            .as_array()
            .expect("runs array")
            .iter()
            .any(|row| row["runId"] == json!(run_id)),
        "status filter excludes the completed run: {running_only}"
    );
    harness.close().await;
}
// ————————————————————————————— ResumeWorkflowRun —————————————————————————————
#[tokio::test]
async fn resume_restarts_a_stopped_run_under_the_same_id() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new("resume", vec![]);
    let created = async_parse(CreateWorkflowTool.execute(
        &json!({"script": HANGING_SCRIPT, "name": "resume-me"}),
        &harness.ctx,
    ))
    .await;
    let run_id = created["backgroundTaskId"]
        .as_str()
        .expect("run id")
        .to_string();
    let service = harness.service();
    wait_running(&service, &run_id).await;
    // User cancellation: the row lands stopped(user), which is what the resume gate requires.
    assert!(service.cancel(
        &run_id,
        nc_code_host::workflow_service::CancelInitiator::User
    ));
    let stopped = wait_terminal(&service, &run_id).await;
    assert_eq!(stopped.status, SnapshotStatus::Cancelled);
    let resumed =
        async_parse(ResumeWorkflowRunTool.execute(&json!({"run_id": run_id}), &harness.ctx)).await;
    assert_eq!(resumed["ok"], json!(true), "resume result: {resumed}");
    assert_eq!(resumed["runId"], json!(run_id), "same run id");
    let running = wait_running(&service, &run_id).await;
    assert_eq!(running.run_id, run_id);
    // Wrap-up: stop again, so a hung engine is not still running when the harness is destroyed.
    service.cancel(
        &run_id,
        nc_code_host::workflow_service::CancelInitiator::Interrupted,
    );
    let _ = wait_terminal(&service, &run_id).await;
    harness.close().await;
}
// ————————————————————————————— ResolveWorkflowQuestion —————————————————————————————
#[tokio::test]
async fn resolve_workflow_question_unparks_the_escalated_actor() {
    if !stack_available() {
        return;
    }
    let script = "const w = agent(\"asker\", \"You ask when unsure.\");\n\
                  const answer = await w.ask(\"Ask the coordinator for the target environment, then report it.\");\n\
                  return { answer };";
    let harness = Harness::new(
        "question",
        vec![
            tool_use_sse(
                "escalate",
                &json!({"question": "Which environment should I target?"}),
            ),
            text_sse("REPORT: staging"),
        ],
    );
    let created = async_parse(CreateWorkflowTool.execute(
        &json!({"script": script, "name": "question-run"}),
        &harness.ctx,
    ))
    .await;
    let run_id = created["backgroundTaskId"]
        .as_str()
        .expect("run id")
        .to_string();
    let service = harness.service();
    // Wait until the actor parks on the escalation: GetWorkflowRun will bring out pendingQuestions.
    let mut question_id = None;
    for _ in 0..400 {
        let detail = parse(GetWorkflowRunTool.execute(&json!({"run_id": run_id}), &harness.ctx));
        if let Some(questions) = detail["pendingQuestions"].as_array() {
            if let Some(first) = questions.first() {
                question_id = first["qid"].as_str().map(str::to_string);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let question_id = question_id.expect("the escalated question is visible on GetWorkflowRun");
    let resolved = parse(ResolveWorkflowQuestionTool.execute(
        &json!({"question_id": question_id, "answer": "staging"}),
        &harness.ctx,
    ));
    assert_eq!(resolved["ok"], json!(true), "resolve result: {resolved}");
    // Answering the same qid a second time: the exit log separates "already answered" from "never existed" (the refusal wording differs).
    let again = ResolveWorkflowQuestionTool
        .execute(
            &json!({"question_id": question_id, "answer": "staging"}),
            &harness.ctx,
        )
        .expect_err("second resolution must be refused");
    assert!(
        again.message.contains("already answered"),
        "refusal names the outcome: {}",
        again.message
    );
    let snapshot = wait_terminal(&service, &run_id).await;
    assert_eq!(snapshot.status, SnapshotStatus::Completed);
    assert_eq!(snapshot.output, Some(json!({"answer": "REPORT: staging"})));
    harness.close().await;
}
