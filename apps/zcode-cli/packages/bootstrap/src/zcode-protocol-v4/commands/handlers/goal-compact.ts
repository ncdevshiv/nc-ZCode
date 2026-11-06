/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
// goal/compact command group: compact / sendGoalCommand / pauseGoal / resumeGoal.
// Semantics are transferred from the old server-operations compactSession/goalSession/continueGoalAfterChange,
// Decision logic (compact deduplication / active turn barrier / goal continuation) directly drives the core without going through the old protocol op.
//
// Mapping to old protocol paths (fidelity baseline):
// - compactSession（server-operations.ts:1805）→ compact
// - goalSession action: "set" (:1919, including repeated set convergence replace) → sendGoalCommand
// - goalSession action:"resume"（:1980）→ resumeGoal
// - continueGoalAfterChange / runGoalContinuationInBackground (:2204-2269) → private shared function in the group
import type {
  CommandEnvelope,
  CommandPayloadMap,
  CommandResult,
} from "@zcode/shared/zcode-protocol-v4";
import type { SteerTurnOptions, SubmitPromptOptions } from "../../../app/types.js";
import { runWithSessionResidencyFinalization } from "../../../zcode-protocol/session-residency.js";
import { inputIntentMetadata } from "../input-intent.js";
import { requireRecord } from "../record-access.js";
import type { V4CommandCoreHost, V4SessionRecordView } from "../types.js";
import {
  applyHeldQueueDisposition,
  enqueueDeferredInputForBusyWork,
  resolveSubmittedExecutionState,
  V4InputAdmissionRejectedError,
} from "./session-flow.js";

/** The ruling of the goal/compact group is rejected (the gateway captures the backward ACK failed, and the message is transparently transmitted to the client). */
export class V4GoalCompactRejectedError extends Error {
  constructor(
    readonly reasonCode:
      | "activeTurn"
      | "compactOperationLock"
      | "restoreWarning"
      | "guard.planGoalMutuallyExclusive"
      | "emptyObjective",
    message: string,
  ) {
    super(message);
    this.name = "V4GoalCompactRejectedError";
  }
}

// Manual compact turns in the background before the actual registration, the next command may have completed admission.
// Only looking at runtime activeTurn will leave a very short duplicate compact window; controller WeakSet only fills in
// The synchronization boundary of operation lock does not copy queue or lifecycle business status.
const manualCompactControllers = new WeakSet<AbortController>();

/**
* compact: manual context compression (v4 payload is an empty object, no instructions variant).
 *
* barrier semantics:
* 1. running/goal verifier/goal continuation/tool ​​work → typed compact intent into FIFO.
* 2. held queue → append to the end of the queue without bypassing the existing future intent.
* 3. Running or queued compact already exists → compactOperationLock, prohibiting repeated compression.
 */
async function compact(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const record = requireRecord(host, envelope.sessionId);
  const activeTurn = record.app.runtime.getActiveTurnInfo();
  const activeController = record.activeAbortController;
  if (
    activeTurn?.kind === "compact" ||
    (activeController ? manualCompactControllers.has(activeController) : false) ||
    host.hasQueueItemKind?.(record.app.sessionId, "compact")
  ) {
    host.logger?.info?.("v4 compact already running or queued, rejected", {
      commandId: envelope.commandId,
      sessionId: record.app.sessionId,
      workspacePath: record.workspace.workspacePath,
    });
    throw new V4GoalCompactRejectedError(
      "compactOperationLock",
      "Compact is already running or queued",
    );
  }
  if (record.restoreWarning) {
// The same gate as prompt-turn: a session that fails to be restored cannot be silently resumed (including compact turn).
    throw new V4GoalCompactRejectedError("restoreWarning", record.restoreWarning.message);
  }

  const routingMode = host.getInputRoutingMode?.(record.app.sessionId) ?? null;
  const busy = Boolean(record.activeAbortController) || Boolean(activeTurn);
  if (busy || routingMode === "enqueue" || routingMode === "guide" || routingMode === "choice") {
    const intent = inputIntentMetadata(envelope, {
      requestedDelivery: "queue",
      text: "/compact",
    });
    const queueOptions = {
      commandKind: "compact" as const,
      inputId: envelope.commandId,
      intent,
      queryId: envelope.commandId as NonNullable<SteerTurnOptions["queryId"]>,
    };
    if (await enqueueDeferredInputForBusyWork(record, "/compact", queueOptions)) {
      return undefined;
    }
    const queued = await record.app.steerTurn("/compact", {
      ...queueOptions,
      delivery: "queue",
    });
    if (queued.kind === "rejected") {
      throw new V4InputAdmissionRejectedError(
        queued.reason === "input_too_large"
          ? "proto.payloadTooLarge"
          : queued.reason === "empty_input"
            ? "proto.invalidPayload"
            : "fault.command.inputRejected",
        `compact input queue rejected: ${queued.reason}`,
      );
    }
    return undefined;
  }

  await startManualCompact(host, record, envelope.commandId);
  return undefined;
}

