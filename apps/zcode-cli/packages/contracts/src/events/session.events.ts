import type { ExecutionOutputPreview } from "../interfaces/execution.port.js";
import type { RuntimeInputPresentation } from "../interfaces/runtime-input-presentation.js";
/* eslint-disable max-lines -- The session event contract is concentrated in a single file export to avoid drift after app/agent protocol types are dispersed. */
// ============================================================
// Session Events - All event types for the agent loop
// ============================================================

import type {
  EventId,
  InteractionRequestOrigin,
  MessageId,
  PartId,
  QueryId,
  SessionId,
  TraceId,
  ToolCallId,
  TurnId,
} from "../interfaces/shared.js";
import type {
  CollaborationMode,
  RiskLevel,
  TurnSteerCommandKind,
  TurnSteerDeliveryMode,
  TurnSteerRejectReason,
  TurnSteerSource,
  TurnInputIntentMetadata,
} from "../interfaces/session.port.js";
import type {
  ModelNetworkStatusEvent,
  ModelSelection,
  ModelUsage,
  ModelUsageSummary,
} from "../model/index.js";
import type { HttpClientEgressInfo } from "../interfaces/http-client.port.js";
import { createModelUsageSummary } from "../model/index.js";
import type { ModelApiErrorPhase, ModelFailureExceptionKind } from "../telemetry/index.js";
import type {
  CompactBoundaryPayload,
  CompactTimelinePayload,
  MicrocompactBoundaryPayload,
} from "../compact/index.js";
import type { HookRunLifecyclePayload } from "../hooks/index.js";
import type { CheckpointCreatedPayload, RewindTriggeredPayload } from "../rewind/index.js";
import type { GoalCompletionVerificationOutput, SessionGoal } from "../tools/target.js";
import type { ToolSideEffectScope } from "../tools/contract.js";
import type { ToolResultDisplayPayload } from "../tools/tool-result-metadata.js";
import type { SkillTelemetryMetadata } from "../skills/index.js";
import type {
  MessageVisibility,
  SessionTitleSource,
  SyntheticUserMessageSource,
} from "../interfaces/session-store.port.js";
import type { SavedWorkflowScope } from "../tools/saved-workflow.js";
import type { PermissionOptionsPolicy, PermissionUpdate } from "../interfaces/permission.port.js";
import type {
  StreamRecoveryAnchorPayload,
  StreamRecoveryAnchorSelectedPayload,
  StreamRecoveryBlockedPayload,
  StreamRecoveryRetryStartedPayload,
  StreamRecoveryStartedPayload,
  StreamRecoveryTailDiscardedPayload,
  StreamingToolLedgerPayload,
} from "./stream-recovery.events.js";

// Re-export for convenience
export type { CollaborationMode, RiskLevel } from "../interfaces/session.port.js";

// Re-export ModelToolCall as ToolCall for core usage
export type { ModelToolCall as ToolCall } from "../model/index.js";

// Base Event
export interface SessionEvent {
  id: EventId;
  sessionId: SessionId;
  turnId?: TurnId;
  type: SessionEventType;
  timestamp: Date;
  traceId: TraceId;
  sequenceNumber: number;
  payload: unknown;
}

export const SessionEventType = {
  SessionCreated: "session_created",
  SessionResumed: "session_resumed",
  SessionForked: "session_forked",
  SessionCompacted: "session_compacted",
  SessionTitleUpdated: "session_title_updated",
  SessionModeChanged: "session_mode_changed",
  SessionEnded: "session_ended",
  TurnStarted: "turn_started",
  TurnInputReceived: "turn_input_received",
  TurnSteerQueued: "turn_steer_queued",
// When the guide closes without encountering an available tool batch, the original intent is redirected to the normal queue.
  TurnSteerDeliveryChanged: "turn_steer_delivery_changed",
// sendQueuedNow Atomic promotion: reservation/promoting/rollback all enter the event stream, and projection is not guessed locally.
  TurnSteerDispatchChanged: "turn_steer_dispatch_changed",
  TurnSteerDrained: "turn_steer_drained",
  TurnSteerRejected: "turn_steer_rejected",
  TurnSteerDiscarded: "turn_steer_discarded",
// session_input and user message/parts have been promoted in the same transaction; CommandInbox can safely unpin.
  SessionInputPromoted: "session_input_promoted",
// v4 queue rearrangement: queue item order changes (reducer rearranges queue rows according to orderedPendingInputIds).
  TurnSteerReordered: "turn_steer_reordered",
// v4 input control: queue autoDrain switch (held grant bit derived from heldQueueInputRequiresChoice).
  QueueAutoDrainChanged: "queue_auto_drain_changed",
// v4 input control: followup routing mode (enqueue vs guide when running).
  FollowupModeChanged: "followup_mode_changed",
  TurnComplete: "turn_complete",
  TurnError: "turn_error",
  UserMessage: "user_message",
  AssistantMessage: "assistant_message",
  AssistantFeedbackUpdated: "assistant_feedback_updated",
  SystemMessage: "system_message",
  ModelRequest: "model_request",
  ModelSelected: "model_selected",
  ModelStreaming: "model_streaming",
  StreamingToolLedgerUpdated: "streaming_tool_ledger_updated",
  StreamRecoveryAnchorCreated: "stream_recovery_anchor_created",
  StreamRecoveryStarted: "stream_recovery_started",
  StreamRecoveryAnchorSelected: "stream_recovery_anchor_selected",
  StreamRecoveryTailDiscarded: "stream_recovery_tail_discarded",
  StreamRecoveryRetryStarted: "stream_recovery_retry_started",
  StreamRecoveryBlocked: "stream_recovery_blocked",
  ModelNetworkStatus: "model_network_status",
  ModelAnomalyWarning: "model_anomaly_warning",
  NetworkRequestStatus: "network_request_status",
  ModelComplete: "model_complete",
  ModelError: "model_error",
  ToolCallScheduled: "tool_call_scheduled",
  ToolCallStarted: "tool_call_started",
  ToolCallProgress: "tool_call_progress",
  ToolCallResult: "tool_call_result",
  ToolCallError: "tool_call_error",
  ToolBatchComplete: "tool_batch_complete",
  BackgroundTaskStarted: "background_task_started",
  BackgroundTaskUpdated: "background_task_updated",
  BackgroundTaskCompleted: "background_task_completed",
// Real-time progress of workflow run: one engine RunEvent and one event, appended to **parent session** (run itself has no session).
// The v4 side is reduced to workflowRuns state keys; the v3 side is stripped in shouldExposeSessionEventToProtocol.
//The script run event of the legacy `Workflow` tool is deliberately named with dynamic_:legacy (workflow_started/
// workflow_completed, script-workflow-runtime.ts) is another set of logs, and the same name can be really confusing.
  DynamicWorkflowRunProgress: "dynamic_workflow_run_progress",
  PermissionRequested: "permission_requested",
  PermissionResolved: "permission_resolved",
  PermissionDenied: "permission_denied",
  UserInputAutoResolutionUpdated: "user_input_auto_resolution_updated",
  WorkspaceHookReviewRequested: "workspace_hook_review_requested",
  WorkspaceHookReviewSettled: "workspace_hook_review_settled",
  WorkspaceHookReviewSuperseded: "workspace_hook_review_superseded",
//Soft access control: emitted when the access status changes. When pendingCount=0, the projection layer clears the snapshot field.
  WorkspaceHookAdmissionUpdated: "workspace_hook_admission_updated",
  HookRunStarted: "hook_run_started",
  HookRunProgress: "hook_run_progress",
  HookRunCompleted: "hook_run_completed",
  HookRunFailed: "hook_run_failed",
  HookRunBlocked: "hook_run_blocked",
  CompactStarted: "compact_started",
  CompactCompleted: "compact_completed",
  CompactFailed: "compact_failed",
  CompactBoundary: "compact_boundary",
  MicrocompactBoundary: "microcompact_boundary",
  RewindTriggered: "rewind_triggered",
  CheckpointCreated: "checkpoint_created",
  TargetChanged: "target_changed",
  TargetCompletionVerification: "target_completion_verification",
  SubagentSpawned: "subagent_spawned",
  SubagentMessage: "subagent_message",
  SubagentStopped: "subagent_stopped",
  Interrupt: "interrupt",
  Cancel: "cancel",
  Resume: "resume",
  Error: "error",
} as const;

