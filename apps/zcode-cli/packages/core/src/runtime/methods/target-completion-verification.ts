import {
  SessionEventType,
  GOAL_COMPLETION_VERIFICATION_QUERY_SOURCE,
  createChildTraceContext,
  failOpenGoalCompletionVerification,
  failedGoalCompletionVerification,
  formatGoalCompletionVerificationPrompt,
  parseGoalCompletionVerificationText,
  runWithModelInvocationContext,
  traceContextToLogContext,
} from "../deps.js";
import type {
  GoalCompletionVerificationOutput,
  Model,
  SessionEvent,
  SessionGoal,
  TraceContext,
} from "../deps.js";
import { buildRuntimeProviderRequestMessages, throwIfTurnAborted } from "../helpers/index.js";
import { projectMessagesForModelMediaPolicy } from "../helpers/media-budget.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { isRuntimeAttachmentEntry, type RuntimeMessageEntry } from "../../agent/message-history.js";
import { createRefreshRuntimeHeadersBeforeModelAttempt } from "./model-runtime-headers.js";
import { resolveModelRequestSessionTypeFromTaskType } from "./model-request-session-type.js";
import { createRuntimeModel } from "./runtime-model.js";
import { isStartPlanBusyStreamRecoveryFailure } from "./streaming-recovery.js";
import { recordModelUsageFact } from "./usage-observability.js";
import { runTargetCompletionVerificationWithTelemetry } from "./target-completion-verification-telemetry.js";

export interface TargetCompletionVerificationResult {
  target: SessionGoal;
  verification: GoalCompletionVerificationOutput;
}

const TARGET_VERIFIER_START_PLAN_BUSY_RETRY_DELAYS_MS = [1_000, 2_000] as const;
const START_PLAN_TARGET_VERIFIER_RETRY_PROVIDER_IDS = new Set([
  "account:bigmodel-start-plan",
  "account:zai-start-plan",
]);

export async function verifyActiveTargetCompletionForContinuation(
  this: AgentRuntimeInternal,
  input: {
    abortSignal?: AbortSignal;
    target: SessionGoal;
    traceContext: TraceContext;
  },
): Promise<TargetCompletionVerificationResult | null> {
  if (this.config.targetCompletionVerification?.enabled === false) return null;
  if (!this.sessionStore) return null;
  if (input.target.status !== "active") return null;

  const execute = async (): Promise<TargetCompletionVerificationResult> => {
    const events: SessionEvent[] = [];
    const verification = await verifyTargetCompletion.call(this, {
      abortSignal: input.abortSignal,
      events,
      target: input.target,
      traceContext: input.traceContext,
    });

    if (!verification.passed) {
      return {
        target: input.target,
        verification,
      };
    }

    const previousTarget = await this.readSessionTargetForContext(input.traceContext);
    const completedTarget =
      (await this.sessionStore!.updateTargetStatus({
        sessionID: this.sessionId,
        status: "complete",
      })) ?? input.target;
    await this.recordTargetChanged({
      action: "status_updated",
      previousTarget,
      source: "runtime",
      target: completedTarget,
      traceContext: input.traceContext,
    });

    return {
      target: completedTarget,
      verification,
    };
  };
  return runTargetCompletionVerificationWithTelemetry(this, input, execute);
}

