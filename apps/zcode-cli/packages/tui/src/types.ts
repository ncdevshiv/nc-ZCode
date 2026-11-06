import type { WorkflowRunProgressEnvelope } from "@zcode/shared/zcode-protocol-v4";
import type { ModelSelection, ZCodeModelOption } from "@zcode/shared";
import type {
  CollaborationMode,
  InputDelivery,
  ModelUsageSummary,
  McpServerStatus,
  PermissionBrokerRequest,
  PermissionBrokerRequestOptions,
  PermissionBrokerResult,
  SessionEvent,
  SessionProjection,
  SupportedLocale,
  ToolResultDisplayPayload,
  UiThemeMode,
  UiThemePreference,
  TurnId,
  TurnSteerResult,
} from "@zcode/contracts";

export type TuiContextUsage = Pick<SessionProjection, "contextUsed" | "contextWindow">;

export type TuiSessionMetadata = Pick<
  TuiSubmitPromptResult,
  "locale" | "model" | "theme" | "thoughtLevel" | "modelOptions" | "effortOptions" | "loginRequired"
>;

export type TuiSwitchableMode = Extract<CollaborationMode, "plan" | "build" | "edit" | "yolo">;

export type TuiSetModeResult = {
  mode: CollaborationMode;
  response?: string;
};

export type TuiSetMode = (mode: TuiSwitchableMode) => Promise<TuiSetModeResult> | TuiSetModeResult;

export type TuiPromptAttachment = {
  type: "file" | "image" | "pdf" | "url";
  path?: string;
  content?: string;
};

export type TuiPromptInput =
  | string
  | {
      text: string;
      attachments?: TuiPromptAttachment[];
      /** Model picker commands retain the reference separately from their display text. */
      modelSelection?: ModelSelection;
    };

export type TuiImageMediaType = "image/png" | "image/jpeg" | "image/gif" | "image/webp";

export type TuiClipboardImage = {
  dataUrl: string;
  mediaType: TuiImageMediaType;
  sizeBytes?: number;
};

export type TuiReadClipboardImage = (options?: {
  abortSignal?: AbortSignal;
}) => Promise<TuiClipboardImage | null>;

export type TuiWriteClipboardText = (text: string) => Promise<void> | void;

export type TuiWorkspacePathKind = "directory" | "file";

export type TuiWorkspacePathSuggestion = {
  kind: TuiWorkspacePathKind;
  path: string;
};

export type TuiWorkspacePathSuggestionResult = {
  items: readonly TuiWorkspacePathSuggestion[];
  truncated: boolean;
};

export type TuiListWorkspacePathSuggestions = (request: {
  abortSignal?: AbortSignal;
  limit?: number;
  token: string;
}) => Promise<TuiWorkspacePathSuggestionResult>;

export type TuiSelectionItem = {
  command: string;
  disabledReason?: string;
  id: string;
  input?: TuiSelectionInput;
  keywords?: readonly string[];
  meta?: string;
  pending?: TuiSelectionPending;
  primary: string;
  secondary?: string;
};

export type TuiSelectionInput = {
  cancelStatus?: string;
  clearStatus?: string;
  emptyStatus?: string;
  help?: string;
  mask?: boolean;
  placeholder?: string;
  primary: string;
  secondary?: string;
  status?: string;
  submitStatus?: string;
};

export type TuiSelectionPending = {
  cancelStatus?: string;
  help?: string;
  primary: string;
  secondary?: string;
  status?: string;
};

export type TuiSelection = {
  emptyMessage: string;
  filterable?: boolean;
  help?: string;
  items: TuiSelectionItem[];
  placement?: "action" | "composer";
  prompt: string;
  selectedIndex?: number;
  title: string;
};

export type TuiRestoredTranscriptPart =
  | {
      text: string;
      type: "text";
    }
  | {
      text: string;
      type: "thought";
    }
  | {
      error?: string;
      input: Record<string, unknown>;
      output?: string;
      resultDisplay?: ToolResultDisplayPayload;
      status: "pending" | "running" | "completed" | "failed";
      title?: string;
      toolCallId: string;
      toolName: string;
      type: "tool";
    };

