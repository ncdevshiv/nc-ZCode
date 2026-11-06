import type { ToolExecutionResult, TraceContext } from "../deps.js";
import type { UsageStorePort } from "@zcode/contracts";
import type { AgentRuntimeInternal } from "../internal.js";

export async function recordToolUsageFromResult(
  runtime: AgentRuntimeInternal,
  result: ToolExecutionResult,
  traceContext: TraceContext,
): Promise<void> {
  const usageStore = usageStoreFor(runtime);
  if (!usageStore) return;

  const metadata = runtime.registry.get(result.toolName)?.metadata;
  const outputBytes = outputSizeBytes(result);
  const errorType = result.error?.type;

  try {
    await usageStore.upsertToolUsage({
      id: toolUsageId(runtime.sessionId, result.toolCallId),
      sessionID: runtime.sessionId,
      turnID: traceContext.turnId,
      traceID: traceContext.traceId,
      toolCallID: result.toolCallId,
      toolName: result.toolName,
      sideEffectScope: metadata?.sideEffectScope,
      readOnly: metadata?.readOnly,
      destructive: metadata?.destructive,
      approvalStatus: "none",
      status: result.success ? "completed" : "error",
      startedAt: result.startedAt.getTime(),
      firstOutputAt: result.completedAt.getTime(),
      completedAt: result.completedAt.getTime(),
      durationMs: result.durationMs,
      exitCode:
        result.performance?.detail?.kind === "command"
          ? result.performance.detail.command.exitCode
          : undefined,
      timeToFirstOutputMs: result.completedAt.getTime() - result.startedAt.getTime(),
      outputBytes,
      truncated: result.serialization?.truncated,
      retryCount: 0,
      retryable: false,
      cancelledByUser: errorType?.includes("cancel") ?? false,
      errorType,
      errorMessage: result.error?.message,
    });
  } catch (error) {
// There is no bypass protection for SQLite Usage writing in the result path, and disk/lock exceptions will block subsequent
// Tool Result persistence and model continuation; failure to observe must not change Agent business semantics.
    runtime.logger?.warn("Usage tool fact write failed", {
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "usage.tool.write.failed",
      module: "core.runtime",
      status: "failed",
      toolCallId: result.toolCallId,
      traceId: traceContext.traceId,
      turnId: traceContext.turnId,
    });
  }
}

function usageStoreFor(runtime: AgentRuntimeInternal): UsageStorePort | undefined {
  const candidate = runtime.sessionStore as Partial<UsageStorePort> | undefined;
  return candidate?.recordModelUsage &&
    candidate.upsertTurnUsage &&
    candidate.upsertToolUsage &&
    candidate.pruneUsage
    ? (candidate as UsageStorePort)
    : undefined;
}

function toolUsageId(sessionId: string, toolCallId: string): string {
  return `usage_tool_${sessionId}_${toolCallId}`;
}

function outputSizeBytes(result: ToolExecutionResult): number | undefined {
  if (result.serialization) return result.serialization.returnedBytes;
  if (typeof result.output === "string") {
    return new TextEncoder().encode(result.output).length;
  }
  return undefined;
}
