//! Cross-runtime parity oracle as a permanent test.
//!
//! The TS pipeline (`scripts/bench/workflow-ts-probe.mjs`) ran the same generated fixtures
//! and captured the event-type sequences and settlements below. This test drives the native
//! stack (real engine + real Node sandbox child + real in-memory journal, scripted model
//! answers exactly like the TS probe) and asserts the same outputs. Fixtures are produced
//! by `scripts/bench/gen-workflow-fixtures.mjs` through the real TypeScript front-end, so
//! both runtimes execute byte-identical lowered bodies.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::OnceCell;

use nc_code_workflow::engine::{EngineConfig, WorkflowEngine};
use nc_code_workflow::journal_memory::InMemoryJournalStore;
use nc_code_workflow::sandbox::{run_workflow_script, SandboxOptions};
use nc_code_workflow::types::{
    ActorRef, ActorSessionSeed, AskMessage, AskSpec, Caps, InstanceRef, JournalStorePort,
    ListEventsOptions, PersonaSpec, RunEvent, RunStatus, SessionRef, SubmitVerdict, WorkflowDriver,
    WorkflowError, WorkflowReportSink,
};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    site_ids: Vec<String>,
    lowered: String,
    ask_specs: HashMap<String, FixtureAskSpec>,
}

#[derive(serde::Deserialize)]
struct FixtureAskSpec {
    typed: bool,
    #[serde(default)]
    schema: Option<Value>,
}

struct ScriptedDriver {
    journal: Arc<InMemoryJournalStore>,
    engine: Arc<OnceCell<Arc<WorkflowEngine>>>,
    answers: HashMap<String, Value>,
}

#[async_trait::async_trait]
impl WorkflowDriver for ScriptedDriver {
    fn journal(&self) -> &dyn JournalStorePort {
        &*self.journal
    }
    fn emit(&self, _event: &RunEvent) {}

    async fn create_actor_session(
        &self,
        actor: &ActorRef,
        _persona: &PersonaSpec,
        _seed: Option<ActorSessionSeed>,
    ) -> Result<SessionRef, WorkflowError> {
        Ok(SessionRef {
            id: format!("parity-session-{}-{}", actor.site_id, actor.ordinal),
        })
    }

    fn start_ask(&self, _session: &SessionRef, instance: &InstanceRef, message: AskMessage) {
        let engine = self.engine.get().expect("engine set").clone();
        let instance = instance.clone();
        let answer = self.answers.get(&instance.site_id).cloned();
        tokio::spawn(async move {
            if message.typed {
                if let Some(answer) = answer {
                    engine.ask_submit_attempted(&instance, answer);
                }
            } else {
                engine.ask_turn_ended(
                    &instance,
                    &format!("final text for {}@{}", instance.site_id, instance.ordinal),
                );
            }
        });
    }

    fn respond_to_submit(&self, _instance: &InstanceRef, _verdict: SubmitVerdict) {}
    fn cancel_ask(&self, _instance: &InstanceRef) {}

    async fn execute_world_read(&self, op: &str, args: Vec<Value>) -> Result<Value, WorkflowError> {
        // Same as the TS probe: read the repository's real README.
        if op == "read" {
            let rel = args.first().and_then(Value::as_str).unwrap_or_default();
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(3)
                .unwrap()
                .to_path_buf();
            return std::fs::read_to_string(root.join(rel))
                .map(Value::String)
                .map_err(|error| {
                    WorkflowError::new(
                        nc_code_workflow::types::WorkflowErrorCode::DriverError,
                        format!("world read failed: {error}"),
                    )
                });
        }
        Err(WorkflowError::new(
            nc_code_workflow::types::WorkflowErrorCode::DriverError,
            format!("parity driver: unsupported world op {op}"),
        ))
    }

    async fn execute_artifact_publish(
        &self,
        request: nc_code_workflow::types::ArtifactPublishRequest,
    ) -> Result<nc_code_workflow::types::ArtifactVersionRecord, WorkflowError> {
        Ok(nc_code_workflow::types::ArtifactVersionRecord {
            id: request.id.clone(),
            kind: request.op,
            version: request.version,
            title: None,
            description: None,
            content_type: Some("text/markdown".to_string()),
            bytes: request.content.as_ref().map(|content| content.len() as u64),
            uri: Some(format!("zcode-artifact://parity/{}", request.id)),
            source_path: None,
            spec: None,
            published_at: Some(0.0),
            primary: None,
        })
    }

    fn dispose(&self) {}
}

