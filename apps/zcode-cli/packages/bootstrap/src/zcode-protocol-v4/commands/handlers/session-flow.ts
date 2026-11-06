// Session flow command group: sendText/stop (mode template).
// One file per command group: handler pure function (host, envelope) → CommandResult|undefined,
// The decision-making logic directly drives the core, and the environment capabilities use the host hook (see the transition annotation of ../types.ts).
import type {
  CommandEnvelope,
  CommandPayloadMap,
  CommandResult,
  SubmissionMode,
} from "@zcode/shared/zcode-protocol-v4";
import type { ModelSelection } from "@zcode/shared";
import { createModelExecutionContext } from "../../../zcode-protocol/model-execution.js";
import type { SteerTurnOptions } from "../../../app/types.js";
import { parseProviderQualifiedModelSelection } from "../../../app/provider-registry-selection.js";
import type { TurnAttachment } from "@zcode/core";
import { mapAttachmentRefsToTurnAttachments } from "../attachment-refs.js";
import { inputIntentMetadata } from "../input-intent.js";
import { startPromptTurn, turnBackgroundAttributionOf } from "../prompt-turn.js";
import { requireRecord } from "../record-access.js";
import type { V4CommandCoreHost, V4SessionRecordView } from "../types.js";
import { V4CommandNoopError } from "../../v4-gateway.js";

/** Wait for idle polling parameters: 25ms interval, 5s timeout. */
const IDLE_POLL_INTERVAL_MS = 25;
const IDLE_POLL_TIMEOUT_MS = 5_000;

export class V4InputAdmissionRejectedError extends Error {
  constructor(
    readonly reasonCode: string,
    message: string,
  ) {
    super(message);
    this.name = "V4InputAdmissionRejectedError";
  }
}

/** V4 unified access to user input: at least one text or attachment exists. */
export function hasPromptInput(text: string, attachments: readonly unknown[] | undefined): boolean {
  return text.trim().length > 0 || Boolean(attachments && attachments.length > 0);
}

/** held(inputRouting.mode=choice) sendText/sendGoalCommand lacks disposition → reject. */
class V4HeldQueueDispositionRequiredError extends Error {
  readonly reasonCode = "heldQueueDispositionRequired";
  constructor() {
    super("held queue requires heldQueueDisposition (clearQueueAndSend | keepQueueAndSend)");
    this.name = "V4HeldQueueDispositionRequiredError";
  }
}

/** After the confirmation box is opened, the queue is added or deleted by the other end: old confirmations cannot continue to be cleared/retained and sent. */
class V4HeldQueueConfirmationStaleError extends Error {
  readonly reasonCode = "guard.heldQueueConfirmationStale";
  constructor() {
    super("paused queue changed after send confirmation opened");
    this.name = "V4HeldQueueConfirmationStaleError";
  }
}

export async function enqueueDeferredInputForBusyWork(
  record: V4SessionRecordView,
  text: string,
  options: {
    commandKind?: SteerTurnOptions["commandKind"];
    inputId: string;
    queryId: SteerTurnOptions["queryId"];
    intent?: SteerTurnOptions["intent"];
    attachments?: TurnAttachment[];
    toolDisallowlist?: SteerTurnOptions["toolDisallowlist"];
  },
): Promise<boolean> {
  if (!record.app.enqueueDeferredInput) return false;
  const result = await record.app.enqueueDeferredInput(text, {
    ...(options.commandKind ? { commandKind: options.commandKind } : {}),
    delivery: "queue",
    inputId: options.inputId,
    ...(options.intent ? { intent: options.intent } : {}),
    ...(options.attachments ? { attachments: options.attachments } : {}),
    ...(options.toolDisallowlist ? { toolDisallowlist: options.toolDisallowlist } : {}),
    queryId: options.queryId,
  });
  if (result.kind === "queued") return true;
  throw new V4InputAdmissionRejectedError(
    result.reason === "input_too_large"
      ? "proto.payloadTooLarge"
      : result.reason === "empty_input"
        ? "proto.invalidPayload"
        : "fault.command.inputRejected",
    `deferred input rejected: ${result.reason}`,
  );
}

/** Wait for idle timeout (the lock is not released within finally 5s of active turn) → give up retransmission and report an error. */
export class V4SessionIdleTimeoutError extends Error {
  constructor(sessionId: string) {
    super(`v4 timed out waiting for session idle: ${sessionId}`);
    this.name = "V4SessionIdleTimeoutError";
  }
}

/**
* When completed + queue>0 + autoDrain=false (projection inputRouting.mode=choice),
* Input is not queued silently:
* clear → clear the queue first and then startNow; keep → keep the queue and startNow directly; default → reject.
 */
