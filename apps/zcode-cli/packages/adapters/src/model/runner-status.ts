import type {
  Logger,
  ModelNetworkStatusEvent,
  ModelReasoningCallHint,
  ModelStatusSink,
  ModelTransportKind,
  ModelRequestSessionType as ModelRequestSessionTypeValue,
  QueryId,
  ResolvedModelApiCallObservation,
  TraceId,
} from "@zcode/contracts";
import {
  ModelFailureReason as ModelFailureReasonValue,
  createTraceId,
  resolveModelApiCallObservation,
} from "@zcode/contracts";
import { UNBOUNDED_RETRY_MAX_ATTEMPTS } from "./retry-budget.js";
// Request attribution header family lives in runner-attribution.ts (max-lines split); the public side is still exported from this file,
// There is no need to change the path of the existing importer.
import { resolveModelRequestSessionType } from "./runner-attribution.js";
import type { AiSdkModelTextRequest, ResolvedAiSdkModel } from "./runner-runtime.js";
import { stringMetadata } from "./runner-record.js";

export {
  createModelRequestAttributionHeaders,
  normalizeModelSessionIdForAttribution,
  resolveModelRequestSessionType,
} from "./runner-attribution.js";

export interface ModelStatusContext {
  traceId: TraceId;
  queryId?: QueryId;
  sessionId?: ModelNetworkStatusEvent["sessionId"];
  turnId?: ModelNetworkStatusEvent["turnId"];
  parentSessionId?: ModelNetworkStatusEvent["parentSessionId"];
  toolCallId?: string;
  spanId?: string;
  parentSpanId?: string;
  querySource?: string;
  requestId: string;
  providerId: ResolvedAiSdkModel["providerId"];
  modelId: ResolvedAiSdkModel["modelId"];
  modelRequestSessionType: ModelRequestSessionTypeValue;
  baseURL?: string;
  providerKind?: string;
  transport: ModelTransportKind;
  maxAttempts: number;
  streamRecovery?: ModelNetworkStatusEvent["streamRecovery"];
  modelCall: ResolvedModelApiCallObservation;
}

export function createStatusContext(input: {
  maxAttempts: number;
  request: AiSdkModelTextRequest;
  resolved: ResolvedAiSdkModel;
  transport: ModelTransportKind;
}): ModelStatusContext {
  const metadata = input.request.metadata ?? {};
  const querySource = stringMetadata(metadata.querySource);
  const resolvedModelCall = resolveModelApiCallObservation(querySource, input.request.modelCall);
  return {
    baseURL: input.resolved.baseURL,
    maxAttempts: input.maxAttempts,
    providerId: input.resolved.providerId,
    modelId: input.resolved.modelId,
    modelRequestSessionType: resolveModelRequestSessionType(
      input.request.modelRequestSessionType,
      resolvedModelCall,
    ),
    requestId: stringMetadata(metadata.requestId) ?? crypto.randomUUID(),
    sessionId: (input.request.traceContext?.sessionId ??
      stringMetadata(metadata.sessionId)) as ModelStatusContext["sessionId"],
    spanId: input.request.traceContext?.spanId ?? stringMetadata(metadata.spanId),
    parentSpanId:
      input.request.traceContext?.parentSpanId ??
      stringMetadata(metadata.parentSpanId) ??
      stringMetadata(metadata.parentId),
    querySource,
    queryId: (input.request.traceContext?.queryId ?? stringMetadata(metadata.queryId)) as
      | QueryId
      | undefined,
    traceId: (input.request.traceContext?.traceId ??
      stringMetadata(metadata.traceId) ??
      createTraceId()) as TraceId,
    providerKind: input.resolved.providerKind,
    streamRecovery: input.request.streamRecovery,
    transport: input.transport,
    turnId: (input.request.traceContext?.turnId ??
      stringMetadata(metadata.turnId)) as ModelStatusContext["turnId"],
    parentSessionId: stringMetadata(metadata.parentSessionId) as
      | ModelStatusContext["parentSessionId"]
      | undefined,
    toolCallId: stringMetadata(metadata.toolCallId),
    modelCall: {
      ...resolvedModelCall,
      agentName: resolvedModelCall.agentName ?? stringMetadata(metadata.agentName),
      operationId:
        resolvedModelCall.operationId ??
        input.request.traceContext?.spanId ??
        stringMetadata(metadata.spanId),
      stepIndex:
        resolvedModelCall.stepIndex ??
        (typeof metadata.stepIndex === "number"
          ? metadata.stepIndex
          : typeof metadata.iteration === "number"
            ? metadata.iteration
            : undefined),
      reasoning: initialReasoningObservation(resolvedModelCall.reasoning, undefined),
    },
  };
}

