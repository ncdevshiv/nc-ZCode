/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { querySessionDebug } from "./session-debug.js";
import {
  zcodePluginsCancelOperationParamsSchema,
  zcodeProtocolMethods,
  zcodeWorkspaceCancelGenerateTextParamsSchema,
  zcodeWorkspaceHookTrustGrantParamsSchema,
} from "@zcode/shared";
import type { BrowserControlPort } from "@zcode/contracts";
import { InMemoryWorkspaceHookPolicyProvider } from "@zcode/core";
import {
  V4_METHODS,
  V4_NOTIFICATIONS,
  parseConversationTopic,
  parseSessionsIndexTopic,
  parseWorkspaceConfigTopic,
} from "@zcode/shared/zcode-protocol-v4";
import type {
  ZCodeProtocolError,
  ZCodeProtocolMessage,
  ZCodeProtocolMethod,
  ZCodeProtocolNotification,
  ZCodeProtocolRequest,
  ZCodeProtocolRequestId,
  ZCodeProtocolResponse,
} from "@zcode/shared";
import {
  cancelBackgroundTask,
  closeSession,
  compactSession,
  createSession,
  forkSession,
  generateWorkspaceText,
  goalSession,
  getTaskTokenUsage,
  getUsageStats,
  listSessions,
  listSessionSubagents,
  readEvents,
  readMessages,
  readSession,
  resumeSession,
  sendPrompt,
  setMode,
  setModel,
  setThoughtLevel,
  stopSession,
  subscribeSession,
} from "./server-operations.js";
import { listChildProcesses } from "./process-child-processes.js";
import { ProtocolRuntimeResources } from "./runtime-resources.js";
import {
  readWorkspacePresentation,
  testProviderModelConnectivity,
} from "./workspace-model-runtime.js";
import {
  addPluginMarketplace,
  configurePlugin,
  describePlugin,
  getPluginsOverview,
  installPlugin,
  listPlugins,
  removePluginMarketplace,
  resetPluginConfig,
  restoreBuiltinPlugin,
  setPluginEnabled,
  uninstallPlugin,
  updatePlugin,
  updatePluginMarketplace,
  validatePlugin,
} from "./plugins.js";
import {
  getPluginReferenceCatalog,
  resolveSuggestedPluginReference,
} from "./plugin-reference-catalog.js";
import { getSkillReferenceCatalog } from "./skill-reference-catalog.js";
import {
  deleteSavedWorkflowOp,
  getSavedWorkflowOp,
  listSavedWorkflowRunsOp,
  listSavedWorkflowsOp,
  moveSavedWorkflowOp,
  updateSavedWorkflowMetaOp,
} from "./saved-workflows.js";
import { listMcpServers } from "./mcp.js";
import { updateInteractionPreferences } from "./interaction-preferences.js";
import { updateAccountProviderConfig } from "./account-provider-config.js";
import { updateModelIoPreferences } from "./model-io-preferences.js";
import { updateOffPeakToolPolicy } from "./off-peak-tool-policy.js";
import { updateDynamicWorkflowPolicy } from "./dynamic-workflow-policy.js";
import { grantWorkspaceHookTrustForProtocol } from "./workspace-hook-trust.js";
import {
  V4InteractionRegistry,
  resolveV4InteractionRegistryOptionsFromEnv,
} from "../zcode-protocol-v4/interaction-registry.js";
import { createConversationV4Gateway } from "./v4-bridge.js";
import { createSessionResidentPoolHost } from "./session-residency.js";
import {
  DEFAULT_SESSION_RESIDENT_HIGH_WATER_COUNT,
  SessionResidentPool,
} from "./session-resident-pool.js";
import { createProtocolBrowserControlBroker } from "./browser-control-broker.js";
import {
  createProtocolLogger,
  isErrorResponse,
  isNotification,
  isRequest,
  isResponse,
  ProtocolRequestError,
  type ParamsSchema,
  parseParams,
  toProtocolError,
  type ZCodeProtocolClientRequestOptions,
  type ZCodeProtocolAgentDependencies,
  type ZCodeProtocolAgentServerContext,
  type ZCodeProtocolSessionRecord,
} from "./server-types.js";
import { createInMemorySessionEventStore } from "@zcode/contracts";

export type { ZCodeProtocolAgentDependencies, ZCodeProtocolSessionRecord };

const MAX_CLIENT_REQUEST_REANNOUNCE_INTERVAL_MS = 10_000;

type ZCodeProtocolOutboundMessage = ZCodeProtocolNotification | ZCodeProtocolRequest;

/**
* The coordinator of each session after the Trust store is placed
* The memory image (loaded only when created) will not be automatically updated, and trusted Hooks will continue to be rejected and banner pendingCount
* Stay at old value. After successful pretrust authorization, press workspaceKey to notify all matching active sessions to reload.
* Independently exported as a pure scheduling function (no network contact, no events), which facilitates the direct construction of sessions Map for regression testing.
 */
