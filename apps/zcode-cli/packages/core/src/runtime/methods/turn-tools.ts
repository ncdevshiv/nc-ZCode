/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { createPartId, traceContextToLogContext, TurnMachineImpl } from "../deps.js";
import type {
  MessageId,
  ModelToolCall,
  ToolCallId,
  TraceContext,
  ToolCall,
  ToolExecutionResult,
} from "../deps.js";
import {
  createStreamRecoveryAnchorId,
  emitStreamRecoveryAnchor,
  emitStreamingToolLedgerUpdate,
  isErrorForToolResult,
  isTurnCancellationError,
  modelContentForToolResult,
  stringifyToolResultOutput,
  toRecordInput,
} from "../helpers/index.js";
import { persistToolResultMediaAttachments } from "../helpers/tool-result-media-persistence.js";
import type { RuntimeModelTextResult, StreamedToolExecutionResult } from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { emitSyntheticStreamedToolError } from "./streaming-tool-synthetic-result.js";
import {
  persistPendingToolPart,
  projectToolNameForNonEmptyBoundary,
} from "./tool-part-persistence.js";
import { persistToolModelStepFinish } from "./turn-step-finish.js";
import { completedToolPartMetadata, mcpToolPartMetadata } from "./tool-part-metadata.js";
import { drainInlineGuideForNextRequest } from "./turn-guide-drain.js";
import { handleToolCallAnomalyWarnings } from "./turn-tool-warnings.js";
import { emitNestedModelUsageEvents } from "./turn-nested-model-usage.js";
import type { RegularTurnLoopState } from "./turn-loop-state.js";
import {
  isAutomationMutationRestrictedTurn,
  isOffPeakCreateRestrictedTurn,
  recordCompletedToolBatch,
} from "./turn-loop-state.js";
import { recordToolUsageFromResult } from "./turn-tool-usage.js";
import { recordBrowserTurnToolResult } from "../../repl/browser-turn-state.js";
import { createRuntimeToolResultEntry } from "../../agent/message-history.js";
import { commitTurnRequestEntries } from "./turn-output-token-continuation.js";
export async function executeToolCallsForModelStep(
  this: AgentRuntimeInternal,
  state: RegularTurnLoopState,
  options: {
    assistantCreatedAt: number;
    assistantMessageId: MessageId;
    modelTraceContext: TraceContext;
    result: RuntimeModelTextResult;
    streamedToolResults?: StreamedToolExecutionResult[];
    toolCalls: ModelToolCall[];
  },
): Promise<"continue" | "break"> {
  const model = state.model;
  if (!model) {
    throw new Error("Model-backed tool execution requires the loop Model");
  }
  const modelSelection = { providerId: model.providerId, modelId: model.modelId };
  const coreToolCalls: ToolCall[] = options.toolCalls.map((tc) => ({
    id: tc.id as ToolCallId,
// Model step admission has completed type/ID verification; the original blank name that can be restored is retained here.
// Let the executor force a registry miss instead of treating the storage placeholder value as a real tool.
    name: tc.name,
    input: tc.input,
  }));
  const streamedResultsById = new Map(
    (options.streamedToolResults ?? []).map((entry) => [entry.toolCallId, entry]),
  );
  const toolCallById = new Map(coreToolCalls.map((toolCall) => [toolCall.id, toolCall]));
  const toolParts = new Map<
    string,
    {
      partID: ReturnType<typeof createPartId>;
      declarationIndex: number;
      input: Record<string, unknown>;
      startedAt: number;
    }
  >();
  for (const [declarationIndex, toolCall] of coreToolCalls.entries()) {
    const streamed = streamedResultsById.get(toolCall.id as ToolCallId);
    if (streamed) {
      toolParts.set(toolCall.id, {
        declarationIndex,
        partID: streamed.partID,
        input: streamed.input,
        startedAt: streamed.result.startedAt.getTime(),
      });
      if (streamed.ledgerRecorded === false) {
        await persistPendingToolPart(this, {
          assistantMessageId: options.assistantMessageId,
          declarationIndex,
          input: streamed.input,
          partID: streamed.partID,
          toolCall,
          traceContext: options.modelTraceContext,
          model,
          metadata: mcpToolPartMetadata(this.registry.getMetadata(toolCall.name)?.mcpPresentation),
        });
        await emitStreamingToolLedgerUpdate(this, state.events, options.modelTraceContext, {
          assistantMessageId: options.assistantMessageId,
          toolCall,
          status: "tool_call_closed",
          executionTiming: "during_stream",
          input: streamed.input,
        });
        await emitSyntheticStreamedToolError(
          this,
          state.events,
          options.modelTraceContext,
          streamed.result,
        );
      }
      continue;
    }
    const partID = createPartId();
    const normalizedInput = toRecordInput(toolCall.input);
    toolParts.set(toolCall.id, {
      declarationIndex,
      partID,
      input: normalizedInput,
      startedAt: Date.now(),
    });
    await persistPendingToolPart(this, {
      assistantMessageId: options.assistantMessageId,
      declarationIndex,
      input: normalizedInput,
      partID,
      toolCall,
      traceContext: options.modelTraceContext,
      model,
      metadata: mcpToolPartMetadata(this.registry.getMetadata(toolCall.name)?.mcpPresentation),
    });
    await emitStreamingToolLedgerUpdate(this, state.events, options.modelTraceContext, {
      assistantMessageId: options.assistantMessageId,
      toolCall,
      status: "tool_call_closed",
      input: normalizedInput,
    });
  }

// After assistant tool_use has entered history, Stop cannot be in tool result
// Throw directly before creation. Continue to hand over the aborted signal to the executor, and the existing cancellation path is
// Each tool call generates ToolCancelled result, and then the turn loop senses abort.
  const schedule = await this.scheduleTools(coreToolCalls);
  state.turnMachine = new TurnMachineImpl(
    state.turnMachine.scheduleTools(coreToolCalls, this.toScheduleState(schedule)),
  );
  const pendingToolCalls = coreToolCalls.filter(
    (toolCall) => !streamedResultsById.has(toolCall.id as ToolCallId),
  );
  let pendingExecutionResults: ToolExecutionResult[] = [];
  state.turnMachine = new TurnMachineImpl(state.turnMachine.startToolExecution());
  if (pendingToolCalls.length > 0) {
    const pendingSchedule =
      pendingToolCalls.length === coreToolCalls.length
        ? schedule
        : await this.scheduleTools(pendingToolCalls);
    const scheduledEvents = await this.emitToolScheduledEvents(
      pendingToolCalls,
      pendingSchedule,
      options.assistantMessageId,
      options.modelTraceContext,
    );
    state.events.push(...scheduledEvents);
    for (const toolCall of pendingToolCalls) {
      await emitStreamingToolLedgerUpdate(this, state.events, options.modelTraceContext, {
        assistantMessageId: options.assistantMessageId,
        toolCall,
        status: "tool_queued",
        input: toolParts.get(toolCall.id)?.input,
      });
    }

    this.logger?.debug("Executing tools", {
      streamedToolCallCount: streamedResultsById.size,
      toolCallCount: pendingToolCalls.length,
      tools: pendingToolCalls.map((tc) => tc.name),
    });
    const execution = await this.executeTools(pendingToolCalls, pendingSchedule, {
      automationTurn: isAutomationMutationRestrictedTurn(state),
      offPeakTurn: isOffPeakCreateRestrictedTurn(state),
      signal: state.turnAbortSignal,
      traceContext: options.modelTraceContext,
      subagentModelOverride: state.subagentModelOverride,
      model: state.model,
      onBatchStart: async (toolCallIds) => {
// The canceled batch is still returned by the executor canceled results, but it cannot be never entered
// The handler's tool parts are incorrectly marked as running.
        if (state.turnAbortSignal?.aborted) return;
        for (const toolCallId of toolCallIds) {
          const toolCall = toolCallById.get(toolCallId as ToolCallId);
          if (!toolCall) continue;
          const persisted = toolParts.get(toolCall.id);
          if (!persisted) continue;
          persisted.startedAt = Date.now();
          const projectedToolName = projectToolNameForNonEmptyBoundary(toolCall.name);
          await this.persistPart(
            {
              id: persisted.partID,
              sessionID: this.sessionId,
              messageID: options.assistantMessageId,
              type: "tool",
              callID: toolCall.id,
              declarationIndex: persisted.declarationIndex,
              tool: projectedToolName.toolName,
              metadata: projectedToolName.metadata,
              state: {
                status: "running",
                input: persisted.input,
                title: projectedToolName.toolName,
                metadata:
                  mcpToolPartMetadata(this.registry.getMetadata(toolCall.name)?.mcpPresentation) ??
                  {},
                time: {
                  start: persisted.startedAt,
                },
              },
            },
            options.modelTraceContext,
          );
          await emitStreamingToolLedgerUpdate(this, state.events, options.modelTraceContext, {
            assistantMessageId: options.assistantMessageId,
            toolCall,
            status: "tool_started",
            input: persisted.input,
            startedAt: new Date(persisted.startedAt),
          });
        }
      },
    });
    pendingExecutionResults = execution.results;
    state.events.push(...execution.events);
  }
  const resultById = new Map<string, ToolExecutionResult>();
  for (const streamed of streamedResultsById.values()) {
    resultById.set(streamed.result.toolCallId, streamed.result);
  }
  for (const pending of pendingExecutionResults) {
    resultById.set(pending.toolCallId, pending);
  }
  const results = coreToolCalls
    .map((toolCall) => resultById.get(toolCall.id))
    .filter((result): result is ToolExecutionResult => result !== undefined);
  for (const result of results) {
    recordBrowserTurnToolResult({
      output: result.output,
      sessionId: this.sessionId,
      toolName: result.toolName,
      turnId: state.turnId,
    });
  }
  await emitNestedModelUsageEvents(this, {
    events: state.events,
    results,
    traceContext: options.modelTraceContext,
  });
  for (const toolResult of results) {
    const resultContent = toolResult.success
      ? modelContentForToolResult(toolResult)
      : (toolResult.error?.message ?? stringifyToolResultOutput(toolResult));
    state.turnMachine = new TurnMachineImpl(
      state.turnMachine.completeTool(toolResult.toolCallId as ToolCallId, {
        success: toolResult.success,
        content: resultContent,
      }),
    );
  }
  state.turnMachine = new TurnMachineImpl(state.turnMachine.aggregateResults());
  this.logger?.debug("Tools executed", {
    resultCount: results.length,
    results: results.map((r) => ({
      toolName: r.toolName,
      success: r.success,
      output: typeof r.output === "string" ? r.output.substring(0, 50) : "[object]",
    })),
  });

  this.logger?.debug("Injecting tool results", { resultCount: results.length });
  let deferredCheckpointCancellation: unknown;
  for (const result of results) {
    await recordToolUsageFromResult(this, result, options.modelTraceContext);
    const content = stringifyToolResultOutput(result);
    const isError = isErrorForToolResult(result);
    const projectedResultToolName = projectToolNameForNonEmptyBoundary(result.toolName);
    const persisted = toolParts.get(result.toolCallId);
    if (persisted) {
      const mediaPersistence = result.success
        ? await persistToolResultMediaAttachments({
            artifactStore: this.artifactStore,
            assistantMessageId: options.assistantMessageId,
            content: modelContentForToolResult(result),
            sessionId: this.sessionId,
            sessionStore: this.sessionStore,
            signal: state.turnAbortSignal,
            toolCallId: result.toolCallId,
            toolName: result.toolName,
            traceContext: options.modelTraceContext,
            turnId: state.turnId,
          })
        : undefined;
      await this.persistPart(
        {
          id: persisted.partID,
          sessionID: this.sessionId,
          messageID: options.assistantMessageId,
          type: "tool",
          callID: result.toolCallId,
          declarationIndex: persisted.declarationIndex,
          tool: projectedResultToolName.toolName,
          metadata: projectedResultToolName.metadata,
          state: result.success
            ? {
                status: "completed",
                input: persisted.input,
                output: content,
                title: projectedResultToolName.toolName,
                metadata: {
                  ...completedToolPartMetadata(result),
                  ...(mediaPersistence
                    ? { modelContentLayout: mediaPersistence.modelContentLayout }
                    : {}),
                },
                time: {
                  start: result.startedAt.getTime(),
                  end: result.completedAt.getTime(),
                },
                ...(mediaPersistence ? { attachments: mediaPersistence.attachments } : {}),
              }
            : {
                status: "error",
                input: persisted.input,
                error: result.error?.message ?? content,
// state.error is UI/log oriented and may be larger than what the model actually receives
// modelContent is more general; only additionally saves the string content for accurate replay of cold recovery.
                metadata: {
                  ...mcpToolPartMetadata(
                    this.registry.getMetadata(result.toolName)?.mcpPresentation,
                  ),
                  ...(typeof result.modelContent === "string"
                    ? { modelContent: result.modelContent }
                    : {}),
                },
                time: {
                  start: result.startedAt.getTime(),
                  end: result.completedAt.getTime(),
                },
              },
        },
        options.modelTraceContext,
      );
    }
    this.logger?.debug("addToolResult", {
      toolCallId: result.toolCallId,
      toolName: result.toolName,
      success: result.success,
      contentLength: content.length,
    });
    commitTurnRequestEntries(this, state.turnRequestState, [
      createRuntimeToolResultEntry(
        result.toolCallId,
        result.toolName,
        modelContentForToolResult(result),
        isError,
      ),
    ]);
    try {
// checkpoint is an additional operation after tool result is closed. Stop if here
// To trigger, all sibling tool results must be submitted first, and reminder flush cannot be entered in advance.
      await this.emitFileMutationCheckpoint({
        abortSignal: state.turnAbortSignal,
        events: state.events,
        messageId: state.userMessageId,
        result,
        toolMessageId: options.assistantMessageId,
        traceContext: options.modelTraceContext,
      });
    } catch (error) {
      if (!isTurnCancellationError(error, state.turnAbortSignal)) throw error;
      deferredCheckpointCancellation ??= error;
      continue;
    }
    const toolCall = toolCallById.get(result.toolCallId as ToolCallId);
    if (toolCall) {
      const resultPartId = persisted?.partID;
      const recoveryAnchorId = createStreamRecoveryAnchorId(
        options.assistantMessageId,
        result.toolCallId as ToolCallId,
      );
      await emitStreamRecoveryAnchor(this, state.events, options.modelTraceContext, {
        assistantMessageId: options.assistantMessageId,
        toolCallId: result.toolCallId as ToolCallId,
        toolName: projectedResultToolName.toolName,
        success: result.success,
        resultPartId,
        committedAt: result.completedAt,
      });
      await emitStreamingToolLedgerUpdate(this, state.events, options.modelTraceContext, {
        assistantMessageId: options.assistantMessageId,
        toolCall,
        status: "tool_result_committed",
        executionTiming: streamedResultsById.has(result.toolCallId as ToolCallId)
          ? "during_stream"
          : "end_of_stream",
        input: persisted?.input,
        startedAt: result.startedAt,
        committedAt: result.completedAt,
        resultPartId,
        recoveryAnchorId,
      });
    }
    await enqueueFollowUpUserInputFromToolResult.call(
      this,
      state,
      result,
      options.modelTraceContext,
    );
  }

  if (deferredCheckpointCancellation) {
    throw deferredCheckpointCancellation;
  }

  const stopTurnResult = results.find((result) => result.turnControl?.stopTurnAfterResult === true);
  if (stopTurnResult) {
    await persistToolModelStepFinish(this, state, options);
    if (stopTurnResult.turnControl?.reason === "automation_create_limit") {
// Upper limit error can only be released manually by the user. Continue to hand over ordinary errors to the model,
// Causes the model to loop List/Delete/Create, even try a Bash bypass. Currently, turn only retains one text closing.
      state.automationCreateLimitReached = true;
      recordCompletedToolBatch(state);
      this.logger?.info("Automation create limit switched turn to text-only response", {
        event: "automation.create_limit.text_only_continuation",
        module: "core.runtime",
        reason: stopTurnResult.turnControl.reason,
        status: "completed",
        toolCallId: stopTurnResult.toolCallId,
        toolName: stopTurnResult.toolName,
      });
      return "continue";
    }
    if (state.activeTurn) {
      await this.fallbackPendingGuidesToQueue({
        activeTurn: state.activeTurn,
        events: state.events,
        reasonCode: "guide.noToolBoundary",
        traceContext: state.turnTraceContext,
      });
    }
    this.logger?.info("Tool result requested turn stop", {
      event: "tool.turn_control.stop",
      module: "core.runtime",
      reason: stopTurnResult.turnControl?.reason,
      status: "completed",
      toolCallId: stopTurnResult.toolCallId,
      toolName: stopTurnResult.toolName,
    });
    if (state.activeTurn) state.activeTurn.steerable = false;
    state.turnMachine = new TurnMachineImpl(
      state.turnMachine.complete(state.modelResponse, "success"),
    );
    return "break";
  }

  await handleToolCallAnomalyWarnings(this, state, {
    modelTraceContext: options.modelTraceContext,
    toolCalls: options.toolCalls,
  });
  await persistToolModelStepFinish(this, state, options);
  await drainInlineGuideForNextRequest(this, state);
  recordCompletedToolBatch(state);
  this.logger?.debug("After inject, message count", {
    compactToolTurnsSinceLastCompact: state.compactTracking?.toolTurnsSinceCompact,
    count: this.messageHistory.getMessageCount(),
    reactiveCompactAttemptedInCurrentModelStep: state.reactiveCompactAttemptedInCurrentModelStep,
  });
  return "continue";
}

