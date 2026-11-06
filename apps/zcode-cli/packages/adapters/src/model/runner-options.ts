import { Output, jsonSchema } from "ai";
import type { ModelToolChoice } from "@zcode/contracts";
import type { EnvRecord } from "./model-execution.js";
import { toAiSdkMessages } from "./transform.js";
import { toAiSdkTools } from "./tool-transform.js";
import type {
  AiSdkGenerateTextOptions,
  AiSdkModelTextRequest,
  AiSdkStreamTextOptions,
  ResolvedAiSdkModel,
} from "./runner-runtime.js";
import { createModelRequestAttributionHeaders, type ModelStatusContext } from "./runner-status.js";

type ExperimentalIncludeWithResponseBody = {
  requestBody?: boolean;
  responseBody?: boolean;
};

/** zcode-plan business code often only appears in the response.body of the finish chunk, and the streaming path needs to be explicitly enabled. */
function shouldIncludeStreamResponseBody(resolved: ResolvedAiSdkModel): boolean {
  return (
    resolved.providerKind === "openai-compatible" && resolved.accountAccess?.mode === "start-plan"
  );
}

function mergeRequestHeaders(
  providerHeaders: Record<string, string> | undefined,
  attributionHeaders: Record<string, string>,
): Record<string, string> {
  return {
    ...providerHeaders,
    ...attributionHeaders,
  };
}

export function createGenerateTextOptions(input: {
  anthropicMetadataUserId?: string;
  env?: EnvRecord;
  includeModelIO: boolean;
  request: AiSdkModelTextRequest;
  resolved: ResolvedAiSdkModel;
  statusContext: ModelStatusContext;
}): AiSdkGenerateTextOptions {
  const providerOptions = mergeProviderOptions(
    input.resolved.providerOptions,
    input.request.providerOptions,
  );
  const providerOptionsWithMetadata = mergeAnthropicRequestMetadata({
    metadataUserId: input.anthropicMetadataUserId,
    providerKind: input.resolved.providerKind,
    providerOptions,
  });
  const requestProviderOptions = withNativeGenerateOutputFormat({
    providerOptions: providerOptionsWithMetadata,
    responseJsonSchema: input.request.responseJsonSchema,
    resolved: input.resolved,
  });
  return removeUndefined({
    model: input.resolved.model,
    messages: toAiSdkMessages(input.request.messages, {
      apiFormat: resolveProviderApiFormat(providerOptions),
      providerOptions,
      providerKind: input.resolved.providerKind,
      inputFormat: input.resolved.properties?.inputFormat,
    }),
    tools: toAiSdkTools(input.request.tools, {
      providerKind: input.resolved.providerKind,
      modelId: input.resolved.modelId,
      requiresMfjsToolSchema: input.resolved.properties.requiresMfjsToolSchema,
      supportsNativeWebSearch: input.resolved.properties.supportsNativeWebSearch,
    }),
    toolChoice: toAiSdkToolChoice(input.request.toolChoice),
    temperature: input.request.temperature,
    topP: input.request.topP,
    topK: input.request.topK,
    presencePenalty: input.request.presencePenalty,
    frequencyPenalty: input.request.frequencyPenalty,
    stopSequences: input.request.stopSequences,
    seed: input.request.seed,
    output: input.request.responseJsonSchema
      ? Output.object({ schema: jsonSchema(input.request.responseJsonSchema) })
      : undefined,
    providerOptions: requestProviderOptions,
    abortSignal: input.request.abortSignal,
    headers: mergeRequestHeaders(
      input.resolved.headers,
      createModelRequestAttributionHeaders(input.statusContext),
    ),
    // ZCode owns system-message construction in core/context. Keep AI SDK from
    // printing its generic system-message warning to process stderr.
    allowSystemInMessages: true,
    maxRetries: 0,
    experimental_include: input.includeModelIO
      ? {
          requestBody: true,
          responseBody: true,
        }
      : undefined,
  }) as AiSdkGenerateTextOptions;
}

