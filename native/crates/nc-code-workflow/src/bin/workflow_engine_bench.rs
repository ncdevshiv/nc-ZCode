//! Workflow engine benchmark (Rust side of the TS-vs-Rust workflow comparison).
//!
//! Runs the SAME generated fixtures (`tests/fixtures/<name>.json`, produced by
//! `scripts/bench/gen-workflow-fixtures.mjs` through the real TypeScript front-end) that the
//! TS bench (`scripts/bench/workflow-bench.mjs`) runs. Real components on both sides:
//! real Node sandbox child, real NDJSON bridge, real engine, real in-memory journal, real
//! world read from the repository README, real artifact bytes written to disk. Scripted:
//! only the model answers (the same stand-in on both sides).
//!
//! Usage: cargo run -p nc-code-workflow --release --bin workflow_engine_bench -- [runs]
//! Output: JSON {engine:"rust", results:[{fixture, runMedianMs, siteIds, eventCount, ...}]}

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use serde_json::{json, Value as JsonValue};
use tokio::sync::OnceCell;

use nc_code_workflow::engine::{EngineConfig, WorkflowEngine};
use nc_code_workflow::journal_memory::InMemoryJournalStore;
use nc_code_workflow::sandbox::{run_workflow_script, SandboxOptions};
use nc_code_workflow::script::interpret::{self, ScriptOptions};
use nc_code_workflow::script::Plan;
use nc_code_workflow::types::{
    ActorRef, ActorSessionSeed, AskMessage, AskSpec, Caps, InstanceRef, JournalStorePort,
    ListEventsOptions, PersonaSpec, RunEvent, RunSettlement, RunStatus, SessionRef, SubmitVerdict,
    WorkflowDriver, WorkflowError, WorkflowReportSink,
};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    #[allow(dead_code)]
    name: String,
    site_ids: Vec<String>,
    lowered: String,
    ask_specs: HashMap<String, FixtureAskSpec>,
}

#[derive(serde::Deserialize)]
struct FixtureAskSpec {
    typed: bool,
    #[serde(default)]
    schema: Option<JsonValue>,
}

struct BenchDriver {
    journal: Arc<InMemoryJournalStore>,
    engine: Arc<OnceCell<Arc<WorkflowEngine>>>,
    workspace: PathBuf,
    store_dir: PathBuf,
    answers: HashMap<String, JsonValue>,
    world_reads: StdMutex<u64>,
}

#[async_trait::async_trait]
impl WorkflowDriver for BenchDriver {
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
            id: format!("bench-session-{}-{}", actor.site_id, actor.ordinal),
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

    async fn execute_world_read(
        &self,
        op: &str,
        args: Vec<JsonValue>,
    ) -> Result<JsonValue, WorkflowError> {
        *self.world_reads.lock().unwrap() += 1;
        match op {
            "read" => {
                let rel = args.first().and_then(JsonValue::as_str).unwrap_or_default();
                let path = self.workspace.join(rel);
                let text = std::fs::read_to_string(&path).map_err(|error| {
                    WorkflowError::new(
                        nc_code_workflow::types::WorkflowErrorCode::DriverError,
                        format!("world read failed: {}: {error}", path.display()),
                    )
                })?;
                Ok(JsonValue::String(text))
            }
            other => Err(WorkflowError::new(
                nc_code_workflow::types::WorkflowErrorCode::DriverError,
                format!("bench driver: unsupported world op {other}"),
            )),
        }
    }

    async fn execute_artifact_publish(
        &self,
        request: nc_code_workflow::types::ArtifactPublishRequest,
    ) -> Result<nc_code_workflow::types::ArtifactVersionRecord, WorkflowError> {
        let file = self
            .store_dir
            .join(format!("{}-v{}.md", request.id, request.version));
        let content = request.content.unwrap_or_default();
        std::fs::write(&file, &content).map_err(|error| {
            WorkflowError::new(
                nc_code_workflow::types::WorkflowErrorCode::DriverError,
                format!("artifact store write failed: {error}"),
            )
        })?;
        Ok(nc_code_workflow::types::ArtifactVersionRecord {
            id: request.id.clone(),
            kind: request.op,
            version: request.version,
            title: None,
            description: None,
            content_type: Some("text/markdown".to_string()),
            bytes: Some(content.len() as u64),
            uri: Some(format!("zcode-artifact://bench/{}", request.id)),
            source_path: None,
            spec: None,
            published_at: Some(0.0),
            primary: None,
        })
    }

