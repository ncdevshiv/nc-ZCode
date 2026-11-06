/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import type {
  CompactLifecyclePayload,
  DynamicWorkflowRunProgressPayload,
  ModelCompletePayload,
  ModelNetworkStatusPayload,
  ModelStreamingPayload,
  PermissionDeniedPayload,
  PermissionRequestedPayload,
  PermissionResolvedPayload,
  SessionEvent,
  ToolCallErrorPayload,
  ToolExecutionTelemetry,
  ToolCallProgressPayload,
  ToolCallResultPayload,
  ToolCallScheduledPayload,
  ToolCallStartedPayload,
  TurnCompletePayload,
  TurnErrorPayload,
  TurnStartedPayload,
} from "@zcode/contracts";
import { getModelUsageTotalTokens, SessionEventType } from "@zcode/contracts";
import { parseAutomationRunId } from "@zcode/shared";
import { workflowLifecycleFactFromProgress } from "./conversation-telemetry-workflow-facts.js";
import {
  conversationTelemetryFactSchema,
  type ConversationTelemetryFact,
} from "@zcode/shared/zcode-protocol-v4";

const MAX_TRACKED_LIFECYCLE_KEYS = 2_000;

class BoundedKeySet {
  private readonly keys = new Set<string>();

  add(key: string): boolean {
    if (this.keys.has(key)) return false;
    this.keys.add(key);
    if (this.keys.size > MAX_TRACKED_LIFECYCLE_KEYS) {
      const oldest = this.keys.values().next().value;
      if (typeof oldest === "string") this.keys.delete(oldest);
    }
    return true;
  }

  deletePrefix(prefix: string): void {
    for (const key of this.keys) {
      if (key.startsWith(prefix)) this.keys.delete(key);
    }
  }
}

class BoundedValueMap<T> {
  private readonly values = new Map<string, T>();

  get(key: string): T | undefined {
    return this.values.get(key);
  }

  set(key: string, value: T): void {
    this.values.delete(key);
    this.values.set(key, value);
    if (this.values.size > MAX_TRACKED_LIFECYCLE_KEYS) {
      const oldest = this.values.keys().next().value;
      if (typeof oldest === "string") this.values.delete(oldest);
    }
  }

  delete(key: string): void {
    this.values.delete(key);
  }