export async function applyHeldQueueDisposition(
  host: V4CommandCoreHost,
  record: V4SessionRecordView,
  disposition: "clearQueueAndSend" | "keepQueueAndSend" | undefined,
  expectedQueueItemIds?: readonly string[],
): Promise<void> {
  const routing = host.getInputRoutingMode?.(record.app.sessionId) ?? null;
  if (routing !== "choice") return;
  if (!disposition) {
    throw new V4HeldQueueDispositionRequiredError();
  }
  if (expectedQueueItemIds) {
    const expected = new Set(expectedQueueItemIds);
    const sameItems =
      expected.size === expectedQueueItemIds.length &&
      host.getQueueLength?.(record.app.sessionId) === expected.size &&
      expectedQueueItemIds.every(
        (queueItemId) => host.getQueueItem?.(record.app.sessionId, queueItemId) !== null,
      );
    if (!sameItems) {
      throw new V4HeldQueueConfirmationStaleError();
    }
  }
  if (disposition === "clearQueueAndSend") {
    await record.app.clearQueueItems();
  }
}

/**
* Compatible with admission: the new sender explicitly submits Selection/Mode; the old sender handles it at the CLI receive boundary
* The current Session value is fixed into the canonical intent. After pinning is completed, Queue/Guide will no longer read the variable Session.
 */
export function resolveSubmittedExecutionState(
  record: V4SessionRecordView,
  payload: {
    modelSelection?: ModelSelection;
    mode?: SubmissionMode;
    planEnabled?: boolean;
  },
): { modelSelection: ModelSelection; mode: SubmissionMode; planEnabled: boolean } {
  let modelSelection = payload.modelSelection;
  if (!modelSelection) {
    const runtimeSelection = record.app.runtime?.getSessionModelSelection?.();
    const entrySelection = runtimeSelection
      ? undefined
      : parseProviderQualifiedModelSelection(record.app.getModel());
    if (!runtimeSelection && !entrySelection) {
      throw new Error(`Session model must be provider-qualified: ${record.app.getModel()}`);
    }
// getThoughtLevel() is the effective display fact of Active Model. make it up
// canonical intent will disguise the Config default value as an explicit pin; the old sender can only pin the Session
// A sparse Selection already held cannot be reinterpreted during admission.
    modelSelection = runtimeSelection
      ? {
          providerId: runtimeSelection.providerId,
          modelId: runtimeSelection.modelId,
          ...(runtimeSelection.options ? { options: { ...runtimeSelection.options } } : {}),
        }
      : {
          providerId: entrySelection!.providerId,
          modelId: entrySelection!.modelId,
          ...(entrySelection!.options ? { options: { ...entrySelection!.options } } : {}),
        };
  }
  const current = resolveExecutionState({
    mode: record.app.getMode?.(),
    planEnabled: record.app.runtime?.getPlanEnabled?.(),
  });
  const state = resolveExecutionState(payload, current);
  return {
    modelSelection,
    mode: state.mode === "auto" ? "build" : state.mode,
    planEnabled: state.planEnabled,
  };
}
/**
* sendText: Only perform protocol/held/model/attachment verification, and hand start/queue to Core admission of the same session.
* held(choice) is still determined by heldQueueDisposition.
 */
