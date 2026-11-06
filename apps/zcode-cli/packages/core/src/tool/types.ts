// ============================================================
// Tool Types - Core tool types for registry and executor
// ============================================================

import type {
  ExecutionShellSelection,
  AutomationPort,
  OffPeakPort,
  EmbeddedSearchBackend,
  ExecutionPort,
  BrowserControlPort,
  FileSystemPort,
  HttpClientPort,
  ImageProcessorPort,
  PdfDocumentPort,
  ModelMessageContent,
  ModelContentProtection,
  Model,
  CoordinatorResponsePort,
  DynamicWorkflowRunPort,
  DynamicWorkflowSnippetPort,
  ModelCatalogPort,
  RiskLevel,
  SessionId,
  SessionEvent,
  SessionModePort,
  SessionStorePort,
  SkillPort,
  SkillTelemetryMetadata,
  SubagentRunOptions,
  SubagentPort,
  ToolArtifactStorePort,
  TraceContext,
  TraceId,
  TurnId,
  WorkflowPort,
  WorkflowEscalatePort,
  WorkflowSubmitPort,
} from "@zcode/contracts";
import type {
  JsonSchema,
  ModelToolSideEffectScope,
  PermissionBrokerReasonSource,
  PermissionCapabilityGroup,
  PermissionRuleBehavior,
  PermissionRuleValue,
  PermissionUpdate,
  ProviderNativeToolSpec,
  ToolExecutionMode,
  ToolCancellationPolicy,
  ToolContractDeclaration,
  ToolResultBudgetStrategy,
  ToolResultDisplayPayload,
  ToolTimeoutPolicy,
  ToolExecutionSpanWriter,
  ToolExecutionTelemetry,
} from "@zcode/contracts";
import type { PersistedReadFileStateMetadata } from "./read-file-state-metadata.js";
import type { RuntimeTaskRegistry } from "../runtime-task/registry.js";

// -----------------------------------------------
// Tool Metadata
// -----------------------------------------------

export interface ToolMetadata {
  name: string;
  description?: string;
  modelInstructions?: readonly string[];
  allowedInPlanMode?: boolean;
  readOnly: boolean;
  destructive: boolean;
  concurrentSafe: boolean;
  requiresUserInteraction?: boolean;
  timeoutMs?: number;
  maxOutputBytes?: number;
  sideEffectScope: ModelToolSideEffectScope;
  riskLevel: RiskLevel;
  needsApproval: boolean;
  providerVisible?: boolean;
  /**
* Declare that the tool is a final tool that "terminates on success": once a successful result is returned, the executor hangs on the result
* turnControl terminates the current turn. This is a declaration of the tool's intrinsic capabilities (like concurrentSafe/destructive),
* Read by the executor instead of guessing the tool name when calling it. submit_result Use it to implement the actor's final state submission.
   */
  stopTurnOnSuccess?: boolean;
/** Trusted display source of MCP discovery; only participates in UI projection and does not participate in permission determination. */
  mcpPresentation?: {
    serverName: string;
    toolName: string;
    description?: string;
/** From an MCP server that declares zcode_official authentication; only used to trust structured identifiers in its results. */
    official?: boolean;
  };
}

// -----------------------------------------------
// Tool Execution Context
// -----------------------------------------------

export type ToolRuntimeScope = "main" | "subagent";

export interface BackgroundTaskControlStopOptions {
/** Who is stopping: Fill in "model" for TaskStop, and the final status notification will be worded accordingly. */
  initiator?: "user" | "model";
  strict: true;
  traceContext?: TraceContext;
}

export interface BackgroundTaskControlStopResult {
  command?: string;
  ok: boolean;
  reason?:
    | "background_task_cancel_not_supported"
    | "background_task_not_found"
    | "background_task_not_running";
  status?: string;
  taskId: string;
  type?: string;
}

export interface BackgroundTaskControlPort {
  stopBackgroundTask(
    taskId: string,
    options: BackgroundTaskControlStopOptions,
  ): Promise<BackgroundTaskControlStopResult>;
}

