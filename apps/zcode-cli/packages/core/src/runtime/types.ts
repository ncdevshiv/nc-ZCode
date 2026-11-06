import type { RuntimeInputPresentation } from "@zcode/contracts";
/* eslint-disable max-lines -- The Runtime type centrally carries the core/runtime external structure. Splitting requires separate migration. */
import { PermissionService, ToolScheduler } from "./deps.js";
import type {
  JsonSchema,
  AgentExecutionTelemetryPort,
  AgentTelemetryCausation,
  BackgroundResultOriginMeta,
  ContextUsageBreakdownItem,
  CoordinatorResponsePort,
  ForkCommitBundle,
  ForkChildSessionMetadata,
  ModelRequestAuth,
  ModelRequestDependencies,
  ModelSelection,
  PluginReferenceCatalog,
  ResolvedUserInstructions,
  StableForkGoalBoundaryMetadata,
  StableForkTargetMetadata,
  WorkspaceHookBundleSnapshot,
  WorkspaceId,
} from "@zcode/contracts";
import type { ZCodeProviderAccountAccess } from "@zcode/shared";
import type { EffectiveModelSelectionResult } from "@zcode/shared/model-selection";
import type { RuntimeMessageEntry } from "../agent/message-history.js";
import type {
  CompactPhase,
  CompactReason,
  CompactTrigger,
  CollaborationMode,
  EmbeddedSearchBackend,
  Logger,
  AttachmentStorageMetadata,
  MessageId,
  MessageVisibility,
  FilePartSource,
  ModelRequestAdmission,
  Model,
  ModelNetworkStatusEvent,
  ModelMessageContentBlock,
  ModelReasoningContentBlock,
  ModelStreamRecoveryStatus,
  ModelToolCall,
  ModelToolContract,
  ModelUsage,
  ModelUsageSummary,
  ModelInputMessage,
  MessageWithParts,
  PendingTurnInput,
  TurnInputIntentMetadata,
  TurnSteerResult,
  PartId,
  PermissionBrokerPort,
  PermissionUpdate,
  QueryId,
  RewindScope,
  RewindStrategy,
  SessionEvent,
  SessionEventSink,
  SessionEventStorePort,
  SessionId,
  SessionTaskType,
  SessionMailboxPort,
  SessionProjection,
  SessionStorePort,
  ContextSourcePort,
  DynamicWorkflowRunPort,
  DynamicWorkflowSnippetPort,
  ModelCatalogPort,
  ExecutionPort,
  BrowserControlPort,
  ExecutionShellSelection,
  AutomationPort,
  OffPeakPort,
  FileSystemPort,
  HttpClientPort,
  ImageProcessorPort,
  PdfDocumentPort,
  HooksRuntimeConfig,
  SkillPort,
  McpPort,
  McpServerConfig,
  SubagentPort,
  ToolArtifactStorePort,
  ToolCallId,
  WorkflowPort,
  WorkflowEscalatePort,
  WorkflowSubmitPort,
  TraceContext,
  TraceId,
  TurnId,
  CheckpointCreatedPayload,
  RewindTargetEvaluation,
  SessionHistoryHydrationResult,
  SyntheticUserMessageSource,
  HookRunner,
  ToolExecutionResult,
  ToolExecutor,
  ToolRegistry,
  ContextBuilder,
  EnvInfo,
  ProjectContext,
  UserInstructionsOptions,
  AutoCompactPolicyConfig,
  ModelAnomalyGuardConfig,
  OutputStylePromptConfig,
} from "./deps.js";
import type { AgentProfile } from "../subagent/profile.js";
import type { RuntimeTaskRegistry } from "../runtime-task/registry.js";
import type { BashTimeoutPolicy } from "../tool/bash-timeout-policy.js";
import type { PresentationSurface } from "../context/types.js";
import type { WorkspaceHookRuntimeAdmissionPort } from "../hooks/workspace-hook-runtime-admission.js";

// -----------------------------------------------
// Agent Runtime
// -----------------------------------------------

