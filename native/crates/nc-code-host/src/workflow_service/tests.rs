//! The unit + e2e tests of the workflow service.
//!
//! e2e runs on the **real stack** (no doubles): a real `HostServer` (in-process) + real transcript persistence + a real SQLite
//! journal + a real compile kernel (node + the dynamic-workflow dist) + a real sandbox child process + a scripted provider
//! (a local SSE server). The scripted provider only plays the model; it replaces no layer of the host.
//!
//! When a prerequisite (node / dist) is missing the test skips explicitly and prints the reason — what is missing is a build artifact, not behaviour.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use nc_code_workflow::compile::{AskSpecWire, CompiledScript};
use nc_code_workflow::types::{NodeKind, RunStatus, RunStopReason, WorkflowErrorCode};

use crate::provider_config::ProviderConfig;
use crate::sessions::SessionSeed;
use crate::workflow_service::service::AmendRequest;
use crate::workflow_service::{
    CancelInitiator, CompiledWorkflow, QuestionRefusal, ResolveQuestionResult, SnapshotStatus,
    SubmitRequest, WorkflowService, WorkflowServiceError,
};
use crate::HostServer;

// ————————————————————————————— the scripted provider —————————————————————————————

fn text_sse(text: &str) -> String {
    let text = serde_json::to_string(text).expect("escape text");
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{text}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn tool_use_sse(id: &str, name: &str, input: &Value) -> String {
    let id = serde_json::to_string(id).expect("escape id");
    let name = serde_json::to_string(name).expect("escape name");
    let input = serde_json::to_string(&input.to_string()).expect("escape input");
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"test-model\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":{id},\"name\":{name},\"input\":{{}}}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{input}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":5}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

/// An SSE server that replays by request order (out of range reuses the last entry). Returns the port and the request counter.
fn spawn_provider(script: Vec<String>) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let script = Arc::new(script);
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_for_thread = Arc::clone(&counter);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let script = Arc::clone(&script);
            let counter = Arc::clone(&counter_for_thread);
            thread::spawn(move || serve(&mut stream, &script, &counter));
        }
    });
    (port, counter)
}

fn serve(stream: &mut TcpStream, script: &Arc<Vec<String>>, counter: &Arc<AtomicUsize>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let trimmed = header.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let index = counter.fetch_add(1, Ordering::SeqCst);
        let payload = &script[index.min(script.len() - 1)];
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{}",
            payload.len(),
            payload
        );
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
    }
}

// ————————————————————————————— fixtures —————————————————————————————

fn fixture_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../nc-code-workflow/tests/fixtures/workflow_linear.ts"
    ))
}

fn linear_fixture_script() -> String {
    std::fs::read_to_string(fixture_path()).expect("workflow_linear.ts fixture")
}

/// The real stack's prerequisites: the node executable + the dynamic-workflow dist. Missing means skip (print the reason, never pass silently).
fn stack_available() -> bool {
    let node = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !node {
        eprintln!(
            "skipping workflow service e2e: node executable not found (set nc_code_NODE_PATH)"
        );
        return false;
    }
    let dist = nc_code_workflow::compile::default_dist_entry();
    if !dist.is_file() {
        eprintln!(
            "skipping workflow service e2e: dynamic-workflow dist not found at {} (build apps/nc-code-cli/packages/dynamic-workflow)",
            dist.display()
        );
        return false;
    }
    true
}

struct Harness {
    base: PathBuf,
    workspace: PathBuf,
    data_dir: PathBuf,
    host: Arc<HostServer>,
    session_id: String,
    requests: Arc<AtomicUsize>,
}

