//! E2e of the v4 workflow read surface: a real run lands a journal that really has markdown artifacts and labeled reports,
//! and then the seven queries are driven through the real JSON-RPC dispatch (`HostServer::handle_request`) with field-by-field assertions.
//!
//! The real stack is the same as `workflow_tools_e2e.rs`: real host + real run service + real compile kernel + real sandbox +
//! a scripted provider (which only plays the model). When the prerequisites are missing, the test skips explicitly.
mod common;
use common::{text_sse, ScriptedResponder};
use nc_code_host::provider_config::ProviderConfig;
use nc_code_host::rpc::RpcRequest;
use nc_code_host::sessions::SessionSeed;
use nc_code_host::tools::workflow::authoring::CreateWorkflowTool;
use nc_code_host::tools::workflow::service_port::HostWorkflowPort;
use nc_code_host::tools::{AsyncTool, ToolContext};
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
        eprintln!("skipping workflow v4 e2e: node executable not found");
        return false;
    }
    let dist = nc_code_workflow::compile::default_dist_entry();
    if !dist.is_file() {
        eprintln!("skipping workflow v4 e2e: dynamic-workflow dist not found");
        return false;
    }
    true
}
/// A script that produces all of: a pre-seeded dashboard (chart + labeled report), a world read, and a content artifact (markdown).
const V4_SCRIPT: &str = r###"
artifact.chart("perf", { x: { field: "n" }, y: { field: "v" } });
report({ n: 1, v: 10 }, "perf");
const readme = await files.read("README.md");
const w = agent("worker", "You answer briefly.");
const reply = await w.ask("Say hi.");
report({ readmeBytes: readme.length, reply });
await artifact.markdown("summary", "## hi\n\nbody");
return { ok: true };
"###;
struct Harness {
    base: PathBuf,
    host: Arc<HostServer>,
    session_id: String,
    ctx: ToolContext,
    _responder: ScriptedResponder,
}
impl Harness {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let base =
            std::env::temp_dir().join(format!("zcode-wf-v4-{}-{unique}", std::process::id()));
        let workspace = base.join("workspace");
        let data_dir = base.join("data");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        std::fs::write(workspace.join("README.md"), "# demo\n\nreadme body\n").expect("readme");
        let (responder, port) = ScriptedResponder::spawn(vec![text_sse("hello from worker")]);
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
        // In-process direct call: the response goes back to the caller and does not enter the session FIFO (the real transport uses the FIFO, see handle_request).
        host.set_inline_responses(true);
        let session = host.open_session(SessionSeed::blank()).expect("session");
        let workflow_port = Arc::new(HostWorkflowPort::new(data_dir, workspace.clone()));
        workflow_port.attach(Arc::downgrade(&host));
        let service = host.workflow_service().expect("workflow service");
        workflow_port.set_service(service);
        let mut ctx = base_ctx(&workspace);
        ctx.session_id = session.id.clone();
        ctx.workflow = Some(workflow_port as nc_code_host::tools::workflow::WorkflowPort);
        Self {
            base,
            host,
            session_id: session.id.clone(),
            ctx,
            _responder: responder,
        }
    }
    fn service(&self) -> Arc<WorkflowService> {
        self.host.workflow_service().expect("workflow service")
    }
    fn workspace(&self) -> PathBuf {
        self.base.join("workspace")
    }
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, Value> {
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            method: method.to_string(),
            params,
        };
        let (line, _post) = self.host.handle_request(request).await;
        let line = line.expect("response line");
        let response: Value = serde_json::from_str(&line).expect("response json");
        if let Some(error) = response.get("error") {
            return Err(error.clone());
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
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
async fn create_finished_run(harness: &Harness) -> String {
    let output = CreateWorkflowTool
        .execute(
            &json!({"script": V4_SCRIPT, "name": "v4-run"}),
            &harness.ctx,
        )
        .await
        .expect("CreateWorkflow executes");
    let created: Value = serde_json::from_str(&output).expect("create json");
    let run_id = created["backgroundTaskId"]
        .as_str()
        .expect("run id")
        .to_string();
    let service = harness.service();
    for _ in 0..600 {
        if let Some(snapshot) = service.get_run(&run_id) {
            if snapshot.status != SnapshotStatus::Running {
                assert_eq!(snapshot.status, SnapshotStatus::Completed, "run completes");
                return run_id;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run never settled");
}
#[tokio::test]
async fn v4_workflow_queries_read_a_real_run() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new();
    let run_id = create_finished_run(&harness).await;
    let session = harness.session_id.clone();
    // ① workflowRuns
    let runs = harness
        .rpc(
            "v4/conversation/workflowRuns",
            json!({"sessionId": session}),
        )
        .await
        .expect("workflowRuns");
    let row = runs["runs"]
        .as_array()
        .expect("runs array")
        .iter()
        .find(|row| row["runId"] == json!(run_id))
        .unwrap_or_else(|| panic!("created run listed: {runs}"));
    assert_eq!(row["status"], json!("completed"));
    assert_eq!(row["resumable"], json!(false));
    assert!(row["updatedAt"].as_i64().unwrap_or(0) > 0);
    // ② workflowRunEvents (the ordering matches the engine's event vocabulary)
    let events = harness
        .rpc(
            "v4/conversation/workflowRunEvents",
            json!({"sessionId": session, "runId": run_id, "limit": 500}),
        )
        .await
        .expect("workflowRunEvents");
    let page = events["events"].as_array().expect("events array");
    assert!(!page.is_empty());
    assert_eq!(page[0]["type"], json!("run-started"));
    assert_eq!(page[0]["sequence"], json!(0));
    assert_eq!(events["hasMore"], json!(false));
    assert!(page
        .iter()
        .any(|event| event["type"] == json!("artifact-published")));
    assert!(page
        .iter()
        .any(|event| event["type"] == json!("run-settled")));
    // ③ workflowRunArtifacts: chart pre-seeded + summary markdown
    let artifacts = harness
        .rpc(
            "v4/conversation/workflowRunArtifacts",
            json!({"sessionId": session, "runId": run_id}),
        )
        .await
        .expect("workflowRunArtifacts");
    let list = artifacts["artifacts"].as_array().expect("artifacts array");
    let chart = list
        .iter()
        .find(|artifact| artifact["id"] == json!("perf"))
        .unwrap_or_else(|| panic!("chart listed: {artifacts}"));
    assert_eq!(chart["kind"], json!("chart"));
    assert_eq!(
        chart["itemCount"],
        json!(1),
        "one tagged report feeds the chart"
    );
    let summary = list
        .iter()
        .find(|artifact| artifact["id"] == json!("summary"))
        .unwrap_or_else(|| panic!("summary listed: {artifacts}"));
    assert_eq!(summary["kind"], json!("markdown"));
    assert_eq!(summary["versions"].as_array().expect("versions").len(), 1);
    assert_eq!(
        summary["versions"][0]["contentType"],
        json!("text/markdown")
    );
    // ④ workflowRunArtifactData: the entry page of a labeled report (the sequence can be passed back as a cursor)
    let data = harness
        .rpc(
            "v4/conversation/workflowRunArtifactData",
            json!({"sessionId": session, "runId": run_id, "artifactId": "perf"}),
        )
        .await
        .expect("workflowRunArtifactData");
    let items = data["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "one tagged report: {data}");
    assert_eq!(items[0]["item"]["n"], json!(1));
    assert_eq!(items[0]["siteId"], json!("report#1"));
    let cursor = items[0]["sequence"].as_u64().expect("cursor");
    let after = harness
        .rpc(
            "v4/conversation/workflowRunArtifactData",
            json!({"sessionId": session, "runId": run_id, "artifactId": "perf", "afterSequence": cursor}),
        )
        .await
        .expect("artifactData after cursor");
    assert_eq!(
        after["items"],
        json!([]),
        "cursor advances past the only item"
    );
    // ⑤ workflowRunArtifactRead: chunked reading of the real markdown bytes
    let full = harness
        .rpc(
            "v4/conversation/workflowRunArtifactRead",
            json!({"sessionId": session, "runId": run_id, "artifactId": "summary", "version": 1, "offset": 0, "limit": 4096}),
        )
        .await
        .expect("workflowRunArtifactRead");
    assert_eq!(full["mediaType"], json!("text/markdown"));
    assert_eq!(full["nextOffset"], Value::Null);
    let bytes = decode_base64(full["dataBase64"].as_str().expect("base64"));
    let text = String::from_utf8(bytes).expect("utf8");
    assert_eq!(text, "## hi\n\nbody");
    assert_eq!(full["totalBytes"].as_u64().unwrap(), text.len() as u64);
    let first = harness
        .rpc(
            "v4/conversation/workflowRunArtifactRead",
            json!({"sessionId": session, "runId": run_id, "artifactId": "summary", "version": 1, "offset": 0, "limit": 4}),
        )
        .await
        .expect("chunked read");
    assert_eq!(first["nextOffset"], json!(4));
    assert_eq!(
        decode_base64(first["dataBase64"].as_str().expect("chunk")),
        b"## h"
    );
    // ⑥ workflowRunWorkspace: the world-read row + the required timestamps
    let workspace = harness
        .rpc(
            "v4/conversation/workflowRunWorkspace",
            json!({"sessionId": session, "runId": run_id}),
        )
        .await
        .expect("workflowRunWorkspace");
    let nodes = workspace["nodes"].as_array().expect("nodes array");
    let read = nodes
        .iter()
        .find(|node| node["kind"] == json!("world-read"))
        .unwrap_or_else(|| panic!("world-read row present: {workspace}"));
    assert_eq!(read["op"], json!("read"));
    assert_eq!(read["args"], json!(["README.md"]));
    assert_eq!(read["status"], json!("completed"));
    assert!(read["summary"]["resultBytes"].as_u64().unwrap() > 0);
    assert!(
        read["createdAt"].as_i64().unwrap() > 0,
        "SQL timestamp present"
    );
    assert!(read["updatedAt"].as_i64().unwrap() > 0);
    // ⑦ workflowRunNodeResult: a bounded read-back of the body
    let ordinal = read["ordinal"].as_u64().expect("ordinal");
    let node_result = harness
        .rpc(
            "v4/conversation/workflowRunNodeResult",
            json!({"sessionId": session, "runId": run_id, "siteId": "world-read#1", "ordinal": ordinal}),
        )
        .await
        .expect("workflowRunNodeResult");
    assert_eq!(node_result["status"], json!("completed"));
    assert_eq!(node_result["truncated"], json!(false));
    let body = node_result["result"].as_str().expect("result text");
    assert!(body.contains("readme body"), "real readme bytes: {body}");
    // totalBytes is the byte count **after serialization** (JSON quotes and escapes count in), the same measure as the TS read surface.
    let serialized = serde_json::to_string(&serde_json::json!(body)).expect("serialize");
    assert_eq!(
        node_result["totalBytes"].as_u64().unwrap() as usize,
        serialized.len()
    );
    let tiny = harness
        .rpc(
            "v4/conversation/workflowRunNodeResult",
            json!({"sessionId": session, "runId": run_id, "siteId": "world-read#1", "ordinal": ordinal, "maxBytes": 8}),
        )
        .await
        .expect("bounded node result");
    assert_eq!(tiny["truncated"], json!(true));
    assert!(tiny["result"].as_str().unwrap().len() <= 8);
    // Authorization: another session cannot see this run.
    let foreign = harness
        .rpc(
            "v4/conversation/workflowRuns",
            json!({"sessionId": "someone-else"}),
        )
        .await
        .expect("foreign session runs");
    assert_eq!(foreign["runs"], json!([]));
    let foreign_events = harness
        .rpc(
            "v4/conversation/workflowRunEvents",
            json!({"sessionId": "someone-else", "runId": run_id}),
        )
        .await
        .expect("foreign session events");
    assert_eq!(foreign_events["events"], json!([]));
    harness.service().close().await;
}
/// A minimal base64 decoder (test side: standard alphabet, no data URI prefix).
fn decode_base64(text: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let value = TABLE
            .iter()
            .position(|candidate| *candidate == byte)
            .unwrap_or_else(|| panic!("invalid base64 byte: {byte}")) as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}
// ————————————————————————————— the four v4 workflow commands —————————————————————————————
use nc_code_host::tools::workflow::authoring::SaveWorkflowTool;
const HANGING: &str = "await new Promise(() => {}); return 1;";
async fn wait_status(
    service: &WorkflowService,
    run_id: &str,
    want: SnapshotStatus,
) -> nc_code_host::workflow_service::RunSnapshot {
    for _ in 0..400 {
        if let Some(snapshot) = service.get_run(run_id) {
            // The resumed engine wakes inside the command-execution task; before that, the row still shows the previous terminal state, so poll.
            if snapshot.status == want {
                return snapshot;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {run_id} never reached {want:?}");
}
/// For an admitted command the immediate ACK is only the admission receipt; the terminal ACK lands in the command ledger, and `v4/commands/query`
/// is its only replay surface (the same value a stdio client receives as final).
async fn final_ack(harness: &Harness, command_id: &str) -> Value {
    for _ in 0..400 {
        let response = harness
            .rpc(
                "v4/commands/query",
                json!({"commands": [{"sessionId": harness.session_id, "commandId": command_id}]}),
            )
            .await
            .expect("commands/query");
        let entry = response["results"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        let result = entry["result"].clone();
        if result != json!("unknown") && !result.is_null() {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("command {command_id} never settled");
}
async fn command(harness: &Harness, command_type: &str, command_id: &str, payload: Value) {
    let envelope =
        common::command_envelope(command_type, command_id, Some(&harness.session_id), payload);
    harness
        .rpc("v4/command", envelope)
        .await
        .unwrap_or_else(|error| panic!("{command_type} command rejected: {error}"));
}
#[tokio::test]
async fn v4_workflow_commands_drive_the_run_service() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new();
    let service = harness.service();
    // ① cancelBackgroundWork: cancel a run that is really in flight (the engine writes a stopped(user) row).
    let created = CreateWorkflowTool
        .execute(
            &json!({"script": HANGING, "name": "cancel-me"}),
            &harness.ctx,
        )
        .await
        .expect("CreateWorkflow");
    let created_json: Value = serde_json::from_str(&created).expect("create json");
    let run_id = created_json["backgroundTaskId"]
        .as_str()
        .expect("run id")
        .to_string();
    wait_status(&service, &run_id, SnapshotStatus::Running).await;
    command(
        &harness,
        "cancelBackgroundWork",
        "cmd-cancel",
        json!({"workId": run_id}),
    )
    .await;
    let stopped = wait_status(&service, &run_id, SnapshotStatus::Cancelled).await;
    assert_eq!(
        stopped.run_status,
        Some(nc_code_workflow::types::RunStatus::Stopped)
    );
    // ② resumeWorkflowRun: bring the same runId back up.
    command(
        &harness,
        "resumeWorkflowRun",
        "cmd-resume",
        json!({"runId": run_id}),
    )
    .await;
    wait_status(&service, &run_id, SnapshotStatus::Running).await;
    command(
        &harness,
        "cancelBackgroundWork",
        "cmd-cancel-2",
        json!({"workId": run_id}),
    )
    .await;
    wait_status(&service, &run_id, SnapshotStatus::Cancelled).await;
    // ③ amendWorkflowRunSettings: revise into a successor run (superseding the predecessor).
    let predecessor = CreateWorkflowTool
        .execute(
            &json!({"script": HANGING, "name": "amend-predecessor"}),
            &harness.ctx,
        )
        .await
        .expect("CreateWorkflow");
    let predecessor_json: Value = serde_json::from_str(&predecessor).expect("create json");
    let predecessor_id = predecessor_json["backgroundTaskId"]
        .as_str()
        .expect("predecessor id")
        .to_string();
    wait_status(&service, &predecessor_id, SnapshotStatus::Running).await;
    command(
        &harness,
        "amendWorkflowRunSettings",
        "cmd-amend",
        json!({"runId": predecessor_id, "script": format!("phase(\"settings\");\n{HANGING}")}),
    )
    .await;
    // The predecessor is superseded; the successor comes up (the same hanging script, so it never finishes either — cancel the successor to wrap up).
    let predecessor_done = wait_status(&service, &predecessor_id, SnapshotStatus::Cancelled).await;
    assert_eq!(
        predecessor_done.run_status,
        Some(nc_code_workflow::types::RunStatus::Stopped)
    );
    let successor_id = predecessor_done
        .superseded_by
        .clone()
        .expect("successor id recorded on the predecessor");
    wait_status(&service, &successor_id, SnapshotStatus::Running).await;
    // The revision path also takes the stage table from the new script's analysis graph (the revision reuses the predecessor's inputId, yet the track draws the new script's stations).
    let successor_launch = service
        .list_events(&successor_id, None, Some(1000))
        .events
        .into_iter()
        .find(|stored| stored.event.type_name() == "run-launched")
        .map(|stored| serde_json::to_value(&stored.event).expect("serialize event"))
        .expect("successor run-launched");
    assert_eq!(
        successor_launch["phaseNames"],
        json!(["settings"]),
        "{successor_launch}"
    );
    command(
        &harness,
        "cancelBackgroundWork",
        "cmd-cancel-3",
        json!({"workId": successor_id}),
    )
    .await;
    wait_status(&service, &successor_id, SnapshotStatus::Cancelled).await;
    // ④ startSavedWorkflow: resolve the project-profile definition → analyze (stage table) → draft copy → submit with an anchor → the run completes.
    SaveWorkflowTool
        .execute(
            &json!({
                "name": "readme-bytes",
                "description": "Counts the README bytes",
                "scope": "project",
                "script": "phase(\"plan\");\nconst readme = await files.read(\"README.md\");\nphase(\"execute\");\nreturn { bytes: readme.length };",
            }),
            &harness.ctx,
        )
        .await
        .expect("SaveWorkflow");
    let saved = nc_code_host::workflow_service::saved_workflows::resolve_saved_workflow(
        &harness.workspace(),
        "readme-bytes",
        Some(nc_code_host::workflow_service::saved_workflows::SavedWorkflowScope::Project),
        None,
    );
    let nc_code_host::workflow_service::saved_workflows::SavedWorkflowResolveResult::Ok(saved) =
        saved
    else {
        panic!("saved definition must resolve");
    };
    let envelope = common::command_envelope(
        "startSavedWorkflow",
        "cmd-start-saved",
        Some(&harness.session_id),
        json!({"name": "readme-bytes"}),
    );
    let ack = harness
        .rpc("v4/command", envelope)
        .await
        .unwrap_or_else(|error| panic!("startSavedWorkflow rejected: {error}"));
    assert_eq!(ack["status"], json!("accepted"), "{ack}");
    let start_final = final_ack(&harness, "cmd-start-saved").await;
    assert_eq!(start_final["status"], json!("accepted"), "{start_final}");
    let launched_tool_call = start_final["result"]["toolCallId"]
        .as_str()
        .expect("start ACK carries the launch toolCallId")
        .to_string();
    // find the new run (the only non-cancelled one in the list)
    let mut saved_run = None;
    for _ in 0..200 {
        let runs = harness
            .rpc(
                "v4/conversation/workflowRuns",
                json!({"sessionId": harness.session_id}),
            )
            .await
            .expect("workflowRuns");
        if let Some(row) = runs["runs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| row["label"] == json!("readme-bytes"))
        {
            saved_run = row["runId"].as_str().map(str::to_string);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let saved_run = saved_run.expect("saved workflow run appears in the list");
    let done = wait_status(&service, &saved_run, SnapshotStatus::Completed).await;
    assert_eq!(
        done.output,
        Some(json!({"bytes": "# demo\n\nreadme body\n".len()}))
    );
    // `run-launched` is the only source for the sidebar track: the genuinely analyzed stage table, the launch anchor and the draft working copy
    // are all on this one event; a saved definition file is never submitted directly.
    let launched = service
        .list_events(&saved_run, None, Some(1000))
        .events
        .into_iter()
        .find(|stored| stored.event.type_name() == "run-launched")
        .map(|stored| serde_json::to_value(&stored.event).expect("serialize event"))
        .expect("run-launched event");
    assert_eq!(
        launched["phaseNames"],
        json!(["plan", "execute"]),
        "{launched}"
    );
    assert_eq!(launched["toolCallId"], json!(launched_tool_call));
    assert!(
        launched["inputId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "the direct launch mints its own anchor: {launched}"
    );
    let draft = launched["scriptPath"]
        .as_str()
        .expect("draft working copy path");
    assert_ne!(
        draft, saved.path,
        "the saved definition is never submitted in place"
    );
    assert_eq!(
        std::fs::read_to_string(draft).expect("draft readable"),
        saved.source,
        "the working copy is byte-identical to the saved definition"
    );
    // Argument validation happens before any persistence: an extra argument is a readable ACK failure, not a run doomed to fail.
    let envelope = common::command_envelope(
        "startSavedWorkflow",
        "cmd-start-bad-args",
        Some(&harness.session_id),
        json!({"name": "readme-bytes", "args": {"bogus": 1}}),
    );
    let ack = harness
        .rpc("v4/command", envelope)
        .await
        .expect("a validation failure is a fault ACK, not a transport error");
    assert_eq!(ack["status"], json!("accepted"), "{ack}");
    let bad_args = final_ack(&harness, "cmd-start-bad-args").await;
    assert_eq!(bad_args["status"], json!("failed"), "{bad_args}");
    assert!(
        bad_args["message"]
            .as_str()
            .is_some_and(|message| message.contains("not valid")),
        "{bad_args}"
    );
    // The controlOnly launch turn: a user-visible input row + a terminal turnHeader, and its metadata is what the GUI draws the run card from;
    // a direct launch has no tool row, so the graph and script can only be taken from this launch row.
    let view = harness
        .host
        .session_transcript_view(&harness.session_id)
        .expect("transcript view");
    let rows = view["rows"].as_array().expect("rows");
    let launch_input = rows
        .iter()
        .find(|row| {
            row["kind"] == json!("userInput")
                && row["origin"] == json!("workflowLaunch")
                && row["workflowLaunch"]["runId"] == json!(saved_run)
                && row["workflowLaunch"].get("amend").is_none()
        })
        .unwrap_or_else(|| panic!("no controlOnly launch input row: {rows:?}"));
    let text = launch_input["text"].as_str().unwrap_or_default();
    assert!(
        text.starts_with(
            "Started the saved workflow \"readme-bytes\" (project) from the workflows hub as run"
        ),
        "{text}"
    );
    let meta = &launch_input["workflowLaunch"];
    assert_eq!(meta["runId"], json!(saved_run));
    assert_eq!(meta["toolCallId"], json!(launched_tool_call));
    assert_eq!(meta["name"], json!("readme-bytes"));
    assert_eq!(meta["scope"], json!("project"));
    assert!(
        meta["script"]
            .as_str()
            .is_some_and(|script| script.contains("phase(\"plan\")")),
        "{meta}"
    );
    assert_eq!(meta["display"]["kind"], json!("create_workflow"), "{meta}");
    assert!(
        meta["display"]["causalityGraph"]["phases"]
            .as_array()
            .is_some_and(|phases| !phases.is_empty()),
        "the launch metadata carries the analysis display graph: {meta}"
    );
    let launch_header = rows
        .iter()
        .find(|row| row["kind"] == json!("turnHeader") && row["origin"] == json!("workflowLaunch"))
        .unwrap_or_else(|| panic!("no controlOnly launch turnHeader: {rows:?}"));
    assert_eq!(launch_header["state"], json!("completedSuccess"));
    assert_eq!(launch_header["executionKind"], json!("controlOnly"));
    assert_eq!(launch_header["workflowLaunch"]["runId"], json!(saved_run));
    assert!(
        !rows.iter().any(|row| {
            row["kind"] == json!("toolCall") && row["toolCallId"] == json!(launched_tool_call)
        }),
        "the direct launch must not invent a toolCall row: {rows:?}"
    );
    // The launch text entered the model history: this real user message is visible in the next model request's context.
    let bodies_before = harness._responder.request_bodies().len();
    harness._responder.push(text_sse("followed up"));
    let _ = harness
        .rpc(
            "v4/command",
            common::command_envelope(
                "sendText",
                "cmd-after-launch",
                Some(&harness.session_id),
                json!({"text": "anything new?"}),
            ),
        )
        .await;
    let mut history_body = String::new();
    for _ in 0..400 {
        let bodies = harness._responder.request_bodies();
        if bodies.len() > bodies_before {
            history_body = bodies.last().cloned().unwrap_or_default();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        history_body.contains("Started the saved workflow"),
        "the launch text must be part of the next model request: {history_body}"
    );
    service.close().await;
}