export interface AgentRuntimeConfig {
  /** shared-host CUA request routing metadata; desktop is the safe default. */
  clientMode?: "desktop-continuous" | "web-remote-replayable";
  deliveryKind?: "desktop-continuous" | "web-remote-replayable";
  remoteSessionId?: string;
  bashTimeoutPolicy?: BashTimeoutPolicy;
  presentationSurface?: PresentationSurface;
  mode?: CollaborationMode;
  planEnabled?: boolean;
  modelStreaming?: "off" | "on";
  streamingToolExecution?: "off" | "readOnly";
/** Session is fixed when created; by default, the shared model context budget default strategy is used. */
  modelContextBudgetStrategy?: "legacy" | "preflight-v1";
  maxTurns?: number;
  permissionTimeoutMs?: number;
  compact?: AutoCompactPolicyConfig;
  targetCompletionVerification?: { enabled?: boolean };
  midConversationSystem?: {
    mode?: "auto" | "force";
  };
  subagents?: {
    enabled?: boolean;
// The foreground subagent does not have a maximum silent time for any child events; the default alignment model stream idle timeout.
    inactivityTimeoutMs?: number;
    autoBackgroundMs?: number;
    backgroundBashMaxMs?: number;
    maxTurns?: number;
    outputRootDir?: string;
    profiles?: readonly AgentProfile[];
    builtInModelSelectionOverrides?: Partial<Record<"general-purpose" | "Explore", ModelSelection>>;
  };
  toolAllowlist?: readonly string[];
  toolDisallowlist?: readonly string[];
  /**
   * Defaults to main. Explore child runtimes use the explore toolset to opt into
* Read-only exploration tool whitelist; whether direct Glob/Grep is included or not is determined by the embedded search branch.
* In main mode, allowlist only performs direct intersection filtering, and the default read-only whitelist will be completed when explore sub-run.
   */
  toolset?: "main" | "explore";
  toolConcurrency?: { maxConcurrency?: number };
  runtimeFeatures?: {
    /**
* Whether to register the node_repl tool (js).
* Deduced by bootstrap based on the start and stop of the ZCode official plug-in, it is not self-declared by the ordinary plug-in manifest.
     */
    nodeRepl?: boolean;
    /**
* Whether to allow node_repl to inject agent.browsers. The host also needs to provide a browserControlPort.
     */
    browserUse?: boolean;
/** Whether to inject CUA broker credentials into the shared node_repl; it does not mean registering an independent CUA MCP. */
    computerUse?: boolean;
    /**
* Official browser-use plugin's docs asset catalog. Deduced by bootstrap from plugin metadata.rootPath,
* Does not belong to plugin manifest schema.
     */
    browserDocumentationRoot?: string;
  };
  modelAnomalyGuard?: Partial<ModelAnomalyGuardConfig>;
  mcp?: {
    enabled?: boolean;
    servers?: Record<string, McpServerConfig>;
    /**
     * Process-local provenance supplied by bootstrap after resolving bundled
     * official plugins. Never derive this list from serialized MCP config.
     */
    trustedOfficialCuaServerNames?: readonly string[];
  };
  /**
* Session frozen Plugin identity catalog.
* Built by bootstrap from the plugin loader results when the app is created; runtime read-only,
* Used for turn start to parse `plugin://` reference and intersect with live inventory.
   */
  pluginReferenceCatalog?: PluginReferenceCatalog;
  hooks?: HooksRuntimeConfig;
  bashShellSelection?: ExecutionShellSelection | undefined;
  embeddedSearchBackend?: EmbeddedSearchBackend;
/** Root Session runtime is fixed when created; false only turns off Bash's bfs/ugrep prelude. */
  nativeSearchEnhancementsEnabled?: boolean;
  memory?: MemoryRuntimeConfig;
/** History recovery allows unbinding; only complete selection can create the execution model of this round. */
  modelSelection?: ModelSelection;
  titleGeneration?: {
    enabled?: boolean;
    modelSelection?: ModelSelection;
    timeoutMs?: number;
  };
  parentSessionId?: SessionId;
  taskType?: SessionTaskType;
  /**
* Dynamic workflow grayscale gate: Host is determined after
* ZCode Protocol is distributed and runtime only consumes it. **On in absence** - TUI, headless `-p` and
* None of the workflow_child will set it, they must retain the full toolface; only trusted Hosts
* Only the created protocol session will explicitly write false to turn off ten workflow tools.
   */
  dynamicWorkflowEnabled?: boolean;

