//! Boundary vocabulary ported from
//! `apps/nc-code-cli/packages/dynamic-workflow/src/engine/types.ts` (plus
//! `ask-observation-types.ts`, `imported-cache-types.ts`, `run-terminal.ts`,
//! `schema/types.ts`, `facade/registry.ts`, `facade/*-caps.ts`).
//!
//! This module is the contract every other module speaks. The wire strings are not
//! cosmetically chosen: `RunEvent` JSON is journaled byte-for-byte (`dwf_event`), and
//! `WorkflowErrorCode` values are compared by readers — they must match the TS
//! serializer exactly. Serde attributes below pin that.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// ————————————————————————————————————————————————————————————————
// identity
// ————————————————————————————————————————————————————————————————

/// ask / world / actor instance identity: site id x per-site execution ordinal (ordinal starts at 1).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRef {
    pub site_id: String,
    pub ordinal: u32,
}

/// actor identity: creating site x ordinal (isomorphic to [`InstanceRef`], but the semantics are an actor).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActorRef {
    pub site_id: String,
    pub ordinal: u32,
}

/// The opaque actor handle held by the script (rendered as `site@ordinal`; the format is invisible to the script but stable and parseable).
pub type ActorId = String;

/// Render a ref as the stable string `site@ordinal` (for logs and handles).
pub fn ref_to_string(site_id: &str, ordinal: u32) -> String {
    format!("{site_id}@{ordinal}")
}

impl InstanceRef {
    pub fn key(&self) -> String {
        ref_to_string(&self.site_id, self.ordinal)
    }
}

impl ActorRef {
    pub fn key(&self) -> String {
        ref_to_string(&self.site_id, self.ordinal)
    }
}

// ————————————————————————————————————————————————————————————————
// persona / ask message
// ————————————————————————————————————————————————————————————————

/// The frozen actor persona (only the identity remains: name + system prompt).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonaSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
}

/// The downlink message of a single ask: instruction body + typed flag and schema (opaque to the core, only passed through).
#[derive(Clone, Debug, PartialEq)]
pub struct AskMessage {
    pub instructions: String,
    pub typed: bool,
    pub schema: Option<Value>,
}

/// The static spec of each ask site. An untyped site records `typed: false` explicitly (an absence is a wiring error).
#[derive(Clone, Debug, PartialEq)]
pub struct AskSpec {
    pub typed: bool,
    pub schema: Option<Value>,
}

// ————————————————————————————————————————————————————————————————
// validation
// ————————————————————————————————————————————————————————————————

/// A single validation violation, usable directly in the repair tool_result (JSON path + expected + received).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    pub path: String,
    pub expected: String,
    pub got: String,
}

/// The contract of the injected validation function: an empty list means it passed.
pub type ValidateFn =
    std::sync::Arc<dyn Fn(Option<&Value>, &Value) -> Vec<Violation> + Send + Sync>;

// ————————————————————————————————————————————————————————————————
// stats / budget / observation vocabulary
// ————————————————————————————————————————————————————————————————

/// The character cap for `instructionsHead` on `node-queued`.
pub const INSTRUCTIONS_HEAD_MAX_CHARS: usize = 240;
/// The character cap for `lastTool.name` on `node-progress`.
pub const LAST_TOOL_NAME_MAX_CHARS: usize = 64;
/// The character cap for `lastTool.target` on `node-progress`.
pub const LAST_TOOL_TARGET_MAX_CHARS: usize = 120;
/// The cap on repair (reject-and-retry) attempts within a node.
pub const REPAIR_ATTEMPTS: u32 = 3;
/// The cap on nudges when a turn ends without a submit.
pub const NUDGE_ATTEMPTS: u32 = 1;
/// The byte cap for a serialized `WorldReadInput` (4 KB).
pub const WORLD_READ_INPUT_MAX_BYTES: usize = 4096;

/// The usage statistics the driver reports after completing one ask.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskStats {
    pub tokens: u64,
    pub tool_calls: u64,
    pub turns: u64,
    /// Of these, the calls that looked at or changed the external world (excluding submit_result / escalate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_tool_calls: Option<u64>,
}

/// The most recently observed tool call (`target` is a hint for humans to read, not the arguments).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskLastTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// The progress of one ask at the moment its turn was parsed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskProgress {
    pub turn: u64,
    pub tool_calls: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<AskLastTool>,
}

/// The model request of an in-flight ask is waiting (process-level admission gate or backoff retry).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskWaitInfo {
    pub cause: AskWaitCause,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskWaitCause {
    Slot,
    Backoff,
}

