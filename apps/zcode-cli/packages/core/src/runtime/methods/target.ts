import {
  CoreErrorType,
  SessionEventType,
  createChildTraceContext,
  createCoreError,
  formatGoalContinuationPrompt,
  traceContextToLogContext,
} from "../deps.js";
import type {
  ModelUsageSummary,
  SessionGoal,
  TargetChangedPayload,
  TraceContext,
} from "../deps.js";
import type { TurnResult } from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { createRuntimeCommandId } from "../command-queue.js";
import type {
  TargetContinuationRuntimeCommand,
  TargetContinuationRuntimeCommandOptions,
} from "../command-queue.js";
import { hasRunningBackgroundRuntimeTask } from "../../runtime-task/registry.js";
import { wrapSystemReminderForSource } from "../../system-reminder/source.js";
import { verifyActiveTargetCompletionForContinuation } from "./target-completion-verification.js";
import { enqueueCancellableRuntimeCommand } from "./runtime-command-submit.js";

export async function recordTargetChanged(
  this: AgentRuntimeInternal,
  input: TargetChangedPayload & { traceContext: TraceContext },
): Promise<void> {
  const { traceContext, ...payload } = input;
  const event = this.createEvent(SessionEventType.TargetChanged, payload, traceContext);
  await this.appendEvent(event, traceContext);
}

export async function continueActiveTargetIfIdle(
  this: AgentRuntimeInternal,
  options?: {
    traceContext?: TraceContext;
    abortSignal?: AbortSignal;
    inputId?: string;
    intent?: TargetContinuationRuntimeCommandOptions["intent"];
    verifyBeforeContinue?: boolean;
  },
): Promise<TurnResult | null> {
  const traceContext = options?.traceContext ?? this.rootTraceContext;

  return await enqueueCancellableRuntimeCommand<
    TurnResult | null,
    TargetContinuationRuntimeCommand
  >(this, {
    abortSignal: options?.abortSignal,
    createCommand: ({ reject, resolve }) => {
      const commandOptions: TargetContinuationRuntimeCommandOptions = {
        ...(options?.abortSignal ? { abortSignal: options.abortSignal } : {}),
        ...(options?.inputId !== undefined ? { inputId: options.inputId } : {}),
        ...(options?.intent ? { intent: options.intent } : {}),
        traceContext,
        ...(options?.verifyBeforeContinue !== undefined
          ? { verifyBeforeContinue: options.verifyBeforeContinue }
          : {}),
      };
      return {
        createdAt: new Date(),
        id: createRuntimeCommandId(),
        mode: "target-continuation",
        options: commandOptions,
        priority: "next",
        reject,
        resolve,
        traceContext,
      };
    },
  });
}

export async function executeTargetContinuationCommand(
  this: AgentRuntimeInternal,
  options: TargetContinuationRuntimeCommandOptions,
): Promise<TurnResult | null> {
  const traceContext = options.traceContext;
  const target = await targetContinuationCandidateForCommand.call(this, traceContext);
  if (!target) return null;

  if (
    options.verifyBeforeContinue === true &&
    (await hasRunningBackgroundTaskForGoalContinuation.call(this))
  ) {
    this.logger?.info("Goal continuation deferred while background tasks are running", {
      ...traceContextToLogContext(traceContext),
      event: "target.continuation.deferred_background_running",
      module: "core.runtime",
      status: "waiting",
      targetId: target.targetID,
    });
    return null;
  }

  const verificationResult = options.verifyBeforeContinue
    ? await verifyActiveTargetCompletionForContinuation.call(this, {
        abortSignal: options.abortSignal,
        target,
        traceContext,
      })
    : null;
  if (verificationResult?.verification.passed) {
    this.logger?.info("Goal continuation skipped after completion verifier passed", {
      ...traceContextToLogContext(traceContext),
      event: "target.continuation.skipped_complete",
      module: "core.runtime",
      status: "completed",
      targetId: verificationResult.target.targetID,
    });
    return null;
  }
// Failure results without nextAction come from verifier's own failure or invalid output,
// This is not the next step in model validation; continuing to run automatically will turn an internal error into an infinite target iteration.
  if (verificationResult && !verificationResult.verification.nextAction?.trim()) {
    this.logger?.warn("Goal continuation skipped after verifier failed without next action", {
      ...traceContextToLogContext(traceContext),
      event: "target.continuation.skipped_no_next_action",
      module: "core.runtime",
      reason: verificationResult.verification.reason,
      targetId: verificationResult.target.targetID,
    });
    return null;
  }
  const latestTarget = await this.readSessionTargetForContext(traceContext);
// The target verification request may not return until the user clicks Stop; the queue will remain stopRequested,
// But the verifier gets the active target before the verification starts. The target state must be reread here to avoid continuing to run with old objects.
  if (
    !latestTarget ||
    latestTarget.status !== "active" ||
    latestTarget.targetID !== target.targetID
  ) {
    this.logger?.info("Goal continuation skipped after target changed during verification", {
      ...traceContextToLogContext(traceContext),
      currentStatus: latestTarget?.status ?? "missing",
      currentTargetId: latestTarget?.targetID,
      event: "target.continuation.skipped_inactive_after_verification",
      module: "core.runtime",
      targetId: target.targetID,
    });
    return null;
  }
  const continuationTarget = latestTarget;
  const prompt = wrapSystemReminderForSource(
    "target_continuation",
    formatGoalContinuationPrompt(continuationTarget, verificationResult?.verification),
  );
  const continuationTrace = createChildTraceContext(traceContext, {
    sessionId: this.sessionId,
    attributes: {
      targetId: continuationTarget.targetID,
      targetContinuation: true,
    },
  });

  this.logger?.info("Goal continuation started", {
    ...traceContextToLogContext(continuationTrace),
    event: "target.continuation.started",
    module: "core.runtime",
    status: "started",
    targetId: continuationTarget.targetID,
  });

  return await this.executeTurnCommand(prompt, undefined, {
    abortSignal: options.abortSignal,
// The automatic continuation of the target triggered during session/send still belongs to the same user submission.
// If the inputId is lost here, eventually turn.completed will fall back to the runtime trace.
// Desktop activeInputId will always stop at streaming/loading if it fails to match.
    inputId: options.inputId,
    intent: options.intent,
    inputSource: "goal-continuation",
    inputVisibility: "model-only",
    targetId: continuationTarget.targetID,
    traceContext: continuationTrace,
  });
}