export type SessionEventType = (typeof SessionEventType)[keyof typeof SessionEventType];

// -----------------------------------------------
// Event Payloads
// -----------------------------------------------

export interface SessionCreatedPayload {
  planEnabled?: boolean;
  mode: CollaborationMode;
  contextWindow: number;
}

export interface SessionResumedPayload {
  directory: string;
  interruptedToolCount: number;
  messageCount: number;
  partCount: number;
  recoveredCompactTimelineCount?: number;
  recoveredSteerInputCount?: number;
  resumedTodoCount?: number;
}

export interface SessionForkedPayload {
  forkedSessionId?: SessionId;
  originalSessionId: SessionId;
  restoredFileCount?: number;
  restoredSnapshotRef?: string;
  strategy?: "fork_required";
  targetCheckpointId?: string;
  targetMessageId?: MessageId;
  /** @deprecated Use targetMessageId for message-level forks. */
  forkPoint: number;
}

export interface SessionCompactedPayload {
  compactBoundary: CompactBoundaryPayload;
  summary?: string;
  preservedEventCount?: number;
  removedEventCount?: number;
}

export type CompactLifecyclePayload = CompactTimelinePayload;

export type MicrocompactBoundaryEventPayload = MicrocompactBoundaryPayload;

/**
* Attachment rendering (additive): The user inputs lightweight display meta information of the attachment, which is delivered with TurnStarted.
* The v4 projection fills in the attachments of the userInput row accordingly.
* Only the fields required for display are carried, and the content body is not included (the content is transferred to FilePart/artifact through resolve/persist).
 */
export interface TurnAttachmentMeta {
  fileName: string;
  mime: string;
  bytes: number;
/** Content reference (local path/artifact URI); default when there is no stable reference such as data URL. */
  ref?: string;
}

/**
* Structured payload for workflow notifications.
* Casting on the launch side, bounded (summary≤500 / result≤4000 / error≤2000 / reports.preview item by item≤500, ≤8 items /
* artifacts ≤8 items, title≤120 / question·context≤4000); with shared
* workflowNotificationMetaSchema maintains manual synchronization.
* The batch notification round is deliberately not carried - the correspondence between one manifest per round does not hold in batches.
 */
export type WorkflowNotificationMeta =
  | {
      kind: "terminal";
      status: "completed" | "errored" | "stopped";
/** Only present if `status === "stopped"` is present. */
      stopReason?: "user" | "model" | "provider" | "interrupted" | "superseded";
      summary: string;
      result?: string;
      resultForm?: "prose" | "json";
      resultTruncated?: true;
      error?: string;
      reports?: { count: number; shown: number; preview: string[] };
      /**
* Chip load of user interface products: ≤ 8 pieces,
* Set `artifactsTruncated` beyond (including those filtered out by category).
       *
* ⚠ Terminology: The artifact here is the output of the script published to the user via `artifact.*`, which is the same as the output on the same payload.
* `result` (the top-level return value of the script, also called artifact inside the engine) is two different things.
       */
      artifacts?: {
        id: string;
        kind: "file" | "markdown" | "chart" | "table" | "metrics" | "board";
        title?: string;
        version: number;
        contentType?: string;
/** run's deliverable; the list starts with it. Only it has a `description` (which completes the card's delivery content). */
        primary?: true;
        description?: string;
      }[];
      artifactsTruncated?: true;
      durationMs?: number;
    }
  | {
      kind: "escalation";
      qid: string;
      actor: string;
      question: string;
      context?: string;
      askedAt?: number;
    }