/// The reason for a change to the process-level concurrency cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyChangeReason {
    RateLimited,
    ProviderOverloaded,
    OffpeakQueued,
    Recovered,
    IdleReset,
}

/// A governor's cap adjustment for a given provider key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConcurrencyChange {
    pub key: String,
    pub previous: u32,
    pub next: u32,
    pub reason: ConcurrencyChangeReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_good: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_bad: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_ms: Option<f64>,
}

/// Run-level capacity caps: fixed at submit time and stored in dwf_run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Caps {
    pub max_concurrency: u32,
}

/// The two reasons the import cache gate closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportCloseCause {
    MutatingTool,
    WorldRun,
}

// ————————————————————————————————————————————————————————————————
// structured errors (errors are first-class)
// ————————————————————————————————————————————————————————————————

/// Stable error codes. Node-level and run-level are divided by the "rejection channel" (see the TS contract comments).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkflowErrorCode {
    ValidationFailed,
    ResultNotSubmitted,
    DriverError,
    WorldReadCapExceeded,
    Cancelled,
    ContextLimit,
    ReportCapExceeded,
    InputHashMismatch,
    UnknownActor,
    MissingAskSpec,
    DuplicateActorName,
    ScriptHashMismatch,
    Interrupted,
    ProviderStop,
    // The user-facing artifact (artifact) channel: content members are node-level, preset members failRun.
    ArtifactSourceMissing,
    ArtifactPathOutsideWorkspace,
    ArtifactTooLarge,
    ArtifactStoreUnavailable,
    ArtifactVersionCapExceeded,
    ArtifactKindMismatch,
    ArtifactCapExceeded,
    ArtifactSpecInvalid,
    ArtifactRedeclared,
    ArtifactUndeclared,
    ArtifactPrimaryConflict,
}

impl WorkflowErrorCode {
    /// TS serializes it as a PascalCase string (character-for-character the same as the variant name).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ValidationFailed => "ValidationFailed",
            Self::ResultNotSubmitted => "ResultNotSubmitted",
            Self::DriverError => "DriverError",
            Self::WorldReadCapExceeded => "WorldReadCapExceeded",
            Self::Cancelled => "Cancelled",
            Self::ContextLimit => "ContextLimit",
            Self::ReportCapExceeded => "ReportCapExceeded",
            Self::InputHashMismatch => "InputHashMismatch",
            Self::UnknownActor => "UnknownActor",
            Self::MissingAskSpec => "MissingAskSpec",
            Self::DuplicateActorName => "DuplicateActorName",
            Self::ScriptHashMismatch => "ScriptHashMismatch",
            Self::Interrupted => "Interrupted",
            Self::ProviderStop => "ProviderStop",
            Self::ArtifactSourceMissing => "ArtifactSourceMissing",
            Self::ArtifactPathOutsideWorkspace => "ArtifactPathOutsideWorkspace",
            Self::ArtifactTooLarge => "ArtifactTooLarge",
            Self::ArtifactStoreUnavailable => "ArtifactStoreUnavailable",
            Self::ArtifactVersionCapExceeded => "ArtifactVersionCapExceeded",
            Self::ArtifactKindMismatch => "ArtifactKindMismatch",
            Self::ArtifactCapExceeded => "ArtifactCapExceeded",
            Self::ArtifactSpecInvalid => "ArtifactSpecInvalid",
            Self::ArtifactRedeclared => "ArtifactRedeclared",
            Self::ArtifactUndeclared => "ArtifactUndeclared",
            Self::ArtifactPrimaryConflict => "ArtifactPrimaryConflict",
        }
    }
}

/// The structured comparison of a value mismatch (expected vs received).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowErrorMismatch {
    pub expected: String,
    pub got: String,
}

/// The serializable form of an error (written to `dwf_node.error_json` / `dwf_run.failure_json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowErrorJson {
    pub code: WorkflowErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub violations: Option<Vec<Violation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mismatch: Option<WorkflowErrorMismatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_stop: Option<ProviderStopDetails>,
}

/// The structured error thrown across Boundary A.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowError {
    pub code: WorkflowErrorCode,
    pub message: String,
    pub violations: Option<Vec<Violation>>,
    pub final_text: Option<String>,
    pub mismatch: Option<WorkflowErrorMismatch>,
    pub provider_stop: Option<ProviderStopDetails>,
    /// The root cause, for diagnostics (TS's `cause`, never written to the journal).
    pub cause: Option<String>,
}

