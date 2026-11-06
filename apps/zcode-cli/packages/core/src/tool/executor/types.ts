import type {
  AgentExecutionTelemetryPort,
  AgentTelemetryActorKind,
  BackgroundResultOriginMeta,
  CollaborationMode,
  CoordinatorResponsePort,
  DynamicWorkflowRunPort,
  DynamicWorkflowSnippetPort,
  ModelCatalogPort,
  EmbeddedSearchBackend,
  ExecutionPort,
  BrowserControlPort,
  ExecutionShellSelection,
  AutomationPort,
  OffPeakPort,
  FileSystemPort,
  HttpClientPort,
  ImageProcessorPort,
  PdfDocumentPort,
  Logger,
  Model,
  PermissionBrokerPort,
  SessionEvent,
  SessionId,
  SessionModePort,
  SessionStorePort,
  SkillPort,
  SubagentRunOptions,
  SubagentPort,
  ToolArtifactStorePort,
  TraceContext,
  TurnId,
  WorkflowPort,
  WorkflowEscalatePort,
  WorkflowSubmitPort,
} from "@zcode/contracts";
import type { HookRunner } from "../../hooks/index.js";
import type { PermissionService } from "../../permission/service.js";
import type { RuntimeTaskRegistry } from "../../runtime-task/registry.js";
import type { ToolRegistry } from "../registry.js";
import type { ToolSchedule } from "../scheduler.js";
import type {
  ExecutableToolCall,
  ReadFileStateMap,
  ToolBatchEvent,
  BackgroundTaskControlPort,
  ToolExecutionResult,
  ToolRuntimeScope,
} from "../types.js";

export interface BackgroundTaskNotificationCommand {
  originMeta?: BackgroundResultOriginMeta;
  taskId?: string;
  text: string;
  toolName?: string;
  traceContext: TraceContext;
}

export type EnqueueBackgroundTaskNotification = (
  notification: BackgroundTaskNotificationCommand,
) => undefined;

export interface BackgroundTaskNotificationPolicyInput {
  runtimeScope: ToolRuntimeScope;
  status: string;
  taskId: string;
  toolName: string;
  traceContext: TraceContext;
}

export type ShouldEnqueueBackgroundTaskNotification = (
  input: BackgroundTaskNotificationPolicyInput,
) => boolean;

export interface ToolExecutorOptions {
  agentTelemetry?: AgentExecutionTelemetryPort;
  agentTelemetryActorKind?: AgentTelemetryActorKind;
  registry: ToolRegistry;
  permissionService: PermissionService;
  permissionBroker?: PermissionBrokerPort;
  emitEvent: (event: SessionEvent) => Promise<void>;
  enqueueBackgroundTaskNotification?: EnqueueBackgroundTaskNotification;
  shouldEnqueueBackgroundTaskNotification?: ShouldEnqueueBackgroundTaskNotification;
  sessionId: SessionId;
  turnId?: TurnId;
  defaultTimeoutMs?: number;
  permissionTimeoutMs?: number;
  logger?: Logger;
  backgroundTaskControlPort?: BackgroundTaskControlPort;
  executionPort?: ExecutionPort;
  browserControlPort?: BrowserControlPort;
  browserDocumentationRoot?: string;
  fileSystemPort?: FileSystemPort;
  httpClientPort?: HttpClientPort;
  imageProcessorPort?: ImageProcessorPort;
  pdfDocumentPort?: PdfDocumentPort;
  model?: Model;
  embeddedSearchBackend?: EmbeddedSearchBackend;
  nativeSearchEnhancementsEnabled?: boolean;
  skillPort?: SkillPort;
  subagentPort?: SubagentPort;
  coordinatorResponsePort?: CoordinatorResponsePort;
  workflowSubmitPort?: WorkflowSubmitPort;
/** The upgrade port of the actor; if it exists, the session is registered to escalate. */
  workflowEscalatePort?: WorkflowEscalatePort;
  artifactStore?: ToolArtifactStorePort;
  automationPort?: AutomationPort;
  offPeakPort?: OffPeakPort;
  sessionStore?: SessionStorePort;
  sessionModePort?: SessionModePort;
  workflowPort?: WorkflowPort;
/** workflow run port; the background life cycle provider of CreateWorkflow when looking up the table by tool name. */
  dynamicWorkflowRunPort?: DynamicWorkflowRunPort;
  dynamicWorkflowSnippetPort?: DynamicWorkflowSnippetPort;
/** Model directory port; if it is absent, ListModels will report that the capability is absent, and the subagent_model of CreateWorkflow will be rejected. */
  modelCatalogPort?: ModelCatalogPort;
  runtimeTaskRegistry?: RuntimeTaskRegistry;
  readFileState?: ReadFileStateMap;
  subagentBackgroundBashMaxMs?: number;
  bashShellSelection?: ExecutionShellSelection;
  getBashShellSelection?: () => ExecutionShellSelection | undefined;
  workingDirectory?: string;
  workspaceRoot?: string;
  workspaceIdentity?: string;
  remoteSessionId?: string;
  clientMode?: "desktop-continuous" | "web-remote-replayable";
  deliveryKind?: "desktop-continuous" | "web-remote-replayable";
  runtimeScope?: ToolRuntimeScope;
  getWorkingDirectory?: () => string;
  setWorkingDirectory?: (cwd: string) => Promise<void> | void;
  getWorkspaceRoot?: () => string;
  getMemoryRoot?: () => string | undefined;
  traceContext?: TraceContext;
  mode?: CollaborationMode;
  getMode?: () => CollaborationMode;
  maxConcurrency?: number;
  hookRunner?: HookRunner;
}