  // Context Builder config
  systemPrompt?: string;
  /**
* Identity input for dynamic workflow subagent: give on the spot
* The context builder takes the path of "base + workflow sub-agent contract + persona overlay" instead of treating persona as
* `systemPrompt` replaces the entire section. Mutually exclusive with `systemPrompt` (builder throws an error).
   */
  workflowActor?: { name?: string; persona?: string };
  /**
   * Selects the subagent-specific context builder for child runtimes. The
   * builder still receives env/date/model data through the normal runtime
   * context snapshot, but assembles provider-visible system sections with the
   * subagent prompt shape instead of the main ContextBuilder stack.
   */
  subagentContext?: {
    agentPrompt: string;
    userInstructions?: ResolvedUserInstructions;
  };
  language?: string;
  outputStyle?: OutputStylePromptConfig;
  agentName?: string; // Default: "zcode-agent"
  workingDirectory?: string; // Required for context builder
  /**
* The actual workspace path passed in by the caller is used for session persistence and local identity recovery.
* File and command execution still only use the normalized workingDirectory.
   */
  workspacePath?: string;
/** Only used for persistence isolation; files and command execution still use workingDirectory. */
  workspaceIdentity?: WorkspaceId;
  envInfo?: EnvInfo; // Optional, will be auto-detected if not provided
  currentDate?: string; // YYYY-MM-DD, resolved by adapter when omitted
  userInstructions?: UserInstructionsOptions; // AGENTS.md
  projectContext?: ProjectContext; // auto-detected if not provided
  skillMetadataBudget?: number;
}

export interface ResumeSessionOptions {
/** Abort cold-resume admission wait; will not forge Workspace Hook review decision. */
  abortSignal?: AbortSignal;
  traceContext?: TraceContext;
/** Cold recovery call-level materialized results provided by the caller; do not enter the life cycle cache, and re-read according to the return value after patching. */
  persistedMessages?: MessageWithParts[];
  /**
* The mode parsed by this invocation. Explicit --mode and headless default yolo are both call-level overrides.
* Must be higher than the historical session mode; remain undefined when interactive resume is not specified, allowing the historical mode to take effect.
   */
  modeOverride?: CollaborationMode;
}

export interface MainTurnCacheHitAggregate {
  requestCount: number;
  totalInputTokens: number;
  totalCacheReadTokens: number;
  totalCacheWriteTokens: number;
}

export type BackgroundTaskNotificationSealReason = "subagent_terminal" | "subagent_cancelled";

export interface SealBackgroundTaskNotificationsInput {
  reason: BackgroundTaskNotificationSealReason;
  traceContext?: TraceContext;
}

export interface PendingModelChangeTimeline {
  createdAt: number;
  fromModel?: ModelSelection;
  fromModelLabel?: string;
  requestId: string;
  toModel: ModelSelection;
  toModelLabel: string;
}

export interface EnqueueSubagentMessageInput {
  responseId: string;
  agentId: string;
  agentType: string;
  childSessionId: SessionId;
  childToolCallId: string;
  parentToolCallId?: string;
  summary: string;
  message: string;
  traceContext: TraceContext;
}

export interface MemoryRuntimeConfig {
  cliStorageRoot?: string;
  enabled?: boolean;
/** Whether to schedule the automatic Extraction after the Main turn successfully; the default is true. */
  extractionEnabled?: boolean;
  storageRoot?: string;
  use?: boolean;
  workspaceIdentity?: string;
}