async function verifyTargetCompletion(
  this: AgentRuntimeInternal,
  input: {
    abortSignal?: AbortSignal;
    events: SessionEvent[];
    target: SessionGoal;
    traceContext: TraceContext;
  },
): Promise<GoalCompletionVerificationOutput> {
// Switching the session default model is also allowed during target verification; request, retry and usage must be shared
// Model snapshot at the beginning of validation, Session Selection cannot be re-read after provider returns.
  const requestedModelSelection = this.getSessionModelSelection();
  const model = createRuntimeModel(this, { selection: requestedModelSelection });
  const modelTraceContext = createChildTraceContext(input.traceContext, {
    attributes: {
      model: `${model.providerId}/${model.modelId}`,
      querySource: GOAL_COMPLETION_VERIFICATION_QUERY_SOURCE,
      targetId: input.target.targetID,
    },
  });
  const verificationId = modelTraceContext.spanId ?? modelTraceContext.traceId;
  const foregroundExecutionId = this.activeForegroundExecution?.foregroundExecutionId;
  const goalIteration = await getNextTargetCompletionVerificationIteration.call(
    this,
    input.target.targetID,
  );
  const anchor = targetCompletionVerificationAnchor.call(this, input.traceContext);
  await this.appendEvent(
    this.createEvent(
      SessionEventType.TargetCompletionVerification,
      {
        ...anchor,
        ...(foregroundExecutionId ? { foregroundExecutionId } : {}),
        goalIteration,
        status: "started",
        targetId: input.target.targetID,
        verificationId,
      },
      modelTraceContext,
    ),
    modelTraceContext,
  );
  const providerMessages = buildRuntimeProviderRequestMessages(this, {
    entries: [
      ...withoutTrailingPendingAssistantToolCallEntries(
        this.messageHistory.borrowReadOnlyRuntimeEntries(),
      ),
      {
        message: {
          role: "user" as const,
          content: formatGoalCompletionVerificationPrompt(input.target),
        },
      },
    ],
    applyCacheControl: true,
    model,
  }).messages;
// verifier directly consumes the complete history but bypasses the media strategy of normal requests; only complements capability
// Will still cause aggregate over-limit failure to enter fail-open. Unify execution capabilities and budget projections before requests.
  const messages = projectMessagesForModelMediaPolicy(
    providerMessages,
    model.properties.inputFormat,
  ).messages;
  const modelRequestEvent = this.createEvent(
    SessionEventType.ModelRequest,
    {
      messages,
      providerId: String(model.providerId),
      modelId: String(model.modelId),
      querySource: GOAL_COMPLETION_VERIFICATION_QUERY_SOURCE,
      toolCount: 0,
    },
    modelTraceContext,
  );
  await this.appendEvent(modelRequestEvent, modelTraceContext);
  input.events.push(modelRequestEvent);
  const modelStartedAt = Date.now();
  const networkEventStartIndex = input.events.length;

  try {
    const result = await generateTargetCompletionVerificationText.call(this, {
      abortSignal: input.abortSignal,
      events: input.events,
      messages,
      model,
      traceContext: modelTraceContext,
    });
    throwIfTurnAborted(input.abortSignal);
    const toolCalls = this.extractToolCallsFromResult(result);
    const modelCompleteEvent = this.createEvent(
      SessionEventType.ModelComplete,
      {
        content: result.text,
        querySource: GOAL_COMPLETION_VERIFICATION_QUERY_SOURCE,
        stopReason: result.finishReason,
        toolCallCount: toolCalls.length,
        usage: result.usage,
      },
      modelTraceContext,
    );
    await this.appendEvent(modelCompleteEvent, modelTraceContext);
    input.events.push(modelCompleteEvent);
    await recordModelUsageFact(this, {
      events: input.events,
      model,
      networkEventStartIndex,
      querySource: GOAL_COMPLETION_VERIFICATION_QUERY_SOURCE,
      result,
      startedAt: modelStartedAt,
      status: "completed",
      toolCallCount: toolCalls.length,
      traceContext: modelTraceContext,
    });
    const verification =
      toolCalls.length > 0
        ? failOpenGoalCompletionVerification(
            "The completion verifier attempted to call tools instead of returning a verification result.",
          )
        : parseGoalCompletionVerificationText(result.text);
    await this.appendEvent(
      this.createEvent(
        SessionEventType.TargetCompletionVerification,
        {
          ...anchor,
          ...(foregroundExecutionId ? { foregroundExecutionId } : {}),
          goalIteration,
          status: "completed",
          targetId: input.target.targetID,
          verification,
          verificationId,
        },
        modelTraceContext,
      ),
      modelTraceContext,
    );
    return verification;
  } catch (error) {
    await recordModelUsageFact(this, {
      error,
      events: input.events,
      model,
      networkEventStartIndex,
      querySource: GOAL_COMPLETION_VERIFICATION_QUERY_SOURCE,
      startedAt: modelStartedAt,
      status: input.abortSignal?.aborted ? "cancelled" : "error",
      traceContext: modelTraceContext,
    });
    if (input.abortSignal?.aborted) {
      const preserveQueueAutoDrainOnCancel =
        this.activeForegroundExecution?.preserveQueueAutoDrainOnCancel === true;
      await this.appendEvent(
        this.createEvent(
          SessionEventType.TargetCompletionVerification,
          {
            ...anchor,
            ...(foregroundExecutionId ? { foregroundExecutionId } : {}),
            goalIteration,
            status: "cancelled",
            targetId: input.target.targetID,
            ...(preserveQueueAutoDrainOnCancel ? { preserveQueueAutoDrainOnCancel: true } : {}),
            verification: failedGoalCompletionVerification(
              "Completion verifier request was cancelled.",
            ),
            verificationId,
          },
          modelTraceContext,
        ),
        modelTraceContext,
      );
// When the user Stop or the queue "Send Immediately" interrupts the goal verifier, there is currently no ordinary
// The cancel closing path of executeTurn will pause the target. If it remains active, follow-up
// resumeSession + sendPrompt will be regarded as goal continuation by the agent, and ordinary user messages will continue to output checkpoints.
      await this.pauseActiveTargetForCancellation(modelTraceContext);
      throw error;
    }
    this.logger?.warn("Goal completion verification failed open", {
      ...traceContextToLogContext(modelTraceContext),
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "target.completion_verification.failed_open",
      module: "core.runtime",
      status: "failed",
      targetId: input.target.targetID,
    });
    const verification = failOpenGoalCompletionVerification(
      error instanceof Error
        ? `Completion verifier request failed: ${error.message}`
        : "The completion verifier could not confirm that every goal requirement is complete.",
    );
    await this.appendEvent(
      this.createEvent(
        SessionEventType.TargetCompletionVerification,
        {
          ...anchor,
          ...(foregroundExecutionId ? { foregroundExecutionId } : {}),
          goalIteration,
          status: "failed_closed",
          targetId: input.target.targetID,
          verification,
          verificationId,
        },
        modelTraceContext,
      ),
      modelTraceContext,
    );
    return verification;
  }
}