async function enqueueFollowUpUserInputFromToolResult(
  this: AgentRuntimeInternal,
  state: RegularTurnLoopState,
  result: ToolExecutionResult,
  traceContext: TraceContext,
): Promise<void> {
  const followUp = result.followUpUserInput;
  if (!followUp) return;

  const input = followUp.input.trim();
  if (!input) return;

  const steerResult = await this.steerTurn({
    delivery: "guide",
    expectedTurnId: state.activeTurn?.turnId,
    input,
    source: followUp.reasonSource,
    traceContext,
  });

  if (steerResult.kind === "queued") {
    this.logger?.debug("Queued follow-up user input from tool result", {
      ...traceContextToLogContext(traceContext),
      event: "tool.follow_up_user_input.queued",
      module: "core.runtime",
      pendingInputId: steerResult.pendingInputId,
      reasonSource: followUp.reasonSource,
      status: "waiting",
      toolCallId: result.toolCallId,
      toolName: result.toolName,
    });
    return;
  }

// ExitPlanMode approval feedback must be upgraded to a real user message;
// If it is rejected here, it means that the active turn status is abnormal or the input exceeds the steer limit and cannot be swallowed silently.
  this.logger?.warn("Failed to queue follow-up user input from tool result", {
    ...traceContextToLogContext(traceContext),
    activeTurnId: steerResult.activeTurnId,
    event: "tool.follow_up_user_input.rejected",
    module: "core.runtime",
    reason: steerResult.reason,
    reasonSource: followUp.reasonSource,
    status: "failed",
    toolCallId: result.toolCallId,
    toolName: result.toolName,
  });
}
