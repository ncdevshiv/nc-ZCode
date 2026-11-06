// ============================================================
// Adaptive concurrency on the driver side: per-actor admission + model state observation + failure attribution
// ============================================================
// From workflow-driver.ts
// Tear it down: That file is just about "session and turn arrangement", and the three things here don't touch turn arrangement - they just wrap the manager port into
// An actor's `ModelRequestAdmission`, read-only actor runtime session event stream, only run-level success/retry beats
// (stall clock). Model failure attribution (formerly modelFailureReasonOf)
// Moved to adapters' policy table (`inspectWorkflowModelFailure`): runner and driver read the same copy.

import {
  SessionEventType,
  type ModelNetworkStatusEvent,
  type ModelRequestAdmission,
  type SessionEvent,
  type SessionId,
} from "@zcode/contracts";
import type { AgentRuntime } from "@zcode/core";
import type {
  AskLastTool,
  AskProgress,
  AskWaitInfo,
  InstanceRef,
  RunStallInfo,
} from "@zcode/dynamic-workflow";
import { createActorToolActivity, type ActorToolCounts } from "./workflow-driver-tool-activity.js";
import {
  workflowConcurrencyKey,
  type WorkflowConcurrencyPort,
} from "./workflow-concurrency-governor.js";

/** The same rule as the manager: these two retry are not provider failures and are not worth a logo. */
const NON_FAILURE_RETRY_REASONS: ReadonlySet<string> = new Set([
  "reasoning_signature_repair",
  "auth_refresh",
]);

interface ActorModelActivityHandlers {
/** The next model request for this ask is in the waiting slot (`cause: "slot"`) or in the backoff (`cause: "backoff"`). */
  onWaiting(info: AskWaitInfo): void;
/** The ask model request is actually sent (the first request, or the one after a period of waiting). */
  onExecuting(): void;
/** Successful completion of any model request (turn or tool side) for this actor - the reset signal for the run-level stall clock. */
  onRequestCompleted?(): void;
/** The runner has scheduled a retry (except signature repair/authentication refresh) - the loading signal for the run-level stall clock. */
  onRetryScheduled?(reason: string): void;
  /**
* The subagent of the on-the-fly ask is about to execute a tool that will rewrite the workspace (at most once per ask; the instance is parsed by this module on a `live` basis,
* If not flying, it will not be reported). The engine accordingly turns off amend-resume's import cache.
   */
  onMutating?(instance: InstanceRef): void;
}

/**
* An actor's **viewing surface**: its runtime's `ModelRequestAdmission` (admission), its model state in the session event stream
* (source of the executing/waiting logo), and tool calls in the same stream (first workspace write → off import cache,
* Plus tool count for this ask, see workflow-driver-tool-activity.ts). The three share the per-ask life cycle, so
* The driver only holds one object and only observe / reset / unsubscribe once; the time after "waiting for a slot"
* `model_request_started` is also recognized as a restore.
 */
export interface ActorModelActivity {
/** Admission port given to runtime deps; undefined (not gated) when manager port is absent. */
  readonly admission: ModelRequestAdmission | undefined;
/** Return to zero when changing ask: The waiting / executing phase and tool count of the previous ask cannot be carried to the next ask. */
  reset(): void;
/** The count of tool calls observed so far for this ask (as fed to AskStats). */
  toolCounts(): ActorToolCounts;
/** The most recent tool call of this ask so far (fed to the lastTool of `node-progress`). */
  lastTool(): AskLastTool | undefined;
  /**
* Note "Another round of turn analysis is completed" and give the ask progress at this moment (`turn` starts from 1).
* The count lives here rather than on the SessionState because it is the same per-ask lifecycle as the tool count:
* The same reset is reset to zero, the same ask is accumulated, and the new round started by nudge is counted as the second turn.
   */
  noteTurnResolved(): AskProgress;
/** Subscribe to the runtime's session events (model state + tool calls); the minimal stub runtime has no subscribeEvents spatio-temporal operation. */
  observe(runtime: AgentRuntime, sessionId: SessionId): void;
  unsubscribe(): void;
}