  deletePrefix(prefix: string): void {
    for (const key of this.values.keys()) {
      if (key.startsWith(prefix)) this.values.delete(key);
    }
  }
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function nonNegative(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : undefined;
}

function providerHostname(baseURL: string | undefined): string | undefined {
  if (!baseURL) return undefined;
  try {
    return new URL(baseURL).hostname || undefined;
  } catch {
    return undefined;
  }
}

function recordValue(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function skillTelemetryFactFields(
  toolName: string | undefined,
  metadata: unknown,
): Record<string, unknown> {
  if (toolName !== "Skill") return {};
  const value = recordValue(metadata);
  const qualifiedName = optionalString(value.qualifiedName);
  const pluginId = optionalString(value.pluginId);
  const source = optionalString(value.source);
  return {
    ...(qualifiedName ? { skillQualifiedName: qualifiedName } : {}),
    ...(pluginId ? { skillPluginId: pluginId } : {}),
    ...(source ? { skillSource: source } : {}),
  };
}

export function streamingParentToolCallId(payload: Record<string, unknown>): string | undefined {
  const meta = recordValue(payload._meta);
  const zcode = recordValue(meta.zcode);
  return (
    optionalString(payload.parentToolCallId) ??
    optionalString(payload.parentToolUseId) ??
    optionalString(meta.parentToolCallId) ??
    optionalString(meta.parentToolUseId) ??
    optionalString(zcode.parentToolCallId) ??
    optionalString(zcode.parentToolUseId)
  );
}

function mirroredSubagentToolFields(
  payload: Record<string, unknown>,
  display: Record<string, unknown> = {},
) {
  const parentToolCallId =
    optionalString(payload.parentToolCallId) ?? optionalString(display.parentToolCallId);
  const childToolCallId =
    optionalString(payload.childToolCallId) ?? optionalString(display.childToolCallId);
  const agentId = optionalString(payload.agentId) ?? optionalString(display.agentId);
  const agentType = optionalString(payload.agentType) ?? optionalString(display.agentType);
  const childSessionId =
    optionalString(payload.childSessionId) ?? optionalString(display.childSessionId);
  return {
    ...(parentToolCallId ? { parentToolCallId } : {}),
    ...(childToolCallId ? { childToolCallId } : {}),
    ...(agentId ? { agentId } : {}),
    ...(agentType ? { agentType } : {}),
    ...(childSessionId ? { childSessionId } : {}),
    ...(payload.background === true ? { background: true } : {}),
  };
}

function eventTimestamp(event: SessionEvent): number {
  const value =
    event.timestamp instanceof Date ? event.timestamp.getTime() : Number(event.timestamp);
  return Number.isFinite(value) && value >= 0 ? value : 0;
}

function automationAdmission(inputId: string | undefined, automationId: string | undefined) {
  if (!inputId || !automationId) return {};
  const parsed = parseAutomationRunId(inputId);
// When the inputId is not the runId of this automation (historical entry is missed, exception is transparently transmitted), the trigger method is not guessed:
// Only keep the association ID to avoid mislabeling ordinary input as schedule or cutting out pseudo scheduledAt from irrelevant strings.
  if (!parsed || parsed.automationId !== automationId) return { automationId };
  return {
    automationId,
    taskTrigger: parsed.trigger,
    ...(parsed.scheduledAt !== undefined ? { scheduledAt: parsed.scheduledAt } : {}),
  };
}

function cronCreateAutomationId(content: unknown): string | undefined {
  let value = content;
  if (typeof value === "string") {
    try {
      value = JSON.parse(value) as unknown;
    } catch {
      return undefined;
    }
  }
  const parsed = recordValue(value);
  return optionalString(recordValue(parsed.automation).automationId);
}

function totalTokensOf(usage: Record<string, unknown>): number {
  return getModelUsageTotalTokens({
    totalTokens: nonNegative(usage.totalTokens),
    inputTokens: nonNegative(usage.inputTokens),
    outputTokens: nonNegative(usage.outputTokens),
    cacheReadTokens: nonNegative(usage.cacheReadTokens) ?? nonNegative(usage.cacheTokens),
    cacheWriteTokens: nonNegative(usage.cacheWriteTokens),
    reasoningTokens: nonNegative(usage.reasoningTokens),
  });
}

type ToolPerformanceFact = NonNullable<
  Extract<ConversationTelemetryFact, { kind: "tool.lifecycle" }>["performance"]
>;

function toToolPerformanceFact(
  perf: ToolExecutionTelemetry | undefined,
): ToolPerformanceFact | undefined {
  if (!perf) return undefined;
  const command = perf.detail?.kind === "command" ? perf.detail.command : undefined;
  const filesystem =
    perf.detail?.kind === "filesystem" || perf.detail?.kind === "patch"
      ? perf.detail.filesystem
      : undefined;
  const patch = perf.detail?.kind === "patch" ? perf.detail.patch : undefined;
  const fact: ToolPerformanceFact = {
    ...(perf.totalMs !== undefined ? { totalMs: perf.totalMs } : {}),
    ...(perf.permissionWaitMs !== undefined ? { permissionWaitMs: perf.permissionWaitMs } : {}),
    ...(command?.runMs !== undefined ? { commandRunMs: command.runMs } : {}),
    ...(command?.firstOutputMs !== undefined ? { firstOutputMs: command.firstOutputMs } : {}),
    ...(command?.noOutputMs !== undefined ? { noOutputMs: command.noOutputMs } : {}),
    ...(command?.exitCode !== undefined ? { exitCode: command.exitCode } : {}),
    ...(command?.timedOut !== undefined ? { timedOut: command.timedOut } : {}),
    ...(command?.outputBytes !== undefined ? { outputBytes: command.outputBytes } : {}),
    ...(command?.category !== undefined ? { commandCategory: command.category } : {}),
    ...(command?.name !== undefined ? { commandName: command.name } : {}),
    ...(command?.count !== undefined ? { commandCount: command.count } : {}),
    ...(command?.status !== undefined ? { commandStatus: command.status } : {}),
    ...(filesystem?.readMs !== undefined ? { fsReadMs: filesystem.readMs } : {}),
    ...(filesystem?.writeMs !== undefined ? { fsWriteMs: filesystem.writeMs } : {}),
    ...(filesystem?.fileCount !== undefined ? { fileCount: filesystem.fileCount } : {}),
    ...(filesystem?.totalBytes !== undefined ? { totalBytes: filesystem.totalBytes } : {}),
    ...(filesystem?.maxFileBytes !== undefined ? { maxFileBytes: filesystem.maxFileBytes } : {}),
    ...(filesystem?.workspaceKind !== undefined ? { workspaceKind: filesystem.workspaceKind } : {}),
    ...(patch?.matchMs !== undefined ? { patchMatchMs: patch.matchMs } : {}),
    ...(patch?.hunkCount !== undefined ? { hunkCount: patch.hunkCount } : {}),
    ...(patch?.matchAttempts !== undefined ? { matchAttempts: patch.matchAttempts } : {}),
  };
  return Object.keys(fact).length > 0 ? fact : undefined;
}

function terminalStatus(resultType: string): "success" | "interrupted" | "failed" {
  if (resultType === "success") return "success";
  if (resultType === "cancelled") return "interrupted";
  return "failed";
}

interface CompletedModelRequestIdentity {
  requestId: string;
  providerId: string;
  modelId: string;
  providerKind?: string;
  providerHostname?: string;
}

function modelRequestQueueKey(sessionId: string, querySource: string | undefined): string {
  return `${sessionId}\0${querySource ?? ""}`;
}

function isStepUsageQuerySource(querySource: string | undefined): boolean {
// `workflow_child`: dynamic workflow subagent.
// This source must be released, otherwise the subagent's token cannot enter the business bureau.
  return (
    querySource === undefined ||
    querySource === "main_turn" ||
    querySource === "subagent" ||
    querySource === "workflow_child"
  );
}

function isStepUsageModelComplete(payload: ModelCompletePayload): boolean {
  return (
    isStepUsageQuerySource(payload.querySource) &&
    (payload.querySource !== undefined || payload.stopReason !== "tool_internal")
  );
}

function compactTerminalStatus(
  status: CompactLifecyclePayload["status"],
): "completed" | "failed" | "interrupted" | null {
  switch (status) {
    case "completed":
    case "failed":
    case "interrupted":
      return status;
    default:
      return null;
  }
}

/**
* Unify the start and end of the rounds in the live SessionEvent of this process into `turn.started` / `turn.terminal` facts,
* For the App/server to count the number of running sessions. The fact does not have text, only the inputId and final state summary given by admission.
* This class does not read transcript/snapshot, so it cannot recreate events during hydration/recovery.
 */
export class ConversationTelemetryFactNormalizer {
  private readonly firstStreamChunks = new BoundedKeySet();
  private readonly sourceCommandByTurn = new BoundedValueMap<string>();
  private readonly toolNameByCall = new BoundedValueMap<string>();
  private readonly modelBySession = new BoundedValueMap<{
    modelName: string;
    modelProvider: string;
  }>();
  private readonly completedModelRequests = new BoundedValueMap<CompletedModelRequestIdentity[]>();

  normalize(
    sessionId: string,
    event: SessionEvent,
    runtimeMetadata?: { modelName?: string; modelProvider?: string; memoryEnabled?: boolean },
  ): ConversationTelemetryFact | null {
    const turnId = event.turnId ? String(event.turnId) : undefined;
    const turnKey = turnId ? `${sessionId}\0${turnId}` : undefined;
    const base = {
      ...(runtimeMetadata?.memoryEnabled !== undefined
        ? { memoryEnabled: runtimeMetadata.memoryEnabled }
        : {}),
      version: 1 as const,
      eventId: String(event.id),
      eventSeq: Math.max(0, Math.floor(event.sequenceNumber)),
      occurredAt: eventTimestamp(event),
      sessionId,
      ...(turnId ? { turnId } : {}),
    };
    const sourceCommandId = turnKey ? this.sourceCommandByTurn.get(turnKey) : undefined;

    switch (event.type) {
      case SessionEventType.TurnStarted: {
        const payload = event.payload as TurnStartedPayload;
        const backgroundSource =
          payload.backgroundSource === "bash" ||
          payload.backgroundSource === "subagent" ||
          payload.backgroundSource === "workflow"
            ? payload.backgroundSource
            : undefined;
// Both user wheel and background wake are provided with inputId by admission, and persistent messageId is not mixed.
        const inputId = optionalString(payload.inputId);
        if (turnKey && inputId) this.sourceCommandByTurn.set(turnKey, inputId);
        return conversationTelemetryFactSchema.parse({
          ...base,
          kind: "turn.started",
          ...(inputId ? { sourceCommandId: inputId } : {}),
          ...automationAdmission(inputId, optionalString(payload.automationId)),
          ...(optionalString(payload.offPeakTaskId)
            ? { offPeakTaskId: optionalString(payload.offPeakTaskId) }
            : {}),
          ...(payload.offPeakRunType ? { offPeakRunType: payload.offPeakRunType } : {}),
          ...(payload.executionKind ? { executionKind: payload.executionKind } : {}),
          ...(payload.inputSource ? { inputSource: payload.inputSource } : {}),
          ...(backgroundSource ? { backgroundSource } : {}),
        });
      }
      case SessionEventType.ModelNetworkStatus: {
        const payload = event.payload as ModelNetworkStatusPayload;
// Both ends of the admission wait are not provider request status:
// fact's status enumeration does not accept them, skip them explicitly instead of letting schema.parse throw.
        if (payload.type === "model_request_queued" || payload.type === "model_request_admitted") {
          return null;
        }
        const modelProvider = String(payload.providerId);
        const modelName = String(payload.modelId);
        this.modelBySession.set(sessionId, { modelName, modelProvider });
        const fact = conversationTelemetryFactSchema.parse({
          ...base,
          kind: "model.request.status",
          ...(sourceCommandId ? { sourceCommandId } : {}),
          requestId: String(payload.requestId),
          status: payload.type,
          providerId: modelProvider,
          modelId: modelName,
          ...(payload.providerKind ? { providerKind: payload.providerKind } : {}),
          ...(providerHostname(payload.baseURL)
            ? { providerHostname: providerHostname(payload.baseURL) }
            : {}),
          transport: payload.transport,
          ...(payload.querySource ? { querySource: payload.querySource } : {}),
          ...(payload.queryId ? { queryId: String(payload.queryId) } : {}),
          attempt: payload.attempt,
          maxAttempts: payload.maxAttempts,
          ...(payload.type === "model_request_completed"
            ? {
                durationMs: payload.durationMs,
              }
            : {}),
          ...(payload.type === "model_request_failed"
            ? {
                ...(payload.durationMs !== undefined ? { durationMs: payload.durationMs } : {}),
                reason: payload.reason,
                retryable: payload.retryable,
                ...(payload.statusCode !== undefined ? { statusCode: payload.statusCode } : {}),
              }
            : {}),
          ...(payload.type === "model_retry_scheduled"
            ? {
                delayMs: payload.delayMs,
                nextAttempt: payload.nextAttempt,
                reason: payload.reason,
                ...(payload.statusCode !== undefined ? { statusCode: payload.statusCode } : {}),
              }
            : {}),
          ...(payload.type === "model_stream_stalled"
            ? { idleMs: payload.idleMs, timeoutMs: payload.timeoutMs }
            : {}),
        });
        const querySource = optionalString(payload.querySource);
        if (payload.type === "model_request_completed" && isStepUsageQuerySource(querySource)) {
          const key = modelRequestQueueKey(sessionId, querySource);
          const queue = this.completedModelRequests.get(key) ?? [];
          queue.push({
            requestId: String(payload.requestId),
            providerId: modelProvider,
            modelId: modelName,
            ...(payload.providerKind ? { providerKind: payload.providerKind } : {}),
            ...(providerHostname(payload.baseURL)
              ? { providerHostname: providerHostname(payload.baseURL) }
              : {}),
          });
          this.completedModelRequests.set(key, queue);
        }
        return fact;
      }
      case SessionEventType.ModelStreaming: {
        const payload = event.payload as ModelStreamingPayload;
        const rawPayload = recordValue(event.payload);
        const channel =
          payload.kind === "text_delta"
            ? "text"
            : payload.kind === "reasoning_delta"
              ? "thought"
              : null;
        if (!channel) return null;
        const parentToolCallId = streamingParentToolCallId(rawPayload);
        const streamKey = `${sessionId}\0${turnId ?? ""}\0${channel}\0${String(payload.partId ?? "")}\0${parentToolCallId ?? ""}`;
        return conversationTelemetryFactSchema.parse({
          ...base,
          kind: "stream.chunk",
          ...(sourceCommandId ? { sourceCommandId } : {}),
          channel,
          chunkLength: payload.delta.length,
          firstChunk: this.firstStreamChunks.add(streamKey),
          ...(payload.assistantMessageId
            ? { assistantMessageId: String(payload.assistantMessageId) }
            : {}),
          ...(payload.partId ? { partId: String(payload.partId) } : {}),
          ...(parentToolCallId ? { parentToolCallId } : {}),
        });
      }
      case SessionEventType.ToolCallScheduled: {
        const payload = event.payload as ToolCallScheduledPayload;
        const rawPayload = recordValue(event.payload);
        const toolCallId = String(payload.toolCallId);
        this.toolNameByCall.set(`${turnKey ?? sessionId}\0${toolCallId}`, payload.toolName);
        return conversationTelemetryFactSchema.parse({
          ...base,
          kind: "tool.lifecycle",
          ...(sourceCommandId ? { sourceCommandId } : {}),
          phase: "scheduled",
          toolCallId,
          toolName: payload.toolName,
          ...mirroredSubagentToolFields(rawPayload),
        });
      }
      case SessionEventType.ToolCallStarted:
      case SessionEventType.ToolCallProgress:
      case SessionEventType.ToolCallResult:
      case SessionEventType.ToolCallError: {
        const payload = event.payload as
          | ToolCallStartedPayload
          | ToolCallProgressPayload
          | ToolCallResultPayload
          | ToolCallErrorPayload;
        const rawPayload = recordValue(event.payload);
        const toolCallId = String(payload.toolCallId);
        const key = `${turnKey ?? sessionId}\0${toolCallId}`;
        const explicitName = "toolName" in payload ? optionalString(payload.toolName) : undefined;
        const toolName = explicitName ?? this.toolNameByCall.get(key);
        const result =
          event.type === SessionEventType.ToolCallResult
            ? (payload as ToolCallResultPayload)
            : null;
        const error =
          event.type === SessionEventType.ToolCallError ? (payload as ToolCallErrorPayload) : null;
        const display = recordValue(result?.result.display);
// runtime changes perf to nested detail, the old normalizer still changes it
// Insert the flat strict fact as it is, causing the entire tool's final state to be discarded. Only explicit whitelist mapping must be done here,
// The detail or command.hash used for local diagnosis cannot be transparently transmitted again.
        const performance = toToolPerformanceFact(result?.result.perf);
        const phase =
          event.type === SessionEventType.ToolCallStarted
            ? "started"
            : event.type === SessionEventType.ToolCallProgress
              ? "progress"
              : event.type === SessionEventType.ToolCallResult
                ? result?.result.success === false
                  ? "failed"
                  : "completed"
                : "failed";
        const automationId =
          phase === "completed" && toolName === "CronCreate"
            ? cronCreateAutomationId(result?.result.content)
            : undefined;
        if (phase === "completed" || phase === "failed") this.toolNameByCall.delete(key);
        return conversationTelemetryFactSchema.parse({
          ...base,
          kind: "tool.lifecycle",
          ...(sourceCommandId ? { sourceCommandId } : {}),
          phase,
          toolCallId,
          ...(toolName ? { toolName } : {}),
          ...(automationId ? { automationId } : {}),
          ...(result ? { durationMs: result.duration } : {}),
          ...(error ? { errorCode: error.error.code ?? error.error.type } : {}),
          ...(error ? { errorMessage: error.error.message } : {}),
          ...(result?.result.error
            ? { errorCode: result.result.error.code ?? result.result.error.type }
            : {}),
          ...(result?.result.error ? { errorMessage: result.result.error.message } : {}),
          ...skillTelemetryFactFields(toolName, error?.skillMetadata ?? result?.skillMetadata),
// subagent mirror puts the parent-child relationship at the top level of tool event payload, old normalizer
// Only result.display is read, causing fields such as agent_id to be silently discarded before entering agent_step.
          ...mirroredSubagentToolFields(rawPayload, display),
          ...(performance ? { performance } : {}),
        });
      }
      case SessionEventType.PermissionRequested:
      case SessionEventType.PermissionResolved:
      case SessionEventType.PermissionDenied: {
        const payload = event.payload as
          | PermissionRequestedPayload
          | PermissionResolvedPayload
          | PermissionDeniedPayload;
        const rawPayload = recordValue(payload);
        const requested =
          event.type === SessionEventType.PermissionRequested
            ? (payload as PermissionRequestedPayload)
            : null;
        const resolved =
          event.type === SessionEventType.PermissionResolved
            ? (payload as PermissionResolvedPayload)
            : null;
        const denied =
          event.type === SessionEventType.PermissionDenied
            ? (payload as PermissionDeniedPayload)
            : null;
        return conversationTelemetryFactSchema.parse({
          ...base,
          kind: "permission.lifecycle",
          ...(sourceCommandId ? { sourceCommandId } : {}),
          phase: requested ? "requested" : resolved ? "resolved" : "denied",
          ...(optionalString(requested?.requestId ?? resolved?.requestId)
            ? { requestId: optionalString(requested?.requestId ?? resolved?.requestId) }
            : {}),
          toolCallId: String(payload.toolCallId),
          ...(requested?.toolName
            ? { toolName: requested.toolName }
            : denied?.toolName
              ? { toolName: denied.toolName }
              : {}),
          ...(optionalString(rawPayload.childSessionId)
            ? { childSessionId: optionalString(rawPayload.childSessionId) }
            : {}),
          ...(rawPayload.background === true ? { background: true } : {}),
          ...(resolved ? { decision: resolved.decision } : {}),
        });
      }
      case SessionEventType.ModelComplete: {
        const payload = event.payload as ModelCompletePayload;
        const requestQueueKey = modelRequestQueueKey(
          sessionId,
          optionalString(payload.querySource),
        );
        const completedRequests = this.completedModelRequests.get(requestQueueKey) ?? [];
        const completedRequest = completedRequests.shift();
        if (completedRequests.length > 0) {
          this.completedModelRequests.set(requestQueueKey, completedRequests);
        } else {
          this.completedModelRequests.delete(requestQueueKey);
        }
//The title sidecar inherits the current turnId. If its ModelComplete is also converted to
// usage.delta, the renderer will accumulate 64/8 tokens of each round of title into the main dialogue completion.
// In this issue, only the main wheel and subagent request usage are released; sidecar/compact/tool_internal is still not available for delivery.
        if (!isStepUsageModelComplete(payload)) return null;
        const usage = recordValue(payload.usage);
        return conversationTelemetryFactSchema.parse({
          ...base,
          kind: "usage.delta",
          ...(sourceCommandId ? { sourceCommandId } : {}),
          ...(completedRequest
            ? {
                requestId: completedRequest.requestId,
                providerId: completedRequest.providerId,
                modelId: completedRequest.modelId,
                ...(completedRequest.providerKind
                  ? { providerKind: completedRequest.providerKind }
                  : {}),
                ...(completedRequest.providerHostname
                  ? { providerHostname: completedRequest.providerHostname }
                  : {}),
              }
            : {}),
          inputTokens: nonNegative(usage.inputTokens) ?? 0,
          outputTokens: nonNegative(usage.outputTokens) ?? 0,
          totalTokens: totalTokensOf(usage),
          reasoningTokens: nonNegative(usage.reasoningTokens) ?? 0,
          cacheReadTokens:
            nonNegative(usage.cacheReadTokens) ?? nonNegative(usage.cacheTokens) ?? 0,
          cacheWriteTokens: nonNegative(usage.cacheWriteTokens) ?? 0,
        });
      }
      case SessionEventType.DynamicWorkflowRunProgress: {
// The ownership fact of the dynamic workflow subagent: actor-created registration,
// run-settled settlement; other engine events will not be buried.
        return workflowLifecycleFactFromProgress(
          base,
          event.payload as DynamicWorkflowRunProgressPayload,
        );
      }
      case SessionEventType.SubagentSpawned:
      case SessionEventType.SubagentStopped: {
        const payload = recordValue(event.payload);
        const agentId = optionalString(payload.agentId);
        const childSessionId = optionalString(payload.childSessionId);
        if (!agentId || !childSessionId) return null;
        return conversationTelemetryFactSchema.parse({
          ...base,
          kind: "subagent.lifecycle",
          ...(sourceCommandId ? { sourceCommandId } : {}),
          phase: event.type === SessionEventType.SubagentSpawned ? "spawned" : "stopped",
          agentId,
          ...(optionalString(payload.agentType)
            ? { agentType: optionalString(payload.agentType) }
            : {}),
          childSessionId,
          ...(optionalString(payload.parentToolCallId)
            ? { parentToolCallId: optionalString(payload.parentToolCallId) }
            : {}),
          background: payload.background === true,
          ...(optionalString(payload.status) ? { status: optionalString(payload.status) } : {}),
// stopped can independently close the background buried points; retain existing errors in Runtime to avoid failure and summary loss reasons.
          ...(event.type === SessionEventType.SubagentStopped && optionalString(payload.error)
            ? { errorMessage: optionalString(payload.error) }
            : {}),
        });
      }
      case SessionEventType.TurnComplete: {
        const payload = event.payload as TurnCompletePayload;
        const directSourceCommandId = optionalString(payload.inputId) ?? sourceCommandId;
        const fact = conversationTelemetryFactSchema.parse({
          ...base,
          kind: "turn.terminal",
          ...(directSourceCommandId ? { sourceCommandId: directSourceCommandId } : {}),
          status: terminalStatus(payload.resultType),
          resultType: payload.resultType,
          durationMs: payload.duration,
          tokenCount: payload.tokenCount,
          toolCallCount: payload.toolCallCount,
          ...(payload.resultType === "cancelled"
            ? {
                errorCode: "USER_INTERRUPT",
                errorMessage: "User stopped generation",
              }
            : {}),
          ...(payload.backgroundSubagentResultConsumed
            ? { backgroundSubagentResultConsumed: true }
            : {}),
          ...(payload.workflowResultConsumed ? { workflowResultConsumed: true } : {}),
        });
        this.clearTurn(turnKey);
        return fact;
      }
      case SessionEventType.TurnError: {
        const payload = event.payload as TurnErrorPayload;
        const directSourceCommandId = optionalString(payload.inputId) ?? sourceCommandId;
        const fact = conversationTelemetryFactSchema.parse({
          ...base,
          kind: "turn.terminal",
          ...(directSourceCommandId ? { sourceCommandId: directSourceCommandId } : {}),
          status: "failed",
          errorCode: payload.error.code ?? payload.error.type,
          errorMessage: payload.error.message,
          ...(payload.error.retryable !== undefined
            ? { errorRetryable: payload.error.retryable }
            : {}),
          ...(payload.backgroundSubagentResultConsumed
            ? { backgroundSubagentResultConsumed: true }
            : {}),
          ...(payload.workflowResultConsumed ? { workflowResultConsumed: true } : {}),
          turnPhase: payload.turnPhase,
        });
        this.clearTurn(turnKey);
        return fact;
      }
      case SessionEventType.CompactCompleted:
      case SessionEventType.CompactFailed: {
        const payload = event.payload as CompactLifecyclePayload;
        const status = compactTerminalStatus(payload.status);
        if (!status) return null;
        const observedModel = this.modelBySession.get(sessionId);
        const model =
          observedModel ??
          (runtimeMetadata?.modelName || runtimeMetadata?.modelProvider
            ? {
                modelName: runtimeMetadata.modelName ?? "",
                modelProvider: runtimeMetadata.modelProvider ?? "",
              }
            : undefined);
        return conversationTelemetryFactSchema.parse({
          ...base,
          kind: "compaction.terminal",
          ...(payload.sourceCommandId ? { sourceCommandId: payload.sourceCommandId } : {}),
          operationId: payload.operationId,
          ...(payload.messageId ? { messageId: String(payload.messageId) } : {}),
          ...(payload.summaryMessageId
            ? { summaryMessageId: String(payload.summaryMessageId) }
            : {}),
          status,
          trigger: payload.trigger,
          ...(payload.compactReason ? { compactReason: payload.compactReason } : {}),
          ...(payload.reason ? { reason: payload.reason } : {}),
          ...(payload.attempt !== undefined ? { attempt: payload.attempt } : {}),
          ...(payload.maxAttempts !== undefined ? { maxAttempts: payload.maxAttempts } : {}),
          ...(payload.startedAt !== undefined ? { startedAt: payload.startedAt } : {}),
          ...(payload.endedAt !== undefined ? { endedAt: payload.endedAt } : {}),
          ...(payload.preCompactTokenCount !== undefined
            ? { preCompactTokenCount: payload.preCompactTokenCount }
            : {}),
          ...(payload.postCompactTokenCount !== undefined
            ? { postCompactTokenCount: payload.postCompactTokenCount }
            : {}),
          ...(payload.truePostCompactTokenCount !== undefined
            ? { truePostCompactTokenCount: payload.truePostCompactTokenCount }
            : {}),
          ...(model ? model : {}),
        });
      }
      default:
        return null;
    }
  }

  private clearTurn(turnKey: string | undefined): void {
    if (!turnKey) return;
    this.sourceCommandByTurn.delete(turnKey);
    this.firstStreamChunks.deletePrefix(`${turnKey}\0`);
    this.toolNameByCall.deletePrefix(`${turnKey}\0`);
  }

  clearSession(sessionId: string): void {
    const prefix = `${sessionId}\0`;
    this.sourceCommandByTurn.deletePrefix(prefix);
    this.firstStreamChunks.deletePrefix(prefix);
    this.toolNameByCall.deletePrefix(prefix);
    this.modelBySession.delete(sessionId);
    this.completedModelRequests.deletePrefix(prefix);
  }
}