export function createAttemptStatusContext(
  statusContext: ModelStatusContext,
  attempt: number,
): ModelStatusContext {
  if (attempt <= 1) {
    return statusContext;
  }

  return {
    ...statusContext,
// adapter retry will initiate a new physical provider request;
// Reusing the first round of requestId will allow upstream logs and retry-after to diagnose mismatched requests.
    requestId: crypto.randomUUID(),
  };
}

function initialReasoningObservation(
  hint: ModelReasoningCallHint | undefined,
  modelVariant: string | undefined,
): ResolvedModelApiCallObservation["reasoning"] {
  const explicit = hint?.explicit;
  return {
    capability: "unknown",
    requestedControl: explicit?.controlType ?? "provider_default",
    requestedState: explicit?.state ?? "provider_default",
    requestedLevel: hint?.requestedLevel ?? modelVariant,
    effectiveControl: explicit?.controlType ?? "unknown",
    effectiveState: explicit?.state ?? "unknown",
    effectiveLevel:
      explicit?.effectiveLevel ?? (explicit?.state === "disabled" ? "disabled" : "unknown"),
    ...(explicit?.effectiveBudgetTokens !== undefined
      ? { effectiveBudgetTokens: explicit.effectiveBudgetTokens }
      : {}),
  };
}

/**
* Status events at both ends of admission waiting: `tryAcquire` of `admitAttempt` is `queued` if it misses, and `queued` is when the ticket is obtained.
* `admitted` (with queue length). There are no tickets yet, so no ticket delivery is required - the manager does not need these two items.
 */
export function admissionWaitPublishers(
  statusContext: ModelStatusContext,
  attempt: number,
  publishOptions: Parameters<typeof publishModelStatus>[1],
): {
  onQueued: () => Promise<void>;
  onAdmitted: (queuedMs: number) => Promise<void>;
} {
  return {
    onQueued: () =>
      publishModelStatus(
        {
          ...statusContext,
          attempt,
          timestamp: new Date().toISOString(),
          type: "model_request_queued",
        },
        publishOptions,
      ),
    onAdmitted: (queuedMs) =>
      publishModelStatus(
        {
          ...statusContext,
          attempt,
          queuedMs,
          timestamp: new Date().toISOString(),
          type: "model_request_admitted",
        },
        publishOptions,
      ),
  };
}

export async function publishModelStatus(
  event: ModelNetworkStatusEvent,
  options: {
    /**
* The admission ticket for this attempt: it is unique to this attempt
* Status event sink, the manager reads the results from here. The same delivery discipline as request/telemetry sink (only alerts on failure).
     */
    admissionTicket?: ModelStatusSink;
    failureError?: unknown;
    logger?: Logger;
    requestStatusSink?: ModelStatusSink;
    statusSink?: ModelStatusSink;
  },
): Promise<void> {
  logStatusEvent(event, options.logger);

  const deliveries: Array<() => void | Promise<void>> = [];
  if (options.admissionTicket) {
    const ticket = options.admissionTicket;
    deliveries.push(() => ticket.publish(event));
  }
  if (options.statusSink) {
    const statusSink = options.statusSink;
    if (options.requestStatusSink && options.requestStatusSink !== options.statusSink) {
      deliveries.push(() => options.requestStatusSink!.publish(event));
    }
    deliveries.push(() =>
      event.type === "model_request_failed" &&
      options.failureError !== undefined &&
      statusSink.publishFailure
        ? statusSink.publishFailure(event, options.failureError)
        : statusSink.publish(event),
    );
  } else if (options.requestStatusSink) {
    deliveries.push(() => options.requestStatusSink!.publish(event));
  }
  if (deliveries.length === 0) return;

  const results = await Promise.allSettled(
    deliveries.map((deliver) => Promise.resolve().then(deliver)),
  );
  for (const result of results) {
    if (result.status === "rejected") {
      options.logger?.warn("Model status sink failed", {
        ...modelStatusLogContext(event),
        errorMessage:
          result.reason instanceof Error ? result.reason.message : String(result.reason),
        event: "model.status_sink.failed",
        status: "failed",
      });
    }
  }
}