export type TuiSubmitPromptResult = {
  effortOptions?: readonly TuiEffortOption[];
  modelOptions?: readonly TuiModelOption[];
  locale?: SupportedLocale;
  loginRequired?: boolean;
  mode?: CollaborationMode;
  model?: string;
  theme?: UiThemePreference;
  projection?: Partial<TuiContextUsage>;
  response: string;
  resetSessionProjection?: boolean;
  restoredMessages?: Array<{
    id?: string;
    content: string;
    parts?: TuiRestoredTranscriptPart[];
    role: "agent" | "system" | "user";
  }>;
  selection?: TuiSelection;
  sessionId?: string;
  thoughtLevel?: string;
  traceId?: string;
  turnId?: string;
  usage?: ModelUsageSummary;
};

export type TuiRequestPermission = (
  request: PermissionBrokerRequest,
  options?: PermissionBrokerRequestOptions,
) => Promise<PermissionBrokerResult>;

export type TuiSubmitPrompt = (
  prompt: TuiPromptInput,
  options: {
    abortSignal: AbortSignal;
    onEvent?: (event: SessionEvent) => void | Promise<void>;
    requestPermission?: TuiRequestPermission;
  },
) => Promise<TuiSubmitPromptResult>;

export type TuiSendInputResult =
  | {
      kind: "started_turn";
      result: TuiSubmitPromptResult;
    }
  | {
      kind: "command_result";
      result: TuiSubmitPromptResult;
    }
  | TurnSteerResult;

export type TuiSendInput = (
  input: TuiPromptInput,
  options: {
    abortSignal?: AbortSignal;
    delivery?: InputDelivery;
    expectedTurnId?: TurnId;
    onEvent?: (event: SessionEvent) => void | Promise<void>;
    requestPermission?: TuiRequestPermission;
  },
) => Promise<TuiSendInputResult>;

export type TuiRecallPreviousInput = (
  skip?: number,
) => Promise<{ attachments?: TuiPromptAttachment[]; text: string } | null>;

export type TuiCancelBackgroundTask = (taskId: string) => Promise<unknown>;

export type TuiReadSubagents = (input?: {
  endedCursor?: string;
  endedLimit?: number;
}) => Promise<import("@zcode/shared").ZCodeSessionSubagentsResult>;
export type TuiSubagentTranscriptSnapshot = {
  sessionId: string;
  sequenceNumber: number;
  messages: NonNullable<TuiSubmitPromptResult["restoredMessages"]>;
  events: SessionEvent[];
  replayMessageIds: string[];
};
export type TuiReadSubagentTranscript = (
  childSessionId: string,
) => Promise<TuiSubagentTranscriptSnapshot>;

/**
* Session event subscription that persists across rounds.
 *
* Why not reuse the per-turn `onEvent` of `submitPrompt` / `sendInput`: dwf progress is **out of turn event**
* (turnId is empty), the per-turn sink dies at the end of the turn - this is the structural reason why TUI lost the dwf event today.
* Background completion notification-driven model return logic (it is self-driven by the runtime command queue, without any per-turn sink listening).
 *
* Return unsubscribe; hung by CLI when `replaceApp` (`/new` `/resume` `/fork`).
 */
export type TuiSubscribeSessionEvents = (sink: (event: SessionEvent) => void) => () => void;

/**
* Session-level workflow run summary (`app.listDynamicWorkflowRuns`), used for reseeding images during cold start/recovery.
 *
* `label` / `updatedAt` are additive optional: the old server does not send these two keys, and the read side returns the runId and does not display the time.
* `resumable` is determined by the server and **never** rederived in the TUI.
 */