impl Harness {
    /// A real host + a real session + a real workspace (`README.md` is the world-read target of the linear fixture).
    fn new(tag: &str, script: Vec<String>) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!(
            "zcode-wf-service-{tag}-{}-{unique}",
            std::process::id()
        ));
        let workspace = base.join("workspace");
        let data_dir = base.join("data");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        std::fs::write(workspace.join("README.md"), "# demo\n\nreadme body\n").expect("readme");

        let (port, requests) = spawn_provider(script);
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
        let session = host
            .open_session(SessionSeed::blank())
            .expect("parent session");
        Self {
            base,
            workspace,
            data_dir,
            host,
            session_id: session.id.clone(),
            requests,
        }
    }

    fn workspace_path(&self) -> String {
        self.workspace.to_string_lossy().to_string()
    }

    fn service(&self) -> Arc<WorkflowService> {
        self.service_with_sink(None)
    }

    fn service_with_sink(
        &self,
        sink: Option<crate::workflow_service::EventSink>,
    ) -> Arc<WorkflowService> {
        WorkflowService::new_with_event_sink(
            Arc::clone(&self.host),
            &self.data_dir,
            self.workspace.clone(),
            sink,
        )
        .expect("workflow service")
    }

    fn submit_request(&self, name: &str) -> SubmitRequest {
        SubmitRequest {
            session_id: self.session_id.clone(),
            script_text: linear_fixture_script(),
            cwd: Some(self.workspace_path()),
            name: Some(name.to_string()),
            ..SubmitRequest::default()
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

async fn wait_settled(
    service: &Arc<WorkflowService>,
    run_id: &str,
) -> nc_code_workflow::types::RunSettlement {
    tokio::time::timeout(Duration::from_secs(120), service.wait_settlement(run_id))
        .await
        .unwrap_or_else(|_| panic!("run {run_id} did not settle within 120s"))
        .expect("run handle exists")
}

/// The event sequence: filters out the **purely observational** events (they arrive asynchronously through the driver's usage reporting
/// after settle, racing the script's next statement, so they take no part in the ordering assertion of "what the engine emitted").
const OBSERVATION_ONLY: &[&str] = &[
    "node-progress",
    "usage-updated",
    "log",
    "node-waiting",
    "node-executing",
    "concurrency-changed",
];

fn signature(service: &WorkflowService, run_id: &str) -> Vec<&'static str> {
    service
        .list_events(run_id, None, Some(1000))
        .events
        .iter()
        .map(|stored| stored.event.type_name())
        .filter(|name| !OBSERVATION_ONLY.contains(name))
        .collect()
}

// ————————————————————————————— e2e —————————————————————————————

#[tokio::test(flavor = "multi_thread")]
async fn a_linear_run_completes_and_every_read_surface_agrees() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new(
        "linear",
        vec![tool_use_sse(
            "toolu-submit-1",
            "submit_result",
            &json!({"result": {
                "steps": ["one", "two", "three"],
                "rationale": "because the docs are thin"
            }}),
        )],
    );
    let live_events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let collected = Arc::clone(&live_events);
    let service = harness.service_with_sink(Some(Arc::new(
        move |session_id: &str,
              run_id: &str,
              tool_call_id: Option<&str>,
              launch_input_id: Option<&str>,
              sequence: u64,
              event: Value| {
            collected.lock().expect("events lock").push(json!({
                "session": session_id,
                "runId": run_id,
                "toolCallId": tool_call_id,
                "launchInputId": launch_input_id,
                "sequence": sequence,
                "event": event,
            }));
        },
    )));

    let submitted = service
        .submit(harness.submit_request("linear"))
        .await
        .expect("submit");
    let run_id = submitted.run_id.clone();
    assert!(run_id.starts_with("dwfrun-"), "{run_id}");

    let settlement = wait_settled(&service, &run_id).await;
    assert_eq!(settlement.status, RunStatus::Completed, "{settlement:?}");
    assert_eq!(
        settlement.artifact,
        Some(json!({
            "rationale": "because the docs are thin",
            "steps": ["one", "two", "three"]
        }))
    );

    // —— the event sequence (which kinds the engine emitted, and in what order) ——
    assert_eq!(
        signature(&service, &run_id),
        vec![
            "run-started",
            // The anchor of the life that created the run (TS likewise carries launch inside submit, and the engine records run-launched from it).
            "run-launched",
            "phase-entered",
            "actor-created",
            "node-queued",
            "node-dispatched",
            "node-settled",
            "phase-entered",
            "node-queued",
            "node-dispatched",
            "node-settled",
            "report",
            "artifact-published",
            "run-settled",
        ],
        "journal event order is the contract"
    );

    // —— realtime fan-out: the sink installed at construction receives the same batch of events (the first one is run-started) ——
    let forwarded: Vec<Value> = live_events.lock().expect("events lock").clone();
    assert!(!forwarded.is_empty(), "the sink must see live events");
    assert!(forwarded
        .iter()
        .all(|entry| entry["runId"] == json!(run_id)));
    assert_eq!(forwarded[0]["event"]["type"], json!("run-started"));
    assert!(forwarded
        .iter()
        .any(|entry| entry["event"]["type"] == json!("run-settled")));

    // —— sink metadata + reducer end to end ——
    // The service must send the parent session, the journal sequence and the run metadata to the projection side together; a missing one of them
    // puts the live state out of step with the journal read surface (sequence) or lands it in the wrong session.
    assert!(
        forwarded
            .iter()
            .all(|entry| entry["session"] == json!(harness.session_id)),
        "every live event carries the parent session"
    );
    let sequences: Vec<u64> = forwarded
        .iter()
        .filter_map(|entry| entry["sequence"].as_u64())
        .collect();
    assert_eq!(
        sequences.len(),
        forwarded.len(),
        "every event carries a sequence"
    );
    // The synthesized actor-created (the patched-in session id) shares the sequence of the event that triggered it; otherwise it is strictly increasing.
    assert!(
        sequences.windows(2).all(|window| window[0] <= window[1]),
        "journal sequences never go backwards: {sequences:?}"
    );
    // The same mapping as the host sink: eventType = the RunEvent type, payload = the event minus type and
    // the __actorSessionId the driver appended; the latter goes into envelope.actorSessionId.
    let envelopes: Vec<Value> = forwarded
        .iter()
        .map(|entry| {
            let event = &entry["event"];
            let mut payload = event.as_object().cloned().unwrap_or_default();
            let event_type = payload.remove("type").unwrap_or(Value::Null);
            let actor_session_id = payload.remove("__actorSessionId");
            let mut envelope = json!({
                "runId": entry["runId"].clone(),
                "sequence": entry["sequence"].clone(),
                "eventType": event_type,
                "payload": payload,
            });
            if let Some(tool_call_id) = entry.get("toolCallId").filter(|value| !value.is_null()) {
                envelope["toolCallId"] = tool_call_id.clone();
            }
            if let Some(actor_session_id) = actor_session_id {
                envelope["actorSessionId"] = actor_session_id;
            }
            if let Some(launch_input_id) =
                entry.get("launchInputId").filter(|value| !value.is_null())
            {
                envelope["launchInputId"] = launch_input_id.clone();
            }
            envelope
        })
        .collect();
    let mut state: Option<Value> = None;
    for envelope in &envelopes {
        if let Some(next) =
            crate::workflow_projection::reduce_workflow_runs(state.as_ref(), envelope)
        {
            state = Some(next);
        }
    }
    let state = state.expect("the forwarded events reduce to a workflowRuns state");
    let runs = state["runs"].as_array().expect("runs");
    let run = runs
        .iter()
        .find(|run| run["runId"] == json!(run_id))
        .expect("run in workflowRuns state");
    assert_eq!(run["status"], json!("completed"), "{run}");
    assert!(
        run["actors"]
            .as_array()
            .is_some_and(|actors| !actors.is_empty()),
        "actor-created is projected: {run}"
    );
    assert!(
        run["nodes"]
            .as_array()
            .is_some_and(|nodes| !nodes.is_empty()),
        "node lifecycle is projected: {run}"
    );
    assert!(
        run["lastEventSequence"].as_u64().unwrap_or(0) > 0,
        "the journal sequence reaches the client state: {run}"
    );

    // —— the snapshot ——
    let snapshot = service.get_run(&run_id).expect("snapshot");
    assert_eq!(snapshot.status, SnapshotStatus::Completed);
    assert_eq!(snapshot.run_status, Some(RunStatus::Completed));
    assert!(!snapshot.resumable);
    assert_eq!(snapshot.max_concurrency, service.concurrency_ceiling());
    assert_eq!(snapshot.nodes.completed, 4, "{:?}", snapshot.nodes);
    assert_eq!(snapshot.reports.len(), 1);
    assert_eq!(snapshot.reports[0]["planSteps"], json!(3));
    assert_eq!(
        snapshot.output,
        Some(json!({
            "rationale": "because the docs are thin",
            "steps": ["one", "two", "three"]
        }))
    );
    assert!(snapshot.pending_questions.is_empty());
    // Usage is reported asynchronously by the driver after the turn ends (racing the script's next statement): wait for it to land before asserting.
    let mut spent = snapshot.spent_tokens;
    for _ in 0..200 {
        if spent > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        spent = service
            .get_run(&run_id)
            .map(|snapshot| snapshot.spent_tokens)
            .unwrap_or(0);
    }
    assert!(spent > 0, "usage is accounted");

    // —— enumeration ——
    let runs = service.list_runs(Some(&harness.session_id));
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, run_id);
    assert_eq!(runs[0].label, "linear");
    assert_eq!(runs[0].status, SnapshotStatus::Completed);
    assert!(!runs[0].resumable);
    assert!(!runs[0].live);
    assert!(runs[0].updated_at >= runs[0].created_at);

    // —— cold-start rebuild: the source of the snapshot after a host restart (journal events + the same reducer) ——
    let rebuilt = service
        .rebuild_workflow_runs(&harness.session_id)
        .expect("journal fold rebuilds workflowRuns");
    let rebuilt_runs = rebuilt["runs"].as_array().expect("rebuilt runs");
    let rebuilt_run = rebuilt_runs
        .iter()
        .find(|run| run["runId"] == json!(run_id))
        .expect("rebuilt run");
    assert_eq!(rebuilt_run["status"], json!("completed"), "{rebuilt_run}");
    assert_eq!(rebuilt_run["lastEventSequence"], json!(15), "{rebuilt_run}");
    assert!(
        rebuilt_run["actors"].as_array().is_some_and(|actors| actors
            .iter()
            .any(|actor| actor["sessionId"].as_str().is_some_and(|id| !id.is_empty()))),
        "the journal actor row supplies the session id on cold rebuild: {rebuilt_run}"
    );
    assert!(
        rebuilt_run["nodes"]
            .as_array()
            .is_some_and(|nodes| nodes.iter().any(|node| node["phase"] == json!("settled"))),
        "node lifecycle survives the rebuild: {rebuilt_run}"
    );

    // —— artifacts ——
    let artifacts = service.list_artifacts(&run_id);
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].id, "summary");
    assert_eq!(
        artifacts[0].kind,
        nc_code_workflow::types::ArtifactOp::Markdown
    );
    assert_eq!(artifacts[0].version, 1);
    assert_eq!(artifacts[0].versions.len(), 1);
    assert_eq!(artifacts[0].item_count, 0);
    let bytes = service
        .artifact_bytes(&run_id, "summary", 1)
        .expect("artifact bytes");
    let text = String::from_utf8(bytes.bytes).expect("utf8 artifact");
    assert!(
        text.starts_with("## Plan\n\n1. one\n2. two\n3. three"),
        "{text}"
    );
    assert_eq!(bytes.content_type.as_deref(), Some("text/markdown"));
    // Out of range / an unknown version / another run are one and the same business fact.
    assert!(service.artifact_bytes(&run_id, "summary", 2).is_none());
    assert!(service
        .artifact_bytes("dwfrun-other", "summary", 1)
        .is_none());

    // —— the workspace manifest ——
    let workspace = service
        .workflow_run_workspace(&run_id)
        .expect("workspace listing");
    assert_eq!(workspace.len(), 1);
    assert_eq!(workspace[0].kind, NodeKind::WorldRead);
    assert_eq!(
        workspace[0].input.as_ref().map(|input| input.op.as_str()),
        Some("read")
    );
    assert!(service.workflow_run_workspace("dwfrun-other").is_none());

    // —— node rows / the message boundary (the amend-resume truncation boundary) ——
    let planned = wait_for_message_boundary(&service, &run_id).await;
    assert!(planned > 0, "the ask must record its message boundary");

    // —— event pagination: the cursor is strictly greater, has_more comes from fetching one extra row ——
    let first = service.list_events(&run_id, None, Some(3));
    assert_eq!(first.events.len(), 3);
    assert!(first.has_more);
    let cursor = first.next_sequence.expect("cursor");
    let second = service.list_events(&run_id, Some(cursor), Some(3));
    assert!(second.events[0].sequence > cursor);
    let all = service.list_events(&run_id, None, Some(1000));
    assert!(!all.has_more);
    let sequences: Vec<u64> = all.events.iter().map(|event| event.sequence).collect();
    let mut sorted = sequences.clone();
    sorted.sort_unstable();
    assert_eq!(sequences, sorted, "sequences are monotonically increasing");
    // `run-settled` is the engine's closing and also the last event (the reporting after settlement emits no further events).
    assert_eq!(
        all.events.last().map(|event| event.event.type_name()),
        Some("run-settled")
    );

    // —— escalation Q&A: with nothing parked it is a structured unknown (not a panic, not a fake success) ——
    match service.resolve_question("dwfq-nope-1", "answer") {
        ResolveQuestionResult::Refused { reason, .. } => {
            assert_eq!(reason, QuestionRefusal::UnknownQuestion)
        }
        ResolveQuestionResult::Resolved { .. } => panic!("an unknown qid must not resolve"),
    }

    // —— cancelling an already settled run: there is nothing to abort ——
    assert!(!service.cancel(&run_id, CancelInitiator::User));
    assert_eq!(
        harness.requests.load(Ordering::SeqCst),
        1,
        "one ask = one model request"
    );

    service.close().await;
}