async function sendText(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["sendText"];
  const record = requireRecord(host, envelope.sessionId);
// The old verification only looks at the text, and the attachment-only query already allowed by the UI will be mistakenly judged as empty by the CLI.
  if (!hasPromptInput(payload.text, payload.attachments)) {
    throw new V4InputAdmissionRejectedError("proto.invalidPayload", "input must not be empty");
  }
  const attachments = await mapAttachmentRefsToTurnAttachments(record.app, payload.attachments);
  const submittedExecutionState = resolveSubmittedExecutionState(record, payload);
  const submissionIntent = (options: Parameters<typeof inputIntentMetadata>[1]) =>
    inputIntentMetadata(envelope, { ...options, ...submittedExecutionState });
  const routingMode = host.getInputRoutingMode?.(envelope.sessionId ?? "") ?? null;
  const forceStartNow = payload.requestedDelivery === "startNow";
  const foregroundPromotionLeaseId = forceStartNow ? `send-now:${envelope.commandId}` : undefined;
  let foregroundPromotionLeaseAcquired = false;
  let preempted = false;
  const releaseForegroundPromotionLease = () => {
    if (!foregroundPromotionLeaseAcquired || !foregroundPromotionLeaseId) return;
    record.app.runtime.releaseForegroundPromotionLease(foregroundPromotionLeaseId);
    foregroundPromotionLeaseAcquired = false;
  };
  if (forceStartNow) {
// If the "send immediately" of the modifier key enters Core busy admission first, the queue item will be created briefly.
// First obtain the only foreground lease and seize the current round, and then hand it to Core to start atomically with idle start_turn.
    const leaseResult = record.app.runtime.acquireForegroundPromotionLease({
      leaseId: foregroundPromotionLeaseId!,
      mode: "after-current",
      promotedInputId: envelope.commandId,
    });
    if (leaseResult.kind !== "acquired") {
      throw new V4InputAdmissionRejectedError(
        "fault.command.inputRejected",
        "send now foreground promotion is busy",
      );
    }
    foregroundPromotionLeaseAcquired = true;
    try {
// startNow old branch mistook the held queue decision as part of the default route and skipped it entirely.
// As a result, old input may still be drained after the user confirms "clear the queue and send". single message
// delivery only determines when new input is consumed and cannot bypass user ruling and expiration verification of existing queues.
      await applyHeldQueueDisposition(
        host,
        record,
        payload.heldQueueDisposition,
        payload.expectedHeldQueueItemIds,
      );
      preempted = await preemptActiveTurnAndWait(host, record, {
        abortMessage: "v4 sendText startNow preempts active turn",
        goalPausedMutationReason: "send_now_goal_paused",
        preserveQueueAutoDrainOnCancel: true,
      });
    } catch (error) {
      releaseForegroundPromotionLease();
      throw error;
    }
  }
  if (!forceStartNow) {
    await applyHeldQueueDisposition(
      host,
      record,
      payload.heldQueueDisposition,
      payload.expectedHeldQueueItemIds,
    );
  }
  let started;
// Attachment command surface: AttachmentRef → TurnAttachment is mapped behind the gate (active turn is excluded).
  try {
    const intent = submissionIntent({
      text: payload.text,
      requestedDelivery:
        payload.requestedDelivery ??
        (routingMode === "guide" ? "guide" : routingMode === "enqueue" ? "queue" : "startNow"),
      ...(routingMode === "guide" && attachments?.length
        ? { fallbackReasonCode: "guide.attachmentsUnsupported" }
        : {}),
      attachmentRefs: payload.attachments,
      sharedContextRefs: payload.context_refs,
    });
    started = await startPromptTurn(host, record, {
      content: payload.text,
      ...(payload.browserAmbientContext
        ? { browserAmbientContext: payload.browserAmbientContext }
        : {}),
      inputId: envelope.commandId,
//Send immediately switches the runtime turn, resulting in missing human prompts for user input during runtime.
// Only mark plain text according to Core's actual preemption receipt; idle and attachment input retain the original path.
      ...(preempted && !attachments?.length ? { inputPresentation: "user_steer" as const } : {}),
      intent,
      ...(payload.context_refs ? { sharedContextRefs: payload.context_refs } : {}),
      ...turnBackgroundAttributionOf(payload),
      toolDisallowlist: payload.toolDisallowlist,
      ...(payload.modelExecution
        ? { modelExecution: createModelExecutionContext(payload.modelExecution) }
        : {}),
      ...(attachments ? { attachments } : {}),
// promotion lease itself belongs to Core busy authority; if requireIdle is not declared,
// After the preemption is completed, startNow will be placed in the deferred queue first, and will be automatically drained after the lease is released.
      ...(forceStartNow ? { requireIdle: true } : {}),
    });
  } finally {
    releaseForegroundPromotionLease();
  }
  if (started.admission.kind === "queued") {
    return {
      type: "inputAccepted",
      delivery: "queue",
      inputId: envelope.commandId,
    };
  }
  return {
    type: "inputAccepted",
    delivery: "startNow",
    inputId: envelope.commandId,
  };
}

