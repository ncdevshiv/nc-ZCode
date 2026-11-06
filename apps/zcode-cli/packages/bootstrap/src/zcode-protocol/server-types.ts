import {
  createRootTraceContext,
  createTraceId,
  type Logger,
  type LoggerFactory,
  type McpPort,
  type SessionEventStorePort,
  type SessionId,
  type SessionTaskType,
  type SessionStorePort,
  type TraceContext,
} from "@zcode/contracts";
import type { McpTelemetryTracker } from "@zcode/adapters";
import type { WorkspaceHookPolicyProvider } from "@zcode/core";
import type { AccountProviderConfigSnapshot } from "@zcode/provider";
import {
  zcodeProtocolErrorCodes,
  type ZCodeDeliveryKind,
  type ModelSelection,
  type ZCodeModelContextBudgetStrategy,
  type ZCodeProtocolMessage,
  type ZCodeProtocolMethod,
  type ZCodeProtocolNotification,
  type ZCodeProtocolRequest,
  type ZCodeProtocolRequestId,
  type ZCodeProtocolTrace,
  type ZCodeSessionMode,
  type ZCodeSessionPersistence,
  type ZCodeWorkspaceRef,
} from "@zcode/shared";
import type { ZCodeApp, ZCodeAppOptions } from "../app/types.js";
import type { V4InteractionRegistry } from "../zcode-protocol-v4/interaction-registry.js";
import type { ConversationV4Gateway } from "../zcode-protocol-v4/v4-gateway.js";
import type { SessionResidentPool, SessionResidentPoolOptions } from "./session-resident-pool.js";

export interface ParamsSchema<T> {
  parse(input: unknown): T;
}

export interface ZCodeProtocolAgentDependencies {
  createZCodeApp(options?: Omit<ZCodeAppOptions, "providerRegistry">): ZCodeApp | Promise<ZCodeApp>;
  /**
* In-memory event store factory for each session record.
* The default turn window retention policy; the test can be injected with spy or unbounded for comparison.
   */
  createSessionEventStore?(sessionId: string): SessionEventStorePort;
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  loggerFactory?: LoggerFactory;
  mcpPort?: McpPort;
/** Resource manager: `process/childProcesses` reads the MCP child process pid and plug-in ownership */
  mcpTelemetry?: Pick<McpTelemetryTracker, "listProcesses">;
  platform?: NodeJS.Platform | string;
/** Mixed resident strategy for constructing a single CLI/app-server only; production defaults are defined by pool. */
  sessionResidentPoolOptions?: SessionResidentPoolOptions;
/** Compatible with old test/embed calls; new code should set low-water via sessionResidentPoolOptions. */
  sessionResidentTargetCount?: number;
  sessionStore?: SessionStorePort;
  version?: string;
/** Hook policy managed by trusted Host; workspace/project configuration must not be overridden. */
  workspaceHookPolicyProvider?: WorkspaceHookPolicyProvider;
/** Synchronize the third-layer Config Overlay formed by the Host account status to the process Registry. */
  syncAccountProviderConfig?: (snapshot: AccountProviderConfigSnapshot) => Promise<boolean>;
/** Actively reread the Config Source of the current process before connection testing and wait for the Registry to be released. */
  refreshProviderRegistry?: (reason: string) => Promise<void>;
}

export type ZCodeProtocolAgentResolvedDependencies = ZCodeProtocolAgentDependencies & {
  createSessionEventStore(sessionId: string): SessionEventStorePort;
  workspaceHookPolicyProvider: WorkspaceHookPolicyProvider;
};

export interface ZCodeProtocolEventSequenceState {
  lastSeq: number;
  seqBySourceEventKey: Map<string, number>;
}

export interface ZCodeProtocolToolInputTransmissionState {
  streamedToolCallIdsWithInput: Set<string>;
}