export interface ToolExecutionContext {
  toolCallId: string;
  /**
* Real-time observation writer for the current Tool. The Handler can only write facts through the narrow interface and cannot touch the original OTel Span.
   */
  telemetry?: ToolExecutionSpanWriter;
/** Whether the current tool call belongs to the automation dispatch round; write the tool handler to use it for final permission verification. */
  automationTurn?: boolean;
/** Whether the current tool call belongs to the idle task dispatch round; the OffPeakCreate handler uses it for final rejection. */
  offPeakTurn?: boolean;
  traceContext?: TraceContext;
  traceId: TraceId;
  spanId?: string;
  parentSpanId?: string;
  abortSignal: AbortSignal;
  backgroundTaskControlPort?: BackgroundTaskControlPort;
  emitEvent?: (event: SessionEvent) => Promise<void>;
  executionPort?: ExecutionPort;
/** browser-use control port; node_repl's agent.browsers.* is executed here. By default, browser is disabled. */
  browserControlPort?: BrowserControlPort;
/** Official browser-use plugin docs asset directory; only used in agent.browsers.documentation() when browser-use is enabled. */
  browserDocumentationRoot?: string;
  fileSystemPort?: FileSystemPort;
  httpClientPort?: HttpClientPort;
  imageProcessorPort?: ImageProcessorPort;
  pdfDocumentPort?: PdfDocumentPort;
  model?: Model;
/** Core Server’s Selection override of the foreground child. */
  subagentModelOverride?: SubagentRunOptions["modelOverride"];
  skillPort?: SkillPort;
  subagentPort?: SubagentPort;
  coordinatorResponsePort?: CoordinatorResponsePort;
/** The port where the workflow actor submits the final results and waits for the engine's decision; only injected in the actor workflow session. */
  workflowSubmitPort?: WorkflowSubmitPort;
/** Port where workflow actor escalation blocks questions and waits for a reply from the master agent; only injected in workflow actor sessions. */
  workflowEscalatePort?: WorkflowEscalatePort;
  artifactStore?: ToolArtifactStorePort;
  automationPort?: AutomationPort;
  offPeakPort?: OffPeakPort;
  sessionStore?: SessionStorePort;
  sessionModePort?: SessionModePort;
  workflowPort?: WorkflowPort;
/** workflow run submission port; if absent, CreateWorkflow will return to placeholder diagnosis without starting. */
  dynamicWorkflowRunPort?: DynamicWorkflowRunPort;
/** dwf snippet synchronization execution port; if it is absent, EvalWorkflowSnippet will report business failure if the capability is absent. */
  dynamicWorkflowSnippetPort?: DynamicWorkflowSnippetPort;
/** Model directory port; if it is absent, ListModels will report that the capability is absent, and the subagent_model of CreateWorkflow will be rejected. */
  modelCatalogPort?: ModelCatalogPort;
  runtimeTaskRegistry?: RuntimeTaskRegistry;
  readFileState?: ReadFileStateMap;
  recordReadFileStateMetadata?: (metadata: PersistedReadFileStateMetadata) => void;
/** Record Skill resolved metadata; only used for telemetry, does not change the visible results of the model. */
  recordSkillTelemetryMetadata?: (metadata: SkillTelemetryMetadata) => void;
  bashShellSelection?: ExecutionShellSelection;
  embeddedSearch?: ToolEmbeddedSearchContext;
  setWorkingDirectory?: (cwd: string) => Promise<void> | void;
  workingDirectory: string;
  workspaceRoot: string;
  workspaceIdentity?: string;
  remoteSessionId?: string;
  clientMode?: "desktop-continuous" | "web-remote-replayable";
  deliveryKind?: "desktop-continuous" | "web-remote-replayable";
  memoryRoot?: string;
  runtimeScope?: ToolRuntimeScope;
  providerVisibleToolNames?: readonly string[];
  sessionId: SessionId;
  turnId?: TurnId;
}

export interface ToolEmbeddedSearchContext {
  backend?: EmbeddedSearchBackend;
  enabled: boolean;
  findAndGrepEnabled?: boolean;
}

export interface ReadFileStateEntry {
  path: string;
  content: string;
  offset?: number;
  limit?: number;
  isPartialView: boolean;
  readAt: Date;
  sourceTool?: "Read" | "Write" | "Edit";
  revisionId?: string;
  mtimeMs?: number;
  sizeBytes?: number;
}

export type ReadFileStateMap = Map<string, ReadFileStateEntry>;

// -----------------------------------------------
// Tool Handler
// -----------------------------------------------

// The tool handler uses this return value to express expected business failure; the successful output does not use this reserved shape.
export interface ToolHandlerFailure {
  result: false;
  errorCode: number;
  message: string;
}

export interface ToolInputValidationContext {
  runtimeTaskRegistry?: RuntimeTaskRegistry;
}

export type ToolInputValidationResult = { result: true } | ToolHandlerFailure;

/**
* The context of {@link ToolEntry.resolveInput}. Narrow enough to only parse what is really needed - the working directory is "in the project
* "Where does the thing live?" The only entrance. If you add more, it will turn a normalized hook into a second execution entrance.
 */