/** run-level stall: one for each stall segment, not the final state. */
  | {
      kind: "stall";
      sinceMs: number;
      reason?: string;
      cap?: number;
    };

/** The display meta-information of the independent background result wheel; the batch wheel can reuse the representative task identity and synthesize the title, without reverse parsing from the notification text. */
export interface BackgroundResultOriginMeta {
/** `workflow` is dynamic-workflow run (workId ≡ runId), reusing the entire background notification pipeline. */
  backgroundSource: "bash" | "subagent" | "workflow";
  workId: string;
  title: string;
/** Only present on a single notification wheel with backgroundSource === "workflow"; the only data source for manifest rendering. */
  workflowNotification?: WorkflowNotificationMeta;
}

/**
* turn whether to host real Agent execution. controlOnly establishes timeline boundaries only for visible control input,
* Session running/activeWorks must not be advanced, nor should a "Working/Worked" state be generated.
 */
export type TurnExecutionKind = "agent" | "controlOnly";

export type OffPeakRunType = "init" | "resume";

export type TurnBackgroundAttribution =
  | { automationId: string; offPeakTaskId?: never; offPeakRunType?: never }
  | {
      offPeakTaskId: string;
      offPeakRunType?: OffPeakRunType;
      automationId?: never;
    }
  | {
      automationId?: undefined;
      offPeakTaskId?: undefined;
      offPeakRunType?: never;
    };

/**
* Hub directly launches the launch wheel metadata of a saved workflow. The same `metadata` (cold recovery source) and `TurnStarted` written into the user message at the same time
* payload (live projection source) - the same shape as hot and cold, the projection draws the startup card according to it instead of displaying text.
 */
/** The display on startup metadata only allows create_workflow projection (identical to the shared row schema). */
export type WorkflowLaunchDisplay = Extract<ToolResultDisplayPayload, { kind: "create_workflow" }>;

/**
* "What was changed" in the settings wheel: only the changed settings are present.
* If one end of each item from / to is missing, that end is the default (model = session model, upper limit = local upper limit). `ceiling` is the local upper limit,
* For the reading "13 → 4". Identical to shared `workflowSettingsAmendMetaSchema`.
 */
export interface WorkflowSettingsAmendMeta {
/** The run replaced (or continued) by this adjustment. */
  predecessorRunId: string;
  subagentModel?: { from?: string; to?: string };
  maxConcurrency?: { from?: number; to?: number };
  ceiling?: number;
}

export interface WorkflowLaunchMeta {
/** The identity of run (≡ backgroundTaskId ≡ cancelBackgroundWork’s workId). */
  runId: string;
  /**
* Start the synthetic toolCall id of this run (hub startup `launch-<uuid>`, setting wheel `settings-<uuid>`;
* Tool card → Details page associated key).
   */
  toolCallId: string;
  /**
* Workflow name (parsing result, non-user/model can override); both the startup card title and the session title take it. The center activates the constant presence; sets the rotation to be
* Adjusted run's own name, unnamed run means absent (the UI uses a vague word instead of using the run id as the title).
   */
  name?: string;
/** The actual scope resolved (not the scope that may be absent from the request). Only for hub startup: there is no saved file to point to in the settings wheel. */
  scope?: SavedWorkflowScope;
/** Save the location of the file, used for details/diagnosis; does not participate in execution. Only central activation. */
  path?: string;
/** The actual parameters of this run (verified according to the declaration and backfilled with default values); see {@link boundWorkflowLaunchMeta} for bounds. */
  args?: Record<string, unknown>;
/** Workflow description (description of saved metadata), bounded ≤ 500 characters. */
  description?: string;
  /**
* Display of `create_workflow` results compiled before startup (bounded cause-and-effect diagram + diagnosis), and CreateWorkflow tool line
* `display` has the same projection and the same constructor. In the run details side panel, press toolCallId to find the "initiating line" to get the picture - there is no direct startup
* Tool row, the picture is taken from here.
   */
  display?: WorkflowLaunchDisplay;
  /**
* The original text of the script actually executed by this run (the same as `port.submit.scriptText`). Tool path put it in the tool input parameter
* `input.script` is for reading by the Script area of ​​the side panel. This is the starting point of the same fact; it is bounded to see
* {@link WORKFLOW_LAUNCH_SCRIPT_MAX_CHARS} (the overall absence of the super boundary, and the absence of the side panel Script area).
   */
  script?: string;
/** Set the turn of events: which run is this run revised with "configuration" and what has been changed. */
  amend?: WorkflowSettingsAmendMeta;
}

/** Upper JSON serialization bound (bytes) for {@link WorkflowLaunchMeta.args}. If it goes out of bounds, it is lost and replaced with a mark key. */
export const WORKFLOW_LAUNCH_ARGS_MAX_BYTES = 4_096;
/** The character upper bound for {@link WorkflowLaunchMeta.description}. */
export const WORKFLOW_LAUNCH_DESCRIPTION_MAX_CHARS = 500;
/**
* The upper character bound for {@link WorkflowLaunchMeta.script}. The `input.script` in the tool path is transcribed without any boundaries.
* Here is an upper limit that is much larger than any reasonable dwf script and can prevent the entire warehouse from being accidentally blocked; the entire super-boundary is absent instead of truncated
* (The half-cut script has no meaning to the Script area, and the picture is still in the display).
 */
export const WORKFLOW_LAUNCH_SCRIPT_MAX_CHARS = 256_000;
/**
* args Substitution key when out of bounds. The real parameters always follow the standard sentence of the model (that part is not truncated), and the metadata here is only fed to the startup card;
* A large 4KB parameter bag cannot be displayed on the card, so the whole is replaced with a single readable mark instead of being stuffed. Use ellipses
* As a key: it falls outside `SAVED_WORKFLOW_NAME_PATTERN` and cannot collide with the real parameter name.
 */