/**
* The phase of a request chain. Chain = "One logical request and all its retries": type (querySource, queryId, toolCallId) -
* turn step has queryId but no toolCallId, two parallel WebSearchs each have their own toolCallId, compression/title sidecar
* Each has querySource - retry changing requestId but the key remains unchanged, so the chain in backoff is the same as its next attempt.
 */
type ChainPhase = "queued" | "executing" | "backoff";

interface Chain {
  phase: ChainPhase;
/** The latest waiting message (queued/backoff); if there are only waiters left after the chain ends, use the latest one to report waiting. */
  wait?: AskWaitInfo;
  waitSeq: number;
}

/**
* Aggregation rules (replaces single phase): driver maintains a collection of on-fly request chains for each actor -
* - `model_request_queued` (`tryAcquire` miss) → chain queued;
* - `model_request_admitted` / `model_request_started` → chain executing;
* - `model_retry_scheduled` (except `reasoning_signature_repair` / `auth_refresh`) → chain backoff;
* - `model_request_completed` / non-retryable `model_request_failed` → end of chain (out of collection).
* Sub-agent phase: any chain executing → executing; otherwise there is a chain waiting → waiting (with the latest waiting information); the set is empty →
* Keep the previous phase and do not send events (during tool execution in the same ask, the subagent still displays executing when the tool itself is not queued).
* A subagent can hold multiple tickets at the same time (parallel tool calls), so single phase cannot be used anymore.
 *
* This is a driver **internal** observation subscription, not a live broadcast channel of transcript: v4 gateway only drains continuous seq, so
* transcript Live broadcast must go through the construction period of child runtime eventSink (run service test pins this); here is read-only
* Model network status, missing seq 1 does not matter.
 */
