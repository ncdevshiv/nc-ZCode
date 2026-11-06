import type {
  TuiPromptAttachment,
  TuiPromptInput,
  TuiSendInput,
  TuiSendInputResult,
  TuiSubmitPrompt,
  TuiSubmitPromptResult,
} from "@zcode/tui";
import type { SupportedLocale, UiLocale } from "@zcode/i18n";
import type { ModelSelection, ZCodeModelOption } from "@zcode/shared";
import type {
  BackgroundTaskCancelResult,
  DynamicWorkflowRunResumeResult,
  DynamicWorkflowRunProgressPayload,
  DynamicWorkflowRunSessionSummary,
  PluginLoadOutcome,
  PluginMetadata,
  WorkflowRunSnapshot,
  WorkflowRunStatus,
} from "@zcode/contracts";
import type {
  CommandCenterCustomCommandContent,
  CommandCenterCustomCommandListOutcome,
} from "../command-center-custom.js";

export type TuiSubmitOptions = Parameters<TuiSubmitPrompt>[1];
export type TuiSendInputOptions = Parameters<TuiSendInput>[1];
export type CommandCenterMode = NonNullable<TuiSubmitPromptResult["mode"]>;
export type SwitchableCommandCenterMode = Extract<
  CommandCenterMode,
  "plan" | "build" | "edit" | "yolo"
>;

export type CommandCenterModelOption = ZCodeModelOption;

export type CommandCenterLocaleResult = {
  configPath?: string;
  locale: SupportedLocale;
  previousLocale?: SupportedLocale;
  requestedLocale: UiLocale;
};

export type CommandCenterMcpStatus = {
  error?: string;
  status: "connecting" | "connected" | "disabled" | "disconnected" | "failed" | "untrusted";
  toolCount: number;
  transport: "stdio" | "http" | "sse";
  updatedAt: string;
};

export type CommandCenterPluginListOutcome = PluginLoadOutcome;

export type CommandCenterPluginSetResult = {
  enabled: boolean;
  path: string;
  plugin: PluginMetadata;
};

export type CommandCenterPluginUninstallResult = {
// null means the plugin is not installed (idempotent no-op).
  removed: { id: string; name: string } | null;
};

export type CommandCenterSkill = {
  description: string;
  name: string;
  path: string;
  pluginName?: string;
  qualifiedName?: string;
  scope: string;
  source: string;
  whenToUse?: string;
};

export type CommandCenterSkillListOutcome = {
  skills: CommandCenterSkill[];
  totalDiscovered: number;
};

export type CommandCenterExpertWorkflowResult = {
  reportPath?: string;
  response: string;
  runId?: string;
  snapshot?: WorkflowRunSnapshot;
  status?: WorkflowRunStatus | string;
  traceId?: string;
};

export type CommandCenterForkResult = {
  copiedMessageCount?: number;
  forkedSessionId: string;
  response: string;
  restoredFileCount?: number;
  restoredFiles?: unknown[];
};

export type CommandCenterCheckpoint = {
  checkpointId: string;
  compactBoundaryId?: string;
  coveredByCompact?: boolean;
  createdAt: Date | number | string;
  fileCount?: number;
  messageId: string;
  preview?: string;
  scope: string;
};

export type CommandCenterSession = {
  directory: string;
  id: string;
  parentId?: string;
  title: string;
  updatedAt: Date | number | string;
};

export type CommandCenterTargetStatus = "active" | "paused" | "budget_limited" | "complete";

export type CommandCenterTarget = {
  objective: string;
  sessionID: string;
  status: CommandCenterTargetStatus;
  summaryTitle?: string | null;
  targetID: string;
  tokenBudget?: number | null;
  tokensUsed?: number;
  timeUsedSeconds?: number;
  time: {
    created: number;
    updated: number;
  };
};

export type CommandCenterLoginResult = {
  browser?: {
    opened: boolean;
    reason?: string;
  };
  configPath: string;
  credentialsPath: string;
  model: string;
  providerId?: "bigmodel" | "zai";
  user: {
    email?: string;
    name?: string;
    user_id: string;
  };
};

export type CommandCenterLoginAuthorizeData = {
  authorize_url: string;
  expires_at: number;
  flow_id: string;
  poll_interval_sec: number;
};

