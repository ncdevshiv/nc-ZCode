/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { DEFAULT_ZCODE_MODEL_CONTEXT_BUDGET_STRATEGY, resolveExecutionState } from "@zcode/shared";
import type { BackgroundBashOutputResult } from "@zcode/shared";
import {
  createDenyPermissionBroker,
  createRootTraceContext,
  createToolRegistry,
  defaultPermissionConfig,
  EventReducer,
  MessageHistoryImpl,
  PermissionService,
  ToolScheduler,
  traceContextToLogContext,
} from "./deps.js";
import type {
  CollaborationMode,
  Logger,
  BackgroundTaskCancelResult,
  SessionEvent,
  MessageId,
  Model,
  ModelSelection,
  ModelSelectionOrigin,
  ModelToolContract,
  PermissionBrokerPort,
  PermissionBrokerRequest,
  ProjectId,
  SessionEventSink,
  SessionEventStorePort,
  SessionId,
  SessionProjection,
  SessionStorePort,
  SessionGoal,
  SavedWorkflowScope,
  TargetChangedPayload,
  DynamicWorkflowRunProgressPayload,
  UserInputAutoResolutionUpdatedPayload,
  ContextSourcePort,
  ExecutionPort,
  FileSystemPort,
  ImageProcessorPort,
  PdfDocumentPort,
  McpConnectionSnapshot,
  SkillLoadOutcome,
  SkillPort,
  McpPort,
  DynamicWorkflowRunPort,
  ModelCatalogPort,
  SubagentPort,
  ToolArtifactStorePort,
  ToolCallId,
  TraceContext,
  TurnSteerInput,
  TurnInputIntentMetadata,
  TurnSteerResult,
  ToolCall,
  TurnState,
  MessageHistory,
  ReadFileStateMap,
  ToolSchedule,
  ToolExecutor,
  ToolRegistry,
  ContextBuilder,
  ContextBuildResult,
  ContextSourceSnapshot,
  ExecutionShellSelection,
  HookRunner,
  TurnId,
} from "./deps.js";
import { installAgentRuntimeMethods } from "./methods/index.js";
import type { StartSavedWorkflowRunResult } from "./methods/dynamic-workflow-run-start.js";
import type {
  AmendWorkflowRunSettingsInput,
  AmendWorkflowRunSettingsResult,
} from "./methods/dynamic-workflow-run-settings.js";
import { createRuntimeCommandQueue } from "./command-queue.js";
import type { RuntimeCommandQueue } from "./command-queue.js";
import type {
  ModelConnectivityTestInput,
  WorkspaceGenerateTextInput,
  WorkspaceGenerateTextResult,
} from "./methods/workspace-generate-text.js";
import type {
  RuntimeBackgroundStopOptions,
  RuntimeBackgroundStopResult,
} from "./methods/background.js";
import { initializeRuntimeTooling } from "./helpers/runtime-tools.js";
import type {
  ActiveTurnInfo,
  ActiveForegroundExecutionState,
  AcquireForegroundPromotionLeaseResult,
  ActiveTurnStartReservation,
  ActiveTurnSteeringState,
  AgentRuntimeConfig,
  AgentRuntimeDeps,
  ContinueActiveTargetLoopOptions,
  ConversationBeforeInputForkOptions,
  ConversationRewindResult,
  ExecuteToolsOptions,
  ExecuteToolsResult,
  ExecuteTurnOptions,
  PromptAdmissionOptions,
  PromptAdmissionReceipt,
  ForegroundPromotionLeaseMode,
  ForegroundPromotionLeaseState,
  PendingModelChangeTimeline,
  PermissionDecisionResult,
  MainTurnCacheHitAggregate,
  RuntimeTurnFileChangeMap,
  ResumeSessionOptions,
  ResumeSessionResult,
  SelectionSideChatCreateOptions,
  StableConversationForkOptions,
  StopActiveForegroundExecutionOptions,
  StopActiveForegroundExecutionResult,
  TurnResult,
  WorkspaceCheckpointSummary,
  WorkspaceFileRewindApplyResult,
  WorkspaceFileRewindPreview,
  WorkspaceForkResult,
} from "./types.js";
import type { AgentRuntimeInternal } from "./internal.js";
import { InMemoryRuntimeTaskRegistry, type RuntimeTaskRegistry } from "../runtime-task/registry.js";
import type { ChildClientPortsContext, ClientFacingPorts } from "./helpers/child-client-ports.js";
import type { ProjectMemoryExtractionScheduler } from "./helpers/project-memory-extraction.js";
import { projectPersistentAgentMemoryTools } from "../subagent/persistent-memory.js";
import { RuntimeTelemetryFacade } from "../telemetry/runtime-telemetry.js";
import type { WorkspaceHookRuntimeAdmissionPort } from "../hooks/workspace-hook-runtime-admission.js";
import { disposeNodeReplSession } from "../tool/handlers/node-repl.js";
import { cloneModelSelection } from "./model-selection.js";

