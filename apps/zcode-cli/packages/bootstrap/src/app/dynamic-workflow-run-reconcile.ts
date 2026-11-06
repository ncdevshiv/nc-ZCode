// ============================================================
// Dynamic Workflow Run Service: Convergence on construction of orphan run
// ============================================================
// dynamic-workflow-run-service.ts reaches the upper limit of oxlint max-lines (400 lines) and converges orphans
// (interruptedRunFailure / reconcileOrphanRuns) is split into this file; the public side is still from
// dynamic-workflow-run-service.ts export. For the semantics, see invariant 4 in the file header there.

import type { Logger } from "@zcode/contracts";
import type { JournalStorePort, RunRecord, WorkflowErrorJson } from "@zcode/dynamic-workflow";
import { supportsNonTerminalRunQuery } from "./dynamic-workflow-run-journal.js";
import {
  INTERRUPTED_FAILURE_CODE,
  TERMINAL_RUN_STATUSES,
} from "./dynamic-workflow-run-observation.js";

/**
* The subset of deps required for convergence (the structural subset of DynamicWorkflowRunServiceDeps, service is passed in as it is).
* Only list these three items without citing the entire deps type: convergence read-only journal, only recognize this session, extra dependencies will only make "it end"
* Touched something" becomes invisible.
 */
interface DynamicWorkflowOrphanReconcileDeps {
  journal: JournalStorePort;
  parentSessionId: string;
  logger?: Logger;
}

/**
* Failure encoding for interrupted. Inherit the engine's {@link WorkflowErrorJson} form (fall dwf_run.failure_json,
* Same reading surface as the engine's own failure), but the code is **exclusive**:
 *
* - It cannot be `DriverError` - the error thrown by the script itself is also encoded into it (dynamic-workflow-runtime/src/harness.ts),
* If the code is the same, you can only rely on the message text to distinguish between "process killed" and "script really failed";
* - The status cannot be `cancelled` - that is "user canceled" semantics. Both can be restored (resume door recognition
* {@link INTERRUPTED_FAILURE_CODE}), but the semantics must remain distinguishable.
 */
function interruptedRunFailure(runId: string): WorkflowErrorJson {
  return {
    code: INTERRUPTED_FAILURE_CODE,
    message: `dynamic workflow run ${runId} was interrupted: the owning process exited before the run settled`,
  };
}

/**
* Convergence of the orphan run of this parent session when constructing (previous question:
* After run-started, the process is shut down, the dwf_run line** always stops at running**, and getTask/waitForTask does not work
* The run in the registry of this process directly returns the journal snapshot, so after the session is restored, each journal read page is told "still running".
* Never heals itself).
 *
* Why the timing is **Construction**: At this moment, zero instances of this service are running, so any entries in the journal belonging to this session
* Non-final lines can only be relics of dead processes. Quadratic constructions are therefore naturally idempotent (no more non-final state terms).
 *
*Three borders:
* - **Only converge this session** (`deps.parentSessionId`). Global cleaning will mark the running run of the same process sibling session as dead;
* Dual processes in the same session are excluded by the single owner of the session.
* - **Do not synthesize dwf_event**. The contract of the event log is "what the engine has sent", and the status authority is on the run line;
* `run-settled` is the engine's seal, not the sweeper's.
* - **Don't let convergence failures bring down the structure**. Convergence is a self-healing action rather than a prerequisite for run: when query or write fails, warn and
* Continue (only the line of lies remains) and never turn an app construction into a startup failure.
 */
export function reconcileOrphanRuns(deps: DynamicWorkflowOrphanReconcileDeps): void {
  const { journal, logger, parentSessionId } = deps;
  if (!supportsNonTerminalRunQuery(journal)) {
    logger?.warn?.("Dynamic workflow orphan run reconciliation skipped", {
      event: "dynamic_workflow.run.reconcile_skipped",
      module: "bootstrap.app",
      reason: "journal_missing_list_non_terminal_runs",
    });
    return;
  }

  let orphans: RunRecord[];
  try {
    orphans = journal.listNonTerminalRuns(parentSessionId);
  } catch (error) {
    logger?.warn?.("Dynamic workflow orphan run query failed", {
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "dynamic_workflow.run.reconcile_failed",
      module: "bootstrap.app",
    });
    return;
  }

  for (const record of orphans) {
// The authority for final status determination lies in this file (TERMINAL_RUN_STATUSES): the status filtering of the journal is just a pre-screening.
    if (TERMINAL_RUN_STATUSES.has(record.status)) continue;
    try {
// stopped(interrupted): Recoverable, and with the user
// Cancel / The model side stops distinguishing - reason says "the process is dead", and code is the second evidence of the same fact.
      journal.updateRunStatus(record.runId, "stopped", {
        stopReason: "interrupted",
        failure: interruptedRunFailure(record.runId),
      });
      logger?.warn?.("Dynamic workflow run reconciled as interrupted", {
        event: "dynamic_workflow.run.reconciled_interrupted",
        module: "bootstrap.app",
        previousStatus: record.status,
        runId: record.runId,
      });
    } catch (error) {
      logger?.warn?.("Dynamic workflow orphan run reconciliation failed", {
        errorMessage: error instanceof Error ? error.message : String(error),
        event: "dynamic_workflow.run.reconcile_failed",
        module: "bootstrap.app",
        runId: record.runId,
      });
    }
  }
}