impl WorkflowError {
    pub fn new(code: WorkflowErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            violations: None,
            final_text: None,
            mismatch: None,
            provider_stop: None,
            cause: None,
        }
    }

    pub fn with_violations(mut self, violations: Vec<Violation>) -> Self {
        self.violations = Some(violations);
        self
    }

    pub fn with_final_text(mut self, final_text: impl Into<String>) -> Self {
        self.final_text = Some(final_text.into());
        self
    }

    pub fn with_mismatch(mut self, expected: impl Into<String>, got: impl Into<String>) -> Self {
        self.mismatch = Some(WorkflowErrorMismatch {
            expected: expected.into(),
            got: got.into(),
        });
        self
    }

    pub fn with_provider_stop(mut self, details: ProviderStopDetails) -> Self {
        self.provider_stop = Some(details);
        self
    }

    pub fn with_cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
        self
    }

    /// Convert into the serializable form and write it to the journal.
    pub fn to_json(&self) -> WorkflowErrorJson {
        WorkflowErrorJson {
            code: self.code,
            message: self.message.clone(),
            violations: self.violations.clone(),
            final_text: self.final_text.clone(),
            mismatch: self.mismatch.clone(),
            provider_stop: self.provider_stop.clone(),
        }
    }

    /// Rebuild from a journal record (used when replay hits a failed node).
    pub fn from_json(json: &WorkflowErrorJson) -> Self {
        Self {
            code: json.code,
            message: json.message.clone(),
            violations: json.violations.clone(),
            final_text: json.final_text.clone(),
            mismatch: json.mismatch.clone(),
            provider_stop: json.provider_stop.clone(),
            cause: None,
        }
    }
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for WorkflowError {}

/// The structured details of `ProviderStop` (notifications and GetWorkflowRun choose their copy from it and never parse message).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStopDetails {
    pub kind: ProviderStopKind,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStopKind {
    Auth,
    NotConfigured,
    ModelUnavailable,
    InvalidRequest,
    Quota,
    Other,
}

/// A run-level stall observation (observed by the driver; the engine only records it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStallInfo {
    pub since_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<u32>,
}

// ————————————————————————————————————————————————————————————————
// user-facing artifacts (Boundary A vocabulary)
// ————————————————————————————————————————————————————————————————

/// The op derived from the artifact registry (six members: file/markdown of the content family, chart/table/metrics/board of the preset family).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactOp {
    File,
    Markdown,
    Chart,
    Table,
    Metrics,
    Board,
}

impl ArtifactOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Markdown => "markdown",
            Self::Chart => "chart",
            Self::Table => "table",
            Self::Metrics => "metrics",
            Self::Board => "board",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "file" => Self::File,
            "markdown" => Self::Markdown,
            "chart" => Self::Chart,
            "table" => Self::Table,
            "metrics" => Self::Metrics,
            "board" => Self::Board,
            _ => return None,
        })
    }

    /// The content members (async, via the driver, rejectable).
    pub fn is_content(&self) -> bool {
        matches!(self, Self::File | Self::Markdown)
    }

    /// The preset members (synchronous void, not via the driver).
    pub fn is_preset(&self) -> bool {
        !self.is_content()
    }
}

/// The artifact reference handed to the script: id + the version number minted by this publish.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,
    pub version: u32,
}

/// The persisted form of one artifact version (the shape of `dwf_node.result_json`). Bytes never enter the journal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactVersionRecord {
    pub id: String,
    pub kind: ArtifactOp,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<f64>,
    /// Stamped by the engine: this id is this run's deliverable (sticky by id; once set, every later version carries it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<bool>,
}

/// The request the driver executes for one content artifact publish (version is computed by the engine).
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactPublishRequest {
    pub run_id: String,
    pub site_id: String,
    pub ordinal: u32,
    pub op: ArtifactOp,
    pub id: String,
    pub version: u32,
    /// `file`: a workspace-relative path.
    pub path: Option<String>,
    /// `markdown`: the body text.
    pub content: Option<String>,
    /// The opts given by the script (passed through verbatim; shape validation belongs to the driver).
    pub opts: Option<Value>,
}

// ————————————————————————————————————————————————————————————————
// Boundary B：driver port
// ————————————————————————————————————————————————————————————————

/// An opaque reference to an actor session (in production this is a zcode session id).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionRef {
    pub id: String,
}

/// The core's verdict on one submit attempt, delivered down via [`WorkflowDriver::respond_to_submit`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SubmitVerdict {
    Accept,
    Reject { violations: Vec<Violation> },
    Nudge,
}

