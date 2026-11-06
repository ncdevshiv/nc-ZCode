//! The production port: folds the run service, the resident compile kernel and the transient snippet execution surface into one
//! [`WorkflowToolPort`] implementation.
//!
//! The division of labour across the three sources:
//! - **run service** (`WorkflowService`): submit / amend / resume plus every observation-surface read model. The tool's
//!   narrow interface does not contain `cancel` / `wait_settlement` — the model surface has neither action (cancellation goes through TaskStop, waiting goes
//!   through TaskOutput / notifications).
//! - **resident compile kernel** (`CompileKernel`): `analyze` gives the authoring-surface tools diagnostics and the declared phase table. The service's own
//!   compiler only serves run submission (`compile_once` is private), and the tool layer must not borrow it before submission — that would create a run
//!   inside a call that only wanted a typecheck.
//! - **transient snippet execution surface**: real compile + real lowering + real sandbox + real world-read, but the journal is
//!   an in-memory implementation, runId is random, no `dwf_*` rows are written and it never enters the background task tracker (the three invariants of the TS snippet service).
//!
//! ## The host handle is a weak reference
//!
//! The port is held by the registry, the registry is held by `HostServer`, and the port needs the host to assemble the driver; storing
//! `Weak<HostServer>` is what keeps this chain from becoming a second permanent cycle (the service itself already holds a strong reference, see
//! `service.rs`'s constructor parameters). The host exists later than the port inside `HostServer::new`, so this is an attach and not a
//! constructor parameter.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use nc_code_workflow::compile::{CompileDiagnostic, CompileKernel};
use nc_code_workflow::engine::{EngineConfig, WorkflowEngine};
use nc_code_workflow::journal_memory::InMemoryJournalStore;
use nc_code_workflow::sandbox::{self, SandboxOptions};
use nc_code_workflow::types::{AskSpec, ImportedRunCache};
use nc_code_workflow::types::{
    Caps, JournalStorePort, RunEvent, RunStatus, ValidateFn, Violation, WorkflowDriver,
};
use serde_json::Value;

use crate::tools::workflow::{ScriptAnalysis, SnippetOutcome, SnippetRequest, WorkflowToolPort};
use crate::workflow::{HostWorkflowDriver, HostWorkflowDriverConfig};
use crate::workflow_service::{
    AmendRequest, AmendResult, EventPage, ResolveQuestionResult, ResumeResult, RunListItem,
    RunSnapshot, SubmitRequest, SubmitResult, WorkflowService, WorkflowServiceError,
};
use crate::HostServer;

/// The snippet's run id prefix (the same as TS `dwfeval-`; it enters the sandbox entry file name, so the character set is safe).
const SNIPPET_RUN_ID_PREFIX: &str = "dwfeval";
const SNIPPET_MAX_LOGS: usize = 100;
const SNIPPET_MAX_LOG_CHARS: usize = 2_048;

pub struct HostWorkflowPort {
    data_dir: PathBuf,
    host: OnceLock<Weak<HostServer>>,
    service: OnceLock<Arc<WorkflowService>>,
    /// The compile kernel reserved for the tool layer: only spawned on the first analyze / snippet compile; if it crashes the next request restarts it.
    kernel: tokio::sync::Mutex<Option<Arc<CompileKernel>>>,
    id_counter: AtomicU64,
}

impl HostWorkflowPort {
    /// `data_dir` is the host directory for the artifact store and the run journal; `workflow_cwd` comes from the session working
    /// directory carried by each individual tool call, so it is not stored here.
    pub fn new(data_dir: PathBuf, _workflow_cwd: PathBuf) -> Self {
        Self {
            data_dir,
            host: OnceLock::new(),
            service: OnceLock::new(),
            kernel: tokio::sync::Mutex::new(None),
            id_counter: AtomicU64::new(1),
        }
    }

    /// Binds the host (`HostServer::new` calls this once the server's Arc exists; OnceLock only accepts the first set).
    pub fn attach(&self, host: Weak<HostServer>) {
        let _ = self.host.set(host);
    }