export function createActorModelActivity(input: {
  port: WorkflowConcurrencyPort | undefined;
  runId: string;
/** The currently flying ask instance (undefined after settlement/cancellation); it is only required for reporting tool activity. */
  live?: () => InstanceRef | undefined;
  handlers: ActorModelActivityHandlers;
}): ActorModelActivity {
  const chains = new Map<string, Chain>();
  let executing = false;
/** The last waiting message that has been reported; execution is cleared as soon as it is reported. Waits with the same contents in the same period of waiting are only reported once. */
  let reportedWait: AskWaitInfo | undefined;
/** The number of parsed turns in this ask (`turn` of `node-progress`); reset to zero when changing ask. */
  let turnsResolved = 0;
  let waitSeq = 0;
  let unsubscribeEvents: (() => void) | undefined;
  const { handlers, live, port, runId } = input;
// Second reader of the same session event stream: tool calls. The implementation is separated into a file (the judgment and counting are all there), here only put it
// Code into the same life cycle so that there is still only one observation object on the driver side.
  const toolActivity = createActorToolActivity({
    onMutating: () => {
      const instance = live?.();
      if (instance !== undefined) handlers.onMutating?.(instance);
    },
  });

  const anyExecuting = (): boolean => {
    for (const chain of chains.values()) if (chain.phase === "executing") return true;
    return false;
  };
  /**
* Still waiting after aggregation: report waiting (exactly one for each period).
* A subagent sends 4 WebSearchs in parallel, four chains are queued in the same millisecond, and each of the four queued events is reported once.
* `waiting(slot)`——In actual measurement, 26 of the 80 waiting records are repetitions of the same second and the same text. The subagent phase has not changed,
* The event should not be sent again: it will be swallowed if it is the same field by field as the last reported waiting message; change executing and wait before re-reporting.
   */
  const reportWaiting = (info: AskWaitInfo): void => {
    if (anyExecuting()) return;
    executing = false;
    if (reportedWait !== undefined && sameWait(reportedWait, info)) return;
    reportedWait = info;
    handlers.onWaiting(info);
  };
  const reportExecuting = (): void => {
    reportedWait = undefined;
    if (executing) return;
    executing = true;
    handlers.onExecuting();
  };
  const setWaiting = (key: string, phase: "queued" | "backoff", wait: AskWaitInfo): void => {
    waitSeq += 1;
    chains.set(key, { phase, wait, waitSeq });
    reportWaiting(wait);
  };
/** End of a chain: If no chain is executing but there is still a chain waiting, the subagent will actually enter waiting at this moment - report with the latest waiting information. */
  const endChain = (key: string): void => {
    if (!chains.delete(key) || anyExecuting()) return;
    let latest: Chain | undefined;
    for (const chain of chains.values()) {
      if (chain.wait !== undefined && (latest === undefined || chain.waitSeq > latest.waitSeq))
        latest = chain;
    }
    if (latest?.wait !== undefined) reportWaiting(latest.wait);
  };

// The admission port is a narrow wrapper of the manager port: fast path = tryAdmit, queue = admit(waiting(slot)
// Reported by the queued event of the runner, it will not be sent by itself here). acquire still tries the fast path first, which is compatible with callers that don't use tryAcquire.
  const admission: ModelRequestAdmission | undefined =
    port === undefined
      ? undefined
      : {
          tryAcquire: ({ model }) => port.tryAdmit(runId, workflowConcurrencyKey(model)),
          acquire: async ({ model, signal }) => {
            const key = workflowConcurrencyKey(model);
            return (
              port.tryAdmit(runId, key) ??
              (await port.admit(runId, key, signal ?? new AbortController().signal))
            );
          },
        };

  return {
    admission,
    reset: () => {
      chains.clear();
      executing = false;
      reportedWait = undefined;
      turnsResolved = 0;
      toolActivity.reset();
    },
    toolCounts: () => toolActivity.counts(),
    lastTool: () => toolActivity.lastTool(),
    noteTurnResolved: () => {
      turnsResolved++;
      const lastTool = toolActivity.lastTool();
      return {
        turn: turnsResolved,
        toolCalls: toolActivity.counts().toolCalls,
        ...(lastTool === undefined ? {} : { lastTool }),
      };
    },
    observe: (runtime, sessionId) => {
      toolActivity.observe(runtime, sessionId);
      if (typeof (runtime as Partial<AgentRuntime>).subscribeEvents !== "function") return;
      unsubscribeEvents = runtime.subscribeEvents({
        onSessionEvent: (event: SessionEvent) => {
          if (event.type !== SessionEventType.ModelNetworkStatus || event.sessionId !== sessionId)
            return;
          const status = event.payload as ModelNetworkStatusEvent;
          const key = chainKey(status);
          switch (status.type) {
            case "model_request_queued":
              setWaiting(key, "queued", { cause: "slot" });
              return;
            case "model_request_admitted":
            case "model_request_started":
              chains.set(key, { phase: "executing", waitSeq: chains.get(key)?.waitSeq ?? 0 });
              reportExecuting();
              return;
            case "model_retry_scheduled": {
              if (NON_FAILURE_RETRY_REASONS.has(status.reason)) return;
              handlers.onRetryScheduled?.(status.reason);
              setWaiting(key, "backoff", {
                cause: "backoff",
                reason: status.reason,
                attempt: status.nextAttempt,
                delayMs: status.delayMs,
                ...(status.retryAfterMs === undefined ? {} : { retryAfterMs: status.retryAfterMs }),
              });
              return;
            }
            case "model_request_completed":
              handlers.onRequestCompleted?.();
              endChain(key);
              return;
            case "model_request_failed":
// The failed retryable:true is followed by a retry_scheduled - that one changes the phase.
              if (!status.retryable) endChain(key);
              return;
            default:
              return;
          }
        },
      });
    },
    unsubscribe: () => {
      toolActivity.unsubscribe();
      unsubscribeEvents?.();
      unsubscribeEvents = undefined;
    },
  };
}

function sameWait(a: AskWaitInfo, b: AskWaitInfo): boolean {
  return (
    a.cause === b.cause &&
    a.reason === b.reason &&
    a.attempt === b.attempt &&
    a.delayMs === b.delayMs &&
    a.retryAfterMs === b.retryAfterMs
  );
}

function chainKey(status: ModelNetworkStatusEvent): string {
  const parts = [status.querySource, status.queryId, status.toolCallId].map((part) =>
    part === undefined ? "" : String(part),
  );
  return parts.some((part) => part.length > 0) ? parts.join("|") : status.requestId;
}

