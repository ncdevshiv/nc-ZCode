//! Workflow service layer (run service, saved workflows, tool wiring) — implemented wave 2.
//!
//! - [`saved_workflows`]: saved workflow definitions (frontmatter + the script source), drafts and argument validation.
//! - [`service`]: the run service itself (submit / amend / resume / cancel / close, plus the observing read model).
//!
//! Two private halves sit at the same layer as `service`: `import_cache` (building the amend-resume imported
//! cache), `ask_validate` (the typed-ask result validator injected as the engine's `ValidateFn`) and `reads`
//! (pure synthesis rules for the run's observing surface, plus the read-only introspection connection).

pub mod compile;
pub mod saved_workflows;
pub mod service;
pub mod v4_payload;

mod ask_validate;
mod import_cache;
mod reads;
pub(crate) mod v4;

#[cfg(test)]
mod tests;

pub use compile::{
    compile_nws, detect_language, nws_diagnostics, CompiledWorkflow, WorkflowLanguage,
};
pub use service::{
    AmendRequest, AmendResult, ArtifactBytes, CancelInitiator, EventPage, EventSink, LabelSource,
    NodeCounts, PendingQuestionView, QuestionRefusal, ResolveQuestionResult, ResumeResult,
    RunArtifact, RunArtifactVersion, RunListItem, RunSnapshot, SnapshotStatus, SubmitRequest,
    SubmitResult, WorkflowService, WorkflowServiceError, WorkspaceNode, WORKFLOW_JOURNAL_FILE,
};
