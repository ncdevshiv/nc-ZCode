/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { beginLocalTurnPreparation } from "@zcode/contracts";
import {
  CompactTrigger,
  CoreErrorType,
  SessionEventType,
  createChildTraceContext,
  createCoreError,
  createMessageId,
  createPartId,
  getModelUsageTotalTokens,
  traceContextToLogContext,
  TurnMachineImpl,
} from "../deps.js";
import type { MessageId, ModelNetworkStatusEvent, ModelToolContract } from "../deps.js";
import {
  createRuntimeAssistantEntry,
  type RuntimeMessageEntry,
} from "../../agent/message-history.js";
import {
  createModelContextExceededFinishError,
  createCompactRapidRefillError,
  objectKeys,
  projectExecutionErrorPayload,
  finalizeSuspiciousEmptyModelResult,
  isContextExceededFinishReason,
  isSuspiciousEmptyModelResult,
  readRawFinishReason,
  throwIfTurnAborted,
  isModelContextExceededError,
  isTurnCancellationError,
  buildTurnFileChangeSummary,
} from "../helpers/index.js";
import type {
  DrainedPendingInputDiagnostics,
  RunModelTextRequestOptions,
  RuntimeModelStreamSnapshot,
  RuntimeModelTextResult,
} from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { executeToolCallsForModelStep } from "./turn-tools.js";
import {
  captureAssistantPersistenceAnchor,
  finishModelStepWithoutToolCalls,
  persistCompletedAssistantStep,
  persistOutputTokenLimitErrorCarrier,
} from "./turn-stop.js";
import { createStreamingToolCoordinator } from "./streaming-tool-coordinator.js";
import { persistCancelledStreamSnapshot } from "./cancelled-stream-persistence.js";
import {
  beginStartPlanBusyAdmissionRetryAttempt,
  createStartPlanBusyAutoRetryExhaustedError,
  emitStreamRecoveryRetryEvents,
  emitStreamRecoveryStarted,
  getStartPlanBusyAdmissionRetryDelayMs,
  isStartPlanBusyStreamRecoveryFailure,
} from "./streaming-recovery.js";
import type { RegularTurnLoopState } from "./turn-loop-state.js";
import {
  evaluateRapidRefill,
  MAX_CONSECUTIVE_RAPID_REFILLS,
  RAPID_REFILL_TOOL_TURN_THRESHOLD,
  recordCompactHistoryRound,
  recordCompactSuccess,
  recordModelHistoryRound,
} from "./turn-loop-state.js";
import {
  querySourceForTask,
  recordMainTurnCacheHitUsage,
  recordMainTurnModelUsage,
} from "./turn-model-step-usage.js";
import { estimateCurrentModelInputTokens } from "./compact.js";
import {
  resolveModelStepMaxOutputTokens,
  resolveNormalRequestMaxOutputTokens,
} from "./model-token-limits.js";
import {
  appendOutputTokenContinuation,
  classifyOutputTokenContinuation,
  commitAssistantToTurnRequest,
  commitTurnRequestEntries,
  completeOutputTokenRecovery,
  hasAssistantReasoningContent,
  OUTPUT_TOKEN_LIMIT_ERROR_MESSAGE,
} from "./turn-output-token-continuation.js";

type ModelStepResult = "continue" | "output_continuation" | "break";

export async function runModelBackedTurnStep(
  this: AgentRuntimeInternal,
  state: RegularTurnLoopState,
  options: {
    drainedSteerForNextRequest?: DrainedPendingInputDiagnostics;
    latestRealUserMessageIndex?: number;
    messages: RunModelTextRequestOptions["messages"];
    sourceEntries: readonly (RuntimeMessageEntry | undefined)[];
    recordedMessages: RunModelTextRequestOptions["messages"];
    requestEntries: readonly RuntimeMessageEntry[];
    tools: ModelToolContract[];
  },
): Promise<ModelStepResult> {
  const assistantMessageId = createMessageId();
  const stepTelemetry = this.agentTelemetry.step({
    stepId: assistantMessageId,
    stepIndex: state.modelStepCount,
  });
  return stepTelemetry.run(async () => {
    try {
      const result = await runModelBackedTurnStepImpl.call(
        this,
        state,
        options,
        assistantMessageId,
      );
      stepTelemetry.finishCompleted(
        result === "output_continuation"
          ? "model_completed"
          : result === "continue"
            ? "tool_requested"
            : "turn_completed",
      );
      return result;
    } catch (error) {
      if (isTurnCancellationError(error, state.turnAbortSignal)) {
        stepTelemetry.finishCancelled("abort_signal");
      } else {
        stepTelemetry.finishFailed("unhandled", "unknown", error);
      }
      throw error;
    }
  });
}

