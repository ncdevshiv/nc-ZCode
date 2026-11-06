import { traceContextToLogContext } from "../deps.js";
import type { ModelRequestAuth } from "@zcode/contracts";
import type { ZCodeProviderAccountAccess } from "@zcode/shared";
import type { Model, TraceContext } from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";

export function createRefreshRuntimeHeadersBeforeModelAttempt(
  runtime: AgentRuntimeInternal,
  input: {
    abortSignal?: AbortSignal;
    model: Model;
    traceContext: TraceContext;
  },
):
  | ((attemptInput: {
      accountAccess?: ZCodeProviderAccountAccess;
      attempt: number;
      reason?: "model-request";
      abortSignal?: AbortSignal;
    }) => Promise<{
      headersApplied: boolean;
      requestAuth?: ModelRequestAuth;
    }>)
  | undefined {
  const runtimeHeadersPort = runtime.providerRuntimeHeadersPort;
  if (
    !runtimeHeadersPort ||
    !(
      runtimeHeadersPort.shouldRefreshBeforeModelRequest?.({
        providerId: String(input.model.providerId),
        modelId: String(input.model.modelId),
      }) ?? true
    )
  ) {
    return undefined;
  }

  return async (attemptInput) => {
// Start Plan's account authentication materials are refreshed on request, adapter's internal retry, WebSearch/native tool
//Requests and individual model requests such as headers/compact must be refreshed before each real request attempt is sent,
// Old materials before entering the adapter cannot be reused.
// Routing identity: The main runtime reports its own session; the port obtained by the child runtime is derived from the parent runtime
// (helpers/child-client-ports.ts), rewrite sessionId to the root session recognized by the client.
    const refreshResult = await runtimeHeadersPort.refreshBeforeModelRequest({
      accountAccess: attemptInput.accountAccess,
      abortSignal: attemptInput.abortSignal ?? input.abortSignal,
      modelId: String(input.model.modelId),
      providerId: String(input.model.providerId),
      reason: attemptInput.reason ?? "model-request",
      sessionId: runtime.sessionId,
      traceContext: input.traceContext,
      turnId: input.traceContext.turnId,
    });
    if (!refreshResult.headersApplied) {
      throw new Error("Provider runtime headers were not applied before model request attempt.");
    }
    runtime.logger?.debug("Provider runtime headers refreshed before model request attempt", {
      ...traceContextToLogContext(input.traceContext),
      attempt: attemptInput.attempt,
      event: "model.request.runtime_headers_refreshed",
      headersApplied: refreshResult.headersApplied,
      module: "core.runtime",
      providerId: String(input.model.providerId),
    });
    return refreshResult;
  };
}