export interface ToolInputResolutionContext {
  workingDirectory?: string;
  runtimeTaskRegistry?: RuntimeTaskRegistry;
  /**
* workflow run port and session id: AmendWorkflow uses them to parse `run_id` into "whether the predecessor belongs to this session,
* Is it still running?" fact block. Still read only -
* The normalization hook does not become the second execution entry.
   */
  dynamicWorkflowRunPort?: DynamicWorkflowRunPort;
  /**
* Model directory port: CreateWorkflow / AmendWorkflow Use it to parse `subagent_model` into a canonical form, but it cannot be solved
* It will be returned as a business failure before the confirmation window**.
* Synchronous, read-only, and here for the same reason as `dynamicWorkflowRunPort`. If the field is given in absence, it will be rejected——
* Do not silently pass a string that the host cannot understand.
   */
  modelCatalogPort?: ModelCatalogPort;
  sessionId?: string;
}

export type ToolInputResolutionResult = { result: true; input: unknown } | ToolHandlerFailure;

export type ToolHandler<TInput = unknown, TOutput = unknown> = (
  input: TInput,
  context: ToolExecutionContext,
) => Promise<TOutput>;

// -----------------------------------------------
// Tool Entry
// -----------------------------------------------

export interface ToolEntry extends ToolContractDeclaration {
  aliases?: readonly string[];
  /**
   * Host-issued atomicity policy for model content. Only an authority-verified
   * registration path may set this; executor code must never infer it from a
   * tool name or from model-visible content.
   */
  modelContentProtection?: ModelContentProtection["kind"];
  /**
   * Optional provider-visible character threshold for tools whose upstream contract
   * budgets UTF-16 characters rather than UTF-8 bytes.
   */
  maxModelChars?: number;
  /**
   * MIME type for provider-visible model content persisted by the result budget.
   * Defaults to the handler result shape when omitted.
   */
  resultArtifactContentType?: string;
  metadata: ToolMetadata;
  /**
* Only written by trusted sources verified by the host; cannot be derived from MCP names or descriptors visible to the model.
   */
  permissionCapabilityGroup?: PermissionCapabilityGroup;
  executionMode?: ToolExecutionMode;
  providerNative?: ProviderNativeToolSpec;
  handler: ToolHandler;
/** The homologous projection of the current turn model capability to the provider descriptor and executor schema. */
  resolveModelContract?: (context: ToolExecutionModelContext) => {
    description?: string;
    inputSchema?: JsonSchema;
  };
  validateInput?: (
    input: unknown,
    context: ToolInputValidationContext,
  ) => ToolInputValidationResult;
  /**
* Normalize the input parameters ** sent by the model into the execution facts that will occur **. executor after `validateInput`,
* Called before PreToolUse hook, the return value directly replaces `executionInput`.
   *
* Location is what it's all about. Afterwards hook, project permission rules, permission event payload, `prepareApproval`, handler
* Everything read is the same normalized input, so three things are done at once:
* 1. The strategy is not bypassed - a PreToolUse hook that scans the script can also see the real script on the saved run;
* 2. Visible across versions - the input channel is schema-free transparent transmission for each client version, but the display channel is not;
* 3. Confirmation and execution are the same bytes - only parsed once, that byte is brought all the way to the handler, there is no approval for A to run B.
   *
* Therefore the return value must still satisfy `inputSchema` / `runtimeInputSchema` (after the hook is rewritten, the executor will
* Check again). If parsing fails, {@link ToolHandlerFailure} will be returned, and the executor will close before the hook - that is
* Business failure is not an infrastructure failure, and the user should not be interrupted to confirm first.
   */
  resolveInput?: (
    input: unknown,
    context: ToolInputResolutionContext,
  ) => Promise<ToolInputResolutionResult> | ToolInputResolutionResult;
  formatModelContent?: (output: unknown) => ModelMessageContent;
  formatPersistedModelContent?: (
    input: ToolPersistedModelContentInput,
  ) => ModelMessageContent | undefined;
  resolveTimeoutBudgetMs?: (
    input: unknown,
    context?: ToolExecutionModelContext,
  ) => number | undefined;
  resolvePermissionCapability?: (
    input: unknown,
    context?: ToolRuntimePermissionCapabilityContext,
  ) => ToolRuntimePermissionCapability | undefined;
  resolvePermissionRulePolicy?: (
    input: unknown,
    context?: ToolRuntimePermissionCapabilityContext,
  ) => ToolPermissionRulePolicy | undefined;
  /**
   * Last word on an `ask` decision, owned by the tool. Runs after the permission service
   * has already decided to ask, so it can only narrow the ask to a pass (`proceed`) or
   * enrich it with a preview — it can never turn an allow into an ask.
   *
   * Synchronous like the other permission hooks: it inspects the input the executor
   * already holds and must not perform I/O on the approval path. A tool that needs to
   * read the world before it can build a preview belongs in {@link resolveInput}, which
   * runs earlier, is async, and whose result the whole downstream chain shares.
   */
  prepareApproval?: (input: unknown) => ToolApprovalGate;
  inputSchema: JsonSchema;
  runtimeInputSchema?: unknown;
  runtimeOutputSchema?: unknown;
  timeout: ToolTimeoutPolicy;
  cancellation: ToolCancellationPolicy;
}

