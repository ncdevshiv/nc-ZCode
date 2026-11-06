//! The ten **model-facing tools** of the dynamic workflow (port of `create-workflow` /
//! `amend-workflow` / `save-workflow` / `eval-workflow-snippet` /
//! `list-workflow-runs` / `get-workflow-run` / `resume-workflow-run` /
//! `resolve-workflow-question` / `list-saved-workflows` under `apps/nc-code-cli/packages/core/src/tool/handlers/`).
//!
//! ## The narrow port
//!
//! The tools themselves **do not hold** the run service: they take a
//! [`WorkflowToolPort`] from [`crate::tools::ToolContext::workflow`], and when it is absent they return **one named business failure** following TS's optional-port convention
//! (`workflow_introspection_unavailable` / `workflow_resume_unavailable` / …). What is structurally identical to TS entry by entry is
//! "an absent port never silently degrades into an empty list or a fake success" — that would make the model wait for a notification that never arrives.
//!
//! The port's production implementation is [`service_port::HostWorkflowPort`]: it folds the run service (submit / amend /
//! resume / the observation-surface read models), the resident compile kernel (analyze / compile) and the transient snippet execution surface into
//! one place; `bin/tool_contracts` does not inject a port when building the registry, so the two declaration gates see the same batch of
//! **contracts** but can never trigger a real run.
//!
//! ## Differences from TS (recorded honestly)
//!
//! - **`causalityGraph` is in production**: `crate::workflow_graph` is the TS-oracle port of `boundCausalityGraph`/`foldPhaseEdges`
//!   (69 fixtures); Create/Amend's success output carries it and `display::
//!   create_workflow_display` builds the `create_workflow` card. Among the observation-class displays all seven
//!   displays are in Rust production (create/get/list/resume/eval/saved; `list_models`' catalogue
//!   output now shares one source with the `subagent_model` resolver). Get's `phases` roster is also projected from the session reducer state; only the two optional
//!   providerLabel/contextWindow fields of the static ProviderConfig remain.
//! - **`phaseAlongside` is wired through**: taken together with `phaseNames` from the same bounded causality graph
//!   ([`support::phase_table`]) and enters `run-launched` with submit/amend; resume does not resend it.
//! - A tool's success result is **compact JSON** (the same convention as the host's other native tools), not TS's
//!   `formatModelContent` XML-ish text; the field set aligns with the TS contract item by item.
//! - The model catalogue comes from `ProviderConfig` (statically known), not TS's dynamic registry; so `subagent_model`
//!   is resolved by the same three-tier rules, but the candidate list only contains the configured models.

pub mod display;

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_workflow::compile::CompileDiagnostic;
use nc_code_workflow::types::NodeRecord;
use serde_json::Value;

use crate::workflow_service::{
    AmendRequest, AmendResult, EventPage, ResolveQuestionResult, ResumeResult, RunListItem,
    RunSnapshot, SubmitRequest, SubmitResult, WorkflowServiceError,
};

pub mod authoring;
pub mod runs;
pub mod service_port;
pub mod support;

pub const CREATE_WORKFLOW_TOOL_NAME: &str = "CreateWorkflow";
pub const AMEND_WORKFLOW_TOOL_NAME: &str = "AmendWorkflow";
pub const SAVE_WORKFLOW_TOOL_NAME: &str = "SaveWorkflow";
pub const EVAL_WORKFLOW_SNIPPET_TOOL_NAME: &str = "EvalWorkflowSnippet";
pub const LIST_WORKFLOW_RUNS_TOOL_NAME: &str = "ListWorkflowRuns";
pub const GET_WORKFLOW_RUN_TOOL_NAME: &str = "GetWorkflowRun";
pub const RESUME_WORKFLOW_RUN_TOOL_NAME: &str = "ResumeWorkflowRun";
pub const RESOLVE_WORKFLOW_QUESTION_TOOL_NAME: &str = "ResolveWorkflowQuestion";
pub const LIST_SAVED_WORKFLOWS_TOOL_NAME: &str = "ListSavedWorkflows";