/** queue promotion shares the only manual compact startup path with direct commands. */
export async function startManualCompact(
  host: V4CommandCoreHost,
  record: V4SessionRecordView,
  inputId: string,
  foregroundPromotionLeaseId?: string,
): Promise<void> {
  if (record.restoreWarning) {
    throw new V4GoalCompactRejectedError("restoreWarning", record.restoreWarning.message);
  }
// After cold recovery, the user may directly trigger /compact (without going through sendText first), and compact the background model request
// Also requires model readiness check - hook (see types.ts).
  await host.ensureModelReady?.(record);
  const abortController = new AbortController();
// The real model request for compact is executed in the background, but Stop still passes
// record.activeAbortController interrupt. If the controller is not registered, requests during compression will end naturally.
  record.activeAbortController = abortController;
  manualCompactControllers.add(abortController);
  void runWithSessionResidencyFinalization(record, () =>
    runCompactTurnInBackground(host, record, {
      abortController,
      foregroundPromotionLeaseId,
      inputId,
    }),
  ).catch(() => {
//Background compact errors are reported via event flow (CompactStarted/final state marker) downgrade; to prevent unhandled rejection.
  });
}

async function runCompactTurnInBackground(
  host: V4CommandCoreHost,
  record: V4SessionRecordView,
  params: {
    abortController: AbortController;
    foregroundPromotionLeaseId?: string;
    inputId: string;
  },
): Promise<void> {
  const startedAt = Date.now();
  let mutationReason = "session_compacted";
  let lifecycleStatus: "success" | "failed" | "cancelled" = "success";
  host.logger?.info?.("v4 background compact started", {
    inputId: params.inputId,
    sessionId: record.app.sessionId,
    workspacePath: record.workspace.workspacePath,
  });
  try {
    await record.app.submitPrompt("/compact", {
      abortSignal: params.abortController.signal,
      inputId: params.inputId,
    });
  } catch (error) {
    lifecycleStatus = params.abortController.signal.aborted ? "cancelled" : "failed";
    mutationReason =
      lifecycleStatus === "cancelled" ? "session_compact_cancelled" : "session_compact_failed";
    if ((host.getQueueLength?.(record.app.sessionId) ?? 0) > 0) {
      try {
// queued compact is a FIFO barrier; if you continue auto-drain after failure/Stop,
// Subsequent text overrides the user's explicit maintenance intent. Same as ordinary Stop and is held.
        await record.app.setQueueAutoDrain(false);
      } catch (holdError) {
        host.logger?.warn?.("v4 compact failed to hold following queue", {
          error: holdError instanceof Error ? holdError.message : String(holdError),
          inputId: params.inputId,
          sessionId: record.app.sessionId,
        });
      }
    }
    host.logger?.warn?.("v4 background compact failed", {
      durationMs: Date.now() - startedAt,
      error: error instanceof Error ? error.message : String(error),
      inputId: params.inputId,
      sessionId: record.app.sessionId,
      workspacePath: record.workspace.workspacePath,
    });
  } finally {
    if (params.foregroundPromotionLeaseId) {
      record.app.runtime.releaseForegroundPromotionLease(params.foregroundPromotionLeaseId);
    }
    manualCompactControllers.delete(params.abortController);
    if (record.activeAbortController === params.abortController) {
// ready boundary (key constraint): after compact ends, active lock must be released before broadcasting;
// Otherwise the queued prompt or subsequent /compact will briefly hit the old controller at the ready boundary.
      record.activeAbortController = undefined;
    }
  }
  try {
// compact has no user message and cannot rely on SessionInputPromoted to solve the pin; regardless of lifecycle
// For success/failure/cancellation, timeline command fact is used to prevent the executed command from being prompted for replay after restart.
    await host.recordPersistentCommandFact?.(
      record.app.sessionId,
      "timeline",
      {
        commandId: params.inputId,
        status: "accepted",
        revisionAtDecision: 0,
      },
      { lifecycleStatus },
    );
  } catch (error) {
// compact has been executed and cannot be disguised as model execution failure due to duplication check bypass write failure.
    host.logger?.warn?.("v4 compact persistent command fact failed", {
      commandId: params.inputId,
      error: error instanceof Error ? error.message : String(error),
      sessionId: record.app.sessionId,
    });
  }
// The old protocol path is adjusted here afterStateMutation → replaced by the hook equivalent (deleted with the old broadcast).
  await host.afterLegacyStateMutation?.(record, mutationReason);
}

