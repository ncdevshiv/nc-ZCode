/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import type { ZCodeToolExecResource, BackgroundBashOutputResult } from "@zcode/shared";
import type { AiSdkModelAdapter } from "@zcode/adapters/model";
import type {
  AgentRuntime,
  AgentRuntimeConfig,
  ExecuteTurnOptions,
  ExpertWorkflowCommandResult,
  ProviderRuntimeHeadersPort,
  PresentationSurface,
  ResumeSessionResult,
  StartSavedWorkflowRunResult,
  AmendWorkflowRunSettingsInput,
  AmendWorkflowRunSettingsResult,
  TurnAttachment,
  ModelExecutionContext,
  TurnResult,
  WorkflowAgentRunner,
  WorkspaceCheckpointSummary,
  WorkspaceForkResult,
  WorkspaceGenerateTextInput,
  WorkspaceHookReviewTarget,
  WorkspaceHookPolicyProvider,
} from "@zcode/core";
import type {
  WorkspaceHookReviewDecision,
  WorkspaceHookTrustRevokeTarget,
} from "@zcode/shared/zcode-protocol-v4";
import type { ZCodeModelOption } from "@zcode/shared";
import type { EffectiveModelSelectionResult } from "@zcode/shared/model-selection";
export type { ZCodeModelOption } from "@zcode/shared";
import type { ModelProviderSourceTitle } from "../model-config.js";
import type { ZCodeInstalledPluginData } from "../plugins.js";
import type {
  AutomationPort,
  OffPeakPort,
  BackgroundTaskCancelResult,
  CollaborationMode,
  ContextSourcePort,
  DynamicWorkflowRunArtifact,
  DynamicWorkflowRunArtifactBytes,
  DynamicWorkflowRunArtifactItem,
  DynamicWorkflowRunWorkspaceNode,
  DynamicWorkflowRunWorkspaceNodeResult,
  DynamicWorkflowRunEvent,
  DynamicWorkflowRunResumeResult,
  DynamicWorkflowRunProgressPayload,
  DynamicWorkflowRunSessionSummary,
  ExecutionPort,
  BrowserControlPort,
  FileSystemPort,
  GoalStatus,
  HttpClientPort,
  ImageProcessorPort,
  PdfDocumentPort,
  InputDelivery,
  InputHistoryEntry,
  InputHistoryKind,
  InputHistoryStorePort,
  LoggerFactory,
  McpPort,
  McpServerStatus,
  ModelSelection,
  PermissionBrokerPort,
  PluginLoadOutcome,
  PluginMetadata,
  PluginReferenceCatalog,
  SessionEvent,
  SessionEventSink,
  SessionEventStorePort,
  SessionGoal,
  SessionId,
  SessionMailboxPort,
  SessionStorePort,
  SkillLoadOutcome,
  SkillPort,
  ToolArtifactReadResult,
  ToolArtifactStorePort,
  TodoItem,
  QueryId,
  TraceContext,
  TurnId,
  TurnSteerResult,
  TurnInputIntentMetadata,
  MessageWithParts,
  ModelUsage,
  ModelToolCall,
  SupportedLocale,
  UiLocale,
  UiThemePreference,
  WorkflowEvent,
  WorkflowRunListItem,
  ExecutionShellSelection,
} from "@zcode/contracts";
import type { NodeReplBrowserBroker } from "./node-repl-browser-broker.js";
import type { SessionTranscriptMessage } from "../session-transcript.js";
import type { WorkspaceHookReviewCommandResult } from "./workspace-hook-review-controller.js";
import type { AgentTelemetryRuntimeOwner, WorkspaceHookPolicy } from "@zcode/contracts";
import type { ProviderRegistryModelSource } from "./provider-registry-model-runtime.js";

export interface WorkspaceHookReviewHostContext {
  taskId: string;
  runId: string;
  workspaceLabel: string;
  remoteSessionId?: string;
}

export type RespondWorkspaceHookReviewInput = WorkspaceHookReviewTarget & {
  decision: WorkspaceHookReviewDecision;
};

export type ToggleWorkspaceHookReviewItemInput = WorkspaceHookReviewTarget & {
  reviewItemId: string;
  enabled: boolean;
};

export type RevokeWorkspaceHookTrustInput =
  | (WorkspaceHookReviewTarget & { reviewItemIds: string[] })
  | WorkspaceHookTrustRevokeTarget;

