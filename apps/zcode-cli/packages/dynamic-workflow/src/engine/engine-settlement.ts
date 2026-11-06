/**
* engine.ts reaches the upper limit of oxlint max-lines (400 lines), and adds the three final paths of run
* (completed/stopped/errored) finishRun shared with them
* Split into this file; the public side is still exported from engine.ts.
 *
* Free function to read and write engine status via {@link EngineState} seam; WorkflowEngine's complete / stop /
* failRun is just a thin delegate. first-wins are held on the first line of each path by `isRunSettled()`.
 */

import type { EngineState } from "./engine-state.js";
import type { RunSettlementRecord, RunStatus, RunStopReason } from "./types.js";
import { WorkflowError } from "./types.js";

/**
* The sandbox script successfully returns the top-level artifact, and the settlement is completed.
 *
* The script may still have questions in flight when it returns (loser of `Promise.race`, ask without `await`). They are related to cancel
* The same path is terminated: driver side cancelAsk, deferred to Cancelled reject, reissue node-settled (cancelled).
* The consequences of not aborting are threefold: the scheduler's liveNodes will always hold the node, the driver side will continue to burn tokens,
* A node-dispatched in the event log can never wait for its node-settled.
 */
export function settleCompleted(state: EngineState, artifact: unknown): void {
  if (state.isRunSettled()) return;
  state.markSettled();
  state.abortInFlight(
    new WorkflowError("Cancelled", "Run completed; in-flight subagent tasks were abandoned."),
    true,
  );
//The product is dropped into the library along with the final state. Writing in two strokes will create a crash window of "completed but product lost", and journal
// Rows are the only persisters of products (`run-settled` events are deliberately not widened).
  finishRun(state, "completed", { result: artifact });
  state.resolveSettled({ status: "completed", artifact });
}

/**
* External stop: abort on the fly ask (deferred to Cancelled reject, reissue node-settled(cancelled)), run
* Settlement `stopped(reason)`; completed journal entries are retained (can be resumed). Four reasons to take the same path:
* `user` / `model` (initiator passed in from the cancel entry), `interrupted` (sandbox failure of harness),
* `provider` (deterministic model-side error reported by driver via `stopRun`). `error` is only present for the latter two.
 */
export function settleStopped(
  state: EngineState,
  reason: RunStopReason,
  error?: WorkflowError,
  supersededBy?: string,
): void {
  if (state.isRunSettled()) return;
  state.markSettled();
  state.abortInFlight(new WorkflowError("Cancelled", "Run stopped."), true);
// The successor id of `superseded` is dropped into the database with the same reason:
// stopped Rewrite the entire envelope. If you write it in two strokes, there will be a "superseded but I don't know who replaced it" window.
  finishRun(state, "stopped", {
    stopReason: reason,
    ...(supersededBy === undefined ? {} : { supersededBy }),
    ...(error === undefined ? {} : { failure: error.toJSON() }),
  });
  state.resolveSettled({
    status: "stopped",
    reason,
    ...(supersededBy === undefined ? {} : { supersededBy }),
    ...(error === undefined ? {} : { error }),
  });
}

/** Run-level failure (script error): abort on-the-fly ask (reject with run error), run settlement errored. */
export function settleFailed(state: EngineState, error: WorkflowError): void {
  if (state.isRunSettled()) return;
  state.markSettled(error);
  state.abortInFlight(error, false);
  finishRun(state, "errored", { failure: error.toJSON() });
  state.resolveSettled({ status: "errored", error });
}

/**
* The `run-settled` event only carries {status, stopReason?, error?}: the product does not go to the high-frequency progress pipeline, but only falls into the journal.
 *
* The three final state paths all pass here, so the driver’s dispose happens exactly once and first-wins is naturally established. put on
* `run-settled` **After**: dispose is the release of resources after settlement (the shutdown chain of actor runtime), not settlement
* part, the event log does not have one more entry because of it.
 */
function finishRun(state: EngineState, status: RunStatus, settlement?: RunSettlementRecord): void {
  state.journal.updateRunStatus(state.runId, status, settlement);
  state.record({
    type: "run-settled",
    status,
    ...(settlement?.stopReason === undefined ? {} : { stopReason: settlement.stopReason }),
    ...(settlement?.supersededBy === undefined ? {} : { supersededBy: settlement.supersededBy }),
    ...(settlement?.failure === undefined ? {} : { error: settlement.failure }),
  });
  state.driver.dispose?.();
}