/// The **tool-layer subset** of one static analysis: diagnostics plus the declared phase table. The analyser's three graphs do not enter this structure (see the file header).
#[derive(Clone, Debug, Default)]
pub struct ScriptAnalysis {
    pub ok: bool,
    pub diagnostics: Vec<CompileDiagnostic>,
    /// The named phases inside the bounded causality graph, in declaration order (cut to 32), with the synthetic `unphased` removed. The same filter as TS
    /// `createWorkflowPhaseNames(causalityGraph)`; empty when there is no causality graph.
    pub phase_names: Vec<String>,
    /// The parallelism table aligned with [`ScriptAnalysis::phase_names`] by index (TS
    /// `createWorkflowPhaseAlongside`); None when there is no parallelism.
    pub phase_alongside: Option<Vec<Vec<u32>>>,
    /// The bounded display graph of `boundCausalityGraph` (`workflow_graph`, the TS-oracle port); None when the analyser has no
    /// causality graph. Create/Amend's success output hands it to the UI's `create_workflow` card.
    pub causality_graph: Option<Value>,
}

/// The request for one snippet execution (this host's subset of `DynamicWorkflowSnippetEvalRequest`).
#[derive(Clone, Debug)]
pub struct SnippetRequest {
    pub code: String,
    pub cwd: String,
    pub session_id: String,
    pub timeout_ms: u64,
}

/// The result of one snippet execution (`DynamicWorkflowSnippetEvalResult`).
pub enum SnippetOutcome {
    Diagnostics(Vec<CompileDiagnostic>),
    Completed {
        artifact: Option<Value>,
        logs: Vec<String>,
        logs_truncated: bool,
    },
    Failed {
        code: String,
        message: String,
        logs: Vec<String>,
        logs_truncated: bool,
    },
}

/// The dynamic-workflow capability surface the tool layer needs.
///
/// This is the **only** dependency from the tools to the execution side: no host handle, no concrete service type, so the tools'
/// unit tests can feed a stub that implements only the needed methods, while production only assembles [`service_port::HostWorkflowPort`].
#[async_trait]
pub trait WorkflowToolPort: Send + Sync {
    /// This machine's process-level concurrency ceiling (`max(1, min(16, cores − 2))`).
    fn concurrency_ceiling(&self) -> Option<u32>;

    /// TS `analyzeScript`: diagnostics plus the declared phase table. When the kernel is unavailable it returns readable internal failure text.
    async fn analyze(&self, script: &str) -> Result<ScriptAnalysis, String>;

    async fn submit(&self, request: SubmitRequest) -> Result<SubmitResult, WorkflowServiceError>;

    async fn amend(&self, request: AmendRequest) -> Result<AmendResult, WorkflowServiceError>;

    async fn resume(&self, run_id: &str) -> Result<ResumeResult, WorkflowServiceError>;

    /// The transient snippet: the same compile / lowering / sandbox / world-read execution surface, in-memory journal.
    async fn eval_snippet(&self, request: SnippetRequest) -> Result<SnippetOutcome, String>;

    fn resolve_question(&self, qid: &str, answer: &str) -> ResolveQuestionResult;

    /// Enumerates runs (`None` = every session), most recently updated first. `Err` = this session has no introspection capability (the journal
    /// could not be opened), **not** an empty project — the two facts must stay distinguishable.
    fn list_runs(
        &self,
        session_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<RunListItem>, String>;

    fn get_run(&self, run_id: &str) -> Result<Option<RunSnapshot>, String>;

    /// The enumeration row for a single run (the only source of label / timestamps / the live annotation).
    fn run_row(&self, run_id: &str) -> Result<Option<RunListItem>, String>;

    /// This run's node rows (GetWorkflowRun's usage and roster use them, instead of treating counts as detail).
    fn run_nodes(&self, run_id: &str) -> Result<Vec<NodeRecord>, String>;

    /// This session's `workflowRuns` state (live reducer / cold rebuild). GetWorkflowRun's phase roster reads it;
    /// a stub port defaults to absent (absent means no `phases` is emitted, not an empty array).
    fn workflow_runs_state(&self, _session_id: &str) -> Option<Value> {
        None
    }

    fn list_events(
        &self,
        run_id: &str,
        after_sequence: Option<u64>,
        limit: Option<usize>,
    ) -> Result<EventPage, String>;

    /// Whether this service currently holds its engine (unsettled).
    fn is_live(&self, run_id: &str) -> bool;
}

/// The type alias of `ToolContext.workflow` (one less layer of angle brackets to write).
pub type WorkflowPort = Arc<dyn WorkflowToolPort>;