export interface ZCodeProtocolSessionRecord {
  app: ZCodeApp;
  memoryEnabled: boolean;
  nativeSearchEnhancementsEnabled: boolean;
  modelContextBudgetStrategy: ZCodeModelContextBudgetStrategy;
  createdAt: number;
  deliveryKind?: ZCodeDeliveryKind;
  /**
* The old session/subscribe does not have unsubscribe RPC; it is only set when actually subscribe and cannot be used.
* will be replaced by deliveryKind written by session/read. Ending the connection destroys the entire CLI process.
   */
  legacyStreamSubscribed?: boolean;
  eventStore: SessionEventStorePort;
  parentSessionId?: SessionId;
  persistence: ZCodeSessionPersistence;
  protocolEventSequences: Map<string, ZCodeProtocolEventSequenceState>;
  protocolToolInputTransmissions: Map<string, ZCodeProtocolToolInputTransmissionState>;
  stateRevision: number;
  taskType?: SessionTaskType;
  traceContext: TraceContext;
  unsubscribe?: () => void;
  updatedAt: number;
  workspace: ZCodeWorkspaceRef;
  activeAbortController?: AbortController;
/** After the background runner releases the ready lock, persist the unfinished reference count of /snapshot/broadcast. */
  residencyFinalizationCount?: number;
/** The currently executing automation dispatches a turn; it only exists during the running of the turn, and recursive CronCreate is prohibited. */
  activeAutomationId?: string;
/** The currently executing idle-time dispatch turn; only exists during the running of turn, and recursive OffPeakCreate is prohibited. */
  activeOffPeakTaskId?: string;
  restoreWarning?: { message: string; type: string };
/** Cold recovery candidates are only for initial projection; new model selection events are cleared immediately and cannot replace Runtime to perform binding. */
  restoredModelSelection?: ModelSelection;
}

export interface ZCodeProtocolClientRequestOptions {
  signal?: AbortSignal;
  timeoutMs?: number;
  trace?: ZCodeProtocolTrace;
  reannounceIntervalMs?: number;
}

export interface ZCodeProtocolAgentServerContext {
/** During process exit, late asynchronous materialization cannot re-register the session. */
  assertServing?: () => void;
  deps: ZCodeProtocolAgentResolvedDependencies;
  logger?: Logger;
  appRuntimePreferences: {
    askUserQuestionAutoResolutionEnabled: boolean;
    modelIoFullRetentionEnabled: boolean;
/** Host synchronized Off-Peak tool surface access control; default false (fail-closed), for v4 cold recovery and other paths without host parameters to read. */
    offPeakToolEnabled: boolean;
    /**
* Host synchronized dynamic workflow grayscale gate.
* Default false (fail-closed): In the old Host that does not know this method or the startup window that has not yet had time to synchronize,
* The workflow tool panel, `/workflow` and dynamic-workflows skills are not exposed.
     */
    dynamicWorkflowEnabled: boolean;
  };
// Vertical cut: v4 conversation channel (subscription/frame/command), coexisting with the old session/* methods.
// The construction order problem (the gateway closure holds the context) is closed with optional fields, and the server is assigned immediately after it is constructed.
  v4Gateway?: ConversationV4Gateway;
// The meeting point of v4 forward command resolveInteraction and reverse request (permission/AskUserQuestion).
// Broker registers deferred and v4 command plane delivery response (the same instance is injected into V4CommandCoreHost through binder).
  v4Interactions: V4InteractionRegistry;
// Single CLI resident session pool. The cold recovery entry waits for the old app.close to finish via waitForDeactivation.
// The protocol request holds an operation lease, prohibiting asynchronous handlers from interleaving with capacity reclamation.
  sessionResidentPool?: SessionResidentPool;
  sessions: Map<string, ZCodeProtocolSessionRecord>;
  notify(notification: ZCodeProtocolNotification): void;
  requestClient<T>(
    method: ZCodeProtocolMethod,
    params: unknown,
    resultSchema: ParamsSchema<T>,
    options?: ZCodeProtocolClientRequestOptions,
  ): Promise<T>;
}

export class ProtocolRequestError extends Error {
  constructor(
    readonly code: number,
    message: string,
    readonly data?: unknown,
  ) {
    super(message);
    this.name = "ProtocolRequestError";
  }
}

export function isRequest(message: ZCodeProtocolMessage): message is ZCodeProtocolRequest {
  return "method" in message && "id" in message;
}

export function isNotification(
  message: ZCodeProtocolMessage,
): message is ZCodeProtocolNotification {
  return "method" in message && !("id" in message);
}

export function isResponse(
  message: ZCodeProtocolMessage,
): message is { id: ZCodeProtocolRequestId; result: unknown } {
  return "id" in message && "result" in message;
}

export function isErrorResponse(message: ZCodeProtocolMessage): message is {
  id: ZCodeProtocolRequestId;
  error: { code: number; message: string; data?: unknown };
} {
  return "id" in message && "error" in message;
}

/**
* Extract human-readable field-level summaries from zod (or zod-like) validation errors and attach them to "Invalid params" messages.
* Originally, it only returned "Invalid params" without saying which field was wrong. Models (such as browser evaluate misinformation function,
* The coordinates are NaN, etc.) cannot be self-corrected and will try repeatedly. Here we use duck typing to read ZodError.issues (without citing the zod dependency),
* Spelled into `expression: Expected string, received function` such actionable prompts.
 */