async function notifyWorkspaceHookTrustGrantSessions(input: {
  grantedWorkspaceKey?: string;
  sessions: Map<string, ZCodeProtocolSessionRecord>;
}): Promise<void> {
  if (!input.grantedWorkspaceKey) return;
  await Promise.all(
    [...input.sessions.values()]
      .filter((record) => record.workspace.workspaceKey === input.grantedWorkspaceKey)
      .map((record) => record.app.reloadWorkspaceHookTrust()),
  );
}

function collectResidencySessionIds(params: unknown): string[] {
  if (!params || typeof params !== "object") return [];
  const candidate = params as {
    commands?: unknown;
    sessionId?: unknown;
    topic?: unknown;
  };
  const sessionIds = new Set<string>();
  if (typeof candidate.sessionId === "string" && candidate.sessionId.length > 0) {
    sessionIds.add(candidate.sessionId);
  }
  if (typeof candidate.topic === "string") {
    const topicSessionId = parseConversationTopic(candidate.topic);
    if (topicSessionId) sessionIds.add(topicSessionId);
  }
  if (Array.isArray(candidate.commands)) {
    for (const command of candidate.commands) {
      if (!command || typeof command !== "object") continue;
      const sessionId = (command as { sessionId?: unknown }).sessionId;
      if (typeof sessionId === "string" && sessionId.length > 0) {
        sessionIds.add(sessionId);
      }
    }
  }
  return [...sessionIds];
}

function getPluginOperationId(params: unknown): string | undefined {
  if (!params || typeof params !== "object") return undefined;
  const operationId = (params as { operationId?: unknown }).operationId;
  return typeof operationId === "string" && operationId.trim().length > 0
    ? operationId.trim()
    : undefined;
}

function getOperationId(params: unknown): string | undefined {
  if (!params || typeof params !== "object") return undefined;
  const operationId = (params as { operationId?: unknown }).operationId;
  return typeof operationId === "string" && operationId.trim().length > 0
    ? operationId.trim()
    : undefined;
}

interface ZCodeProtocolPostResponseBatch {
  readonly messages: readonly ZCodeProtocolOutboundMessage[];
  commit(): boolean;
}

interface PendingClientRequest<T> {
  method: string;
  reject: (error: Error) => void;
  resolve: (value: T) => void;
  resultSchema: ParamsSchema<T>;
  requestKeys: Set<string>;
  signal?: AbortSignal;
  timeout?: ReturnType<typeof setTimeout>;
  reannounceTimer?: ReturnType<typeof setTimeout>;
  abortHandler?: () => void;
}

export class ZCodeProtocolAgentServer {
  private readonly runtimeResources: ProtocolRuntimeResources;
  private shutdownPromise?: Promise<void>;
  readonly browserControlPort: BrowserControlPort;
  /**
* Minimal context required for the official MCP identity header port.
* The MCP connection pool was constructed earlier than the server, and the reference held by the closure needs to be backfilled after the server is ready——
* The same construction sequence closing method as v4Gateway. Only the requestClient is exposed, not the entire context.
   */
  get officialMcpAuthRequestContext(): Pick<ZCodeProtocolAgentServerContext, "requestClient"> {
    return this.context;
  }

  private messageSink?: (message: ZCodeProtocolOutboundMessage) => void;
  private clientDisconnectError?: Error;
  private readonly context: ZCodeProtocolAgentServerContext;
  private readonly logger;
  private readonly pendingClientRequests = new Map<string, PendingClientRequest<unknown>>();
  private readonly pluginOperationControllers = new Map<string, AbortController>();
  private readonly workspaceGenerateTextControllers = new Map<string, AbortController>();
  /**
* The subscribe initial frame is isolated by JSON-RPC request id. connection must be taken first,
* Then write the response line, and finally write the notification in array order. You cannot rely on microtask to guess the timing.
   */
  private readonly postResponseOutbox = new Map<
    ZCodeProtocolRequestId,
    ZCodeProtocolPostResponseBatch
  >();
  private nextClientRequestId = 1;

