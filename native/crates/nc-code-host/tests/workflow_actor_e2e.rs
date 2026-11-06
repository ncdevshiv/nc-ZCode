//! workflow actor execution-layer e2e: real host + real driver + real child-session turn loop + scripted provider.
//!
//! This wave does not have the run service yet (launch / projection land later). So the test assembles
//! `HostServer` + `HostWorkflowDriver` + a `WorkflowReportSink` playing the engine directly **in-process**: after
//! the sink receives `ask_submit_attempted` it calls `respond_to_submit` per the engine's contract (same call stack), and
//! everything else is real — real child sessions, real turns, real `submit_result` / `escalate` tools, real model requests
//! (a local scripted SSE responder).
//!
//! Assertions land on the child session's row projection, the request bodies the provider received and the driver's journal events, not on internal structure.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use common::{text_sse, tool_use_sse_with_id, ScriptedResponder};
use nc_code_host::provider_config::ProviderConfig;
use nc_code_host::session::Session;
use nc_code_host::workflow::{HostWorkflowDriver, HostWorkflowDriverConfig};
use nc_code_host::HostServer;
use nc_code_workflow::journal_memory::InMemoryJournalStore;
use nc_code_workflow::types::{
    ActorRef, AskMessage, AskProgress, AskStats, AskWaitInfo, ConcurrencyChange, InstanceRef,
    JournalStorePort, ListEventsOptions, PersonaSpec, RunEvent, RunStallInfo, SubmitVerdict,
    Violation, WorkflowDriver, WorkflowError, WorkflowReportSink,
};
use serde_json::{json, Value};

const RUN_ID: &str = "dwfrun-e2e-actor";

fn temp_base(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "zcode-workflow-actor-{name}-{}",
        std::process::id()
    ))
}

// ——————————————————————————————— the sink playing the engine ———————————————————————————————

#[derive(Default)]
struct SinkState {
    submits: Vec<Value>,
    turn_ended: Vec<String>,
    stats: Vec<AskStats>,
    failures: Vec<String>,
    stops: Vec<String>,
    mutating: u32,
}

struct TestSink {
    driver: Mutex<Option<Weak<HostWorkflowDriver>>>,
    state: Mutex<SinkState>,
    /// The first submit returns Reject (with one violation), so the test gets to see the repair channel.
    reject_first: bool,
    /// At the end of the first turn it returns Nudge (letting the driver start another turn on the same session).
    nudge_first_turn: bool,
}

impl TestSink {
    fn new(reject_first: bool, nudge_first_turn: bool) -> Arc<Self> {
        Arc::new(Self {
            driver: Mutex::new(None),
            state: Mutex::new(SinkState::default()),
            reject_first,
            nudge_first_turn,
        })
    }

    fn attach(&self, driver: &Arc<HostWorkflowDriver>) {
        if let Ok(mut slot) = self.driver.lock() {
            *slot = Some(Arc::downgrade(driver));
        }
    }

    fn driver(&self) -> Option<Arc<HostWorkflowDriver>> {
        self.driver
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|weak| weak.upgrade()))
    }

    fn snapshot(&self) -> SinkState {
        let state = self.state.lock().expect("sink state");
        SinkState {
            submits: state.submits.clone(),
            turn_ended: state.turn_ended.clone(),
            stats: state.stats.clone(),
            failures: state.failures.clone(),
            stops: state.stops.clone(),
            mutating: state.mutating,
        }
    }

    fn stats_len(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.stats.len())
            .unwrap_or(0)
    }
}