export interface AgentRuntimeDeps {
  agentTelemetry?: AgentExecutionTelemetryPort;
  agentTelemetryCausation?: AgentTelemetryCausation;
  agentTelemetryCausationMode?: "child" | "linked_root";
  appVersion?: string;
  eventStore: SessionEventStorePort;
  sessionStore?: SessionStorePort;
  sessionMailboxPort?: SessionMailboxPort;
  modelFactory: RuntimeModelFactory;
/** Optional host capability: parses explicit intent for future execution; not used to modify frozen Models. */
  resolveEffectiveModelSelection?: (selection: ModelSelection) => EffectiveModelSelectionResult;
  modelIoDir?: string;
  providerRuntimeHeadersPort?: ProviderRuntimeHeadersPort;
  permissionService?: PermissionService;
  permissionBroker?: PermissionBrokerPort;
  toolScheduler?: ToolScheduler;
  toolRegistry?: ToolRegistry;
  toolExecutor?: ToolExecutor;
  hookRunner?: HookRunner;
  workspaceHookAdmission?: WorkspaceHookRuntimeAdmissionPort;
  workspaceHookSnapshot?: WorkspaceHookBundleSnapshot;
  executionPort?: ExecutionPort;
/** browser-use control port; transparently transmitted to ToolExecutionContext.browserControlPort for use by node_repl. */
  browserControlPort?: BrowserControlPort;
  fileSystemPort?: FileSystemPort;
  httpClientPort?: HttpClientPort;
  imageProcessorPort?: ImageProcessorPort;
  pdfDocumentPort?: PdfDocumentPort;
  skillPort?: SkillPort;
  mcpPort?: McpPort;
  subagentPort?: SubagentPort;
  coordinatorResponsePort?: CoordinatorResponsePort;
/** The port for workflow actors to submit final results; it exists as a registration gate for the submit_result tool. */
  workflowSubmitPort?: WorkflowSubmitPort;
  /**
* typed `submit_result` for mono subagent: tool declaration registered when present is `{ result: <this schema> }` instead
* Any JSON. Only change
* Provider visible declarations and strict qualifications; handler, permissions, termination semantics are the same as when workflowSubmitPort is present alone.
* Ignore if there is no port (the port is the registration gate).
   */
  workflowSubmitSchema?: JsonSchema;
  /**
* Workflow actor upgrades the port that blocks the problem; there is a registration door that acts as an escalate tool. The same injection method and the same gate as workflowSubmitPort.
   */
  workflowEscalatePort?: WorkflowEscalatePort;
  /**
* Process-level admission port for model requests: when present
* Each model request attempts to get a ticket through it first; along the calling context to the adapter. dwf actor runtime takes driver's per-actor
* Packaging (subject to gate constraints), the main runtime takes the manager's observer (only feeds signals); in its absence, there is no gate.
   */
  modelRequestAdmission?: ModelRequestAdmission;
  workflowPort?: WorkflowPort;
/** The submission/observation/cancellation port of workflow run; if it exists, CreateWorkflow will be started, if it is absent, it will return to the placeholder diagnosis. */
  dynamicWorkflowRunPort?: DynamicWorkflowRunPort;
  dynamicWorkflowSnippetPort?: DynamicWorkflowSnippetPort;
/** Model directory port; if it is absent, ListModels will report that the capability is absent, and the subagent_model of CreateWorkflow will be rejected. */
  modelCatalogPort?: ModelCatalogPort;
  runtimeTaskRegistry?: RuntimeTaskRegistry;
  artifactStore?: ToolArtifactStorePort;
  automationPort?: AutomationPort;
  offPeakPort?: OffPeakPort;
  contextSourcePort?: ContextSourcePort;
  eventSink?: SessionEventSink;
  logger?: Logger;
  traceContext?: TraceContext;
  contextBuilder?: ContextBuilder; // Optional, will be created from config
  now?: () => Date;
  isRemoteWorkspace?: () => boolean;
  memoryRoot?: string;
}

export interface RuntimeModelFactoryInput {
  selection: ModelSelection;
/** Only binds to the Model created this time, and does not enter the public ModelRequest or Session persistence. */
  requestDependencies?: ModelRequestDependencies;
}

export type RuntimeModelFactory = (input: RuntimeModelFactoryInput) => Model;

/**
* Provider runtime headers port for protocol clients.
 *
* The sessionId entered as a parameter must be routed to the session held by the client. The ledger identity of the child runtime cannot
* Directly used for client requests, otherwise the client cannot find the session and return a response, and the first model request will wait forever.
* The child runtime derives ports through deriveChildClientPorts and routes requests to the client session bound to the parent port.
 */
export interface ProviderRuntimeHeadersPort {
  shouldRefreshBeforeModelRequest?(input: { providerId: string; modelId: string }): boolean;
  refreshBeforeModelRequest(input: {
    accountAccess?: ZCodeProviderAccountAccess;
    abortSignal?: AbortSignal;
    modelId: string;
    providerId: string;
    reason: "model-request";
    sessionId: SessionId;
    traceContext: TraceContext;
    turnId?: TurnId;
  }): Promise<{
    headersApplied: boolean;
    requestAuth?: ModelRequestAuth;
  }>;
}

