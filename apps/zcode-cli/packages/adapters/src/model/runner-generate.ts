/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import type { Logger, ModelStatusSink, ModelTextResult } from "@zcode/contracts";
import {
  ModelErrorCode,
  ModelProtocolError,
  ModelRetryReason,
  ModelTransportKind as ModelTransportKindValue,
} from "@zcode/contracts";
import { classifyModelFailure, inspectProviderFailure } from "./failure-classifier.js";
import type { ClassifiedModelFailure } from "./failure-classifier.js";
import { getResponseHeaders, unwrapRetryError } from "./failure-inspection.js";
import { offPeakTicketExpiredMessage, resolveOffPeakFailureDecision } from "./offpeak-retry.js";
import { AiSdkModelAdapterError } from "./errors.js";
import { resolveAnthropicRequestMetadataUserId } from "./anthropic-request-metadata.js";
import { createGenerateTextOptions } from "./runner-options.js";
import { detectProviderBusinessFinishError } from "./provider-finish-business-error.js";
import {
  normalizeReasoning,
  normalizeSources,
  normalizeToolCalls,
  normalizeToolResults,
  normalizeUsage,
} from "./runner-normalization.js";
import {
  isDevelopmentModelIOEnv,
  recordGenerateTextDebug,
  shouldRecordModelIO,
} from "./runner-debug.js";
import {
  getGenerateTextResultMetadata,
  isZeroOutputModelCompletion,
  logGenerateTextDiagnostics,
} from "./runner-diagnostics.js";
import { sanitizeModelNetworkHeaders } from "./runner-network-headers.js";
import { canRetryEmptyCompletion, scheduleEmptyCompletionRetry } from "./empty-completion-retry.js";
import {
  calculateRetryDelay,
  logRetryDelayDecision,
  sleep,
  toAdapterError,
} from "./runner-retry.js";
import {
  admissionWaitPublishers,
  createAttemptStatusContext,
  createStatusContext,
  publishModelStatus,
} from "./runner-status.js";
import type { EnvRecord } from "./model-execution.js";
import type { ResolvedAiSdkModelRetryOptions } from "./retry-policy.js";
import type {
  AiSdkModelRuntime,
  AiSdkModelTextRequest,
  ResolvedAiSdkModel,
} from "./runner-runtime.js";
import { resolveModelForAttempt, RuntimeHeadersRefreshError } from "./runner-runtime-headers.js";
import { retryAllowedByFailurePolicy } from "./workflow-model-failure-policy.js";
import { modelFailureStatusFields, providerRequestIdFromHeaders } from "./runner-telemetry.js";
import { repairReasoningHistoryAfterSignatureRejection } from "./reasoning-history-normalization.js";
import { admitAttempt, type AttemptAdmission } from "./request-admission.js";
import {
  retryAttemptLoopContinues,
  retryBudgetAllows,
  retryBudgetMaxAttempts,
} from "./retry-budget.js";

