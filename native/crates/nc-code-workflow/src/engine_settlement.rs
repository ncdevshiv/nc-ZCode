//! The run's three terminal paths (port of `engine-settlement.ts`).
//!
//! first-wins is guarded by the `is_run_settled()` check on the first line of each path.

use serde_json::Value;

use crate::engine_state::Core;
use crate::types::{
    RunEvent, RunSettlement, RunSettlementRecord, RunStatus, RunStopReason, WorkflowError,
    WorkflowErrorCode,
};

/// The sandbox script returned a top-level artifact successfully: abort in-flight asks (cancelled), settle as completed.
pub(crate) fn settle_completed(core: &mut Core, artifact: Value) {
    if core.is_run_settled() {
        return;
    }
    core.mark_settled(None);
    core.abort_in_flight(
        WorkflowError::new(
            WorkflowErrorCode::Cancelled,
            "Run completed; in-flight subagent tasks were abandoned.",
        ),
        true,
    );
    // The artifact is persisted in the same write as the terminal status; splitting it into two writes would open a crash window of "completed but the artifact is lost".
    let settlement = RunSettlementRecord {
        result: Some(artifact.clone()),
        ..Default::default()
    };
    finish_run(core, RunStatus::Completed, Some(&settlement));
    core.resolve_settled(RunSettlement {
        status: RunStatus::Completed,
        artifact: Some(artifact),
        reason: None,
        superseded_by: None,
        error: None,
    });
}

/// External stop: abort in-flight asks, settle as stopped(reason). first-wins: if already settled, ignore.
pub(crate) fn settle_stopped(
    core: &mut Core,
    reason: RunStopReason,
    error: Option<WorkflowError>,
    superseded_by: Option<String>,
) {
    if core.is_run_settled() {
        return;
    }
    core.mark_settled(None);
    core.abort_in_flight(
        WorkflowError::new(WorkflowErrorCode::Cancelled, "Run stopped."),
        true,
    );
    let settlement = RunSettlementRecord {
        stop_reason: Some(reason),
        superseded_by: superseded_by.clone(),
        failure: error.as_ref().map(|e| e.to_json()),
        result: None,
    };
    finish_run(core, RunStatus::Stopped, Some(&settlement));
    core.resolve_settled(RunSettlement {
        status: RunStatus::Stopped,
        artifact: None,
        reason: Some(reason),
        superseded_by,
        error,
    });
}

/// Run-level failure (a mistake in the script): abort in-flight asks (rejecting them with the run error), settle as errored.
pub(crate) fn settle_failed(core: &mut Core, error: WorkflowError) {
    if core.is_run_settled() {
        return;
    }
    core.mark_settled(Some(error.clone()));
    core.abort_in_flight(error.clone(), false);
    let settlement = RunSettlementRecord {
        failure: Some(error.to_json()),
        ..Default::default()
    };
    finish_run(core, RunStatus::Errored, Some(&settlement));
    core.resolve_settled(RunSettlement {
        status: RunStatus::Errored,
        artifact: None,
        reason: None,
        superseded_by: None,
        error: Some(error),
    });
}

/// Shared by the three terminal paths: persist + `run-settled` event + driver dispose (exactly once).
fn finish_run(core: &mut Core, status: RunStatus, settlement: Option<&RunSettlementRecord>) {
    core.journal()
        .update_run_status(&core.run_id, status, settlement);
    core.record(RunEvent::RunSettled {
        status,
        stop_reason: settlement.and_then(|s| s.stop_reason),
        superseded_by: settlement.and_then(|s| s.superseded_by.clone()),
        error: settlement.and_then(|s| s.failure.clone()),
    });
    core.driver.dispose();
}