/** The new Session can use the Environment default selection; the restored Session is allowed to remain unbound and does not add the default model. */
export type ZCodeAppRuntimeConfigInput = AgentRuntimeConfig;

export interface ZCodeAppOptions {
  sessionId?: SessionId;
  resume?: boolean;
  version?: string;
  traceContext?: TraceContext;
  runtimeConfig?: ZCodeAppRuntimeConfigInput;
  /**
* The agent process of stdio protocol mode is started by Electron host, and the model service needs to see the electron source.
* Ordinary CLI does not pass, continue to use the cli default value.
   */
  sourceTitle?: ModelProviderSourceTitle;
  eventStore?: SessionEventStorePort;
  sessionStore?: SessionStorePort;
  sessionMailboxPort?: SessionMailboxPort;
  inputHistoryStore?: InputHistoryStorePort;
  modelAdapter?: AiSdkModelAdapter;
/** Registry owned by the Worker process; App only borrows and is not responsible for releasing. */
  providerRegistry: ProviderRegistryModelSource;
  resolveEffectiveModelSelection?: (selection: ModelSelection) => EffectiveModelSelectionResult;
/** The default selection of Environment used by new Sessions; only participates in initialization when there is no explicit runtime modelSelection. */
  configuredDefaultModelSelection?: ModelSelection;
  modelIoFullRetentionEnabled?: boolean;
/** The same process embedded host can inject the complete borrowed process-level Owner; the Endpoint configuration must not override it. */
  telemetryOwner?: AgentTelemetryRuntimeOwner;
  /**
* Provider runtime headers port: The main runtime reports its own session every time it is called; the child runtime always reports to the parent
* runtime takes the derived instance.
   */
  providerRuntimeHeadersPort?: ProviderRuntimeHeadersPort;
  loggerFactory?: LoggerFactory;
  officialPluginRoots?: string[];
  pluginStorageRoot?: string;
  executionPort?: ExecutionPort;
/** Resource telemetry bypass; injected by the protocol host, shared by the execution adapter of the main task and workflow. */
  onToolExecResource?: (sample: ZCodeToolExecResource) => void;
/** browser-use control port; agent.browsers.* of node_repl is available after injection. Not available by default. */
  browserControlPort?: BrowserControlPort;
/** Process-level node_repl Browser broker that can be injected by the protocol host; by default, the app builds and owns it. */
  nodeReplBrowserBroker?: NodeReplBrowserBroker;
  fileSystemPort?: FileSystemPort;
  httpClientPort?: HttpClientPort;
  imageProcessorPort?: ImageProcessorPort;
  pdfDocumentPort?: PdfDocumentPort;
  artifactStore?: ToolArtifactStorePort;
  contextSourcePort?: ContextSourcePort;
  skillPort?: SkillPort;
  mcpPort?: McpPort;
/** Provided by the host per-app lease; the output port is owned by the app. */
  mcpPortFactory?: (input: { workingDirectory?: string }) => McpPort;
  permissionBroker?: PermissionBrokerPort;
  eventSink?: SessionEventSink;
  env?: NodeJS.ProcessEnv;
  platform?: NodeJS.Platform | string;
  projectConfigPath?: string;
  skipUserConfig?: boolean;
  userConfigPath?: string;
  uiDetectedLocale?: string | null;
  uiLocale?: UiLocale;
  onWorkflowEvent?: (event: WorkflowEvent) => void | Promise<void>;
  automationPort?: AutomationPort;
  offPeakPort?: OffPeakPort;
/** Parsed once when the first real user executes or cold-resume fallback, and then cached by the app life cycle. */
  resolveInitialBashShellSelection?: () => Promise<ExecutionShellSelection | undefined>;
  /** Trusted embedder policy; workspace/project files cannot populate this field. */
  workspaceHookPolicy?: WorkspaceHookPolicy;
  /** Protocol Host-owned provider shared by session Runtime and no-session Settings pretrust. */
  workspaceHookPolicyProvider?: WorkspaceHookPolicyProvider;
  /** Rollout gate; false keeps project Hooks hard-blocked and does not read Trust records. */
  workspaceHookTrustEnabled?: boolean;
  /** Presence means this owner Host supports the dedicated Workspace Hook review route. */
  workspaceHookReviewHost?: WorkspaceHookReviewHostContext;
}

