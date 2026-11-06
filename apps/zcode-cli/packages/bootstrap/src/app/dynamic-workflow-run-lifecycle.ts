// ============================================================
// Dynamic Workflow Run: The life cycle bookkeeping of the run owned by this session (settlement/closure/closing the gate/external final state line leaving traces)
// ============================================================
// Reason for splitting: dynamic-workflow-run-service.ts reaches the upper limit of oxlint max-lines (400 lines), and the run is moved from settlement to
// The closed life cycle is split into this file - settlement bookkeeping (final state entry, failure to normalize, permanent registration, settlement notification, final state
// Closures for entry elimination) and service-side shutdown and foreign final state line checks.
// They only borrow the registry and journal of service and pass it in through narrow dependencies; invariants 6 and 7 of the service file header are still their specifications.

import type { Logger } from "@zcode/contracts";
import type { JournalStorePort, RunSettlement } from "@zcode/dynamic-workflow";
import {
  TERMINAL_RUN_STATUSES,
  type RunRegistryEntry,
} from "./dynamic-workflow-run-observation.js";

/** The number of entries retained by the final run in the memory registry (the product is only in the settlement, and the journal does not store the script return value). */
const TERMINAL_REGISTRY_LIMIT = 32;

/** A notification after settlement of a run; `liveRunCount` is the number of runs still flying under the name of this service at the moment of notification. */
export interface DynamicWorkflowRunSettledNotice {
  runId: string;
  liveRunCount: number;
}

interface RunServiceLifecycleDeps {
/** Read-only `getRun`: The judgment of the foreign final state line depends on the state on the line. */
  journal: Pick<JournalStorePort, "getRun">;
  logger?: Logger;
  parentSessionId: string;
/** See `DynamicWorkflowRunServiceDeps.registerResidencyBlockingWork` (invariant 6) of service. */
  registerResidencyBlockingWork?: (work: Promise<unknown>) => void;
/** The registry body of the service (not a snapshot): old final entries are eliminated during settlement, in-flight entries are enumerated when closed, and flags are set when leaving traces. */
  runs: Map<string, RunRegistryEntry>;
}

interface RunServiceLifecycle {
/** The number of runs that have not been settled at this moment (`terminal === undefined` entries in the registry). */
  countLiveRuns(): number;
/** Subscribe to settlement notification (sent after bookkeeping; if the listener throws an error, it will only be logged). Return the unsubscribe function. */
  subscribeRunSettled(listener: (notice: DynamicWorkflowRunSettledNotice) => void): () => void;
/** Settlement bookkeeping (submit / amend / resume sharing): final entry, failure normalization, permanent registration. Never reject. */
  trackSettlement(
    runId: string,
    entry: RunRegistryEntry,
    launched: Promise<RunSettlement>,
  ): Promise<RunSettlement>;
/** See the close description of {@link createRunServiceLifecycle}. Idempotent. */
  close(): Promise<void>;
/** The launch after closing is a wiring error: throwing instead of adding a member to the rejected enumeration of contracts. */
  assertOpen(): void;
/** The final status line appears under the live entry ⇒ External writing, each entry records a warn. */
  noteForeignTerminalRow(runId: string): void;
}

/**
* Close (service file header invariant 7).
 *
* The engine is the closure of this App (actor runtime, event chain, cancellation controller, settlement
* promises are all here), once the App is closed, it loses the host, but the journal line is still running - the next activation can only
* Rely on orphan convergence to guess. Therefore, shutdown requires **active stop**: abort each flying entry with `"interrupted"`, and normalize harness
* becomes `engine.stop("interrupted", Interrupted)`, and the line is written by the engine itself as a resumeable stop.
 *
* Wait without timeout: the harness will be settled synchronously after receiving the stop, and "waiting for the predecessor's own settlement promise" is exactly what amend has already done
* The discipline in use (never use a timer to replace the state). Settling promises never reject (see {@link RunServiceLifecycle.trackSettlement}).
 */