export async function targetContinuationCandidate(
  this: AgentRuntimeInternal,
  traceContext: TraceContext,
): Promise<SessionGoal | null> {
  if (this.hasActiveOrQueuedTurnWork()) return null;
  return await targetContinuationCandidateForCommand.call(this, traceContext);
}

async function targetContinuationCandidateForCommand(
  this: AgentRuntimeInternal,
  traceContext: TraceContext,
): Promise<SessionGoal | null> {
  if (!this.sessionStore) return null;
  if (this.getPlanEnabled()) return null;
  if (this.activeTurn || this.activeTurnStartReservation) return null;
  if (!this.sessionPersisted) return null;

  const target = await this.readSessionTargetForContext(traceContext);
  if (!target || target.status !== "active") return null;
  return target;
}

async function hasRunningBackgroundTaskForGoalContinuation(
  this: AgentRuntimeInternal,
): Promise<boolean> {
  return hasRunningBackgroundRuntimeTask(this.runtimeTaskRegistry);
}

export async function accountTargetTurnCompletion(
  this: AgentRuntimeInternal,
  input: {
    inputID: string;
    startedAtMs: number;
    startedTarget: SessionGoal | null;
    traceContext: TraceContext;
    usage?: ModelUsageSummary;
  },
): Promise<void> {
  const target = input.startedTarget;
  if (!target || !this.sessionStore) return;
  if (target.status !== "active") return;

  const tokensUsedDelta = input.usage?.totalTokens ?? 0;
  const endedAtMs = Date.now();

  try {
    const previousTarget = await this.readSessionTargetForContext(input.traceContext);
    const accountedTarget = this.sessionStore.finishTargetRun
      ? await this.sessionStore.finishTargetRun({
          endedAtMs,
          inputID: input.inputID,
          sessionID: this.sessionId,
          targetID: target.targetID,
          tokensUsedDelta,
        })
      : await this.sessionStore.accountTargetUsage({
          sessionID: this.sessionId,
          targetID: target.targetID,
          tokensUsedDelta,
          timeUsedSecondsDelta: Math.max(0, Math.ceil((endedAtMs - input.startedAtMs) / 1000)),
        });
    if (accountedTarget?.targetID === target.targetID) {
      await this.recordTargetChanged({
        action: this.sessionStore.finishTargetRun ? "run_finished" : "usage_accounted",
        previousTarget,
        source: "runtime",
        target: accountedTarget,
        traceContext: input.traceContext,
      });
    }
  } catch (error) {
    this.logger?.warn("Failed to account goal turn completion", {
      ...traceContextToLogContext(input.traceContext),
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "target.account.failed",
      module: "core.runtime",
      status: "failed",
      targetId: target.targetID,
    });
  }
}