  constructor(deps: ZCodeProtocolAgentDependencies) {
    this.runtimeResources = new ProtocolRuntimeResources(deps.createZCodeApp);
    const resolvedDeps = {
      ...deps,
      createZCodeApp: this.runtimeResources.create,
//Default turn window retention policy.
      createSessionEventStore:
        deps.createSessionEventStore ?? (() => createInMemorySessionEventStore()),
      workspaceHookPolicyProvider:
        deps.workspaceHookPolicyProvider ?? new InMemoryWorkspaceHookPolicyProvider(),
    };
    this.logger = createProtocolLogger(resolvedDeps);
    this.context = {
      assertServing: () => this.runtimeResources.assertServing(),
      deps: resolvedDeps,
      logger: this.logger,
      appRuntimePreferences: {
        askUserQuestionAutoResolutionEnabled: true,
        modelIoFullRetentionEnabled: false,
        offPeakToolEnabled: false,
//Dynamic workflow grayscale gate fail-closed: Host must explicitly workspace/updateDynamicWorkflowPolicy
// Just turn it on.
        dynamicWorkflowEnabled: false,
      },
      notify: (notification) => this.messageSink?.(notification),
      requestClient: (method, params, resultSchema, options) =>
        this.requestClient(method, params, resultSchema, options),
      sessions: new Map<string, ZCodeProtocolSessionRecord>(),
//Interaction response registration table (the meeting point of broker reverse request × v4 resolveInteraction command).
      v4Interactions: new V4InteractionRegistry(
        resolveV4InteractionRegistryOptionsFromEnv(deps.env ?? process.env),
      ),
    };
// v4 channel: gateway closure holds the context for frame export and command side effects, and hangs back immediately after construction.
    this.context.v4Gateway = createConversationV4Gateway(this.context);
    this.browserControlPort = createProtocolBrowserControlBroker(this.context);
    const sessionResidentTargetCount =
      deps.sessionResidentPoolOptions?.targetCount ?? deps.sessionResidentTargetCount;
    const sessionResidentHighWaterCount =
      deps.sessionResidentPoolOptions?.highWaterCount ??
      (sessionResidentTargetCount === undefined
        ? undefined
        : Math.max(DEFAULT_SESSION_RESIDENT_HIGH_WATER_COUNT, sessionResidentTargetCount));
//Single CLI resident session pool: protocol request release actively converges, and the resource sampler is only for back-up.
    this.context.sessionResidentPool = new SessionResidentPool(
      createSessionResidentPoolHost(this.context),
      {
        ...deps.sessionResidentPoolOptions,
// The legacy target once covered high/low at the same time, causing the hysteresis window to collapse to 0; only low was covered.
// Only configure the target and raise the implicit high when it exceeds the default high. Explicit illegal combinations are still rejected by the pool.
        highWaterCount: sessionResidentHighWaterCount,
        targetCount: sessionResidentTargetCount,
      },
    );
  }

/** Low-frequency sampler back-up entry; normal convergence is triggered by the release of the operation lease of each protocol request. */
  rebalanceResidentSessions(): void {
    this.context.sessionResidentPool?.rebalance();
  }

  /**
* Borrow the same 60s beat to do the event store time and eliminate it completely:
* The subagent sub-session has only one turn. It cannot wait for the next turn_started and can only be cleared according to time. Returns the number of eliminated items.
   */
  pruneSessionEventStores(nowMs: number = Date.now()): number {
    let evicted = 0;
    for (const record of this.context.sessions.values()) {
      evicted += record.eventStore.pruneTransientEvents?.(nowMs) ?? 0;
    }
    return evicted;
  }

/** The same 60s beat: release the detached subagent child publisher that is finalized, has no subscribers, and has no records. */
  pruneDetachedChildPublishers(nowMs: number = Date.now()): number {
    return this.context.v4Gateway?.pruneDetachedChildPublishers(nowMs) ?? 0;
  }

  /**
* Memory diagnostic counter, write local log with 60s resource sampling.
* Read-only Map.size / array length, does not touch session status; persistent event store does not provide getStats timer 0.
   */
  collectMemoryDiagnostics(): Record<string, number> {
    let eventRows = 0;
    let eventEvicted = 0;
    let eventTransientRetained = 0;
    for (const record of this.context.sessions.values()) {
      const stats = record.eventStore.getStats?.();
      eventRows += stats?.events ?? 0;
      eventEvicted += stats?.evictedEvents ?? 0;
      eventTransientRetained += stats?.retainedTransient ?? 0;
    }
    const counters: Record<string, number> = {
      sessions: this.context.sessions.size,
      eventRows,
      eventEvicted,
      eventTransientRetained,
    };
    const v4 = this.context.v4Gateway?.collectMemoryDiagnostics();
    if (v4) {
      for (const [key, value] of Object.entries(v4)) {
        counters[`v4.${key}`] = value;
      }
    }
    return counters;
  }

  setNotificationSink(sink: (message: ZCodeProtocolOutboundMessage) => void): void {
    this.runtimeResources.assertServing();
    this.clientDisconnectError = undefined;
    this.messageSink = sink;
  }

