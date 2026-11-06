import type { MessageId, MessageWithParts, Model, ModelUsage, TraceContext } from "../deps.js";
import type { MainTurnCacheHitAggregate, RuntimeModelTextResult } from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";
import type { RegularTurnLoopState } from "./turn-loop-state.js";
import type { RuntimeMessageEntry } from "../../agent/message-history.js";
import {
  persistedTokenUsageBaseline,
  type PersistedTokenUsageBaseline,
} from "../../agent/message-history-usage.js";
import { recordModelUsageFact } from "./usage-observability.js";

interface RecordMainTurnModelUsageInput {
  assistantMessageId: MessageId;
  error?: unknown;
  model: Model;
  modelTraceContext: TraceContext;
  networkEventStartIndex: number;
  result?: RuntimeModelTextResult;
  startedAt: number;
  status: "completed" | "error" | "cancelled";
  toolCallCount?: number;
}

export async function recordMainTurnModelUsage(
  runtime: AgentRuntimeInternal,
  state: RegularTurnLoopState,
  input: RecordMainTurnModelUsageInput,
): Promise<void> {
  await recordModelUsageFact(runtime, {
    assistantMessageId: input.assistantMessageId,
    error: input.error,
    events: state.events,
// Mold cutting during operation will immediately update the Session Selection; if usage is read again after await,
//The token of the old request will be assigned to the new model. Here only the immutable snapshot at the beginning of the model step is consumed.
    model: input.model,
    networkEventStartIndex: input.networkEventStartIndex,
    parentUserMessageId: state.currentUserMessageId,
    querySource: querySourceForTask(runtime.config.taskType),
    result: input.result,
    startedAt: input.startedAt,
    status: input.status,
    toolCallCount: input.toolCallCount,
    traceContext: input.modelTraceContext,
  });
}

export function querySourceForTask(taskType: AgentRuntimeInternal["config"]["taskType"]): string {
  if (taskType === "subagent_child") return "subagent";
  if (taskType === "workflow_child" || taskType === "nested_workflow_child")
    return "workflow_child";
  return "main_turn";
}

export function findLatestCommittedAssistantUsage(
  sourceEntries: readonly (RuntimeMessageEntry | undefined)[],
): { messageIndex: number; baseline: PersistedTokenUsageBaseline } | undefined {
  for (let messageIndex = sourceEntries.length - 1; messageIndex >= 0; messageIndex--) {
    const entry = sourceEntries[messageIndex];
    if (!entry || entry.kind === "attachment" || entry.message.role !== "assistant") continue;
    const baseline = persistedTokenUsageBaseline(entry.tokens);
    if (baseline) return { messageIndex, baseline };
  }
  return undefined;
}

export function mainTurnCacheHitAggregateFromMessages(input: {
  activeMessages: readonly MessageWithParts[];
  persistedMessages: readonly MessageWithParts[];
}): MainTurnCacheHitAggregate {
  const activeMessageIds = new Set(input.activeMessages.map((message) => message.info.id));

  return input.persistedMessages.reduce<MainTurnCacheHitAggregate>(
    (aggregate, message) => {
      if (
        !activeMessageIds.has(message.info.id) ||
        message.info.role !== "assistant" ||
        message.info.summary
      ) {
        return aggregate;
      }

// activeMessages is provider/context projection, Compact preserved usage
// Might have been cleared. Active IDs only determine branch membership, and the cache aggregate must read the persisted original tokens.
      const inputTokens = nonNegativeInteger(message.info.tokens.input) ?? 0;
      const cacheReadTokens = nonNegativeInteger(message.info.tokens.cache.read) ?? 0;
      const cacheWriteTokens = nonNegativeInteger(message.info.tokens.cache.write) ?? 0;
      if (inputTokens <= 0 && cacheReadTokens <= 0 && cacheWriteTokens <= 0) {
        return aggregate;
      }

      return {
        requestCount: aggregate.requestCount + 1,
        totalInputTokens: aggregate.totalInputTokens + inputTokens,
        totalCacheReadTokens: aggregate.totalCacheReadTokens + cacheReadTokens,
        totalCacheWriteTokens: aggregate.totalCacheWriteTokens + cacheWriteTokens,
      };
    },
    {
      requestCount: 0,
      totalInputTokens: 0,
      totalCacheReadTokens: 0,
      totalCacheWriteTokens: 0,
    },
  );
}

