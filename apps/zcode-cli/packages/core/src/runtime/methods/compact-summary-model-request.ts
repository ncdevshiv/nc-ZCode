import {
  ModelErrorCode,
  runWithModelInvocationContext,
  traceContextToLogContext,
} from "../deps.js";
import type {
  Logger,
  Model,
  ModelInvocationContext,
  ModelStreamEvent,
  ModelToolCall,
  ModelUsage,
  TraceContext,
} from "../deps.js";
import {
  isModelContextExceededError,
  isModelMediaTooLargeError,
  isTurnCancellationError,
  normalizeModelToolCallsForRuntime,
  normalizeStreamError,
} from "../helpers/index.js";
import type { RuntimeModelTextResult } from "../types.js";

type CompactSummaryModelRequest = {
  abortSignal?: AbortSignal;
  maxOutputTokens?: number;
  messages: Parameters<Model["generateText"]>[0]["messages"];
  modelCall?: ModelInvocationContext["modelCall"];
  modelRequestSessionType?: ModelInvocationContext["modelRequestSessionType"];
  metadata?: ModelInvocationContext["metadata"];
  preserveProviderStreamBoundaries?: boolean;
  statusSink?: ModelInvocationContext["statusSink"];
  tools?: Parameters<Model["generateText"]>[0]["tools"];
  traceContext: TraceContext;
  refreshRuntimeHeadersBeforeAttempt?: ModelInvocationContext["refreshRuntimeHeadersBeforeAttempt"];
};

interface CompactSummaryFinish {
  finishReason: string;
  providerMetadata?: Record<string, unknown>;
  usage: ModelUsage;
}

interface CompactSummaryStreamState {
  committedText: string;
  committedContentBlock: boolean;
  currentTextBlockId?: string;
  finish?: CompactSummaryFinish;
  pendingTextById: Map<string | undefined, string>;
  providerContentBlockTypes: Map<number, string>;
  providerMessageProtocolObserved: boolean;
  providerResponseStarted: boolean;
  providerStopReasonPresent: boolean;
  sawDelta: boolean;
  toolCallIds: Set<string>;
  toolCalls: ModelToolCall[];
}

interface RunCompactSummaryModelRequestInput {
  logger?: Logger;
  model: Model;
  request: CompactSummaryModelRequest;
}

const COMPACT_SUMMARY_SETUP_ERROR_CODES = new Set<string>([
  ModelErrorCode.InvalidModelSelection,
  ModelErrorCode.ModelConfigMissing,
  ModelErrorCode.ProviderNotFound,
  ModelErrorCode.ProviderNotConfigured,
  ModelErrorCode.ModelNotFound,
  ModelErrorCode.InvalidModelRequest,
]);

export async function runCompactSummaryModelRequest(
  input: RunCompactSummaryModelRequestInput,
): Promise<RuntimeModelTextResult> {
  const state = createCompactSummaryStreamState();
  const streamingLogicalCallId = crypto.randomUUID();
  const streamingRequest: CompactSummaryModelRequest = {
    ...input.request,
    modelCall: {
      ...input.request.modelCall,
      callCause: input.request.modelCall?.callCause ?? "initial",
      logicalCallId: streamingLogicalCallId,
    },
  };

  try {
    for await (const event of compactModelStream(input, streamingRequest)) {
      applyCompactSummaryStreamEvent(state, event, input);
    }

    if (!state.finish) {
      throw new Error("Compact summary stream ended before finish");
    }
    if (
      (state.providerMessageProtocolObserved && !state.providerResponseStarted) ||
      (!state.committedContentBlock && !hasCompactSummaryStopReason(state))
    ) {
// AI SDK will synthesize finish(other) with empty SSE, and will also swallow the real stop reason of message_delta;
//Accept the stream only when response start is observed and at least one of block stop / truthy stop reason is true.
      throw new Error("Compact summary stream ended without a complete provider response");
    }
  } catch (error) {
    return handleCompactSummaryStreamFailure({ ...input, request: streamingRequest }, state, error);
  }

  return compactSummaryStreamResult(state, state.finish);
}