  disconnectClient(error: Error): void {
    this.clientDisconnectError = error;
// After the connection is closed, it is impossible to receive a response for the reverse request, and the pending must be ended first.
// Otherwise, the handler that is materializing the Session will block the connection's closing process.
    const pendingRequests = new Set(this.pendingClientRequests.values());
    for (const pending of pendingRequests) {
      this.cleanupClientRequest(pending);
      pending.reject(error);
    }
  }

/** Process resources are closed, if session/close is not used it will delete the product session/release session.removed. */
  shutdown(): Promise<void> {
    if (this.shutdownPromise) return this.shutdownPromise;
    this.shutdownPromise = this.runtimeResources.close();
    const error = new Error("ZCode Protocol runtime stopping");
    this.disconnectClient(error);
    this.messageSink = undefined;
    this.clearPostResponseMessages();
    for (const controller of this.pluginOperationControllers.values()) controller.abort(error);
    for (const controller of this.workspaceGenerateTextControllers.values())
      controller.abort(error);
    for (const record of this.context.sessions.values()) {
      record.activeAbortController?.abort(error);
      try {
        record.unsubscribe?.();
      } catch {
        this.logger?.warn("Session unsubscribe failed during protocol shutdown", {
          event: "zcode_protocol.session.unsubscribe.failed",
        });
      }
    }
    return this.shutdownPromise;
  }

/** Release the shadow after app drain is bounded; must be executed even if an app.close is pending. */
  disposeProjections(): void {
    this.context.v4Gateway?.dispose();
    this.context.sessions.clear();
  }

/** Take the post-response messages of a request at once; repeated take returns an empty array. */
  takePostResponseMessages(requestId: ZCodeProtocolRequestId): ZCodeProtocolOutboundMessage[] {
    const batch = this.takePostResponseBatch(requestId);
    batch?.commit();
    return [...(batch?.messages ?? [])];
  }

/** production NDJSON takes the complete batch; commit is called only after all writes are successful. */
  takePostResponseBatch(requestId: ZCodeProtocolRequestId): ZCodeProtocolPostResponseBatch | null {
    const batch = this.postResponseOutbox.get(requestId) ?? null;
    this.postResponseOutbox.delete(requestId);
    return batch;
  }

/** Release the unwritten initial frame reference when connection close / server dispose. */
  clearPostResponseMessages(): void {
    this.postResponseOutbox.clear();
  }

  async handleMessage(
    message: ZCodeProtocolMessage,
  ): Promise<ZCodeProtocolError | ZCodeProtocolResponse | undefined> {
    this.runtimeResources.assertServing();
    if (isResponse(message)) {
      this.resolveClientRequest(message.id, message.result);
      return undefined;
    }
    if (isErrorResponse(message)) {
      this.rejectClientRequest(
        message.id,
        new ProtocolRequestError(message.error.code, message.error.message, message.error.data),
      );
      return undefined;
    }
    if (isRequest(message)) {
      return await this.handleRequest(message);
    }
    if (isNotification(message)) {
      this.logger?.debug("ZCode Protocol notification ignored", {
        event: "zcode_protocol.notification.ignored",
        method: message.method,
        module: "bootstrap.zcode_protocol",
      });
    }
    return undefined;
  }

  private async handleRequest(
    request: ZCodeProtocolRequest,
  ): Promise<ZCodeProtocolError | ZCodeProtocolResponse> {
// The request id can be reused after the previous request is completed; new requests cannot inherit unconsumed old outboxes.
    this.postResponseOutbox.delete(request.id);
    let releaseResidencyOperation: (() => void) | undefined;
    try {
// subscribe hydration, workspace configuration and resume may span await. If you only look at
// The current state of the session, the sampler will close the handler when it holds the old record. process-level lease
// Cover the entire request; the recognized sessionIds are additionally used for cold recovery gates and LRU touch.
      releaseResidencyOperation = await this.context.sessionResidentPool?.acquireOperation(
        collectResidencySessionIds(request.params),
      );
      const result = await this.dispatchRequest(request);
      return this.ok(request.id, result);
    } catch (error) {
      this.postResponseOutbox.delete(request.id);
      const protocolError = toProtocolError(error);
      return this.fail(request.id, protocolError.code, protocolError.message, protocolError.data);
    } finally {
      releaseResidencyOperation?.();
    }
  }