export async function runGenerateText(input: {
  debugDir?: string;
  env: EnvRecord;
  logger?: Logger;
  request: AiSdkModelTextRequest;
  resolveModel: () => ResolvedAiSdkModel;
  resolved: ResolvedAiSdkModel;
  retry: ResolvedAiSdkModelRetryOptions;
  runtime: AiSdkModelRuntime;
  statusSink?: ModelStatusSink;
  modelIoFullRetentionEnabled: boolean;
}): Promise<ModelTextResult> {
//Retry budget gear: workflow actor's request is unbounded,
// Only relax the abandon condition for transient failure; maxAttempts in the status event is 0 to indicate no upper limit.
  const retryBudget = input.request.modelRetryBudget;
  const statusMaxAttempts = (extraAttempts: number): number =>
    retryBudgetMaxAttempts(retryBudget, input.retry.maxAttempts + extraAttempts);
  const baseStatusContext = createStatusContext({
    maxAttempts: statusMaxAttempts(0),
    request: input.request,
    resolved: input.resolved,
    transport: ModelTransportKindValue.Http,
  });
  const recordModelIO =
    input.request.metadata?.skipTranscript !== true && shouldRecordModelIO(input.env);
  const isDev = isDevelopmentModelIOEnv(input.env);
  let requestMessages = input.request.messages;
  let signatureRepairAttempted = false;
  let emptyCompletionRetryCount = 0;

  for (
    let attempt = 1;
    retryAttemptLoopContinues(
      retryBudget,
      attempt,
      input.retry.maxAttempts + Number(signatureRepairAttempted),
    );
    attempt += 1
  ) {
    const retryBudgetAttempt =
      attempt - Number(signatureRepairAttempted);
    const attemptRequest = { ...input.request, messages: requestMessages };
    const startedAt = Date.now();
    let resolved = input.resolved;
    let statusContext = createAttemptStatusContext(
      {
        ...baseStatusContext,
        maxAttempts: statusMaxAttempts(
          Number(signatureRepairAttempted),
        ),
      },
      attempt,
    );
    let options: ReturnType<typeof createGenerateTextOptions> | undefined;
    let requestInvocationCompleted = false;
    let requestHeaders: Record<string, string> = {};
    let requestHeaderCount = 0;

// Process-level admission: each attempt to issue the first slot,
// The ticket is returned at the end of this attempt (success/failure/throw are all handled by finally; returned before exiting sleep). waiting to be
// Cancellation → The same path as sleep being canceled: remember that canceled in the connect phase fails and is thrown.
    let admission: AttemptAdmission;
    try {
      admission = await admitAttempt({
        admission: input.request.modelRequestAdmission,
        model: { providerId: String(resolved.providerId), modelId: String(resolved.modelId) },
        signal: input.request.abortSignal,
        ...admissionWaitPublishers(statusContext, attempt, statusPublishOptions(input)),
      });
    } catch (admitError) {
      const admitFailure = classifyModelFailure(admitError, input.request.abortSignal);
      await publishModelStatus(
        {
          ...statusContext,
          attempt,
          message: admitFailure.message,
          reason: admitFailure.reason,
          requestHeaderCount,
          requestHeaders,
          retryable: false,
          statusCode: admitFailure.statusCode,
          ...modelFailureStatusFields(admitError, admitFailure, "connect"),
          timestamp: new Date().toISOString(),
          type: "model_request_failed",
        },
        {
          ...statusPublishOptions(input),
          failureError: unwrapRetryError(admitError),
        },
      );
      throw toAdapterError(admitError, admitFailure, statusContext, attempt, {
        errorPhase: "connect",
      });
    }

    try {
      resolved = await resolveModelForAttempt({
        attempt,
        request: attemptRequest,
        resolveModel: input.resolveModel,
      });
      const anthropicMetadataUserId = await resolveAnthropicRequestMetadataUserId({
        env: input.env,
        providerKind: resolved.providerKind,
        sessionId: statusContext.sessionId,
      });
      options = createGenerateTextOptions({
        anthropicMetadataUserId,
        env: input.env,
        includeModelIO: recordModelIO,
        request: attemptRequest,
        resolved,
        statusContext,
      });
      requestHeaders = sanitizeModelNetworkHeaders(options.headers);
      requestHeaderCount = Object.keys(requestHeaders).length;
      await publishModelStatus(
        {
          ...statusContext,
          attempt,
          requestHeaderCount,
          requestHeaders,
          timestamp: new Date(startedAt).toISOString(),
          type: "model_request_started",
        },
        statusPublishOptions(input, admission),
      );

// Some non-streaming provider/fetch compatibility layers will not settle in time after receiving AbortSignal.
// generateText promise causes the runtime to stop, and the goal verifier still has to wait for the upstream to return naturally before stopping.
// The adapter is a local cancellation contract boundary: the signal is rejected immediately once aborted, and the late provider result is only discarded.
      const pendingResult = input.runtime.generateText(options);
// The successful construction of options does not mean that the runtime has accepted the request; the synchronous setup exception will be thrown directly at the call point.
// Only when the generateText call returns a pending promise does it enter the response attribution boundary to avoid recording the local setup as a provider.
      requestInvocationCompleted = true;
      const result = await waitForGenerateTextOrAbort(pendingResult, input.request.abortSignal);
      const responseHeaders = sanitizeModelNetworkHeaders(
        getGenerateTextResultMetadata(result)?.response?.headers,
      );
      const providerBusinessFinishError = detectProviderBusinessFinishError({
        providerId: String(resolved.providerId),
        providerKind: resolved.providerKind,
        source: {
          finishReason: result.finishReason,
          providerMetadata: result.providerMetadata,
          rawFinishReason: (result.providerMetadata as Record<string, unknown> | undefined)
            ?.rawFinishReason,
          response: (result as unknown as { response?: unknown }).response,
        },
      });
      if (providerBusinessFinishError) {
        throw providerBusinessFinishError;
      }
      const usage = normalizeUsage(result.totalUsage ?? result.usage);
      const toolCalls = normalizeToolCalls(result, input.logger);
      const toolResults = normalizeToolResults(result, toolCalls);
      const sources = normalizeSources(result);
      const text = input.request.responseJsonSchema
        ? serializeStructuredOutput(result)
        : result.text;
      const reasoning = normalizeReasoning(result.reasoning);
      const reasoningLength = (reasoning ?? []).reduce(
        (total, block) => total + block.text.length,
        0,
      );
      if (
        input.request.preserveProviderStreamBoundaries !== true &&
        isZeroOutputModelCompletion({
          finishReason: result.finishReason,
          reasoningLength,
          textLength: text.length,
          toolCallCount: toolCalls?.length ?? 0,
          usage,
        }) &&
        canRetryEmptyCompletion({
          abortSignal: input.request.abortSignal,
          attempt,
          maxAttempts: input.retry.maxAttempts,
          retryCount: emptyCompletionRetryCount,
        })
      ) {
        const completedAt = Date.now();
// Empty completion means that the provider promise resolves normally and will not enter the exception retry catch;
// It must be recognized and retried before the adapter returns, otherwise the core can only receive a final empty response error.
        logGenerateTextDiagnostics({
          attempt,
          completedAt,
          logger: input.logger,
          result,
          startedAt,
          statusContext,
          toolCallCount: toolCalls?.length ?? 0,
          usage,
        });
        emptyCompletionRetryCount += 1;
        await scheduleEmptyCompletionRetry({
          abortSignal: input.request.abortSignal,
          attempt,
          completedAt,
          errorPhase: "response",
          logger: input.logger,
          requestHeaders,
          requestStatusSink: input.request.statusSink,
          responseHeaders,
          retry: input.retry,
          retryBudgetAttempt,
          startedAt,
          statusContext,
          statusSink: input.statusSink,
        });
        continue;
      }
      const completedAt = Date.now();

      recordGenerateTextDebug({
        modelIoFullRetentionEnabled: input.modelIoFullRetentionEnabled,
        attempt,
        debugDir: input.debugDir,
        isDev,
        normalizedToolCalls: toolCalls,
        options,
        recordModelIO,
        request: attemptRequest,
        requestId: statusContext.requestId,
        resolved,
        result,
        startedAt,
      });
      logGenerateTextDiagnostics({
        attempt,
        completedAt,
        logger: input.logger,
        result,
        statusContext,
        startedAt,
        toolCallCount: toolCalls?.length ?? 0,
        usage,
      });
      await publishModelStatus(
        {
          ...statusContext,
          attempt,
          durationMs: completedAt - startedAt,
          finishReason: result.finishReason,
          requestHeaderCount,
          requestHeaders,
          responseHeaderCount: Object.keys(responseHeaders).length,
          responseHeaders,
          providerRequestId: providerRequestIdFromHeaders(responseHeaders),
          timestamp: new Date(completedAt).toISOString(),
          type: "model_request_completed",
          usage,
        },
        statusPublishOptions(input, admission),
      );

      return {
        text,
        finishReason: result.finishReason,
        usage,
        reasoning,
        toolCalls,
        toolResults,
        sources,
        providerMetadata: result.providerMetadata as Record<string, unknown> | undefined,
      };
    } catch (error) {
// After merging, the authentication parsing enters attempt try; consistent with stream, the typed error of missing credentials before the network is retained.
      if (
        error instanceof ModelProtocolError &&
        error.code === ModelErrorCode.ModelRequestAuthMissing
      )
        throw error;
      const completedAt = Date.now();
      const classified = classifyModelFailure(error, input.request.abortSignal);
      if (error instanceof RuntimeHeadersRefreshError) {
        classified.message = error.message;
        classified.retryable = false;
      }
// off-peak special decision (only idle plan provider, see offpeak-retry.ts): queue 429 exempt budget,
// 3102 (compatible with old 3001) triggers desktop side continuation with stable mark failure.
      const offPeak = resolveOffPeakFailureDecision({
        offPeak: resolved.accountAccess?.mode === "off-peak",
        failure: classified,
        error: unwrapRetryError(error),
      });
      const failure: ClassifiedModelFailure =
        offPeak?.kind === "ticketExpired"
          ? {
              ...classified,
              retryable: false,
              message: offPeakTicketExpiredMessage(classified.message),
            }
          : offPeak?.kind === "queued"
            ? { ...classified, retryable: true, retryReason: ModelRetryReason.OffpeakQueued }
            : classified;
      const responseHeaders = sanitizeModelNetworkHeaders(
        getResponseHeaders(unwrapRetryError(error)),
      );
      const repairedMessages =
        !signatureRepairAttempted && resolved.providerKind === "anthropic"
          ? repairReasoningHistoryAfterSignatureRejection(requestMessages, error)
          : undefined;
      const retryWithRepairedHistory = repairedMessages !== undefined;
      if (repairedMessages) {
// The signature is only valid for the thinking block that generated it. When signature verification 400 is explicitly received,
// Only replace the copy of this request and give a physical request opportunity that does not occupy the ordinary retry budget; it cannot pass
// Fallback attempt reuses requestId, and cannot rewrite canonical history.
        signatureRepairAttempted = true;
        requestMessages = repairedMessages;
        statusContext = {
          ...statusContext,
          maxAttempts: statusMaxAttempts(1),
        };
      }
      const canRetryWithFailurePolicy =
        offPeak?.kind === "queued"
          ? true
          : retryBudgetAllows(retryBudget, retryBudgetAttempt, input.retry.maxAttempts) &&
// Workflow traffic (uncapped budget) reads the policy table instead of the classifier's retryable; bounded budgets are literal unchanged.
            retryAllowedByFailurePolicy(
              failure,
              retryBudget,
              inspectProviderFailure(error).providerErrorCode,
            );
      const canRetry = retryWithRepairedHistory || canRetryWithFailurePolicy;

      if (options) {
        recordGenerateTextDebug({
          modelIoFullRetentionEnabled: input.modelIoFullRetentionEnabled,
          attempt,
          debugDir: input.debugDir,
          error,
          isDev,
          normalizedToolCalls: undefined,
          options,
          recordModelIO,
          request: attemptRequest,
          requestId: statusContext.requestId,
          resolved,
          startedAt,
        });
      }
      await publishModelStatus(
        {
          ...statusContext,
          attempt,
          durationMs: completedAt - startedAt,
          message: failure.message,
          reason: failure.reason,
          requestHeaderCount,
          requestHeaders,
          responseHeaderCount: Object.keys(responseHeaders).length,
          responseHeaders,
          retryable: canRetry,
          statusCode: failure.statusCode,
          ...modelFailureStatusFields(error, failure, options ? "response" : "prepare"),
          timestamp: new Date(completedAt).toISOString(),
          type: "model_request_failed",
        },
        {
          ...statusPublishOptions(input, admission),
          failureError: unwrapRetryError(error),
        },
      );

      if (!canRetry) {
        logRetryDelayDecision({
          attempt,
          canRetry,
          failure,
          logger: input.logger,
          responseHeaders,
          statusContext,
        });
        throw toAdapterError(error, failure, statusContext, attempt, {
          errorPhase: requestInvocationCompleted ? "response" : "prepare",
        });
      }

      if (retryWithRepairedHistory) {
        input.logger?.warn("Retrying model request after thinking signature rejection", {
          attempt,
          event: "model.reasoning_signature_repair.retry",
          maxAttempts: statusContext.maxAttempts,
          nextAttempt: attempt + 1,
          requestId: statusContext.requestId,
          status: "waiting",
        });
        await publishRetryScheduledStatus(
          input,
          statusContext,
          attempt,
          0,
          {
            ...failure,
            retryReason: ModelRetryReason.ReasoningSignatureRepair,
          },
          requestHeaders,
          responseHeaders,
          admission,
        );
        continue;
      }

      const delayMs =
        offPeak?.kind === "queued"
          ? offPeak.delayMs
          : calculateRetryDelay(input.retry, retryBudgetAttempt, failure.retryAfterMs);
      logRetryDelayDecision({
        attempt,
        canRetry,
        delayMs,
        failure,
        logger: input.logger,
        responseHeaders,
        statusContext,
      });

      await publishRetryScheduledStatus(
        input,
        statusContext,
        attempt,
        delayMs,
        failure,
        requestHeaders,
        responseHeaders,
        admission,
      );

//Do not hold a ticket during the retreat period: give the slot to others, try again and gain admission.
      admission.release();
      try {
        await sleep(delayMs, input.request.abortSignal);
      } catch (sleepError) {
        const sleepFailure = classifyModelFailure(sleepError, input.request.abortSignal);
        const sleepResponseHeaders = sanitizeModelNetworkHeaders(
          getResponseHeaders(unwrapRetryError(sleepError)),
        );
        await publishModelStatus(
          {
            ...statusContext,
            attempt,
            message: sleepFailure.message,
            reason: sleepFailure.reason,
            requestHeaderCount,
            requestHeaders,
            responseHeaderCount: Object.keys(sleepResponseHeaders).length,
            responseHeaders: sleepResponseHeaders,
            retryable: false,
            statusCode: sleepFailure.statusCode,
            ...modelFailureStatusFields(sleepError, sleepFailure, "connect"),
            timestamp: new Date().toISOString(),
            type: "model_request_failed",
          },
          {
// The ticket has been returned during the withdrawal period: this cancellation does not belong to any attempt and the ticket will not be transferred.
            ...statusPublishOptions(input),
            failureError: unwrapRetryError(sleepError),
          },
        );
        throw toAdapterError(sleepError, sleepFailure, statusContext, attempt, {
          errorPhase: "connect",
        });
      }
      if (offPeak?.kind === "queued") {
// Waiting in queue does not consume the retry budget: the rollback count allows for to retry in place after incrementing, with unlimited detection.
        attempt -= 1;
      }
    } finally {
      admission.release();
    }
  }

  throw new AiSdkModelAdapterError(
    ModelErrorCode.ModelRequestFailed,
    "Model request failed before an attempt could complete",
    { context: { requestId: baseStatusContext.requestId } },
  );
}