    /// Binds the run service. When the journal cannot be opened this is **not called**: the tools then report a visible capability
    /// absence instead of receiving a service that silently drops persistence.
    pub fn set_service(&self, service: Arc<WorkflowService>) {
        let _ = self.service.set(service);
    }

    fn host(&self) -> Result<Arc<HostServer>, String> {
        self.host
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| "this workflow tool port is not attached to a host".to_string())
    }

    /// The bound run service, when the journal opened successfully.
    pub fn service_handle(&self) -> Option<Arc<WorkflowService>> {
        self.service.get().cloned()
    }

    fn service(&self) -> Result<Arc<WorkflowService>, WorkflowServiceError> {
        self.service.get().cloned().ok_or_else(|| {
            WorkflowServiceError::Internal(
                "the workflow run journal could not be opened by this host, so no run is reachable"
                    .to_string(),
            )
        })
    }

    async fn kernel(&self) -> Result<Arc<CompileKernel>, String> {
        let mut guard = self.kernel.lock().await;
        if let Some(kernel) = guard.as_ref() {
            return Ok(Arc::clone(kernel));
        }
        let kernel = Arc::new(
            CompileKernel::spawn()
                .await
                .map_err(|error| error.to_string())?,
        );
        *guard = Some(Arc::clone(&kernel));
        Ok(kernel)
    }

    fn next_id(&self, prefix: &str) -> String {
        crate::store::generate_id(prefix, &self.id_counter)
    }

    /// The transient snippet: compile once → the same lowering / sandbox / world-read surface → an in-memory journal → synchronous settlement.
    async fn run_snippet(&self, request: SnippetRequest) -> Result<SnippetOutcome, String> {
        let host = self.host()?;
        let kernel = self.kernel().await?;
        let compiled = match kernel.compile(&request.code).await {
            Ok(compiled) => compiled,
            Err(nc_code_workflow::compile::CompileError::Diagnostics(diagnostics)) => {
                return Ok(SnippetOutcome::Diagnostics(diagnostics));
            }
            Err(nc_code_workflow::compile::CompileError::Kernel(message)) => return Err(message),
        };
        // The scratch facade has no agent(): an ask appearing in the site table can only mean a workflow script was submitted as
        // a snippet. Running it would let it spin up a whole run orchestration — refuse loudly here instead of running it on its behalf.
        if !compiled.ask_specs.is_empty() {
            return Ok(SnippetOutcome::Failed {
                code: "SnippetFacadeViolation".to_string(),
                message: "This snippet uses agent(), which the snippet facade does not provide. Snippets run world reads (files.*, git.*, log) and pure logic; use CreateWorkflow for anything that spawns subagents. Nothing was executed.".to_string(),
                logs: Vec::new(),
                logs_truncated: false,
            });
        }
        let Some(lowered) = compiled.lowered.clone() else {
            return Err("the snippet compiled without a lowered body".to_string());
        };
        let Some(parent) = host.load_session(&request.session_id) else {
            return Err(format!(
                "session {} does not exist in this host; a snippet needs it as its parent",
                request.session_id
            ));
        };

        let run_id = self.next_id(SNIPPET_RUN_ID_PREFIX);
        let journal: Arc<dyn JournalStorePort> = Arc::new(InMemoryJournalStore::new());
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, Value)>();
        let declared: BTreeSet<String> = compiled.declared_run_commands.iter().cloned().collect();
        let driver = HostWorkflowDriver::new(
            HostWorkflowDriverConfig {
                run_id: run_id.clone(),
                data_dir: self.data_dir.clone(),
                // The authorization surface for world.run: the compile-time literal set (the same fail-closed rule as a production run).
                declared_run_commands: Some(declared),
                events: Some(events_tx),
            },
            Arc::clone(&host),
            parent,
            Arc::clone(&journal),
        );
        // The snippet's ask set is excluded at compile time, so the validator is unreachable; injecting a version that **fails loudly**
        // is defence in depth, not a silent pass-through empty implementation.
        let validate: ValidateFn = Arc::new(|_schema: Option<&Value>, _value: &Value| {
            vec![Violation {
                path: "$".to_string(),
                expected: "a snippet without asks".to_string(),
                got: "an ask whose schema needs validation".to_string(),
            }]
        });
        let ask_specs: HashMap<String, AskSpec> = HashMap::new();
        let engine = WorkflowEngine::new(EngineConfig {
            run_id: run_id.clone(),
            driver: Arc::clone(&driver) as Arc<dyn WorkflowDriver>,
            caps: Caps {
                max_concurrency: self.concurrency_ceiling().unwrap_or(1),
            },
            ask_specs,
            validate,
            script_text: Some(request.code.clone()),
            name: None,
            script_hash: Some(compiled.script_hash.clone()),
            args: None,
            parent_session_id: Some(request.session_id.clone()),
            cwd: Some(request.cwd.clone()),
            tool_call_id: None,
            resumed_from: None,
            launch: None,
            inherited_tokens: None,
            imported_cache: None as Option<ImportedRunCache>,
        })
        .map_err(|error| error.to_string())?;

        let mut options = SandboxOptions::new(run_id, lowered, request.cwd);
        // The wall-clock timeout is enforced by the harness (it kills the child process on the dot and settles stopped(interrupted)), the same semantics as the TS
        // snippet timeout.
        options.timeout_ms = Some(request.timeout_ms);
        let settlement = sandbox::run_workflow_script(engine, options).await;

        let (logs, logs_truncated) = drain_snippet_logs(&mut events_rx);
        match settlement.status {
            RunStatus::Completed => Ok(SnippetOutcome::Completed {
                artifact: settlement.artifact,
                logs,
                logs_truncated,
            }),
            RunStatus::Errored => Ok(SnippetOutcome::Failed {
                code: settlement
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str().to_string())
                    .unwrap_or_else(|| "DriverError".to_string()),
                message: settlement
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "the snippet failed".to_string()),
                logs,
                logs_truncated,
            }),
            _ => Ok(SnippetOutcome::Failed {
                code: settlement
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str().to_string())
                    .unwrap_or_else(|| "Cancelled".to_string()),
                message: settlement
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "snippet evaluation was cancelled".to_string()),
                logs,
                logs_truncated,
            }),
        }
    }
}