export interface SubmitPromptOptionsBase {
  traceContext?: TraceContext;
  abortSignal?: AbortSignal;
  inputId?: string;
  queryId?: QueryId;
  intent?: TurnInputIntentMetadata;
  sharedContextRefs?: TurnInputIntentMetadata["sharedContextRefs"];
  onEvent?: (event: SessionEvent) => void | Promise<void>;
/** Internal admission observation point: It only means that the runtime sink saw TurnStarted, but it does not mean that the projection has been applied. */
  onTurnStartedObserved?: (event: SessionEvent) => void;
/** Only the current turn is removed from the provider tool list; the session runtime is not permanently changed. */
  toolDisallowlist?: readonly string[];
/** App only reads the provided provider-only IAB environment status and does not enter the UI transcript. */
  browserAmbientContext?: ExecuteTurnOptions["browserAmbientContext"];
/** Single execution constraint for standard Selection; does not enter Session Selection or persistence. */
  modelExecution?: ModelExecutionContext;
}

export type SubmitPromptOptions = SubmitPromptOptionsBase &
  import("@zcode/contracts").TurnBackgroundAttribution;

export type PrepareUserExecutionBoundary = (
  options?: Pick<SubmitPromptOptions, "abortSignal" | "traceContext">,
) => Promise<void>;

export interface SteerTurnOptions {
  inputId?: string;
  queryId?: QueryId;
  expectedTurnId?: TurnId;
  commandKind?: "sendText" | "sendGoalCommand" | "compact";
/** Delivery semantics: queue=cut a new round when consuming; guide=inline the current round. Default queue. */
  delivery?: "guide" | "queue";
  intent?: TurnInputIntentMetadata;
  attachments?: TurnAttachment[];
/** The current queued/guide input tool name is not exposed to the provider when consuming. */
  toolDisallowlist?: readonly string[];
  onEvent?: (event: SessionEvent) => void | Promise<void>;
  traceContext?: TraceContext;
}

export type SendInputOptions = SubmitPromptOptions & {
  inputId?: string;
/** Presentation tag determined by the trusted consumption portal; does not change the original user content or scheduling semantics. */
  inputPresentation?: ExecuteTurnOptions["inputPresentation"];
  delivery?: InputDelivery;
/** Product guide/queue intent of sendText; whether busy is still determined by Core admission. */
  queueDelivery?: "guide" | "queue";
  requireIdle?: boolean;
  expectedTurnId?: TurnId;
  commandKind?: "sendText" | "sendGoalCommand" | "compact";
};

export interface UserPromptInput {
  text: string;
  attachments?: TurnAttachment[];
}

export type PromptInput = string | UserPromptInput;

export type SendInputResult =
  | {
/** Core has completed admission; completion is only for life cycle cleanup, not ACK waiting boundary. */
      completion: Promise<TurnResult>;
      kind: "started_turn";
      turnId: TurnId;
    }
  | TurnSteerResult;

export interface ResumeOptions {
  abortSignal?: AbortSignal;
  traceContext?: TraceContext;
  onEvent?: (event: SessionEvent) => void | Promise<void>;
/** The call-level materialized results of the same cold recovery; do not enter the app life cycle cache. After compact is patched, it must be refreshed according to the return value. */
  persistedMessages?: MessageWithParts[];
}

export interface ZCodePluginSetResult {
  enabled: boolean;
  path: string;
  plugin: PluginMetadata;
}

export interface ZCodePluginUninstallResult {
// null means that the plugin id is currently not installed (idempotent no-op), and the caller will prompt "not installed" accordingly.
  removed: ZCodeInstalledPluginData | null;
}

export interface SetLocaleResult {
  configPath: string;
  locale: SupportedLocale;
  previousLocale: SupportedLocale;
  requestedLocale: UiLocale;
  traceId: TraceContext["traceId"];
}

