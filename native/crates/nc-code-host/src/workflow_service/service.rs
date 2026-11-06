//! The dynamic workflow run service (port of
//! `apps/nc-code-cli/packages/bootstrap/src/app/dynamic-workflow-run-service.ts` and its
//! `-submit` / `-launch` / `-lifecycle` / `-reconcile` half-modules).
//!
//! Responsibility: compile once → assemble driver + engine → run the sandbox fire-and-forget → hand the run's observation surface (snapshot / enumeration /
//! event pagination / artifacts / workspace manifest / escalation Q&A / cancel / resume / amend) out through a narrow interface.
//!
//! ## Where the seven TS invariants land in this file
//!
//! 1. **The engine owns createRun**: `submit` / `amend` never pre-insert a `dwf_run` row; the row is written by
//!    the createRun branch of `WorkflowEngine::new`. `resume` relies on the same mechanism in reverse (hitting an
//!    existing row takes the resume branch). The two entries point in opposite directions and never share a gate.
//! 2. **Compile exactly once**: `compile_once` calls `CompileKernel::compile` a single time; diagnostics are a business
//!    failure and not one run row is created. `submit_compiled` hands "already compiled" over as the only entry for callers
//!    that must analyze the script first (the v4 gateway / fixture-driven tests), so no second compile path exists.
//! 3. **No journal, no construction of this service**: `new` returns `Err` when it cannot open the journal file, so the
//!    caller takes a visible degradation instead of getting a service that silently loses persistence.
//! 4. **Orphan convergence at construction time**: see [`reconcile_orphan_runs`](WorkflowService::reconcile_orphan_runs).
//! 5. **resume replaces the registry entry before starting it**: the "register the handle → spawn the sandbox" order
//!    inside `launch` is load-bearing, and submit / amend / resume share one `launch`.
//! 6. **Resident work registration**: TS uses it to block idle shutdown; the native host has no idle resident pool, so the
//!    equivalent is `close()`'s "stop every run it owns and wait for them to settle" — see `close` and the
//!    "resident-work / idle-shutdown equivalence" gap in the delivery report.
//! 7. **close's terminal rows are written by the engine itself**: `close` only calls `engine.stop(interrupted)` and then
//!    awaits `settled()`; the service never calls `journal.update_run_status` to write a terminal state (the only
//!    exception is orphan convergence, which handles exactly the rows that **have no engine** — see invariant 4).
//!
//! ## Differences from TS (recorded honestly)
//!
//! - journal location: TS uses the `dwf_*` tables inside the session store (`SqliteSessionStore`); the native host uses its
//!   own durable journal file `<data-dir>/workflow-journal.sqlite`. The physical schema is kept column by column
//!   (the migrations of `nc_code_workflow::journal::sqlite`), so both runtimes can still read the same data mutually.
//! - the scope of orphan convergence: TS converges by `parentSessionId` (the session store is shared by every session of
//!   the same app); the native journal file is private to the host's data-dir and this process is its only writer, so **the
//!   file is the scope**. The residual risk (two hosts sharing one data-dir) is recorded in the delivery report.
//! - the `pendingQuestions` refusal taxonomy: TS's parked table remembers the exit history of "answered / withdrawn"; the
//!   native `EscalationRegistry` keeps only the parked items, so any qid that misses is classified as `unknown_question`.
//! - the resume fallbacks for args / cwd: TS lets the sandbox take `{}` for an absent `args` and falls back to
//!   `process.cwd()` for an absent `cwd`; native falls back to the construction-time `workflow_cwd` for `cwd` and keeps
//!   `args` absent (the sandbox reads it as `{}`). `script_text` / `script_hash` are a **hard requirement** (a missing one
//!   is a named refusal): without them the "byte-identical resume" cannot be honoured.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nc_code_workflow::compile::{CompileDiagnostic, CompileError, CompileKernel};
use nc_code_workflow::engine::{EngineConfig, RunLaunchConfig, WorkflowEngine};
use nc_code_workflow::journal::SqliteJournal;
use nc_code_workflow::sandbox::{self, SandboxOptions};
use nc_code_workflow::script::interpret;
use nc_code_workflow::types::{
    AskSpec, Caps, ImportedRunCache, JournalStorePort, ListEventsOptions, NodeRecord, RunEvent,
    RunSettlement, RunSettlementRecord, RunStatus, RunStopReason, StoredEvent, WorkflowDriver,
    WorkflowError, WorkflowErrorCode, WorkflowErrorJson,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::workflow::{HostWorkflowDriver, HostWorkflowDriverConfig};
use crate::HostServer;

use super::ask_validate::validate;
use super::compile::{
    compile_nws, detect_language, nws_diagnostics, CompiledWorkflow, WorkflowLanguage,
};
use super::import_cache::{self, ImportRefusal};
// The read model's public types are re-exported through this module: the field types of `RunSnapshot` must be publicly
// reachable (otherwise "a private type appears in a public interface"), and callers need not know they live in `reads`.
use super::reads::{
    self, JournalIntrospection, DEFAULT_LIST_EVENTS_LIMIT, DEFAULT_LIST_RUNS_LIMIT,
    MAX_LIST_EVENTS_LIMIT, MAX_LIST_RUNS_LIMIT,
};
pub use super::reads::{
    LabelSource, NodeCounts, PendingQuestionView, RunArtifact, RunArtifactVersion, RunListItem,
    SnapshotStatus, WorkspaceNode,
};

/// The native host's journal file name (below the constructor parameter `data_dir`).
pub const WORKFLOW_JOURNAL_FILE: &str = "workflow-journal.sqlite";
/// The run id prefix (same as TS `mintRunId`; the character set `[A-Za-z0-9-]` is safe in URLs / paths / logs).
const RUN_ID_PREFIX: &str = "dwfrun";
const INPUT_ID_PREFIX: &str = "dwfinput";
/// The concurrency ceiling: `max(1, min(16, availableParallelism − 2))` (TS `workflow-concurrency-ceiling.ts`).
const CONCURRENCY_CEILING_MAX: u32 = 16;
const CONCURRENCY_RESERVED: u32 = 2;
const CONCURRENCY_FLOOR: u32 = 1;
/// `run-launched` is written only once, at the floor, so reading back the "anchor / subagent model / script path" only needs a scan of the journal head.
const RUN_LAUNCH_SCAN_LIMIT: usize = 8;

/// The callback shape of the realtime event fan-out: `(runId, RunEvent JSON)`.
///
/// **Serves projection only**: the journal is the persistent truth of events (the engine has appended before it emits), so losing
/// events on this channel affects only the realtime panel, never a read surface. The payload carries no journal sequence (the engine
/// drops `append_event`'s return value); consumers that need a sequence read [`WorkflowService::list_events`].
pub type EventSink = Arc<dyn Fn(&str, &str, Option<&str>, Option<&str>, u64, Value) + Send + Sync>;

// ————————————————————————————————————————————————————————————————
// Public error / result types
// ————————————————————————————————————————————————————————————————

/// A service-layer failure. **Business refusals are kept apart from internal faults**: the former are discriminant keys the
/// model/user can change their action on, the latter's text is surfaced verbatim (including the compile kernel's stderr tail).
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowServiceError {
    /// Someone still wants to start a run after `close()` (a wiring fault, not a business refusal).
    Closed,
    /// The script is not clean: the diagnostics are the data, and not one run row was created.
    CompileFailed { diagnostics: Vec<CompileDiagnostic> },
    /// The compile kernel itself is unavailable (node missing / dist missing / timeout / crash).
    CompileKernel(String),
    /// The journal has no such run.
    RunNotFound { run_id: String },
    /// This process is already running the same runId (resume cannot take over a live engine).
    AlreadyRunning { run_id: String },
    /// A run superseded by an amendment: not resumable (go look at the successor).
    Superseded {
        run_id: String,
        superseded_by: Option<String>,
    },
    /// The state is not resumable (completed / errored; stopped(superseded) has its own variant).
    NotResumable {
        run_id: String,
        status: RunStatus,
        stop_reason: Option<RunStopReason>,
    },
    /// The row lacks the metadata resume requires (`script_text` / `script_hash`).
    MissingMetadata { run_id: String, field: &'static str },
    /// The record is not self-consistent (`script_text` and `script_hash` no longer corroborate each other): refuse instead of launching.
    ScriptMismatch {
        run_id: String,
        expected: String,
        got: String,
    },
    /// The parent session is not in this host's store (the run's actor sessions have nothing to inherit from).
    SessionUnavailable {
        session_id: String,
        run_id: Option<String>,
    },
    /// The amendment was refused (the three gates of the pre-flight / the build).
    AmendRejected {
        run_id: String,
        reason: ImportRefusal,
    },
    /// The engine constructor refused (an engine-side failure other than `ScriptHashMismatch`).
    Engine {
        code: WorkflowErrorCode,
        message: String,
    },
    /// Storage / assembly fault (cannot open the journal, the compile artifact has no lowered body, and so on).
    Internal(String),
}

impl std::fmt::Display for WorkflowServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(
                formatter,
                "the workflow run service is closed; a launch after close is a wiring fault"
            ),
            Self::CompileFailed { diagnostics } => {
                let body = diagnostics
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                write!(
                    formatter,
                    "The workflow script does not compile (no run was created):\n{body}"
                )
            }
            Self::CompileKernel(message) => write!(formatter, "{message}"),
            Self::RunNotFound { run_id } => write!(formatter, "unknown workflow run: {run_id}"),
            Self::AlreadyRunning { run_id } => write!(
                formatter,
                "workflow run {run_id} is still running in this process; wait for it to settle"
            ),
            Self::Superseded { run_id, .. } => write!(
                formatter,
                "workflow run {run_id} was superseded by an amendment; resume the successor instead"
            ),
            Self::NotResumable {
                run_id,
                status,
                stop_reason,
            } => write!(
                formatter,
                "workflow run {run_id} is not resumable (status {}{})",
                status.as_str(),
                stop_reason
                    .map(|reason| format!(", stop reason {}", reason.as_str()))
                    .unwrap_or_default()
            ),
            Self::MissingMetadata { run_id, field } => write!(
                formatter,
                "workflow run {run_id} has no {field}; it cannot be resumed byte-identically"
            ),
            Self::ScriptMismatch {
                run_id,
                expected,
                got,
            } => write!(
                formatter,
                "workflow run {run_id} has an inconsistent script hash (recorded {expected}, got {got})"
            ),
            Self::SessionUnavailable { session_id, .. } => write!(
                formatter,
                "session {session_id} does not exist in this host's store; a workflow run needs it \
                 as its parent"
            ),
            Self::AmendRejected { run_id, reason } => write!(
                formatter,
                "cannot amend workflow run {run_id}: {}",
                reason.as_str()
            ),
            Self::Engine { code, message } => write!(formatter, "[{}] {message}", code.as_str()),
            Self::Internal(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for WorkflowServiceError {}

type ServiceResult<T> = Result<T, WorkflowServiceError>;

/// The input for submitting a brand-new run.
#[derive(Clone, Debug, Default)]
pub struct SubmitRequest {
    /// The parent session (the host's session id): the actor child sessions inherit its run configuration, and the run's `parent_session_id` uses it too.
    pub session_id: String,
    pub script_text: String,
    /// The run's working directory (the sandbox cwd and `dwf_run.cwd`). Absent uses the construction-time `workflow_cwd`.
    pub cwd: Option<String>,
    pub name: Option<String>,
    pub args: Option<Value>,
    pub tool_call_id: Option<String>,
    /// The script's declared phase table (lives only on `run-launched`; `dwf_run` has no such column).
    pub phase_names: Option<Vec<String>>,
    /// The "still running alongside" table, index-aligned with [`SubmitRequest::phase_names`] (TS
    /// `createWorkflowPhaseAlongside`): `phase_alongside[i]` lists the other phase indices still running when phase
    /// i is entered. Also lives only on `run-launched`.
    pub phase_alongside: Option<Vec<Vec<u32>>>,
    /// The launch anchor (TS `launchInputId`): the same value as `run-launched.inputId`. Absent on the tool path
    /// (the service mints one itself); a direct GUI launch mints one and shares it across the controlOnly launch turns.
    pub launch_input_id: Option<String>,
    /// The requested concurrency bound (absent = the ceiling; a given value is clamped to `[1, ceiling]`).
    pub max_concurrency: Option<u32>,
    /// This run's subagent model as the canonical picker string (`providerId/modelId[$reasoning]`).
    pub subagent_model: Option<String>,
    /// The absolute path of this run's script file (pure model-surface metadata; not part of the resume validation).
    pub script_path: Option<String>,
}

/// The input for amending a predecessor run.
#[derive(Clone, Debug)]
pub struct AmendRequest {
    /// The parent session; absent keeps the `parent_session_id` on the predecessor row.
    pub session_id: Option<String>,
    pub predecessor_run_id: String,
    pub script_text: String,
    pub cwd: Option<String>,
    /// The display name; absent keeps the predecessor's (an amendment is the next version of the same piece of work).
    pub name: Option<String>,
    pub max_concurrency: Option<u32>,
    pub subagent_model: Option<String>,
    pub script_path: Option<String>,
    /// Whether to reuse the predecessor's args (the GUI "configure" rerun; the tool path does not pass it).
    pub inherit_args: bool,
    pub tool_call_id: Option<String>,
    pub phase_names: Option<Vec<String>>,
    /// The concurrency table index-aligned with [`AmendRequest::phase_names`]: an amendment reuses the predecessor's `inputId`,
    /// while the sidebar track draws the new script's sites, so both tables must be taken together from the new script's graph.
    pub phase_alongside: Option<Vec<Vec<u32>>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitResult {
    pub run_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmendResult {
    pub run_id: String,
    /// The predecessor stopped by this amendment (only set when it was in flight).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_run_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeResult {
    pub run_id: String,
    /// Whether the imported cache was rebuilt (`false` = the predecessor is unavailable, so this continuation re-executes the unconsumed imports live).
    pub imported_cache: bool,
}

/// The cancellation initiator: decides the `stopReason` that lands in storage (and whether interrupted carries a structured error).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelInitiator {
    User,
    Model,
    Interrupted,
}

/// The answer result of an escalation question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ok", rename_all = "lowercase")]
pub enum ResolveQuestionResult {
    #[serde(rename = "true")]
    Resolved { qid: String },
    #[serde(rename = "false")]
    Refused {
        qid: String,
        reason: QuestionRefusal,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionRefusal {
    AlreadyResolved,
    RunNotInFlight,
    UnknownQuestion,
}

/// The projection of one ask's parked question onto the snapshot (when empty the whole field is absent; no empty array is emitted).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSnapshot {
    pub run_id: String,
    pub task_id: String,
    pub status: SnapshotStatus,
    /// The real terminal word (`completed` / `errored` / `stopped`), present only in a terminal state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_status: Option<RunStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<RunStopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    pub max_concurrency: u32,
    pub spent_tokens: u64,
    /// This run's subagent model (present only when it was set; the authority is `run-launched`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_model: Option<String>,
    /// This run's script file (present only when it was recorded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    pub nodes: NodeCounts,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<RunArtifact>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending_questions: Vec<PendingQuestionView>,
    /// The script's top-level return value (the engine-internal `artifact`). **Not** a user-facing artifact — see `artifacts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<WorkflowErrorJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub resumable: bool,
}

/// Event pagination (cursor = journal sequence; only rows strictly greater than it are returned).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventPage {
    pub events: Vec<StoredEvent>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_sequence: Option<u64>,
}

/// The bytes of one artifact version.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactBytes {
    pub bytes: Vec<u8>,
    /// Takes the contentType from the **journal row** (the UI dispatches renderers by the exact string).
    pub content_type: Option<String>,
}

// ————————————————————————————————————————————————————————————————
// Internals: settlement slot / run handle
// ————————————————————————————————————————————————————————————————

/// One run's settlement promise: `set` exactly once (when the sandbox task returns), `wait` waits with no timeout.
#[derive(Default)]
struct SettlementSlot {
    terminal: Mutex<Option<RunSettlement>>,
    notify: tokio::sync::Notify,
}

impl SettlementSlot {
    fn get(&self) -> Option<RunSettlement> {
        self.terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set(&self, settlement: RunSettlement) {
        {
            let mut terminal = self
                .terminal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if terminal.is_some() {
                return;
            }
            *terminal = Some(settlement);
        }
        self.notify.notify_waiters();
    }

    /// Wait for settlement. Register interest before checking the current value: `notify_waiters` stores no permits, so the direct
    /// `if let Some(..) { return } notified().await` would lose a wake-up that lands "between the check and the wait".
    async fn wait(&self) -> RunSettlement {
        loop {
            let notified = self.notify.notified();
            if let Some(settlement) = self.get() {
                return settlement;
            }
            notified.await;
        }
    }
}

/// The closing guard of one sandbox execution: it must settle even when the task is aborted / panics, otherwise `close()` waits forever
/// (the row would still sit at running, and the next construction's orphan convergence would reconcile it to stopped(interrupted)).
struct SettlementGuard {
    slot: Arc<SettlementSlot>,
    finished: bool,
}

impl SettlementGuard {
    fn finish(mut self, settlement: RunSettlement) {
        self.finished = true;
        self.slot.set(settlement);
    }
}

impl Drop for SettlementGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.slot.set(RunSettlement {
            status: RunStatus::Errored,
            artifact: None,
            reason: None,
            superseded_by: None,
            error: Some(WorkflowError::new(
                WorkflowErrorCode::DriverError,
                "The workflow sandbox task ended without settling the run",
            )),
        });
    }
}

/// A run owned by this service. Registration happens before launch (the same discipline as invariant 5).
struct RunHandle {
    run_id: String,
    /// The parent session id (used for cancellation / enumeration ownership).
    session_id: String,
    engine: Arc<WorkflowEngine>,
    driver: Arc<HostWorkflowDriver>,
    settlement: Arc<SettlementSlot>,
}

impl RunHandle {
    fn is_live(&self) -> bool {
        self.settlement.get().is_none()
    }
}

/// The full input of `launch` (the execution-side shape shared by submit / amend / resume).
struct LaunchSpec {
    run_id: String,
    session_id: String,
    script_text: String,
    compiled: CompiledWorkflow,
    cwd: String,
    name: Option<String>,
    args: Option<Value>,
    tool_call_id: Option<String>,
    caps: Caps,
    phase_names: Option<Vec<String>>,
    phase_alongside: Option<Vec<Vec<u32>>>,
    subagent_model: Option<String>,
    script_path: Option<String>,
    /// The amend path: the lineage pointer + the imported cache (they come as a pair).
    resumed_from: Option<String>,
    imported_cache: Option<ImportedRunCache>,
    /// The amend path: the predecessor's `spentTokens` after settlement (the engine writes it as the initial value when creating the row).
    inherited_tokens: Option<u64>,
    /// The anchor an amendment reuses from the predecessor; submit mints one when it is absent.
    launch_input_id: Option<String>,
}

/// The few pieces of host metadata on `run-launched` that are "fixed in the life that created the run and never change again".
struct RunLaunchHeader {
    input_id: String,
    subagent_model: Option<String>,
    script_path: Option<String>,
}

// ————————————————————————————————————————————————————————————————
// The service
// ————————————————————————————————————————————————————————————————

/// The dynamic workflow run service. One per host instance (= one window's Local Host).
pub struct WorkflowService {
    /// Weak by construction: the host owns the port that owns this service, so a strong
    /// handle here would close a reference cycle (host → port → service → host). The
    /// cycle leaks the resident compile kernel — and its Node child — past host shutdown,
    /// which keeps the runtime's process reaper waiting forever (tests never exit).
    host: std::sync::Weak<HostServer>,
    data_dir: PathBuf,
    workflow_cwd: PathBuf,
    /// The journal the engine faces (a synchronous port with a Mutex-protected connection inside).
    journal: Arc<SqliteJournal>,
    /// The read-only introspection surface the host faces (a separate connection: enumeration/counting never queue behind the engine's write transactions).
    introspection: JournalIntrospection,
    /// The resident compile kernel: spawned only on the first compile, and a crash is restarted once by the next request.
    kernel: tokio::sync::Mutex<Option<Arc<CompileKernel>>>,
    /// runId → handle. Registered before launch and kept after settlement (the in-memory terminal state outranks the journal row).
    runs: Mutex<HashMap<String, Arc<RunHandle>>>,
    event_sink: Option<EventSink>,
    id_counter: AtomicU64,
    /// The process concurrency ceiling (`max(1, min(16, cores − 2))`): shared by the caps starting point and the read surface's criterion.
    concurrency_ceiling: u32,
    closed: AtomicBool,
    close_lock: tokio::sync::Mutex<()>,
}

impl WorkflowService {
    /// Construct the service: open (creating if needed) `<data_dir>/workflow-journal.sqlite` and converge orphan runs per invariant 4.
    ///
    /// A journal that cannot be opened is an `Err` — durability is a precondition of a run, and a run that silently loses its
    /// persistence is worse than no run at all (no basis for resume, no source for the detail page, no record after cancellation).
    pub fn new(
        host: Arc<HostServer>,
        data_dir: &Path,
        workflow_cwd: PathBuf,
    ) -> ServiceResult<Arc<Self>> {
        Self::new_with_event_sink(host, data_dir, workflow_cwd, None)
    }

    /// Same as [`Self::new`], but installs the realtime event fan-out at construction time (**the projection channel this service chose**).
    ///
    /// Why "a callback given at construction time" was chosen instead of "`subscribe(run_id) -> Receiver`": the projection side is one long-lived
    /// consumer (v4 frame forwarding) that does not need to manage subscription lifetimes per run; and a per-run channel exposed outward would make
    /// callers tidy up after every single run (otherwise unbounded channels grow with the run count). The callback shape keeps the lifecycle inside the service.
    pub fn new_with_event_sink(
        host: Arc<HostServer>,
        data_dir: &Path,
        workflow_cwd: PathBuf,
        event_sink: Option<EventSink>,
    ) -> ServiceResult<Arc<Self>> {
        let journal_path = data_dir.join(WORKFLOW_JOURNAL_FILE);
        // Connection pragmas + migrations (idempotent) live in `SqliteJournal::open`; the introspection connection is read-only, so it comes after.
        let journal = Arc::new(SqliteJournal::open(&journal_path).map_err(|error| {
            WorkflowServiceError::Internal(format!(
                "cannot open the workflow journal at {}: {error}",
                journal_path.display()
            ))
        })?);
        let introspection = JournalIntrospection::open(&journal_path).map_err(|error| {
            WorkflowServiceError::Internal(format!(
                "cannot open the workflow journal introspection connection at {}: {error}",
                journal_path.display()
            ))
        })?;
        let service = Arc::new(Self {
            host: Arc::downgrade(&host),
            data_dir: data_dir.to_path_buf(),
            workflow_cwd,
            journal,
            introspection,
            kernel: tokio::sync::Mutex::new(None),
            runs: Mutex::new(HashMap::new()),
            event_sink,
            id_counter: AtomicU64::new(1),
            concurrency_ceiling: resolve_concurrency_ceiling(),
            closed: AtomicBool::new(false),
            close_lock: tokio::sync::Mutex::new(()),
        });
        service.reconcile_orphan_runs();
        Ok(service)
    }

    /// This process's concurrency ceiling (the caps starting point; the read surface uses the same value to judge "is this bound worth displaying").
    pub fn concurrency_ceiling(&self) -> u32 {
        self.concurrency_ceiling
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn assert_open(&self) -> ServiceResult<()> {
        if self.is_closed() {
            return Err(WorkflowServiceError::Closed);
        }
        Ok(())
    }

    // ——————————————————————— the three entries ———————————————————————

    /// Submit a brand-new run: compile once → mint the id → assemble and launch. **Never pre-inserts a `dwf_run` row** (invariant 1).
    pub async fn submit(&self, request: SubmitRequest) -> ServiceResult<SubmitResult> {
        self.assert_open()?;
        let compiled = self.compile_once(&request.script_text).await?;
        self.submit_compiled(request, &compiled).await
    }

    /// The "already compiled" entry of [`Self::submit`]: the compile-once seam, for callers that already hold a compile artifact
    /// (the v4 gateway runs `analyze` first, fixture-driven tests feed a parity fixture directly). It runs the
    /// **same** launch path, so there is no second state machine.
    pub async fn submit_compiled(
        &self,
        request: SubmitRequest,
        compiled: &CompiledWorkflow,
    ) -> ServiceResult<SubmitResult> {
        self.assert_open()?;
        let session_id = request.session_id.trim().to_string();
        if session_id.is_empty() {
            return Err(WorkflowServiceError::SessionUnavailable {
                session_id,
                run_id: None,
            });
        }
        let run_id = self.mint_run_id();
        let caps = Caps {
            max_concurrency: clamp_run_concurrency(
                request.max_concurrency,
                self.concurrency_ceiling,
            ),
        };
        self.launch(LaunchSpec {
            run_id: run_id.clone(),
            session_id,
            script_text: request.script_text,
            compiled: compiled.clone(),
            cwd: self.resolve_cwd(request.cwd),
            name: request.name,
            args: request.args,
            tool_call_id: request.tool_call_id,
            caps,
            phase_names: request.phase_names,
            phase_alongside: request.phase_alongside,
            subagent_model: request.subagent_model,
            script_path: request.script_path,
            resumed_from: None,
            imported_cache: None,
            inherited_tokens: None,
            launch_input_id: request.launch_input_id,
        })?;
        Ok(SubmitResult { run_id })
    }

    /// Amend: compile once → pre-flight the predecessor → stop the in-flight predecessor and await its own settlement → build the imported cache → launch the new run.
    ///
    /// The order is the semantics (same as TS `amendDynamicWorkflowRun` above; the only difference is recorded below):
    ///   1. **pre-flight** the predecessor (exists ∧ its boundaries are complete) — when refused nothing has moved and the predecessor keeps running;
    ///   2. compile the new script. TS puts the compile inside `startNewRun` (so it stops the predecessor **first** and then compiles), which means
    ///      a badly written revision script would kill a running predecessor for nothing; native hoists the compile above the stop — still
    ///      "compile once" and still creating no row, just not paying for one syntax error with the predecessor's life;
    ///   3. mint the new id;
    ///   4. if the predecessor is in flight, stop it with `superseded` and **await its own settlement** (not a timeout, not polling the journal),
    ///      so the predecessor settles as `stopped(superseded, supersededBy)`;
    ///   5. build the cache from the already-settled predecessor (the pre-flight passed, so a refusal here can only mean the predecessor is held by another process);
    ///   6. launch on the same path as a fresh submit, carrying `resumed_from` and the cache.
    pub async fn amend(&self, request: AmendRequest) -> ServiceResult<AmendResult> {
        self.assert_open()?;
        // 1. Pre-flight: the predecessor exists ∧ its boundaries are complete. When refused nothing has moved (not even the compile) and the predecessor keeps running.
        let predecessor =
            import_cache::preflight_amend(self.journal.as_ref(), &request.predecessor_run_id)
                .map_err(|reason| WorkflowServiceError::AmendRejected {
                    run_id: request.predecessor_run_id.clone(),
                    reason,
                })?;
        // 2. Compile the new script (before stopping the predecessor; see the ordering note above for the reason).
        let compiled = self.compile_once(&request.script_text).await?;

        // 3. Mint the new id.
        let run_id = self.mint_run_id();
        let mut superseded_run_id = None;
        if let Some(handle) = self
            .handle(&request.predecessor_run_id)
            .filter(|handle| handle.is_live())
        {
            handle
                .engine
                .stop(RunStopReason::Superseded, None, Some(run_id.clone()));
            // Await the predecessor's **own** settlement promise (the same slot `wait_settlement` uses), not a timer.
            let settlement = handle.settlement.wait().await;
            eprintln!(
                "[nc-code-host] workflow run {} superseded by {} ({:?})",
                request.predecessor_run_id, run_id, settlement.status
            );
            superseded_run_id = Some(request.predecessor_run_id.clone());
        }

        let host = self
            .host
            .upgrade()
            .ok_or_else(|| WorkflowServiceError::Internal("the host is gone".to_string()))?;
        let built = import_cache::build_imported_cache(
            &host,
            self.journal.as_ref(),
            &request.predecessor_run_id,
        )
        .map_err(|reason| WorkflowServiceError::AmendRejected {
            run_id: request.predecessor_run_id.clone(),
            reason,
        })?;

        // Usage starting point: read the row **again after the predecessor has settled** (the pre-flight copy is a snapshot taken before the stop, and
        // using it would miss the predecessor's last rounds of spend — amending an in-flight run is exactly this path's most common use).
        let settled = self.journal.get_run(&request.predecessor_run_id);
        let inherited_tokens = settled.as_ref().map(|row| row.spent_tokens).unwrap_or(0);
        // Args are reused only when the caller says so explicitly; when the predecessor has no args the whole field is absent.
        let inherited_args = if request.inherit_args {
            settled.as_ref().and_then(|row| row.args.clone())
        } else {
            None
        };
        let session_id = request
            .session_id
            .clone()
            .filter(|session| !session.trim().is_empty())
            .or_else(|| predecessor.parent_session_id.clone())
            .ok_or_else(|| WorkflowServiceError::SessionUnavailable {
                session_id: String::new(),
                run_id: Some(request.predecessor_run_id.clone()),
            })?;
        let name = request.name.clone().or_else(|| predecessor.name.clone());
        // Anchor: an amendment reuses the predecessor's `run-launched` (every step of the same piece of work hangs off the same message).
        let launch_input_id = self
            .read_run_launch_header(&request.predecessor_run_id)
            .map(|header| header.input_id);
        let caps = Caps {
            max_concurrency: clamp_run_concurrency(
                request.max_concurrency,
                self.concurrency_ceiling,
            ),
        };

        self.launch(LaunchSpec {
            run_id: run_id.clone(),
            session_id,
            script_text: request.script_text,
            compiled,
            cwd: self.resolve_cwd(request.cwd),
            name,
            args: inherited_args,
            tool_call_id: request.tool_call_id,
            caps,
            phase_names: request.phase_names,
            phase_alongside: request.phase_alongside,
            subagent_model: request.subagent_model,
            script_path: request.script_path,
            resumed_from: Some(built.resumed_from.clone()),
            imported_cache: Some(built.cache),
            inherited_tokens: Some(inherited_tokens),
            launch_input_id,
        })?;
        Ok(AmendResult {
            run_id,
            superseded_run_id,
        })
    }

    /// Resume an existing run (the engine hits an existing row ⇒ the resume branch; the script hash is verified a second time inside the engine).
    ///
    /// The refusal surface is all named business branches (not internal faults): unknown run / in flight in this process / superseded / state not resumable /
    /// missing metadata / record not self-consistent / the new script fails to compile. **On failure not one row is touched, no entry is created, nothing is launched.**
    pub async fn resume(&self, run_id: &str) -> ServiceResult<ResumeResult> {
        self.assert_open()?;
        let Some(record) = self.journal.get_run(run_id) else {
            return Err(WorkflowServiceError::RunNotFound {
                run_id: run_id.to_string(),
            });
        };
        if self
            .handle(run_id)
            .map(|handle| handle.is_live())
            .unwrap_or(false)
        {
            return Err(WorkflowServiceError::AlreadyRunning {
                run_id: run_id.to_string(),
            });
        }
        if record.status == RunStatus::Stopped
            && record.stop_reason == Some(RunStopReason::Superseded)
        {
            return Err(WorkflowServiceError::Superseded {
                run_id: run_id.to_string(),
                superseded_by: record.superseded_by.clone(),
            });
        }
        if !reads::is_resumable(record.status, record.stop_reason) {
            return Err(WorkflowServiceError::NotResumable {
                run_id: run_id.to_string(),
                status: record.status,
                stop_reason: record.stop_reason,
            });
        }
        let Some(script_text) = record.script_text.clone() else {
            return Err(WorkflowServiceError::MissingMetadata {
                run_id: run_id.to_string(),
                field: "script_text",
            });
        };
        let Some(recorded_hash) = record.script_hash.clone() else {
            return Err(WorkflowServiceError::MissingMetadata {
                run_id: run_id.to_string(),
                field: "script_hash",
            });
        };
        // An old run's journal text was written against the facade **of that time**; a recompilation that no longer passes is a business branch the user can
        // expect (pointing at AmendWorkflow), not an internal fault.
        let compiled = self.compile_once(&script_text).await?;
        // The server validates first (the engine constructor still re-validates — defence in depth): when the record itself was rewritten from outside, create no entry and launch nothing.
        if compiled.script_hash() != recorded_hash {
            return Err(WorkflowServiceError::ScriptMismatch {
                run_id: run_id.to_string(),
                expected: recorded_hash,
                got: compiled.script_hash().to_string(),
            });
        }
        let session_id = record
            .parent_session_id
            .clone()
            .filter(|session| !session.is_empty())
            .ok_or_else(|| WorkflowServiceError::SessionUnavailable {
                session_id: String::new(),
                run_id: Some(run_id.to_string()),
            })?;
        // resume of an amended run: rebuild the imported cache. **Failure only degrades, it never refuses** (see the comment on
        // `import_cache::rebuild_imported_cache_for_resume`).
        let imported = record.resumed_from.as_deref().and_then(|predecessor| {
            let host = self.host.upgrade()?;
            import_cache::rebuild_imported_cache_for_resume(
                &host,
                self.journal.as_ref(),
                run_id,
                predecessor,
            )
        });
        let imported_cache = imported.is_some();
        let launch_input_id = self
            .read_run_launch_header(run_id)
            .map(|header| header.input_id);

        // caps are reused from the journal record: spentTokens accumulated against this very caps, so recomputing them silently moves the goalposts.
        self.launch(LaunchSpec {
            run_id: run_id.to_string(),
            session_id,
            script_text,
            compiled,
            cwd: record
                .cwd
                .clone()
                .unwrap_or_else(|| self.workflow_cwd.to_string_lossy().to_string()),
            name: record.name.clone(),
            // Args: resume **replays** the stored copy and never accepts new ones — a run's identity includes its args.
            args: record.args.clone(),
            tool_call_id: record.tool_call_id.clone(),
            caps: record.caps.clone(),
            // The phase table / subagent model / script path are all not passed down: they were fixed on `run-launched` in the life that created
            // the run, and the engine only records them on the created branch (resume hits an existing row, so passing them is harmless and pointless).
            phase_names: None,
            phase_alongside: None,
            subagent_model: None,
            script_path: None,
            // The lineage pointer is not passed down either: `createRun` already fixed it at submit time.
            resumed_from: None,
            imported_cache: imported,
            inherited_tokens: None,
            launch_input_id,
        })?;
        Ok(ResumeResult {
            run_id: run_id.to_string(),
            imported_cache,
        })
    }

    // ——————————————————————— cancel / close ———————————————————————

    /// Stop a run this service owns (`engine.stop` → one of the three terminal paths).
    ///
    /// Returning false = there is nothing to abort (unknown run or already settled), which lets the layer above normalize it into a structured
    /// not_found instead of reporting a cancellation that never happened.
    pub fn cancel(&self, run_id: &str, initiator: CancelInitiator) -> bool {
        let Some(handle) = self.handle(run_id) else {
            return false;
        };
        if !handle.is_live() {
            return false;
        }
        let (reason, error) = cancel_parts(run_id, initiator);
        handle.engine.stop(reason, error, None);
        true
    }

    /// Close: stop every in-flight run this service owns and wait for them to settle **with no timeout** (invariant 7).
    ///
    /// The terminal rows are written by the engine itself (the tail of `finishRun`); this method performs not one journal write — writing around the
    /// engine would create a second writer of `dwf_run`, and the journal would hold no matching `run-settled`.
    ///
    /// Difference from TS: TS has an extra "resident blocking work registration" that blocks the App's idle shutdown; the native host has no idle
    /// resident pool — the close moment is decided by the composition root (which calls this method before the process exits), and the equivalence is recorded in the delivery report.
    pub async fn close(&self) {
        let guard = self.close_lock.lock().await;
        // Set the flag synchronously: a launch arriving during close must be blocked by the gate immediately.
        self.closed.store(true, Ordering::SeqCst);
        let live: Vec<Arc<RunHandle>> = self
            .runs
            .lock()
            .map(|runs| {
                runs.values()
                    .filter(|handle| handle.is_live())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for handle in &live {
            let (reason, error) = cancel_parts(&handle.run_id, CancelInitiator::Interrupted);
            handle.engine.stop(reason, error, None);
        }
        for handle in &live {
            // Wait for settlement — not a timeout, not polling: the harness settles synchronously once it receives the stop.
            let settlement = handle.settlement.wait().await;
            eprintln!(
                "[nc-code-host] workflow run {} settled on close ({:?})",
                handle.run_id, settlement.status
            );
        }
        // Wind the compile kernel down (kill_on_drop is only a backstop; an explicit shutdown lets node exit cleanly on its own).
        let kernel = {
            let mut guard = self.kernel.lock().await;
            guard.take()
        };
        if let Some(kernel) = kernel {
            let _ = kernel.shutdown().await;
        }
        drop(guard);
    }

    /// Wait for a run's settlement (returns immediately when it already settled). `None` = this service has no entry for that run.
    pub async fn wait_settlement(&self, run_id: &str) -> Option<RunSettlement> {
        let handle = self.handle(run_id)?;
        if let Some(settlement) = handle.settlement.get() {
            return Some(settlement);
        }
        Some(handle.settlement.wait().await)
    }

    // ——————————————————————— read surfaces ———————————————————————

    /// A single-run snapshot: journal row + node summary + parked questions + artifacts. An unknown run returns `None`.
    pub fn get_run(&self, run_id: &str) -> Option<RunSnapshot> {
        let record = self.journal.get_run(run_id);
        let handle = self.handle(run_id);
        if record.is_none() && handle.is_none() {
            return None;
        }
        let terminal = handle.as_ref().and_then(|handle| handle.settlement.get());
        let live = terminal.is_none() && handle.is_some();
        let status = reads::synthesize_run_status(
            terminal.as_ref(),
            live,
            record.as_ref().map(|row| row.status),
        );
        let is_terminal = reads::is_terminal(status);
        let stop_reason = reads::stop_reason_of(terminal.as_ref(), live, record.as_ref());
        // Failure is surfaced only when the synthesized status is terminal (under a live engine, a terminal state/failure on the row is a foreign write — see the state synthesis).
        let failure = if is_terminal {
            reads::failure_of(terminal.as_ref(), record.as_ref())
        } else {
            None
        };
        let nodes = self.journal.list_nodes(run_id);
        let header = self.read_run_launch_header(run_id);
        // The projection source of the parked questions is the **in-memory** escalation table: only while this service holds the entry (in flight)
        // or the run is terminal can "is anybody waiting for an answer" be told honestly; otherwise an empty table looks exactly like "don't know",
        // while "nobody is waiting" is the truth of this moment (a dead process's parked items vanish with the process).
        let pending_questions = if handle.is_some() || is_terminal {
            handle
                .as_ref()
                .map(|handle| {
                    reads::pending_question_views(
                        &handle.driver.escalation_registry().pending_questions(),
                    )
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let output = if status == RunStatus::Completed {
            terminal
                .as_ref()
                .and_then(|settlement| settlement.artifact.clone())
                .or_else(|| record.as_ref().and_then(|row| row.result.clone()))
        } else {
            None
        };
        let superseded_by = reads::superseded_by_of(terminal.as_ref(), record.as_ref());
        Some(RunSnapshot {
            run_id: run_id.to_string(),
            task_id: run_id.to_string(),
            status: reads::fold_snapshot_status(status),
            run_status: is_terminal.then_some(status),
            stop_reason,
            // When the row carries no parent session (an old row / rewritten from outside), the copy on the entry is still there: it is not a
            // guess but the session id this launch actually used.
            parent_session_id: record
                .as_ref()
                .and_then(|row| row.parent_session_id.clone())
                .or_else(|| handle.as_ref().map(|handle| handle.session_id.clone())),
            resumed_from: record.as_ref().and_then(|row| row.resumed_from.clone()),
            superseded_by,
            max_concurrency: record
                .as_ref()
                .map(|row| row.caps.max_concurrency)
                .unwrap_or(1),
            spent_tokens: record.as_ref().map(|row| row.spent_tokens).unwrap_or(0),
            subagent_model: header.as_ref().and_then(|h| h.subagent_model.clone()),
            script_path: header.as_ref().and_then(|h| h.script_path.clone()),
            nodes: reads::count_nodes(&nodes),
            reports: reads::reported_items(&nodes),
            artifacts: reads::merge_artifacts(&nodes),
            pending_questions,
            output,
            error: failure.as_ref().map(|failure| failure.message.clone()),
            failure,
            resumable: reads::is_resumable(status, stop_reason),
        })
    }

    /// Enumerate runs (`None` = all sessions), most recently updated first, 16 by default.
    ///
    /// Enumeration is **read-only**: annotations like `possiblyInterrupted` never rewrite the journal — burying a sibling session's in-flight run
    /// means marking a live run dead; the authority to perform orphan convergence belongs only to the owning host's construction moment.
    pub fn list_runs(&self, session_id: Option<&str>) -> Vec<RunListItem> {
        self.list_runs_limited(session_id, None)
    }

    /// Same as above with an explicit count (clamped to `[1, MAX_LIST_RUNS_LIMIT]`).
    pub fn list_runs_limited(
        &self,
        session_id: Option<&str>,
        limit: Option<usize>,
    ) -> Vec<RunListItem> {
        let limit = limit
            .unwrap_or(DEFAULT_LIST_RUNS_LIMIT)
            .clamp(1, MAX_LIST_RUNS_LIMIT);
        // TS also needs a branch here for the "in the registry, missing from the journal" gap (the few microtasks after submit returns and before the
        // engine's createRun). Native has no such gap: `WorkflowEngine::new` writes the `dwf_run` row synchronously before `launch` returns, so the
        // enumeration surface (the journal) is complete from the moment the entry returns and needs no complement pass.
        let rows = match self.introspection.run_rows(session_id, limit) {
            Ok(rows) => rows,
            Err(error) => {
                // An enumeration failure is a visible degradation (an empty list), not a fake "there are no runs".
                eprintln!("[nc-code-host] workflow run enumeration failed: {error}");
                return Vec::new();
            }
        };
        rows.into_iter()
            .filter_map(|row| self.list_item(&row))
            .collect()
    }

    /// One run's event page (cursor = returns only `sequence > after_sequence`).
    ///
    /// `has_more` is decided by **fetching one extra row** (not `len == limit`: a count exactly equal to the limit would misreport, and a misreport
    /// sends the model chasing a page of history that does not exist).
    pub fn list_events(
        &self,
        run_id: &str,
        after_sequence: Option<u64>,
        limit: Option<usize>,
    ) -> EventPage {
        let limit = limit
            .unwrap_or(DEFAULT_LIST_EVENTS_LIMIT)
            .clamp(1, MAX_LIST_EVENTS_LIMIT);
        let mut events = self.journal.list_events(
            run_id,
            ListEventsOptions {
                after_sequence,
                limit: Some(limit + 1),
            },
        );
        let has_more = events.len() > limit;
        events.truncate(limit);
        EventPage {
            next_sequence: events.last().map(|event| event.sequence),
            events,
            has_more,
        }
    }

    /// Cold-start rebuild: fold this session's run events from the journal into `workflowRuns` with the same reducer.
    ///
    /// The state key is an in-memory projection; after a host restart it must be replayed once, otherwise the reconnect snapshot loses every run. The
    /// oldest runs are folded first (the reducer's maxRuns eviction keeps the most recent 8); each run's events are read to the end, paged by journal sequence.
    /// The derived fields come from the same sources as the realtime path: `toolCallId` from the run row, `actorSessionId` from `dwf_actor`
    /// (the native engine creates the actor session only after actor-created), `launchInputId` from run-launched.
    pub fn rebuild_workflow_runs(&self, session_id: &str) -> Option<Value> {
        let mut runs = self.list_runs_limited(Some(session_id), Some(8));
        runs.reverse();
        let mut state: Option<Value> = None;
        for run in runs {
            let mut actor_sessions: HashMap<(String, u32), String> = HashMap::new();
            for actor in self.journal.list_actors(&run.run_id) {
                if let Some(session) = actor.session_id {
                    actor_sessions.insert((actor.site_id, actor.ordinal), session);
                }
            }
            let mut launch_input_id: Option<String> = None;
            let mut after: Option<u64> = None;
            loop {
                let page = self.list_events(&run.run_id, after, None);
                for stored in &page.events {
                    let Ok(mut event) = serde_json::to_value(&stored.event) else {
                        continue;
                    };
                    let Some(object) = event.as_object_mut() else {
                        continue;
                    };
                    let Some(event_type) = object
                        .remove("type")
                        .and_then(|value| value.as_str().map(str::to_string))
                    else {
                        continue;
                    };
                    if event_type == "run-launched" {
                        launch_input_id = object
                            .get("inputId")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    let actor_key = event
                        .pointer("/actor/siteId")
                        .and_then(Value::as_str)
                        .zip(event.pointer("/actor/ordinal").and_then(Value::as_u64))
                        .map(|(site, ordinal)| (site.to_string(), ordinal as u32));
                    let mut envelope = json!({
                        "runId": &run.run_id,
                        "sequence": stored.sequence,
                        "eventType": event_type,
                        "payload": event,
                    });
                    if let Some(tool_call_id) = &run.tool_call_id {
                        envelope["toolCallId"] = json!(tool_call_id);
                    }
                    if let Some(key) = actor_key {
                        if let Some(session) = actor_sessions.get(&key) {
                            envelope["actorSessionId"] = json!(session);
                        }
                    }
                    if let Some(input_id) = &launch_input_id {
                        envelope["launchInputId"] = json!(input_id);
                    }
                    if let Some(next) =
                        crate::workflow_projection::reduce_workflow_runs(state.as_ref(), &envelope)
                    {
                        state = Some(next);
                    }
                }
                after = page.events.last().map(|event| event.sequence);
                if !page.has_more || after.is_none() {
                    break;
                }
            }
        }
        state
    }

    /// This run's node rows (ask / world-read / world-run / report / artifact), in journal insertion order.
    ///
    /// The observation surface of the detail page and of the amendment pre-flight: `messageBoundary` (the amend-resume truncation boundary) and
    /// `input` (the world-read args) exist only on this layer of rows.
    /// Node timestamps (a required field of the v4 workspace manifest); wholly absent when the introspection connection cannot be opened (the read surface degrades to an empty table).
    pub(crate) fn node_times(
        &self,
        run_id: &str,
    ) -> Vec<crate::workflow_service::reads::NodeTimeRow> {
        self.introspection.node_times(run_id).unwrap_or_default()
    }

    pub fn run_nodes(&self, run_id: &str) -> Vec<NodeRecord> {
        self.journal.list_nodes(run_id)
    }

    /// This run's user-facing artifact manifest (attached in any state: a run that died at step 12 still delivered the image from its earlier steps).
    pub fn list_artifacts(&self, run_id: &str) -> Vec<RunArtifact> {
        let nodes = self.journal.list_nodes(run_id);
        reads::merge_artifacts(&nodes)
    }

    /// Read the bytes of one artifact version.
    ///
    /// The authorization / locating chain (the same shape as TS `readWorkflowArtifactBytes`): the run row exists → the journal has a
    /// **completed** artifact row for `(id, version)` → the row carries a uri → resolve the target using **the row's** uri.
    /// The id sent by the renderer is only used to look rows up and never becomes a path directly; out-of-bounds / a missing file / a preset board are
    /// one and the same business fact for the caller (`None`), and separating them would only tell an over-reaching caller which half it guessed right.
    pub fn artifact_bytes(
        &self,
        run_id: &str,
        artifact_id: &str,
        version: u32,
    ) -> Option<ArtifactBytes> {
        self.journal.get_run(run_id)?;
        let nodes = self.journal.list_nodes(run_id);
        let (uri, content_type) = reads::locate_artifact_version(&nodes, artifact_id, version)?;
        // The row's uri is `nc-code-artifact://<runId>/<id>` (the driver's store convention). Parse it instead of trusting the input: a uri
        // pointing at another run must not read that row's bytes.
        let (uri_run, uri_id) = Self::parse_artifact_uri(&uri)?;
        if uri_run != run_id || uri_id != artifact_id {
            return None;
        }
        let path = self.artifact_file(run_id, artifact_id, version)?;
        let bytes = std::fs::read(path).ok()?;
        Some(ArtifactBytes {
            bytes,
            content_type,
        })
    }

    /// This run's workspace transcript manifest (the world-read / world-run node rows plus their inputs).
    /// An unknown run returns `None` (which is `run_not_found` for the caller).
    pub fn workflow_run_workspace(&self, run_id: &str) -> Option<Vec<WorkspaceNode>> {
        self.journal.get_run(run_id)?;
        Some(reads::workspace_nodes(&self.journal.list_nodes(run_id)))
    }

    /// The escalation questions a run has parked right now (in asking order).
    pub fn pending_questions(&self, run_id: &str) -> Vec<PendingQuestionView> {
        self.handle(run_id)
            .map(|handle| {
                reads::pending_question_views(
                    &handle.driver.escalation_registry().pending_questions(),
                )
            })
            .unwrap_or_default()
    }

    /// Answer a blocking question an actor escalated: look through each run's parked table → release the `escalate` tool that is waiting.
    /// **The run's status never moves** (an escalation is one slow tool call inside an ask, not a run lifecycle event).
    ///
    /// The qid is globally unique (`dwfq-<runId fragment>-<seq>`), but the registry is built per run, so this tries run by run.
    pub fn resolve_question(&self, qid: &str, answer: &str) -> ResolveQuestionResult {
        let handles: Vec<Arc<RunHandle>> = self
            .runs
            .lock()
            .map(|runs| runs.values().cloned().collect())
            .unwrap_or_default();
        for handle in &handles {
            if handle
                .driver
                .escalation_registry()
                .resolve_question(qid, answer)
            {
                return ResolveQuestionResult::Resolved {
                    qid: qid.to_string(),
                };
            }
        }
        // The exit log (inside this process) separates the three endings: answered → AlreadyResolved; withdrawn together with its ask →
        // RunNotInFlight; only anything else is UnknownQuestion. After a cross-process restart the log is empty and an old qid falls back to
        // UnknownQuestion — that fact is written into the fallback wording, so the model does not have to guess.
        let mut retired = None;
        for handle in &handles {
            if let Some(kind) = handle.driver.escalation_registry().retired_kind(qid) {
                retired = Some(kind);
                break;
            }
        }
        match retired {
            Some(crate::workflow::actor_tools::EscalationRetiredKind::Answered) => {
                ResolveQuestionResult::Refused {
                    qid: qid.to_string(),
                    reason: QuestionRefusal::AlreadyResolved,
                    message: format!(
                        "Question {qid} was already answered. Read the run's pendingQuestions for \
                         the ids that are still awaiting an answer."
                    ),
                }
            }
            Some(crate::workflow::actor_tools::EscalationRetiredKind::Withdrawn) => {
                ResolveQuestionResult::Refused {
                    qid: qid.to_string(),
                    reason: QuestionRefusal::RunNotInFlight,
                    message: format!(
                        "Question {qid} was withdrawn together with its ask (the run settled or \
                         the ask was cancelled); nobody is waiting on it. A resumed run makes the \
                         subagent ask again under a new id."
                    ),
                }
            }
            None => ResolveQuestionResult::Refused {
                qid: qid.to_string(),
                reason: QuestionRefusal::UnknownQuestion,
                message: format!(
                    "Unknown question id {qid}. It may be misspelled or left over from a process \
                     that is gone: parked questions are not persisted, so a resume makes the \
                     subagent ask again under a new id. Read the run's pendingQuestions for the \
                     ids that are actually awaiting an answer."
                ),
            },
        }
    }

    /// Whether a run is currently held by this service and not yet settled.
    pub fn is_live(&self, run_id: &str) -> bool {
        self.handle(run_id)
            .map(|handle| handle.is_live())
            .unwrap_or(false)
    }

    /// The number of in-flight runs under this service (the observation measure for settlement bookkeeping / the resident pool).
    pub fn count_live_runs(&self) -> usize {
        self.runs
            .lock()
            .map(|runs| runs.values().filter(|handle| handle.is_live()).count())
            .unwrap_or(0)
    }

    // ——————————————————————— orphan convergence (invariant 4) ———————————————————————

    /// Converge orphan runs at construction time: a `running` row is decidable only at that moment (zero in-flight runs under this service).
    ///
    /// The convergence writes `stopped(interrupted)` (carrying the `Interrupted` failure code), resumable like every other stopped.
    /// The three boundaries are item by item the same as TS `reconcileOrphanRuns`:
    ///   - **no dwf_event is synthesized**: the event log's contract is "what the engine emitted", and the status authority is on the run row;
    ///     `run-settled` is the engine's closing, not the sweeper's;
    ///   - **a failed convergence never sinks the construction**: a query or write failure logs one stderr line (the only consequence is that the lie is still in the row);
    ///   - **scope**: TS uses `parentSessionId` (the session store is shared by all sessions); the native journal file is held exclusively by this
    ///     host (`<data-dir>` private), so the file is the scope — equivalence and its residual risk are in the file header.
    fn reconcile_orphan_runs(&self) {
        let ids = match self.introspection.non_terminal_run_ids(None) {
            Ok(ids) => ids,
            Err(error) => {
                eprintln!("[nc-code-host] workflow orphan run query failed: {error}");
                return;
            }
        };
        for run_id in ids {
            // The authority for the terminal decision is the decoded logical status; the SQL status filter is only an index-friendly pre-filter.
            let Some(record) = self.journal.get_run(&run_id) else {
                continue;
            };
            if reads::is_terminal(record.status) {
                continue;
            }
            let settlement = RunSettlementRecord {
                stop_reason: Some(RunStopReason::Interrupted),
                superseded_by: None,
                failure: Some(interrupted_run_failure(&run_id)),
                result: None,
            };
            // The journal port's error strategy is panic (a synchronous port has no Result); convergence is a self-healing action rather than a
            // precondition of a run, so the panic is folded into one local warning here.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.journal
                    .update_run_status(&run_id, RunStatus::Stopped, Some(&settlement));
            }));
            match outcome {
                Ok(()) => eprintln!(
                    "[nc-code-host] workflow run {run_id} reconciled as interrupted (was {})",
                    record.status.as_str()
                ),
                Err(_) => {
                    eprintln!(
                        "[nc-code-host] workflow orphan run reconciliation failed for {run_id}"
                    )
                }
            }
        }
    }

    // ——————————————————————— assembly / launch ———————————————————————

    /// Compile once, routed by the language the script is written in (see
    /// [`super::compile`]). Diagnostics are a business failure (`CompileFailed`); a kernel
    /// fault is an internal one.
    ///
    /// An NWS script never reaches the kernel: it is lexed, parsed, resolved and planned in
    /// this process, which is the point of the language — an NWS run needs no Node
    /// executable at all, for compiling or for executing.
    async fn compile_once(&self, script_text: &str) -> ServiceResult<CompiledWorkflow> {
        if detect_language(script_text) == WorkflowLanguage::Nws {
            return match compile_nws(script_text) {
                Ok(plan) => Ok(CompiledWorkflow::Nws(plan)),
                Err(diagnostics) => Err(WorkflowServiceError::CompileFailed {
                    diagnostics: nws_diagnostics(&diagnostics),
                }),
            };
        }
        let kernel = self.kernel().await?;
        match kernel.compile(script_text).await {
            Ok(compiled) => Ok(CompiledWorkflow::TypeScript(Arc::new(compiled))),
            Err(CompileError::Diagnostics(diagnostics)) => {
                Err(WorkflowServiceError::CompileFailed { diagnostics })
            }
            Err(CompileError::Kernel(message)) => Err(WorkflowServiceError::CompileKernel(message)),
        }
    }

    /// Take (spawning on first use) the resident compile kernel. Spawn inside the lock: two concurrent first compiles start only one process.
    async fn kernel(&self) -> ServiceResult<Arc<CompileKernel>> {
        let mut guard = self.kernel.lock().await;
        if let Some(kernel) = guard.as_ref() {
            return Ok(Arc::clone(kernel));
        }
        let kernel = Arc::new(
            CompileKernel::spawn()
                .await
                .map_err(|error| WorkflowServiceError::CompileKernel(error.to_string()))?,
        );
        *guard = Some(Arc::clone(&kernel));
        Ok(kernel)
    }

    /// The single launch path of submit / amend / resume: assemble the driver → construct the engine → **register the handle** →
    /// spawn the sandbox (fire-and-forget).
    ///
    /// Registering before launching is load-bearing (invariant 5): `cancel` may arrive at any moment after the entry returns, and the read surfaces
    /// can see this run from that very moment.
    fn launch(&self, spec: LaunchSpec) -> ServiceResult<()> {
        self.assert_open()?;
        // A TypeScript compile without a lowered body is a wiring fault. An NWS plan has no
        // textual body at all — its executable form is the tree — so the guard is made
        // against the runtime that is about to run it.
        let nws_plan = match &spec.compiled {
            CompiledWorkflow::TypeScript(compiled) => {
                if compiled.lowered.is_none() {
                    return Err(WorkflowServiceError::Internal(format!(
                        "workflow run {} has a clean compile without a lowered body",
                        spec.run_id
                    )));
                }
                None
            }
            CompiledWorkflow::Nws(plan) => Some(Arc::clone(plan)),
        };
        let lowered = spec.compiled.lowered().map(str::to_string);
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| WorkflowServiceError::SessionUnavailable {
                session_id: spec.session_id.clone(),
                run_id: Some(spec.run_id.clone()),
            })?;
        let parent = host.load_session(&spec.session_id).ok_or_else(|| {
            WorkflowServiceError::SessionUnavailable {
                session_id: spec.session_id.clone(),
                run_id: Some(spec.run_id.clone()),
            }
        })?;

        // One realtime event channel per run: the driver's emit only forwards; the real consumer is the sink this service installed.
        // unbounded + a dedicated forwarding task = a slow consumer can never back-pressure the engine's record path.
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, Value)>();
        let event_sink = self.event_sink.clone();
        let events_run_id = spec.run_id.clone();
        let events_session_id = spec.session_id.clone();
        let events_tool_call_id = spec.tool_call_id.clone();
        let events_launch_input_id = spec.launch_input_id.clone();
        let events_journal = Arc::clone(&self.journal);
        tokio::spawn(async move {
            let Some(sink) = event_sink else {
                // Drain even when there is no consumer: otherwise the values in the channel pile up forever (the driver's send never fails).
                while events_rx.recv().await.is_some() {}
                return;
            };
            // `actor-created` is recorded **before** the session is really created (the engine creates actor sessions lazily), so the
            // event itself carries no host session id. The run service patches it here: remember the original actor-created
            // payload, and once the journal's dwf_actor.session_id shows up, re-emit a same-shape event carrying
            // `__actorSessionId` **before the next event** — the reducer's actor table upserts by siteId/ordinal, so the
            // re-emission only fills in the sessionId (status is recomputed from the nodes), which makes it equivalent to
            // the TS driver's events that already carry an id at creation time.
            let mut actor_created: std::collections::HashMap<(String, u32), Value> =
                std::collections::HashMap::new();
            let mut actor_session_sent: std::collections::HashSet<(String, u32)> =
                std::collections::HashSet::new();
            while let Some((sequence, event)) = events_rx.recv().await {
                if event.get("type").and_then(Value::as_str) == Some("actor-created") {
                    if let (Some(site), Some(ordinal)) = (
                        event.pointer("/actor/siteId").and_then(Value::as_str),
                        event.pointer("/actor/ordinal").and_then(Value::as_u64),
                    ) {
                        actor_created.insert((site.to_string(), ordinal as u32), event.clone());
                    }
                }
                // Re-emit for every actor of this round that has a known actor-created but no session id yet.
                let pending: Vec<(String, u32)> = actor_created
                    .keys()
                    .filter(|key| !actor_session_sent.contains(key))
                    .cloned()
                    .collect();
                for (site, ordinal) in pending {
                    let session_id = events_journal
                        .get_actor(&events_run_id, &site, ordinal)
                        .and_then(|record| record.session_id);
                    let Some(session_id) = session_id else {
                        continue;
                    };
                    let Some(template) = actor_created.get(&(site.clone(), ordinal)) else {
                        continue;
                    };
                    let mut enriched = template.clone();
                    if let Some(object) = enriched.as_object_mut() {
                        object.insert("__actorSessionId".to_string(), Value::String(session_id));
                    }
                    sink(
                        &events_session_id,
                        &events_run_id,
                        events_tool_call_id.as_deref(),
                        events_launch_input_id.as_deref(),
                        sequence,
                        enriched,
                    );
                    actor_session_sent.insert((site, ordinal));
                }
                sink(
                    &events_session_id,
                    &events_run_id,
                    events_tool_call_id.as_deref(),
                    events_launch_input_id.as_deref(),
                    sequence,
                    event,
                );
            }
        });

        let journal_port: Arc<dyn JournalStorePort> = self.journal.clone();
        let declared_run_commands: BTreeSet<String> = spec
            .compiled
            .declared_run_commands()
            .iter()
            .cloned()
            .collect();
        let driver = HostWorkflowDriver::new(
            HostWorkflowDriverConfig {
                run_id: spec.run_id.clone(),
                data_dir: self.data_dir.clone(),
                // The authorization surface of `world.run`: literals collected at compile time; absent means refusing every world.run.
                // An empty set has the same outcome as "undeclared" (fail-closed), so Some is always given.
                declared_run_commands: Some(declared_run_commands),
                events: Some(events_tx),
            },
            Arc::clone(&host),
            parent,
            journal_port,
        );

        let ask_specs: HashMap<String, AskSpec> = spec
            .compiled
            .ask_specs()
            .iter()
            .map(|(site_id, spec)| (site_id.clone(), spec.clone().into_engine()))
            .collect();
        let launch = RunLaunchConfig {
            input_id: spec
                .launch_input_id
                .clone()
                .unwrap_or_else(|| self.next_id(INPUT_ID_PREFIX)),
            phase_names: spec.phase_names.clone(),
            subagent_model: spec.subagent_model.clone(),
            script_path: spec.script_path.clone(),
            phase_alongside: spec.phase_alongside.clone(),
        };
        let engine = WorkflowEngine::new(EngineConfig {
            run_id: spec.run_id.clone(),
            driver: Arc::clone(&driver) as Arc<dyn WorkflowDriver>,
            caps: spec.caps.clone(),
            ask_specs,
            // TS injects the adapter package's `validate`; native has no schema dependency, so its verbatim port is injected
            // instead (`ask_validate`). Without it, the typed ask's repair loop fails wholesale.
            validate: Arc::new(validate),
            script_text: Some(spec.script_text.clone()),
            name: spec.name.clone(),
            // What lands in storage is the **author's original text** and its sha256, not the lowered body (resume compares against the original text).
            script_hash: Some(spec.compiled.script_hash().to_string()),
            args: spec.args.clone(),
            parent_session_id: Some(spec.session_id.clone()),
            cwd: Some(spec.cwd.clone()),
            tool_call_id: spec.tool_call_id.clone(),
            resumed_from: spec.resumed_from.clone(),
            // The anchor reaches the journal only in the life that creates the run (the gate is engine-side); resume passing it is harmless.
            launch: Some(launch),
            inherited_tokens: spec.inherited_tokens,
            imported_cache: spec.imported_cache,
        })
        .map_err(|error| engine_construction_error(&spec.run_id, error))?;
        // The driver exists before the sink; after construction the engine attaches itself as the sink (Weak, so no cycle).
        driver.attach_sink(
            Arc::clone(&engine) as Arc<dyn nc_code_workflow::types::WorkflowReportSink>
        );

        let slot = Arc::new(SettlementSlot::default());
        let handle = Arc::new(RunHandle {
            run_id: spec.run_id.clone(),
            session_id: spec.session_id.clone(),
            engine: Arc::clone(&engine),
            driver,
            settlement: Arc::clone(&slot),
        });
        {
            let mut runs = self
                .runs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // resume replaces the existing entry (invariant 5's "replace first, then start"); only a **live** entry with the same id is a
            // wiring fault (two engines sharing one runId would write two journals that step on each other).
            if let Some(existing) = runs.get(&spec.run_id) {
                if existing.is_live() {
                    return Err(WorkflowServiceError::Internal(format!(
                        "workflow run {} already has a live engine in this service",
                        spec.run_id
                    )));
                }
            }
            runs.insert(spec.run_id.clone(), handle);
        }

        // No wall-clock timeout on either path: caps bound concurrency only, and
        // cancellation is the only way to stop a run (unchanged from the TypeScript runtime).
        // The run's stored args are replayed on resume; an absent value reads as `{}`.
        let args = spec.args.clone();
        let cwd = spec.cwd.clone();
        let run_id = spec.run_id.clone();
        match (nws_plan, lowered) {
            (Some(plan), _) => {
                let mut options = interpret::ScriptOptions::new(plan);
                options.args = args;
                tokio::spawn(async move {
                    let guard = SettlementGuard {
                        slot,
                        finished: false,
                    };
                    let settlement = interpret::run_script(engine, options).await;
                    guard.finish(settlement);
                });
            }
            (None, Some(lowered)) => {
                let mut options = SandboxOptions::new(run_id, lowered, cwd);
                options.args = args;
                tokio::spawn(async move {
                    let guard = SettlementGuard {
                        slot,
                        finished: false,
                    };
                    let settlement = sandbox::run_workflow_script(engine, options).await;
                    guard.finish(settlement);
                });
            }
            (None, None) => {
                // Unreachable: the guard above rejects a TypeScript compile with no body.
                return Err(WorkflowServiceError::Internal(format!(
                    "workflow run {run_id} has a compile this host cannot execute",
                )));
            }
        }
        Ok(())
    }

    fn resolve_cwd(&self, requested: Option<String>) -> String {
        requested
            .filter(|cwd| !cwd.trim().is_empty())
            .unwrap_or_else(|| self.workflow_cwd.to_string_lossy().to_string())
    }

    fn handle(&self, run_id: &str) -> Option<Arc<RunHandle>> {
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(run_id)
            .cloned()
    }

    fn mint_run_id(&self) -> String {
        // The repo deliberately avoids uuid for ids (see `store::generate_id`'s comment); the same minting scheme yields ids that are
        // `[A-Za-z0-9-]`-safe, satisfying TS's character-set constraint for run ids (they go into URLs / paths / logs).
        crate::store::generate_id(RUN_ID_PREFIX, &self.id_counter)
    }

    fn next_id(&self, prefix: &str) -> String {
        crate::store::generate_id(prefix, &self.id_counter)
    }

    /// Read the host metadata on the first `run-launched` (the anchor / subagent model / script path).
    /// Only the journal head is scanned (that event follows `run-started` immediately).
    fn read_run_launch_header(&self, run_id: &str) -> Option<RunLaunchHeader> {
        let stored = self.journal.list_events(
            run_id,
            ListEventsOptions {
                after_sequence: None,
                limit: Some(RUN_LAUNCH_SCAN_LIMIT),
            },
        );
        for event in stored {
            if let RunEvent::RunLaunched {
                input_id,
                subagent_model,
                script_path,
                ..
            } = event.event
            {
                return Some(RunLaunchHeader {
                    input_id,
                    subagent_model,
                    script_path,
                });
            }
        }
        None
    }

    /// The artifact store's landing point: `<data_dir>/workflow-artifacts/<runId>/<id>-v<n>.<ext>`.
    ///
    /// File names are matched by **directory scan** (the `<id>-v<n>.` prefix + a delimiter-free extension) rather than by concatenation: even an id
    /// carrying hostile characters cannot compose a path traversal this way, and the extension is computed by the driver from the content type/source file, so the read surface does not have to copy that table.
    fn artifact_file(&self, run_id: &str, artifact_id: &str, version: u32) -> Option<PathBuf> {
        let directory = self
            .data_dir
            .join("workflow-artifacts")
            .join(sanitize_path_segment(run_id));
        let prefix = format!("{artifact_id}-v{version}.");
        let entries = std::fs::read_dir(directory).ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(extension) = name.strip_prefix(&prefix) else {
                continue;
            };
            if !extension.is_empty() && extension.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Some(entry.path());
            }
        }
        None
    }

    /// The `uri` on the row (`nc-code-artifact://<runId>/<id>`) → `(runId, id)`.
    fn parse_artifact_uri(uri: &str) -> Option<(String, String)> {
        let rest = uri.strip_prefix("zcode-artifact://")?;
        let (run_id, artifact_id) = rest.split_once('/')?;
        if run_id.is_empty() || artifact_id.is_empty() {
            return None;
        }
        Some((run_id.to_string(), artifact_id.to_string()))
    }

    // ——————————————————————— synthesizing the list rows ———————————————————————

    fn list_item(&self, row: &reads::RunListRow) -> Option<RunListItem> {
        let record = self.journal.get_run(&row.run_id)?;
        let handle = self.handle(&row.run_id);
        let terminal = handle.as_ref().and_then(|handle| handle.settlement.get());
        let live = terminal.is_none() && handle.is_some();
        let status = reads::synthesize_run_status(terminal.as_ref(), live, Some(record.status));
        let stop_reason = reads::stop_reason_of(terminal.as_ref(), live, Some(&record));
        let (label, label_source) = reads::run_label(
            &row.run_id,
            record.name.as_deref(),
            record.script_text.as_deref(),
        );
        // The same precedence as the snapshot: under a live engine, a terminal state/failure on the row is a foreign write and takes no part in the synthesis.
        let failure = if reads::is_terminal(status) {
            reads::failure_of(terminal.as_ref(), Some(&record))
        } else {
            None
        };
        Some(RunListItem {
            run_id: row.run_id.clone(),
            label,
            label_source,
            status: reads::fold_snapshot_status(status),
            run_status: reads::is_terminal(status).then_some(status),
            stop_reason,
            resumed_from: record.resumed_from.clone(),
            superseded_by: reads::superseded_by_of(terminal.as_ref(), Some(&record)),
            parent_session_id: record.parent_session_id.clone(),
            tool_call_id: record.tool_call_id.clone(),
            spent_tokens: record.spent_tokens,
            resumable: reads::is_resumable(status, stop_reason),
            failure_code: failure
                .as_ref()
                .map(|failure| failure.code.as_str().to_string()),
            failure_message: failure.as_ref().map(|failure| failure.message.clone()),
            live,
            created_at: row.time_created,
            updated_at: row.time_updated,
        })
    }
}

// ————————————————————————————————————————————————————————————————
// Pure helpers
// ————————————————————————————————————————————————————————————————

/// `max(1, min(16, cores − 2))`: both each run's own concurrency bound and the starting point of the process-level bucket (the same TS formula).
fn resolve_concurrency_ceiling() -> u32 {
    let cores = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get() as u64)
        .unwrap_or(1);
    let adjusted = cores.saturating_sub(u64::from(CONCURRENCY_RESERVED));
    u32::try_from(adjusted)
        .unwrap_or(u32::MAX)
        .clamp(CONCURRENCY_FLOOR, CONCURRENCY_CEILING_MAX)
}

/// The requested concurrency bound → the bound actually in effect for this run. **Clamp rather than refuse** (an oversized value expresses the
/// intent "do not limit me"; turning it into a failure only sends the model guessing how many cores the machine has).
fn clamp_run_concurrency(requested: Option<u32>, ceiling: u32) -> u32 {
    match requested {
        None => ceiling,
        Some(requested) => requested.clamp(1, ceiling.max(1)),
    }
}

fn cancel_parts(
    run_id: &str,
    initiator: CancelInitiator,
) -> (RunStopReason, Option<WorkflowError>) {
    match initiator {
        CancelInitiator::User => (RunStopReason::User, None),
        CancelInitiator::Model => (RunStopReason::Model, None),
        CancelInitiator::Interrupted => (
            RunStopReason::Interrupted,
            Some(WorkflowError::new(
                WorkflowErrorCode::Interrupted,
                format!("dynamic workflow run {run_id} was interrupted"),
            )),
        ),
    }
}

/// The failure code the orphan convergence writes: **its own** `Interrupted` (not `DriverError` — a script that throws is encoded as that
/// too, and with the same code only the wording could tell "the process was killed" from "the script genuinely failed"); the same wording as TS `interruptedRunFailure`.
fn interrupted_run_failure(run_id: &str) -> WorkflowErrorJson {
    WorkflowError::new(
        WorkflowErrorCode::Interrupted,
        format!(
            "dynamic workflow run {run_id} was interrupted: the owning process exited before the \
             run settled"
        ),
    )
    .to_json()
}

/// An engine construction failure → a service error. `ScriptHashMismatch` has its own variant (it is resume's named refusal).
fn engine_construction_error(run_id: &str, error: WorkflowError) -> WorkflowServiceError {
    if error.code == WorkflowErrorCode::ScriptHashMismatch {
        let (expected, got) = error
            .mismatch
            .as_ref()
            .map(|mismatch| (mismatch.expected.clone(), mismatch.got.clone()))
            .unwrap_or_default();
        return WorkflowServiceError::ScriptMismatch {
            run_id: run_id.to_string(),
            expected,
            got,
        };
    }
    WorkflowServiceError::Engine {
        code: error.code,
        message: error.message,
    }
}

/// The folding of a runId before it enters a directory name (the same character set as the driver's artifact store).
fn sanitize_path_segment(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "run".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_workflow::types::NodeKind;

    #[test]
    fn concurrency_ceiling_matches_the_ts_formula() {
        // The formula itself: max(1, min(16, cores − 2)). The floor and the hard cap are each pinned once.
        let ceiling = resolve_concurrency_ceiling();
        assert!(
            (1..=CONCURRENCY_CEILING_MAX).contains(&ceiling),
            "{ceiling}"
        );
        assert_eq!(clamp_run_concurrency(None, 8), 8, "absent = ceiling");
        assert_eq!(clamp_run_concurrency(Some(3), 8), 3);
        assert_eq!(
            clamp_run_concurrency(Some(99), 8),
            8,
            "never above the ceiling"
        );
        assert_eq!(clamp_run_concurrency(Some(0), 8), 1, "never below one");
    }

    #[test]
    fn artifact_uris_are_parsed_not_trusted() {
        assert_eq!(
            WorkflowService::parse_artifact_uri("zcode-artifact://dwfrun-1/summary"),
            Some(("dwfrun-1".to_string(), "summary".to_string()))
        );
        assert_eq!(
            WorkflowService::parse_artifact_uri("file:///etc/passwd"),
            None
        );
        assert_eq!(
            WorkflowService::parse_artifact_uri("zcode-artifact://run/"),
            None
        );
        assert_eq!(
            WorkflowService::parse_artifact_uri("zcode-artifact:///x"),
            None
        );
    }

    #[test]
    fn cancel_parts_name_the_initiator_and_only_interrupted_carries_a_failure() {
        assert_eq!(
            cancel_parts("r", CancelInitiator::User).0,
            RunStopReason::User
        );
        assert!(cancel_parts("r", CancelInitiator::User).1.is_none());
        assert_eq!(
            cancel_parts("r", CancelInitiator::Model).0,
            RunStopReason::Model
        );
        let (reason, error) = cancel_parts("run-7", CancelInitiator::Interrupted);
        assert_eq!(reason, RunStopReason::Interrupted);
        let error = error.expect("interrupted carries a structured error");
        assert_eq!(error.code, WorkflowErrorCode::Interrupted);
        assert!(error.message.contains("run-7"));
    }

    #[test]
    fn node_kind_is_imported_for_the_read_side() {
        // This assertion exists only to pin `NodeKind`'s dependency at compile time (the read surface's ordering uses its discriminant).
        assert_eq!(
            serde_json::to_value(NodeKind::WorldRun).unwrap(),
            serde_json::json!("world-run")
        );
    }
}