/**
* sendGoalCommand: set/update goal (old goalSession action: "set").
 *
* barrier semantics (since old protocol path fidelity + v4 queue completion):
* 1. active turn → join the queue as sendGoalCommand: /goal is the target status write, not a normal prompt;
* The target cannot be changed directly during operation, but it cannot be discarded either. Queue items must retain command identity pending execution at the ready boundary.
* 2. Repeated set converges replace semantics (key constraint): `/goal new goal` in the input box is the user
* Explicitly submitted new goals; continuing to require replace when there is already a goal will make the user think that the goal has changed but the database remains
* Old target. When it is read here that the target already exists, it will be processed according to the replacement path (the difference is only reflected in the broadcast reason).
 */
async function sendGoalCommand(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["sendGoalCommand"];
  const record = requireRecord(host, envelope.sessionId);
  const objective = payload.text.trim();
  if (objective.length === 0) {
    throw new V4GoalCompactRejectedError("emptyObjective", "Usage: /goal <objective>");
  }
  const submittedExecutionState = resolveSubmittedExecutionState(record, payload);
  if (submittedExecutionState.planEnabled) {
    throw new V4GoalCompactRejectedError(
      "guard.planGoalMutuallyExclusive",
      "Plan and Goal cannot be active at the same time.",
    );
  }
  const submissionIntent = (options: Parameters<typeof inputIntentMetadata>[1]) =>
    inputIntentMetadata(envelope, { ...options, ...submittedExecutionState });
  const routingMode = host.getInputRoutingMode?.(record.app.sessionId) ?? null;
  if (record.activeAbortController || routingMode === "enqueue" || routingMode === "guide") {
// /goal is a target control command, target cannot be written directly in active turn;
// But product semantics require that running/compacting/goal verifier can be enqueued. busy projection possible
// Registered earlier than the controller, so inputRouting is consumed at the same time; commandKind retains the identity of the control command,
// For subsequent consumption, use sendGoalCommand instead of the normal user prompt.
    const queuedText = goalCommandQueueText(payload.displayText, objective);
    if (
      await enqueueDeferredInputForBusyWork(record, queuedText, {
        commandKind: "sendGoalCommand",
        inputId: envelope.commandId,
        queryId: envelope.commandId as SteerTurnOptions["queryId"],
        intent: submissionIntent({ requestedDelivery: "queue", text: objective }),
      })
    ) {
      return undefined;
    }
    const queued = await record.app.steerTurn(queuedText, {
      commandKind: "sendGoalCommand",
      inputId: envelope.commandId,
      queryId: envelope.commandId as SteerTurnOptions["queryId"],
      intent: submissionIntent({ requestedDelivery: "queue", text: objective }),
    });
    if (queued.kind === "rejected") {
      throw new V4InputAdmissionRejectedError(
        queued.reason === "input_too_large"
          ? "proto.payloadTooLarge"
          : queued.reason === "empty_input"
            ? "proto.invalidPayload"
            : "fault.command.inputRejected",
        `goal input queue rejected: ${queued.reason}`,
      );
    }
    return undefined;
  }
  await applyGoalCommand(host, record, {
    displayText: goalCommandQueueText(payload.displayText, objective),
    heldQueueDisposition: payload.heldQueueDisposition,
    expectedHeldQueueItemIds: payload.expectedHeldQueueItemIds,
    inputId: envelope.commandId,
    objective,
    intent: submissionIntent({ requestedDelivery: "startNow", text: objective }),
  });
  return undefined;
}