/// Session seed: handed to the driver on a diverged actor's first live dispatch, so the new session opens with the source session's full-fidelity transcript prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorSessionSeed {
    pub source_session_id: String,
    pub message_count: u64,
    pub resolved_model: Option<String>,
}

/// The downlink port: the core asks the driver to perform side effects. The core is a pure deterministic state machine.
#[async_trait::async_trait]
pub trait WorkflowDriver: Send + Sync {
    fn journal(&self) -> &dyn JournalStorePort;
    fn emit(&self, event: &RunEvent);
    /// Fan-out after recording: `sequence` is the return value of the journal's `append_event` (the same yardstick
    /// as the read surface's `lastEventSequence` for live projection). Defaults back to `emit`, so old drivers are unaffected.
    fn emit_recorded(&self, event: &RunEvent, _sequence: u64) {
        self.emit(event)
    }
    async fn create_actor_session(
        &self,
        actor: &ActorRef,
        persona: &PersonaSpec,
        seed: Option<ActorSessionSeed>,
    ) -> Result<SessionRef, WorkflowError>;
    fn start_ask(&self, session: &SessionRef, instance: &InstanceRef, message: AskMessage);
    fn respond_to_submit(&self, instance: &InstanceRef, verdict: SubmitVerdict);
    fn cancel_ask(&self, instance: &InstanceRef);
    async fn execute_world_read(&self, op: &str, args: Vec<Value>) -> Result<Value, WorkflowError>;
    /// Optional: when absent, the engine rejects that node with `ArtifactStoreUnavailable` (a named failure, not a silent downgrade).
    async fn execute_artifact_publish(
        &self,
        _request: ArtifactPublishRequest,
    ) -> Result<ArtifactVersionRecord, WorkflowError> {
        Err(WorkflowError::new(
            WorkflowErrorCode::ArtifactStoreUnavailable,
            "The driver has no artifact store configured",
        ))
    }
    /// Resource release after the run settles (the engine calls it exactly once on each of the three terminal paths).
    fn dispose(&self) {}
}

/// The uplink surface: the driver reports session progress into the core (implemented by the engine; tests can play the "model" directly).
pub trait WorkflowReportSink: Send + Sync {
    fn ask_submit_attempted(&self, instance: &InstanceRef, payload: Value);
    fn ask_turn_ended(&self, instance: &InstanceRef, final_text: &str);
    fn ask_progress(&self, instance: &InstanceRef, progress: AskProgress);
    fn ask_stats(&self, instance: &InstanceRef, stats: AskStats);
    fn ask_failed(&self, instance: &InstanceRef, error: WorkflowError);
    fn stop_run(&self, error: WorkflowError);
    fn run_stalled(&self, info: RunStallInfo);
    fn ask_waiting(&self, instance: &InstanceRef, info: AskWaitInfo);
    fn ask_executing(&self, instance: &InstanceRef);
    fn ask_mutating(&self, instance: &InstanceRef);
    fn concurrency_changed(&self, change: ConcurrencyChange);
}

/// Boundary A: the surface that the lowered script's facade shim calls across the sandbox boundary (implemented by the engine).
#[async_trait::async_trait]
pub trait WorkflowHostApi: Send + Sync {
    fn create_actor(
        &self,
        site_id: &str,
        name: Option<String>,
        persona: Option<serde_json::Value>,
    ) -> Result<ActorId, WorkflowError>;
    async fn ask(
        &self,
        site_id: &str,
        actor: &ActorId,
        instructions: &str,
    ) -> Result<Value, WorkflowError>;
    async fn world_read(
        &self,
        site_id: &str,
        op: &str,
        args: Vec<Value>,
    ) -> Result<Value, WorkflowError>;
    fn report(&self, site_id: &str, item: Value, artifact_id: Option<&str>);
    fn enter_phase(&self, name: &str);
    async fn publish_artifact(
        &self,
        site_id: &str,
        op: &str,
        args: Vec<Value>,
    ) -> Result<ArtifactRef, WorkflowError>;
    fn declare_artifact(&self, site_id: &str, op: &str, args: Vec<Value>);
    fn log(&self, message: &str);
}

// ————————————————————————————————————————————————————————————————
// Boundary C: run events (the journal wire format; field names are character-for-character identical to TS)
// ————————————————————————————————————————————————————————————————

/// Node kind. report / artifact have rows but no nodes (a deliverable is not a step someone else can await).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeKind {
    Ask,
    WorldRead,
    WorldRun,
    Report,
    Artifact,
}

/// The semantic outcome of a node settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeOutcome {
    Ok,
    Failed,
    Cancelled,
}

