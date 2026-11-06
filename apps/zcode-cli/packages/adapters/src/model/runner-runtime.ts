import { generateText as aiGenerateText, streamText as aiStreamText } from "ai";
import type {
  ModelProperties,
  ModelRequestAuth,
  ModelTextRequest,
  TraceContext,
} from "@zcode/contracts";
import type { ZCodeProviderAccountAccess } from "@zcode/shared";
import type { AiSdkResolvedModel } from "./model-execution.js";

export type AiSdkGenerateTextOptions = Parameters<typeof aiGenerateText>[0];
export type AiSdkGenerateTextResult = Awaited<ReturnType<typeof aiGenerateText>>;
export type AiSdkStreamTextOptions = Parameters<typeof aiStreamText>[0];
export type AiSdkStreamTextResult = ReturnType<typeof aiStreamText>;
export type ResolvedAiSdkModel = AiSdkResolvedModel & {
  properties: ModelProperties;
  accountAccess?: ZCodeProviderAccountAccess;
};

export interface AiSdkModelRuntime {
  generateText(options: AiSdkGenerateTextOptions): Promise<AiSdkGenerateTextResult>;
  streamText(options: AiSdkStreamTextOptions): AiSdkStreamTextResult;
}

export interface AiSdkModelTextRequest extends ModelTextRequest {
  abortSignal?: AbortSignal;
  traceContext?: TraceContext;
// Start Plan's account authentication material is refreshed according to attempt; the adapter's internal retry is also a real model request.
// Core/host must be given a refresh opportunity before each attempt is sent.
  refreshRuntimeHeadersBeforeAttempt?: (input: {
    accountAccess?: ZCodeProviderAccountAccess;
    attempt: number;
    reason?: "model-request";
    abortSignal?: AbortSignal;
    providerId: string;
    modelId: string;
    traceContext?: TraceContext;
  }) => Promise<{
    headersApplied: boolean;
    requestAuth?: ModelRequestAuth;
  }>;
// The source file is often used directly in adapter testing and development mode; here we explicitly receive the SSE idle timeout incrementing serial number transparently transmitted by core recovery.
  streamIdleTimeoutRetryNumber?: number;
// The same source file loading boundary also needs to explicitly catch the compact dedicated provider stream boundary.
// Prevent the adapter independent typecheck from losing the runtime-only field when the contracts build product has not been refreshed.
  preserveProviderStreamBoundaries?: boolean;
}

export const defaultRuntime: AiSdkModelRuntime = {
  generateText: aiGenerateText,
  streamText: aiStreamText,
};