export type CommandCenterLoginOptions = {
  abortSignal?: AbortSignal;
  onAuthorizeUrl?: (data: CommandCenterLoginAuthorizeData) => Promise<void> | void;
};
export type CommandCenterBigmodelLoginOptions = CommandCenterLoginOptions;

export type CommandCenterBigmodelLoginResult = {
  browser?: {
    opened: boolean;
    reason?: string;
  };
  configPath: string;
  model: string;
  providerId: "bigmodel";
};

export type CommandCenterApiKeyOptions = {
  apiKey: string;
  providerId: "bigmodel" | "zai";
};

export type CommandCenterApiKeyResult = {
  configPath: string;
  model: string;
  providerId: "bigmodel" | "zai";
};

export type CommandCenterLogoutResult = {
  credentialsPath: string;
};

export type CommandCenterApp = {
  readonly sessionId: string;
  readonly traceId: string;
  getMode?(): CommandCenterMode;
  getModel?(): string | undefined;
  getCurrentModelOption?(): CommandCenterModelOption | undefined;
  getLocale?(): TuiSubmitPromptResult["locale"];
  getTheme?(): TuiSubmitPromptResult["theme"];
  getThoughtLevel?(): string | undefined;
  loadSessionTranscript?(): Promise<NonNullable<TuiSubmitPromptResult["restoredMessages"]>>;
  readSubagents?: import("@zcode/tui").TuiReadSubagents;
  readSubagentTranscript?: import("@zcode/tui").TuiReadSubagentTranscript;
  readTarget?(): Promise<CommandCenterTarget | null>;
  setTarget?(input: {
    objective: string;
    status?: CommandCenterTargetStatus;
    tokenBudget?: number | null;
  }): Promise<CommandCenterTarget>;
  updateTargetStatus?(status: CommandCenterTargetStatus): Promise<CommandCenterTarget | null>;
  clearTarget?(): Promise<boolean>;
  continueActiveTarget?(options?: {
    abortSignal?: AbortSignal;
    onEvent?: TuiSubmitOptions["onEvent"];
  }): Promise<TuiSubmitPromptResult | null>;
  listModels?(): CommandCenterModelOption[] | Promise<CommandCenterModelOption[]>;
  listThoughtLevels?(): string[] | Promise<string[]>;
  listPlugins?(): Promise<CommandCenterPluginListOutcome>;
  setPluginEnabled?(plugin: string, enabled: boolean): Promise<CommandCenterPluginSetResult>;
  uninstallPlugin?(plugin: string): Promise<CommandCenterPluginUninstallResult>;
  listMcpServers?(): Promise<Record<string, CommandCenterMcpStatus>>;
  listCheckpoints?(options?: { limit?: number }): Promise<CommandCenterCheckpoint[]>;
  connectMcpServer?(name: string): Promise<CommandCenterMcpStatus>;
  disconnectMcpServer?(name: string): Promise<CommandCenterMcpStatus | undefined>;
  expertWorkflowStatus?(options?: {
    abortSignal?: AbortSignal;
    runId?: string;
  }): Promise<CommandCenterExpertWorkflowResult>;
  /**
* Enumeration side of workflow run, service `/dwf list`. Optional capability: run the entire service when dwf journal is unavailable
* If not constructed, the member will be absent - the command will return "unavailable" instead of an empty list.
* The server has been filtered by parent session (listRunsByParentSession), and each row returned belongs to this session.
   */
  listDynamicWorkflowRuns?(input: { limit?: number }): Promise<DynamicWorkflowRunSessionSummary[]>;
  /**
* Cold replay of workflow run, cold start of service TUI image:
* journal → The same progress load as live. Optional capability, the absence conditions are the same as {@link listDynamicWorkflowRuns}.
   */
  replayDynamicWorkflowRuns?(input: {
    excludeRunIds: ReadonlySet<string>;
  }): Promise<DynamicWorkflowRunProgressPayload[]>;
  /**
* Cancellation side of workflow run, service `/dwf cancel`. runId ≡ taskId ≡ workId (same identification).
   */
  cancelBackgroundTask?(taskId: string): Promise<BackgroundTaskCancelResult>;
  /**
* Resume surface of workflow run, service `/dwf resume`. Use structured reason instead of throw when failure occurs.
* Commands render server-side rulings as is - `resumable` is never re-derived on the client.
   */
  resumeWorkflowRun?(input: {
    workId: string;
    name?: string;
  }): Promise<DynamicWorkflowRunResumeResult>;
  forkFromCheckpoint?(options?: { targetCheckpointId?: string }): Promise<CommandCenterForkResult>;
  recallPreviousInputHistory?(
    skip?: number,
  ): Promise<{ attachments?: TuiPromptAttachment[]; text: string } | null>;
  resume(options?: { onEvent?: TuiSubmitOptions["onEvent"] }): Promise<{
    appliedMessageCount: number;
    directory: string;
    interruptedToolCount: number;
    messageCount: number;
    partCount: number;
    traceId?: string;
  }>;
  resumeExpertWorkflow?(options?: {
    abortSignal?: AbortSignal;
    onEvent?: TuiSubmitOptions["onEvent"];
    runId?: string;
  }): Promise<CommandCenterExpertWorkflowResult>;
  submitPrompt(
    prompt: TuiPromptInput,
    options?: {
      abortSignal?: AbortSignal;
      onEvent?: TuiSubmitOptions["onEvent"];
    },
  ): Promise<TuiSubmitPromptResult>;
  runExpertWorkflow?(
    input: { task: string },
    options?: {
      abortSignal?: AbortSignal;
      onEvent?: TuiSubmitOptions["onEvent"];
    },
  ): Promise<CommandCenterExpertWorkflowResult>;
  sendInput?(input: TuiPromptInput, options?: TuiSendInputOptions): Promise<TuiSendInputResult>;
  setModel?(
    modelId: string | ModelSelection,
  ):
    | Promise<{ model: string; previousModel?: string; thoughtLevel?: string }>
    | { model: string; previousModel?: string; thoughtLevel?: string };
  setThoughtLevel?(
    level: string,
  ):
    | Promise<{ previousThoughtLevel?: string; thoughtLevel: string }>
    | { previousThoughtLevel?: string; thoughtLevel: string };
  setLocale?(locale: UiLocale): Promise<CommandCenterLocaleResult> | CommandCenterLocaleResult;
  stopExpertWorkflow?(options?: {
    abortSignal?: AbortSignal;
    runId?: string;
  }): Promise<CommandCenterExpertWorkflowResult>;
};