  private async dispatchRequest(request: ZCodeProtocolRequest) {
    switch (request.method) {
// ── v4 conversation channel (vertical cut, coexisting with old session/*)──
      case V4_METHODS.connectionFlow: {
        this.requireV4Gateway().setConnectionFlowState(request.params);
        return {};
      }
      case V4_METHODS.conversationSubscribe: {
// The same subscribe method is dispatched by topic prefix:
// sessions-index/* → list subscription; workspace-config/* → configure directory subscription; otherwise conversation.
        const gateway = this.requireV4Gateway();
        const topic = (request.params as { topic?: unknown } | null)?.topic;
        let dispatch;
        if (typeof topic === "string" && parseSessionsIndexTopic(topic) !== null) {
          dispatch = await gateway.subscribeSessionsIndexReserved(request.params);
        } else if (typeof topic === "string" && parseWorkspaceConfigTopic(topic) !== null) {
          dispatch = await gateway.subscribeWorkspaceConfigReserved(request.params);
        } else {
          dispatch = await gateway.subscribeReserved(request.params);
        }
        if (dispatch.initialWires.length > 0) {
          this.postResponseOutbox.set(request.id, {
            messages: dispatch.initialWires.map((wire) => ({
              method: V4_NOTIFICATIONS.conversationFrame,
              params: wire,
            })),
            commit: dispatch.commit,
          });
        }
        return { ack: dispatch.ack };
      }
      case V4_METHODS.conversationResync: {
// same-sub recovery and subscribe share deterministic post-response outbox; public
// response is still strict ACK-only, physical recovery can only be sent after the ACK line.
        const dispatch = this.requireV4Gateway().resyncReserved(request.params);
        if (dispatch.initialWires.length > 0) {
          this.postResponseOutbox.set(request.id, {
            messages: dispatch.initialWires.map((wire) => ({
              method: V4_NOTIFICATIONS.conversationFrame,
              params: wire,
            })),
            commit: dispatch.commit,
          });
        }
        return { ack: dispatch.ack };
      }
      case V4_METHODS.conversationUnsubscribe: {
// topic + subscriptionId + connectionId accurately hits the only publisher; no pressing is allowed
// subId casts a wide net on conversation/sessions-index/workspace-config.
        this.requireV4Gateway().unsubscribe(request.params);
        return {};
      }
//── Row paging query (independent branch, easy to merge with frame dispatch changes)──
      case V4_METHODS.conversationRowsRange:
        return await this.requireV4Gateway().rowsRange(request.params);
      case V4_METHODS.conversationPlans:
        return await this.requireV4Gateway().plans(request.params);
      case V4_METHODS.backgroundBashOutput:
        return await this.requireV4Gateway().backgroundBashOutput(request.params);
      case V4_METHODS.conversationFileChanges:
        return await this.requireV4Gateway().fileChanges(request.params);
      case V4_METHODS.conversationFileRewindPreview:
        return await this.requireV4Gateway().fileRewindPreview(request.params);
// workflow run event log paging (read-only, stateless, timeout retransmission safe; new method is naturally biased and safe).
      case V4_METHODS.conversationWorkflowRunEvents:
        return await this.requireV4Gateway().workflowRunEvents(request.params);
// dwf run enumeration (discovery query after restart).
      case V4_METHODS.conversationWorkflowRuns:
        return await this.requireV4Gateway().workflowRuns(request.params);
// Three reading surfaces of dwf user interface product. Same family: read-only, stateless,
// Timeout retransmission security; ArtifactRead authorization is on the host port side, and the gateway only verifies parameters and chunking.
      case V4_METHODS.conversationWorkflowRunArtifacts:
        return await this.requireV4Gateway().workflowRunArtifacts(request.params);
      case V4_METHODS.conversationWorkflowRunArtifactData:
        return await this.requireV4Gateway().workflowRunArtifactData(request.params);
      case V4_METHODS.conversationWorkflowRunArtifactRead:
        return await this.requireV4Gateway().workflowRunArtifactRead(request.params);
// Two reading sides of the transcript in the dwf workspace. Same race.
      case V4_METHODS.conversationWorkflowRunWorkspace:
        return await this.requireV4Gateway().workflowRunWorkspace(request.params);
      case V4_METHODS.conversationWorkflowRunNodeResult:
        return await this.requireV4Gateway().workflowRunNodeResult(request.params);
// Attachments can only use small RPC transactions, and full-data attachment/put single lines are prohibited.
      case V4_METHODS.attachmentBegin:
        return await this.requireV4Gateway().attachmentBegin(request.params);
      case V4_METHODS.attachmentChunk:
        return await this.requireV4Gateway().attachmentChunk(request.params);
      case V4_METHODS.attachmentCommit:
        return await this.requireV4Gateway().attachmentCommit(request.params);
      case V4_METHODS.attachmentAbort:
        await this.requireV4Gateway().attachmentAbort(request.params);
        return {};
      case V4_METHODS.attachmentRead:
        return await this.requireV4Gateway().attachmentRead(request.params);
      case V4_METHODS.conversationAttachmentRead:
        return await this.requireV4Gateway().conversationAttachmentRead(request.params);
      case V4_METHODS.conversationAttachmentStat:
        return await this.requireV4Gateway().conversationAttachmentStat(request.params);
      case V4_METHODS.attachmentPreviewSource:
        return await this.requireV4Gateway().attachmentPreviewSource(request.params);
// ── usage query (additive): same data access as old usage/stats, session/usage
// Layer (usage store aggregation), only change v4 namespace - without v4Gateway (no session projection dependency),
// Also not dispatched via old op (no bridge). Old cases are retained until old words are deleted (old host versions are compatible). ──
      case V4_METHODS.usageStats:
        return await getUsageStats(this.context, request.params);
      case V4_METHODS.conversationUsage:
        return await getTaskTokenUsage(this.context, request.params);
      case V4_METHODS.command:
        return this.requireV4Gateway().handleCommand(request.params);
      case V4_METHODS.commandsQuery:
        return this.requireV4Gateway().queryCommands(request.params);
      case zcodeProtocolMethods.sessionCreate:
        return await createSession(this.context, request.params, request.trace);
      case zcodeProtocolMethods.sessionResume:
        return await resumeSession(this.context, request.params);
      case zcodeProtocolMethods.sessionList:
        return await listSessions(this.context, request.params);
      case zcodeProtocolMethods.sessionSubagents:
        return await listSessionSubagents(this.context, request.params);
      case zcodeProtocolMethods.sessionRead:
        return await readSession(this.context, request.params);
      case zcodeProtocolMethods.sessionMessages:
        return await readMessages(this.context, request.params);
      case zcodeProtocolMethods.sessionEvents:
        return await readEvents(this.context, request.params);
      case zcodeProtocolMethods.sessionSubscribe:
        return await subscribeSession(this.context, request.params);
      case zcodeProtocolMethods.sessionSend:
        return await sendPrompt(this.context, request.params);
      case zcodeProtocolMethods.sessionStop:
        return await stopSession(this.context, request.params);
      case zcodeProtocolMethods.sessionCancelBackgroundTask:
        return await cancelBackgroundTask(this.context, request.params);
      case zcodeProtocolMethods.sessionFork:
        return await forkSession(this.context, request.params);
      case zcodeProtocolMethods.sessionCompact:
        return await compactSession(this.context, request.params);
      case zcodeProtocolMethods.sessionGoal:
        return await goalSession(this.context, request.params);
      case zcodeProtocolMethods.sessionSetModel:
        return await setModel(this.context, request.params);
      case zcodeProtocolMethods.sessionSetThoughtLevel:
        return await setThoughtLevel(this.context, request.params);
      case zcodeProtocolMethods.sessionSetMode:
        return await setMode(this.context, request.params);
      case zcodeProtocolMethods.sessionClose:
        return await closeSession(this.context, request.params);
      case zcodeProtocolMethods.workspaceReadPresentation:
        return await readWorkspacePresentation(this.context, request.params);
      case zcodeProtocolMethods.workspaceHookTrustGrant: {
        const grantResult = await grantWorkspaceHookTrustForProtocol(request.params, {
          appVersion: this.context.deps.version,
          policyProvider: this.context.deps.workspaceHookPolicyProvider,
        });
        if (grantResult.accepted) {
          await notifyWorkspaceHookTrustGrantSessions({
// The params of the dispatch layer are weakly typed; the same schema parse has been used internally in grant, here
// safeParse only takes out the workspaceKey for matching. If it fails, it will skip the notification (defense, it will succeed normally).
            grantedWorkspaceKey: zcodeWorkspaceHookTrustGrantParamsSchema.safeParse(request.params)
              .success
              ? zcodeWorkspaceHookTrustGrantParamsSchema.parse(request.params).workspace
                  .workspaceKey
              : undefined,
            sessions: this.context.sessions,
          });
        }
        return grantResult;
      }
      case zcodeProtocolMethods.providerUpdateAccountConfig:
        return await updateAccountProviderConfig(this.context, request.params);
      case zcodeProtocolMethods.workspaceUpdateInteractionPreferences:
        return await updateInteractionPreferences(this.context, request.params);
      case zcodeProtocolMethods.workspaceUpdateModelIoPreferences:
        return await updateModelIoPreferences(this.context, request.params);
      case zcodeProtocolMethods.workspaceUpdateOffPeakToolPolicy:
        return await updateOffPeakToolPolicy(this.context, request.params);
      case zcodeProtocolMethods.workspaceUpdateDynamicWorkflowPolicy:
        return await updateDynamicWorkflowPolicy(this.context, request.params);
      case zcodeProtocolMethods.workspaceGenerateText:
        return await this.withWorkspaceGenerateTextSignal(request, (signal) =>
          generateWorkspaceText(this.context, request.params, signal),
        );
      case zcodeProtocolMethods.workspaceCancelGenerateText:
        return this.cancelWorkspaceGenerateText(request.params);
      case zcodeProtocolMethods.providerTestModelConnectivity:
        return await testProviderModelConnectivity(this.context, request.params);
      case zcodeProtocolMethods.mcpList:
        return await listMcpServers(this.context, request.params);
      case zcodeProtocolMethods.pluginsList:
        return await listPlugins(this.context, request.params);
      case zcodeProtocolMethods.pluginsReferenceCatalogWithCategory:
        return await getPluginReferenceCatalog(this.context, request.params, true);
      case zcodeProtocolMethods.pluginsReferenceCatalog:
        return await getPluginReferenceCatalog(this.context, request.params);
      case zcodeProtocolMethods.skillsReferenceCatalog:
        return await getSkillReferenceCatalog(this.context, request.params);
      case zcodeProtocolMethods.workflowsList:
        return await listSavedWorkflowsOp(this.context, request.params);
      case zcodeProtocolMethods.workflowsGet:
        return await getSavedWorkflowOp(this.context, request.params);
      case zcodeProtocolMethods.workflowsUpdateMeta:
        return await updateSavedWorkflowMetaOp(this.context, request.params);
      case zcodeProtocolMethods.workflowsDelete:
        return await deleteSavedWorkflowOp(this.context, request.params);
      case zcodeProtocolMethods.workflowsRuns:
        return await listSavedWorkflowRunsOp(this.context, request.params);
      case zcodeProtocolMethods.workflowsMove:
        return await moveSavedWorkflowOp(this.context, request.params);
      case zcodeProtocolMethods.pluginsResolveSuggestedReference:
        return await this.withPluginOperationSignal(request, (signal) =>
          resolveSuggestedPluginReference(this.context, request.params, signal),
        );
      case zcodeProtocolMethods.pluginsSetEnabled:
        return await this.withPluginOperationSignal(request, (signal) =>
          setPluginEnabled(this.context, request.params, signal),
        );
      case zcodeProtocolMethods.pluginsOverview:
        return await getPluginsOverview(this.context, request.params);
      case zcodeProtocolMethods.processChildProcesses:
        return listChildProcesses(this.context.deps.mcpTelemetry?.listProcesses() ?? []);
      case zcodeProtocolMethods.runtimeCapabilities:
        return { independentPlanState: true };
      case zcodeProtocolMethods.pluginsMarketplaceAdd:
        return await this.withPluginOperationSignal(request, (signal) =>
          addPluginMarketplace(this.context, request.params, signal),
        );
      case zcodeProtocolMethods.pluginsMarketplaceRemove:
        return await removePluginMarketplace(this.context, request.params);
      case zcodeProtocolMethods.pluginsMarketplaceUpdate:
        return await this.withPluginOperationSignal(request, (signal) =>
          updatePluginMarketplace(this.context, request.params, signal),
        );
      case zcodeProtocolMethods.pluginsInstall:
        return await this.withPluginOperationSignal(request, (signal) =>
          installPlugin(this.context, request.params, signal),
        );
      case zcodeProtocolMethods.pluginsCancelOperation:
        return this.cancelPluginOperation(request.params);
      case zcodeProtocolMethods.pluginsUninstall:
        return await uninstallPlugin(this.context, request.params);
      case zcodeProtocolMethods.pluginsUpdate:
        return await updatePlugin(this.context, request.params);
      case zcodeProtocolMethods.pluginsRestoreBuiltin:
        return await restoreBuiltinPlugin(this.context, request.params);
      case zcodeProtocolMethods.pluginsConfigure:
        return await configurePlugin(this.context, request.params);
      case zcodeProtocolMethods.pluginsResetConfig:
        return await resetPluginConfig(this.context, request.params);
      case zcodeProtocolMethods.pluginsValidate:
        return await validatePlugin(this.context, request.params);
      case zcodeProtocolMethods.pluginsDescribe:
        return await describePlugin(this.context, request.params);
      case zcodeProtocolMethods.usageStats:
        return await getUsageStats(this.context, request.params);
      case zcodeProtocolMethods.sessionDebug:
        return querySessionDebug(this.context, request.params);
      case zcodeProtocolMethods.sessionUsage:
        return await getTaskTokenUsage(this.context, request.params);
      default:
        throw new ProtocolRequestError(-32601, `Method not found: ${request.method}`);
    }
  }