export interface TurnResult {
  response: string;
  turnId: TurnId;
  traceId: TraceId;
  usage?: ModelUsageSummary;
  events: SessionEvent[];
  projection: SessionProjection;
}

/**
* Execution constraints for standard Submission Selection. It does not carry Provider/Model static facts,
* Nor will it become a second source for Session Selection.
 */
export interface ModelExecutionContext {
/** Only the current Turn skips automatic Project Memory Extraction; the Session Memory configuration is not modified. */
  memoryExtraction?: "skip";
  selectionScope: "execution";
  requestDependencies?: ModelRequestDependencies;
  subagents?: {
    foregroundModel: "submission";
    background: "deny";
  };
}

export interface ExecuteTurnOptionsBase {
  abortSignal?: AbortSignal;
  browserAmbientContext?: {
    tabCount: number;
    currentUrl?: string;
  };
  continueActiveTargetAfterTurn?: boolean;
  displayInput?: string;
  /**
* `input` From this index onwards is the engine text appended by the caller (dwf ask endnote/nudge). Only enter TurnStarted with users
* Message metadata is provided for GUI collapse; model history and persistent text parts are still full text (the transcription copy of amend-resume requires full text).
   */
  epilogueStart?: number;
  inputId?: string;
  intent?: TurnInputIntentMetadata;
  sharedContextRefs?: TurnInputIntentMetadata["sharedContextRefs"];
  queryId?: QueryId;
  inputSource?: SyntheticUserMessageSource;
  inputPresentation?: RuntimeInputPresentation;
  inputVisibility?: MessageVisibility;
  originMeta?: BackgroundResultOriginMeta;
/** Only used to freeze the entire batch of causal sources of background notification batch, not for display. */
  backgroundSource?: BackgroundResultOriginMeta["backgroundSource"];
/** Calculated by runtime command drain, indicating that the subagent background results have been consumed in this round. */
  backgroundSubagentResultConsumed?: boolean;
/** Calculated by runtime command drain, indicating that dynamic-workflow run notifications (completion/question) have been consumed in this round. */
  workflowResultConsumed?: boolean;
  recordedInputMessageId?: MessageId;
  skipInputRecord?: boolean;
  skipUserPromptSubmitHooks?: boolean;
  targetId?: string;
/** Tools hidden from the provider for the current turn only; does not modify the persistent tool surface of the session runtime. */
  toolDisallowlist?: readonly string[];
  traceContext?: TraceContext;
/** The current Submission Selection is only used for this execution, and can be bound to request-by-request dependencies. */
  modelExecution?: ModelExecutionContext;
}

export type ExecuteTurnOptions = ExecuteTurnOptionsBase &
  import("@zcode/contracts").TurnBackgroundAttribution;

/**
* Call parameters for Core prompt admission. Bootstrap only provides input facts and expected delivery semantics,
* The selection of start/queue is completed by the AgentRuntime atom holding the session state.
 */
export type PromptAdmissionOptions = ExecuteTurnOptions & {
  commandKind?: "sendText" | "sendGoalCommand" | "compact";
  delivery?: "auto" | "start_turn" | "steer_active_turn";
  expectedTurnId?: TurnId;
/** Product queue semantics when busy; fallback queue by Core when attachment or unsteerable. */
  queueDelivery?: "guide" | "queue";
/** Internal calls such as queue promotion require admission to be idle, otherwise it will be rejected directly. */
  requireIdle?: boolean;
};

export type PromptAdmissionReceipt =
  | {
      kind: "started";
      completion: Promise<TurnResult>;
      turnId: TurnId;
    }
  | TurnSteerResult;

export type ActiveTargetLoopTrigger = "manual" | "user-prompt" | "task-notification";

export interface ContinueActiveTargetLoopOptions {
  abortSignal?: AbortSignal;
  inputId?: string;
  intent?: TurnInputIntentMetadata;
  traceContext?: TraceContext;
  trigger: ActiveTargetLoopTrigger;
  verifyBeforeFirstContinue?: boolean;
}

export interface ActiveForegroundExecutionState {
  controller: AbortController;
  disposeParentAbort: () => void;
  foregroundExecutionId: string;
  preserveQueueAutoDrainOnCancel: boolean;
}

export interface StopActiveForegroundExecutionOptions {
  expectedForegroundExecutionId?: string;
  preserveQueueAutoDrainOnCancel?: boolean;
  reason?: string;
}