export const WORKFLOW_LAUNCH_ARGS_TRUNCATED_KEY = "…";

/**
* Include startup wheel metadata within the boundary. args exceeds {@link WORKFLOW_LAUNCH_ARGS_MAX_BYTES} after serialization, which is the whole
* Replaced with `{ "…": "arguments omitted (N bytes)" }` tag (see {@link WORKFLOW_LAUNCH_ARGS_TRUNCATED_KEY});
* description exceeds {@link WORKFLOW_LAUNCH_DESCRIPTION_MAX_CHARS}, that is, truncated and ellipsis added. Cast side in-place call,
* Let message metadata and TurnStarted payload get the same bounded value.
 */
export function boundWorkflowLaunchMeta(input: WorkflowLaunchMeta): WorkflowLaunchMeta {
  const bounded: WorkflowLaunchMeta = {
    runId: input.runId,
    toolCallId: input.toolCallId,
    ...(input.name === undefined ? {} : { name: input.name }),
    ...(input.scope === undefined ? {} : { scope: input.scope }),
    ...(input.path === undefined ? {} : { path: input.path }),
  };
  if (input.args !== undefined) {
    const serialized = JSON.stringify(input.args);
    const bytes = serialized === undefined ? 0 : Buffer.byteLength(serialized, "utf8");
    bounded.args =
      bytes > WORKFLOW_LAUNCH_ARGS_MAX_BYTES
        ? { [WORKFLOW_LAUNCH_ARGS_TRUNCATED_KEY]: `arguments omitted (${bytes} bytes)` }
        : input.args;
  }
  if (input.description !== undefined) {
    bounded.description =
      input.description.length > WORKFLOW_LAUNCH_DESCRIPTION_MAX_CHARS
        ? `${input.description.slice(0, WORKFLOW_LAUNCH_DESCRIPTION_MAX_CHARS - 1)}…`
        : input.description;
  }
// The display has been limited in length at the construction location (createCreateWorkflowDisplay + boundCausalityGraph) and is transparently transmitted as it is.
  if (input.display !== undefined) bounded.display = input.display;
  if (input.script !== undefined && input.script.length <= WORKFLOW_LAUNCH_SCRIPT_MAX_CHARS) {
    bounded.script = input.script;
  }
// The amend block itself is bounded (the two model strings are taken from the parsed canonical form by the caller, and have the same upper bound as the run state).
  if (input.amend !== undefined) bounded.amend = input.amend;
  return bounded;
}

export interface TurnStartedPayloadBase {
/** The monotonic epoch millisecond of the execution entry; hooks/persistence occurs before TurnStarted is released, and the start of execution cannot be inferred based on the release time. */
  executionStartedAt?: number;
  turnNumber: number;
  input: string;
  /**
* v4 file summary launches query based on turn rowId, but workspace checkpoint uses user messageId
* To restore the anchor point; TurnStarted must carry the same persistent id so that projection can change the userInput row
* Find the corresponding checkpoint.
   */
  messageId?: MessageId;
  inputId?: string;
/** Only admission is transparently passed to telemetry fact without text; it cannot be deduced from session ownership. */
/** Stable cancellation of identity overriding primary turn → goal verify/continue within the same runtime command. */
  foregroundExecutionId?: string;
  queryId?: QueryId;
  inputSource?: SyntheticUserMessageSource;
  inputVisibility?: MessageVisibility;
  /**
* The hub directly launches the launch wheel metadata of a saved workflow (when `inputSource === "workflow_launch"` is present).
* The live projection draws the startup card according to it; aligned with the same part in the message metadata (hot and cold are the same shape).
   */
  workflowLaunch?: WorkflowLaunchMeta;
/** The default is agent, compatible with old events and historical transcripts. */
  executionKind?: TurnExecutionKind;
  /**
* `input` Starting from this index is the engine additional text (dwf ask endnote / nudge), the GUI folds it into the disclosure; 0 = the entire article is;
* Absent = None. Aligned with the same copy in the message metadata (hot and cold are of the same shape).
   */
  epilogueStart?: number;
/** Structured source of idle background wake; active-loop merging does not create independent TurnStarted. */
  originMeta?: BackgroundResultOriginMeta;
/** Transparent transmission only when all members of a background notification batch come from the same source; leave mixed sources blank. */
  backgroundSource?: BackgroundResultOriginMeta["backgroundSource"];
  targetId?: string;
/** The attachment meta information entered by the user in this round (default when there is no attachment). */
  attachments?: TurnAttachmentMeta[];
/** CLI admission metadata; inputId continues to be a sourceCommandId compatible anchor. */
  intent?: TurnInputIntentMetadata;
}

export type TurnStartedPayload = TurnStartedPayloadBase & TurnBackgroundAttribution;

export interface TurnInputReceivedPayload {
  input: string;
  attachments?: Attachment[];
}

export interface TurnSteerQueuedPayload {
  pendingInputId: string;
  inputId?: string;
  queryId?: QueryId;
  input: string;
  inputPreview: string;
  inputSize: number;
  commandKind?: TurnSteerCommandKind;
  source?: TurnSteerSource;
  inputPresentation?: RuntimeInputPresentation;
/** The tool name that is not exposed to the provider when queuing input consumption. */
  toolDisallowlist?: readonly string[];
/** Delivery semantics: queue=cut new product turn when consuming; guide=inline current round. */
  delivery?: TurnSteerDeliveryMode;
  targetTurnId: TurnId;
  queueLength: number;
  intent?: TurnInputIntentMetadata;
}