  private requireV4Gateway() {
    if (!this.context.v4Gateway) {
      throw new ProtocolRequestError(-32603, "v4 gateway is not initialized");
    }
    return this.context.v4Gateway;
  }

  private async withPluginOperationSignal<T>(
    request: ZCodeProtocolRequest,
    run: (signal?: AbortSignal) => Promise<T>,
  ): Promise<T> {
    const operationId = getPluginOperationId(request.params);
    if (!operationId) return await run();

    const controller = new AbortController();
    this.pluginOperationControllers.set(operationId, controller);
    try {
      return await run(controller.signal);
    } finally {
      if (this.pluginOperationControllers.get(operationId) === controller) {
        this.pluginOperationControllers.delete(operationId);
      }
    }
  }

  private cancelPluginOperation(rawParams: unknown) {
    const params = parseParams(zcodePluginsCancelOperationParamsSchema, rawParams);
    const controller = this.pluginOperationControllers.get(params.operationId);
    if (!controller) return { operationId: params.operationId, cancelled: false };
// The cancelability of plug-in synchronization must be retained in the V4 server; only the corresponding link is terminated according to operationId.
    controller.abort();
    this.pluginOperationControllers.delete(params.operationId);
    return { operationId: params.operationId, cancelled: true };
  }

  private async withWorkspaceGenerateTextSignal<T>(
    request: ZCodeProtocolRequest,
    run: (signal?: AbortSignal) => Promise<T>,
  ): Promise<T> {
    const operationId = getOperationId(request.params);
    if (!operationId) return await run();

    if (this.workspaceGenerateTextControllers.has(operationId)) {
// Repeating the operationId will overwrite the AbortController of the first request, causing the first request to lose the ability to cancel.
// The active operationId must remain unique; finally will be released after the request is completed, and then reuse is allowed.
      throw new ProtocolRequestError(
        -32600,
        `Workspace generate operation is already active: ${operationId}`,
      );
    }

    const controller = new AbortController();
    this.workspaceGenerateTextControllers.set(operationId, controller);
    try {
      return await run(controller.signal);
    } finally {
      if (this.workspaceGenerateTextControllers.get(operationId) === controller) {
        this.workspaceGenerateTextControllers.delete(operationId);
      }
    }
  }