/**
* Provider streaming milestones are only sent to process-level Telemetry Sink and do not write SessionEvents.
* requestStatusSink belongs to the dialogue product status and cannot be contaminated by pure observation events.
 */
export async function publishModelTelemetryMilestone(
  event: Extract<
    ModelNetworkStatusEvent,
    {
      type: "model_first_provider_event" | "model_first_content" | "model_first_text";
    }
  >,
  options: {
    logger?: Logger;
    statusSink?: ModelStatusSink;
  },
): Promise<void> {
  logStatusEvent(event, options.logger);
  if (!options.statusSink) return;
  try {
    await options.statusSink.publish(event);
  } catch (error) {
    options.logger?.warn("Model telemetry milestone sink failed", {
      ...modelStatusLogContext(event),
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "model.telemetry_milestone.failed",
      status: "failed",
    });
  }
}

export function modelStatusContextToLogContext(
  statusContext: ModelStatusContext | ModelNetworkStatusEvent,
  attempt: number,
): Record<string, unknown> {
  return {
    attempt,
    baseURL: statusContext.baseURL,
    maxAttempts: statusContext.maxAttempts,
// maxAttempts is 0, which means there is no upper limit: maxRetries is also expressed as 0, not the 0 obtained by max(0, -1) by coincidence.
    maxRetries:
      statusContext.maxAttempts === UNBOUNDED_RETRY_MAX_ATTEMPTS
        ? UNBOUNDED_RETRY_MAX_ATTEMPTS
        : Math.max(0, statusContext.maxAttempts - 1),
    modelId: statusContext.modelId,
    parentSpanId: statusContext.parentSpanId,
    providerId: statusContext.providerId,
    providerKind: statusContext.providerKind,
    queryId: statusContext.queryId,
    querySource: statusContext.querySource,
    requestId: statusContext.requestId,
    sessionId: statusContext.sessionId,
    spanId: statusContext.spanId,
    traceId: statusContext.traceId,
    transport: statusContext.transport,
    turnId: statusContext.turnId,
  };
}

function modelStatusLogContext(event: ModelNetworkStatusEvent): Record<string, unknown> {
  return modelStatusContextToLogContext(event, event.attempt);
}

function logStatusEvent(event: ModelNetworkStatusEvent, logger?: Logger): void {
  switch (event.type) {
    case "model_request_queued":
      logger?.debug("Model request queued for admission", {
        ...modelStatusLogContext(event),
        event: "model.request.queued",
        status: "waiting",
      });
      return;

    case "model_request_admitted":
      logger?.debug("Model request admitted", {
        ...modelStatusLogContext(event),
        event: "model.request.admitted",
        queuedMs: event.queuedMs,
        status: "started",
      });
      return;

    case "model_request_started":
      logger?.debug("Model request attempt started", {
        ...modelStatusLogContext(event),
        event: "model.request.started",
        status: "started",
      });
      return;

    case "model_request_completed":
      logger?.info("Model request completed", {
        ...modelStatusLogContext(event),
        durationMs: event.durationMs,
        event: "model.request.completed",
        finishReason: event.finishReason,
        status: "completed",
      });
      return;

    case "model_retry_scheduled":
      logger?.warn("Model request retry scheduled", {
        ...modelStatusLogContext(event),
        delayMs: event.delayMs,
        event: "model.retry.scheduled",
        nextAttempt: event.nextAttempt,
        reason: event.reason,
        status: "waiting",
        statusCode: event.statusCode,
        statusMessage: event.message,
      });
      return;

    case "model_request_failed":
      logger?.warn("Model request attempt failed", {
        ...modelStatusLogContext(event),
        durationMs: event.durationMs,
        event: "model.request.failed",
        reason: event.reason,
        retryable: event.retryable,
        status: event.reason === ModelFailureReasonValue.Cancelled ? "cancelled" : "failed",
        statusCode: event.statusCode,
        statusMessage: event.message,
      });
      return;

    case "model_stream_stalled":
      logger?.warn("Model stream stalled", {
        ...modelStatusLogContext(event),
        event: "model.stream.stalled",
        idleMs: event.idleMs,
        status: "waiting",
        statusMessage: event.message,
        timeoutMs: event.timeoutMs,
      });
      return;

    case "model_first_provider_event":
    case "model_first_content":
    case "model_first_text":
      logger?.debug("Model telemetry milestone observed", {
        ...modelStatusLogContext(event),
        elapsedMs: event.elapsedMs,
        event: event.type,
        status: "completed",
      });
      return;
  }
}
