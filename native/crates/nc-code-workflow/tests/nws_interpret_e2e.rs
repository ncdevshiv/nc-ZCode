//! End-to-end tests for the Rust-native NWS path: real engine, real journal, in-process
//! interpreter, scripted model answers.
//!
//! These cover acceptance 1-3 of `specs/nc-code-workflow-rust.md`: an NWS script runs to
//! settlement through the unchanged engine, journals the same `(siteId, ordinal)` keys the
//! Node sandbox produced, and needs no child process — asserted here by the sandbox's entry
//! directory never being created.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::OnceCell;

use nc_code_workflow::engine::{EngineConfig, RunLaunchConfig, WorkflowEngine};
use nc_code_workflow::journal_memory::InMemoryJournalStore;
use nc_code_workflow::script::diagnostic;
use nc_code_workflow::script::interpret::{self, ScriptOptions};
use nc_code_workflow::script::Plan;
use nc_code_workflow::types::{
    ActorRef, ActorSessionSeed, ArtifactPublishRequest, ArtifactVersionRecord, AskMessage, Caps,
    InstanceRef, JournalStorePort, ListEventsOptions, NodeKind, PersonaSpec, RunEvent,
    RunSettlement, RunStatus, RunStopReason, SessionRef, SubmitVerdict, Violation, WorkflowDriver,
    WorkflowError, WorkflowErrorCode, WorkflowReportSink,
};

/// Every world call the driver made, in order: `(op, args)`.
type WorldReads = Arc<Mutex<Vec<(String, Vec<Value>)>>>;

/// A driver that answers asks from a table keyed by ask site id, the way the cross-runtime
/// parity harness does. `silent` holds a run in flight for the timeout and cancel tests,
/// and a rejected submit is retried so the engine's repair loop runs to its real end.
struct ScriptedDriver {
    journal: Arc<InMemoryJournalStore>,
    engine: Arc<OnceCell<Arc<WorkflowEngine>>>,
    answers: HashMap<String, Value>,
    silent: HashSet<String>,
    world_reads: WorldReads,
    submits: Mutex<HashMap<String, u32>>,
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
            id: format!("nws-session-{}-{}", actor.site_id, actor.ordinal),
        })
    }

    fn start_ask(&self, _session: &SessionRef, instance: &InstanceRef, message: AskMessage) {
        if self.silent.contains(&instance.site_id) {
            return;
        }
        let engine = self.engine.get().expect("engine set").clone();
        let instance = instance.clone();
        let answer = self
            .answers
            .get(&instance.site_id)
            .cloned()
            .unwrap_or_else(|| json!({ "text": format!("answer to {}", instance.key()) }));
        let typed = message.typed;
        tokio::spawn(async move {
            if typed {
                engine.ask_submit_attempted(&instance, answer);
            } else {
                engine.ask_turn_ended(&instance, &answer.to_string());
            }
        });
    }

    fn respond_to_submit(&self, instance: &InstanceRef, verdict: SubmitVerdict) {
        // The engine's repair channel: keep re-submitting the same (invalid) answer until
        // the repair budget runs out, which is how a node actually fails in production.
        if !matches!(verdict, SubmitVerdict::Reject { .. }) {
            return;
        }
        let mut submits = self.submits.lock().expect("submit counter");
        let count = submits.entry(instance.key()).or_insert(0);
        *count += 1;
        if *count > 4 {
            return;
        }
        let answer = self
            .answers
            .get(&instance.site_id)
            .cloned()
            .unwrap_or(Value::Null);
        let engine = self.engine.get().expect("engine set").clone();
        let instance = instance.clone();
        tokio::spawn(async move {
            engine.ask_submit_attempted(&instance, answer);
        });
    }

    fn cancel_ask(&self, _instance: &InstanceRef) {}

    async fn execute_world_read(&self, op: &str, args: Vec<Value>) -> Result<Value, WorkflowError> {
        self.world_reads
            .lock()
            .expect("world read log")
            .push((op.to_string(), args.clone()));
        match op {
            "read" => Ok(Value::String("x".repeat(100))),
            "glob" => Ok(json!(["a.md", "b.md"])),
            "run" => Ok(json!({"stdout": "ok", "stderr": "", "code": 0})),
            other => Err(WorkflowError::new(
                WorkflowErrorCode::DriverError,
                format!("nws test driver: unsupported world op {other}"),
            )),
        }
    }

    async fn execute_artifact_publish(
        &self,
        request: ArtifactPublishRequest,
    ) -> Result<ArtifactVersionRecord, WorkflowError> {
        Ok(ArtifactVersionRecord {
            id: request.id.clone(),
            kind: request.op,
            version: request.version,
            title: None,
            description: None,
            content_type: Some("text/markdown".to_string()),
            bytes: request.content.as_ref().map(|content| content.len() as u64),
            uri: Some(format!("nws-artifact://test/{}", request.id)),
            source_path: None,
            spec: None,
            published_at: Some(0.0),
            primary: None,
        })
    }

    fn dispose(&self) {}
}

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}.nws"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("missing fixture {} ({error})", path.display()))
}