function serializeStructuredOutput(result: unknown): string {
  const output = (result as { output?: unknown }).output;
  if (output === undefined) {
    throw new Error("Structured output is unavailable");
  }
  const serialized = JSON.stringify(output);
  if (serialized === undefined) {
    throw new Error("Structured output is unavailable");
  }
  return serialized;
}

function waitForGenerateTextOrAbort<T>(
  pending: Promise<T>,
  abortSignal: AbortSignal | undefined,
): Promise<T> {
  if (!abortSignal) {
    return pending;
  }

  return new Promise<T>((resolve, reject) => {
    const cleanup = (): void => {
      abortSignal.removeEventListener("abort", onAbort);
    };
    const onAbort = (): void => {
      cleanup();
      reject(
        abortSignal.reason instanceof Error
          ? abortSignal.reason
          : new Error("Model request was cancelled."),
      );
    };

    if (abortSignal.aborted) {
      onAbort();
      return;
    }

    abortSignal.addEventListener("abort", onAbort, { once: true });
    pending.then(
      (value) => {
        cleanup();
        resolve(value);
      },
      (error: unknown) => {
        cleanup();
        reject(error);
      },
    );
  });
}

function statusPublishOptions(
  input: {
    logger?: Logger;
    request: AiSdkModelTextRequest;
    statusSink?: ModelStatusSink;
  },
  admission?: AttemptAdmission,
) {
  return {
    logger: input.logger,
    requestStatusSink: input.request.statusSink,
    statusSink: input.statusSink,
// The admission ticket for this attempt is also its status event sink.
    ...(admission?.ticket === undefined ? {} : { admissionTicket: admission.ticket }),
  };
}

async function publishRetryScheduledStatus(
  input: {
    logger?: Logger;
    request: AiSdkModelTextRequest;
    statusSink?: ModelStatusSink;
  },
  statusContext: ReturnType<typeof createStatusContext>,
  attempt: number,
  delayMs: number,
  failure: ReturnType<typeof classifyModelFailure>,
  requestHeaders: Record<string, string>,
  responseHeaders: Record<string, string>,
  admission?: AttemptAdmission,
): Promise<void> {
  await publishModelStatus(
    {
      ...statusContext,
      attempt,
      delayMs,
      message: failure.message,
      nextAttempt: attempt + 1,
      reason: failure.retryReason,
      requestHeaderCount: Object.keys(requestHeaders).length,
      requestHeaders,
      responseHeaderCount: Object.keys(responseHeaders).length,
      responseHeaders,
      statusCode: failure.statusCode,
      errorCode: failure.code,
      retryAfterMs: failure.retryAfterMs,
      timestamp: new Date().toISOString(),
      type: "model_retry_scheduled",
    },
    statusPublishOptions(input, admission),
  );
}