export type StopActiveForegroundExecutionResult =
  | { kind: "stopped"; foregroundExecutionId: string }
  | { kind: "idle" }
  | { kind: "mismatch"; activeForegroundExecutionId: string };

export type ForegroundPromotionLeaseMode = "after-current" | "idle-only";

export interface ForegroundPromotionLeaseState {
  leaseId: string;
  promotedInputId: string;
}

export type AcquireForegroundPromotionLeaseResult =
  | { kind: "acquired"; leaseId: string }
  | { kind: "busy" }
  | { kind: "conflict"; leaseId: string };

export interface ActiveTurnInfo {
  kind: ActiveTurnKind;
/** The inputId of this round (the same value of TurnStarted.inputId); the launch anchor point of workflow run is taken from here. */
  inputId?: string;
  queueLength: number;
  steerable: boolean;
  turnId: TurnId;
}

export interface WorkspaceRewindRestoredFile {
  action: "delete" | "restore";
  bytesWritten?: number;
  path: string;
}

export interface WorkspaceRewindResult {
  checkpoint?: CheckpointCreatedPayload;
  evaluation?: RewindTargetEvaluation;
  restoredFiles: WorkspaceRewindRestoredFile[];
  response: string;
  rewindId: string;
  strategy: RewindStrategy;
}

export type WorkspaceFileRewindAction = "restore" | "delete";

export type WorkspaceFileRewindUnsafeReason =
  | "checkpoint_missing"
  | "checkpoint_unreadable"
  | "external_modified"
  | "file_read_failed"
  | "unsupported_checkpoint";

export interface WorkspaceFileRewindSafeFile {
  action: WorkspaceFileRewindAction;
  operationCount: number;
  path: string;
  toolNames: string[];
}

export interface WorkspaceFileRewindUnsafeFile {
  currentHash?: string;
  expectedHash?: string;
  message?: string;
  operationCount: number;
  path: string;
  reason: WorkspaceFileRewindUnsafeReason;
  toolNames: string[];
}

export interface WorkspaceFileRewindIgnoredFile {
  operationCount: number;
  path: string;
  reason: "bash_ignored";
  toolNames: string[];
}

export interface WorkspaceFileRewindPreview {
  canApply: boolean;
  ignoredFiles: WorkspaceFileRewindIgnoredFile[];
  safeFiles: WorkspaceFileRewindSafeFile[];
  unsafeFiles: WorkspaceFileRewindUnsafeFile[];
}

export interface WorkspaceFileRewindApplyResult {
  applied: boolean;
  preview: WorkspaceFileRewindPreview;
  response: string;
}

export interface ConversationRewindResult {
  branchGeneration?: number;
  evaluation?: RewindTargetEvaluation;
  keptMessageCount: number;
  response: string;
  rewindId: string;
  strategy: RewindStrategy;
  targetMessageId: MessageId;
}

export interface WorkspaceForkResult {
  checkpoint?: CheckpointCreatedPayload;
  copiedMessageCount: number;
  forkedSessionId: SessionId;
  parentSessionId: SessionId;
  targetMessageId: MessageId;
  targetCheckpointId?: string;
  restoredFiles: WorkspaceRewindRestoredFile[];
  response: string;
}

/** V4 resolver Fixed target product turn raw transcript segment. */
export type StableConversationForkTarget = StableForkTargetMetadata;

/** Explicit none or complete fork point goal/verifier snapshot; undefined does not belong to new data. */
export type StableConversationForkGoalBoundary = StableForkGoalBoundaryMetadata;

export type StableConversationForkChildMetadata = ForkChildSessionMetadata;

export interface StableConversationForkOptions {
  forkedSessionId?: SessionId;
  modelSelection?: ModelSelection;
  goalBoundary: StableConversationForkGoalBoundary;
  sourceCommandId: string;
  revisionAtDecision?: number;
  target: StableConversationForkTarget;
  traceContext?: TraceContext;
}

/** Create a hidden secondary screen child from the stable drop boundary of the parent session; do not copy the goal/queue/blocking running state. */
export interface SelectionSideChatCreateOptions {
  modelSelection?: ModelSelection;
  sourceCommandId: string;
  revisionAtDecision?: number;
  traceContext?: TraceContext;
}