impl WorkflowReportSink for TestSink {
    fn ask_submit_attempted(&self, instance: &InstanceRef, payload: Value) {
        let attempt = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.submits.push(payload);
            state.submits.len()
        };
        let Some(driver) = self.driver() else {
            return;
        };
        // The engine's verdict is delivered synchronously in the same call stack as ask_submit_attempted.
        if self.reject_first && attempt == 1 {
            driver.respond_to_submit(
                instance,
                SubmitVerdict::Reject {
                    violations: vec![Violation {
                        path: "$.answer".to_string(),
                        expected: "integer".to_string(),
                        got: "string".to_string(),
                    }],
                },
            );
        } else {
            driver.respond_to_submit(instance, SubmitVerdict::Accept);
        }
    }

    fn ask_turn_ended(&self, instance: &InstanceRef, final_text: &str) {
        let turns = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.turn_ended.push(final_text.to_string());
            state.turn_ended.len()
        };
        if self.nudge_first_turn && turns == 1 {
            if let Some(driver) = self.driver() {
                driver.respond_to_submit(instance, SubmitVerdict::Nudge);
            }
        }
    }

    fn ask_progress(&self, _instance: &InstanceRef, _progress: AskProgress) {}

    fn ask_stats(&self, _instance: &InstanceRef, stats: AskStats) {
        if let Ok(mut state) = self.state.lock() {
            state.stats.push(stats);
        }
    }

    fn ask_failed(&self, _instance: &InstanceRef, error: WorkflowError) {
        if let Ok(mut state) = self.state.lock() {
            state.failures.push(error.to_string());
        }
    }

    fn stop_run(&self, error: WorkflowError) {
        if let Ok(mut state) = self.state.lock() {
            state.stops.push(error.to_string());
        }
    }

    fn run_stalled(&self, _info: RunStallInfo) {}

    fn ask_waiting(&self, _instance: &InstanceRef, _info: AskWaitInfo) {}

    fn ask_executing(&self, _instance: &InstanceRef) {}

    fn ask_mutating(&self, _instance: &InstanceRef) {
        if let Ok(mut state) = self.state.lock() {
            state.mutating += 1;
        }
    }

    fn concurrency_changed(&self, _change: ConcurrencyChange) {}
}

// ——————————————————————————————— in-process assembly ———————————————————————————————

struct Harness {
    base: PathBuf,
    host: Arc<HostServer>,
    parent: Arc<Session>,
    driver: Arc<HostWorkflowDriver>,
    journal: Arc<InMemoryJournalStore>,
    provider: ScriptedResponder,
}

impl Harness {
    async fn new(script: Vec<String>, sink: Arc<TestSink>, name: &str) -> Self {
        let (provider, port) = ScriptedResponder::spawn(script);
        let base = temp_base(name);
        let _ = std::fs::remove_dir_all(&base);
        let workspace = base.join("workspace");
        let data_dir = base.join("data");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&data_dir).expect("data dir");
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
        let provider_config = ProviderConfig::load(&config_path).expect("load provider config");
        let (host, outbox) = HostServer::new(
            workspace,
            data_dir.clone(),
            Some(provider_config),
            true,
            vec![],
        )
        .expect("host");
        // Frame outlet: an in-process assembly has no stdout writer, so drain it (we care about the session rows, not the frames).
        tokio::spawn(async move {
            let mut outbox = outbox;
            while outbox.recv().await.is_some() {}
        });