export interface TurnSteerDeliveryChangedPayload {
  pendingInputId: string;
  targetTurnId: TurnId;
  requestedDelivery: "guide";
  admittedDelivery: "queue";
  fallbackReasonCode: string;
/** The complete intent after change; reducer/cold replay does not guess delivery from the bypass state. */
  intent?: TurnInputIntentMetadata;
}

export interface TurnSteerReorderedPayload {
  orderedPendingInputIds: string[];
  targetTurnId: TurnId;
}

export interface TurnSteerDispatchChangedPayload {
  pendingInputId: string;
  reservationId?: string;
  state: "queued" | "reserved" | "promoting";
  targetTurnId: TurnId;
}

export interface QueueAutoDrainChangedPayload {
  autoDrain: boolean;
}

export interface FollowupModeChangedPayload {
  mode: "queue" | "guide";
}

export interface TurnSteerDrainedPayload {
  pendingInputIds: string[];
  queryIds?: QueryId[];
  targetTurnId: TurnId;
  injectedMessageIds: MessageId[];
  /**
* The drain fact comes with its own text and persistent messageId, and projection no longer relies on it.
* Get the text of the memory queue status (if the queue item cannot be found, the user line will be silently lost).
* delivery determines the semantics of wheel cutting; the default is queue (one round for each line).
   */
  drainedInputs?: Array<{
    pendingInputId: string;
    messageId: MessageId;
    text: string;
    delivery?: TurnSteerDeliveryMode;
    intent?: TurnInputIntentMetadata;
    toolDisallowlist?: readonly string[];
  }>;
}

export interface TurnSteerRejectedPayload {
  reason: TurnSteerRejectReason;
  activeTurnId?: TurnId;
  expectedTurnId?: TurnId;
  inputPreview?: string;
  inputSize?: number;
}

export interface TurnSteerDiscardedPayload {
  pendingInputIds: string[];
  targetTurnId: TurnId;
// user_removed (v4 queue single item deletion): The user explicitly removes an item from the queue, which is different from the turn life cycle discarding.
// promoted: sendQueuedNow has obtained execution rights and is only removed from the queue projection; session_input must continue
// Remain admitted until user message is atomically promoted (or discarded after reboot).
  reason: "turn_cancelled" | "turn_failed" | "session_resumed" | "user_removed" | "promoted";
}

export interface SessionInputPromotedPayload {
  pendingInputId: string;
  sourceCommandId: string;
  messageId: MessageId;
}

export interface TurnCompletePayload {
  response: string;
  tokenCount: number;
  usage?: ModelUsageSummary;
  toolCallCount: number;
/** The current query has been successfully submitted to the provider. The number of model products with visible persistent history. */
  historyRoundCount?: number;
  duration: number;
  inputId?: string;
  resultType: TurnResultType;
/** Whether the current turn has consumed background result notifications from subagent. */
  backgroundSubagentResultConsumed?: boolean;
/** Whether the current turn has consumed background notifications from workflow (dynamic-workflow run). */
  workflowResultConsumed?: boolean;
/** Internal preemption (such as sendQueuedNow) only terminates the current turn and is not equivalent to the user manually stopping the queue. */
  preserveQueueAutoDrainOnCancel?: boolean;
}

export interface TurnErrorPayload {
  error: ErrorPayload;
  turnPhase: string;
  inputId?: string;
/** Whether the current turn has consumed background result notifications from subagent. */
  backgroundSubagentResultConsumed?: boolean;
/** Whether the current turn has consumed background notifications from workflow (dynamic-workflow run). */
  workflowResultConsumed?: boolean;
}

export type TurnResultType =
  | "success"
// "cancelled": User-initiated interruption (TurnCancelled) is a normal end. Use TurnComplete to report instead of TurnError.
  | "cancelled"
  | "error_max_turns"
  | "error_max_budget"
  | "error_during_execution"
  | "error_max_tool_calls";

export interface UserMessagePayload {
  content: string;
  attachments?: Attachment[];
}

export interface AssistantMessagePayload {
  content: string;
  toolCalls?: ToolCallPayload[];
}

export interface AssistantFeedbackUpdatedPayload {
  entityId: string;
  feedback: "like" | "dislike" | null;
}

export interface SystemMessagePayload {
  type: "init" | "compact_boundary" | "interrupted";
  content: string;
  compactBoundary?: CompactBoundaryPayload;
}

export interface SessionTitleUpdatedPayload {
  messageID?: MessageId;
  previousTitle: string;
  source: SessionTitleSource;
  title: string;
}

export interface SessionModeChangedPayload {
  permissionGrant?: { interactionId: string; queueItemIds: string[] };
  planEnabled?: boolean;
  previousPlanEnabled?: boolean;
  mode: CollaborationMode;
  previousMode: CollaborationMode;
  source: "tool" | "command" | "system";
  toolCallId?: ToolCallId;
}

export type TargetCompletionVerificationStatus =
  | "started"
  | "completed"
  | "failed_closed"
  | "cancelled";

export interface TargetCompletionVerificationPayload {
  targetId: string;
  status: TargetCompletionVerificationStatus;
  verificationId: string;
/** Shared with the front-end runtime command that triggers the verification, used for Stop as stale guard. */
  foregroundExecutionId?: string;
/** When sendQueuedNow preempts the verifier, only the goal is paused and the future queue is not paused. */
  preserveQueueAutoDrainOnCancel?: boolean;
  verification?: GoalCompletionVerificationOutput;
  goalIteration?: number;
  anchorAssistantMessageId?: MessageId;
  anchorTurnId?: TurnId;
}

export interface ModelRequestPayload {
  messages: ModelMessage[];
  providerId: string;
  modelId: string;
  querySource?: string;
  temperature?: number;
  maxTokens?: number;
}

export type ModelSelectionOrigin = "registryFallback";