export function createStreamTextOptions(input: {
  anthropicMetadataUserId?: string;
  env?: EnvRecord;
  includeModelIO: boolean;
  request: AiSdkModelTextRequest;
  resolved: ResolvedAiSdkModel;
  statusContext: ModelStatusContext;
}): AiSdkStreamTextOptions {
  const providerOptions = mergeProviderOptions(
    input.resolved.providerOptions,
    input.request.providerOptions,
  );
  const requestProviderOptions = mergeAnthropicRequestMetadata({
    metadataUserId: input.anthropicMetadataUserId,
    providerKind: input.resolved.providerKind,
    providerOptions,
  });
  return removeUndefined({
    model: input.resolved.model,
    messages: toAiSdkMessages(input.request.messages, {
      apiFormat: resolveProviderApiFormat(providerOptions),
      providerOptions,
      providerKind: input.resolved.providerKind,
      inputFormat: input.resolved.properties?.inputFormat,
    }),
    tools: toAiSdkTools(input.request.tools, {
      providerKind: input.resolved.providerKind,
      modelId: input.resolved.modelId,
      requiresMfjsToolSchema: input.resolved.properties.requiresMfjsToolSchema,
      supportsNativeWebSearch: input.resolved.properties.supportsNativeWebSearch,
    }),
    toolChoice: toAiSdkToolChoice(input.request.toolChoice),
    temperature: input.request.temperature,
    topP: input.request.topP,
    topK: input.request.topK,
    presencePenalty: input.request.presencePenalty,
    frequencyPenalty: input.request.frequencyPenalty,
    stopSequences: input.request.stopSequences,
    seed: input.request.seed,
    providerOptions: requestProviderOptions,
    abortSignal: input.request.abortSignal,
    headers: mergeRequestHeaders(
      input.resolved.headers,
      createModelRequestAttributionHeaders(input.statusContext),
    ),
    // ZCode owns system-message construction in core/context. Keep AI SDK from
    // printing its generic system-message warning to process stderr.
    allowSystemInMessages: true,
    maxRetries: 0,
// AI SDK will swallow metadata events such as Anthropic message_start; compact requires
// Only by observing the raw event in the adapter can the SSE retry be accurately ended. The raw chunk will not be sent to Core/UI.
    includeRawChunks: input.request.preserveProviderStreamBoundaries ? true : undefined,
// The business code of zcode-plan may only be in the tail body of the streaming response, and the responseBody needs to be reserved for error classification and reading.
    experimental_include: createStreamExperimentalInclude(input),
  }) as AiSdkStreamTextOptions;
}

function createStreamExperimentalInclude(input: {
  includeModelIO: boolean;
  resolved: ResolvedAiSdkModel;
}): ExperimentalIncludeWithResponseBody | undefined {
  if (input.includeModelIO) {
    return {
      requestBody: true,
      responseBody: true,
    };
  }
  return shouldIncludeStreamResponseBody(input.resolved) ? { responseBody: true } : undefined;
}

function toAiSdkToolChoice(
  toolChoice?: ModelToolChoice,
): AiSdkGenerateTextOptions["toolChoice"] | undefined {
  return toolChoice as AiSdkGenerateTextOptions["toolChoice"] | undefined;
}

function mergeProviderOptions(
  base?: Record<string, unknown>,
  override?: Record<string, unknown>,
): Record<string, unknown> | undefined {
  if (!base && !override) {
    return undefined;
  }
  return {
    ...base,
    ...override,
  };
}

function withNativeGenerateOutputFormat(input: {
  providerOptions: Record<string, unknown> | undefined;
  resolved: ResolvedAiSdkModel;
  responseJsonSchema: AiSdkModelTextRequest["responseJsonSchema"];
}): Record<string, unknown> | undefined {
  if (!input.responseJsonSchema || input.resolved.providerKind !== "anthropic") {
    return input.providerOptions;
  }

  const anthropicOptions = asPlainRecord(input.providerOptions?.anthropic);
  return {
    ...input.providerOptions,
// The real model ID of the Lite role may not be in the static capability table of the AI ​​SDK;
// The explicit schema must continue to generate the target output_config and cannot degenerate into the JSON tool.
    anthropic: { ...anthropicOptions, structuredOutputMode: "outputFormat" },
  };
}

function mergeAnthropicRequestMetadata(input: {
  metadataUserId: string | undefined;
  providerKind: ResolvedAiSdkModel["providerKind"];
  providerOptions: Record<string, unknown> | undefined;
}): Record<string, unknown> | undefined {
  if (input.providerKind !== "anthropic" || input.metadataUserId === undefined) {
    return input.providerOptions;
  }

  const anthropicOptions = asPlainRecord(input.providerOptions?.anthropic) ?? {};
  const metadata = asPlainRecord(anthropicOptions.metadata) ?? {};
  return {
    ...input.providerOptions,
    anthropic: {
      ...anthropicOptions,
      metadata: {
        ...metadata,
        userId: input.metadataUserId,
      },
    },
  };
}

function asPlainRecord(value: unknown): Record<string, unknown> | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }
  return value as Record<string, unknown>;
}

function resolveProviderApiFormat(providerOptions?: Record<string, unknown>): string | undefined {
  const apiFormat = providerOptions?.apiFormat;
  return typeof apiFormat === "string" ? apiFormat : undefined;
}

function removeUndefined<T extends Record<string, unknown>>(value: T): Partial<T> {
  return Object.fromEntries(
    Object.entries(value).filter(([, entryValue]) => entryValue !== undefined),
  ) as Partial<T>;
}
