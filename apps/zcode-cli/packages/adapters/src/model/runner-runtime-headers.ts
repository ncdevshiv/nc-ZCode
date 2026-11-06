import type { ModelRequestAuth } from "@zcode/contracts";
import { ModelErrorCode, ModelProtocolError } from "@zcode/contracts";
import type { AiSdkModelTextRequest, ResolvedAiSdkModel } from "./runner-runtime.js";

export class RuntimeHeadersRefreshError extends Error {
  constructor(cause: unknown) {
    super(cause instanceof Error ? cause.message : String(cause), { cause });
    this.name = "RuntimeHeadersRefreshError";
  }
}

export async function resolveModelForAttempt(input: {
  attempt: number;
  reason?: "model-request";
  request: AiSdkModelTextRequest;
  resolveModel: (requestAuth?: ModelRequestAuth) => ResolvedAiSdkModel;
}): Promise<ResolvedAiSdkModel> {
  const signal = input.request.abortSignal;
  signal?.throwIfAborted();
  const boundModel = input.resolveModel();
  if (!input.request.refreshRuntimeHeadersBeforeAttempt) return boundModel;
  try {
    const refreshResult = await waitForHeaders(
      () =>
        input.request.refreshRuntimeHeadersBeforeAttempt!({
          attempt: input.attempt,
          reason: input.reason ?? "model-request",
          abortSignal: signal,
          providerId: String(boundModel.providerId),
          modelId: String(boundModel.modelId),
          traceContext: input.request.traceContext,
        }),
      signal,
    );
    signal?.throwIfAborted();
    if (!refreshResult.headersApplied || !refreshResult.requestAuth) {
      throw new Error("Provider request auth was not returned before model request attempt.");
    }
// The current binding is responsible for projecting the full authentication material to the private request, without writing to the shared Registry, or reselecting the model.
    return input.resolveModel(refreshResult.requestAuth);
  } catch (error) {
    if (signal?.aborted) throw error;
// There is a stable error code for missing execution scope credentials; it cannot be swallowed by the generic wrapper waiting for headers.
    if (
      error instanceof ModelProtocolError &&
      error.code === ModelErrorCode.ModelRequestAuthMissing
    )
      throw error;
    throw new RuntimeHeadersRefreshError(error);
  }
}

async function waitForHeaders<T>(run: () => Promise<T>, signal?: AbortSignal): Promise<T> {
  if (!signal) return run();
  signal.throwIfAborted();
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => reject(signal.reason);
    signal.addEventListener("abort", onAbort, { once: true });
    Promise.resolve()
      .then(() => {
        signal.throwIfAborted();
        return run();
      })
      .then(resolve, reject)
      .finally(() => signal.removeEventListener("abort", onAbort));
  });
}