/// The run's logical status (the mapping to the five physical column values lives only in the SQLite codec).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Pending,
    Running,
    Completed,
    Errored,
    Stopped,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Errored => "errored",
            Self::Stopped => "stopped",
        }
    }
}

/// Why the run stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStopReason {
    User,
    Model,
    Provider,
    Interrupted,
    Superseded,
}

impl RunStopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Interrupted => "interrupted",
            Self::Superseded => "superseded",
        }
    }
}

/// The run event vocabulary (Boundary C). Serializing it is the journal wire format.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum RunEvent {
    RunStarted {
        run_id: String,
        caps: Caps,
    },
    RunLaunched {
        input_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase_names: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase_alongside: Option<Vec<Vec<u32>>>,
    },
    ActorCreated {
        actor: ActorRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        persona: Option<PersonaSpec>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase_name: Option<String>,
    },
    NodeQueued {
        instance: InstanceRef,
        kind: NodeKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<ActorRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor_seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions_head: Option<String>,
    },
    NodeDispatched {
        instance: InstanceRef,
    },
    NodeRepairing {
        instance: InstanceRef,
        attempt: u32,
        violations: Vec<Violation>,
    },
    NodeNudged {
        instance: InstanceRef,
    },
    NodeWaiting {
        instance: InstanceRef,
        cause: AskWaitCause,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delay_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<f64>,
    },
    NodeExecuting {
        instance: InstanceRef,
    },
    ConcurrencyChanged {
        key: String,
        previous: u32,
        next: u32,
        reason: ConcurrencyChangeReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_good: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_bad: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cooldown_ms: Option<f64>,
    },
    NodeSettled {
        instance: InstanceRef,
        outcome: NodeOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cached: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<WorkflowErrorJson>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase_name: Option<String>,
    },
    NodeProgress {
        instance: InstanceRef,
        turn: u64,
        tool_calls: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_tool: Option<AskLastTool>,
    },
    UsageUpdated {
        spent_tokens: u64,
    },
    Log {
        message: String,
    },
    ImportCacheClosed {
        instance: InstanceRef,
        cause: ImportCloseCause,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor_name: Option<String>,
    },
    PhaseEntered {
        name: String,
        ordinal: u64,
    },
    Report {
        instance: InstanceRef,
        item: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact_id: Option<String>,
    },
    ArtifactPublished {
        instance: InstanceRef,
        artifact: ArtifactVersionRecord,
    },
    ArtifactFailed {
        instance: InstanceRef,
        id: String,
        op: ArtifactOp,
        error: WorkflowErrorJson,
    },
    Compaction {
        actor: ActorRef,
    },
    EscalationRaised {
        qid: String,
        actor: ActorRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor_name: Option<String>,
        question: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        asked_at: f64,
    },
    EscalationResolved {
        qid: String,
        answer: String,
    },
    RunStalled {
        since_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cap: Option<u32>,
    },
    RunSettled {
        status: RunStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<RunStopReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        superseded_by: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<WorkflowErrorJson>,
    },
}

impl RunEvent {
    /// The type value in the journal wire format (used to assert event sequences).
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run-started",
            Self::RunLaunched { .. } => "run-launched",
            Self::ActorCreated { .. } => "actor-created",
            Self::NodeQueued { .. } => "node-queued",
            Self::NodeDispatched { .. } => "node-dispatched",
            Self::NodeRepairing { .. } => "node-repairing",
            Self::NodeNudged { .. } => "node-nudged",
            Self::NodeWaiting { .. } => "node-waiting",
            Self::NodeExecuting { .. } => "node-executing",
            Self::ConcurrencyChanged { .. } => "concurrency-changed",
            Self::NodeSettled { .. } => "node-settled",
            Self::NodeProgress { .. } => "node-progress",
            Self::UsageUpdated { .. } => "usage-updated",
            Self::Log { .. } => "log",
            Self::ImportCacheClosed { .. } => "import-cache-closed",
            Self::PhaseEntered { .. } => "phase-entered",
            Self::Report { .. } => "report",
            Self::ArtifactPublished { .. } => "artifact-published",
            Self::ArtifactFailed { .. } => "artifact-failed",
            Self::Compaction { .. } => "compaction",
            Self::EscalationRaised { .. } => "escalation-raised",
            Self::EscalationResolved { .. } => "escalation-resolved",
            Self::RunStalled { .. } => "run-stalled",
            Self::RunSettled { .. } => "run-settled",
        }
    }
}

// ————————————————————————————————————————————————————————————————
// Journal storage port + records
// ————————————————————————————————————————————————————————————————