/// The phase table and alongside carried at submit time must land in `run-launched` verbatim: the sidebar mini track reads only this one event,
/// so the service layer swallowing them (even though the engine does not read them) means two tracks that cannot line up on screen.
#[tokio::test(flavor = "multi_thread")]
async fn run_launched_records_the_declared_phase_table_and_alongside() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new(
        "phase-table",
        vec![tool_use_sse(
            "toolu-submit-1",
            "submit_result",
            &json!({"result": {"steps": ["one"], "rationale": "phase table"}}),
        )],
    );
    let service = harness.service();
    let mut request = harness.submit_request("phases");
    request.phase_names = Some(vec![
        "plan".to_string(),
        "fan-a".to_string(),
        "fan-b".to_string(),
    ]);
    request.phase_alongside = Some(vec![vec![], vec![2], vec![1]]);
    let submitted = service.submit(request).await.expect("submit");
    wait_settled(&service, &submitted.run_id).await;

    let launched = service
        .list_events(&submitted.run_id, None, Some(1000))
        .events
        .into_iter()
        .find(|stored| stored.event.type_name() == "run-launched")
        .map(|stored| serde_json::to_value(&stored.event).expect("serialize event"))
        .expect("run-launched event");
    assert_eq!(launched["phaseNames"], json!(["plan", "fan-a", "fan-b"]));
    assert_eq!(launched["phaseAlongside"], json!([[], [2], [1]]));

    service.close().await;
}