export function recordMainTurnCacheHitUsage(
  runtime: AgentRuntimeInternal,
  usage: ModelUsage | undefined,
):
  | {
      cacheReadTokens: number;
      cacheWriteTokens: number;
      hitRate: number | null;
      hitRateRequestCount: number;
      inputTokens: number;
      latestHitRate: number | null;
      totalCacheReadTokens: number;
      totalCacheWriteTokens: number;
      totalInputTokens: number;
    }
  | undefined {
  if (!usage) {
    return undefined;
  }
// AI SDK v6 has incorporated Anthropic cache read/write into inputTokens;
// The denominator of the cache hit rate should use total input, and the cache field cannot be repeatedly added to the denominator or context usage.
  const inputTokens = modelUsageInputWindowTokens(usage) ?? 0;
  const cacheReadTokens = nonNegativeInteger(usage.cacheReadTokens) ?? 0;
  const cacheWriteTokens = nonNegativeInteger(usage.cacheWriteTokens) ?? 0;
  if (inputTokens <= 0 && cacheReadTokens <= 0 && cacheWriteTokens <= 0) {
    return undefined;
  }

  runtime.mainTurnCacheHitAggregate = {
    requestCount: runtime.mainTurnCacheHitAggregate.requestCount + 1,
    totalInputTokens: runtime.mainTurnCacheHitAggregate.totalInputTokens + inputTokens,
    totalCacheReadTokens: runtime.mainTurnCacheHitAggregate.totalCacheReadTokens + cacheReadTokens,
    totalCacheWriteTokens:
      runtime.mainTurnCacheHitAggregate.totalCacheWriteTokens + cacheWriteTokens,
  };

  const aggregate = runtime.mainTurnCacheHitAggregate;
  return {
    inputTokens,
    cacheReadTokens,
    cacheWriteTokens,
    latestHitRate: inputTokens > 0 ? cacheReadTokens / inputTokens : null,
    hitRate:
      aggregate.totalInputTokens > 0
        ? aggregate.totalCacheReadTokens / aggregate.totalInputTokens
        : null,
    hitRateRequestCount: aggregate.requestCount,
    totalInputTokens: aggregate.totalInputTokens,
    totalCacheReadTokens: aggregate.totalCacheReadTokens,
    totalCacheWriteTokens: aggregate.totalCacheWriteTokens,
  };
}

function modelUsageInputWindowTokens(usage?: ModelUsage): number | undefined {
  if (!usage) return undefined;

// core test/runtime resolves @zcode/contracts through the package entry, and the new contracts helper is not available when it is not built.
// The same algorithm is retained here: cache read is incorporated into the input window of the current request according to Anthropic caliber.
  const inputTokens = positiveInteger(usage.inputTokens);
  if (inputTokens !== undefined) {
// Anthropic inputTokens of AI SDK v6 is already the total input of normal input + cache read/write.
// Use the provider's normalized input uniformly to avoid automatic compression and UI context meter repeated calculation of the cache.
    return inputTokens;
  }

  const totalTokens = positiveInteger(usage.totalTokens);
  if (totalTokens !== undefined) {
    const outputTokens = nonNegativeInteger(usage.outputTokens) ?? 0;
    return Math.max(0, totalTokens - outputTokens);
  }

  const cacheTokens =
    (nonNegativeInteger(usage.cacheReadTokens) ?? 0) +
    (nonNegativeInteger(usage.cacheWriteTokens) ?? 0);
  return cacheTokens > 0 ? cacheTokens : undefined;
}

function positiveInteger(value: number | undefined): number | undefined {
  if (value === undefined || !Number.isFinite(value)) return undefined;
  const integer = Math.floor(value);
  return integer > 0 ? integer : undefined;
}

function nonNegativeInteger(value: number | undefined): number | undefined {
  if (value === undefined || !Number.isFinite(value)) return undefined;
  const integer = Math.floor(value);
  return integer >= 0 ? integer : undefined;
}