function applyCompactSummaryStreamEvent(
  state: CompactSummaryStreamState,
  event: ModelStreamEvent,
  input: RunCompactSummaryModelRequestInput,
): void {
  switch (event.type) {
    case "start":
    case "reasoning_start":
    case "tool_input_start":
      return;

    case "compact_stream_boundary":
      applyCompactProviderBoundary(state, event);
      return;

    case "text_start":
      state.currentTextBlockId = event.id;
      state.pendingTextById.set(event.id, "");
      return;

    case "tool_input_end":
      commitNormalizedContentBlock(state);
      return;

    case "text_delta": {
      state.sawDelta = true;
      const textBlockId = event.id ?? state.currentTextBlockId;
      state.pendingTextById.set(
        textBlockId,
        (state.pendingTextById.get(textBlockId) ?? "") + event.text,
      );
      return;
    }

    case "text_end":
      state.committedText +=
        state.pendingTextById.get(event.id) ?? state.pendingTextById.get(undefined) ?? "";
      state.pendingTextById.delete(event.id);
      state.pendingTextById.delete(undefined);
      if (state.currentTextBlockId === event.id) {
        state.currentTextBlockId = undefined;
      }
      commitNormalizedContentBlock(state);
      return;

    case "reasoning_delta":
      state.sawDelta = true;
      return;

    case "reasoning_end":
      commitNormalizedContentBlock(state);
      return;

    case "tool_input_delta":
      state.sawDelta = true;
      return;

    case "tool_call": {
// tool call is only responsible for denying the required payload aggregation; raw message-block provider
//Whether it has been submitted is determined by compact_stream_boundary. The tool call synthesized by the SDK cannot be regarded as a commit.
      commitNormalizedContentBlock(state);
      const [toolCall] =
        normalizeModelToolCallsForRuntime([event.toolCall], {
          logger: input.logger,
          model: modelSelection(input.model),
          source: "compactStreamText",
          traceContext: input.request.traceContext,
        }) ?? [];
      if (!toolCall || state.toolCallIds.has(toolCall.id)) return;
      state.toolCallIds.add(toolCall.id);
      state.toolCalls.push(toolCall);
      return;
    }

    case "finish":
// finish only describes the request result, not equivalent to content_block_stop; clean EOF
// Will return normally, but if iterator tail error occurs after finish and there is no block end, HTTP fallback is still allowed.
      state.finish = {
        finishReason: event.finishReason,
        providerMetadata: event.providerMetadata,
        usage: event.usage,
      };
      return;

    case "error":
      throw event.error;
  }
}

function applyCompactProviderBoundary(
  state: CompactSummaryStreamState,
  event: Extract<ModelStreamEvent, { type: "compact_stream_boundary" }>,
): void {
  if (event.boundary === "inferred_content_block_stop") {
    commitNormalizedContentBlock(state);
    return;
  }

  if (!state.providerMessageProtocolObserved) {
// Once raw provenance appears, it takes over the commit judgment; normalized end cannot continue to serve as a commit proof.
    state.committedContentBlock = false;
  }
  state.providerMessageProtocolObserved = true;

  switch (event.boundary) {
    case "provider_stop_reason":
      state.providerStopReasonPresent = event.present;
      return;

    case "provider_response_start":
      state.providerResponseStarted = true;
      return;

    case "provider_content_block_start":
      if (event.index === null || event.blockType === null) {
        throw new Error("Invalid compact provider content block start");
      }
      state.providerContentBlockTypes.set(event.index, event.blockType);
      return;

    case "provider_content_block_delta": {
      const blockType =
        event.index === null ? undefined : state.providerContentBlockTypes.get(event.index);
      if (
        blockType === undefined ||
        !isProviderContentBlockDeltaCompatible(blockType, event.deltaType)
      ) {
        throw new Error("Invalid compact provider content block delta");
      }
      return;
    }

    case "provider_content_block_stop":
      if (
        !state.providerResponseStarted ||
        event.index === null ||
        !state.providerContentBlockTypes.has(event.index)
      ) {
// content_block_stop must first have message_start and the same index block start; orphan stop
// Fail immediately; the unified IteratorClose will release the provider reader, and the fallback gate will then press the existing commit to determine whether it can be replayed.
        throw new Error("Invalid compact provider content block stop");
      }
      state.committedContentBlock = true;
      return;
  }
}

async function handleCompactSummaryStreamFailure(
  input: RunCompactSummaryModelRequestInput,
  state: CompactSummaryStreamState,
  error: unknown,
): Promise<RuntimeModelTextResult> {
  if (
    isTurnCancellationError(error, input.request.abortSignal) ||
    isModelContextExceededError(error) ||
    isModelMediaTooLargeError(error) ||
    isCompactSummarySetupFailure(error) ||
    state.committedContentBlock
  ) {
    throw error;
  }
  const normalizedError = error instanceof Error ? error : normalizeStreamError(error);

// stream delta of compact summary never enters session/UI, in content block
// Can be safely discarded and redirected to non-stream before submission; this is different from the visible streaming recovery boundary of a normal main request.
  input.logger?.warn("Compact summary stream failed; falling back to non-streaming", {
    ...traceContextToLogContext(input.request.traceContext),
    errorMessage: normalizedError.message,
    event: "compact.summary.stream_to_non_stream_fallback",
    model: `${input.model.providerId}/${input.model.modelId}`,
    module: "core.runtime",
    observedPartialOutput: state.sawDelta,
  });

  const fallbackRequest = {
    ...input.request,
    modelCall: {
      ...input.request.modelCall,
      attributes: {
        ...input.request.modelCall?.attributes,
      },
      callCause: "fallback_replacement" as const,
      logicalCallId: crypto.randomUUID(),
      previousLogicalCallId: input.request.modelCall?.logicalCallId,
    },
  };
  return runWithModelInvocationContext(invocationContext(fallbackRequest), () =>
    input.model.generateText(cleanRequest(fallbackRequest)),
  );
}