export async function applyGoalCommand(
  host: V4CommandCoreHost,
  record: V4SessionRecordView,
  params: {
    displayText?: string;
    heldQueueDisposition?: "clearQueueAndSend" | "keepQueueAndSend";
    expectedHeldQueueItemIds?: readonly string[];
    inputId: string;
    objective: string;
    foregroundPromotionLeaseId?: string;
    intent?: SteerTurnOptions["intent"];
  },
): Promise<void> {
// held choice ruling (same as sendText; sendGoalCommand).
  await applyHeldQueueDisposition(
    host,
    record,
    params.heldQueueDisposition,
    params.expectedHeldQueueItemIds,
  );
  const replacesExistingGoal = Boolean(await record.app.readTarget());
// The execution status of the Goal submission has also been frozen; the Plan that was explicitly canceled this time is closed first, and the continuation of the run cannot be blocked according to the old Runtime status.
  if (params.intent?.planEnabled !== undefined) {
    if (params.intent.planEnabled)
      throw new V4GoalCompactRejectedError(
        "guard.planGoalMutuallyExclusive",
        "Plan and Goal cannot be active at the same time.",
      );
    await record.app.runtime.setExecutionState(
      { mode: params.intent.mode, planEnabled: false },
      record.traceContext,
    );
  }
  await record.app.setTarget({
    ...(params.displayText ? { displayText: params.displayText } : {}),
    objective: params.objective,
    status: "active",
    ...(params.intent ? { intent: params.intent } : {}),
  });
  await continueGoalAfterChange(host, record, {
    foregroundPromotionLeaseId: params.foregroundPromotionLeaseId,
    inputId: params.inputId,
    intent: params.intent,
    reason: replacesExistingGoal ? "goal_replaced" : "goal_set",
  });
}

export function parseGoalObjectiveFromCommandText(text: string): string {
  const trimmed = text.trim();
  const match = /^\/(?:goal|target)(?:\s+([\s\S]*))?$/i.exec(trimmed);
  if (!match) return trimmed;
  const args = match[1]?.trim() ?? "";
  return args.replace(/^replace\s+/i, "").trim();
}

function goalCommandQueueText(displayText: string | undefined, objective: string): string {
  const trimmed = displayText?.trim();
  return trimmed ? trimmed : `/goal ${objective}`;
}

/**
* pauseGoal: independent target control, does not reuse the queue hold/disposition of general stop.
* The old V4 only has stop, which makes it impossible to pause the target when there is no active controller, and also makes the UI unable to
* Accurately express the difference between "pausing the target" and "terminating the current round". The target active run is settled first, and then the current goal work is terminated.
 */
async function pauseGoal(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const record = requireRecord(host, envelope.sessionId);
  const target = await record.app.readTarget();
  if (!target || target.status !== "active") {
    return undefined;
  }

  const activeController = record.activeAbortController;
  const paused = await record.app.updateTargetStatus("paused");
  if (!paused) return undefined;

  activeController?.abort(new Error("v4 goal paused"));
  await host.afterLegacyStateMutation?.(record, "goal_paused");
  return undefined;
}

/**
* resumeGoal: paused → active (the reverse operation of stopPausesActiveGoalTarget).
* No target → Idempotent success (the old protocol path returns "No goal to resume." and does not change the status and does not throw an error).
 */