export interface ZCodeApp {
  readonly sessionId: SessionId;
  readonly traceId: string;
  readonly runtime: AgentRuntime;
  respondWorkspaceHookReview(
    input: RespondWorkspaceHookReviewInput,
  ): Promise<WorkspaceHookReviewCommandResult>;
  toggleWorkspaceHookReviewItem(
    input: ToggleWorkspaceHookReviewItemInput,
  ): Promise<WorkspaceHookReviewCommandResult & { request?: unknown }>;
  revokeWorkspaceHookTrust(
    input: RevokeWorkspaceHookTrustInput,
  ): Promise<WorkspaceHookReviewCommandResult>;
/** Soft access control: open the audit flow on demand, and it is safe no-op when there are no pending items */
  requestWorkspaceHookReview(input: {
    workspaceIdentity: string;
    bundleDigest: string;
  }): Promise<WorkspaceHookReviewCommandResult>;
  /**
* Trust store is the coordinator of this session after the order is placed.
* The memory image will not be automatically updated (per-session, only loaded once when created). Settings without task
* After pretrust authorization is successful, the server will call this method one by one according to the workspace and reload the file into
* coordinator and resend the admission status, otherwise the trusted Hook will continue to be rejected and the banner will not be refreshed.
   */
  reloadWorkspaceHookTrust(): Promise<void>;
  close?(): Promise<void>;
  getMode(): CollaborationMode;
  getModel(): string;
/** current-only protocol snapshot reads the current Registry model exactly, avoiding enumeration of the entire directory. */
  getCurrentModelOption?(): ZCodeModelOption | undefined;
/** Read-only Registry metadata; does not require options to be complete and does not bind an execution model. */
  getModelOption?(selection: ModelSelection): ZCodeModelOption | undefined;
  getLocale(): SupportedLocale;
  getTheme(): UiThemePreference;
  getDefaultThoughtLevel(): string | undefined;
  getThoughtLevel(): string | undefined;
  loadSessionTranscript(): Promise<SessionTranscriptMessage[]>;
  readSubagents(input?: {
    endedCursor?: string;
    endedLimit?: number;
  }): Promise<import("@zcode/shared").ZCodeSessionSubagentsResult>;
  readSubagentTranscript(
    childSessionId: string,
  ): Promise<import("./subagent-observation.js").SubagentTranscriptSnapshot>;
  readTodos(): Promise<TodoItem[]>;
  readTarget(): Promise<SessionGoal | null>;
  setCustomSessionTitle(input: { title: string; traceContext?: TraceContext }): Promise<void>;
  readToolResultArtifact(uri: string): Promise<ToolArtifactReadResult>;
/** After chunk transaction commit, the complete binary atomic is stored in the session artifact store. */
  writePromptAttachment(input: {
    fileName: string;
    mime: string;
    bytes: Uint8Array;
  }): Promise<{ ref: string }>;
/** Sent image/video preview: Read the artifact or actual path within the owning session's runtime. */
  readPromptAttachment(input: {
    ref: string;
    mime: string;
    maxBytes: number;
    messageId?: string;
    attachmentIndex?: number;
  }): Promise<{ bytes: Uint8Array; mediaType: string }>;
/** Share read-only userInput attachment metadata during the selection phase; the complete content is not read. */
  statPromptAttachment?(input: {
    ref: string;
    mime: string;
    messageId?: string;
    attachmentIndex?: number;
  }): Promise<{ totalBytes: number; mediaType: string; mtimeMs?: number }>;
/** Desktop local sent video: parse artifact-first local playback source; otherwise keep reading in slices. */
  resolvePromptAttachmentPreviewSource(input: {
    ref: string;
    mime: string;
    messageId?: string;
    attachmentIndex?: number;
  }): Promise<{ kind: "local_path"; path: string; mediaType: string } | { kind: "chunked" }>;
  setTarget(input: {
    objective: string;
/** The original goal command submitted by the user; target still only contains objective, and the chat line is displayed according to this text. */
    displayText?: string;
    status?: GoalStatus;
    tokenBudget?: number | null;
    intent?: TurnInputIntentMetadata;
  }): Promise<SessionGoal>;
  updateTargetStatus(status: GoalStatus): Promise<SessionGoal | null>;
  clearTarget(): Promise<boolean>;
  continueActiveTarget(options?: SubmitPromptOptions): Promise<TurnResult | null>;
  recordInputHistory(
    input: PromptInput,
    kind?: InputHistoryKind,
  ): Promise<InputHistoryEntry | null>;
  recallPreviousInputHistory(skip?: number): Promise<InputHistoryEntry | null>;
  listModels(): ZCodeModelOption[];
  listThoughtLevels(): string[];
  listPlugins(): Promise<PluginLoadOutcome>;
  setPluginEnabled(plugin: string, enabled: boolean): Promise<ZCodePluginSetResult>;
  uninstallPlugin(plugin: string): Promise<ZCodePluginUninstallResult>;
  /**
* Session frozen Plugin identity catalog.
* Constructed once by the resolveStartupPlugins result when the App is created, and then read-only;
* When `plugins/referenceCatalog` contains sessionId, this is used as the session authority.
   */
  getPluginReferenceCatalog(): PluginReferenceCatalog;
/** The AgentRuntime Skill discovery snapshot of the current Session; rediscover it after cold recovery and rebuilding the runtime. */
  getSkillCatalog(): Promise<SkillLoadOutcome>;
  listMcpServers(): Promise<Record<string, McpServerStatus>>;
  connectMcpServer(name: string): Promise<McpServerStatus>;
  readBackgroundBashOutput(workId: string, sessionId?: string): Promise<BackgroundBashOutputResult>;
  cancelBackgroundTask?(
    taskId: string,
    options?: { traceContext?: TraceContext },
  ): Promise<BackgroundTaskCancelResult>;
  /**
* Event log pagination of workflow run (details page audit page). Optional capability: run service when dwf journal is unavailable
* Entirely unstructured, this method is absent, and the gateway's ability to return structure accordingly does not support errors rather than empty pages——
* "No event" and "This session does not have this ability" are two different things.
   *
* cursor = journal sequence (`appendEvent` monotonic allocation), with workflowRuns[].lastEventSequence
* The same ruler; out of bounds cursor returns an empty page without reporting an error.
   */
  listDynamicWorkflowRunEvents?(input: {
    runId: string;
    afterSequence?: number;
    limit?: number;
  }): Promise<DynamicWorkflowRunEvent[]>;
  /**
* Resume a dwf run. Optional ability, absence conditions are the same as
* {@link listDynamicWorkflowRunEvents}. The success path is responsible for **tracking the heavy arm** in addition to port.resume
* (runtime.trackResumedDynamicWorkflowRun): If you miss it, the restored run cannot be canceled and the completion notification is lost.
* Sessions are treated as idle by recycling guardrails. Failure is returned with a structured reason (not throw) - all five reasons are caller
* Predictable business branches.
   */
  resumeWorkflowRun?(input: {
    workId: string;
    name?: string;
  }): Promise<DynamicWorkflowRunResumeResult>;
  /**
* Hub launches a saved workflow directly. GUI build one in the target project
* Send it `startSavedWorkflow` after an empty session: the agent parses the saved source + verifies the actual parameters + compiles, and if it is clean, use one
* controlOnly "start wheel" puts the user's real actions into the session and `port.submit` starts the run (without going through the model round, without bombing
* `CreateWorkflow` confirmation window - the user clicks in the hub to agree). Optional ability, absence conditions are the same as
* {@link resumeWorkflowRun} (if there is no dwf port, it will not be registered; the gateway return capability is not supported error). Failure with structured `reason`
* Return (not throw) - the six reasons are all business branches that the caller can expect, `message` carries human-readable diagnosis for in-line display in the actual parameter window;
* ①② Phase failure occurs **before any persistence** (no run, no message, no event, no task), and the GUI accordingly `deleteSession`
* Recover the empty session, and only the actually started run will appear in the transcription.
   */
  startSavedWorkflow?(input: {
    name: string;
    scope?: "project" | "global";
    args?: Record<string, unknown>;
  }): Promise<StartSavedWorkflowRunResult>;
  /**
* GUI "Configuration" changes the subagent model and concurrency upper bound of a run: use the same script to revise a new run without going through the model wheel or opening a confirmation window. Optional capabilities: ports
* Not registered when absent or the port does not have `amend` / `getScript` (the gateway return capability is not supported error). Failure with structured `reason`
* Return - each occurs before stopping or creating anything new.
   */
  amendWorkflowRunSettings?(
    input: Omit<AmendWorkflowRunSettingsInput, "traceContext">,
  ): Promise<AmendWorkflowRunSettingsResult>;
  /**
* Enumeration side of workflow run (discovery query after restart). Optional ability, absence conditions are the same as
* {@link listDynamicWorkflowRunEvents}; journal returns an empty list when there is no enumeration narrow query (honest answer——
* The run of the memory journal will not survive the process). `resumable` is evaluated by the same predicate of the resume gate.
   */
  listDynamicWorkflowRuns?(input: { limit?: number }): Promise<DynamicWorkflowRunSessionSummary[]>;
  /**
* Cold replay of workflow run: under the name of this session, `excludeRunIds`
* Runs other than this are played back from the journal into progress event payloads, and cold materialization feeds them to the same reducer as memory events -
* `workflowRuns` projection is therefore consistent across restarts. Optional capability, the absence conditions are the same as {@link listDynamicWorkflowRuns}.
   */
  replayDynamicWorkflowRuns?(input: {
    excludeRunIds: ReadonlySet<string>;
  }): Promise<DynamicWorkflowRunProgressPayload[]>;
  /**
* Read the **user interface product** of workflow run. Three abilities
* Registered together, absent together: they are three slices of the same journal reading surface, partial presence will only allow the UI to get one
* There are cards but the side panel cannot be opened. The absence conditions are the same as {@link listDynamicWorkflowRunEvents}, plus the three ports
* Optional members must all be present.
   *
* ⚠ Terminology: The artifact here is the output of the script published to **users** through `artifact.*`, not the internal processing of the engine.
* The same name as "script top-level return value".
   *
* Unknown runId returns `undefined` (the gateway is normalized to not found); the run of the part returns an empty array.
   */
  listDynamicWorkflowRunArtifacts?(input: {
    runId: string;
  }): Promise<readonly DynamicWorkflowRunArtifact[] | undefined>;
  /**
* Feed the `report` entries of a preset kanban board (cursor = journal sequence, strictly greater than).
* `limit` is clamped by the gateway and then passed down. Here it is **accurately** fulfilled - the caller passes "upper limit + 1" to detect hasMore.
* Absence conditions are the same as {@link listDynamicWorkflowRunArtifacts}.
   */
  listDynamicWorkflowRunArtifactItems?(input: {
    runId: string;
    artifactId: string;
    afterSequence?: number;
    limit: number;
  }): Promise<readonly DynamicWorkflowRunArtifactItem[]>;
  /**
* Read **all** bytes of a product version; returned to the gateway in chunks (≤ 512 KiB chunks). The authorization chain is on the port implementation side:
* The run must belong to this session ∧ There is a completed line of `(artifactId, version)` in the journal, and then it can be retrieved
* The uri on line ** goes to the store to read - any id passed by the caller never directly becomes the path.
* No such version / preset kanban (no bytes) / store will return `undefined` if it is absent.
* Absence conditions are the same as {@link listDynamicWorkflowRunArtifacts}.
   */
  readDynamicWorkflowRunArtifact?(input: {
    runId: string;
    artifactId: string;
    version: number;
  }): Promise<DynamicWorkflowRunArtifactBytes | undefined>;
  /**
* workspace of workflow run transcript: `files.*` /
* The journal lines of `git.*` / `world.run`, both registered and absent (list + bounded body of a node).
* Authorization is on the port implementation side (run must belong to this session); not your run / unknown run will return `undefined`.
* Absence conditions are the same as {@link listDynamicWorkflowRunArtifacts}.
   */
  listDynamicWorkflowRunWorkspaceNodes?(input: {
    runId: string;
  }): Promise<readonly DynamicWorkflowRunWorkspaceNode[] | undefined>;
  readDynamicWorkflowRunNodeResult?(input: {
    runId: string;
    siteId: string;
    ordinal: number;
    maxBytes: number;
  }): Promise<DynamicWorkflowRunWorkspaceNodeResult | undefined>;
  disconnectMcpServer(name: string): Promise<McpServerStatus | undefined>;
  listCheckpoints(options?: { limit?: number }): Promise<WorkspaceCheckpointSummary[]>;
  forkFromCheckpoint(options?: {
    targetCheckpointId?: string;
    targetMessageId?: string;
    traceContext?: TraceContext;
  }): Promise<WorkspaceForkResult>;
  generateWorkspaceText(
    input: WorkspaceGenerateTextInput,
    options?: { abortSignal?: AbortSignal; traceContext?: TraceContext },
  ): Promise<{
    text: string;
    selection: WorkspaceGenerateTextInput["selection"];
    finishReason: string;
    usage?: ModelUsage;
    toolCalls?: ModelToolCall[];
  }>;
  testModelConnectivity(
    input: { selection: ModelSelection },
    options?: { abortSignal?: AbortSignal; traceContext?: TraceContext },
  ): Promise<void>;
  expertWorkflowStatus(options?: {
    abortSignal?: AbortSignal;
    definitionId?: string;
    runId?: string;
    workflowKind?: string;
  }): Promise<ExpertWorkflowCommandResult>;
  workflowStatus?(options?: {
    abortSignal?: AbortSignal;
    definitionId?: string;
    runId?: string;
    workflowKind?: string;
  }): Promise<ExpertWorkflowCommandResult>;
  validateWorkflowScript?(input: { scriptPath: string }): Promise<ExpertWorkflowCommandResult>;
  runWorkflowScript?(
    input: { args?: unknown; resumeFromRunId?: string; scriptPath: string },
    options?: SubmitPromptOptions,
  ): Promise<ExpertWorkflowCommandResult>;
  resumeWorkflowScript?(
    input: { runId: string },
    options?: SubmitPromptOptions,
  ): Promise<ExpertWorkflowCommandResult>;
  scriptWorkflowStatus?(options?: { runId?: string }): Promise<ExpertWorkflowCommandResult>;
  listScriptWorkflows?(options?: { limit?: number }): Promise<ExpertWorkflowCommandResult>;
  retryWorkflow?(options?: {
    abortSignal?: AbortSignal;
    activityId?: string;
    definitionId?: string;
    nodeId?: string;
    onEvent?: SubmitPromptOptions["onEvent"];
    phase?: string;
    runId?: string;
    traceContext?: TraceContext;
    workflowKind?: string;
  }): Promise<ExpertWorkflowCommandResult>;
  listExpertWorkflows?(options?: {
    abortSignal?: AbortSignal;
    definitionId?: string;
    limit?: number;
    workflowKind?: string;
  }): Promise<WorkflowRunListItem[]>;
  listWorkflows?(options?: {
    abortSignal?: AbortSignal;
    definitionId?: string;
    limit?: number;
    workflowKind?: string;
  }): Promise<WorkflowRunListItem[]>;
  readExpertWorkflowEvents?(options: {
    abortSignal?: AbortSignal;
    limit?: number;
    runId: string;
  }): Promise<WorkflowEvent[]>;
  readWorkflowEvents?(options: {
    abortSignal?: AbortSignal;
    limit?: number;
    runId: string;
  }): Promise<WorkflowEvent[]>;
  resume(options?: ResumeOptions): Promise<ResumeSessionResult>;
  resumeExpertWorkflow(options?: {
    abortSignal?: AbortSignal;
    definitionId?: string;
    onEvent?: SubmitPromptOptions["onEvent"];
    runId?: string;
    traceContext?: TraceContext;
    workflowKind?: string;
  }): Promise<ExpertWorkflowCommandResult>;
  resumeWorkflow?(options?: {
    abortSignal?: AbortSignal;
    definitionId?: string;
    onEvent?: SubmitPromptOptions["onEvent"];
    runId?: string;
    traceContext?: TraceContext;
    workflowKind?: string;
  }): Promise<ExpertWorkflowCommandResult>;
  sendInput(input: PromptInput, options?: SendInputOptions): Promise<SendInputResult>;
  setMode(mode: CollaborationMode): Promise<{
    mode: CollaborationMode;
    previousMode: CollaborationMode;
    traceId: TraceContext["traceId"];
  }>;
  setModelIoFullRetentionEnabled?(enabled: boolean): void;
  setModel(
    modelId: string | ModelSelection,
    options?: {
      /**
* per-turn (off-peak idle plan): true = only switch running state - do not write disk model selection,
* Does not generate modelChange chat notification. Used for turn-level temporary switching (apply/restore paired).
       */
      transient?: boolean;
    },
  ): Promise<{
    model: string;
    previousModel: string;
    thoughtLevel?: string;
    traceId: TraceContext["traceId"];
  }>;
  setThoughtLevel(level: string): Promise<{
    previousThoughtLevel?: string;
    thoughtLevel: string;
    traceId: TraceContext["traceId"];
  }>;
  setLocale(locale: UiLocale): Promise<SetLocaleResult>;
  stopExpertWorkflow(options?: {
    abortSignal?: AbortSignal;
    definitionId?: string;
    runId?: string;
    workflowKind?: string;
  }): Promise<ExpertWorkflowCommandResult>;
  stopWorkflow?(options?: {
    abortSignal?: AbortSignal;
    definitionId?: string;
    runId?: string;
    workflowKind?: string;
  }): Promise<ExpertWorkflowCommandResult>;
  runExpertWorkflowBackground?(
    input: { definitionId?: string; task: string; workflowKind?: string },
    options?: SubmitPromptOptions,
  ): Promise<ExpertWorkflowCommandResult>;
  runWorkflowBackground?(
    input: { definitionId?: string; task: string; workflowKind?: string },
    options?: SubmitPromptOptions,
  ): Promise<ExpertWorkflowCommandResult>;
  /**
* v4 deferred queue: busy but no steerable active turn (compact/goal verifier/
* goal continuation boundary), the normal input must first fall into TurnSteerQueued, and cannot be
* runtime.steerTurn(no_active_turn/turn_not_steerable) disappeared from composer.
   */
  enqueueDeferredInput?(input: string, options?: SteerTurnOptions): Promise<TurnSteerResult>;
  steerTurn(input: string, options?: SteerTurnOptions): Promise<TurnSteerResult>;
/** v4 queue single item deletion: remove a queued input according to pendingInputId. Returns whether it is a hit. */
  removeQueueItem(
    pendingInputId: string,
    options?: {
      reason?: "user_removed" | "promoted";
      reservationId?: string;
      traceContext?: TraceContext;
    },
  ): Promise<boolean>;
/** sendQueuedNow atomic promotion: ordinary drain/delete after reserve shall not consume this item. */
  reserveQueueItem(
    pendingInputId: string,
    reservationId: string,
    options?: { traceContext?: TraceContext },
  ): Promise<boolean>;
  markQueueItemPromoting(
    pendingInputId: string,
    reservationId: string,
    options?: { traceContext?: TraceContext },
  ): Promise<boolean>;
  releaseQueueItemReservation(
    pendingInputId: string,
    reservationId: string,
    options?: { traceContext?: TraceContext },
  ): Promise<boolean>;
/** v4 queue single item editing: replace the queued input text by pendingInputId (save position). Returns whether it is a hit. */
  editQueueItem(
    pendingInputId: string,
    newText: string,
    options?: { traceContext?: TraceContext },
  ): Promise<boolean>;
/** v4 queue rearrangement: move pendingInputId to beforePendingInputId (null=queue tail). */
  reorderQueueItem(
    pendingInputId: string,
    beforePendingInputId: string | null,
    options?: { traceContext?: TraceContext },
  ): Promise<boolean>;
/** v4 heldQueueDisposition=clearQueueAndSend: Clear all queued inputs and return the number of discarded items. */
  clearQueueItems(options?: { traceContext?: TraceContext }): Promise<number>;
/** v4 setAutoDrain: Toggle queue autoDrain authorization bit (session level configuration). */
  setQueueAutoDrain(autoDrain: boolean, options?: { traceContext?: TraceContext }): Promise<void>;
/** The pause queue has been consumed by the CLI outer layer until it is empty: resume core's inline drain on subsequent running queues. */
  completeExternalQueueDrain(): void;
/** v4 setFollowupMode: Flip followup routing mode (queue/guide, session-level configuration). */
  setFollowupMode(
    mode: "queue" | "guide",
    options?: { traceContext?: TraceContext },
  ): Promise<void>;
  runExpertWorkflow(
    input: { definitionId?: string; task: string; workflowKind?: string },
    options?: SubmitPromptOptions,
  ): Promise<ExpertWorkflowCommandResult>;
  runWorkflow?(
    input: { definitionId?: string; task: string; workflowKind?: string },
    options?: SubmitPromptOptions,
  ): Promise<ExpertWorkflowCommandResult>;
  submitPrompt(prompt: PromptInput, options?: SubmitPromptOptions): Promise<TurnResult>;
}

export interface ResolveLatestSessionOptions {
  directory: string;
  env?: NodeJS.ProcessEnv;
  sessionStore?: SessionStorePort;
}

export interface RunZCodeProtocolAgentOptions {
/** The entrance has an exit time limit; bootstrap only arranges cancellation and resource cleanup, and does not exit the process directly. */
  lifecycle?: {
    readonly signal: AbortSignal;
    readonly deadlineAt: number | undefined;
    requestShutdown(error?: Error): void;
  };
/** Desktop internal command: Only run the original storage preparation and exit. */
  prepareStorageOnly?: boolean;
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  input?: NodeJS.ReadableStream;
  output?: NodeJS.WritableStream;
  presentationSurface?: PresentationSurface;
  version?: string;
}

export interface ListZCodeSessionsOptions {
  directory?: string;
  env?: NodeJS.ProcessEnv;
  limit?: number;
  sessionStore?: SessionStorePort;
}

export type { WorkflowAgentRunner };