export async function startTargetTurnAccounting(
  this: AgentRuntimeInternal,
  input: {
    inputID: string;
    startedAtMs: number;
    startedTarget: SessionGoal | null;
    traceContext: TraceContext;
  },
): Promise<SessionGoal | null> {
  const target = input.startedTarget;
  if (!target || !this.sessionStore?.startTargetRun) return target;
  if (target.status !== "active") return target;

  try {
    const startedTarget = await this.sessionStore.startTargetRun({
      inputID: input.inputID,
      sessionID: this.sessionId,
      startedAtMs: input.startedAtMs,
      targetID: target.targetID,
    });
    if (startedTarget?.targetID === target.targetID) {
      await this.recordTargetChanged({
        action: "run_started",
        previousTarget: target,
        source: "runtime",
        target: startedTarget,
        traceContext: input.traceContext,
      });
      return startedTarget;
    }
    return target;
  } catch (error) {
    this.logger?.warn("Failed to start goal active run accounting", {
      ...traceContextToLogContext(input.traceContext),
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "target.run_start.failed",
      module: "core.runtime",
      status: "failed",
      targetId: target.targetID,
    });
    return target;
  }
}

export async function heartbeatTargetTurnAccounting(
  this: AgentRuntimeInternal,
  input: {
    inputID: string;
    seenAtMs: number;
    startedTarget: SessionGoal | null;
    traceContext: TraceContext;
  },
): Promise<void> {
  const target = input.startedTarget;
  if (!target || !this.sessionStore?.heartbeatTargetRun) return;
  if (target.status !== "active") return;
  try {
    await this.sessionStore.heartbeatTargetRun({
      inputID: input.inputID,
      seenAtMs: input.seenAtMs,
      sessionID: this.sessionId,
      targetID: target.targetID,
    });
  } catch (error) {
    this.logger?.debug("Failed to heartbeat goal active run accounting", {
      ...traceContextToLogContext(input.traceContext),
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "target.run_heartbeat.failed",
      module: "core.runtime",
      status: "failed",
      targetId: target.targetID,
    });
  }
}

export async function finishTargetTurnAccounting(
  this: AgentRuntimeInternal,
  input: {
    inputID: string;
    endedAtMs: number;
    startedTarget: SessionGoal | null;
    status?: "paused";
    traceContext: TraceContext;
  },
): Promise<SessionGoal | null> {
  const target = input.startedTarget;
  if (!target || !this.sessionStore?.finishTargetRun) return target;
  if (target.status !== "active") return target;

  try {
    const previousTarget = await this.readSessionTargetForContext(input.traceContext);
    const finishedTarget = await this.sessionStore.finishTargetRun({
      endedAtMs: input.endedAtMs,
      inputID: input.inputID,
      sessionID: this.sessionId,
      status: input.status,
      targetID: target.targetID,
    });
    if (finishedTarget?.targetID === target.targetID) {
      await this.recordTargetChanged({
        action: "run_finished",
        previousTarget,
        source: "runtime",
        target: finishedTarget,
        traceContext: input.traceContext,
      });
      return finishedTarget;
    }
    return target;
  } catch (error) {
    this.logger?.warn("Failed to finish goal active run accounting", {
      ...traceContextToLogContext(input.traceContext),
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "target.run_finish.failed",
      module: "core.runtime",
      status: "failed",
      targetId: target.targetID,
    });
    return target;
  }
}

export async function pauseActiveTargetForCancellation(
  this: AgentRuntimeInternal,
  traceContext: TraceContext,
): Promise<void> {
  if (!this.sessionStore) return;

  const target = await this.readSessionTargetForContext(traceContext);
  if (!target || target.status !== "active") return;

  try {
    const pausedTarget = await this.sessionStore.updateTargetStatus({
      sessionID: this.sessionId,
      status: "paused",
    });
    if (pausedTarget) {
      await this.recordTargetChanged({
        action: "status_updated",
        previousTarget: target,
        source: "runtime",
        target: pausedTarget,
        traceContext,
      });
    }
  } catch (error) {
    throw createCoreError(
      CoreErrorType.InvalidStateTransition,
      "Failed to pause active goal after turn cancellation",
      {
        cause: error instanceof Error ? error : new Error(String(error)),
        context: { sessionId: this.sessionId, targetId: target.targetID },
        recoverable: true,
      },
    );
  }
}

export async function activatePausedTargetAfterResume(
  this: AgentRuntimeInternal,
  traceContext: TraceContext,
): Promise<SessionGoal | null> {
  if (!this.sessionStore || this.getPlanEnabled()) return null;
  const target = await this.readSessionTargetForContext(traceContext);
// When the user stops a running goal, canceling the closing will mark the target as paused.
// Cold recovery cannot automatically change it back to active, otherwise verifier/continuation will continue after the user explicitly stops it.
// It will also cause the desktop queue "send immediately" to be stuck in the residual active goal state. Explicit /goal resume is required to reactivate.
  return target;
}