async function resumeGoal(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const record = requireRecord(host, envelope.sessionId);
  if (record.activeAbortController) {
// Same as sendGoalCommand: resume is a non-pause action of the old goalSession and is rejected during operation.
    throw new V4GoalCompactRejectedError(
      "activeTurn",
      "Cannot manage goals while a prompt is running",
    );
  }
// Just skipping the continuation will still leave active Goal + Plan; check before restoring the goal, you cannot write it first and then reject it.
  const planEnabled = record.app.runtime?.getPlanEnabled?.() ?? record.app.getMode?.() === "plan";
  if (planEnabled && (await record.app.readTarget())) {
    throw new V4GoalCompactRejectedError(
      "guard.planGoalMutuallyExclusive",
      "Plan and Goal cannot be active at the same time.",
    );
  }
  const target = await record.app.updateTargetStatus("active");
  if (!target) {
    host.logger?.info?.("v4 resumeGoal without target, noop", {
      commandId: envelope.commandId,
      sessionId: record.app.sessionId,
    });
    return undefined;
  }
  await continueGoalAfterChange(host, record, {
    inputId: envelope.commandId,
    reason: "goal_resumed",
  });
  return undefined;
}

/**
* Continue running after goal change (old continueGoalAfterChange handling, set/resume is shared by both places):
* Do not continue running in plan mode or when there is an active turn (only the target will be dropped, and the user will explicitly advance in the future);
* Otherwise model readiness check → lock → background continueActiveTarget.
 */
async function continueGoalAfterChange(
  host: V4CommandCoreHost,
  record: V4SessionRecordView,
  params: {
    foregroundPromotionLeaseId?: string;
    inputId: string;
    intent?: SteerTurnOptions["intent"];
    reason: string;
  },
): Promise<void> {
  const isPlanMode = record.app.runtime?.getPlanEnabled?.() ?? record.app.getMode?.() === "plan";
// continueActiveTarget is a required capability of App; whether it can continue only depends on the current mode and whether there is an active turn.
  const canContinue = !isPlanMode && !record.activeAbortController;
  if (canContinue) {
    await host.ensureModelReady?.(record);
    const abortController = new AbortController();
    record.activeAbortController = abortController;
    void runWithSessionResidencyFinalization(record, () =>
      runGoalContinuationInBackground(host, record, {
        abortController,
        foregroundPromotionLeaseId: params.foregroundPromotionLeaseId,
        inputId: params.inputId,
        intent: params.intent,
      }),
    ).catch(() => {
// The failure of the background goal continuation is reported through the event flow downgrade; to prevent unhandled rejection.
    });
  }
// The old protocol path is immediately after the restart afterStateMutation(goal_set/goal_replaced/goal_resumed)
// → Equivalent replacement for hooks; v4 projections are naturally closed via the TargetChanged event.
  await host.afterLegacyStateMutation?.(record, params.reason);
}

async function runGoalContinuationInBackground(
  host: V4CommandCoreHost,
  record: V4SessionRecordView,
  params: {
    abortController: AbortController;
    foregroundPromotionLeaseId?: string;
    inputId: string;
    intent?: SteerTurnOptions["intent"];
  },
): Promise<void> {
  let mutationReason = "goal_continuation_completed";
  try {
    await record.app.continueActiveTarget?.({
      abortSignal: params.abortController.signal,
      inputId: params.inputId,
      intent: params.intent,
      queryId: params.inputId as SubmitPromptOptions["queryId"],
    });
  } catch {
    mutationReason = "goal_continuation_failed";
  } finally {
    if (params.foregroundPromotionLeaseId) {
      record.app.runtime.releaseForegroundPromotionLease(params.foregroundPromotionLeaseId);
    }
    if (record.activeAbortController === params.abortController) {
// The active lock should be released immediately after the continuation is completed; broadcasting is just a follow-up action.
// If the lock continues to be occupied, consecutive /goals will be misjudged as active turns.
      record.activeAbortController = undefined;
    }
  }
  await host.afterLegacyStateMutation?.(record, mutationReason);
}

export const goalCompactHandlers = { compact, pauseGoal, resumeGoal, sendGoalCommand };