/// Wait for the driver to write the ask's message boundary back onto the row (it writes it after the ask settles; the amend pre-flight reads it).
async fn wait_for_message_boundary(service: &Arc<WorkflowService>, run_id: &str) -> u64 {
    for _ in 0..200 {
        let nodes = service.run_nodes(run_id);
        if let Some(node) = nodes.iter().find(|node| node.kind == NodeKind::Ask) {
            if let Some(boundary) = node.message_boundary {
                return boundary;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the ask never recorded its message boundary");
}

#[tokio::test(flavor = "multi_thread")]
async fn amend_imports_the_predecessor_cache_without_calling_the_model_again() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new(
        "amend",
        vec![tool_use_sse(
            "toolu-submit-1",
            "submit_result",
            &json!({"result": {"steps": ["a", "b", "c"], "rationale": "first pass"}}),
        )],
    );
    let service = harness.service();
    let first = service
        .submit(harness.submit_request("linear"))
        .await
        .expect("submit");
    let settlement = wait_settled(&service, &first.run_id).await;
    assert_eq!(settlement.status, RunStatus::Completed);
    wait_for_message_boundary(&service, &first.run_id).await;
    assert_eq!(harness.requests.load(Ordering::SeqCst), 1);

    let amended = service
        .amend(AmendRequest {
            session_id: None,
            predecessor_run_id: first.run_id.clone(),
            script_text: linear_fixture_script(),
            cwd: Some(harness.workspace_path()),
            name: Some("linear".to_string()),
            max_concurrency: None,
            subagent_model: None,
            script_path: None,
            inherit_args: false,
            tool_call_id: None,
            phase_names: None,
            phase_alongside: None,
        })
        .await
        .expect("amend");
    assert!(
        amended.superseded_run_id.is_none(),
        "the predecessor had settled"
    );

    let settlement = wait_settled(&service, &amended.run_id).await;
    assert_eq!(
        settlement.status,
        RunStatus::Completed,
        "the amended run completes on the imported cache: {settlement:?}"
    );
    assert_eq!(
        harness.requests.load(Ordering::SeqCst),
        1,
        "imported asks must not call the model again"
    );

    // The new run's ask row is a real row (an import hit still lands in storage), carrying the cached event.
    let snapshot = service.get_run(&amended.run_id).expect("snapshot");
    assert_eq!(
        snapshot.resumed_from.as_deref(),
        Some(first.run_id.as_str())
    );
    assert_eq!(snapshot.status, SnapshotStatus::Completed);
    let cached_sites: Vec<String> = service
        .list_events(&amended.run_id, None, Some(1000))
        .events
        .into_iter()
        .filter_map(|stored| match stored.event {
            nc_code_workflow::types::RunEvent::NodeSettled {
                instance,
                cached: Some(true),
                ..
            } => Some(instance.site_id),
            _ => None,
        })
        .collect();
    // // The imported cache covers both the ask and the world read: both settle as cached, and there is no second model request.
    assert_eq!(cached_sites, vec!["ask#1", "world-read#1"]);
    // The amendment does not change the predecessor's row.
    let predecessor = service.get_run(&first.run_id).expect("predecessor");
    assert_eq!(predecessor.status, SnapshotStatus::Completed);
    assert_eq!(predecessor.superseded_by, None);

    service.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_refuses_with_named_errors_and_never_touches_the_row() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new(
        "resume",
        vec![tool_use_sse(
            "toolu-submit-1",
            "submit_result",
            &json!({"result": {"steps": ["a", "b", "c"], "rationale": "first pass"}}),
        )],
    );
    let service = harness.service();
    let submitted = service
        .submit(harness.submit_request("linear"))
        .await
        .expect("submit");
    wait_settled(&service, &submitted.run_id).await;

    match service.resume("dwfrun-missing").await {
        Err(WorkflowServiceError::RunNotFound { run_id }) => assert_eq!(run_id, "dwfrun-missing"),
        other => panic!("unknown run must be RunNotFound, got {other:?}"),
    }
    // completed is not resumable (only stopped counts).
    match service.resume(&submitted.run_id).await {
        Err(WorkflowServiceError::NotResumable { status, .. }) => {
            assert_eq!(status, RunStatus::Completed)
        }
        other => panic!("a completed run must be NotResumable, got {other:?}"),
    }
    assert!(!service.cancel(&submitted.run_id, CancelInitiator::Model));

    service.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_reopens_a_stopped_run_in_the_same_journal() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new("resume-open", vec![text_sse("unused")]);
    let service = harness.service();
    let mut request = harness.submit_request("hang");
    request.script_text = "const never = new Promise<void>(() => {});
await never;
return { ok: true };"
        .to_string();
    let submitted = service.submit(request).await.expect("submit");
    for _ in 0..200 {
        if service.is_live(&submitted.run_id)
            && !service
                .list_events(&submitted.run_id, None, Some(1))
                .events
                .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(service.cancel(&submitted.run_id, CancelInitiator::User));
    let stopped = wait_settled(&service, &submitted.run_id).await;
    assert_eq!(stopped.status, RunStatus::Stopped);
    assert_eq!(stopped.reason, Some(RunStopReason::User));
    let snapshot = service.get_run(&submitted.run_id).expect("snapshot");
    assert!(snapshot.resumable, "stopped(user) is resumable");
    assert_eq!(
        service
            .list_runs(Some(&harness.session_id))
            .first()
            .map(|row| row.resumable),
        Some(true)
    );

    // resume of the same runId: the engine hits the existing row ⇒ the resume branch (the script hash is verified as before).
    let resumed = service.resume(&submitted.run_id).await.expect("resume");
    assert_eq!(resumed.run_id, submitted.run_id);
    assert!(!resumed.imported_cache, "no lineage, no imported cache");
    assert!(service.is_live(&submitted.run_id));
    let snapshot = service
        .get_run(&submitted.run_id)
        .expect("snapshot after resume");
    assert_eq!(snapshot.status, SnapshotStatus::Running);
    assert_eq!(
        snapshot.run_status, None,
        "the row is running again, not terminal"
    );
    // The second run-started proves the engine took the resume branch (instead of creating another run).
    let started = service
        .list_events(&submitted.run_id, None, Some(1000))
        .events
        .iter()
        .filter(|stored| stored.event.type_name() == "run-started")
        .count();
    assert_eq!(started, 2);
    assert_eq!(
        service.list_runs(Some(&harness.session_id)).len(),
        1,
        "same run, no new row"
    );

    // Already in flight in this process: resuming again is AlreadyRunning (wait for it to settle instead of starting a second engine).
    match service.resume(&submitted.run_id).await {
        Err(WorkflowServiceError::AlreadyRunning { .. }) => {}
        other => panic!("a live run must refuse resume, got {other:?}"),
    }
    assert!(service.cancel(&submitted.run_id, CancelInitiator::Interrupted));
    let settlement = wait_settled(&service, &submitted.run_id).await;
    assert_eq!(settlement.status, RunStatus::Stopped);
    assert_eq!(settlement.reason, Some(RunStopReason::Interrupted));
    assert_eq!(
        service
            .get_run(&submitted.run_id)
            .and_then(|snapshot| snapshot.failure)
            .map(|failure| failure.code),
        Some(WorkflowErrorCode::Interrupted)
    );

    service.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_script_is_a_business_failure_and_creates_no_run_row() {
    if !stack_available() {
        return;
    }
    let harness = Harness::new("dirty", vec![text_sse("unused")]);
    let service = harness.service();
    let mut request = harness.submit_request("dirty");
    request.script_text =
        "const broken: number = \"not a number\";\nreturn { ok: true };".to_string();
    match service.submit(request).await {
        Err(WorkflowServiceError::CompileFailed { diagnostics }) => {
            assert!(
                !diagnostics.is_empty(),
                "diagnostics are data, not a message"
            )
        }
        other => panic!("a dirty script must fail compilation, got {other:?}"),
    }
    // Invariants 1 + 2: a refused submit creates not one run row.
    assert!(service.list_runs(None).is_empty());
    assert_eq!(harness.requests.load(Ordering::SeqCst), 0);

    service.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn close_interrupts_live_runs_and_the_engine_writes_the_terminal_row() {
    if !stack_available() {
        return;
    }
    // A script that never returns: the run stays live (the sandbox process is alive), with no model request involved.
    let harness = Harness::new("close", vec![text_sse("unused")]);
    let service = harness.service();
    let mut request = harness.submit_request("hang");
    request.script_text =
        "const never = new Promise<void>(() => {});\nawait never;\nreturn { ok: true };"
            .to_string();
    let submitted = service.submit(request).await.expect("submit");
    // Wait for run-started to land (the engine has already created the row before the sandbox comes up).
    for _ in 0..200 {
        if service
            .get_run(&submitted.run_id)
            .map(|snapshot| snapshot.status == SnapshotStatus::Running)
            .unwrap_or(false)
            && !service
                .list_events(&submitted.run_id, None, Some(1))
                .events
                .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(service.is_live(&submitted.run_id));

    tokio::time::timeout(Duration::from_secs(60), service.close())
        .await
        .expect("close must settle every owned run without a timeout of its own");

    let snapshot = service
        .get_run(&submitted.run_id)
        .expect("snapshot after close");
    assert_eq!(snapshot.status, SnapshotStatus::Cancelled);
    assert_eq!(snapshot.run_status, Some(RunStatus::Stopped));
    assert_eq!(snapshot.stop_reason, Some(RunStopReason::Interrupted));
    assert!(snapshot.resumable, "interrupted is resumable");
    assert_eq!(
        snapshot.failure.map(|failure| failure.code),
        Some(WorkflowErrorCode::Interrupted),
        "the engine writes the interrupted failure itself"
    );
    // The close gate: a submit after it is a wiring fault, not a business refusal.
    match service.submit(harness.submit_request("after-close")).await {
        Err(WorkflowServiceError::Closed) => {}
        other => panic!("submit after close must be Closed, got {other:?}"),
    }
}

// ————————————————————— unit: the fixture-driven compile artifact —————————————————————

/// `workflow_linear.json` is the compile artifact of the two-runtime parity (`lowered` + the ask specs + the source hash).
/// Feeding it straight into `submit_compiled` verifies that the "compile-once seam" runs the same state machine even where there is no compile kernel
/// (this is the entry point of the v4 gateway and of the benchmark).
#[tokio::test(flavor = "multi_thread")]
async fn submit_compiled_accepts_the_parity_fixture() {
    if !stack_available() {
        return;
    }
    let fixture: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../nc-code-workflow/tests/fixtures/workflow_linear.json"
        ))
        .expect("workflow_linear.json fixture"),
    )
    .expect("fixture json");
    let ask_specs: BTreeMap<String, AskSpecWire> =
        serde_json::from_value(fixture["askSpecs"].clone()).expect("ask specs");
    let compiled = CompiledScript {
        script_hash: fixture["scriptHash"].as_str().expect("hash").to_string(),
        lowered: Some(fixture["lowered"].as_str().expect("lowered").to_string()),
        site_ids: fixture["siteIds"]
            .as_array()
            .expect("site ids")
            .iter()
            .filter_map(|site| site.as_str().map(str::to_string))
            .collect(),
        ask_specs,
        declared_run_commands: Vec::new(),
        actor_submit_profiles: BTreeMap::new(),
        diagnostics: Vec::new(),
    };

    let harness = Harness::new(
        "compiled",
        vec![tool_use_sse(
            "toolu-submit-1",
            "submit_result",
            &json!({"result": {"steps": ["x"], "rationale": "fixture"}}),
        )],
    );
    let service = harness.service();
    let mut request = harness.submit_request("fixture");
    request.script_text = linear_fixture_script();
    let submitted = service
        .submit_compiled(request, &CompiledWorkflow::TypeScript(Arc::new(compiled)))
        .await
        .expect("submit_compiled");
    let settlement = wait_settled(&service, &submitted.run_id).await;
    assert_eq!(settlement.status, RunStatus::Completed);
    assert_eq!(
        service
            .get_run(&submitted.run_id)
            .map(|snapshot| snapshot.nodes.completed),
        Some(4)
    );
    service.close().await;
}

// ————————————————————— NWS: the Rust-native workflow path —————————————————————

/// An NWS script of the shape a model would author: one typed ask, one report, a return.
/// It is deliberately small — everything this test is about is that the path from submit to
/// settlement uses no Node process and no TypeScript compile step.
fn nws_host_script() -> String {
    r#"schema Plan {
  steps: string[]
  rationale: string
}

workflow "nws-host-run" {
  phase "plan";
  let planner = agent("planner", "You produce short, concrete plans.");
  let plan: Plan = await planner.ask("Plan the repository work");
  report({ steps: plan.steps });

  phase "close";
  return { steps: plan.steps, rationale: plan.rationale };
}
"#
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_nws_run_settles_through_the_service_without_node() {
    // No `stack_available()` gate on purpose: this path must work when the Node executable
    // and the TypeScript build output are absent, which is the acceptance criterion.
    let harness = Harness::new(
        "nws",
        vec![tool_use_sse(
            "toolu-nws-1",
            "submit_result",
            &json!({"result": {"steps": ["read", "write"], "rationale": "two steps"}}),
        )],
    );
    let service = harness.service();
    let mut request = harness.submit_request("nws-host-run");
    request.script_text = nws_host_script();

    let submitted = service.submit(request).await.expect("submit nws");
    let settlement = wait_settled(&service, &submitted.run_id).await;
    assert_eq!(settlement.status, RunStatus::Completed, "{settlement:?}");

    let snapshot = service.get_run(&submitted.run_id).expect("snapshot");
    assert_eq!(
        snapshot.output,
        Some(json!({"steps": ["read", "write"], "rationale": "two steps"})),
        "the script's return value is the run's output"
    );
    assert_eq!(
        snapshot.reports,
        vec![json!({"steps": ["read", "write"]})],
        "the report row is journaled and readable"
    );

    // One ask node and one report row, both settled.
    assert_eq!(snapshot.nodes.completed, 2, "{:?}", snapshot.nodes);

    // The Node sandbox wrote `<cwd>/.zcode/workflow-runs/<runId>.mjs` before every launch.
    // An NWS run has no child process, so the directory must not exist at all.
    let workspace = PathBuf::from(harness.workspace_path());
    assert!(
        !workspace.join(".zcode").exists(),
        "an NWS run must not create the sandbox entry directory under {:?}",
        workspace
    );
    service.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_nws_diagnostic_refuses_the_run_before_any_row() {
    // A script that does not resolve fails at compile, with the NWS diagnostic code in the
    // message, and creates no run row — the same invariant the TypeScript path enforces.
    let harness = Harness::new("nws-diagnostic", Vec::new());
    let service = harness.service();
    let mut request = harness.submit_request("nws-broken");
    request.script_text = "workflow \"nws-broken\" {\n  return missingName;\n}".to_string();

    let error = service
        .submit(request)
        .await
        .expect_err("an unresolved identifier cannot start a run");
    let diagnostics = match error {
        WorkflowServiceError::CompileFailed { diagnostics } => diagnostics,
        other => panic!("expected CompileFailed, got {other:?}"),
    };
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(
        diagnostics[0].message.contains("nws_unknown_identifier"),
        "{}",
        diagnostics[0].message
    );
    assert_eq!(
        diagnostics[0].line, 2,
        "the diagnostic points at the identifier"
    );
    assert!(
        service.list_runs(Some(&harness.session_id)).is_empty(),
        "a refused submit creates no run row"
    );
    service.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_typescript_script_still_routes_to_the_kernel_stack() {
    // Routing is by the script's own first token, so the live TypeScript path is untouched:
    // a script that is not NWS reaches the kernel exactly as before.
    if !stack_available() {
        return;
    }
    let harness = Harness::new(
        "ts-routing",
        vec![tool_use_sse(
            "toolu-ts-1",
            "submit_result",
            &json!({"result": {"steps": ["x"], "rationale": "r"}}),
        )],
    );
    let service = harness.service();
    let mut request = harness.submit_request("ts-run");
    request.script_text = linear_fixture_script();
    let submitted = service.submit(request).await.expect("submit ts");
    let settlement = wait_settled(&service, &submitted.run_id).await;
    assert_eq!(settlement.status, RunStatus::Completed, "{settlement:?}");
    // The TypeScript path is the one that writes the sandbox entry file.
    let workspace = PathBuf::from(harness.workspace_path());
    assert!(
        workspace.join(".zcode").join("workflow-runs").exists(),
        "the sandbox entry file is expected on the TypeScript path"
    );
    service.close().await;
}