/// The dwf_run record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    pub caps: Caps,
    pub spent_tokens: u64,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<RunStopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<WorkflowErrorJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// The dwf_actor record, unique(runId, siteId, ordinal).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActorRecord {
    pub run_id: String,
    pub site_id: String,
    pub ordinal: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<PersonaSpec>,
    /// Driver-owned fields (the engine's putActor only carries existing values through unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The model pin written by the driver (`providerId/modelId`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
}

/// Node persistence status. A report node is written once and lands as completed (the only exception).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRecordStatus {
    Running,
    Completed,
    Failed,
}

/// The bounded input of a world-read / world-run node (above 4KB each item is truncated into a string preview and truncated is set).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldReadInput {
    pub op: String,
    pub args: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

/// The dwf_node record. unique(runId, siteId, ordinal); ask additionally has unique(runId, actorSiteId, actorOrdinal, actorSeq).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeRecord {
    pub run_id: String,
    pub site_id: String,
    pub ordinal: u32,
    pub kind: NodeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_site_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_ordinal: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_seq: Option<u64>,
    pub input_hash: String,
    pub status: NodeRecordStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WorkflowErrorJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<AskStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<WorldReadInput>,
    /// ask only: the length of the actor session's message log once the ask's full exchange has ended (written by the driver).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_boundary: Option<u64>,
}

/// One persisted event; sequence is assigned monotonically by appendEvent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredEvent {
    pub sequence: u64,
    pub event: RunEvent,
    /// The moment the storage layer appended this event (epoch milliseconds) -- the only clock behind any "how long ago" in the event log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_created: Option<f64>,
}

/// Event pagination parameters (cursor = journal sequence; returns only events strictly greater than it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListEventsOptions {
    pub after_sequence: Option<u64>,
    pub limit: Option<usize>,
}

/// The persisted content accompanying a run settlement (one write: the terminal status and the artifact are not split into two UPDATEs).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSettlementRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<RunStopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<WorkflowErrorJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// The run's final settlement (the resolved value of the engine's `settled`).
#[derive(Clone, Debug, PartialEq)]
pub struct RunSettlement {
    pub status: RunStatus,
    pub artifact: Option<Value>,
    pub reason: Option<RunStopReason>,
    pub superseded_by: Option<String>,
    pub error: Option<WorkflowError>,
}

/// The journal storage port: repository-style, synchronous methods, no SQL leakage.
pub trait JournalStorePort: Send + Sync {
    fn create_run(&self, record: RunRecord);
    fn get_run(&self, run_id: &str) -> Option<RunRecord>;
    fn update_run_status(
        &self,
        run_id: &str,
        status: RunStatus,
        settlement: Option<&RunSettlementRecord>,
    );
    /// Persist cumulative usage on its own (a high-frequency small write). An unknown runId must fail; it must never touch status / failure.
    fn update_run_usage(&self, run_id: &str, spent_tokens: u64);
    fn put_actor(&self, record: ActorRecord);
    fn get_actor(&self, run_id: &str, site_id: &str, ordinal: u32) -> Option<ActorRecord>;
    fn list_actors(&self, run_id: &str) -> Vec<ActorRecord>;
    fn put_node(&self, record: NodeRecord);
    fn get_node(&self, run_id: &str, site_id: &str, ordinal: u32) -> Option<NodeRecord>;
    fn list_nodes(&self, run_id: &str) -> Vec<NodeRecord>;
    fn append_event(&self, run_id: &str, event: &RunEvent) -> StoredEvent;
    /// Ascending by sequence; with cursor/limit the filtering must happen in the storage layer (an unknown run / out-of-range cursor returns empty).
    fn list_events(&self, run_id: &str, opts: ListEventsOptions) -> Vec<StoredEvent>;
}

// ————————————————————————————————————————————————————————————————
// import cache (amend-resume, pure data)
// ————————————————————————————————————————————————————————————————

/// One importable, already settled ask (the array index is the actorSeq).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedAskEntry {
    pub input_hash: String,
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<AskStats>,
    /// The number of messages in the source session after that ask settled (the boundary for seed truncation, required).
    pub message_boundary: u64,
}

/// The importable prefix of a named actor from a predecessor run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedActorCandidate {
    pub persona: PersonaSpec,
    /// The longest all-completed ask prefix, indexed by actorSeq.
    pub entries: Vec<ImportedAskEntry>,
    pub transcript_source_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
}

/// One importable world node (world-read / world-run), matched by content + occurrence order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedWorldEntry {
    pub input_hash: String,
    pub kind: NodeKind,
    pub result: Value,
}

