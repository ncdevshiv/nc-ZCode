// ============================================================
// AgentRuntime-backed WorkflowDriver: Containment failed on model side
// ============================================================
// workflow-driver.ts reaches the upper limit of oxlint max-lines (400 lines), and corrects the error on the model side when the turn is rejected.
// Containment (strategy statement stop → entire run
// stopped(provider); context_exceeded → node ContextLimit failed; retry / canceled → back off and redrive)
// Split into this file. The free function reads the driver's dependencies and sink, callback runTurn through {@link ModelFailureHost}; private state
// Not exposed. Constants and pure helpers (backoff curve, Retry-After reading, calling) live in workflow-driver-helpers.ts.

import {
  inspectWorkflowModelFailure,
  type WorkflowModelFailureInspection,
} from "@zcode/adapters/model";
import {
  refToString,
  WorkflowError,
  type InstanceRef,
  type ProviderStopDetails,
  type WorkflowReportSink,
} from "@zcode/dynamic-workflow";
import {
  PROVIDER_STOP_RAW_MESSAGE_MAX_CHARS,
  TRANSIENT_CONTINUE_PROMPT,
  defaultSchedule,
  readRetryAfterMs,
  subagentLabel,
  transientBackoffMs,
} from "./workflow-driver-helpers.js";
import type { AgentRuntimeWorkflowDriverDeps, SessionState } from "./workflow-driver-types.js";

/** The driver hands it over to the host side of containment logic: dependencies, sink, whether it has been disposed, and another turn on the same session. */
export interface ModelFailureHost {
  readonly deps: Pick<AgentRuntimeWorkflowDriverDeps, "clock" | "logger" | "runId">;
  readonly sink: WorkflowReportSink;
  isDisposed(): boolean;
  runTurn(state: SessionState, instance: InstanceRef, input: string, epilogueStart: number): void;
}

/**
* Model-side containment when turn is rejected: driver and runner read the same strategy table. Return `false` to indicate this is not a model layer error
* (inspect returns undefined), the caller handles the failure according to the driver side; returning `true` indicates that it has been handled according to the policy.
 */
export function handleModelTurnFailure(
  host: ModelFailureHost,
  state: SessionState,
  instance: InstanceRef,
  error: unknown,
): boolean {
  const inspected = inspectWorkflowModelFailure(error);
  if (inspected === undefined) return false;
  switch (inspected.policy.decision) {
    case "stop":
// Deterministic model-side error: The entire run is stopped (stopped(provider), recoverable), and the node is not settled.
      host.sink.stopRun(providerStopError(state, inspected, error));
      return true;
    case "context_exceeded":
// Core compression failed: the ask itself is too large, it's the script's fault - the node failed with ContextLimit, the script can catch it.
      host.sink.askFailed(
        instance,
        new WorkflowError(
          "ContextLimit",
          `Subagent ${subagentLabel(state)} exceeded the model's context window even after ` +
            `compaction. Give this ask a smaller input or split the work across subagents.`,
          { cause: error },
        ),
      );
      return true;
    default:
// retry / canceled: transient failure put by the runner (stream recovery exhaustion, etc.)——
// The same curve as the runner's retreat. After waiting, start another round and continue running. Only cancel can end it.
      scheduleTransientRedrive(host, state, instance, inspected, error);
      return true;
  }
}

/**
* Driver focus on transient failure: per-ask count, 2s→60s jitter backoff (Retry-After priority), first report
* `askWaiting(backoff)` waits again, respects the cancellation during the waiting period, and then sends a continuation turn on the same persistent runtime
* (Same mechanism as nudge). Uncapped - The run-level stall clock is responsible for letting people know it's waiting.
 */
function scheduleTransientRedrive(
  host: ModelFailureHost,
  state: SessionState,
  instance: InstanceRef,
  inspected: WorkflowModelFailureInspection,
  error: unknown,
): void {
  state.transientAttempts += 1;
  const attempt = state.transientAttempts;
  const retryAfterMs = readRetryAfterMs(error);
  const delayMs = retryAfterMs ?? transientBackoffMs(attempt, host.deps.clock?.random);
  host.sink.askWaiting(instance, {
    cause: "backoff",
    reason: inspected.reason,
    attempt,
    delayMs,
    ...(retryAfterMs === undefined ? {} : { retryAfterMs }),
  });
  host.deps.logger?.warn?.("Dynamic workflow subagent turn failed transiently; redriving", {
    attempt,
    delayMs,
    event: "dynamic_workflow.ask.transient_redrive",
    instance: refToString(instance),
    module: "bootstrap.app",
    reason: inspected.reason,
    runId: host.deps.runId ?? "run",
  });
  const schedule = host.deps.clock?.schedule ?? defaultSchedule;
  state.cancelRedrive?.();
  state.cancelRedrive = schedule(() => {
    state.cancelRedrive = undefined;
// During the waiting period, ask was canceled/settled/substituted: there are no listeners in this round of continuation.
    if (
      host.isDisposed() ||
      state.cancelled ||
      state.accepted ||
      state.abortController?.signal.aborted === true ||
      state.currentInstance === undefined ||
      refToString(state.currentInstance) !== refToString(instance)
    ) {
      return;
    }
    host.runTurn(state, instance, TRANSIENT_CONTINUE_PROMPT, 0);
  }, delayMs);
}

/** `ProviderStop` error: Model-side error in the policy statement stop + structured details required in the notification copy. */
function providerStopError(
  state: SessionState,
  inspected: WorkflowModelFailureInspection,
  cause: unknown,
): WorkflowError {
  const kind = inspected.policy.decision === "stop" ? inspected.policy.kind : "other";
  const rawMessage = (
    inspected.rawMessage ?? (cause instanceof Error ? cause.message : String(cause))
  ).slice(0, PROVIDER_STOP_RAW_MESSAGE_MAX_CHARS);
  const details: ProviderStopDetails = {
    kind,
    reason: inspected.reason,
    subagent: refToString(state.actor),
    ...(state.actorName === undefined ? {} : { subagentName: state.actorName }),
    ...(inspected.providerId === undefined ? {} : { providerId: inspected.providerId }),
    ...(inspected.modelId === undefined ? {} : { modelId: inspected.modelId }),
    ...(inspected.providerCode === undefined ? {} : { providerCode: inspected.providerCode }),
    ...(rawMessage.length === 0 ? {} : { rawMessage }),
    ...(inspected.resetAt === undefined ? {} : { resetAt: inspected.resetAt }),
  };
  const code = inspected.providerCode === undefined ? "" : ` [${inspected.providerCode}]`;
  return new WorkflowError(
    "ProviderStop",
    `Subagent ${subagentLabel(state)} hit a permanent model-side error ` +
      `(${inspected.reason}${code}): ${rawMessage}`,
    { cause, providerStop: details },
  );
}