async function runModelBackedTurnStepImpl(
  this: AgentRuntimeInternal,
  state: RegularTurnLoopState,
  options: {
    drainedSteerForNextRequest?: DrainedPendingInputDiagnostics;
    latestRealUserMessageIndex?: number;
    messages: RunModelTextRequestOptions["messages"];
    sourceEntries: readonly (RuntimeMessageEntry | undefined)[];
    recordedMessages: RunModelTextRequestOptions["messages"];
    requestEntries: readonly RuntimeMessageEntry[];
    tools: ModelToolContract[];
  },
  assistantMessageId: MessageId,
): Promise<ModelStepResult> {
  const model = state.model;
  const modelStepIndex = state.modelStepCount;
  const modelStartedAt = Date.now();
  const assistantCreatedAt = modelStartedAt;
  const assistantPersistenceAnchor = captureAssistantPersistenceAnchor(this);
  const querySource = querySourceForTask(this.config.taskType);
  const executionModelSelection = { providerId: model.providerId, modelId: model.modelId };
// The request budget is explicitly determined by the Agent execution chain. Ordinary Turn chooses to fill the upper limit declared by the model.
// ModelFactory no longer disguises this request parameter as long-term ModelSelection/Active Model state.
  const executionMaxOutputTokens = model.optionSpecs.maxOutputTokens.max;
  const executionContextWindow = model.properties.contextWindow;
  const modelTraceContext = createChildTraceContext(state.turnTraceContext, {
    attributes: {
      providerId: String(model.providerId),
      modelId: String(model.modelId),
      iteration: state.toolCallCount === 0 ? 0 : Math.ceil(state.toolCallCount / 10),
      querySource,
    },
  });

  this.logModelRequestSteeringContext({
    activeTurn: state.activeTurn,
    drained: options.drainedSteerForNextRequest,
    messages: options.messages,
    modelStepCount: state.modelStepCount,
    traceContext: modelTraceContext,
  });
  const finishPersistence = beginLocalTurnPreparation(modelTraceContext, "persistence");
  await this.persistAssistantMessage(
    assistantMessageId,
    state.currentUserMessageId,
    assistantCreatedAt,
    undefined,
    modelTraceContext,
    model,
  );
  await this.persistPart(
    {
      id: createPartId(),
      sessionID: this.sessionId,
      messageID: assistantMessageId,
      type: "step-start",
    },
    modelTraceContext,
  );

  const modelRequestEvent = this.createEvent(
    SessionEventType.ModelRequest,
    {
// The automatic renewal prompt only belongs to this request and should not be written to the persistent ModelRequest track.
      messages: options.recordedMessages,
      providerId: String(model.providerId),
      modelId: String(model.modelId),
      querySource,
      toolCount: options.tools.length,
      iteration: state.toolCallCount === 0 ? 0 : Math.ceil(state.toolCallCount / 10),
    },
    modelTraceContext,
  );
  await this.appendEvent(modelRequestEvent, modelTraceContext);
  state.events.push(modelRequestEvent);
  finishPersistence();
  const streamingToolCoordinator = createStreamingToolCoordinator(this, state, {
    assistantMessageId,
    model,
    traceContext: modelTraceContext,
  });
  const networkEventStartIndex = state.events.length;
  let latestStreamSnapshot: RuntimeModelStreamSnapshot = { reasoning: [], text: "" };
  const streamRecoveryRequest = state.pendingStreamRecoveryRequest;
  state.pendingStreamRecoveryRequest = undefined;
  let latestModelRequestId: string | undefined;
  let latestFailedModelRequestId: string | undefined;
  const recordModelNetworkStatus = (event: ModelNetworkStatusEvent): void => {
    if (event.type === "model_request_started") {
      latestModelRequestId = event.requestId;
      return;
    }
    if (event.type === "model_stream_stalled" || event.type === "model_request_failed") {
      latestFailedModelRequestId = event.requestId;
    }
  };

  let result: RuntimeModelTextResult;
  try {
    const baselineMaxOutputTokens = resolveNormalRequestMaxOutputTokens({
      modelMaxOutputTokens: executionMaxOutputTokens,
    });
    result = await this.runModelTextRequest({
      abortSignal: state.turnAbortSignal,
      assistantMessageId,
      events: state.events,
      maxOutputTokens: resolveModelStepMaxOutputTokens({
        baselineMaxOutputTokens,
        contextWindow: executionContextWindow,
        estimatedCurrentUsage: estimateCurrentModelInputTokens(
          options.messages,
          options.sourceEntries,
        ),
        modelContextBudgetStrategy: this.config.modelContextBudgetStrategy,
      }),
      latestRealUserMessageIndex: options.latestRealUserMessageIndex,
      messages: options.messages,
      sourceEntries: options.sourceEntries,
      model,
      onStreamSnapshot: (snapshot) => {
        latestStreamSnapshot = snapshot;
      },
      onModelNetworkStatus: recordModelNetworkStatus,
      onStreamReasoningDelta: (text) => streamingToolCoordinator.recordReasoningDelta(text),
      onStreamTextDelta: (text) => streamingToolCoordinator.recordTextDelta(text),
      onStreamToolCall: (toolCall) => streamingToolCoordinator.accept(toolCall),
      streamRecovery: streamRecoveryRequest,
      tools: options.tools,
      traceContext: modelTraceContext,
    });
    throwIfTurnAborted(state.turnAbortSignal);
  } catch (error) {
    let finalError = error;
    await recordMainTurnModelUsage(this, state, {
      assistantMessageId,
      error: finalError,
      model,
      modelTraceContext,
      networkEventStartIndex,
      startedAt: modelStartedAt,
      status: state.turnAbortSignal.aborted ? "cancelled" : "error",
    });
    const failedRequestId = latestFailedModelRequestId ?? latestModelRequestId;
    const toolCallCountBeforeStreamRecovery = state.toolCallCount;
    if (
      await streamingToolCoordinator.recoverFromModelFailure(
        error,
        assistantCreatedAt,
        failedRequestId ? { failedRequestId } : undefined,
      )
    ) {
      if (state.toolCallCount > toolCallCountBeforeStreamRecovery) {
        completeOutputTokenRecovery(state.turnRequestState);
      }
      return "continue";
    }
    const admissionRetryDelayMs = getStartPlanBusyAdmissionRetryDelayMs({
      error: finalError,
      providerId: executionModelSelection.providerId,
      state,
      turnNumber: this.turnNumber,
    });
    if (!state.turnAbortSignal.aborted && admissionRetryDelayMs !== undefined) {
// In the second round and beyond, the Start Plan may be rejected by the admission concurrency limit before the first token;
// There is no text or tool anchor at this time, the old stream recovery will not start, you must close the empty assistant and try again.
      const recoveryAttempt = beginStartPlanBusyAdmissionRetryAttempt(state);
      this.logger?.warn("Main turn retrying after Start Plan admission busy", {
        ...traceContextToLogContext(modelTraceContext),
        event: "model.main_turn.retry_start_plan_admission_busy",
        module: "core.runtime",
        retryDelayMs: admissionRetryDelayMs,
        retryNumber: recoveryAttempt.retryNumber,
        maxRetries: recoveryAttempt.maxRetries,
        status: "waiting",
      });
      await emitStreamRecoveryStarted(
        this,
        state,
        {
          assistantMessageId,
          ...(failedRequestId ? { failedRequestId } : {}),
          traceContext: modelTraceContext,
        },
        finalError,
        recoveryAttempt,
      );
      await this.persistAssistantMessage(
        assistantMessageId,
        state.userMessageId,
        assistantCreatedAt,
        {
          completed: Date.now(),
          finish: "start_plan_admission_retry_discarded",
        },
        modelTraceContext,
        model,
      );
      state.modelResponse = "";
      state.modelStepCount += 1;
      recordModelHistoryRound(state);
      state.turnMachine = new TurnMachineImpl(state.turnMachine.receiveModelResponse(""));
      state.turnMachine = new TurnMachineImpl(state.turnMachine.aggregateResults());
      await emitStreamRecoveryRetryEvents(
        this,
        state,
        {
          assistantMessageId,
          ...(failedRequestId ? { failedRequestId } : {}),
          traceContext: modelTraceContext,
        },
        {
          ...recoveryAttempt,
          discardedReasoningBytes: 0,
          discardedTextBytes: 0,
          reason: "no_tool_committed",
          toolCallIds: [],
        },
      );
      await streamingToolCoordinator.abandon("model_failed");
      await new Promise((resolve) => setTimeout(resolve, admissionRetryDelayMs));
      throwIfTurnAborted(state.turnAbortSignal);
      return "continue";
    }
    if (
      state.streamRecoveryRetryCount > 0 &&
      !state.turnAbortSignal.aborted &&
      isStartPlanBusyStreamRecoveryFailure(finalError)
    ) {
//Start Plan will run the interrupted stream first and go through core stream recovery; after the recovery times are exhausted,
// Continuing to throw away the original provider copy will be indistinguishable from the first round of busy failure, and the UI will not be able to display "automatic retries reached the maximum number of times".
      finalError = createStartPlanBusyAutoRetryExhaustedError(finalError);
    }
    await streamingToolCoordinator.abandon(
      state.turnAbortSignal.aborted ? "cancelled" : "model_failed",
    );
    if (
      state.turnAbortSignal.aborted &&
      isTurnCancellationError(finalError, state.turnAbortSignal)
    ) {
      await persistCancelledStreamSnapshot(this, {
        assistantCreatedAt,
        assistantMessageId,
        snapshot: latestStreamSnapshot,
        traceContext: modelTraceContext,
      });
      const reasoning = latestStreamSnapshot.reasoning.filter(hasAssistantReasoningContent);
      if (latestStreamSnapshot.text.length > 0 || reasoning.length > 0) {
// The durable snapshot has been persisted when canceling, but the live history commit of the successful path
// and historyRoundCount will not be executed, causing the current process to be inconsistent with the provider history of cold resume.
        commitTurnRequestEntries(this, state.turnRequestState, [
          createRuntimeAssistantEntry(
            latestStreamSnapshot.text,
            undefined,
            reasoning,
            state.model
              ? { providerId: state.model.providerId, modelId: state.model.modelId }
              : undefined,
          ),
        ]);
        recordModelHistoryRound(state);
      }
    }
    const finalErrorRecord =
      finalError && typeof finalError === "object"
        ? (finalError as Record<string, unknown>)
        : undefined;
    const persistedErrorCode =
      typeof finalErrorRecord?.code === "string" ? finalErrorRecord.code : undefined;
    const persistedErrorProjection = projectExecutionErrorPayload(finalError);
    const persistedTurnResult = isTurnCancellationError(finalError, state.turnAbortSignal)
      ? "cancelled"
      : undefined;
    await this.persistAssistantMessage(
      assistantMessageId,
      state.userMessageId,
      assistantCreatedAt,
      {
        completed: Date.now(),
        error: {
          name: finalError instanceof Error ? finalError.name : "UnknownError",
          data: {
            message: finalError instanceof Error ? finalError.message : String(finalError),
            ...(persistedErrorCode ? { code: persistedErrorCode } : {}),
// live TurnError has structured attribution, but transcript was not persisted in the past and will be lost to runtime after cold recovery.
            ...(persistedErrorProjection.attribution
              ? { attribution: persistedErrorProjection.attribution }
              : {}),
// The model of user Stop only persists the general error name/message in the past.
// cold hydration cannot distinguish between normal cancellation and real provider failure, ultimately incorrectly generating a TurnError.
            ...(persistedTurnResult ? { turnResult: persistedTurnResult } : {}),
          },
        },
      },
      modelTraceContext,
      model,
    );
    if (
      isModelContextExceededError(finalError) &&
      (await recoverModelStepAfterContextExceeded.call(
        this,
        state,
        finalError,
        modelStepIndex,
        options.requestEntries,
      ))
    ) {
      return "continue";
    }
    throw finalError;
  }

  state.modelResponse = result.text;
  state.modelStepCount += 1;
  state.tokenCount += getModelUsageTotalTokens(result.usage);

  if (result.usage.cacheReadTokens && result.usage.cacheReadTokens > 0) {
    this.messageHistory.setCacheHit(result.usage.cacheReadTokens);
  }

  let toolCalls = this.extractToolCallsFromResult(result);
  const providerToolCallCount = toolCalls.length;
  const localTerminalResponse = state.automationCreateLimitReached === true;
  if (state.automationCreateLimitReached && toolCalls.length > 0) {
// Even if the provider still hallucinates tool calls after tools=[], it cannot re-enter the executor;
//The current user turn after the upper limit is hit is already the plain text termination boundary.
    this.logger?.warn("Ignored tool calls after automation create limit was reached", {
      event: "automation.create_limit.tool_calls_ignored",
      module: "core.runtime",
      status: "completed",
      toolCallCount: toolCalls.length,
    });
    toolCalls = [];
    state.modelResponse = buildAutomationCreateLimitFallback(state.input);
  } else if (state.automationCreateLimitReached && state.modelResponse.trim().length === 0) {
    state.modelResponse = buildAutomationCreateLimitFallback(state.input);
  }
  const usage = result.usage ?? {};
  const responseLength = state.modelResponse.length;
  const rawFinishReason = readRawFinishReason(result.providerMetadata);
// Automation create-limit has taken over the termination semantics of the current response; if cleared
//The length/context reason will still be reinterpreted after provider tool calls, and the plain text final state will continue to run 3 times.
  const outputTokenContinuation = localTerminalResponse
    ? "none"
    : classifyOutputTokenContinuation({
        continuationCount: state.turnRequestState.outputTokenContinuationCount,
        finishReason: result.finishReason,
        rawFinishReason,
        toolCallCount: providerToolCallCount,
      });
  this.logger?.info("Model response diagnostics", {
    ...traceContextToLogContext(modelTraceContext),
    event: "model.response.diagnostics",
    finishReason: result.finishReason,
    module: "core.runtime",
    providerMetadataKeys: objectKeys(result.providerMetadata),
    rawFinishReason,
    responseEmpty: responseLength === 0,
    responseLength,
    status: "completed",
    toolCallCount: toolCalls.length,
    usageCacheReadTokens: usage.cacheReadTokens,
    usageCacheWriteTokens: usage.cacheWriteTokens,
    usageInputTokens: usage.inputTokens,
    usageOutputTokens: usage.outputTokens,
    usageReasoningTokens: usage.reasoningTokens,
    usageTotalTokens: usage.totalTokens,
  });
  if (
    !localTerminalResponse &&
    outputTokenContinuation === "none" &&
    toolCalls.length === 0 &&
    isContextExceededFinishReason(result.finishReason, rawFinishReason)
  ) {
// Overflow provider may return empty content and zero usage; overflow must be identified first,
// Otherwise, it will be wrapped into a normal ModelError by suspicious empty, and subsequent reactive compact cannot be triggered.
    const contextError = createModelContextExceededFinishError({
      finishReason: result.finishReason,
      rawFinishReason,
    });
    if (
      await recoverModelStepAfterContextExceeded.call(
        this,
        state,
        contextError,
        modelStepIndex,
        options.requestEntries,
      )
    ) {
      return "continue";
    }
    throw contextError;
  }
  if (
    !localTerminalResponse &&
    outputTokenContinuation === "none" &&
    isSuspiciousEmptyModelResult(result.finishReason, responseLength, toolCalls.length, usage)
  ) {
    this.logger?.warn("Model returned an empty non-stop result", {
      ...traceContextToLogContext(modelTraceContext),
      event: "model.response.suspicious_empty",
      finishReason: result.finishReason,
      module: "core.runtime",
      rawFinishReason,
      responseLength,
      status: "completed",
      toolCallCount: toolCalls.length,
      usageTotalTokens: usage.totalTokens,
    });
    finalizeSuspiciousEmptyModelResult({
      finishReason: result.finishReason,
      model: executionModelSelection,
      providerMetadata: result.providerMetadata,
      rawFinishReason,
    });
  }
// AI SDK may normalize non-standard output-limit to other; after Runtime has confirmed the recovery semantics,
// Live events and persistence must use length uniformly, and at the same time, the original facts of the provider are retained by the diagnostics above.
  if (outputTokenContinuation !== "none") result.finishReason = "length";
  for (const reasoning of result.reasoning ?? []) {
    if (!hasAssistantReasoningContent(reasoning)) continue;
    await this.persistPart(
      {
        id: createPartId(),
        sessionID: this.sessionId,
        messageID: assistantMessageId,
        type: "reasoning",
        text: reasoning.text,
        metadata: reasoning.providerOptions,
        time: {
          start: modelStartedAt,
          end: Date.now(),
        },
      },
      modelTraceContext,
    );
  }
  if (state.modelResponse.length > 0) {
    await this.persistPart(
      {
        id: createPartId(),
        sessionID: this.sessionId,
        messageID: assistantMessageId,
        type: "text",
        text: state.modelResponse,
        time: {
          start: modelStartedAt,
          end: Date.now(),
        },
      },
      modelTraceContext,
    );
  }

  const cacheHit =
    querySource === "main_turn" ? recordMainTurnCacheHitUsage(this, result.usage) : undefined;
// The file checkpoint of subagent has been persisted, but the old gate only allows main_turn to
// Summary is written to ModelComplete, resulting in child details unable to restore summary and undo entries from authoritative events.
  const supportsTurnFileChanges = querySource === "main_turn" || querySource === "subagent";
  const fileChanges =
    supportsTurnFileChanges && toolCalls.length === 0
      ? await buildTurnFileChangeSummary(this.currentTurnFileChanges)
      : undefined;
  const modelCompleteEvent = this.createEvent(
    SessionEventType.ModelComplete,
    {
      content: state.modelResponse,
// Desktop continuous real-time events only carry the current model_complete payload.
// If the main round only sends usage but not contextWindow, the old task stream cannot generate usage_update.
// In long-term tasks, the input field will not be able to obtain the size of the context meter and will be hidden.
      ...(querySource === "main_turn" && executionContextWindow !== undefined
        ? { contextWindow: executionContextWindow }
        : {}),
      querySource,
      stopReason: result.finishReason,
      usage: result.usage,
      ...(cacheHit ? { cacheHit } : {}),
      ...(fileChanges ? { fileChanges } : {}),
      ...(querySource === "main_turn" && result.contextUsageBreakdown
        ? { contextUsageBreakdown: result.contextUsageBreakdown }
        : {}),
      toolCallCount: toolCalls.length,
    },
    modelTraceContext,
  );
  await this.appendEvent(modelCompleteEvent, modelTraceContext);
  state.events.push(modelCompleteEvent);
  this.lastAssistantCompletedAtMs = Date.now();
  await recordMainTurnModelUsage(this, state, {
    assistantMessageId,
    model,
    modelTraceContext,
    networkEventStartIndex,
    result,
    startedAt: modelStartedAt,
    status: "completed",
    toolCallCount: toolCalls.length,
  });
  state.turnMachine = new TurnMachineImpl(
    state.turnMachine.receiveModelResponse(state.modelResponse),
  );
  throwIfTurnAborted(state.turnAbortSignal);

  this.logger?.info("Model request completed", {
    ...traceContextToLogContext(modelTraceContext),
    durationMs: Date.now() - modelStartedAt,
    event: "model.request.completed",
    module: "core.runtime",
    status: "completed",
    totalTokens: state.tokenCount,
    toolCallCount: toolCalls.length,
  });

  const executableToolCalls = toolCalls.filter((toolCall) => !toolCall.providerExecuted);
  const streamedToolResults = await streamingToolCoordinator.drain(executableToolCalls);
  if (outputTokenContinuation !== "none") {
// When output-limit is hit for the first time, the current request may have a one-time project-memory attachment;
// The query-local state must be advanced from the actual request array and cannot be returned to the array before the request.
    state.turnRequestState.entries = options.requestEntries;
    const assistantCommitted = await persistCompletedAssistantStep(this, state, {
      assistantPersistenceAnchor,
      assistantCreatedAt,
      assistantMessageId,
      includeEmptyAssistant: false,
      modelTraceContext,
      result,
    });
    if (assistantCommitted) recordModelHistoryRound(state);
    if (outputTokenContinuation === "continue") {
      appendOutputTokenContinuation(state.turnRequestState);
      state.reactiveCompactAttemptedInCurrentModelStep = false;
      state.turnMachine = new TurnMachineImpl(state.turnMachine.aggregateResults());
      return "output_continuation";
    }

    const exhaustedError = createCoreError(
      CoreErrorType.ModelError,
      OUTPUT_TOKEN_LIMIT_ERROR_MESSAGE,
      {
        context: {
          providerCode: "model_output_limit_exceeded",
          reason: "model_output_limit_exceeded",
          source: "provider",
        },
        recoverable: true,
      },
    );
    const exhaustedErrorProjection = projectExecutionErrorPayload(
      exhaustedError,
      OUTPUT_TOKEN_LIMIT_ERROR_MESSAGE,
    );
    completeOutputTokenRecovery(state.turnRequestState);
    await persistOutputTokenLimitErrorCarrier(this, state, {
      error: {
        name: exhaustedErrorProjection.code ?? exhaustedError.type,
        data: {
          ...(exhaustedErrorProjection.code ? { code: exhaustedErrorProjection.code } : {}),
          message: exhaustedErrorProjection.message,
// Existing cold hydration uses retryable to restore UI recoverable; this field is reused here.
// Do not expand transcript/hydration schema for single errors.
          retryable: exhaustedError.recoverable,
          ...(exhaustedErrorProjection.attribution
            ? { attribution: exhaustedErrorProjection.attribution }
            : {}),
        },
      },
      finishReason: result.finishReason,
      model,
      modelTraceContext,
    });
    if (state.activeTurn) state.activeTurn.steerable = false;
// The upstream query loop will hand over the max_output_tokens API-error assistant to the outer layer; reuse here
// The existing ModelError -> TurnError closure expresses the same real-time error and only ends the current Turn command.
    throw exhaustedError;
  }
  completeOutputTokenRecovery(state.turnRequestState);
  if (executableToolCalls.length === 0) {
    return await finishModelStepWithoutToolCalls.call(this, state, {
      assistantPersistenceAnchor,
      assistantCreatedAt,
      assistantMessageId,
      modelTraceContext,
      result,
    });
  }

  state.toolCallCount += executableToolCalls.length;
// Merge fix: Tool calling assistant must enter canonical history and current round request history at the same time.
// Only writing canonical history will cause the tool results that follow to lose the corresponding assistant tool-call.
  if (commitAssistantToTurnRequest(this, state, result, executableToolCalls)) {
    recordModelHistoryRound(state);
  }
  const toolStepResult = await executeToolCallsForModelStep.call(this, state, {
    assistantCreatedAt,
    assistantMessageId,
    modelTraceContext,
    result,
    toolCalls: executableToolCalls,
    streamedToolResults,
  });
  return toolStepResult;
}

