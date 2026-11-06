import {
  CoreErrorType,
  createCoreError,
  createRootTraceContext,
  traceContextToLogContext,
} from "@zcode/contracts";
import type { ToolSchedule } from "../scheduler.js";
import type { ExecutableToolCall, ToolBatchEvent, ToolExecutionResult } from "../types.js";
import { createErrorResult } from "./errors.js";
import { emitToolCallError } from "./events.js";
// import { emitSkippedToolError } from "./events.js";
import type { ToolBatchExecuteOptions, ToolExecuteOptions, ToolExecutorDeps } from "./types.js";

// const TOOL_SKIPPED_AFTER_BLOCKING_FAILURE =
//   "Tool skipped because a previous tool call in the scheduled sequence failed.";
const TOOL_CANCELLED_AFTER_TURN_STOP =
  "Tool cancelled because a previous tool result requested a turn stop.";

type ExecuteToolCall = (
  toolCall: ExecutableToolCall,
  options?: ToolExecuteOptions,
) => Promise<ToolExecutionResult>;

export async function executeToolBatch(
  deps: ToolExecutorDeps,
  executeOne: ExecuteToolCall,
  toolCalls: ExecutableToolCall[],
  options?: ToolBatchExecuteOptions,
): Promise<ToolExecutionResult[]> {
  const maxConcurrency = options?.maxConcurrency ?? deps.maxConcurrency;

  if (toolCalls.length <= maxConcurrency) {
    return Promise.all(toolCalls.map((tc) => executeOne(tc, options)));
  }

  const results: ToolExecutionResult[] = [];
  for (let i = 0; i < toolCalls.length; i += maxConcurrency) {
    const batch = toolCalls.slice(i, i + maxConcurrency);
    const batchResults = await Promise.all(
      batch.map((tc) =>
        executeOne(tc, {
          automationTurn: options?.automationTurn,
          offPeakTurn: options?.offPeakTurn,
          signal: options?.signal,
          traceContext: options?.traceContext,
          subagentModelOverride: options?.subagentModelOverride,
          model: options?.model,
        }),
      ),
    );
    results.push(...batchResults);
  }
  return results;
}

export async function* executeToolSchedule(
  deps: ToolExecutorDeps,
  executeBatch: (
    toolCalls: ExecutableToolCall[],
    options?: ToolBatchExecuteOptions,
  ) => Promise<ToolExecutionResult[]>,
  toolCalls: ExecutableToolCall[],
  schedule: ToolSchedule,
  options?: ToolBatchExecuteOptions,
): AsyncGenerator<ToolBatchEvent, ToolExecutionResult[], void> {
  const toolMap = new Map(toolCalls.map((tc) => [tc.id, tc]));
  const allResults: ToolExecutionResult[] = [];
  const maxConcurrency = options?.maxConcurrency ?? deps.maxConcurrency;

  for (let groupIndex = 0; groupIndex < schedule.parallelGroups.length; groupIndex++) {
    const group = schedule.parallelGroups[groupIndex];
    const groupTools = group
      .map((id) => toolMap.get(id))
      .filter((tc): tc is ExecutableToolCall => tc !== undefined);

    if (groupTools.length === 0) continue;

    yield { type: "batch_start", parallelGroupIndex: groupIndex, toolCallIds: group };

    const groupResults = await executeBatch(groupTools, {
      automationTurn: options?.automationTurn,
      signal: options?.signal,
      traceContext: options?.traceContext,
      subagentModelOverride: options?.subagentModelOverride,
      model: options?.model,
      maxConcurrency,
    });
    allResults.push(...groupResults);

    yield { type: "batch_complete", parallelGroupIndex: groupIndex, results: groupResults };

    const shouldStopTurn = groupResults.some(
      (result) => result.turnControl?.stopTurnAfterResult === true,
    );
    if (shouldStopTurn) {
      const skippedTools = schedule.parallelGroups
        .slice(groupIndex + 1)
        .flat()
        .map((id) => toolMap.get(id))
        .filter((toolCall): toolCall is ExecutableToolCall => toolCall !== undefined);
      const eventTraceContext =
        options?.traceContext ??
        deps.traceContext ??
        createRootTraceContext({ sessionId: deps.sessionId, turnId: deps.turnId });
      const turnId = eventTraceContext.turnId ?? deps.turnId;

      for (const toolCall of skippedTools) {
        const result = createErrorResult(
          toolCall,
          createCoreError(CoreErrorType.ToolCancelled, TOOL_CANCELLED_AFTER_TURN_STOP, {
            context: { toolCallId: toolCall.id, toolName: toolCall.name },
            recoverable: true,
          }),
        );
        allResults.push(result);
        await emitToolCallError(deps, result.toolCallId, eventTraceContext, turnId, result.error);
        deps.logger?.warn("Tool call cancelled after turn stop", {
          ...traceContextToLogContext(eventTraceContext),
          event: "tool.call.cancelled_after_turn_stop",
          module: "core.tool.executor",
          status: "cancelled",
          toolCallId: result.toolCallId,
          toolName: result.toolName,
        });
      }

      break;
    }

// The old policy will skip all subsequent scheduling groups after the pre-order non-concurrentSafe tool fails.
// In the continuous subagent scenario, local failure such as TodoWrite will mistakenly truncate the subsequent Agent; now continue to press
// parallelGroups are executed sequentially, and subsequent tools determine the result by their own permissions, cancellation signals, and handlers.
    // const hasBlockingFailure = groupResults.some(
    //   (result) => !result.success && !deps.registry.get(result.toolName)?.metadata.concurrentSafe,
    // );
    // if (!hasBlockingFailure) continue;
    //
    // const skippedTools = schedule.parallelGroups
    //   .slice(groupIndex + 1)
    //   .flat()
    //   .map((id) => toolMap.get(id))
    //   .filter((toolCall): toolCall is ExecutableToolCall => toolCall !== undefined);
    //
    // for (const toolCall of skippedTools) {
    //   const result = createErrorResult(
    //     toolCall,
    //     createCoreError(CoreErrorType.ToolExecutionFailed, TOOL_SKIPPED_AFTER_BLOCKING_FAILURE, {
    //       context: { toolCallId: toolCall.id, toolName: toolCall.name },
    //       recoverable: true,
    //     }),
    //   );
    //   allResults.push(result);
    //   await emitSkippedToolError(deps, result, options?.traceContext);
    // }
    //
    // break;
  }

  return allResults;
}