// oxlint-disable typescript-eslint/no-unsafe-declaration-merging
export class AgentRuntime {
  private sessionId: SessionId;
  private turnNumber: number;
  private config: AgentRuntimeConfig;
  private appVersion: string;
  private permissionService: PermissionService;
  private permissionBroker: PermissionBrokerPort;
  private toolScheduler: ToolScheduler;
  private eventReducer: EventReducer;
  private eventStore: SessionEventStorePort;
  private rootTraceContext: TraceContext;
  private logger?: Logger;
  private eventSinks = new Set<SessionEventSink>();
  private now: () => Date;
  private isRemoteWorkspace: () => boolean;
  private registry: ToolRegistry;
  private executor: ToolExecutor;
  private hookRunner?: HookRunner;
  private workspaceHookAdmission?: WorkspaceHookRuntimeAdmissionPort;
  private modelFactory: AgentRuntimeDeps["modelFactory"];
  private modelIoDir?: string;
  private providerRuntimeHeadersPort?: AgentRuntimeDeps["providerRuntimeHeadersPort"];
  private browserControlPort?: AgentRuntimeDeps["browserControlPort"];
/** Model request access port; enters the calling context with each model request. */
  private modelRequestAdmission?: AgentRuntimeDeps["modelRequestAdmission"];
  private sessionModelSelection: ModelSelection | undefined;
  private messageHistory: MessageHistory;
  private readFileState: ReadFileStateMap;
  private cachedTools: ModelToolContract[] | null = null;
  private contextBuilder: ContextBuilder | null = null;
  private contextInitialized = false;
  private contextSourceSnapshot?: ContextSourceSnapshot;
  private latestContextBuildResult?: ContextBuildResult;
  private memoryRoot?: string;
  private memoryIndexContent?: string;
  private memoryExtractionScheduler?: ProjectMemoryExtractionScheduler;
  private contextSourcePort?: ContextSourcePort;
  private skillPort?: SkillPort;
  private mcpPort?: McpPort;
  private mcpStartupPromise?: Promise<McpConnectionSnapshot>;
  private residencyBlockingWorkCount = 0;
  private mcpInitialized = false;
  private mcpToolsRegistered = false;
  private subagentPort?: SubagentPort;
  private dynamicWorkflowRunPort?: DynamicWorkflowRunPort;
  private modelCatalogPort?: ModelCatalogPort;
  private runtimeTaskRegistry: RuntimeTaskRegistry;
  private branchGeneration = 0;
  private artifactStore?: ToolArtifactStorePort;
  private executionPort?: ExecutionPort;
  private fileSystemPort?: FileSystemPort;
  private imageProcessorPort?: ImageProcessorPort;
  private pdfDocumentPort?: PdfDocumentPort;
  private skillLoadOutcome?: SkillLoadOutcome;
  private workingDirectory: string;
  private workspaceRoot: string;
  private sessionStore?: SessionStorePort;
  private sessionPersisted = false;
  private needsPlanModeExitReminder = false;
  private latestConversationMessageId?: MessageId;
  private latestAssistantMessageId?: MessageId;
  private latestAssistantTurnId?: TurnId;
  private mainTurnCacheHitAggregate: MainTurnCacheHitAggregate = {
    requestCount: 0,
    totalInputTokens: 0,
    totalCacheReadTokens: 0,
    totalCacheWriteTokens: 0,
  };
  private currentTurnFileChanges: RuntimeTurnFileChangeMap = new Map();
  private lastAssistantCompletedAtMs?: number;
  private lastEmittedLocalDate?: string;
  private autoCompactConsecutiveFailures = 0;
  private runtimeCommandQueue: RuntimeCommandQueue;
  private runtimeCommandDrainActive = false;
  private activeForegroundExecution?: ActiveForegroundExecutionState;
/** Core scheduling right of sendQueuedNow; only lives in the current process, matches the runtime command and is consumed when dequeued. */
  private foregroundPromotionLease?: ForegroundPromotionLeaseState;
  private activeTurn?: ActiveTurnSteeringState;
  private activeTurnStartReservation?: ActiveTurnStartReservation;
  private pendingInputSequence = 0;
/** sendQueuedNow reservation; only lives in the current CLI process to prevent drain/multi-terminal repeated promotion. */
  private pendingInputReservations = new Map<string, string>();
// When v4 setAutoDrain: false, the queued input will not be automatically consumed.
// (turn-stop does not continue running, roundtrip does not drain), reserved as held for explicit consumption.
  private queueAutoDrain = true;
// After the pause queue is restored, the CLI will promote it item by item according to the projected FIFO. Disable core in this window and only view the current
// activeTurn.pendingInputs does an inline drain, otherwise newly enqueued messages would pass over old pause items that still remain in the projection.
  private queueExternalDrainActive = false;
  private shuttingDown = false;
  private backgroundTaskNotificationsSealed = false;
  private backgroundTaskNotificationSealReason?: "subagent_terminal" | "subagent_cancelled";
  private pendingModelChangeTimeline?: PendingModelChangeTimeline;
  private sessionStartHookRan = false;
  private sessionTitleGenerationAttempted = false;
  private agentTelemetry: RuntimeTelemetryFacade;