function buildAutomationCreateLimitFallback(input: string): string {
  if (/\p{Script=Han}/u.test(input)) {
    return "The number of scheduled tasks has reached the upper limit of 20 and has not been created this time. Please go to \"Automation\" to manually delete an existing task and try again.";
  }
  return "The limit of 20 scheduled tasks has been reached, so no task was created. Manually delete an existing task on the Automations page, then try again.";
}

async function recoverModelStepAfterContextExceeded(
  this: AgentRuntimeInternal,
  state: RegularTurnLoopState,
  contextError: unknown,
  modelStepIndex: number,
  activeEntries: readonly RuntimeMessageEntry[],
): Promise<boolean> {
  if (state.reactiveCompactAttemptedInCurrentModelStep) {
    return false;
  }

  const rapidRefill = evaluateRapidRefill(state.compactTracking);
  if (rapidRefill.shouldBlock) {
    this.logger?.warn("Reactive compact rapid-refill breaker tripped", {
      ...traceContextToLogContext(state.turnTraceContext),
      event: "compact.rapid_refill_breaker",
      consecutiveRapidRefills: rapidRefill.consecutiveRapidRefills,
      modelStepIndex,
      module: "core.runtime",
      status: "failed",
      toolTurnsSinceCompact: rapidRefill.toolTurnsSinceCompact,
      trigger: CompactTrigger.Reactive,
    });
    throw createCompactRapidRefillError({
      consecutiveRapidRefills: rapidRefill.consecutiveRapidRefills,
      maxConsecutiveRapidRefills: MAX_CONSECUTIVE_RAPID_REFILLS,
      toolTurnThreshold: RAPID_REFILL_TOOL_TURN_THRESHOLD,
      toolTurnsSinceCompact: rapidRefill.toolTurnsSinceCompact,
    });
  }

  state.reactiveCompactAttemptedInCurrentModelStep = true;
  const compactOutcome = await this.reactiveCompactAfterContextExceeded(
    contextError,
    state.turnTraceContext,
    state.events,
    state.turnAbortSignal,
    {
      activeEntries,
      modelStepIndex,
      rapidRefillCount: rapidRefill.consecutiveRapidRefills,
      model: state.model,
      turnRequestState: state.turnRequestState,
    },
  );
  if (compactOutcome !== "compacted") {
    return false;
  }

  recordCompactSuccess(state, rapidRefill);
  recordCompactHistoryRound(state);
  state.turnMachine = new TurnMachineImpl(
    TurnMachineImpl.create(
      this.sessionId,
      this.turnNumber,
      state.input,
      state.traceId,
      state.turnId,
    ).start(),
  );
  return true;
}