export type TuiWorkflowRunSummary = {
  runId: string;
  toolCallId?: string;
  status: "completed" | "errored" | "pending" | "running" | "stopped";
  stopReason?: "user" | "model" | "provider" | "interrupted" | "superseded";
  label?: string;
  updatedAt?: number;
  resumable?: boolean;
};

export type TuiListWorkflowRuns = () => Promise<readonly TuiWorkflowRunSummary[]>;

/**
* Cold replay of workflow run: put this session under the name and in the mirror
* The unfinished runs are played back from the journal into progress event envelopes and fed to the shared reducer of the mirror one by one - restart / `/resume`
* Cards now show real steps, usage and subagents instead of empty shells of 0/0.
 */
export type TuiReplayWorkflowRuns = (input: {
  excludeRunIds: ReadonlySet<string>;
}) => Promise<readonly WorkflowRunProgressEnvelope[]>;

/**
* The current **main session** id. It is a getter instead of a value: `/new` `/resume` `/fork` will change the session,
* The snapshot id will expire immediately, and then the entire transcript will be misjudged as an external event.
 */
export type TuiGetMainSessionId = () => string | undefined;

export type TuiListMcpServers = () => Promise<Record<string, McpServerStatus>>;

export type TuiSlashCommandSuggestion = {
  aliases?: readonly string[];
  name: string;
  summary: string;
  usage: string;
};

export type TuiModelOption = ZCodeModelOption;

export type TuiEffortOption = {
  description?: string;
  id: string;
  label: string;
};

export type TuiModeOption = {
  description: string;
  id: TuiSwitchableMode;
  label: string;
};

export type TuiOptions = {
  /** Called after the startup screen has painted; runtime ownership stays in CLI. */
  loadStartupOptions?: () => Promise<TuiStartupOptions>;
  initialMode?: CollaborationMode;
  initialModel?: string;
  initialResult?: TuiSubmitPromptResult;
  initialThoughtLevel?: string;
  loginRequired?: boolean;
  locale?: SupportedLocale;
  theme?: UiThemePreference;
  initialThemeMode?: UiThemeMode;
  developerMode?: boolean;
  version?: string;
  workspaceDirectory?: string;
  workspaceGitBranch?: string;
  noColor: boolean;
  stderr: NodeJS.WriteStream;
  stdin: NodeJS.ReadStream;
  stdout: NodeJS.WriteStream;
  cancelBackgroundTask?: TuiCancelBackgroundTask;
  readSubagents?: TuiReadSubagents;
  readSubagentTranscript?: TuiReadSubagentTranscript;
  listWorkspacePathSuggestions?: TuiListWorkspacePathSuggestions;
  listMcpServers?: TuiListMcpServers;
  listWorkflowRuns?: TuiListWorkflowRuns;
  replayWorkflowRuns?: TuiReplayWorkflowRuns;
  getMainSessionId?: TuiGetMainSessionId;
  readClipboardImage?: TuiReadClipboardImage;
  recallPreviousInput?: TuiRecallPreviousInput;
  sendInput?: TuiSendInput;
  setMode?: TuiSetMode;
  effortOptions?: readonly TuiEffortOption[];
  modelOptions?: readonly TuiModelOption[];
  listModelOptions?: () => Promise<readonly TuiModelOption[]>;
  slashCommands?: readonly TuiSlashCommandSuggestion[];
  submitPrompt: TuiSubmitPrompt;
  subscribeSessionEvents?: TuiSubscribeSessionEvents;
  subscribeThemeMode?: (listener: (mode: UiThemeMode) => void) => () => void;
  setTerminalBackgroundColor?: (color: string) => void;
  writeClipboardText?: TuiWriteClipboardText;
};

export type TuiStartupOptions = Pick<
  TuiOptions,
  | "initialMode"
  | "initialModel"
  | "initialThoughtLevel"
  | "loginRequired"
  | "locale"
  | "theme"
  | "modelOptions"
  | "effortOptions"
  | "slashCommands"
  | "workspaceGitBranch"
>;