    fn dispose(&self) {}
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[mid - 1] + samples[mid]) / 2.0
    } else {
        samples[mid]
    }
}

fn repo_root() -> PathBuf {
    // native/crates/nc-code-workflow -> repo root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("repo root")
        .to_path_buf()
}

async fn run_once(
    fixture: &Fixture,
    workspace: &std::path::Path,
    answers: &HashMap<String, JsonValue>,
    run_id: &str,
) -> (RunSettlement, u64, Arc<InMemoryJournalStore>) {
    let journal = Arc::new(InMemoryJournalStore::new());
    let cell: Arc<OnceCell<Arc<WorkflowEngine>>> = Arc::new(OnceCell::new());
    let store_dir = std::env::temp_dir().join(format!("zcode-wf-bench-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&store_dir).expect("store dir");
    let cwd = std::env::temp_dir().join(format!("zcode-wf-bench-cwd-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).expect("cwd");
    let driver = Arc::new(BenchDriver {
        journal: journal.clone(),
        engine: cell.clone(),
        workspace: workspace.to_path_buf(),
        store_dir,
        answers: answers.clone(),
        world_reads: StdMutex::new(0),
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
    let engine = WorkflowEngine::new(EngineConfig {
        run_id: run_id.to_string(),
        driver,
        caps: Caps { max_concurrency: 4 },
        ask_specs,
        validate: Arc::new(|_schema: Option<&JsonValue>, _value: &JsonValue| Vec::new()),
        script_text: None,
        name: None,
        script_hash: Some("bench".to_string()),
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
    let options = SandboxOptions::new(
        run_id,
        fixture.lowered.clone(),
        cwd.to_string_lossy().to_string(),
    );
    let settlement = run_workflow_script(engine, options).await;
    let event_count = journal
        .list_events(run_id, ListEventsOptions::default())
        .len() as u64;
    (settlement, event_count, journal)
}

/// Run one NWS plan through the same engine, driver, journal and caps the sandbox row uses.
/// The only difference is the script runtime: the plan is walked in-process, so no child is
/// spawned and no entry file is written.
async fn run_nws_once(
    plan: &Arc<Plan>,
    workspace: &std::path::Path,
    answers: &HashMap<String, JsonValue>,
    run_id: &str,
) -> (RunSettlement, u64, Arc<InMemoryJournalStore>) {
    let journal = Arc::new(InMemoryJournalStore::new());
    let cell: Arc<OnceCell<Arc<WorkflowEngine>>> = Arc::new(OnceCell::new());
    let store_dir = std::env::temp_dir().join(format!("zcode-nws-bench-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&store_dir).expect("store dir");
    let cwd = std::env::temp_dir().join(format!("zcode-nws-bench-cwd-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).expect("cwd");
    let driver = Arc::new(BenchDriver {
        journal: Arc::clone(&journal),
        engine: Arc::clone(&cell),
        workspace: workspace.to_path_buf(),
        store_dir,
        answers: answers.clone(),
        world_reads: StdMutex::new(0),
    });
    let engine = WorkflowEngine::new(EngineConfig {
        run_id: run_id.to_string(),
        driver: Arc::clone(&driver) as Arc<dyn WorkflowDriver>,
        caps: Caps { max_concurrency: 4 },
        ask_specs: plan.engine_ask_specs(),
        validate: Arc::new(|_schema: Option<&JsonValue>, _value: &JsonValue| Vec::new()),
        script_text: Some(plan.source.clone()),
        name: Some(plan.workflow_name.clone()),
        script_hash: Some(plan.script_hash.clone()),
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
    let _ = cell.set(Arc::clone(&engine));
    let mut options = ScriptOptions::new(Arc::clone(plan));
    options.args = None;
    let settlement = interpret::run_script(engine, options).await;
    let event_count = journal
        .list_events(run_id, ListEventsOptions::default())
        .len() as u64;
    (settlement, event_count, journal)
}

fn answers_for(fixture: &str) -> HashMap<String, JsonValue> {
    match fixture {
        "workflow_linear" => HashMap::from([(
            "ask#1".to_string(),
            json!({"steps": ["s1", "s2", "s3"], "rationale": "bench rationale"}),
        )]),
        "workflow_fanout" => HashMap::from([(
            "ask#2".to_string(),
            json!({"merged": "merged findings", "count": 4}),
        )]),
        other => panic!("no scripted answers for fixture {other}"),
    }
}

/// The NWS mirrors of the two TypeScript fixtures get the same scripted model answers the
/// TS bench records for them, keyed by the site ids the NWS compiler assigned.
fn answers_for_nws(fixture: &str) -> HashMap<String, JsonValue> {
    match fixture {
        "bench_linear" => HashMap::from([(
            "ask#1".to_string(),
            json!({"steps": ["s1", "s2", "s3"], "rationale": "bench rationale"}),
        )]),
        "bench_fanout" => HashMap::from([(
            "ask#5".to_string(),
            json!({"merged": "merged findings", "count": 4}),
        )]),
        other => panic!("no scripted answers for NWS fixture {other}"),
    }
}

/// Engine-only overhead: drive the host API surface directly (no sandbox child), the same
/// call sequence on both runtimes: createRun + createActor + typed ask + report + complete.
/// This isolates the state machine + journal + event recording from Node startup, which
/// dominates the full-run rows on both sides.
async fn engine_only_median(runs: usize) -> f64 {
    let workspace = repo_root();
    let mut samples = Vec::with_capacity(runs);
    for i in 0..runs {
        let journal = Arc::new(InMemoryJournalStore::new());
        let cell: Arc<OnceCell<Arc<WorkflowEngine>>> = Arc::new(OnceCell::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = Arc::new(ChannelAskDriver {
            journal: journal.clone(),
            started: tx,
            workspace: workspace.clone(),
        });
        let mut specs = HashMap::new();
        specs.insert(
            "ask#1".to_string(),
            AskSpec {
                typed: false,
                schema: None,
            },
        );
        let started_at = Instant::now();
        let engine = WorkflowEngine::new(EngineConfig {
            run_id: format!("engine-only-{i}"),
            driver,
            caps: Caps { max_concurrency: 4 },
            ask_specs: specs,
            validate: Arc::new(|_schema: Option<&JsonValue>, _value: &JsonValue| Vec::new()),
            script_text: None,
            name: None,
            script_hash: Some("bench".to_string()),
            args: None,
            parent_session_id: None,
            cwd: None,
            tool_call_id: None,
            resumed_from: None,
            launch: None,
            inherited_tokens: None,
            imported_cache: None,
        })
        .expect("engine");
        let _ = cell.set(engine.clone());
        let actor = engine
            .create_actor("actor#1", Some("bench".into()), None)
            .unwrap();
        let ask = engine.ask("ask#1", &actor, "bench ask");
        let (instance, _message) = rx.recv().await.expect("start_ask");
        engine.ask_turn_ended(&instance, "bench final text");
        let _ = ask.await.expect("ask settles");
        engine.report("report#1", json!({"n": i}), None);
        engine.complete(json!({"done": true}));
        let settlement = engine.settled().await;
        assert_eq!(settlement.status, RunStatus::Completed);
        samples.push(started_at.elapsed().as_secs_f64() * 1000.0);
    }
    median(&mut samples)
}

struct ChannelAskDriver {
    journal: Arc<InMemoryJournalStore>,
    started: tokio::sync::mpsc::UnboundedSender<(InstanceRef, AskMessage)>,
    workspace: PathBuf,
}

#[async_trait::async_trait]
impl WorkflowDriver for ChannelAskDriver {
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
            id: format!("engine-only-{}", actor.key()),
        })
    }
    fn start_ask(&self, _session: &SessionRef, instance: &InstanceRef, message: AskMessage) {
        let _ = self.started.send((instance.clone(), message));
    }
    fn respond_to_submit(&self, _instance: &InstanceRef, _verdict: SubmitVerdict) {}
    fn cancel_ask(&self, _instance: &InstanceRef) {}
    async fn execute_world_read(
        &self,
        op: &str,
        args: Vec<JsonValue>,
    ) -> Result<JsonValue, WorkflowError> {
        if op == "read" {
            let rel = args.first().and_then(JsonValue::as_str).unwrap_or_default();
            return std::fs::read_to_string(self.workspace.join(rel))
                .map(JsonValue::String)
                .map_err(|error| {
                    WorkflowError::new(
                        nc_code_workflow::types::WorkflowErrorCode::DriverError,
                        format!("world read failed: {error}"),
                    )
                });
        }
        Err(WorkflowError::new(
            nc_code_workflow::types::WorkflowErrorCode::DriverError,
            "engine-only driver: unsupported world op",
        ))
    }
}

/// The first compile in this process, which is what an author's first `CreateWorkflow`
/// pays: lexer, parser, resolver and lowering, with no resident helper to start.
fn nws_cold_compile_ms() -> f64 {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bench_linear.nws");
    let text = std::fs::read_to_string(&path).expect("NWS fixture");
    let started = Instant::now();
    Plan::compile(&text).expect("NWS fixture compiles");
    (started.elapsed().as_secs_f64() * 1000.0 * 1000.0).round() / 1000.0
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let runs: usize = args
        .get(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);
    let warmup = 3usize;
    let workspace = repo_root();
    let fixtures_dir = workspace.join("native/crates/nc-code-workflow/tests/fixtures");
    let mut results = Vec::new();
    for name in ["workflow_linear", "workflow_fanout"] {
        let path = fixtures_dir.join(format!("{name}.json"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "missing fixture {} ({error}); run `node scripts/bench/gen-workflow-fixtures.mjs`",
                path.display()
            )
        });
        let fixture: Fixture = serde_json::from_str(&text).expect("fixture json");
        let answers = answers_for(name);
        for i in 0..warmup {
            let (settlement, _, _) =
                run_once(&fixture, &workspace, &answers, &format!("warm-{name}-{i}")).await;
            assert_eq!(
                settlement.status,
                RunStatus::Completed,
                "{name} warmup must complete"
            );
        }
        let mut samples = Vec::with_capacity(runs);
        let mut last_events = 0u64;
        let mut last_journal = None;
        let mut last_run_id = String::new();
        for i in 0..runs {
            let run_id = format!("bench-{name}-{i}");
            let started = Instant::now();
            let (settlement, event_count, journal) =
                run_once(&fixture, &workspace, &answers, &run_id).await;
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(
                settlement.status,
                RunStatus::Completed,
                "{name} run {i} must complete"
            );
            samples.push(elapsed);
            last_events = event_count;
            last_journal = Some(journal);
            last_run_id = run_id;
        }
        // Journal read cost after a settled run: full event page + run row.
        let journal = last_journal.expect("at least one run");
        let read_started = Instant::now();
        let events = journal.list_events(&last_run_id, ListEventsOptions::default());
        let row = journal.get_run(&last_run_id).expect("run row");
        let journal_read_ms = read_started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(row.status, RunStatus::Completed);
        results.push(json!({
            "fixture": name,
            "runtime": "node-sandbox",
            "siteIds": fixture.site_ids,
            "runs": runs,
            "runMedianMs": (median(&mut samples) * 1000.0).round() / 1000.0,
            "runMinMs": (samples.first().copied().unwrap_or(0.0) * 1000.0).round() / 1000.0,
            "eventCount": last_events,
            "journalReadMs": (journal_read_ms * 1000.0).round() / 1000.0,
            "eventsRead": events.len(),
        }));
    }
    // Compile rows for both front-ends, collected alongside the run rows.
    let mut compile_results = Vec::new();

    // Same scenarios on the Rust-native script runtime: the plan is walked in-process, so
    // these rows are the counterpart of the sandbox rows above with the engine, journal,
    // caps and scripted answers held identical.
    for name in ["bench_linear", "bench_fanout"] {
        let path = fixtures_dir.join(format!("{name}.nws"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("missing NWS fixture {} ({error})", path.display()));
        let plan = Arc::new(Plan::compile(&text).unwrap_or_else(|diagnostics| {
            panic!(
                "{name} must compile: {}",
                diagnostics
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }));
        let answers = answers_for_nws(name);
        for i in 0..warmup {
            let (settlement, _, _) =
                run_nws_once(&plan, &workspace, &answers, &format!("warm-{name}-{i}")).await;
            assert_eq!(
                settlement.status,
                RunStatus::Completed,
                "{name} NWS warmup must complete"
            );
        }
        let mut samples = Vec::with_capacity(runs);
        let mut last_events = 0u64;
        let mut last_journal = None;
        let mut last_run_id = String::new();
        for i in 0..runs {
            let run_id = format!("nwsbench-{name}-{i}");
            let started = Instant::now();
            let (settlement, event_count, journal) =
                run_nws_once(&plan, &workspace, &answers, &run_id).await;
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(
                settlement.status,
                RunStatus::Completed,
                "{name} NWS run {i} must complete"
            );
            samples.push(elapsed);
            last_events = event_count;
            last_journal = Some(journal);
            last_run_id = run_id;
        }
        let journal = last_journal.expect("at least one run");
        let read_started = Instant::now();
        let events = journal.list_events(&last_run_id, ListEventsOptions::default());
        let row = journal.get_run(&last_run_id).expect("run row");
        let journal_read_ms = read_started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(row.status, RunStatus::Completed);
        results.push(json!({
            "fixture": name,
            "runtime": "nws-interpreter",
            "siteIds": plan.site_ids(),
            "runs": runs,
            "runMedianMs": (median(&mut samples) * 1000.0).round() / 1000.0,
            "runMinMs": (samples.first().copied().unwrap_or(0.0) * 1000.0).round() / 1000.0,
            "eventCount": last_events,
            "journalReadMs": (journal_read_ms * 1000.0).round() / 1000.0,
            "eventsRead": events.len(),
            "childProcess": false,
        }));
    }

    // Compile rows for the Rust-native front-end: parse + resolve + lower in this process,
    // measured the same way the kernel row measures one warm compile.
    let mut nws_compile_samples = Vec::with_capacity(runs);
    for name in ["bench_linear", "bench_fanout"] {
        let text =
            std::fs::read_to_string(fixtures_dir.join(format!("{name}.nws"))).expect("NWS fixture");
        for _ in 0..(runs / 2) {
            let started = Instant::now();
            Plan::compile(&text).expect("NWS fixture compiles");
            nws_compile_samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    compile_results.push(json!({
        "runtime": "nws-in-process",
        "coldFirstCompileMs": nws_cold_compile_ms(),
        "warmCompileMedianMs": (median(&mut nws_compile_samples) * 1000.0).round() / 1000.0,
        "needsNode": false,
    }));

    // Authoring-surface row: the cost of the preview card's projection on top of a compile. This is
    // the operation `CreateWorkflow` performs per script, so it belongs beside the compile rows.
    // The TypeScript baseline for the same question is `analyze` through the resident kernel, and
    // specs/tool-benchmark.md carries it as a frozen pre-cutover number (warm 59 ms, one-time spawn
    // 390 ms) that cannot be re-measured here; it is also not the same operation — that analyze runs
    // a JavaScript taint analysis, which is precisely the work this tree walk replaces.
    {
        use nc_code_workflow::script::graph::project_analysis;
        let mut samples: Vec<f64> = Vec::with_capacity(runs * 2);
        let mut step_counts: Vec<usize> = Vec::with_capacity(runs * 2);
        for name in ["bench_linear", "bench_fanout"] {
            let text =
                std::fs::read_to_string(fixtures_dir.join(format!("{name}.nws"))).expect("fixture");
            for _ in 0..runs {
                let started = Instant::now();
                let plan = Plan::compile(&text).expect("NWS fixture compiles");
                let analysis = project_analysis(&plan);
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                step_counts.push(
                    analysis["causality"]["steps"]
                        .as_array()
                        .map_or(0, |steps| steps.len()),
                );
            }
        }
        // The wide case is what the caps are for: one lane, seventy awaits.
        let mut wide = String::from("workflow \"wide\" {\n  let a = agent(\"a\");\n");
        for round in 1..=70 {
            wide.push_str(&format!(
                "  let x{round} = await a.ask(\"question {round}\");\n"
            ));
        }
        wide.push_str("}\n");
        let wide_started = Instant::now();
        let wide_plan = Plan::compile(&wide).expect("wide compiles");
        let wide_analysis = project_analysis(&wide_plan);
        let wide_ms = wide_started.elapsed().as_secs_f64() * 1000.0;
        compile_results.push(json!({
            "runtime": "nws-authoring-projection",
            "fixtures": ["bench_linear", "bench_fanout"],
            "stepsPerFixture": step_counts,
            // `median` sorts in place, so the minimum is read after it, never from the raw sample
            // order: the first sample of a fresh process is a cold cache, not a floor.
            "compilePlusProjectMedianMs": (median(&mut samples) * 1000.0).round() / 1000.0,
            "compilePlusProjectMinMs": (samples.first().copied().unwrap_or(0.0) * 1000.0).round()
                / 1000.0,
            "wideSeventyAwaitsMs": (wide_ms * 1000.0).round() / 1000.0,
            "wideSteps": wide_analysis["causality"]["steps"]
                .as_array()
                .map_or(0, |steps| steps.len()),
            "needsNode": false,
        }));
    }

    // Compile-once row: the Rust host drives the SAME TypeScript compiler through a
    // persistent Node kernel (specs/workflow-capability-boundary.md states this explicitly).
    // Spawn cost and warm per-compile cost are reported separately, never blended into the
    // run numbers.
    if node_available() {
        let script_text =
            std::fs::read_to_string(fixtures_dir.join("workflow_linear.ts")).unwrap_or_default();
        let spawn_started = Instant::now();
        match nc_code_workflow::compile::CompileKernel::spawn().await {
            Ok(kernel) => {
                let spawn_ms = spawn_started.elapsed().as_secs_f64() * 1000.0;
                for _ in 0..3 {
                    let _ = kernel
                        .compile(&format!(
                            "{script_text}
// warm {}",
                            uuid::Uuid::new_v4()
                        ))
                        .await;
                }
                let mut samples = Vec::new();
                for _ in 0..10 {
                    // Cache-busting suffix per sample: the kernel cache is real and must not
                    // hide the compiler cost being measured.
                    let variant = format!(
                        "{script_text}
// bench {}",
                        uuid::Uuid::new_v4()
                    );
                    let started = Instant::now();
                    let compiled = kernel.compile(&variant).await;
                    samples.push(started.elapsed().as_secs_f64() * 1000.0);
                    assert!(compiled.is_ok(), "kernel compile must succeed");
                }
                compile_results.push(json!({
                    "row": "compile_once (kernel, warm)",
                    "medianMs": (median(&mut samples) * 1000.0).round() / 1000.0,
                    "spawnMs": (spawn_ms * 1000.0).round() / 1000.0,
                }));
                let _ = kernel.shutdown().await;
            }
            Err(error) => {
                compile_results.push(json!({"row": "compile_once", "skipped": error.to_string()}));
            }
        }
    }
    let engine_only_ms = engine_only_median(200).await;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "engine": "rust",
            "results": results,
            "compile": compile_results,
            "engineOnly": {
                "row": "engine_only (createRun+actor+ask+report+complete)",
                "runs": 200,
                "medianMs": (engine_only_ms * 1000.0).round() / 1000.0,
            },
        }))
        .expect("json")
    );
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