export interface ModelSelectedPayload {
  modelSelection: ModelSelection;
/** The reasoning parsed out by Active Model is only used to perform fact/display projection and does not belong to sparse Selection. */
  effectiveReasoningLevel?: string;
/** Missing means only updating the current selection; null means explicit ∅→selection model boundary. */
  previousModelSelection?: ModelSelection | null;
/** Only carried when Agent automatically restores the model; missing indicates normal selection or old events. */
  origin?: ModelSelectionOrigin;
/** The thinking gear ability corresponding to the current selection; it is released atomically with the same event as the model selection. */
  supportedThoughtLevels?: string[];
/** The context window of the actual application of the current selection in runtime; null means explicit clearing, and missing fields are compatible with old events. */
  contextWindow?: number | null;
}

export type ModelStreamingKind =
  | "start"
  | "text_start"
  | "text_delta"
  | "text_end"
  | "reasoning_start"
  | "reasoning_delta"
  | "reasoning_end"
  | "tool_input_start"
  | "tool_input_delta"
  | "tool_input_end"
  | "tool_call"
  | "finish"
  | "error";

export interface ModelStreamingPayload {
  delta: string;
  done: boolean;
  kind?: ModelStreamingKind;
  assistantMessageId?: MessageId;
  partId?: PartId;
  toolCallId?: ToolCallId;
  toolName?: string;
  input?: unknown;
  providerExecuted?: boolean;
}

export type ModelNetworkStatusPayload = ModelNetworkStatusEvent;

export type ModelAnomalyWarningCategory =
  | "tool_call_budget"
  | "repeated_tool_call"
  | "provider_finish_mismatch"
  | "malformed_tool_call";

export type ModelAnomalyWarningSeverity = "info" | "warning";

export interface ModelAnomalyWarningPayload {
  category: ModelAnomalyWarningCategory;
  severity: ModelAnomalyWarningSeverity;
  observedCount?: number;
  threshold?: number;
  toolName?: string;
  toolCallId?: ToolCallId;
  warningInjected: boolean;
  modelVisibleMessageId?: MessageId;
}

export type NetworkRequestSource = "http_client" | "model" | "tool" | "mcp" | "plugin" | "unknown";

export type NetworkRequestLifecycleStatus = "pending" | "complete" | "error";

export interface NetworkRequestStatusPayload {
  requestId: string;
  source: NetworkRequestSource;
  status: NetworkRequestLifecycleStatus;
  method: string;
  url: string;
  startedAt: string;
  completedAt?: string;
  durationMs?: number;
  statusCode?: number;
  error?: string;
  toolCallId?: ToolCallId | string;
  toolName?: string;
  attempt?: number;
  egress?: HttpClientEgressInfo;
}

export interface ModelCompletePayload {
  cacheHit?: {
    inputTokens: number;
    cacheReadTokens: number;
    cacheWriteTokens: number;
    latestHitRate: number | null;
    hitRate: number | null;
    hitRateRequestCount: number;
    totalInputTokens: number;
    totalCacheReadTokens: number;
    totalCacheWriteTokens: number;
  };
  content: string;
  contextUsageBreakdown?: ContextUsageBreakdownItem[];
  contextWindow?: number;
  fileChanges?: TurnFileChangeSummary;
  querySource?: string;
  stopReason: string;
/** The number of complete tool calls assembled from the current model request; old events may be missing. */
  toolCallCount?: number;
  usage: TokenUsage | ModelUsage;
}

export interface TurnFileChangeSummary {
  additions: number;
  deletions: number;
  files: number;
  items: TurnFileChangeSummaryItem[];
}

export interface TurnFileChangeSummaryItem {
  additions: number;
  deletions: number;
  path: string;
  toolNames?: string[];
  writeCount: number;
}

export type ContextUsageBreakdownSource =
  | "system_prompt"
  | "meta_user_context"
  | "skills"
  | "tool_prompt"
  | "system_tool_schemas"
  | "mcp_tool_schemas"
  | "messages";

export interface ContextUsageBreakdownItem {
  source: ContextUsageBreakdownSource;
  chars: number;
}

export interface ModelErrorPayload {
  error: ErrorPayload;
  retryable: boolean;
}

export interface ToolCallScheduledPayload {
  toolCallId: ToolCallId;
  assistantMessageId?: MessageId;
  toolName: string;
  input: unknown;
  dependencies?: ToolCallId[];
  parallelGroupIndex?: number;
  canRunParallel?: boolean;
  display?: ToolResultDisplayPayload;
  schedule: ToolSchedulePayload;
}

export interface ToolCallStartedPayload {
  toolCallId: ToolCallId;
  toolName?: string;
  startedAt: Date;
  display?: ToolResultDisplayPayload;
  /**
* Side effect capabilities after **parsing** of this call (tool metadata + runtime capabilities of parsing by input parameters, such as Bash's read-only command determination).
* The event is emitted before the handler takes action, so the subscriber can know "it is about to be written" before the first byte is placed on the disk - dynamic-workflow
* The driver accordingly closes amend-resume's import cache (`isWorkspaceMutatingToolCall`). Optional: There are no events earlier than this field.
   */
  readOnly?: boolean;
  sideEffectScope?: ToolSideEffectScope;
}

export interface ToolCallProgressPayload {
  toolCallId: ToolCallId;
  toolName?: string;
  elapsedMs?: number;
  pid?: number;
  stdoutBytes?: number;
  stderrBytes?: number;
  outputBytes?: number;
  outputPreview?: ExecutionOutputPreview;
  stdoutTail?: string;
  stderrTail?: string;
}

export interface ToolCallResultPayload {
  toolCallId: ToolCallId;
  result: ToolResultPayload;
  duration: number;
/** Skill metadata is only used for telemetry and does not enter the result.content visible to the model. */
  skillMetadata?: SkillTelemetryMetadata;
}

export interface ToolCallErrorPayload {
  toolCallId: ToolCallId;
  error: ErrorPayload;
/** Skill metadata is only used for telemetry and does not enter the error body visible to the model. */
  skillMetadata?: SkillTelemetryMetadata;
}

