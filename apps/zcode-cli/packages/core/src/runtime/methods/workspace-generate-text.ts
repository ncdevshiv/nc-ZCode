import {
  SessionEventType,
  createChildTraceContext,
  runWithModelInvocationContext,
  traceContextToLogContext,
} from "../deps.js";
import type {
  ModelInputMessage,
  ModelSelection,
  ModelRequest,
  ModelToolCall,
  ModelToolContract,
  ModelUsage,
  SessionEvent,
  TraceContext,
} from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { createRefreshRuntimeHeadersBeforeModelAttempt } from "./model-runtime-headers.js";
import { recordModelUsageFact } from "./usage-observability.js";
import { createRuntimeModel } from "./runtime-model.js";
import { normalizeStreamError } from "../helpers/index.js";
import { auxiliaryModelOptions } from "../../model/auxiliary-model-options.js";

const WORKSPACE_GENERATE_TEXT_TIMEOUT_MS = 60_000;
const CONNECTIVITY_PROBE_MAX_OUTPUT_TOKENS = 1;
// Use a fixed minimum prompt for detection requests to avoid redundant reasoning overhead; roles, text, or session history cannot be rewritten or mixed into session history.
const CONNECTIVITY_PROBE_SYSTEM = "You are ZCode connectivity probe.";
const CONNECTIVITY_PROBE_USER = "hi";
const GIT_COMMIT_MESSAGE_QUERY_SOURCE = "git_commit_message";

export interface WorkspaceGenerateTextInput {
  selection: ModelSelection;
  prompt?: string;
  messages?: ModelInputMessage[];
  tools?: ModelToolContract[];
  querySource: string;
  maxOutputTokens?: number;
}

export interface WorkspaceGenerateTextResult {
  text: string;
  selection: ModelSelection;
  finishReason: string;
  usage?: ModelUsage;
  toolCalls?: ModelToolCall[];
}

export interface ModelConnectivityTestInput {
  selection: ModelSelection;
}

export async function testModelConnectivity(
  this: AgentRuntimeInternal,
  input: ModelConnectivityTestInput,
  options?: { abortSignal?: AbortSignal; traceContext?: TraceContext },
): Promise<void> {
  const baseModel = createRuntimeModel(this, { selection: input.selection });
// Connection detection does not require body generation; the 5,000 budget for reuse auxiliary generation will wait for redundant inference and output.
// Independently limited to 1 Token, still using the lowest public level, and not changing the budget of other auxiliary calls.
  const model = baseModel.bind({
    reasoningLevel: baseModel.optionSpecs.reasoningLevel.values[0]!,
    maxOutputTokens: CONNECTIVITY_PROBE_MAX_OUTPUT_TOKENS,
  });
  const traceContext = createChildTraceContext(options?.traceContext ?? this.rootTraceContext, {
    attributes: {
      providerId: String(model.providerId),
      modelId: String(model.modelId),
      querySource: "provider_settings_connectivity",
    },
  });
  const abortSignal =
    options?.abortSignal ?? AbortSignal.timeout(WORKSPACE_GENERATE_TEXT_TIMEOUT_MS);
  const request: ModelRequest = {
    abortSignal,
    messages: [
      { role: "system", content: CONNECTIVITY_PROBE_SYSTEM },
      { role: "user", content: CONNECTIVITY_PROBE_USER },
    ],
  };
  let finished = false;
  await runWithModelInvocationContext(
    {
      metadata: traceContextToLogContext(traceContext),
      modelRequestSessionType: "other",
      modelCall: { operation: "workspace_generate_text" },
      statusSink: this.createModelStatusSink(traceContext, []),
      traceContext,
      refreshRuntimeHeadersBeforeAttempt: createRefreshRuntimeHeadersBeforeModelAttempt(this, {
        abortSignal,
        model,
        traceContext,
      }),
    },
    async () => {
      for await (const event of model.streamText(request)) {
        if (event.type === "error") throw normalizeStreamError(event.error);
        if (event.type === "finish") finished = true;
      }
    },
  );
  if (!finished) throw new Error("The model connectivity test flow ends before the finish event");
}

export async function generateWorkspaceText(
  this: AgentRuntimeInternal,
  input: WorkspaceGenerateTextInput,
  options?: { abortSignal?: AbortSignal; traceContext?: TraceContext },
): Promise<WorkspaceGenerateTextResult> {
  assertWorkspaceModelInput(input);
  const querySource = input.querySource.trim() || "workspace_generate_text";
  const traceContext = options?.traceContext ?? this.rootTraceContext;
  const operationTelemetry = this.agentTelemetry.detached({
    executionKind: "foreground",
    operation:
      querySource === GIT_COMMIT_MESSAGE_QUERY_SOURCE
        ? "workspace_git_commit_message"
        : "workspace_generate_text",
    targetKind: "workspace",
    trigger: "user",
    traceContext,
  });
  return operationTelemetry.run(async () => {
    try {
      const result = await generateWorkspaceTextImpl.call(this, input, options);
      operationTelemetry.setResultType("text");
      operationTelemetry.finishCompleted();
      return result;
    } catch (error) {
      if (options?.abortSignal?.aborted) {
        operationTelemetry.finishCancelled("abort_signal");
      } else {
        operationTelemetry.finishFailed("execute", "unknown", error);
      }
      throw error;
    }
  });
}