/// Drains the event channel and keeps only the tail of the `log` events (the count × per-entry length is bounded, and truncation must speak up).
fn drain_snippet_logs(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, Value)>,
) -> (Vec<String>, bool) {
    let mut logs: Vec<String> = Vec::new();
    let mut truncated = false;
    while let Ok((_, value)) = rx.try_recv() {
        let Ok(event) = serde_json::from_value::<RunEvent>(value) else {
            continue;
        };
        let RunEvent::Log { message } = event else {
            continue;
        };
        if logs.len() >= SNIPPET_MAX_LOGS {
            truncated = true;
            continue;
        }
        let clipped: String = message.chars().take(SNIPPET_MAX_LOG_CHARS).collect();
        if clipped.chars().count() < message.chars().count() {
            truncated = true;
        }
        logs.push(clipped);
    }
    (logs, truncated)
}

#[async_trait]
impl WorkflowToolPort for HostWorkflowPort {
    fn concurrency_ceiling(&self) -> Option<u32> {
        self.service
            .get()
            .map(|service| service.concurrency_ceiling())
    }

    /// Analyze a script for the authoring surface: diagnostics plus the declared phase
    /// table. Routed by the script's own language, so an NWS script is answered by the Rust
    /// compiler and never reaches the Node kernel — an author creating an NWS workflow needs
    /// no Node executable for the check, the submit, or the run.
    async fn analyze(&self, script: &str) -> Result<ScriptAnalysis, String> {
        if crate::workflow_service::detect_language(script)
            == crate::workflow_service::WorkflowLanguage::Nws
        {
            return Ok(nws_analysis(script));
        }
        let kernel = self.kernel().await?;
        let value = kernel
            .analyze(script)
            .await
            .map_err(|error| error.to_string())?;
        Ok(parse_analysis(&value))
    }

    async fn submit(&self, request: SubmitRequest) -> Result<SubmitResult, WorkflowServiceError> {
        self.service()?.submit(request).await
    }