export interface ToolBatchCompletePayload {
  toolCallIds: ToolCallId[];
  successCount: number;
  errorCount: number;
}

export type BackgroundTaskStatus =
  | "running"
  | "completed"
  | "failed"
  | "timed_out"
  | "cancelled"
  | "spawn_error"
  | "lost";

export interface BackgroundTaskPayloadBase {
  taskId: string;
  toolCallId?: ToolCallId | string;
  toolName?: string;
// "workflow" = workflow run(CreateWorkflow). Tracker behavior has been modified per-tool lifecycleProvider
// Dispatch, taskKind only determines panel grouping and icons, so widening is a pure display modification and does not change any life cycle semantics.
  taskKind?: "bash" | "subagent" | "workflow";
  childSessionId?: SessionId | string;
  blocked?: boolean;
  blockedReason?: "interactive_prompt_detected" | "no_output_progress" | string;
  cancellable?: boolean;
  cancelRequestedAt?: Date;
  command?: string;
  description?: string;
  status: BackgroundTaskStatus;
  pid?: number;
  startedAt?: Date;
  completedAt?: Date;
  outputPath?: string;
  stderrPersistedOutputPath?: string;
  stdoutPersistedOutputPath?: string;
  outputBytes?: number;
  outputTruncated?: boolean;
  outputTail?: string;
  stderrBytes?: number;
  stderrTail?: string;
  stdoutBytes?: number;
  stdoutTail?: string;
  terminalId?: string;
}

export type BackgroundTaskStartedPayload = BackgroundTaskPayloadBase & {
  status: "running";
};

export type BackgroundTaskUpdatedPayload = BackgroundTaskPayloadBase;

export type BackgroundTaskCompletedPayload = BackgroundTaskPayloadBase & {
  status: Exclude<BackgroundTaskStatus, "running">;
};

/**
* A workflow run progress event (parent session). The field is the same as {@link DynamicWorkflowRunEvent}——
* **One serialization, two consumers**: The event page of `listEvents` and the session event here use the same bounded payload,
* The reader therefore does not need two sets of interpretation rules. `type` is already occupied by the session event envelope, so the engine event type is called `eventType`.
 */
export interface DynamicWorkflowRunProgressPayload {
  runId: string;
/** Initiate the CreateWorkflow tool call for this run (association key of tool card → details page). */
  toolCallId?: ToolCallId | string;
/** journal sequence (`appendEvent` monotonic allocation); the cursor of the event log is the same as it. */
  sequence: number;
/** Engine event types: run-started / actor-created / node-* / usage-updated / log / report / phase-entered / run-settled. */
  eventType: string;
  payload: Record<string, unknown>;
  truncated?: boolean;
/** Derived field (toProgressPayload): The id of this subagent session on `actor-created` (driver cast `sess_dwf-…`). */
  actorSessionId?: string;
  /**
* Derived field (similar to `actorSessionId`, see toProgressPayload): `run-launched.inputId` for this run.
* Only hangs on the `actor-created` / `run-settled` events for the downstream to assign the sub-agent to the round that initiated the run. The run before upgrade does not have `run-launched` and the field is absent.
   */
  launchInputId?: string;
}

export interface PermissionRequestedPayload {
  requestId?: string;
  toolCallId: ToolCallId;
  toolName: string;
  riskLevel: RiskLevel;
  reason: string;
  input: unknown;
  suggestedPermissionUpdates?: PermissionUpdate[];
  origin?: InteractionRequestOrigin;
  /**
* Confirmation preview reported by the tool, produced by the `prepareApproval` hook. display projection of multiplexed result channels,
* Let ask share the same set of bounded shapes with the scorecard, without adding unbounded fields.
   */
  display?: ToolResultDisplayPayload;
  fullAccessSupported?: boolean;
  optionsPolicy?: PermissionOptionsPolicy;
}

export interface PermissionResolvedPayload {
  requestId?: string;
  toolCallId: ToolCallId;
  decision: PermissionDecision;
  reason?: string;
  modifiedInput?: unknown;
}

export interface PermissionDeniedPayload {
  toolCallId: ToolCallId;
  toolName: string;
  reason: string;
  inputSummary?: unknown;
}

export type UserInputAutoResolutionState =
  | {
      state: "hiddenGrace" | "visibleCountdown";
      startedAt: number;
      visibleAt: number;
      deadlineAt: number;
    }
  | {
      state: "snoozed";
      startedAt: number;
      snoozedAt: number;
    };

export interface UserInputAutoResolutionUpdatedPayload {
  interactionId: string;
  toolCallId: ToolCallId;
  autoResolution: UserInputAutoResolutionState;
}

export interface WorkspaceHookReviewRequestedPayload {
  request: unknown;
}

export interface WorkspaceHookReviewSettledPayload {
  interactionId: string;
  state: "resolved" | "timed_out" | "configuration_error";
  reasonCode?: string;
}

export interface WorkspaceHookReviewSupersededPayload {
  interactionId: string;
  supersededByInteractionId: string;
}

//Soft access control: After the access layer completes the evaluation, the pending status is reported.
// pendingCount = configuredEnabled && admissionClass === "pending" number of claims;
// pendingCount === 0 → The projection layer sets snapshot.workspaceHookAdmission to null (the prompt bar disappears).
export interface WorkspaceHookAdmissionUpdatedPayload {
  pendingCount: number;
  bundleDigest: string;
  workspaceIdentity?: string;
}

export type PermissionDecision = "allow" | "deny" | "escalate" | "modify";

export type TargetChangedAction =
  | "set"
  | "status_updated"
  | "cleared"
  | "usage_accounted"
  | "run_started"
  | "run_finished"
  | "summary_updated";

export type TargetChangedSource = "command" | "tool" | "runtime";