// ———————————————————————————run level stall clock——————————————————————————————

/** How long in a row without a successful model request will the main agent be notified. */
const WORKFLOW_STALL_NOTIFY_AFTER_MS = 20 * 60_000;

/** Clocks and timers (can be injected, fake time for testing). */
export interface WorkflowClock {
  now?: () => number;
/** Returns the cancellation function. */
  schedule?: (callback: () => void, delayMs: number) => () => void;
}

interface RunStallClockOptions extends WorkflowClock {
/** Default {@link WORKFLOW_STALL_NOTIFY_AFTER_MS}. */
  afterMs?: number;
  onStalled: (info: RunStallInfo) => void;
}

/**
* Stagnation observation of a run: all actors (turn and tool-side requests are integrated) share a table.
* - `noteSuccess`: any `model_request_completed` → reset to zero, cancel alarm clock, clear reason count, end of this section;
* - `noteRetryScheduled`: once `model_retry_scheduled` → count reason; the alarm is set before this paragraph is loaded.
* "Last success + afterMs";
* - Alarm sounds: ≥ afterMs since last success and this segment has at least one scheduled retry → `onStalled` exactly once (each stall
* Section 1), it will not be reloaded until the next success;
* - `noteCap`: Cap change of manager fanout - the current cap is reported in the notification (the value of the latest fanout; if you haven't seen it, you will be absent).
* Pure observation, no decision-making; `dispose` will cancel the alarm after run is settled.
 */
export interface RunStallClock {
  noteSuccess(): void;
  noteRetryScheduled(reason: string): void;
  noteCap(cap: number): void;
  dispose(): void;
}

export function createRunStallClock(options: RunStallClockOptions): RunStallClock {
  const now = options.now ?? Date.now;
  const schedule = options.schedule ?? defaultSchedule;
  const afterMs = options.afterMs ?? WORKFLOW_STALL_NOTIFY_AFTER_MS;
  let lastSuccessAt = now();
  let notified = false;
  let cancelTimer: (() => void) | undefined;
  let cap: number | undefined;
  const reasons = new Map<string, number>();
  let disposed = false;

  const dominantReason = (): string | undefined => {
    let best: string | undefined;
    let bestCount = 0;
    for (const [reason, count] of reasons) {
      if (count > bestCount) {
        best = reason;
        bestCount = count;
      }
    }
    return best;
  };
  const fire = (): void => {
    cancelTimer = undefined;
    if (disposed || notified) return;
    const sinceMs = now() - lastSuccessAt;
    if (sinceMs < afterMs) {
// Clock drift / The injected clock is not monotonic: wait again for the remaining amount, instead of missing this period.
      cancelTimer = schedule(fire, afterMs - sinceMs);
      return;
    }
    if (reasons.size === 0) return;
    notified = true;
    const reason = dominantReason();
    options.onStalled({
      sinceMs,
      ...(reason === undefined ? {} : { reason }),
      ...(cap === undefined ? {} : { cap }),
    });
  };

  return {
    noteSuccess: () => {
      if (disposed) return;
      lastSuccessAt = now();
      notified = false;
      reasons.clear();
      cancelTimer?.();
      cancelTimer = undefined;
    },
    noteRetryScheduled: (reason) => {
      if (disposed || notified) return;
      reasons.set(reason, (reasons.get(reason) ?? 0) + 1);
      if (cancelTimer !== undefined) return;
      cancelTimer = schedule(fire, Math.max(0, afterMs - (now() - lastSuccessAt)));
    },
    noteCap: (next) => {
      cap = next;
    },
    dispose: () => {
      disposed = true;
      cancelTimer?.();
      cancelTimer = undefined;
    },
  };
}

const defaultSchedule = (callback: () => void, delayMs: number): (() => void) => {
  const timer = setTimeout(callback, delayMs);
// Don't let a stalled timer pin the process: it shouldn't have voting rights when the run ends and the process wants to exit.
  if (typeof timer === "object" && timer !== null && "unref" in timer) timer.unref();
  return () => clearTimeout(timer);
};
