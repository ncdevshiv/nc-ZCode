// ============================================================
// WebSearch Tool
// ============================================================

import {
  CoreErrorType,
  WEBSEARCH_PROVIDER_NATIVE_SPEC,
  WEBSEARCH_TOOL_CONTRACT,
  WebSearchInputJsonSchema,
  WebSearchInputSchema,
  WebSearchOutputJsonSchema,
  WebSearchOutputSchema,
  createCoreError,
  runWithModelInvocationContext,
  toWebSearchProviderNativeArgs,
  type ModelStreamEvent,
  type ModelTextResult,
  type ModelToolCall,
  type ModelUsage,
  type ModelToolContract,
  type WebSearchInput,
  type WebSearchOutput,
} from "@zcode/contracts";
import type { ToolEntry, ToolHandler } from "../types.js";
import { auxiliaryModelOptions } from "../../model/auxiliary-model-options.js";
import { buildWebSearchOutput, formatWebSearchModelContent } from "./websearch-results.js";
import { webSearchTraceFromContext } from "./websearch-support.js";

const WEBSEARCH_TOOL_NAME = "WebSearch";
const PROVIDER_WEBSEARCH_TOOL_NAME = "web_search";
const DEFAULT_TIMEOUT_MS = 60_000;
const DEFAULT_MAX_USES = 8;

const WEBSEARCH_MONTH_NAMES = [
  "January",
  "February",
  "March",
  "April",
  "May",
  "June",
  "July",
  "August",
  "September",
  "October",
  "November",
  "December",
] as const;

function buildWebSearchProviderDescription(now: Date = new Date()): string {
  const currentMonth = `${WEBSEARCH_MONTH_NAMES[now.getMonth()]} ${now.getFullYear()}`;
  return [
    "Search the web. Returns result blocks with titles and URLs. US-only.",
    "",
    `- The current month is ${currentMonth} — use this when searching for recent information.`,
    "- `allowed_domains` / `blocked_domains` filter results.",
    '- After answering from results, end with a "Sources:" list of the URLs you used as markdown links.',
  ].join("\n");
}

const webSearchHandler: ToolHandler<WebSearchInput, WebSearchOutput> = async (input, context) => {
  const startedAt = Date.now();
  const model = context.model;

  if (!model) {
    throw createCoreError(CoreErrorType.ConfigurationError, "Model is required for WebSearch", {
      context: { toolCallId: context.toolCallId, toolName: WEBSEARCH_TOOL_NAME },
      recoverable: false,
    });
  }

  if (!model.properties.supportsNativeWebSearch) {
    throw createCoreError(
      CoreErrorType.ConfigurationError,
      "Current model does not support native WebSearch",
      {
        context: { toolCallId: context.toolCallId, toolName: WEBSEARCH_TOOL_NAME },
        recoverable: true,
      },
    );
  }

  const request: Parameters<typeof model.streamText>[0] = {
    messages: [
      {
        role: "system",
        content: "You are an assistant for performing a web search tool use.",
      },
      {
        role: "user",
        content: `Perform a web search for the query: ${input.query}`,
      },
    ],
    tools: [createProviderNativeWebSearchContract(input)],
// BigModel's Anthropic-compatible endpoint rejects named forced web_search tool_choice (1210).
// Keep automatic selection here, relying on single tool requests and prompts to trigger provider-native searches.
    options: {
      ...auxiliaryModelOptions(model),
      maxOutputTokens: Math.min(4096, model.optionSpecs.maxOutputTokens.max),
    },
    abortSignal: context.abortSignal,
  };

// BigModel Anthropic compatible endpoint's non-streaming JSON will be passed inside the provider
// The web_search result is returned as assistant-side bare tool_result, and the AI ​​SDK will
// Throw Invalid JSON response during the verification phase. Flow-through reuses existing SSE compat.
  const result = await collectWebSearchStreamResult({
    events: runWithModelInvocationContext(
      {
        metadata: {
          traceId: context.traceId,
          sessionId: context.sessionId,
          turnId: context.turnId,
          toolCallId: context.toolCallId,
          toolName: WEBSEARCH_TOOL_NAME,
          querySource: "web_search_tool",
        },
        modelRequestSessionType: "other",
        modelCall: { operation: "web_search" },
// statusSink is not set here: the context.model handed over by the executor already has the default session event outlet.
        traceContext: webSearchTraceFromContext(context),
      },
      () => model.streamText(request),
    ),
  });

  return buildWebSearchOutput(input, result, startedAt);
};

export const webSearchToolEntry: ToolEntry = {
  ...WEBSEARCH_TOOL_CONTRACT,
  providerNative: undefined,
  metadata: {
    name: WEBSEARCH_TOOL_NAME,
// Writing a dead month will cause the model's visible WebSearch description to expire.
// Regenerate each time it is read to prevent the resident process from continuing to deliver the old month after crossing the month.
    get description() {
      return buildWebSearchProviderDescription();
    },
    readOnly: true,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: DEFAULT_TIMEOUT_MS,
    maxOutputBytes: 20_000,
    sideEffectScope: "network",
    riskLevel: "low",
    needsApproval: false,
  },
  handler: webSearchHandler as ToolHandler,
  formatModelContent: formatWebSearchModelContent,
  inputSchema: WebSearchInputJsonSchema,
  outputSchema: WebSearchOutputJsonSchema,
  runtimeInputSchema: WebSearchInputSchema,
  runtimeOutputSchema: WebSearchOutputSchema,
};

function createProviderNativeWebSearchContract(input: WebSearchInput): ModelToolContract {
  return {
    name: PROVIDER_WEBSEARCH_TOOL_NAME,
    capability: "web_search",
    description: "Provider-native web search used internally by the WebSearch tool",
    executionMode: "providerNative",
    providerNative: {
      ...WEBSEARCH_PROVIDER_NATIVE_SPEC,
      args: toWebSearchProviderNativeArgs(input, {
        maxUses: DEFAULT_MAX_USES,
      }),
    },
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string" },
      },
      required: ["query"],
    },
    outputSchema: { type: "object" },
  };
}

async function collectWebSearchStreamResult(input: {
  events: AsyncIterable<ModelStreamEvent>;
}): Promise<ModelTextResult> {
  let text = "";
  let finishReason = "unknown";
  let providerMetadata: Record<string, unknown> | undefined;
  let usage: ModelUsage = {};
  const toolCalls: ModelToolCall[] = [];

  for await (const event of input.events) {
    switch (event.type) {
      case "text_delta":
        text += event.text;
        break;
      case "tool_call":
        toolCalls.push(event.toolCall);
        break;
      case "finish":
        finishReason = event.finishReason;
        usage = event.usage;
        providerMetadata = event.providerMetadata;
        break;
      case "error":
        throw normalizeStreamError(event.error);
    }
  }

  return {
    finishReason,
    providerMetadata,
    text,
    toolCalls: toolCalls.length > 0 ? toolCalls : undefined,
    usage,
  };
}

function normalizeStreamError(error: unknown): Error {
  if (error instanceof Error) {
    return error;
  }

  return createCoreError(CoreErrorType.ModelError, "WebSearch stream failed", {
    context: { error },
    recoverable: true,
  });
}