function summarizeParamsError(error: unknown): string | undefined {
  const issues = (error as { issues?: Array<{ path?: unknown[]; message?: string }> })?.issues;
  if (!Array.isArray(issues) || issues.length === 0) {
    return undefined;
  }
  const parts = issues.slice(0, 5).map((issue) => {
    const path =
      Array.isArray(issue.path) && issue.path.length > 0 ? issue.path.join(".") : "(root)";
    return `${path}: ${issue.message ?? "invalid"}`;
  });
  const more = issues.length > 5 ? ` (+${issues.length - 5} more)` : "";
  return parts.join("; ") + more;
}

export function parseParams<T>(schema: ParamsSchema<T>, params: unknown): T {
  try {
    return schema.parse(params);
  } catch (error) {
    const detail = summarizeParamsError(error);
    throw new ProtocolRequestError(
      -32602,
      detail ? `Invalid params — ${detail}` : "Invalid params",
      error,
    );
  }
}

export function assertExpectedRevision(
  record: ZCodeProtocolSessionRecord,
  expectedRevision: number | undefined,
): void {
  if (expectedRevision !== undefined && expectedRevision !== record.stateRevision) {
    throw new ProtocolRequestError(-32009, "Session state revision mismatch", {
      actualRevision: record.stateRevision,
      expectedRevision,
    });
  }
}

export function toProtocolError(error: unknown): {
  code: number;
  data?: unknown;
  message: string;
} {
  if (error instanceof ProtocolRequestError) {
    return { code: error.code, data: error.data, message: error.message };
  }
  if (error instanceof Error) {
    const businessCode = "code" in error && typeof error.code === "string" ? error.code : undefined;
    return {
      code: -32603,
// Code for business errors such as ModelProtocolError needs to be retained for the UI across JSON-RPC.
// The outer layer is still JSON-RPC internal error, and the stable business code is placed in data.code for banner localization.
// No transparent transmission error.context: The upstream context does not guarantee JSON-safe, and the error response cannot fail twice due to diagnostic information.
      data: {
        name: error.name,
        stack: error.stack,
        ...(businessCode ? { code: businessCode } : {}),
      },
      message: error.message,
    };
  }
  return {
    code: -32603,
    message: String(error),
  };
}

function createProtocolTraceId(_sessionId: SessionId): TraceContext["traceId"] {
// traceId is the observation link on the session and should not be spelled out by sessionId.
// The UUID algorithm of agent/contracts is reused here to ensure that the trace formats on both ends of the app and agent are consistent.
  return createTraceId();
}

export function createProtocolRootTraceContext(
  sessionId: SessionId,
  trace?: ZCodeProtocolTrace,
): TraceContext {
  const context = createRootTraceContext({
    sessionId,
    traceId: (trace?.traceId ?? createProtocolTraceId(sessionId)) as TraceContext["traceId"],
  });
  if (trace?.spanId) {
    context.spanId = trace.spanId;
  }
  if (trace?.parentId) {
    context.parentId = trace.parentId;
    context.parentSpanId = trace.parentId;
  }
  return context;
}

export function protocolTraceFromTraceContext(context: TraceContext): ZCodeProtocolTrace {
  return {
    traceId: context.traceId,
    ...(context.spanId ? { spanId: context.spanId } : {}),
    ...((context.parentId ?? context.parentSpanId)
      ? { parentId: context.parentId ?? context.parentSpanId }
      : {}),
  };
}

export function requireSession(
  context: ZCodeProtocolAgentServerContext,
  sessionId: string,
  options: { deliveryKind?: ZCodeDeliveryKind; operation?: string } = {},
): ZCodeProtocolSessionRecord {
  const record = context.sessions.get(sessionId);
  if (!record) {
// Diagnosis: readSession only reads the active runtime; when the record is missing, distinguish between "cold session has not been restored" and "ID has expired".
// You can't just leave the same error text, otherwise you can't tell whether the UI read early or the task index brought dirty references.
    context.logger?.warn("ZCode Protocol session runtime missing", {
      activeSessionCount: context.sessions.size,
      event: "zcode_protocol.session.require_missing",
      hasSessionStore: Boolean(context.deps.sessionStore),
      module: "bootstrap.zcode_protocol",
      operation: options.operation ?? "unknown",
      ...(options.deliveryKind ? { deliveryKind: options.deliveryKind } : {}),
      sessionId,
    });
    throw new ProtocolRequestError(
      zcodeProtocolErrorCodes.sessionUnavailable,
      `Session is not active: ${sessionId}`,
    );
  }
  return record;
}

export function createProtocolLogger(deps: ZCodeProtocolAgentDependencies): Logger | undefined {
  return deps.loggerFactory?.createLogger("zcode").child({
    module: "bootstrap.zcode_protocol",
  });
}