  private cancelWorkspaceGenerateText(rawParams: unknown) {
    const params = parseParams(zcodeWorkspaceCancelGenerateTextParamsSchema, rawParams);
    const controller = this.workspaceGenerateTextControllers.get(params.operationId);
    if (!controller) return { operationId: params.operationId, cancelled: false };
    controller.abort(new DOMException("Workspace model request cancelled", "AbortError"));
    this.workspaceGenerateTextControllers.delete(params.operationId);
    return { operationId: params.operationId, cancelled: true };
  }

  private ok(id: ZCodeProtocolRequestId, result: unknown): ZCodeProtocolResponse {
    return { id, result };
  }

  private fail(
    id: ZCodeProtocolRequestId,
    code: number,
    message: string,
    data?: unknown,
  ): ZCodeProtocolError {
    return { error: { code, data, message }, id };
  }

  private requestClient<T>(
    method: ZCodeProtocolMethod,
    params: unknown,
    resultSchema: ParamsSchema<T>,
    options?: ZCodeProtocolClientRequestOptions,
  ): Promise<T> {
    if (this.clientDisconnectError) {
      throw this.clientDisconnectError;
    }
    if (!this.messageSink) {
      throw new ProtocolRequestError(-32020, `No ZCode Protocol client is attached for ${method}`);
    }

    return new Promise<T>((resolve, reject) => {
      let active = true;
      const pending: PendingClientRequest<T> = {
        method,
        reject,
        resolve,
        resultSchema,
        requestKeys: new Set(),
        signal: options?.signal,
      };
      const cleanup = () => {
        active = false;
        this.cleanupClientRequest(pending);
      };
      pending.abortHandler = () => {
        cleanup();
        reject(new ProtocolRequestError(-32021, `Client request cancelled: ${method}`));
      };
      if (options?.signal?.aborted) {
        pending.abortHandler();
        return;
      }
      if (options?.timeoutMs !== undefined) {
        pending.timeout = setTimeout(() => {
          cleanup();
          reject(
            new ProtocolRequestError(-32022, `Client request timed out: ${method}`, {
              timeoutMs: options.timeoutMs,
            }),
          );
        }, options.timeoutMs);
      }
      options?.signal?.addEventListener("abort", pending.abortHandler, { once: true });
      const sendClientRequest = () => {
        if (!active) {
          return;
        }
        const id = `server-${this.nextClientRequestId++}`;
        const key = String(id);
        pending.requestKeys.add(key);
        this.pendingClientRequests.set(key, pending as PendingClientRequest<unknown>);
        this.messageSink?.({
          id,
          method,
          params,
          ...(options?.trace ? { trace: options.trace } : {}),
        });
      };
      sendClientRequest();
      const reannounceIntervalMs =
        options?.reannounceIntervalMs !== undefined &&
        Number.isFinite(options.reannounceIntervalMs) &&
        options.reannounceIntervalMs > 0
          ? Math.floor(options.reannounceIntervalMs)
          : undefined;
      if (reannounceIntervalMs !== undefined) {
        let nextReannounceIntervalMs = reannounceIntervalMs;
        const scheduleReannounce = () => {
          pending.reannounceTimer = setTimeout(() => {
            if (!active) {
              return;
            }
            sendClientRequest();
            nextReannounceIntervalMs = Math.min(
              nextReannounceIntervalMs * 2,
              MAX_CLIENT_REQUEST_REANNOUNCE_INTERVAL_MS,
            );
            scheduleReannounce();
          }, nextReannounceIntervalMs);
        };
        scheduleReannounce();
      }
    });
  }

  private resolveClientRequest(id: ZCodeProtocolRequestId, result: unknown): void {
    const key = String(id);
    const pending = this.pendingClientRequests.get(key);
    if (!pending) {
      return;
    }
    this.cleanupClientRequest(pending);
    try {
      pending.resolve(pending.resultSchema.parse(result));
    } catch (error) {
      pending.reject(
        error instanceof Error ? error : new Error(`Invalid response: ${pending.method}`),
      );
    }
  }

  private rejectClientRequest(id: ZCodeProtocolRequestId, error: Error): void {
    const key = String(id);
    const pending = this.pendingClientRequests.get(key);
    if (!pending) {
      return;
    }
    this.cleanupClientRequest(pending);
    pending.reject(error);
  }

  private cleanupClientRequest<T>(pending: PendingClientRequest<T>): void {
    if (pending.timeout) {
      clearTimeout(pending.timeout);
    }
    if (pending.reannounceTimer) {
      clearTimeout(pending.reannounceTimer);
    }
    if (pending.abortHandler) {
      pending.signal?.removeEventListener("abort", pending.abortHandler);
    }
    for (const requestKey of pending.requestKeys) {
      this.pendingClientRequests.delete(requestKey);
    }
    pending.requestKeys.clear();
  }
}