/// The import cache injected into the engine (pure data; every hit writes a real dwf_node, and losing it can be rebuilt from resumed_from).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportedRunCache {
    /// key = the effective actor name.
    pub actors: BTreeMap<String, ImportedActorCandidate>,
    /// key = inputHash({op,args}); value = a queue ordered by the predecessor's listNodes insertion order.
    pub world: BTreeMap<String, Vec<ImportedWorldEntry>>,
}

// ————————————————————————————————————————————————————————————————
// policy constants (part of the contract)
// ————————————————————————————————————————————————————————————————

/// Caps on artifact count, size and text length.
#[derive(Clone, Copy, Debug)]
pub struct ArtifactCaps {
    pub max_artifacts_per_run: usize,
    pub max_versions_per_artifact: u32,
    pub max_file_bytes: u64,
    pub max_markdown_bytes: u64,
    pub max_title_length: usize,
    pub max_description_length: usize,
    pub max_spec_serialized_bytes: usize,
    pub max_id_length: usize,
}

pub const ARTIFACT_CAPS: ArtifactCaps = ArtifactCaps {
    max_artifacts_per_run: 32,
    max_versions_per_artifact: 16,
    max_file_bytes: 20 * 1024 * 1024,
    max_markdown_bytes: 256 * 1024,
    max_title_length: 120,
    max_description_length: 500,
    max_spec_serialized_bytes: 8 * 1024,
    max_id_length: 64,
};

/// The caps for world-read / world-run (execution lives in the driver, the constants in the contract).
#[derive(Clone, Copy, Debug)]
pub struct WorldReadCaps {
    pub glob_max_files: usize,
    pub grep_max_matches: usize,
    pub grep_max_serialized_bytes: usize,
    pub git_diff_max_bytes: usize,
    pub git_log_max_count: u32,
    pub git_log_default_count: u32,
    pub run_stdout_max_bytes: usize,
    pub run_stderr_max_bytes: usize,
    pub run_default_timeout_ms: u64,
}

pub const WORLD_READ_CAPS: WorldReadCaps = WorldReadCaps {
    glob_max_files: 2000,
    grep_max_matches: 2000,
    grep_max_serialized_bytes: 256 * 1024,
    git_diff_max_bytes: 512 * 1024,
    git_log_max_count: 100,
    git_log_default_count: 20,
    run_stdout_max_bytes: 256 * 1024,
    run_stderr_max_bytes: 256 * 1024,
    run_default_timeout_ms: 300_000,
};

/// The caps on report count per run and serialized bytes per item (execution lives in the engine core).
#[derive(Clone, Copy, Debug)]
pub struct ReportCaps {
    pub max_items_per_run: usize,
    pub max_item_serialized_bytes: usize,
}

pub const REPORT_CAPS: ReportCaps = ReportCaps {
    max_items_per_run: 256,
    max_item_serialized_bytes: 32 * 1024,
};

// ————————————————————————————————————————————————————————————————
// Deterministic hashing (replay's defensive consistency check, port of hash.ts)
// ————————————————————————————————————————————————————————————————