export interface ConversationBeforeInputForkOptions {
  modelSelection?: ModelSelection;
  forkedSessionId?: SessionId;
  goalBoundary: StableConversationForkGoalBoundary;
  sourceCommandId: string;
  targetMessageId: MessageId;
  targetProductTurnId: string;
  targetTranscriptTurnId: string;
  initialInput: ForkCommitBundle["initialInput"];
  commandFact: ForkCommitBundle["commandFact"];
  traceContext?: TraceContext;
}

export interface WorkspaceCheckpointSummary {
  checkpointId: string;
  compactBoundaryId?: string;
  coveredByCompact?: boolean;
  createdAt: Date;
  diffRef?: string;
  fileCount?: number;
  messageId: MessageId;
  targetMessageId?: MessageId;
  toolMessageId?: MessageId;
  preview?: string;
  scope: RewindScope;
  snapshotRef: string;
}

export interface RuntimeTurnFileChangeEntry {
  afterContent?: string;
  beforeContent: string | null;
  fallbackAdditions: number;
  fallbackDeletions: number;
  path: string;
  toolNames: Set<string>;
  writeCount: number;
}

export type RuntimeTurnFileChangeMap = Map<string, RuntimeTurnFileChangeEntry>;

export interface CompactTimelineContext {
  operationId: string;
  messageId: MessageId;
  partId: PartId;
  trigger: CompactTrigger;
  phase: CompactPhase;
  compactReason: CompactReason;
  sourceCommandId?: string;
  startedAt: number;
  preCompactTokenCount?: number;
}

export interface ResumeSessionResult extends SessionHistoryHydrationResult {
  directory: string;
/** Current recovery candidates: Only valid model identities are allowed for interface selection, which does not mean that the runtime has been bound. */
  modelSelection?: ModelSelection;
/** Whether resume writes back the compact patching facts that cannot be fully reflected by the current materialization. */
  persistedMessagesReloadRequired: boolean;
  readFileStateRestoredCount: number;
  readFileStateSkippedRangeReadCount: number;
  readFileStateSkippedUnreadableEditCount: number;
  traceId: TraceId;
}

export interface PermissionDecisionResult {
  allowed: boolean;
  reason?: string;
  modifiedInput?: unknown;
  permissionUpdates?: PermissionUpdate[];
}

export interface ExecuteToolsOptions {
  automationTurn?: boolean;
  offPeakTurn?: boolean;
  signal?: AbortSignal;
  traceContext?: TraceContext;
/** Only transparently transmitted to the Agent child waiting for the current turn synchronously. */
  subagentModelOverride?: import("@zcode/contracts").SubagentRunOptions["modelOverride"];
  model?: Model;
  onBatchStart?: (toolCallIds: string[]) => Promise<void>;
}

export interface ExecuteToolsResult {
  results: ToolExecutionResult[];
  events: SessionEvent[];
}

export type ActiveTurnKind = "regular" | "compact" | "rewind";

export const INLINE_TEXT_ATTACHMENT_MAX_BYTES = 64 * 1024;

export const INLINE_MEDIA_ATTACHMENT_MAX_BYTES = 20 * 1024 * 1024;

export const MAX_IMAGE_ATTACHMENT_DIMENSION = 2000;

export interface ResolvedTurnAttachment {
  contentBlock: ModelMessageContentBlock;
  filename?: string;
  metadata: AttachmentStorageMetadata;
  mime: string;
  source?: FilePartSource;
  url: string;
}

export interface PreparedImageData {
  dataUrl: string;
  mediaType: string;
  metadata?: AttachmentStorageMetadata["image"];
}

export interface ActiveTurnSteeringState {
  kind: ActiveTurnKind;
// When active turn is Stopped, goal reminder cannot be inserted into tool results that have not yet been closed.
// deferral is only opened within the regular model/tool ​​loop, and is closed before materializing the pending when exiting the loop.
  goalStateChangeReminderDeferralOpen: boolean;
  pendingGoalStateChangeReminder?: {
    text: string;
  };
  pendingInputs: PendingTurnInput[];
  steerable: boolean;
  traceContext: TraceContext;
  turnId: TurnId;
/** The inputId of this round (same origin as TurnStarted.inputId); the regular turn is recorded when beginActiveTurn. */
  inputId?: string;
}

export interface ActiveTurnStartReservation {
  kind: ActiveTurnKind;
  traceContext: TraceContext;
  turnId: TurnId;
}