        // createSession goes through the real command surface (the host's only session-creation path).
        let line = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "v4/command",
            "params": {
                "type": "createSession",
                "commandId": "cmd-create",
                "clientId": "e2e",
                "sessionId": null,
                "payload": {},
                "issuedAt": 1_700_000_000_000.0,
            }
        })
        .to_string();
        let (response, _) = host.handle_line(&line).await;
        let response: Value =
            serde_json::from_str(response.as_deref().expect("createSession response"))
                .expect("json response");
        let parent_id = response["result"]["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string();
        let parent = host.load_session(&parent_id).expect("parent session");

        let journal = Arc::new(InMemoryJournalStore::new());
        let journal_port: Arc<dyn JournalStorePort> =
            Arc::clone(&journal) as Arc<dyn JournalStorePort>;
        let driver = HostWorkflowDriver::new(
            HostWorkflowDriverConfig {
                run_id: RUN_ID.to_string(),
                data_dir,
                declared_run_commands: None,
                events: None,
            },
            Arc::clone(&host),
            Arc::clone(&parent),
            journal_port,
        );
        let sink_port: Arc<dyn WorkflowReportSink> =
            Arc::clone(&sink) as Arc<dyn WorkflowReportSink>;
        driver.attach_sink(sink_port);
        sink.attach(&driver);
        Self {
            base,
            host,
            parent,
            driver,
            journal,
            provider,
        }
    }

    fn actor(&self) -> ActorRef {
        ActorRef {
            site_id: "actor#1".to_string(),
            ordinal: 1,
        }
    }

    fn instance(&self) -> InstanceRef {
        InstanceRef {
            site_id: "ask#1".to_string(),
            ordinal: 1,
        }
    }

    fn persona() -> PersonaSpec {
        PersonaSpec {
            name: Some("critic".to_string()),
            system: Some(
                "You are the workflow critic. Answer only in the required schema.".to_string(),
            ),
        }
    }

    fn typed_message(&self) -> AskMessage {
        AskMessage {
            instructions: "Report how many items there are.".to_string(),
            typed: true,
            schema: Some(json!({
                "type": "object",
                "properties": {"answer": {"type": "integer"}},
                "required": ["answer"]
            })),
        }
    }

    fn child(&self, session_id: &str) -> Arc<Session> {
        self.host.load_session(session_id).expect("child session")
    }

    async fn create_session(&self) -> nc_code_workflow::types::SessionRef {
        self.driver
            .create_actor_session(&self.actor(), &Self::persona(), None)
            .await
            .expect("create actor session")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.driver.dispose();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

async fn wait_until(mut predicate: impl FnMut() -> bool, label: &str) {
    for _ in 0..1_000 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {label}");
}

fn terminal_tool_row(child: &Session, name: &str) -> Option<Value> {
    child.rows_snapshot().into_iter().rev().find(|row| {
        row["kind"] == json!("toolCall")
            && row["toolName"] == json!(name)
            && matches!(row["status"].as_str(), Some("success") | Some("error"))
    })
}

fn submit_sse(payload: Value) -> String {
    tool_use_sse_with_id("toolu-submit", "submit_result", &json!({"result": payload}))
}

// ——————————————————————————————— tests ———————————————————————————————

#[tokio::test(flavor = "multi_thread")]
async fn a_submitted_payload_is_accepted_and_the_ask_settles() {
    let sink = TestSink::new(false, false);
    let harness = Harness::new(
        vec![
            submit_sse(json!({"answer": 42})),
            // After acceptance the driver raises the session cancel, so in theory there is no second request; this is a defensive script entry.
            text_sse("should not be reached"),
        ],
        Arc::clone(&sink),
        "accept",
    )
    .await;

    let session = harness.create_session().await;
    // The session identity lands on the host's meta: title = the persona name, parent, subagent type.
    let child = harness.child(&session.id);
    assert_eq!(child.title().0, "critic");
    assert_eq!(
        child.parent_session_id.as_deref(),
        Some(harness.parent.id.as_str())
    );
    assert_eq!(child.task_type.as_deref(), Some("subagent_child"));
    assert_eq!(
        harness.driver.session_for_actor(&harness.actor()),
        Some(session.clone()),
        "the driver registry must map the actor to the created session"
    );

    let instance = harness.instance();
    harness
        .driver
        .start_ask(&session, &instance, harness.typed_message());

    wait_until(
        || sink.stats_len() > 0 && terminal_tool_row(&child, "submit_result").is_some(),
        "ask stats after the accepted turn",
    )
    .await;

    // Driver layer: the submit payload arrives verbatim on the engine's verdict surface, and was submitted exactly once.
    {
        let state = sink.snapshot();
        assert_eq!(state.submits, vec![json!({"answer": 42})]);
        assert!(
            state.turn_ended.is_empty(),
            "an accepted ask must not report ask_turn_ended: {:?}",
            state.turn_ended
        );
        assert!(state.failures.is_empty(), "{:?}", state.failures);
        assert!(state.stops.is_empty(), "{:?}", state.stops);
        let stats = state.stats.last().expect("stats");
        assert!(
            stats.tokens > 0,
            "tokens must come from the real usage: {stats:?}"
        );
        assert_eq!(stats.tool_calls, 1, "submit_result counts as one tool call");
        assert_eq!(
            stats.world_tool_calls,
            Some(0),
            "submit_result is a protocol tool and never touches the world"
        );
    }

    // The child session's real row projection: submit_result succeeded, and the model content confirms acceptance.
    let row = terminal_tool_row(&child, "submit_result").expect("row");
    assert_eq!(row["status"], json!("success"), "{row}");
    assert!(
        row["output"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("accepted"),
        "{row}"
    );

    // The prompt the model received: instruction body + quality trailer + schema trailer (generic submit profile).
    let bodies = harness.provider.request_bodies();
    assert_eq!(
        bodies.len(),
        1,
        "accept stops the turn before another model request"
    );
    assert!(bodies[0].contains("Report how many items there are."));
    assert!(bodies[0].contains("submit_result"));
    assert!(bodies[0].contains("JSON Schema"));
    assert!(bodies[0].contains("You are the workflow critic."));
    assert!(
        !bodies[0].contains("\"name\":\"Agent\""),
        "the actor tool face must not advertise the recursive Agent tool"
    );

    // Driver-private fact: dwf_actor's session_id has been written back into the journal (the authoritative source for resume).
    let actor_record = harness
        .journal
        .get_actor(RUN_ID, "actor#1", 1)
        .expect("actor record");
    assert_eq!(
        actor_record.session_id.as_deref(),
        Some(session.id.as_str()),
        "the journal must remember which host session this actor runs in"
    );
    assert_eq!(actor_record.persona.as_ref(), Some(&Harness::persona()));
    // The host's session has no provider/model tier (the provider is host-level), so no fake pin is written.
    assert!(actor_record.resolved_model.is_none(), "{actor_record:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_payload_returns_a_repairable_tool_result() {
    let sink = TestSink::new(true, false);
    let harness = Harness::new(
        vec![
            submit_sse(json!({"answer": "not an integer"})),
            submit_sse(json!({"answer": 7})),
            text_sse("should not be reached"),
        ],
        Arc::clone(&sink),
        "repair",
    )
    .await;
    let session = harness.create_session().await;
    let child = harness.child(&session.id);
    harness
        .driver
        .start_ask(&session, &harness.instance(), harness.typed_message());

    wait_until(
        || sink.snapshot().submits.len() == 2 && sink.stats_len() > 0,
        "the repaired submit to settle",
    )
    .await;
    let state = sink.snapshot();
    assert_eq!(
        state.submits,
        vec![json!({"answer": "not an integer"}), json!({"answer": 7})]
    );
    assert!(
        state.turn_ended.is_empty(),
        "the repair happens inside one turn, so no turn ended"
    );
    assert!(state.failures.is_empty(), "{:?}", state.failures);

    // The repair channel: the per-item violations are visible in the second model request (the same error tool_result).
    let bodies = harness.provider.request_bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(
        bodies[1].contains("$.answer: expected integer, got string"),
        "the rejected tool result must be repairable: {}",
        bodies[1]
    );
    assert!(bodies[1].contains("does not match the required schema"));

    let row = terminal_tool_row(&child, "submit_result").expect("submit row");
    assert_eq!(row["status"], json!("success"), "{row}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_ends_without_submitting_is_nudged_once() {
    let sink = TestSink::new(false, true);
    let harness = Harness::new(
        vec![
            text_sse("I forgot to submit."),
            submit_sse(json!({"answer": 1})),
            text_sse("should not be reached"),
        ],
        Arc::clone(&sink),
        "nudge",
    )
    .await;
    let session = harness.create_session().await;
    harness
        .driver
        .start_ask(&session, &harness.instance(), harness.typed_message());

    wait_until(
        || {
            let state = sink.snapshot();
            state.submits.len() == 1 && state.turn_ended.len() == 1 && sink.stats_len() >= 2
        },
        "the nudged turn to submit",
    )
    .await;
    let state = sink.snapshot();
    assert_eq!(state.turn_ended, vec!["I forgot to submit.".to_string()]);
    assert_eq!(state.submits, vec![json!({"answer": 1})]);
    assert!(state.failures.is_empty(), "{:?}", state.failures);

    let bodies = harness.provider.request_bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(
        bodies[1].contains("You ended your turn without submitting a result."),
        "the nudge prompt must be the second turn's instruction"
    );
    // The nudge turn is still inside the same ask: the later stats are the accumulated tool counts.
    let last = state.stats.last().expect("stats");
    assert_eq!(last.tool_calls, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn escalate_parks_the_turn_until_the_main_agent_answers() {
    let sink = TestSink::new(false, false);
    let harness = Harness::new(
        vec![
            tool_use_sse_with_id(
                "toolu-escalate",
                "escalate",
                &json!({
                    "question": "Which gate should I use?",
                    "context": "Both gates are capped."
                }),
            ),
            submit_sse(json!({"answer": 3})),
            text_sse("should not be reached"),
        ],
        Arc::clone(&sink),
        "escalate",
    )
    .await;
    let session = harness.create_session().await;
    let registry = harness.driver.escalation_registry();
    harness
        .driver
        .start_ask(&session, &harness.instance(), harness.typed_message());

    // The tool parks in the registry waiting for an answer; the ask has not settled (there is no submit yet).
    wait_until(
        || !registry.pending_questions().is_empty(),
        "the escalation to be parked",
    )
    .await;
    let pending = registry.pending_questions();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].question, "Which gate should I use?");
    assert_eq!(
        pending[0].context.as_deref(),
        Some("Both gates are capped.")
    );
    assert_eq!(pending[0].actor_name.as_deref(), Some("critic"));
    let qid = pending[0].qid.clone();
    assert!(registry.resolve_question(&qid, "Use gate B."));
    assert!(
        !registry.resolve_question(&qid, "again"),
        "one answer per qid"
    );

    wait_until(
        || sink.snapshot().submits.len() == 1 && sink.stats_len() > 0,
        "the ask to settle after the answer",
    )
    .await;
    let state = sink.snapshot();
    assert!(state.failures.is_empty(), "{:?}", state.failures);
    assert_eq!(
        state.stats.last().expect("stats").tool_calls,
        2,
        "escalate + submit both count as tool calls"
    );

    // The answer entered the model context.
    let bodies = harness.provider.request_bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(
        bodies[1].contains("Use gate B."),
        "the answer must reach the model as the tool result"
    );

    // Both escalation events landed in the journal (the two-track landing of the driver's private events).
    let events = harness
        .journal
        .list_events(RUN_ID, ListEventsOptions::default());
    assert!(
        events
            .iter()
            .any(|stored| matches!(&stored.event, RunEvent::EscalationRaised { qid: raised, .. } if raised == &qid)),
        "escalation-raised must be journaled"
    );
    assert!(
        events.iter().any(|stored| matches!(
            &stored.event,
            RunEvent::EscalationResolved { qid: resolved, answer } if resolved == &qid && answer == "Use gate B."
        )),
        "escalation-resolved must be journaled"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mutating_tool_call_closes_the_import_cache_before_it_runs() {
    let sink = TestSink::new(false, false);
    let harness = Harness::new(
        vec![
            tool_use_sse_with_id(
                "toolu-write",
                "Write",
                &json!({"file_path": "notes.txt", "content": "written by the actor"}),
            ),
            submit_sse(json!({"answer": 5})),
            text_sse("should not be reached"),
        ],
        Arc::clone(&sink),
        "mutating",
    )
    .await;
    let session = harness.create_session().await;
    harness
        .driver
        .start_ask(&session, &harness.instance(), harness.typed_message());

    wait_until(
        || sink.snapshot().submits.len() == 1 && sink.stats_len() > 0,
        "the mutating ask to settle",
    )
    .await;
    let state = sink.snapshot();
    assert_eq!(
        state.mutating, 1,
        "the close signal fires once per ask, before the first workspace write"
    );
    let stats = state.stats.last().expect("stats");
    assert_eq!(stats.tool_calls, 2, "Write + submit_result");
    assert_eq!(
        stats.world_tool_calls,
        Some(1),
        "Write touches the world; submit_result does not"
    );
    let written = harness.parent.workspace_root.join("notes.txt");
    assert_eq!(
        std::fs::read_to_string(&written).expect("the actor's write landed"),
        "written by the actor",
        "the wrapper must delegate, not swallow the tool call"
    );
}