export interface TargetChangedPayload {
  action: TargetChangedAction;
  source: TargetChangedSource;
  target: SessionGoal | null;
  previousTarget?: SessionGoal | null;
}

// -----------------------------------------------
// Supporting Types
// -----------------------------------------------

export interface Attachment {
  type: "file" | "image" | "pdf" | "url";
  path?: string;
  content?: string;
  mimeType?: string;
}

export interface ModelMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  toolCalls?: ToolCallPayload[];
  toolCallId?: string;
}

export interface ToolCallPayload {
  id: ToolCallId;
  name: string;
  input: unknown;
}

export interface ToolResultPayload {
  success: boolean;
  content: string;
  display?: ToolResultDisplayPayload;
  perf?: import("../tools/performance.js").ToolExecutionTelemetry;
  error?: ErrorPayload;
  truncated?: boolean;
  originalBytes?: number;
  returnedBytes?: number;
  budgetStrategy?: string;
  artifactPath?: string;
}

export interface ToolSchedulePayload {
  parallelGroups: ToolCallId[][];
  executionOrder: ToolCallId[];
}

export interface TokenUsage {
  inputTokens: number;
  outputTokens: number;
  totalTokens: number;
  cacheTokens?: number;
}

export interface ErrorPayload {
  code?: string;
  detail?: string;
/** The original message of the deepest non-wrapper frame in the error chain, used for client telemetry to locate the root cause. */
  underlyingErrorMessage?: string;
/** The detail/errorDetails/details content of the same underlying frame. */
  underlyingErrorDetail?: string;
  type: string;
  message: string;
  attribution?: ErrorAttribution;
  retryable?: boolean;
  stack?: string;
  data?: unknown;
}

/** Only carries aggregable failure facts and does not contain request bodies, credentials, URLs, or monitoring policies. */
export interface ErrorAttribution {
  source?: "provider" | "runtime" | "tool" | "network";
  reason?: string;
  errorPhase?: ModelApiErrorPhase;
  exceptionKind?: ModelFailureExceptionKind;
  providerId?: string;
  modelId?: string;
  providerKind?: string;
  transport?: "http" | "sse" | "websocket";
  statusCode?: number;
  providerErrorCode?: string;
  retryable?: boolean;
}

// CollaborationMode and RiskLevel are re-exported from ports

// -----------------------------------------------
// Event Factory
// -----------------------------------------------

export function createSessionEvent<T>(
  type: SessionEventType,
  sessionId: SessionId,
  payload: T,
  options?: {
    turnId?: TurnId;
    traceId?: TraceId;
    sequenceNumber?: number;
  },
): SessionEvent {
  return {
    id: crypto.randomUUID() as EventId,
    sessionId,
    turnId: options?.turnId,
    type,
    timestamp: new Date(),
    traceId: options?.traceId ?? (crypto.randomUUID() as TraceId),
    sequenceNumber: options?.sequenceNumber ?? 0,
    payload,
  };
}

export function createModelUsageSummaryFromEvents(
  events: readonly SessionEvent[],
): ModelUsageSummary | undefined {
  const usages: ModelUsage[] = [];

  for (const event of events) {
    if (event.type !== SessionEventType.ModelComplete) continue;
    const payload = event.payload as Partial<ModelCompletePayload>;
    if (payload.usage) {
      usages.push(payload.usage as ModelUsage);
    }
  }

  return createModelUsageSummary(usages);
}

export type SessionEventPayload =
  | SessionCreatedPayload
  | SessionResumedPayload
  | SessionForkedPayload
  | SessionCompactedPayload
  | SessionTitleUpdatedPayload
  | SessionModeChangedPayload
  | TurnStartedPayload
  | TurnInputReceivedPayload
  | TurnSteerQueuedPayload
  | TurnSteerDeliveryChangedPayload
  | TurnSteerDispatchChangedPayload
  | TurnSteerDrainedPayload
  | TurnSteerReorderedPayload
  | QueueAutoDrainChangedPayload
  | FollowupModeChangedPayload
  | TurnSteerRejectedPayload
  | TurnSteerDiscardedPayload
  | SessionInputPromotedPayload
  | TurnCompletePayload
  | TurnErrorPayload
  | UserMessagePayload
  | AssistantMessagePayload
  | SystemMessagePayload
  | ModelRequestPayload
  | ModelSelectedPayload
  | ModelStreamingPayload
  | StreamingToolLedgerPayload
  | StreamRecoveryAnchorPayload
  | StreamRecoveryStartedPayload
  | StreamRecoveryAnchorSelectedPayload
  | StreamRecoveryTailDiscardedPayload
  | StreamRecoveryRetryStartedPayload
  | StreamRecoveryBlockedPayload
  | ModelNetworkStatusPayload
  | ModelAnomalyWarningPayload
  | NetworkRequestStatusPayload
  | ModelCompletePayload
  | TargetCompletionVerificationPayload
  | ModelErrorPayload
  | ToolCallScheduledPayload
  | ToolCallStartedPayload
  | ToolCallProgressPayload
  | ToolCallResultPayload
  | ToolCallErrorPayload
  | ToolBatchCompletePayload
  | BackgroundTaskStartedPayload
  | BackgroundTaskUpdatedPayload
  | BackgroundTaskCompletedPayload
  | DynamicWorkflowRunProgressPayload
  | PermissionRequestedPayload
  | PermissionResolvedPayload
  | PermissionDeniedPayload
  | UserInputAutoResolutionUpdatedPayload
  | WorkspaceHookReviewRequestedPayload
  | WorkspaceHookReviewSettledPayload
  | WorkspaceHookReviewSupersededPayload
  | WorkspaceHookAdmissionUpdatedPayload
  | HookRunLifecyclePayload
  | CompactLifecyclePayload
  | MicrocompactBoundaryEventPayload
  | CheckpointCreatedPayload
  | RewindTriggeredPayload
  | CompactBoundaryPayload;