/** stop: Exactly cancel the runtime foreground execution seen in the projection, and close the active goal to paused. */
async function stop(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const record = requireRecord(host, envelope.sessionId);
  const payload = envelope.payload as CommandPayloadMap["stop"];
  const runtimeStop = record.app.runtime?.stopActiveForegroundExecution?.({
    expectedForegroundExecutionId: payload.expectedForegroundExecutionId,
    reason: "v4 session stopped",
  });
  host.logger?.info?.("v4 stop foreground execution inspected", {
    activeForegroundExecutionId:
      runtimeStop?.kind === "mismatch"
        ? runtimeStop.activeForegroundExecutionId
        : runtimeStop?.kind === "stopped"
          ? runtimeStop.foregroundExecutionId
          : undefined,
    event: "v4.stop.foreground_execution_inspected",
    expectedForegroundExecutionId: payload.expectedForegroundExecutionId,
    module: "bootstrap.zcode_protocol_v4.commands",
    runtimeStopKind: runtimeStop?.kind ?? "unsupported",
    sessionId: record.app.sessionId,
  });
  if (
    payload.expectedForegroundExecutionId !== undefined &&
    (runtimeStop?.kind === "idle" || runtimeStop?.kind === "mismatch")
  ) {
// Stop There is an asynchronous window from renderer to host; if verifier has ended and the next round has started,
// Continue abort. The outer controller will accidentally kill new executions that the user has not seen. Execution id does not match only noop.
    throw new V4CommandNoopError("guard.stopTargetChanged");
  }
  if (runtimeStop?.kind === "stopped") {
// Interrupt runtime-owned verifier/continuation first, and then wait for goal pause; otherwise verifier may be in
// Pause RPC passes before completion and accesses the next continuation.
    record.activeAbortController?.abort(new Error("v4 session stopped"));
    if ((host.getQueueLength?.(record.app.sessionId) ?? 0) > 0) {
      try {
// verifier has passed the normal turn catch and cannot rely on TurnComplete(cancelled)
// Flip the runtime queue gate; explicitly write false to ensure that the future queue is held in place.
        await record.app.setQueueAutoDrain(false);
      } catch (error) {
        host.logger?.warn?.("v4 stop failed to hold following queue", {
          error: error instanceof Error ? error.message : String(error),
          sessionId: record.app.sessionId,
        });
      }
    }
    const pausedGoal = await pauseActiveGoal(host, record);
    if (pausedGoal) {
      await host.afterLegacyStateMutation?.(record, "session_stop_goal_paused");
    }
    return undefined;
  }

// Compatible with compact and older clients: they do not have runtime foreground execution tokens and are still controlled by
// The bootstrap outer controller provides a cancellation window.
  const hadActivePrompt = Boolean(record.activeAbortController);
  let pausedGoal = false;
  if (hadActivePrompt) {
    pausedGoal = await pauseActiveGoal(host, record);
  }
  record.activeAbortController?.abort(new Error("v4 session stopped"));
  if (pausedGoal) {
    await host.afterLegacyStateMutation?.(record, "session_stop_goal_paused");
  }
  return undefined;
}

/** goal-pause barrier commonware: stop is reused with sendQueuedNow (preemption and resend), and is not copied. */
async function pauseActiveGoal(
  host: V4CommandCoreHost,
  record: V4SessionRecordView,
): Promise<boolean> {
// Note: The method must be called by app (cannot be deconstructed, the implementation may rely on this binding).
  const target = await record.app.readTarget();
  if (!target || target.status !== "active") {
    return false;
  }
  try {
    const paused = await record.app.updateTargetStatus("paused");
    return Boolean(paused);
  } catch (error) {
    host.logger?.warn?.("v4 stop failed to pause active goal", {
      error: error instanceof Error ? error.message : String(error),
      sessionId: record.app.sessionId,
    });
    return false;
  }
}

/** Polling, etc. Bootstrap turn and finally of Core foreground command release authority. */
async function waitForSessionIdle(record: V4SessionRecordView): Promise<void> {
  const deadline = Date.now() + IDLE_POLL_TIMEOUT_MS;
  while (
    record.activeAbortController !== undefined ||
    record.app.runtime?.getActiveForegroundExecutionId?.() !== undefined
  ) {
    if (Date.now() >= deadline) {
      throw new V4SessionIdleTimeoutError(record.app.sessionId);
    }
    await new Promise((resolve) => setTimeout(resolve, IDLE_POLL_INTERVAL_MS));
  }
}

/** Waits for the old execution to be released and returns whether Core actually canceled the foreground execution. */
export async function preemptActiveTurnAndWait(
  host: V4CommandCoreHost,
  record: V4SessionRecordView,
  options: {
    abortMessage: string;
    goalPausedMutationReason: string;
    preserveQueueAutoDrainOnCancel?: boolean;
  },
): Promise<boolean> {
  const bootstrapAbortController = record.activeAbortController;
// model-only turn of background notification is provided by Core runtime command
// Hold foreground authority independently and Bootstrap activeAbortController will not be created.
// Ignoring this will misjudge idle, and then steer the promoted queue item back to the old notification turn.
  const runtimeStop = record.app.runtime?.stopActiveForegroundExecution?.({
    preserveQueueAutoDrainOnCancel: options.preserveQueueAutoDrainOnCancel === true,
    reason: options.abortMessage,
  });
  if (bootstrapAbortController || runtimeStop?.kind === "stopped") {
    const pausedGoal = await pauseActiveGoal(host, record);
    if (runtimeStop?.kind !== "stopped") {
      bootstrapAbortController?.abort(new Error(options.abortMessage));
    }
    if (pausedGoal) {
      await host.afterLegacyStateMutation?.(record, options.goalPausedMutationReason);
    }
  }
  await waitForSessionIdle(record);
  return runtimeStop?.kind === "stopped";
}

export const sessionFlowHandlers = { sendText, stop };
import { resolveExecutionState } from "@zcode/shared";