export function createRunServiceLifecycle(deps: RunServiceLifecycleDeps): RunServiceLifecycle {
  const { runs } = deps;
  const terminalOrder: string[] = [];
  const settledListeners = new Set<(notice: DynamicWorkflowRunSettledNotice) => void>();
  const countLiveRuns = (): number => {
    let live = 0;
    for (const entry of runs.values()) if (entry.terminal === undefined) live += 1;
    return live;
  };
// Settlement notifications are issued after bookkeeping: the count read by the listener (the host's registry security boundary) must have deducted this run.
  const notifySettled = (runId: string): void => {
    if (settledListeners.size === 0) return;
    const notice: DynamicWorkflowRunSettledNotice = { runId, liveRunCount: countLiveRuns() };
    for (const listener of settledListeners) {
      try {
        listener(notice);
      } catch (error: unknown) {
        deps.logger?.warn?.("Dynamic workflow run settled listener threw", {
          errorMessage: error instanceof Error ? error.message : String(error),
          event: "dynamic_workflow.run.settled_listener_failed",
          module: "bootstrap.app",
          runId,
        });
      }
    }
  };
  const subscribeRunSettled = (
    listener: (notice: DynamicWorkflowRunSettledNotice) => void,
  ): (() => void) => {
    settledListeners.add(listener);
    return () => {
      settledListeners.delete(listener);
    };
  };

  const rememberTerminal = (runId: string): void => {
    terminalOrder.push(runId);
    while (terminalOrder.length > TERMINAL_REGISTRY_LIMIT) {
      const evicted = terminalOrder.shift();
      if (evicted !== undefined && runs.get(evicted)?.terminal !== undefined) runs.delete(evicted);
    }
  };

  /**
* Settlement bookkeeping (shared by submit and resume): entry in final state, normalization on failure.
* Failures outside the harness (engine configuration errors, etc.) are normalized to the errored final state: never encoded as stopped - in the journal
* The semantics of the two are different from resume UX (stopped can be resumed, errored can only be revised).
   */
  const trackSettlement = (
    runId: string,
    entry: RunRegistryEntry,
    launched: Promise<RunSettlement>,
  ): Promise<RunSettlement> => {
    const settlement = launched.then(
      (result) => {
        entry.terminal = result;
        entry.completedAt = new Date();
        rememberTerminal(runId);
        notifySettled(runId);
        return result;
      },
      (error: unknown) => {
        const message = error instanceof Error ? error.message : String(error);
        deps.logger?.warn?.("Dynamic workflow run failed before settling", {
          errorMessage: message,
          event: "dynamic_workflow.run.start_failed",
          module: "bootstrap.app",
          runId,
        });
        entry.completedAt = new Date();
        const result: RunSettlement = {
          status: "errored",
          error: error instanceof Error ? (error as never) : (new Error(message) as never),
        };
        entry.terminal = result;
        rememberTerminal(runId);
        notifySettled(runId);
        return result;
      },
    );
// Service file header invariant 6: Three entries (submit / amend / resume) share this registration point and must be in launch
// Registration of the same sync slice - one microtask later, a resident rebalancing can fall between startup and registration. Registration is **after settlement**
// promise (`settlement` never rejects, bookkeeping is done), the count is released with its finally.
    deps.registerResidencyBlockingWork?.(settlement);
    return settlement;
  };

  let closed = false;
  let closePromise: Promise<void> | undefined;
  const close = (): Promise<void> => {
// Idempotent + **synchronization** set: `closed` must be true in this synchronization slice, otherwise a launch arrives during closing
// will slip in and build an engine that no one has.
    closePromise ??= (async () => {
      closed = true;
      const live = [...deps.runs.values()].filter((entry) => entry.terminal === undefined);
      deps.logger?.info?.("Dynamic workflow run service closing", {
        event: "dynamic_workflow.service.closing",
        liveRunCount: live.length,
        module: "bootstrap.app",
        parentSessionId: deps.parentSessionId,
      });
      for (const entry of live) entry.controller.abort("interrupted");
      await Promise.all(live.map((entry) => entry.settlement));
      deps.logger?.info?.("Dynamic workflow run service closed", {
        event: "dynamic_workflow.service.closed",
        module: "bootstrap.app",
        parentSessionId: deps.parentSessionId,
        stoppedRunCount: live.length,
      });
    })();
    return closePromise;
  };

  /**
* The launch after closing is a **wiring error**, not a business rejection: the closing gate of the resident pool has blocked the command surface, and it can be reached
* This shows that someone bypassed it. So throw, instead of adding a member to contracts' rejection enum - that would make every caller
* Both have to deal with a branch that never appears in normal operation.
   */
  const assertOpen = (): void => {
    if (closed) {
      throw new Error(
        "dynamic workflow run service is closed; a launch after App close is a wiring fault",
      );
    }
  };

  /**
* The final status line appears under the live entry ⇒ External writing, each entry records a warn.
   *
* A second desktop instance cold-restored the same session, and its orphan convergence
* The run that the process is still running is written as the final line. Reading the live entry of {@link RunRegistryEntry} ignores it
* (synthesizeRunStatus of observation), but this thing itself must leave traces - otherwise "two instances step on each other" can only
* Relying on second guessing.
   *
* Once per entry: The tracker polls `getTask` once per second. Without a flag, the same warn is issued every second. When the flag is not set
* Read `getRun` (index single-row query) one more time, even this time is omitted after setting.
   */
  const noteForeignTerminalRow = (runId: string): void => {
    const entry = deps.runs.get(runId);
    if (entry === undefined || entry.terminal !== undefined || entry.foreignTerminalLogged) return;
    const record = deps.journal.getRun(runId);
    if (record === undefined || !TERMINAL_RUN_STATUSES.has(record.status)) return;
    entry.foreignTerminalLogged = true;
    deps.logger?.warn?.("Dynamic workflow run has a foreign terminal row under a live engine", {
      event: "dynamic_workflow.run.foreign_terminal_row",
      journalStatus: record.status,
      module: "bootstrap.app",
      runId,
      ...(record.stopReason === undefined ? {} : { stopReason: record.stopReason }),
    });
  };

  return {
    countLiveRuns,
    subscribeRunSettled,
    trackSettlement,
    close,
    assertOpen,
    noteForeignTerminalRow,
  };
}
