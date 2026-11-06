import { context } from "@opentelemetry/api";
import { AsyncLocalStorageContextManager } from "@opentelemetry/context-async-hooks";
import { OTLPMetricExporter } from "@opentelemetry/exporter-metrics-otlp-proto";
import { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-proto";
import { CompressionAlgorithm } from "@opentelemetry/otlp-exporter-base";
import { resourceFromAttributes } from "@opentelemetry/resources";
import {
  AggregationTemporality,
  AggregationType,
  MeterProvider,
  PeriodicExportingMetricReader,
  type ViewOptions,
} from "@opentelemetry/sdk-metrics";
import {
  BasicTracerProvider,
  BatchSpanProcessor,
  ParentBasedSampler,
  TraceIdRatioBasedSampler,
  type Sampler,
} from "@opentelemetry/sdk-trace-base";
import {
  TELEMETRY_SCHEMA_VERSION,
  type AgentTelemetryRuntimeOwner,
  type TelemetryIdentitySnapshot,
  type TelemetryResourceContext,
} from "@zcode/contracts/telemetry";
import { AgentExecutionTelemetryRuntime } from "./agent-trace-runtime.js";
import { OtelAgentTelemetryMetrics } from "./agent-metrics.js";
import { ModelApiTelemetryStatusSink } from "./model-api-recorder.js";

const AGENT_METRIC_EXPORT_INTERVAL_MS = 300_000;
const AGENT_TRACE_SAMPLE_RATIO = 0.1;

interface CreateOwnedAgentTelemetryRuntimeOptions {
  endpoint: string;
  headers?: Record<string, string>;
  identity?: TelemetryIdentitySnapshot;
  maxBatchSize?: number;
  maxQueueSize?: number;
  metricEndpoint?: string;
  metricExportIntervalMs?: number;
  metricHeaders?: Record<string, string>;
  onWarning?: (message: string, context: Record<string, unknown>) => void;
  resource: TelemetryResourceContext;
  timeoutMs?: number;
  traceSampleRatio?: number;
}

/**
* This module is only dynamically loaded by asynchronous Bootstrap after Telemetry is confirmed to be enabled. Don't statically import from the public entrance,
* Otherwise `--help`, version query and disabled mode will still bear the OTel SDK initialization cost.
 */
export function createOwnedAgentTelemetryRuntime(
  options: CreateOwnedAgentTelemetryRuntimeOptions,
): AgentTelemetryRuntimeOwner {
  const timeoutMs = options.timeoutMs ?? 3_000;
  const exporter = new OTLPTraceExporter({
    compression: CompressionAlgorithm.GZIP,
    headers: options.headers,
    timeoutMillis: timeoutMs,
    url: options.endpoint,
  });
  const processor = new BatchSpanProcessor(exporter, {
    exportTimeoutMillis: timeoutMs,
    maxExportBatchSize: options.maxBatchSize ?? 100,
    maxQueueSize: options.maxQueueSize ?? 2_000,
    scheduledDelayMillis: 5_000,
  });
  const traceResource = resourceFromAttributes(resourceAttributes(options.resource));
  const metricResource = resourceFromAttributes(metricResourceAttributes(options.resource));
  const provider = new BasicTracerProvider({
    forceFlushTimeoutMillis: timeoutMs,
    resource: traceResource,
    sampler: createAgentTraceSampler(options.traceSampleRatio),
    spanProcessors: [processor],
  });
  const metricExporter = new OTLPMetricExporter({
    compression: CompressionAlgorithm.GZIP,
    headers: options.metricHeaders ?? options.headers,
// CLI Metric does not carry per-process instance ID, and DELTA must be used to avoid the accumulation of multiple short-life cycle producers.
// Counter/Histogram reset conflict occurred in the backend.
    temporalityPreference: AggregationTemporality.DELTA,
    timeoutMillis: timeoutMs,
    url: options.metricEndpoint ?? options.endpoint,
  });
  const metricReader = new PeriodicExportingMetricReader({
    exporter: metricExporter,
    exportIntervalMillis: options.metricExportIntervalMs ?? AGENT_METRIC_EXPORT_INTERVAL_MS,
    exportTimeoutMillis: timeoutMs,
  });
  const meterProvider = new MeterProvider({
    readers: [metricReader],
    resource: metricResource,
    views: metricViews(),
  });
  const contextManager = new AsyncLocalStorageContextManager();
  const registered = context.setGlobalContextManager(contextManager.enable());
  if (!registered) {
//Reuse the host instance when the same process host has registered the Context Manager; the instance that has not been successfully registered must be closed.
// Avoid leaving a second set of AsyncLocalStorage.
    contextManager.disable();
  }

  const tracer = provider.getTracer("@zcode/cli-agent-telemetry", String(TELEMETRY_SCHEMA_VERSION));
  const meter = meterProvider.getMeter(
    "@zcode/cli-agent-telemetry",
    String(TELEMETRY_SCHEMA_VERSION),
  );
  const metrics = new OtelAgentTelemetryMetrics(meter);
  const execution = new AgentExecutionTelemetryRuntime({
    identity: options.identity,
    metrics,
    onWarning: options.onWarning,
    tracer,
  });
  const statusSink = new ModelApiTelemetryStatusSink({
    modelExecution: execution,
    onWarning: options.onWarning,
  });
  let closed = false;

  return {
    abandonSession(sessionId) {
      execution.abandonSession(sessionId);
      statusSink.abandonSession(sessionId);
    },
    agentExecution: execution,
    enabled: true,
    modelExecution: execution,
    statusSink,
    async flush(flushOptions) {
      if (closed) return;
      await withTimeout(
        Promise.all([provider.forceFlush(), meterProvider.forceFlush()]),
        flushOptions?.timeoutMs ?? timeoutMs,
      );
    },
    async shutdown(shutdownOptions) {
      if (closed) return;
      closed = true;
      statusSink.shutdown();
      execution.abandonProcess();
      try {
        await withTimeout(
          Promise.all([provider.shutdown(), meterProvider.shutdown()]),
          shutdownOptions?.timeoutMs ?? timeoutMs,
        );
      } finally {
        if (registered) contextManager.disable();
      }
    },
    updateIdentity(snapshot) {
      execution.updateIdentity(snapshot);
    },
  };
}

function createAgentTraceSampler(ratio = AGENT_TRACE_SAMPLE_RATIO): Sampler {
  return new ParentBasedSampler({
    root: new TraceIdRatioBasedSampler(ratio),
  });
}

function metricViews(): ViewOptions[] {
  return [
    histogramView("zcode.agent.turn.duration", [1, 5, 10, 30, 60, 300, 600]),
    histogramView("zcode.model.*.duration", [0.25, 1, 2, 5, 10, 30, 60, 120]),
    histogramView(
      "zcode.context.compaction.duration",
      [0.01, 0.05, 0.1, 0.5, 1, 5, 30, 120, 300, 600],
    ),
    histogramView(
      "zcode.detached.operation.duration",
      [0.01, 0.05, 0.1, 0.5, 1, 5, 30, 120, 300, 600],
    ),
    histogramView("zcode.agent.step.duration", [0.01, 0.05, 0.1, 0.5, 1, 5, 30, 120, 300, 600]),
    histogramView("zcode.tool.execution.duration", [0.01, 0.1, 0.5, 1, 5, 30, 120, 300]),
    histogramView(
      "zcode.command.execution.duration",
      [0.01, 0.05, 0.1, 0.5, 1, 5, 30, 120, 300, 600],
    ),
    histogramView("zcode.*.time_to_first_*", [0.25, 0.5, 1, 2, 5, 10, 30, 60]),
    histogramView("zcode.model.call.attempts", [1, 2, 3, 5, 8]),
    cardinalityView("zcode.model.attempt.tokens"),
    cardinalityView("zcode.model.attempt.stream_stall.count"),
    cardinalityView("zcode.telemetry.creation_drop.count"),
    cardinalityView("zcode.telemetry.abandoned.count"),
  ];
}

function histogramView(instrumentName: string, boundaries: number[]): ViewOptions {
  return {
    aggregation: {
      options: { boundaries, recordMinMax: false },
      type: AggregationType.EXPLICIT_BUCKET_HISTOGRAM,
    },
    aggregationCardinalityLimit: 250,
    instrumentName,
  };
}

function cardinalityView(instrumentName: string): ViewOptions {
  return {
    aggregationCardinalityLimit: 250,
    instrumentName,
  };
}

function resourceAttributes(
  resource: TelemetryResourceContext,
): Record<string, string | number | boolean> {
  return telemetryResourceAttributes(resource, {
    includeInstallationId: true,
    includeServiceInstanceId: true,
  });
}

function metricResourceAttributes(
  resource: TelemetryResourceContext,
): Record<string, string | number | boolean> {
  return telemetryResourceAttributes(resource, {
    includeInstallationId: false,
    includeServiceInstanceId: false,
  });
}

function telemetryResourceAttributes(
  resource: TelemetryResourceContext,
  options: {
    includeInstallationId: boolean;
    includeServiceInstanceId: boolean;
  },
): Record<string, string | number | boolean> {
  return compactResourceAttributes({
    "deployment.environment.name": resource.deploymentEnvironment,
    "host.arch": hostArchitecture(),
    "os.type": process.platform,
    "process.runtime.name": "nodejs",
    "process.runtime.version": process.version.replace(/^v/u, ""),
// Metric Resource also participates in time series Series identity. The instance ID and installation ID generated each time the CLI is launched can only
// Belongs to Trace; if it enters Metric, unbounded cardinality will be created among all user processes.
    "service.instance.id": options.includeServiceInstanceId
      ? resource.serviceInstanceId
      : undefined,
    "service.name": resource.serviceName,
    "service.version": resource.cliVersion,
    "zcode.build.commit_id": resource.buildCommitId,
    "zcode.device.installation_id": options.includeInstallationId
      ? resource.installationId
      : undefined,
    "zcode.product.version": resource.productVersion,
    "zcode.runtime.distribution": resource.runtimeDistribution,
    "zcode.runtime.surface": resource.runtimeSurface,
    "zcode.telemetry.schema_owner": "cli",
    "zcode.telemetry.schema_version": TELEMETRY_SCHEMA_VERSION,
  });
}

function hostArchitecture(): string {
  switch (process.arch) {
    case "x64":
      return "amd64";
    case "ia32":
      return "x86";
    default:
      return process.arch;
  }
}

function compactResourceAttributes(
  attributes: Record<string, string | number | boolean | undefined>,
): Record<string, string | number | boolean> {
  const compacted: Record<string, string | number | boolean> = {};
  for (const [key, value] of Object.entries(attributes)) {
    if (value !== undefined && value !== "") compacted[key] = value;
  }
  return compacted;
}

async function withTimeout<T>(promise: Promise<T>, timeoutMs: number): Promise<T | undefined> {
  let timeout: number | undefined;
  try {
    return await Promise.race([
      promise,
      new Promise<undefined>((resolve) => {
        timeout = setTimeout(resolve, Math.max(1, timeoutMs));
      }),
    ]);
  } finally {
    if (timeout) clearTimeout(timeout);
  }
}