export interface DrainedPendingInputDiagnostics {
  injectedMessageIds: MessageId[];
/** The atomic Submission configuration carried by the inline Guide itself; consumed by the next model-step boundary. */
  intent?: TurnInputIntentMetadata;
  latestMessageId: MessageId | undefined;
  pendingInputIds: string[];
  queryIds?: QueryId[];
/** This drain has submitted the immutable entries of canonical history for synchronous advancement of turn-local query. */
  runtimeEntries: readonly RuntimeMessageEntry[];
/** The tool hidden list attached to the input injected by this drain must continue to take effect on the next provider request. */
  toolDisallowlist?: readonly string[];
}

export interface ProviderContextUsageSnapshot {
  cacheReadTokens?: number;
  cacheWriteTokens?: number;
  contextUsageTokens?: number;
  inputTokens: number;
  messageCount: number;
  model: { providerId: Model["providerId"]; modelId: Model["modelId"] };
  outputTokens?: number;
  recordedAt: number;
  traceId: TraceId;
  turnId?: TurnId;
}

export interface RunModelTextRequestOptions {
  abortSignal?: AbortSignal;
  assistantMessageId: MessageId;
  events: SessionEvent[];
  maxOutputTokens?: number;
  latestRealUserMessageIndex?: number;
  messages: ModelInputMessage[];
/** The canonical source corresponding to messages by index, only used for local usage statistics. */
  sourceEntries?: readonly (RuntimeMessageEntry | undefined)[];
  model: Model;
  onStreamSnapshot?: (snapshot: RuntimeModelStreamSnapshot) => void;
  onStreamReasoningDelta?: (text: string) => void;
  onStreamTextDelta?: (text: string) => void;
  onStreamToolCall?: (toolCall: ModelToolCall) => void;
  onModelNetworkStatus?: (event: ModelNetworkStatusEvent) => void;
  streamRecovery?: ModelStreamRecoveryStatus;
  tools: ModelToolContract[];
  traceContext: TraceContext;
}

export interface RuntimeModelStreamSnapshot {
  reasoning: ModelReasoningContentBlock[];
  text: string;
}

export interface StreamedToolExecutionResult {
  input: Record<string, unknown>;
  ledgerRecorded?: boolean;
  partID: PartId;
  result: ToolExecutionResult;
  toolCallId: ToolCallId;
}

export interface RuntimeModelTextResult {
  contextUsageBreakdown?: ContextUsageBreakdownItem[];
  finishReason: string;
  providerMetadata?: Record<string, unknown>;
  reasoning?: ModelReasoningContentBlock[];
  text: string;
  toolCalls?: ModelToolCall[];
  usage: ModelUsage;
}

export type ParsedRewindCommand =
  | {
      action: "apply";
      targetCheckpointId?: string;
    }
  | {
      action: "fork";
      targetCheckpointId?: string;
    }
  | {
      action: "cascade-message";
      scope: RewindScope;
      targetMessageId: MessageId;
    }
  | {
      action: "message";
      scope: RewindScope;
      targetMessageId: MessageId;
    }
  | {
      action: "status";
    };

export type ContextUsageTokenMethod = "estimated" | "provider_count" | "proportional_estimate";

export type ContextUsageConfidence = "high" | "medium" | "low";

export interface ContextUsageMetric {
  chars: number;
  confidence: ContextUsageConfidence;
  tokenMethod: ContextUsageTokenMethod;
  tokenizer: string;
  tokens: number;
}

export interface ContextUsageCategory extends ContextUsageMetric {
  name: string;
  percentTokens: number;
  source:
    | "system_prompt"
    | "meta_user_context"
    | "skills"
    | "tool_prompt"
    | "system_tool_schemas"
    | "mcp_tool_schemas"
    | "messages";
}

export interface ContextUsageToolDetail extends ContextUsageMetric {
  name: string;
  readOnly?: boolean;
  serverName?: string;
  sideEffectScope?: string;
  source: "system_tool" | "mcp_tool";
}

export interface ContextUsageSkillDetail extends ContextUsageMetric {
  name: string;
  path: string;
  scope: string;
  source: string;
}

export interface ContextUsageMessageRoleBreakdown extends ContextUsageMetric {
  count: number;
  role: string;
}