async function generateWorkspaceTextImpl(
  this: AgentRuntimeInternal,
  input: WorkspaceGenerateTextInput,
  options?: { abortSignal?: AbortSignal; traceContext?: TraceContext },
): Promise<WorkspaceGenerateTextResult> {
  assertWorkspaceModelInput(input);
  const requestedSelection = input.selection;
  const querySource = input.querySource.trim() || "workspace_generate_text";
  const baseModel = createRuntimeModel(this, { selection: requestedSelection });
// The auxiliary request requires the lowest public level, rather than scanning off/nothink and other names and then forcing it to close.
  const model =
    querySource === GIT_COMMIT_MESSAGE_QUERY_SOURCE
      ? baseModel.bind(auxiliaryModelOptions(baseModel))
      : baseModel;
  const baseTraceContext = options?.traceContext ?? this.rootTraceContext;
  const modelTraceContext = createChildTraceContext(baseTraceContext, {
    attributes: {
      model: `${model.providerId}/${model.modelId}`,
      querySource,
    },
  });

  const events: SessionEvent[] = [];
  const messages: ModelInputMessage[] = input.messages
    ? input.messages.map((message) => ({ ...message }))
    : [{ role: "user", content: input.prompt!.trim() }];
  const tools = input.tools ?? [];
  const modelRequestEvent = this.createEvent(
    SessionEventType.ModelRequest,
    {
      messages,
      providerId: String(model.providerId),
      modelId: String(model.modelId),
      querySource,
      toolCount: tools.length,
    },
    modelTraceContext,
  );
  await this.appendEvent(modelRequestEvent, modelTraceContext);
  events.push(modelRequestEvent);

  const modelStartedAt = Date.now();
  const networkEventStartIndex = events.length;
  const abortSignal =
    options?.abortSignal ?? AbortSignal.timeout(WORKSPACE_GENERATE_TEXT_TIMEOUT_MS);
// The Git Commit caller once passed in a fixed value of 256, and Core discarded it according to querySource, forming a false interface.
// The universal generation entry only processes the budget truly provided by the caller; Git auxiliary calls no longer have a fixed upper limit forged by the upstream.
  const requestMaxOutputTokens =
    querySource === GIT_COMMIT_MESSAGE_QUERY_SOURCE ? undefined : input.maxOutputTokens;

  const modelRequest = {
    abortSignal,
    messages,
    tools,
    ...(requestMaxOutputTokens === undefined
      ? {}
      : { options: { maxOutputTokens: requestMaxOutputTokens } }),
  };

  const result = await runWithModelInvocationContext(
    {
      metadata: traceContextToLogContext(modelTraceContext),
      modelRequestSessionType: "other" as const,
      modelCall: {
        operation:
          querySource === GIT_COMMIT_MESSAGE_QUERY_SOURCE
            ? "workspace_git_commit_message"
            : "workspace_generate_text",
        ...(querySource === GIT_COMMIT_MESSAGE_QUERY_SOURCE && model.options.reasoningLevel
          ? { reasoning: { requestedLevel: model.options.reasoningLevel } }
          : {}),
      },
      statusSink: this.createModelStatusSink(modelTraceContext, events),
      traceContext: modelTraceContext,
      refreshRuntimeHeadersBeforeAttempt: createRefreshRuntimeHeadersBeforeModelAttempt(this, {
        abortSignal,
        model,
        traceContext: modelTraceContext,
      }),
    },
    () => model.generateText(modelRequest),
  ).catch(async (error: unknown) => {
    await recordModelUsageFact(this, {
      error,
      events,
      model,
      networkEventStartIndex,
      querySource,
      startedAt: modelStartedAt,
      status: "error",
      traceContext: modelTraceContext,
    });
    throw error;
  });

  const toolCalls = this.extractToolCallsFromResult(result);
  const modelCompleteEvent = this.createEvent(
    SessionEventType.ModelComplete,
    {
      content: result.text,
      querySource,
      stopReason: result.finishReason,
      toolCallCount: toolCalls.length,
      usage: result.usage,
    },
    modelTraceContext,
  );
  await this.appendEvent(modelCompleteEvent, modelTraceContext);
  events.push(modelCompleteEvent);
  await recordModelUsageFact(this, {
    events,
    model,
    networkEventStartIndex,
    querySource,
    result,
    startedAt: modelStartedAt,
    status: "completed",
    toolCallCount: toolCalls.length,
    traceContext: modelTraceContext,
  });

  return {
    text: result.text,
    selection: {
      providerId: requestedSelection.providerId,
      modelId: requestedSelection.modelId,
      ...(requestedSelection.options ? { options: { ...requestedSelection.options } } : {}),
    },
    finishReason: result.finishReason,
    usage: result.usage,
    ...(toolCalls.length > 0 ? { toolCalls } : {}),
  };
}

function assertWorkspaceModelInput(input: WorkspaceGenerateTextInput): void {
  if (input.messages && input.messages.length > 0) return;
  if (input.prompt?.trim()) return;
  throw new Error("Model text generation prompt or messages cannot be empty");
}