export interface ToolExecutor {
  execute(toolCall: ExecutableToolCall, options?: ToolExecuteOptions): Promise<ToolExecutionResult>;
  executeBatch(
    toolCalls: ExecutableToolCall[],
    options?: ToolBatchExecuteOptions,
  ): Promise<ToolExecutionResult[]>;
  executeSchedule(
    toolCalls: ExecutableToolCall[],
    schedule: ToolSchedule,
    options?: ToolBatchExecuteOptions,
  ): AsyncGenerator<ToolBatchEvent, ToolExecutionResult[], void>;
  /**
* Include a background task that is not started by this round's tool call into tracking (the resume heavy arm of dwf run). `toolCall` is the descriptor synthesized by the caller (id = original
* toolCallId, name determines per-tool life cycle dispatch) - tracker only reads its id/name/input,
* Does not require a real on-the-fly tool call. The effect is exactly the same as the submit path: runtime-task registry registration
* (session recycling guardrail), BackgroundTaskStarted (backgroundWorks panel + cancelable),
* Polling/final state waiter, settlement notification. Repeated calls with the same taskId are deduplicated by the tracker's poller (idempotent).
   */
  trackExternalBackgroundTask(
    toolCall: ExecutableToolCall,
    output: Record<string, unknown>,
    traceContext: TraceContext,
    turnId?: TurnId,
  ): Promise<void>;
}

export interface ToolExecuteOptions {
  automationTurn?: boolean;
  offPeakTurn?: boolean;
  signal?: AbortSignal;
  traceContext?: TraceContext;
  subagentModelOverride?: SubagentRunOptions["modelOverride"];
  model?: Model;
}

export interface ToolBatchExecuteOptions extends ToolExecuteOptions {
  maxConcurrency?: number;
}

export interface ToolExecutorDeps {
  agentTelemetry?: AgentExecutionTelemetryPort;
  agentTelemetryActorKind?: AgentTelemetryActorKind;
  registry: ToolRegistry;
  permissionService: PermissionService;
  permissionBroker: PermissionBrokerPort;
  emitEvent: (event: SessionEvent) => Promise<void>;
  enqueueBackgroundTaskNotification?: EnqueueBackgroundTaskNotification;
  shouldEnqueueBackgroundTaskNotification?: ShouldEnqueueBackgroundTaskNotification;
  sessionId: SessionId;
  turnId?: TurnId;
  defaultTimeoutMs: number;
  permissionTimeoutMs?: number;
  logger?: Logger;
  backgroundTaskControlPort?: BackgroundTaskControlPort;
  executionPort?: ExecutionPort;
  browserControlPort?: BrowserControlPort;
  browserDocumentationRoot?: string;
  fileSystemPort?: FileSystemPort;
  httpClientPort?: HttpClientPort;
  imageProcessorPort?: ImageProcessorPort;
  pdfDocumentPort?: PdfDocumentPort;
  model?: Model;
  embeddedSearchBackend?: EmbeddedSearchBackend;
  nativeSearchEnhancementsEnabled?: boolean;
  skillPort?: SkillPort;
  subagentPort?: SubagentPort;
  coordinatorResponsePort?: CoordinatorResponsePort;
  workflowSubmitPort?: WorkflowSubmitPort;
/** The upgrade port of the actor; if it exists, the session is registered to escalate. */
  workflowEscalatePort?: WorkflowEscalatePort;
  artifactStore?: ToolArtifactStorePort;
  automationPort?: AutomationPort;
  offPeakPort?: OffPeakPort;
  sessionStore?: SessionStorePort;
  sessionModePort?: SessionModePort;
  workflowPort?: WorkflowPort;
/** workflow run port; the background life cycle provider of CreateWorkflow when looking up the table by tool name. */
  dynamicWorkflowRunPort?: DynamicWorkflowRunPort;
  dynamicWorkflowSnippetPort?: DynamicWorkflowSnippetPort;
/** Model directory port; if it is absent, ListModels will report that the capability is absent, and the subagent_model of CreateWorkflow will be rejected. */
  modelCatalogPort?: ModelCatalogPort;
  runtimeTaskRegistry?: RuntimeTaskRegistry;
  readFileState: ReadFileStateMap;
  subagentBackgroundBashMaxMs?: number;
  bashShellSelection?: ExecutionShellSelection;
  getBashShellSelection?: () => ExecutionShellSelection | undefined;
  getWorkingDirectory: () => string;
  setWorkingDirectory?: (cwd: string) => Promise<void> | void;
  getWorkspaceRoot: () => string;
  workspaceIdentity?: string;
  remoteSessionId?: string;
  clientMode?: "desktop-continuous" | "web-remote-replayable";
  deliveryKind?: "desktop-continuous" | "web-remote-replayable";
  getMemoryRoot?: () => string | undefined;
  runtimeScope: ToolRuntimeScope;
  traceContext?: TraceContext;
  getMode: () => CollaborationMode;
  maxConcurrency: number;
  hookRunner?: HookRunner;
}