function targetCompletionVerificationAnchor(
  this: AgentRuntimeInternal,
  traceContext: TraceContext,
): { anchorAssistantMessageId?: string; anchorTurnId?: string } {
  const anchorAssistantMessageId = this.latestAssistantMessageId;
  const anchorTurnId = this.latestAssistantTurnId ?? traceContext.turnId;
  return {
    ...(anchorAssistantMessageId ? { anchorAssistantMessageId } : {}),
    ...(anchorTurnId ? { anchorTurnId } : {}),
  };
}

async function generateTargetCompletionVerificationText(
  this: AgentRuntimeInternal,
  input: {
    abortSignal?: AbortSignal;
    events: SessionEvent[];
    messages: ReturnType<typeof buildRuntimeProviderRequestMessages>["messages"];
    model: Model;
    traceContext: TraceContext;
  },
) {
  const maxAttempts = TARGET_VERIFIER_START_PLAN_BUSY_RETRY_DELAYS_MS.length + 1;
  for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
    try {
      const invocationContext = {
        metadata: traceContextToLogContext(input.traceContext),
        modelRequestSessionType: resolveModelRequestSessionTypeFromTaskType(this.config.taskType),
        modelCall: {
          operation: "goal_completion_verification" as const,
        },
        statusSink: this.createModelStatusSink(input.traceContext, input.events),
        traceContext: input.traceContext,
        refreshRuntimeHeadersBeforeAttempt: createRefreshRuntimeHeadersBeforeModelAttempt(this, {
          abortSignal: input.abortSignal,
          model: input.model,
          traceContext: input.traceContext,
        }),
      };
      return await runWithModelInvocationContext(invocationContext, () =>
        input.model.generateText({
          abortSignal: input.abortSignal,
          messages: input.messages,
// Verifier inherits the bound thinking configuration and cannot apply the downshifting and capping strategies of low-cost auxiliary calls.
          options: { maxOutputTokens: input.model.optionSpecs.maxOutputTokens.max },
          tools: [],
        }),
      );
    } catch (error) {
      const retryDelayMs = TARGET_VERIFIER_START_PLAN_BUSY_RETRY_DELAYS_MS[attempt - 1];
      if (
        input.abortSignal?.aborted ||
        retryDelayMs === undefined ||
        !isTargetVerifierStartPlanBusyFailure(error, input.model.providerId)
      ) {
        throw error;
      }

// Goal completion verification occurs after the user has seen the assistant iteration; Start Plan busy
// Is admission instantaneous concurrency. Retry briefly first to avoid directly using fail-open and mistaking recoverable concurrency for completion.
      this.logger?.warn("Goal completion verification retrying after Start Plan busy", {
        ...traceContextToLogContext(input.traceContext),
        attempt,
        event: "target.completion_verification.retry_start_plan_busy",
        maxAttempts,
        module: "core.runtime",
        retryDelayMs,
        status: "waiting",
      });
      await new Promise((resolve) => setTimeout(resolve, retryDelayMs));
      throwIfTurnAborted(input.abortSignal);
    }
  }

  throw new Error("Goal completion verification retry loop exhausted unexpectedly.");
}

function isTargetVerifierStartPlanBusyFailure(error: unknown, providerId: string): boolean {
  return (
    START_PLAN_TARGET_VERIFIER_RETRY_PROVIDER_IDS.has(providerId) &&
    isStartPlanBusyStreamRecoveryFailure(error)
  );
}

async function getNextTargetCompletionVerificationIteration(
  this: AgentRuntimeInternal,
  targetId: string,
): Promise<number> {
  const projection = await this.rebuildProjection();
  const targetTimeline = projection.targetCompletionVerificationTimeline.filter(
    (item) => item.targetId === targetId,
  );
  const latestIteration = targetTimeline.reduce(
    (maxIteration, item, index) => Math.max(maxIteration, item.goalIteration ?? index + 1),
    0,
  );
// Goal iterations are advanced by the verifier lifecycle, not by normal turns or user continuations.
// The runtime fixes the same number on started/completed/failed_closed/cancelled, so that the UI and snapshot will not guess separately.
  return latestIteration + 1;
}

function withoutTrailingPendingAssistantToolCallEntries(
  entries: readonly RuntimeMessageEntry[],
): readonly RuntimeMessageEntry[] {
  const last = entries.at(-1);
  if (
    !last ||
    isRuntimeAttachmentEntry(last) ||
    last?.message.role !== "assistant" ||
    !last.message.toolCalls ||
    last.message.toolCalls.length === 0
  ) {
    return entries;
  }
  return entries.slice(0, -1);
}