function compactModelStream(
  input: RunCompactSummaryModelRequestInput,
  request: CompactSummaryModelRequest,
): AsyncIterable<ModelStreamEvent> {
  return runWithModelInvocationContext(invocationContext(request), () =>
    input.model.streamText(cleanRequest(request)),
  );
}

function cleanRequest(request: CompactSummaryModelRequest) {
  return {
    messages: request.messages,
    tools: request.tools,
    abortSignal: request.abortSignal,
    ...(request.maxOutputTokens !== undefined
      ? { options: { maxOutputTokens: request.maxOutputTokens } }
      : {}),
  };
}

function invocationContext(request: CompactSummaryModelRequest) {
  return {
    metadata: request.metadata,
    modelCall: request.modelCall,
    modelRequestSessionType: request.modelRequestSessionType,
    statusSink: request.statusSink,
    traceContext: request.traceContext,
    preserveProviderStreamBoundaries: request.preserveProviderStreamBoundaries,
    refreshRuntimeHeadersBeforeAttempt: request.refreshRuntimeHeadersBeforeAttempt,
  };
}

function isCompactSummarySetupFailure(error: unknown): boolean {
  const errorRecord = asRecord(error);
  const context = asRecord(errorRecord?.context);
  const streamFailurePhase = context?.streamFailurePhase;
  if (streamFailurePhase === "response_body") {
    return false;
  }

  if (streamFailurePhase === "request_setup") {

// Other synchronous setups or explicit HTTP rejections are original request errors and cannot be covered by the second transport.
    return context?.httpResponseStatus !== 404;
  }

  return (
    typeof errorRecord?.code === "string" && COMPACT_SUMMARY_SETUP_ERROR_CODES.has(errorRecord.code)
  );
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object"
    ? (value as Record<string, unknown>)
    : undefined;
}

function compactSummaryStreamResult(
  state: CompactSummaryStreamState,
  finish: CompactSummaryFinish,
): RuntimeModelTextResult {
  return {
    finishReason: finish.finishReason,
    providerMetadata: finish.providerMetadata,
    text: state.committedText,
    toolCalls: state.toolCalls.length > 0 ? state.toolCalls : undefined,
    usage: finish.usage,
  };
}

function modelSelection(model: Model) {
  return { providerId: model.providerId, modelId: model.modelId };
}

function commitNormalizedContentBlock(state: CompactSummaryStreamState): void {
// Providers without raw message-block provenance continue to use AI SDK normalized end inference;
// Once this provenance is observed, only provider content_block_stop can solidify the commit.
  if (!state.providerMessageProtocolObserved) {
    state.committedContentBlock = true;
  }
}

function hasCompactSummaryStopReason(state: CompactSummaryStreamState): boolean {
  if (state.providerMessageProtocolObserved) {
    return state.providerStopReasonPresent;
  }
  const rawFinishReason = state.finish?.providerMetadata?.rawFinishReason;
  if (typeof rawFinishReason === "string" && rawFinishReason.trim().length > 0) {
    return true;
  }
  const finishReason = state.finish?.finishReason.trim().toLowerCase();
  return finishReason !== undefined && finishReason.length > 0 && finishReason !== "other";
}

function isProviderContentBlockDeltaCompatible(
  blockType: string,
  deltaType: string | null,
): boolean {
  switch (deltaType) {
    case "text_delta":
      return blockType === "text";
    case "input_json_delta":
      return blockType === "tool_use" || blockType === "server_tool_use";
    case "signature_delta":
      return blockType === "thinking";
    case "thinking_delta":
      return blockType === "thinking" || blockType === "redacted_thinking";
    case "citations_delta":
    default:
// citations and unknown/future delta only require that the block already exists and do not participate in text aggregation.
      return true;
  }
}

function createCompactSummaryStreamState(): CompactSummaryStreamState {
  return {
    committedText: "",
    committedContentBlock: false,
    pendingTextById: new Map(),
    providerContentBlockTypes: new Map(),
    providerMessageProtocolObserved: false,
    providerResponseStarted: false,
    providerStopReasonPresent: false,
    sawDelta: false,
    toolCallIds: new Set(),
    toolCalls: [],
  };
}