  constructor(sessionId: SessionId, config: AgentRuntimeConfig, deps: AgentRuntimeDeps) {
    const runtime = this as unknown as AgentRuntimeInternal;
    this.sessionId = sessionId;
    this.turnNumber = 0;
// 3.12.2: Compatible with old Host/internal calls passed into legacy, but this version of Runtime, logs and sub-Agents only use preflight.
    this.config = projectPersistentAgentMemoryTools({
      ...config,
      modelContextBudgetStrategy: DEFAULT_ZCODE_MODEL_CONTEXT_BUDGET_STRATEGY,
    });
    Object.assign(this.config, resolveExecutionState(config));
    this.agentTelemetry = new RuntimeTelemetryFacade({
      agentName: config.agentName,
      causation: deps.agentTelemetryCausation,
      causationMode: deps.agentTelemetryCausationMode,
      parentSessionId: config.parentSessionId,
      port: deps.agentTelemetry,
      sessionId,
      taskType: config.taskType,
    });
    this.permissionService =
      deps.permissionService ?? new PermissionService(defaultPermissionConfig);
    this.permissionBroker = deps.permissionBroker ?? createDenyPermissionBroker();
    this.toolScheduler =
      deps.toolScheduler ??
      new ToolScheduler({
        maxConcurrency: this.config.toolConcurrency?.maxConcurrency,
      });
    this.eventReducer = new EventReducer();
    this.eventStore = deps.eventStore;
    this.sessionStore = deps.sessionStore;
    this.rootTraceContext = deps.traceContext ?? createRootTraceContext({ sessionId });
    this.appVersion = deps.appVersion ?? "0.0.0";
    this.logger = deps.logger?.child({
      ...traceContextToLogContext(this.rootTraceContext),
      module: "core.runtime",
    });
    if (deps.eventSink) {
      this.eventSinks.add(deps.eventSink);
    }
    this.now = deps.now ?? (() => new Date());
    this.isRemoteWorkspace = deps.isRemoteWorkspace ?? (() => false);
    this.modelFactory = deps.modelFactory;
    this.modelIoDir = deps.modelIoDir;
    this.providerRuntimeHeadersPort = deps.providerRuntimeHeadersPort;
    this.browserControlPort = deps.browserControlPort;
    this.modelRequestAdmission = deps.modelRequestAdmission;
// Lack of selection for old sessions does not prevent history recovery; no default model is made here.
    this.sessionModelSelection =
      config.modelSelection && cloneModelSelection(config.modelSelection);
    this.messageHistory = new MessageHistoryImpl();
    this.readFileState = new Map();
    this.runtimeCommandQueue = createRuntimeCommandQueue();
    this.workingDirectory = config.workingDirectory ?? ".";
    this.contextSourcePort = deps.contextSourcePort;
    this.skillPort = deps.skillPort;
    this.mcpPort = deps.mcpPort;
    this.runtimeTaskRegistry = deps.runtimeTaskRegistry ?? new InMemoryRuntimeTaskRegistry();
    this.runtimeTaskRegistry.setActiveBranchGeneration?.(this.branchGeneration);
    this.artifactStore = deps.artifactStore;
    this.executionPort = deps.executionPort;
    this.fileSystemPort = deps.fileSystemPort;
    this.imageProcessorPort = deps.imageProcessorPort;
    this.pdfDocumentPort = deps.pdfDocumentPort;
    this.subagentPort = deps.subagentPort ?? runtime.createDefaultSubagentPort(deps);
    this.dynamicWorkflowRunPort = deps.dynamicWorkflowRunPort;
// The directory used by GUI "Configuration" to parse the subagent model (the same port as the tool context).
    this.modelCatalogPort = deps.modelCatalogPort;
    this.registry = deps.toolRegistry ?? createToolRegistry();
    this.workspaceRoot = this.workingDirectory;
    const tooling = initializeRuntimeTooling(runtime, deps, sessionId);
    this.hookRunner = tooling.hookRunner;
    this.workspaceHookAdmission = deps.workspaceHookAdmission;
    this.executor = tooling.executor;

    this.contextBuilder = deps.contextBuilder ?? null;
    if (this.contextBuilder) {
      runtime.initializeMessageHistoryFromContext(this.contextBuilder, this.rootTraceContext);
      this.contextInitialized = true;
    }
    runtime.startMcpStartup(this.rootTraceContext);
  }