/// Canonical JSON: object keys sorted by code point (giving semantically equal values a stable byte order).
///
/// Number formatting differs from JS `JSON.stringify`: Rust's `f64::to_string` and JS both use the shortest round-trip
/// representation, and integral floats print as `1` on either side; when `|x| >= 1e21` or `0 < |x| < 1e-6` JS uses exponential
/// notation while Rust uses decimal expansion -- a known divergence, and the arguments of host calls never land in that range (the pattern / path / cmd of world reads).
pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| {
                    let v = &map[k.as_str()];
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canonical_json(v)
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// FNV-1a 32-bit hash, output as an 8-digit hexadecimal string.
///
/// Hashes UTF-16 code unit by code unit (the high byte is folded in too), byte-identical to the TS `charCodeAt` implementation,
/// so journal hashes are mutually recognizable across the two languages for any run.
pub fn fnv1a(input: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for unit in input.encode_utf16() {
        hash ^= u32::from(unit & 0xff);
        hash ^= u32::from((unit >> 8) & 0xff);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

/// Compute the defensive hash of one host call's input: FNV-1a over the canonical JSON.
pub fn input_hash(value: &Value) -> String {
    fnv1a(&canonical_json(value))
}

/// Convenient constructor: the input hash of a `{op, args}` shaped input (the journal key for world-read / world-run).
pub fn world_input_hash(op: &str, args: &[Value]) -> String {
    let mut map = Map::new();
    map.insert("op".to_string(), Value::String(op.to_string()));
    map.insert("args".to_string(), Value::Array(args.to_vec()));
    input_hash(&Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn error_codes_serialize_as_ts_strings() {
        let err = WorkflowError::new(WorkflowErrorCode::ScriptHashMismatch, "boom")
            .with_mismatch("a", "b");
        let json = serde_json::to_value(err.to_json()).unwrap();
        assert_eq!(json["code"], json!("ScriptHashMismatch"));
        assert_eq!(json["mismatch"]["expected"], json!("a"));
        let back = WorkflowError::from_json(&serde_json::from_value(json).unwrap());
        assert_eq!(back.code, WorkflowErrorCode::ScriptHashMismatch);
        assert_eq!(back.message, "boom");
    }

    #[test]
    fn run_event_wire_names_match_ts() {
        let event = RunEvent::NodeQueued {
            instance: InstanceRef {
                site_id: "ask#1".into(),
                ordinal: 2,
            },
            kind: NodeKind::Ask,
            actor: Some(ActorRef {
                site_id: "actor#1".into(),
                ordinal: 1,
            }),
            actor_seq: Some(0),
            phase_name: Some("plan".into()),
            instructions_head: Some("do it".into()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], json!("node-queued"));
        assert_eq!(json["instance"]["siteId"], json!("ask#1"));
        assert_eq!(json["instance"]["ordinal"], json!(2));
        assert_eq!(json["actorSeq"], json!(0));
        assert_eq!(json["phaseName"], json!("plan"));
        assert_eq!(json["instructionsHead"], json!("do it"));
        let back: RunEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn world_and_artifact_kinds_use_kebab_and_lowercase() {
        assert_eq!(
            serde_json::to_value(NodeKind::WorldRun).unwrap(),
            json!("world-run")
        );
        assert_eq!(
            serde_json::to_value(RunStatus::Errored).unwrap(),
            json!("errored")
        );
        assert_eq!(
            serde_json::to_value(RunStopReason::Superseded).unwrap(),
            json!("superseded")
        );
        assert_eq!(
            serde_json::to_value(ImportCloseCause::MutatingTool).unwrap(),
            json!("mutating-tool")
        );
        assert_eq!(
            serde_json::to_value(ArtifactOp::Markdown).unwrap(),
            json!("markdown")
        );
    }

    #[test]
    fn canonical_json_sorts_keys_and_matches_ts_examples() {
        let value = json!({"b": 1, "a": [true, null, "x"]});
        assert_eq!(canonical_json(&value), r#"{"a":[true,null,"x"],"b":1}"#);
    }

    #[test]
    fn fnv1a_is_utf16_stable() {
        // Vectors pinned from the TS implementation (hash.ts run under node, 2026-10-30).
        assert_eq!(fnv1a(""), "811c9dc5");
        assert_eq!(fnv1a("hello"), "4f9f2cab");
        assert_eq!(fnv1a("a😀b"), "241490f7");
        let mut manual = 0x811c_9dc5u32;
        for unit in "a😀b".encode_utf16() {
            manual ^= u32::from(unit & 0xff);
            manual ^= u32::from((unit >> 8) & 0xff);
            manual = manual.wrapping_mul(0x0100_0193);
        }
        assert_eq!(fnv1a("a😀b"), format!("{manual:08x}"));
    }

    #[test]
    fn world_input_hash_pins_op_and_args() {
        let a = world_input_hash("read", &[json!("README.md")]);
        let b = world_input_hash("read", &[json!("README.md")]);
        let c = world_input_hash("glob", &[json!("README.md")]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 8);
        // Pinned from the TS implementation (canonicalJson + fnv1a under node).
        assert_eq!(a, "20cb8498");
    }

    #[test]
    fn settlement_and_usage_records_round_trip() {
        let record = RunRecord {
            run_id: "r1".into(),
            parent_session_id: Some("s1".into()),
            cwd: Some("/tmp".into()),
            name: Some("demo".into()),
            tool_call_id: None,
            script_text: Some("return 1;".into()),
            script_hash: Some("abcd1234".into()),
            args: Some(json!({"x": 1})),
            resumed_from: None,
            caps: Caps { max_concurrency: 4 },
            spent_tokens: 0,
            status: RunStatus::Running,
            stop_reason: None,
            superseded_by: None,
            failure: None,
            result: None,
        };
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["runId"], json!("r1"));
        assert_eq!(json["caps"]["maxConcurrency"], json!(4));
        let back: RunRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back, record);
    }
}