/// Compile an NWS script; a compile failure fails the test with the diagnostics inline.
fn compile(name: &str) -> Arc<Plan> {
    let source = fixture(name);
    Arc::new(Plan::compile(&source).unwrap_or_else(|diagnostics| {
        panic!(
            "{name} must compile, got:\n{}",
            diagnostic::render(&diagnostics)
        )
    }))
}

/// Ask-result validator: the schema's `required` keys must be present, so the typed path in
/// these tests is a real validation round rather than a no-op.
fn required_key_validator() -> nc_code_workflow::types::ValidateFn {
    Arc::new(|schema: Option<&Value>, value: &Value| {
        let Some(required) = schema
            .and_then(|schema| schema.get("required"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        required
            .iter()
            .filter_map(Value::as_str)
            .filter(|key| match value.get(*key) {
                None => true,
                Some(found) => found.is_null(),
            })
            .map(|key| Violation {
                path: format!("/{key}"),
                expected: "a required field".to_string(),
                got: "missing".to_string(),
            })
            .collect()
    })
}

struct Harness {
    journal: Arc<InMemoryJournalStore>,
    engine: Arc<WorkflowEngine>,
    run_id: String,
    cwd: PathBuf,
    world_reads: WorldReads,
}

/// Build a fresh run: real engine over an in-memory journal, scripted driver.
fn harness(name: &str, plan: &Plan, args: Value, answers: HashMap<String, Value>) -> Harness {
    harness_with(name, plan, args, answers, HashSet::new(), None)
}

fn harness_with(
    name: &str,
    plan: &Plan,
    args: Value,
    answers: HashMap<String, Value>,
    silent: HashSet<String>,
    resumed: Option<&Harness>,
) -> Harness {
    let journal = resumed.map_or_else(
        || Arc::new(InMemoryJournalStore::new()),
        |previous| Arc::clone(&previous.journal),
    );
    let cwd = resumed.map_or_else(
        || {
            let path = std::env::temp_dir().join(format!("nws-e2e-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp cwd");
            path
        },
        |previous| previous.cwd.clone(),
    );
    let cell: Arc<OnceCell<Arc<WorkflowEngine>>> = Arc::new(OnceCell::new());
    let world_reads = Arc::new(Mutex::new(Vec::new()));
    let driver = Arc::new(ScriptedDriver {
        journal: Arc::clone(&journal),
        engine: Arc::clone(&cell),
        answers,
        silent,
        world_reads: Arc::clone(&world_reads),
        submits: Mutex::new(HashMap::new()),
    });
    let run_id = format!("nws-{name}");
    let engine = WorkflowEngine::new(EngineConfig {
        run_id: run_id.clone(),
        driver,
        caps: Caps { max_concurrency: 4 },
        ask_specs: plan.engine_ask_specs(),
        validate: required_key_validator(),
        script_text: Some(plan.source.clone()),
        name: Some(plan.workflow_name.clone()),
        // Resume compares this hash against the row it created; the byte-identical script
        // is the precondition, so it is the plan's own hash and never a placeholder.
        script_hash: Some(plan.script_hash.clone()),
        args: Some(args),
        parent_session_id: None,
        cwd: Some(cwd.to_string_lossy().to_string()),
        tool_call_id: None,
        resumed_from: None,
        // Only the first birth of a run carries the launch anchor.
        launch: resumed.map_or_else(
            || {
                Some(RunLaunchConfig {
                    input_id: format!("input-{name}"),
                    phase_names: (!plan.phase_names.is_empty()).then(|| plan.phase_names.clone()),
                    subagent_model: None,
                    script_path: None,
                    phase_alongside: (!plan.phase_alongside.is_empty())
                        .then(|| plan.phase_alongside.clone()),
                })
            },
            |_| None,
        ),
        inherited_tokens: None,
        imported_cache: None,
    })
    .expect("engine");
    let _ = cell.set(Arc::clone(&engine));
    Harness {
        journal,
        engine,
        run_id,
        cwd,
        world_reads,
    }
}

async fn execute(harness: &Harness, plan: Arc<Plan>, args: Value) -> RunSettlement {
    let mut options = ScriptOptions::new(plan);
    options.args = Some(args);
    interpret::run_script(Arc::clone(&harness.engine), options).await
}

fn events(journal: &InMemoryJournalStore, run_id: &str) -> Vec<RunEvent> {
    journal
        .list_events(run_id, ListEventsOptions::default())
        .into_iter()
        .map(|stored| stored.event)
        .collect()
}

fn node_keys(journal: &InMemoryJournalStore, run_id: &str) -> Vec<String> {
    journal
        .list_nodes(run_id)
        .into_iter()
        .map(|node| format!("{}@{}", node.site_id, node.ordinal))
        .collect()
}

#[tokio::test]
async fn a_linear_nws_script_runs_to_settlement_through_the_real_engine() {
    let plan = compile("nws_linear");
    let answers = HashMap::from([(
        "ask#1".to_string(),
        json!({"steps": ["read", "write", "check"], "rationale": "three steps"}),
    )]);
    let args = json!({"repo": "nc-code"});
    let harness = harness("linear", &plan, args.clone(), answers);
    let settlement = execute(&harness, Arc::clone(&plan), args).await;

    assert_eq!(settlement.status, RunStatus::Completed, "{settlement:?}");
    assert_eq!(
        settlement.artifact,
        Some(json!({
            "steps": ["read", "write", "check"],
            "rationale": "three steps",
            // The world read is a driver call with a known length; the script measured it.
            "readmeLength": 100,
        })),
        "the return value is the run artifact"
    );

    assert_eq!(
        node_keys(&harness.journal, &harness.run_id),
        vec![
            "artifact#1@1",
            "ask#1@1",
            "world-read#1@1",
            "report#1@1",
            "artifact#2@1",
        ],
        "artifact#1 is the board declaration at the top of the body, artifact#2 the publish"
    );

    let phases: Vec<String> = events(&harness.journal, &harness.run_id)
        .into_iter()
        .filter_map(|event| match event {
            RunEvent::PhaseEntered { name, ordinal } => Some(format!("{name}#{ordinal}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        phases,
        ["plan#1", "execute#1"],
        "phase markers reach the journal"
    );

    let events = events(&harness.journal, &harness.run_id);
    let types = events
        .iter()
        .map(RunEvent::type_name)
        .collect::<Vec<&str>>();
    assert_eq!(types.first().copied(), Some("run-started"));
    assert_eq!(types.get(1).copied(), Some("run-launched"));
    for expected in [
        "actor-created",
        "node-queued",
        "node-dispatched",
        "node-settled",
        "report",
        "artifact-published",
        "log",
    ] {
        assert!(
            types.contains(&expected),
            "{expected} missing from {types:?}"
        );
    }
    assert_eq!(types.last().copied(), Some("run-settled"));

    let reports: Vec<Value> = events
        .into_iter()
        .filter_map(|event| match event {
            RunEvent::Report {
                item, artifact_id, ..
            } => Some(json!({"item": item, "artifactId": artifact_id})),
            _ => None,
        })
        .collect();
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0]["artifactId"],
        json!("summary"),
        "the report tag routes to the declared board"
    );
    assert_eq!(reports[0]["item"]["repo"], json!("nc-code"));
    assert_eq!(
        reports[0]["item"]["steps"],
        json!(["read", "write", "check"])
    );
    assert_eq!(reports[0]["item"]["readmeLength"], json!(100));

    // The launch anchor transcribed the compiled phase table, exactly as the TypeScript
    // submit path filled it from the analysis graph.
    let launched = events_with_launch(&harness.journal, &harness.run_id);
    assert_eq!(launched["phaseNames"], json!(["plan", "execute"]));
    assert_eq!(
        launched["phaseAlongside"],
        Value::Null,
        "a straight-line script has no strands running alongside"
    );
}

fn events_with_launch(journal: &InMemoryJournalStore, run_id: &str) -> Value {
    events(journal, run_id)
        .into_iter()
        .find_map(|event| match event {
            RunEvent::RunLaunched {
                phase_names,
                phase_alongside,
                input_id,
                ..
            } => Some(json!({
                "inputId": input_id,
                "phaseNames": phase_names,
                "phaseAlongside": phase_alongside,
            })),
            _ => None,
        })
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn an_nws_run_creates_no_sandbox_child_entry() {
    // The Node sandbox wrote `<cwd>/.zcode/workflow-runs/<runId>.mjs` before every launch.
    // An NWS run executes in-process, so the directory must never appear.
    let plan = compile("nws_linear");
    let answers = HashMap::from([(
        "ask#1".to_string(),
        json!({"steps": ["a"], "rationale": "one"}),
    )]);
    let harness = harness("no-child", &plan, json!({"repo": "x"}), answers);
    let settlement = execute(&harness, Arc::clone(&plan), json!({"repo": "x"})).await;
    assert_eq!(settlement.status, RunStatus::Completed);
    assert!(
        !harness.cwd.join(".zcode").exists(),
        "an NWS run must not create the sandbox entry directory: {:?}",
        harness.cwd
    );
}

#[tokio::test]
async fn loops_reuse_one_site_id_with_journal_ordinals_and_strands_fan_out() {
    let plan = compile("nws_fanout");
    let args = json!({"files": ["one.md", "two.md", "three.md"]});
    let harness = harness("fanout", &plan, args.clone(), HashMap::new());
    let settlement = execute(&harness, Arc::clone(&plan), args).await;

    assert_eq!(settlement.status, RunStatus::Completed, "{settlement:?}");
    assert_eq!(
        settlement.artifact,
        Some(json!({"verdictCount": 3, "noteCount": 3})),
        "`verdicts.length` came from the join, `args.files.length` from the loop"
    );

    let keys = node_keys(&harness.journal, &harness.run_id);
    for expected in [
        // Promise.all: three distinct ask sites, one execution each.
        "ask#1@1", "ask#2@1", "ask#3@1", // The loop body: one site, three ordinals.
        "ask#4@1", "ask#4@2", "ask#4@3",
        // The parallel block: two strands, each with its own site.
        "ask#5@1", "ask#6@1",
    ] {
        assert!(
            keys.contains(&expected.to_string()),
            "{expected} missing from {keys:?}"
        );
    }

    let actors = harness.journal.list_actors(&harness.run_id);
    assert_eq!(
        actors
            .iter()
            .map(|actor| actor.site_id.as_str())
            .collect::<Vec<_>>(),
        ["actor#1", "actor#2", "actor#3", "actor#4"],
        "four actors, in source order"
    );

    let phases: Vec<String> = events(&harness.journal, &harness.run_id)
        .into_iter()
        .filter_map(|event| match event {
            RunEvent::PhaseEntered { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(phases, ["review", "collect", "close"]);

    let reports = events(&harness.journal, &harness.run_id)
        .into_iter()
        .filter(|event| matches!(event, RunEvent::Report { .. }))
        .count();
    assert_eq!(reports, 6, "one review, three loop items, two strands");
    assert_eq!(
        keys.iter().filter(|key| key.starts_with("report#")).count(),
        6,
        "one report node per report site ordinal"
    );
}

#[tokio::test]
async fn a_completed_run_resumes_from_the_journal_without_re_running_the_world() {
    let plan = compile("nws_linear");
    let answers = HashMap::from([(
        "ask#1".to_string(),
        json!({"steps": ["read", "write"], "rationale": "two steps"}),
    )]);
    let args = json!({"repo": "nc-code"});
    let first = harness("resume", &plan, args.clone(), answers.clone());
    let before = execute(&first, Arc::clone(&plan), args.clone()).await;
    assert_eq!(before.status, RunStatus::Completed);
    assert_eq!(
        first.world_reads.lock().expect("world reads").len(),
        1,
        "the first run really read the world"
    );

    // Resume: a second engine over the same journal rows and the same script hash.
    let second = harness_with(
        "resume",
        &plan,
        args.clone(),
        answers,
        HashSet::new(),
        Some(&first),
    );
    let after = execute(&second, Arc::clone(&plan), args).await;
    assert_eq!(after.status, RunStatus::Completed, "{after:?}");
    assert_eq!(
        after.artifact, before.artifact,
        "a resumed run settles with the same artifact"
    );

    let cached: Vec<String> = events(&second.journal, &second.run_id)
        .into_iter()
        .filter_map(|event| match event {
            RunEvent::NodeSettled {
                instance, cached, ..
            } => cached.unwrap_or(false).then(|| instance.key()),
            _ => None,
        })
        .collect();
    assert!(
        cached.contains(&"ask#1@1".to_string()),
        "the replayed ask must be marked cached: {cached:?}"
    );
    assert!(
        cached.contains(&"world-read#1@1".to_string()),
        "the replayed world read must be marked cached: {cached:?}"
    );
    assert!(
        second.world_reads.lock().expect("world reads").is_empty(),
        "every journaled node replayed, so the resumed run must not touch the world again"
    );
}

#[tokio::test]
async fn resuming_a_changed_script_is_refused_by_name() {
    let plan = compile("nws_linear");
    let answers = HashMap::from([(
        "ask#1".to_string(),
        json!({"steps": ["a"], "rationale": "r"}),
    )]);
    let first = harness("hash", &plan, json!({"repo": "x"}), answers.clone());
    execute(&first, Arc::clone(&plan), json!({"repo": "x"})).await;

    // One trailing newline is a different script: the resume check is byte identity.
    let revised = Arc::new(
        Plan::compile(&format!("{}\n", fixture("nws_linear"))).expect("revision compiles"),
    );
    assert_ne!(revised.script_hash, plan.script_hash);
    let cell: Arc<OnceCell<Arc<WorkflowEngine>>> = Arc::new(OnceCell::new());
    let driver = Arc::new(ScriptedDriver {
        journal: Arc::clone(&first.journal),
        engine: Arc::clone(&cell),
        answers,
        silent: HashSet::new(),
        world_reads: Arc::new(Mutex::new(Vec::new())),
        submits: Mutex::new(HashMap::new()),
    });
    let refused = WorkflowEngine::new(EngineConfig {
        run_id: first.run_id.clone(),
        driver,
        caps: Caps { max_concurrency: 4 },
        ask_specs: revised.engine_ask_specs(),
        validate: required_key_validator(),
        script_text: Some(revised.source.clone()),
        name: None,
        script_hash: Some(revised.script_hash.clone()),
        args: None,
        parent_session_id: None,
        cwd: None,
        tool_call_id: None,
        resumed_from: None,
        launch: None,
        inherited_tokens: None,
        imported_cache: None,
    });
    let error = match refused {
        Ok(_) => panic!("a byte-different script cannot resume the same run"),
        Err(error) => error,
    };
    assert_eq!(error.code, WorkflowErrorCode::ScriptHashMismatch, "{error}");
}

#[tokio::test]
async fn a_script_that_never_finishes_hits_the_wall_clock_timeout() {
    let plan = compile("nws_linear");
    let silent = HashSet::from(["ask#1".to_string()]);
    let harness = harness_with(
        "timeout",
        &plan,
        json!({"repo": "x"}),
        HashMap::new(),
        silent,
        None,
    );
    let mut options = ScriptOptions::new(Arc::clone(&plan));
    options.args = Some(json!({"repo": "x"}));
    options.timeout_ms = Some(300);
    let settlement = interpret::run_script(Arc::clone(&harness.engine), options).await;

    assert_eq!(settlement.status, RunStatus::Stopped);
    assert_eq!(settlement.reason, Some(RunStopReason::Interrupted));
    let message = settlement.error.expect("error").message;
    assert!(message.contains("wall-clock timeout"), "{message}");
}

#[tokio::test]
async fn an_ask_rejection_ends_the_run_with_the_engines_own_code() {
    // A typed ask whose answer fails validation is refused until the repair budget runs
    // out. The script cannot catch it, so the interpreter reports the engine's own
    // structured code rather than a generic failure.
    let plan = compile("nws_linear");
    let answers = HashMap::from([("ask#1".to_string(), json!({"steps": "not an array"}))]);
    let harness = harness("reject", &plan, json!({"repo": "x"}), answers);
    let settlement = execute(&harness, Arc::clone(&plan), json!({"repo": "x"})).await;

    assert_eq!(settlement.status, RunStatus::Errored, "{settlement:?}");
    let error = settlement.error.expect("error");
    assert_eq!(
        error.code,
        WorkflowErrorCode::ValidationFailed,
        "the engine's code survives the interpreter: {}",
        error.code.as_str()
    );
    assert!(
        error
            .violations
            .as_ref()
            .is_some_and(|violations| !violations.is_empty()),
        "the violations reach the settlement: {error:?}"
    );
}

#[tokio::test]
async fn a_world_run_command_is_authorized_from_the_compiled_set() {
    let plan = Arc::new(
        Plan::compile(
            r#"workflow "nws-run" {
  phase "build";
  let outcome = await world.run("cargo test", ["-q"]);
  return outcome.stdout;
}"#,
        )
        .expect("compiles"),
    );
    assert_eq!(plan.declared_run_commands, ["cargo test"]);
    let harness = harness("run", &plan, json!({}), HashMap::new());
    let settlement = execute(&harness, Arc::clone(&plan), json!({})).await;
    assert_eq!(settlement.status, RunStatus::Completed, "{settlement:?}");
    assert_eq!(settlement.artifact, Some(json!("ok")));
    let reads = harness.world_reads.lock().expect("world reads");
    assert_eq!(
        reads.as_slice(),
        [("run".to_string(), vec![json!("cargo test"), json!(["-q"])])],
        "the driver sees the literal command the compiler recorded"
    );
}

#[tokio::test]
async fn stopping_the_run_from_outside_returns_instead_of_hanging() {
    // The other half of the settlement race: the run settles while the script is parked on
    // an ask, and the interpreter returns that settlement rather than waiting for an answer
    // that will never come.
    let plan = compile("nws_linear");
    let silent = HashSet::from(["ask#1".to_string()]);
    let harness = harness_with(
        "cancel",
        &plan,
        json!({"repo": "x"}),
        HashMap::new(),
        silent,
        None,
    );
    let engine = Arc::clone(&harness.engine);
    let running_plan = Arc::clone(&plan);
    let run = tokio::spawn(async move {
        let mut options = ScriptOptions::new(running_plan);
        options.args = Some(json!({"repo": "x"}));
        interpret::run_script(engine, options).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    harness.engine.stop(
        RunStopReason::User,
        Some(WorkflowError::new(
            WorkflowErrorCode::Cancelled,
            "cancelled by user",
        )),
        None,
    );
    let settlement = run.await.expect("interpreter task");
    assert_eq!(settlement.status, RunStatus::Stopped);
    assert_eq!(settlement.reason, Some(RunStopReason::User));
}

#[tokio::test]
async fn a_caught_rejection_recovers_and_the_run_still_completes() {
    // The TypeScript facade documented `try { await artifact.file(…) } catch { … }` as the
    // intended recovery idiom, so NWS must have it: a rejected facade call is handed to the
    // catch body as the engine's own structured error, and the run carries on.
    let plan = Arc::new(
        Plan::compile(
            r#"workflow "nws-caught" {
  phase "work";
  let worker = agent("worker");
  let answer = await worker.ask("answer");
  try {
    let status = await git.status();
    report({ readItBack: status });
    return { caught: null, answer: answer };
  } catch (err) {
    report({ code: err.code });
    return { caught: err.code, answer: answer };
  }
}"#,
        )
        .expect("compiles"),
    );
    let harness = harness("caught", &plan, json!({}), HashMap::new());
    let settlement = execute(&harness, Arc::clone(&plan), json!({})).await;

    assert_eq!(settlement.status, RunStatus::Completed, "{settlement:?}");
    let artifact = settlement.artifact.expect("artifact");
    assert_eq!(
        artifact["caught"],
        json!("DriverError"),
        "the catch binding carries the engine's code, not a prose message"
    );
    assert!(
        !artifact["answer"].is_null(),
        "the ask before the try is untouched by the recovery: {artifact}"
    );

    // Only the catch body reported; the try body stopped at the failing call.
    let reports: Vec<Value> = events(&harness.journal, &harness.run_id)
        .into_iter()
        .filter_map(|event| match event {
            RunEvent::Report { item, .. } => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(reports, vec![json!({"code": "DriverError"})]);

    // The rejected world read is a settled-failed node, not a run failure: the script caught
    // it, so the run's own outcome stays completed.
    let failed_node = harness
        .journal
        .list_nodes(&harness.run_id)
        .into_iter()
        .find(|node| node.site_id == "world-read#1")
        .expect("the world read journaled");
    assert_eq!(
        failed_node.status,
        nc_code_workflow::types::NodeRecordStatus::Failed,
        "the node records the rejection the script recovered from"
    );
}

#[tokio::test]
async fn a_return_inside_try_unwinds_the_run_without_running_the_catch_body() {
    // `return` is not an error. If the try body completes (or returns) the catch body must not
    // run, so a successful publish cannot be mistaken for a failure.
    let plan = Arc::new(
        Plan::compile(
            r#"workflow "nws-uncaught" {
  phase "work";
  try {
    let paths = await files.glob("*.md");
    report({ matched: paths.length });
    return { caught: null, matched: paths.length };
  } catch (err) {
    report({ shouldNotRun: err.code });
    return { caught: err.code };
  }
}"#,
        )
        .expect("compiles"),
    );
    let harness = harness("uncaught", &plan, json!({}), HashMap::new());
    let settlement = execute(&harness, Arc::clone(&plan), json!({})).await;

    assert_eq!(settlement.status, RunStatus::Completed, "{settlement:?}");
    assert_eq!(
        settlement.artifact,
        Some(json!({"caught": null, "matched": 2})),
        "the try body's return is the run's output"
    );
    let reports: Vec<Value> = events(&harness.journal, &harness.run_id)
        .into_iter()
        .filter_map(|event| match event {
            RunEvent::Report { item, .. } => Some(item),
            _ => None,
        })
        .collect();
    assert_eq!(
        reports,
        vec![json!({"matched": 2})],
        "the catch body never ran"
    );
}

#[tokio::test]
async fn journaled_node_kinds_match_the_engine_vocabulary() {
    let plan = compile("nws_linear");
    let answers = HashMap::from([(
        "ask#1".to_string(),
        json!({"steps": ["a"], "rationale": "r"}),
    )]);
    let harness = harness("kinds", &plan, json!({"repo": "x"}), answers);
    execute(&harness, Arc::clone(&plan), json!({"repo": "x"})).await;
    let kinds = harness
        .journal
        .list_nodes(&harness.run_id)
        .into_iter()
        .map(|node| node.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            NodeKind::Artifact,
            NodeKind::Ask,
            NodeKind::WorldRead,
            NodeKind::Report,
            NodeKind::Artifact,
        ],
        "each facade call journals the node kind the engine expects"
    );
}