  async closeBrowserSession(): Promise<void> {
    this.beginShutdown();
    disposeNodeReplSession(this.sessionId);
    try {
      await this.browserControlPort?.closeSession?.({
        sessionId: this.sessionId,
        traceContext: this.rootTraceContext,
      });
    } catch (error) {
// Browser backend cleanup failure cannot block the main shutdown link of execution/MCP/session store.
      this.logger?.warn("Browser session cleanup failed", {
        error: error instanceof Error ? error.message : String(error),
        event: "browser.session_cleanup.failed",
      });
    }
  }

  beginShutdown(): void {
// ExecutionPort.close() will close the background Bash port as canceled; if allowed
// The teardown terminal event wakes up the model again and competes with the subsequently closed session store.
    this.shuttingDown = true;
// The process still survives after closing a single session,
// Therefore, the Extraction of the runtime must be terminated first, and you cannot just give up waiting after the timeout.
    this.memoryExtractionScheduler?.shutdown();
  }
}

export interface AgentRuntime {
  lastPermissionGrantId?: string;
  beginShutdown(): void;
  closeBrowserSession(): Promise<void>;
  updateConfig(
    patch: Pick<AgentRuntimeConfig, "mode" | "planEnabled" | "language" | "outputStyle">,
  ): void;
  initializeSessionShellEnvironmentIfNeeded(
    selection: ExecutionShellSelection | (() => ExecutionShellSelection),
  ): boolean;
  getSessionShellSelection(): ExecutionShellSelection | undefined;
  getMode(): CollaborationMode;
  getPlanEnabled(): boolean;
  grantPermissionFullAccess(interactionId: string, signal?: AbortSignal): Promise<string>;
  setExecutionState(
    input: { mode?: string; planEnabled?: boolean },
    traceContext?: TraceContext,
  ): Promise<void>;
  getSessionModelSelection(): ModelSelection | undefined;
  setSessionModelSelection(selection: ModelSelection | undefined): void;
  getProjectId(): ProjectId;
  ensureSessionPersistedForExternalActivity(
    input: string,
    options?: { traceContext?: TraceContext },
  ): Promise<void>;
  maybeStartSessionTitleGenerationFromExternalInput(
    input: string,
    options?: { goalSummaryTargetID?: string; traceContext?: TraceContext },
  ): void;
/** renameSession: User explicitly renames (titleSource=custom, sends SessionTitleUpdated). */
  setCustomSessionTitle(input: { title: string; traceContext: TraceContext }): Promise<void>;
  maybeStartGoalSummaryTitleGeneration(
    input: string,
    targetID: string,
    options?: { traceContext?: TraceContext },
  ): boolean;
  recordExternalUserPrompt(
    input: string,
    options?: {
      goalSummaryTargetID?: string;
      traceContext?: TraceContext;
      intent?: TurnInputIntentMetadata;
    },
  ): Promise<MessageId>;
  recordPendingModelChange(input: {
    fromModel?: ModelSelection;
    fromModelLabel?: string;
    toModel: ModelSelection;
    toModelLabel: string;
  }): void;
  getActiveTurnInfo(): ActiveTurnInfo | undefined;
  admitPrompt(
    input: string,
    attachments?: TurnState["attachments"],
    options?: PromptAdmissionOptions,
  ): Promise<PromptAdmissionReceipt>;
/** The runtime busy authoritative fact used by the Session resident pool, including queue/drain/reservation. */
  hasActiveOrQueuedTurnWork(): boolean;
/** Session resident pool used by background Bash/Agent/Workflow running authoritative facts. */
  hasRunningBackgroundTasks(): boolean;
  /**
* The runtime owned-work aggregate fact consumed by the Session resident pool is the only one consumed.
* Includes frontend/queue, registry background task, detached sidecar and memory work.
   */
  hasResidencyBlockingWork(): boolean;
  /**
* Register a runtime-owned work that will cross the current synchronization call stack. The count is incremented in **this synchronization piece** and in the promise.
* Finally released (see runtime/methods/residency.ts for implementation).
   *
* It is exposed on this side because sidecars outside the runtime also need to be registered through the same port: the dwf engine runs in the session App,
* The runtime task registry is not entered, so "the session was shut down by idle while the engine was in flight" appears.
* All new sidecars are registered here, instead of adding another guess on the bootstrap side.
   */
  trackResidencyBlockingWork<T>(work: Promise<T>): Promise<T>;
  /**
* Whether the session has been dropped into the persistence store. The draft (deferred) determination of the protocol layer record uses this as the source of fact: any runtime
* The first persistence path (first input, external activity, launch wheel launched directly by the hub) will cause the session to leave the draft.
   */
  isSessionPersisted(): boolean;
  getActiveForegroundExecutionId(): string | undefined;
  acquireForegroundPromotionLease(options: {
    leaseId: string;
    mode: ForegroundPromotionLeaseMode;
    promotedInputId: string;
  }): AcquireForegroundPromotionLeaseResult;
  releaseForegroundPromotionLease(leaseId: string): boolean;
  enqueueDeferredInput(input: string | TurnSteerInput): Promise<TurnSteerResult>;
  steerTurn(input: string | TurnSteerInput): Promise<TurnSteerResult>;
/** v4 queue single item deletion: remove a queued input of the current active turn according to pendingInputId. */
  removePendingInputById(options: {
    pendingInputId: string;
    reason: "user_removed" | "promoted";
    reservationId?: string;
    traceContext?: TraceContext;
  }): Promise<boolean>;
  reservePendingInputById(options: {
    pendingInputId: string;
    reservationId: string;
    traceContext?: TraceContext;
  }): Promise<boolean>;
  markPendingInputPromoting(options: {
    pendingInputId: string;
    reservationId: string;
    traceContext?: TraceContext;
  }): Promise<boolean>;
  releasePendingInputReservation(options: {
    pendingInputId: string;
    reservationId: string;
    traceContext?: TraceContext;
  }): Promise<boolean>;
/** v4 queue single item editing: replace the queued input text by pendingInputId (save position). */
  editPendingInputById(options: {
    pendingInputId: string;
    newText: string;
    traceContext?: TraceContext;
  }): Promise<boolean>;
/** v4 queue rearrangement: move pendingInputId to beforePendingInputId (null=queue tail). */
  reorderPendingInput(options: {
    pendingInputId: string;
    beforePendingInputId: string | null;
    traceContext?: TraceContext;
  }): Promise<boolean>;
  /**
* v4 heldQueueDisposition=clearQueueAndSend executor:
* Clear all queued inputs (active turn memory items + held projection residues), and return the number of discarded items.
   */
  clearAllPendingInputs(traceContext: TraceContext): Promise<number>;
/** v4 setAutoDrain: Toggle queue autoDrain authorization bit (session level). */
  setQueueAutoDrain(options: { autoDrain: boolean; traceContext?: TraceContext }): Promise<void>;
/** The outer FIFO of the pause queue has been consumed until it is empty, and the inline drain of the subsequent running queue is resumed. */
  completeExternalQueueDrain(): void;
/** v4 setFollowupMode: Flip followup routing mode (queue/guide, session level). */
  setFollowupMode(options: { mode: "queue" | "guide"; traceContext?: TraceContext }): Promise<void>;
/** v4 switchModelConfig: Reissue ModelSelected (config/marker projection) after the model selection changes. */
  emitModelSelected(options: {
    modelSelection: ModelSelection;
    model?: Model;
    effectiveReasoningLevel?: string;
    previousModelSelection?: ModelSelection | null;
    origin?: ModelSelectionOrigin;
    supportedThoughtLevels?: readonly string[];
    traceContext?: TraceContext;
  }): Promise<void>;
/** v4 switchCollaborationMode: Reissue SessionModeChanged (config.mode projection) after the collaboration mode is switched. */
  emitModeChanged(options: {
    mode: CollaborationMode;
    previousMode: CollaborationMode;
    traceContext: TraceContext;
  }): Promise<void>;
  getToolRegistry(): ToolRegistry;
  /**
* Let getTools recalculate after the registry is overwritten externally. The only consumer that exposes it is the dwf driver's submit profile runtime
* Guard: When the static profile does not match the actual ask,
* Typed's submit_result is replaced with a universal statement - the same registry is changed, and the old statement will continue to be sent to the model if the cache is not invalidated.
   */
  invalidateToolCache(): void;
  getToolExecutor(): ToolExecutor;
  subscribeEvents(sink: SessionEventSink): () => void;
  /** Bootstrap-owned lifecycle producers append only validated session events through this durable path. */
  appendEvent(event: SessionEvent, traceContext: TraceContext): Promise<void>;
  /**
* The seam of external sub-runtime (1): Hand over the session event store of this runtime for the sub-class constructed outside the class
* runtime sharing (according to `eventStore: this.eventStore` of `subagent.ts`). See the reason
* Implementation comments for `methods/config.ts`.
   */
  getSessionEventStore(): SessionEventStorePort;
  /**
* Seam of external sub-runtime (2): fan out the original events of the sub-session to the external sink set of this runtime
* (Keep sub-sessionId, only notify but not append). **Must be installed during the construction of the sub-runtime
* `deps.eventSink`** - see the implementation comments in `methods/config.ts` for the reason.
   */
  notifyExternalChildSessionEvent(input: {
    childSessionId: SessionId;
    event: SessionEvent;
    traceContext?: TraceContext;
  }): Promise<void>;
  /**
* The seam of the external sub-runtime (3): the external interaction port of the casting sub-runtime (permission broker +
* provider runtime headers), the client routing identity of this runtime has been bound. Child runtime constructed outside class
* **Must** get these two ports through here, not from appOptions - see the implementation comments of `methods/config.ts` for the reason.
   */
  createChildClientPorts(context: ChildClientPortsContext): ClientFacingPorts;
  getContextBuilder(): ContextBuilder;
/** Snapshot of the Session Skill used by Composer; frozen in the same runtime, rediscovered after the runtime is rebuilt. */
  getSkillCatalog(traceContext: TraceContext): Promise<SkillLoadOutcome>;
  resumeFromStore(options?: ResumeSessionOptions): Promise<ResumeSessionResult>;
  recordTargetChanged(input: TargetChangedPayload & { traceContext: TraceContext }): Promise<void>;
  recordUserInputAutoResolutionUpdate(
    input: UserInputAutoResolutionUpdatedPayload & { traceContext?: TraceContext },
  ): Promise<void>;
/** The output round of workflow run progress is appended (the event source is in the run service of bootstrap). */
  recordDynamicWorkflowRunProgress(
    input: DynamicWorkflowRunProgressPayload & { traceContext?: TraceContext },
  ): Promise<void>;
/** The tracking heavy arm of the restored workflow run (registry registration + started event + waiter + settlement notification). */
  trackResumedDynamicWorkflowRun(input: {
    runId: string;
    toolCallId?: string;
    name?: string;
    traceContext?: TraceContext;
  }): Promise<void>;
  /**
* The hub directly starts a saved workflow: parsing + verification + compilation,
* If it is clean, submit to start run, drop controlOnly to start the wheel, and register background tracking. `app.startSavedWorkflow` capability
* Implemented on the ground (port registered when present).
   */
  startSavedWorkflowRun(input: {
    name: string;
    scope?: SavedWorkflowScope;
    args?: Record<string, unknown>;
    traceContext?: TraceContext;
  }): Promise<StartSavedWorkflowRunResult>;
  /**
* GUI "Configuration" changes the subagent model and concurrency upper bound of a run: revise a new run, register background tracking, and queue the setting wheel with the same script.
* Implementation of `app.amendWorkflowRunSettings` capability.
   */
  amendWorkflowRunSettings(
    input: AmendWorkflowRunSettingsInput,
  ): Promise<AmendWorkflowRunSettingsResult>;
  recordGoalStateChangeReminder(input: {
    text: string;
    traceContext?: TraceContext;
  }): Promise<void>;
  continueActiveTargetIfIdle(options?: {
    abortSignal?: AbortSignal;
    inputId?: string;
    intent?: TurnInputIntentMetadata;
    traceContext?: TraceContext;
    verifyBeforeContinue?: boolean;
  }): Promise<TurnResult | null>;
  continueActiveTargetLoop(options: ContinueActiveTargetLoopOptions): Promise<TurnResult | null>;
  stopActiveForegroundExecution(
    options?: StopActiveForegroundExecutionOptions,
  ): StopActiveForegroundExecutionResult;
  activatePausedTargetAfterResume(traceContext: TraceContext): Promise<SessionGoal | null>;
  executeTurn(
    input: string,
    attachments?: TurnState["attachments"],
    options?: ExecuteTurnOptions,
  ): Promise<TurnResult>;
  scheduleTools(toolCalls: ToolCall[]): Promise<ToolSchedule>;
  executeTools(
    toolCalls: ToolCall[],
    schedule: ToolSchedule,
    options?: ExecuteToolsOptions,
  ): Promise<ExecuteToolsResult>;
  emitPermissionRequest(toolCallId: ToolCallId, toolName: string, riskLevel: string): Promise<void>;
  resolvePermission(toolCallId: ToolCallId, decision: PermissionDecisionResult): Promise<void>;
  getPendingPermissionRequests(): PermissionBrokerRequest[];
  getProjection(): Promise<SessionProjection>;
  readBackgroundBashOutput(workId: string, sessionId?: string): Promise<BackgroundBashOutputResult>;
  cancelBackgroundTask(
    taskId: string,
    options?: { traceContext?: TraceContext },
  ): Promise<BackgroundTaskCancelResult>;
  stopBackgroundTask(
    taskId: string,
    options: RuntimeBackgroundStopOptions,
  ): Promise<RuntimeBackgroundStopResult>;
  cancelRunningRuntimeBackgroundTasks(input: {
    reason: "subagent_cancelled";
    traceContext?: TraceContext;
  }): Promise<void>;
  sealBackgroundTaskNotifications(input: {
    reason: "subagent_terminal" | "subagent_cancelled";
    traceContext?: TraceContext;
  }): void;
  getSessionId(): SessionId;
  listWorkspaceCheckpoints(options?: { limit?: number }): Promise<WorkspaceCheckpointSummary[]>;
  forkWorkspaceFromCheckpoint(options?: {
    abortSignal?: AbortSignal;
    forkedSessionId?: SessionId;
    targetCheckpointId?: string;
    targetMessageId?: MessageId;
    traceContext?: TraceContext;
  }): Promise<WorkspaceForkResult>;
  forkStableConversationAtMessage(
    options: StableConversationForkOptions,
  ): Promise<WorkspaceForkResult>;
  createSelectionSideConversation(
    options: SelectionSideChatCreateOptions,
  ): Promise<WorkspaceForkResult>;
  forkConversationBeforeMessage(
    options: ConversationBeforeInputForkOptions,
  ): Promise<WorkspaceForkResult>;
  /**
* Same-session branch cut primitive for conversation edit/retry.
   *
* This entry intentionally does not go through the executeTurn command queue: combined file rewind will be executed in the file transaction
* Call it within the commit gate; if `/rewind` is queued again, the current edit command will wait for itself to release the queue.
   */
  rewindConversationToMessage(options: {
    abortSignal?: AbortSignal;
    events: SessionEvent[];
    targetMessageId: MessageId;
    traceContext: TraceContext;
  }): Promise<ConversationRewindResult>;
  previewWorkspaceFileRewind(options?: {
    abortSignal?: AbortSignal;
    targetCheckpointId?: string;
    targetMessageId?: MessageId;
    targetMessageIds?: MessageId[];
    targetTurnId?: TurnId;
    traceContext?: TraceContext;
  }): Promise<WorkspaceFileRewindPreview>;
  applyWorkspaceFileRewind(options?: {
    abortSignal?: AbortSignal;
    targetCheckpointId?: string;
    targetMessageId?: MessageId;
    targetMessageIds?: MessageId[];
    targetTurnId?: TurnId;
    traceContext?: TraceContext;
    commitAfterApply?: () => Promise<void>;
  }): Promise<WorkspaceFileRewindApplyResult>;
  generateWorkspaceText(
    input: WorkspaceGenerateTextInput,
    options?: { abortSignal?: AbortSignal; traceContext?: TraceContext },
  ): Promise<WorkspaceGenerateTextResult>;
  testModelConnectivity(
    input: ModelConnectivityTestInput,
    options?: { abortSignal?: AbortSignal; traceContext?: TraceContext },
  ): Promise<void>;
  isProjectMemoryEnabled(): boolean;
/** The default wait is up to 60 seconds; null waits for all scheduled extractions to end without setting a drain deadline. */
  drainMemoryExtractions(timeoutMs?: number | null): Promise<void>;
}

installAgentRuntimeMethods(AgentRuntime);