fn fixture(name: &str) -> Fixture {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "missing fixture {} ({error}); run `node scripts/bench/gen-workflow-fixtures.mjs`",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("fixture json")
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

async fn run_fixture(name: &str, answers: HashMap<String, Value>) -> (Vec<String>, Value, Value) {
    let fixture = fixture(name);
    let journal = Arc::new(InMemoryJournalStore::new());
    let cell: Arc<OnceCell<Arc<WorkflowEngine>>> = Arc::new(OnceCell::new());
    let driver = Arc::new(ScriptedDriver {
        journal: journal.clone(),
        engine: cell.clone(),
        answers,
    });
    let ask_specs = fixture
        .ask_specs
        .iter()
        .map(|(site, spec)| {
            (
                site.clone(),
                AskSpec {
                    typed: spec.typed,
                    schema: spec.schema.clone(),
                },
            )
        })
        .collect::<HashMap<_, _>>();
    let cwd = std::env::temp_dir().join(format!("zcode-wf-parity-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let engine = WorkflowEngine::new(EngineConfig {
        run_id: format!("parity-{name}"),
        driver,
        caps: Caps { max_concurrency: 4 },
        ask_specs,
        validate: Arc::new(|_schema: Option<&Value>, _value: &Value| Vec::new()),
        script_text: None,
        name: None,
        script_hash: Some("parity".to_string()),
        args: None,
        parent_session_id: None,
        cwd: Some(cwd.to_string_lossy().to_string()),
        tool_call_id: None,
        resumed_from: None,
        launch: None,
        inherited_tokens: None,
        imported_cache: None,
    })
    .expect("engine");
    let _ = cell.set(engine.clone());
    let settlement = run_workflow_script(
        engine,
        SandboxOptions::new(
            format!("parity-{name}"),
            fixture.lowered.clone(),
            cwd.to_string_lossy().to_string(),
        ),
    )
    .await;
    assert_eq!(
        settlement.status,
        RunStatus::Completed,
        "{name}: settlement"
    );
    let events = journal
        .list_events(&format!("parity-{name}"), ListEventsOptions::default())
        .into_iter()
        .map(|stored| stored.event.type_name().to_string())
        .collect::<Vec<_>>();
    let artifact = settlement.artifact.expect("artifact");
    let site_ids = fixture.site_ids;
    (events, artifact, json!(site_ids))
}

/// workflow_linear: the per-event sequence the TS probe captured (2026-10-30).
#[tokio::test]
async fn linear_matches_the_ts_event_oracle_and_settlement() {
    if !node_available() {
        eprintln!("skipping: node not available");
        return;
    }
    let (events, artifact, site_ids) = run_fixture(
        "workflow_linear",
        HashMap::from([(
            "ask#1".to_string(),
            json!({"steps": ["s1", "s2", "s3"], "rationale": "probe rationale"}),
        )]),
    )
    .await;
    assert_eq!(
        site_ids,
        json!(["actor#1", "ask#1", "world-read#1", "report#1", "artifact#1"])
    );
    let expected = [
        "run-started",
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
    ];
    assert_eq!(events, expected, "linear event sequence is the TS oracle");
    // TS probe settlement: { rationale, steps } (the scripted typed answer).
    assert_eq!(
        artifact,
        json!({"rationale": "probe rationale", "steps": ["s1", "s2", "s3"]})
    );
}

/// workflow_fanout: the settle order of concurrent siblings is no contract; the counts and the order invariants are.
#[tokio::test]
async fn fanout_matches_the_ts_counts_and_order_invariants() {
    if !node_available() {
        eprintln!("skipping: node not available");
        return;
    }
    let (events, artifact, site_ids) = run_fixture(
        "workflow_fanout",
        HashMap::from([(
            "ask#2".to_string(),
            json!({"merged": "merged findings", "count": 4}),
        )]),
    )
    .await;
    assert_eq!(
        site_ids,
        json!(["actor#1", "ask#1", "report#1", "actor#2", "ask#2"])
    );
    let count = |kind: &str| events.iter().filter(|event| *event == kind).count();
    assert_eq!(count("run-started"), 1);
    assert_eq!(count("actor-created"), 5, "4 workers + coordinator");
    assert_eq!(count("node-queued"), 5);
    assert_eq!(count("node-dispatched"), 5);
    assert_eq!(count("node-settled"), 5);
    assert_eq!(count("report"), 1);
    assert_eq!(count("run-settled"), 1);
    assert_eq!(events.first().map(String::as_str), Some("run-started"));
    assert_eq!(events.last().map(String::as_str), Some("run-settled"));
    assert_eq!(events.len(), 23, "TS probe captured 23 events");
    // Every node-queued comes before its node-dispatched and then before node-settled, balanced by order of appearance.
    let mut queued = 0usize;
    let mut dispatched = 0usize;
    let mut settled = 0usize;
    for event in &events {
        match event.as_str() {
            "node-queued" => queued += 1,
            "node-dispatched" => {
                dispatched += 1;
                assert!(dispatched <= queued, "dispatch cannot precede its queue");
            }
            "node-settled" => {
                settled += 1;
                assert!(settled <= dispatched, "settle cannot precede its dispatch");
            }
            _ => {}
        }
    }
    assert_eq!(artifact, json!({"merged": "merged findings", "count": 4}));
}