    async fn amend(&self, request: AmendRequest) -> Result<AmendResult, WorkflowServiceError> {
        self.service()?.amend(request).await
    }

    async fn resume(&self, run_id: &str) -> Result<ResumeResult, WorkflowServiceError> {
        self.service()?.resume(run_id).await
    }

    async fn eval_snippet(&self, request: SnippetRequest) -> Result<SnippetOutcome, String> {
        self.run_snippet(request).await
    }

    fn resolve_question(&self, qid: &str, answer: &str) -> ResolveQuestionResult {
        match self.service.get() {
            Some(service) => service.resolve_question(qid, answer),
            // The same wording as the service's "unknown qid" (no guessing whether it lives under another process's name).
            None => ResolveQuestionResult::Refused {
                qid: qid.to_string(),
                reason: crate::workflow_service::QuestionRefusal::UnknownQuestion,
                message: "Unknown question id. This session cannot reach the workflow run `service` — workflow execution is not available here, so no parked question is reachable.".to_string(),
            },
        }
    }

    fn list_runs(
        &self,
        session_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<RunListItem>, String> {
        let service = self.service().map_err(|error| error.to_string())?;
        Ok(service.list_runs_limited(session_id, limit))
    }

    fn get_run(&self, run_id: &str) -> Result<Option<RunSnapshot>, String> {
        let service = self.service().map_err(|error| error.to_string())?;
        Ok(service.get_run(run_id))
    }

    fn run_row(&self, run_id: &str) -> Result<Option<RunListItem>, String> {
        let service = self.service().map_err(|error| error.to_string())?;
        // The only public way to look up a single run row is enumeration (`limit` is clamped to 64 on the service side): first the full window, then
        // the run's own session window. When neither has it, return None and the detail page falls back to the event time and the runId.
        let mut rows = service.list_runs_limited(None, Some(usize::MAX));
        if let Some(found) = rows.iter().find(|row| row.run_id == run_id).cloned() {
            return Ok(Some(found));
        }
        if let Some(snapshot) = service.get_run(run_id) {
            if let Some(session) = snapshot.parent_session_id.as_deref() {
                rows = service.list_runs_limited(Some(session), Some(usize::MAX));
                if let Some(found) = rows.iter().find(|row| row.run_id == run_id).cloned() {
                    return Ok(Some(found));
                }
            }
        }
        Ok(None)
    }

    fn run_nodes(&self, run_id: &str) -> Result<Vec<nc_code_workflow::types::NodeRecord>, String> {
        let service = self.service().map_err(|error| error.to_string())?;
        Ok(service.run_nodes(run_id))
    }

    fn workflow_runs_state(&self, session_id: &str) -> Option<Value> {
        self.host().ok()?.workflow_runs_state(session_id)
    }

    fn list_events(
        &self,
        run_id: &str,
        after_sequence: Option<u64>,
        limit: Option<usize>,
    ) -> Result<EventPage, String> {
        let service = self.service().map_err(|error| error.to_string())?;
        Ok(service.list_events(run_id, after_sequence, limit))
    }

    fn is_live(&self, run_id: &str) -> bool {
        self.service
            .get()
            .map(|service| service.is_live(run_id))
            .unwrap_or(false)
    }
}

/// The JSON of the `analyze` inner kernel → the tool-layer subset. The diagnostic shape matches `CompileDiagnostic` field by field (code /
/// line / column / message), and the phase table comes from the **bounded causality graph** (TS's `createWorkflowPhaseNames` /
/// `createWorkflowPhaseAlongside` read exactly this graph): unnamed phases are skipped, cut to 32, and alongside lands
/// in the same table's index space.
/// The authoring analysis of an NWS script, read off the same plan the run executes.
///
/// The display graph comes from `nc_code_workflow::script::graph`, which derives it from the lowered
/// tree: NWS writes its concurrency in the source, so there is no JavaScript data flow to taint
/// (see `specs/nws-causality-projection.md`). It is then handed to the same bounding pass the
/// TypeScript path uses, so one display contract serves both languages instead of two that could
/// drift.
fn nws_analysis(script: &str) -> ScriptAnalysis {
    const PHASE_TABLE_LIMIT: usize = 32;
    match crate::workflow_service::compile_nws(script) {
        Ok(plan) => {
            let analysis = nc_code_workflow::script::graph::project_analysis(&plan);
            ScriptAnalysis {
                ok: true,
                diagnostics: Vec::new(),
                phase_names: plan
                    .phase_names
                    .iter()
                    .take(PHASE_TABLE_LIMIT)
                    .cloned()
                    .collect(),
                phase_alongside: (!plan.phase_alongside.is_empty())
                    .then(|| plan.phase_alongside.clone()),
                causality_graph: crate::workflow_graph::bound_causality_graph(&analysis),
            }
        }
        Err(diagnostics) => ScriptAnalysis {
            ok: false,
            diagnostics: crate::workflow_service::nws_diagnostics(&diagnostics),
            phase_names: Vec::new(),
            phase_alongside: None,
            causality_graph: None,
        },
    }
}

fn parse_analysis(value: &Value) -> ScriptAnalysis {
    let diagnostics: Vec<CompileDiagnostic> = value
        .get("diagnostics")
        .cloned()
        .and_then(|diagnostics| serde_json::from_value(diagnostics).ok())
        .unwrap_or_default();
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false) && diagnostics.is_empty();
    let causality_graph = crate::workflow_graph::bound_causality_graph(value);
    let (phase_names, phase_alongside) =
        crate::tools::workflow::support::phase_table(causality_graph.as_ref());
    ScriptAnalysis {
        ok,
        diagnostics,
        phase_names,
        phase_alongside,
        causality_graph,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn analysis_parsing_reads_diagnostics_and_named_phases_only() {
        let analysis = parse_analysis(&json!({
            "ok": false,
            "diagnostics": [{"code": 2322, "line": 3, "column": 7, "message": "bad type"}],
            "causality": {"steps": [], "lanes": []},
            "flow": {"phases": [
                {"id": "unphased"},
                {"id": "phase#1", "name": "plan"},
                {"id": "phase#2", "name": "execute"}
            ]}
        }));
        assert!(!analysis.ok);
        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!(analysis.diagnostics[0].line, 3);
        // The synthetic unphased does not enter the table: it is the UI localisation fallback stop, not a phase an author wrote down.
        assert_eq!(analysis.phase_names, vec!["plan", "execute"]);
        assert_eq!(analysis.phase_alongside, None);
    }

    #[test]
    fn analysis_parsing_reads_phase_alongside_from_the_same_graph() {
        let analysis = parse_analysis(&json!({
            "ok": true,
            "causality": {"steps": [], "lanes": []},
            "flow": {"phases": [
                {"id": "phase#1", "name": "plan"},
                {"id": "phase#2", "name": "fan-a", "alongside": ["phase#3"]},
                {"id": "phase#3", "name": "fan-b"}
            ]}
        }));
        assert!(analysis.ok);
        assert_eq!(analysis.phase_names, vec!["plan", "fan-a", "fan-b"]);
        assert_eq!(
            analysis.phase_alongside,
            Some(vec![vec![], vec![2], vec![]])
        );
    }

    #[test]
    fn analysis_without_a_causality_graph_has_no_phase_table() {
        // The same posture as TS: without a causality graph there is no phase vocabulary (`createWorkflowPhaseNames(undefined)`),
        // so nothing is emitted even when the control-flow projection happens to list phases.
        let analysis = parse_analysis(&json!({
            "ok": true,
            "flow": {"phases": [{"id": "phase#1", "name": "plan"}]}
        }));
        assert!(analysis.phase_names.is_empty());
        assert_eq!(analysis.phase_alongside, None);
    }

    #[test]
    fn a_clean_analysis_with_diagnostics_is_still_not_ok() {
        // When `ok` and the diagnostics diverge as two sources, "there are diagnostics" wins: letting the model submit a script with errors is unacceptable,
        // and a dropped diagnostic is exactly why it cannot fix the problem.
        let analysis = parse_analysis(&json!({
            "ok": true,
            "diagnostics": [{"code": 1, "line": 1, "column": 1, "message": "x"}]
        }));
        assert!(!analysis.ok);
    }
}