export type ToolApprovalGate =
  | { gate: "proceed" }
  | { gate: "ask"; display?: ToolResultDisplayPayload };

export interface ToolPermissionRulePolicy {
  evaluateRules: (
    behavior: PermissionRuleBehavior,
    rules: readonly PermissionRuleValue[],
  ) => boolean;
  suggestedPermissionUpdates: PermissionUpdate[];
}

export interface ToolPersistedModelContentInput {
  output: unknown;
  content: string;
  persistedPath: string;
  originalBytes: number;
}

export interface ToolRuntimePermissionCapability {
  allowedInPlanMode?: boolean;
  destructive?: boolean;
  needsApproval?: boolean;
  readOnly?: boolean;
  requiresUserInteraction?: boolean;
  riskLevel?: RiskLevel;
  sideEffectScope?: ModelToolSideEffectScope;
  permission?: Partial<ToolContractDeclaration["permission"]>;
}

export interface ToolRuntimePermissionCapabilityContext {
  runtimeScope?: ToolRuntimeScope;
  workingDirectory?: string;
  workspaceRoot?: string;
}

export interface ToolExecutionModelContext {
  model?: Model;
}

// -----------------------------------------------
// Execution Results
// -----------------------------------------------

export interface ToolExecutionResult {
  toolCallId: string;
  toolName: string;
  success: boolean;
  output: unknown;
  turnControl?: ToolExecutionTurnControl;
  followUpUserInput?: ToolExecutionFollowUpUserInput;
  display?: ToolResultDisplayPayload;
  modelContent?: ModelMessageContent;
  readFileStateMetadata?: PersistedReadFileStateMetadata;
  serialization?: ToolResultSerialization;
/** Executor’s summarized internal performance facts; visible Tool Output without entering the model. */
  performance?: ToolExecutionTelemetry;
  error?: {
    code?: string;
    detail?: string;
    type: string;
    message: string;
    reasonSource?: PermissionBrokerReasonSource;
    stack?: string;
  };
  durationMs: number;
  startedAt: Date;
  completedAt: Date;
}

export interface ToolExecutionFollowUpUserInput {
  input: string;
  reasonSource: PermissionBrokerReasonSource;
}

export interface ToolExecutionTurnControl {
  reason: "automation_create_limit" | "plan_exit_denied" | "subagent_terminal";
  stopTurnAfterResult: boolean;
}

export interface ToolResultSerialization {
  content: string;
  modelContent?: ModelMessageContent;
  originalBytes: number;
  /**
* The actual bytes requested into the model. For protected CUA structured frames, serialize text within the image block
* Only rendered as a short placeholder, but the real base64 raster is sent as-is - so here = serialized text
* Bytes + real media payload, cost/usage observations (setOutputBytes, turn-tool-usage,
* usage-observability) images may not be undercounted. The aggregate is the only public metering state.
   */
  returnedBytes: number;
  truncated: boolean;
  budgetStrategy: ToolResultBudgetStrategy;
  artifactPath?: string;
}

export interface ToolBatchResult {
  toolCallId: string;
  results: ToolExecutionResult[];
  allSucceeded: boolean;
}

// -----------------------------------------------
// Executable Tool Call
// -----------------------------------------------

export interface ExecutableToolCall {
  id: string;
  name: string;
  input: unknown;
}

// -----------------------------------------------
// Batch Events
// -----------------------------------------------

export type ToolBatchEvent =
  | { type: "batch_start"; parallelGroupIndex: number; toolCallIds: string[] }
  | {
      type: "batch_complete";
      parallelGroupIndex: number;
      results: ToolExecutionResult[];
    }
  | { type: "tool_start"; toolCallId: string }
  | { type: "tool_complete"; result: ToolExecutionResult }
  | { type: "error"; error: Error; toolCallId?: string };