export type CommandCenterDeps = {
  forkApp?: (targetCheckpointId?: string) => Promise<CommandCenterForkResult>;
  getApp(): Promise<CommandCenterApp>;
  getLocale?: () => TuiSubmitPromptResult["locale"];
  getMode?: () => CommandCenterMode;
  hasSelectableModels?: () => Promise<boolean> | boolean;
  listSessions?: () => Promise<CommandCenterSession[]>;
  listCustomCommands?: () => Promise<CommandCenterCustomCommandListOutcome>;
  listSkills?: () => Promise<CommandCenterSkillListOutcome>;
  login?: (options?: CommandCenterLoginOptions) => Promise<CommandCenterLoginResult>;
  loginBigmodel?: (
    options?: CommandCenterBigmodelLoginOptions,
  ) => Promise<CommandCenterBigmodelLoginResult>;
  configureApiKey?: (options: CommandCenterApiKeyOptions) => Promise<CommandCenterApiKeyResult>;
  loadCustomCommand?: (name: string) => Promise<CommandCenterCustomCommandContent>;
  newApp?: () => Promise<CommandCenterApp>;
  recordInputHistory?: (
    input: TuiPromptInput,
    kind?: "slash_command",
  ) => Promise<unknown> | unknown;
  resumeApp(sessionId?: string): Promise<CommandCenterApp>;
/** After the user actively switches successfully, the complete default selection is saved; session recovery and automatic initialization are not called. */
  saveDefaultModelSelection?: (selection: ModelSelection) => Promise<void>;
  logout?: () => Promise<CommandCenterLogoutResult>;
  setLocale?: (locale: UiLocale) => Promise<CommandCenterLocaleResult> | CommandCenterLocaleResult;
  setMode?: (mode: SwitchableCommandCenterMode) => Promise<CommandCenterMode> | CommandCenterMode;
};
