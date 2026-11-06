/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { PERMISSION_FULL_ACCESS_OPTION_ID } from "@zcode/shared/zcode-protocol-v4";
// ProductProjection - CLI authoritative projection second reducer.
// Input: CLI event log (SessionEvent, authoritative source of truth); Output: ConversationDelta[].
// Snapshot promotion reuse protocol specification apply (applyConversationDeltas) - projection evolution and delta flow
// Byte-by-byte consistency is a construction guarantee, and the golden test uses independent replay cross-validation.
//
// Coverage: session/turn life cycle, streaming text/thinking, tool call state machine,
// Permission interaction, turn-steer queue, usage, error state, late final state rejection,
// compact marker, goal state machine, fork marker. Transmit shell (TopicFrame/subscribe) in subsequent pieces.
import {
  projectToolActivity,
  clearSettledOutputPreviews,
} from "./product-projection-bash-progress.js";
import type {
  CompactLifecyclePayload,
  AssistantFeedbackUpdatedPayload,
  DynamicWorkflowRunProgressPayload,
  HookRunLifecyclePayload,
  ModelCompletePayload,
  ModelNetworkStatusPayload,
  ModelSelectedPayload,
  ModelStreamingPayload,
  ModelUsage,
  PermissionDeniedPayload,
  PermissionRequestedPayload,
  PermissionResolvedPayload,
  SessionEvent,
  SessionForkedPayload,
  SessionInputPromotedPayload,
  StreamRecoveryRetryStartedPayload,
  StreamRecoveryStartedPayload,
  TargetChangedPayload,
  TargetCompletionVerificationPayload,
  ToolCallErrorPayload,
  ToolCallResultPayload,
  ToolCallScheduledPayload,
  ToolCallStartedPayload,
  ToolResultDisplayPayload,
  TurnCompletePayload,
  TurnErrorPayload,
  TurnInputIntentMetadata,
  TurnSteerDispatchChangedPayload,
  TurnSteerDiscardedPayload,
  TurnSteerDeliveryChangedPayload,
  TurnSteerDrainedPayload,
  TurnSteerQueuedPayload,
  UserInputAutoResolutionUpdatedPayload,
  WorkspaceHookReviewRequestedPayload,
  WorkspaceHookReviewSettledPayload,
  WorkspaceHookReviewSupersededPayload,
  WorkspaceHookAdmissionUpdatedPayload,
} from "@zcode/contracts";
import {
  CoreErrorType,
  AMEND_WORKFLOW_TOOL_NAME,
  CREATE_WORKFLOW_TOOL_NAME,
  SessionEventType,
  getModelUsageContextTokens,
} from "@zcode/contracts";
// review monotonicity ruling single source; projection only implements "apply strategy" (advance/no_current accepted,
// Ignore the rest; clear onSessionResumed across flows, etc.).
// (Change the direct monotonicity subpath; the re-export of the discovery barrel
// The Desktop build chain in packages/ui will fail to parse, and the App cannot be opened after restarting. )
import { verdictWorkspaceHookReviewRequest } from "@zcode/shared/workspace-hook-review-monotonicity";
import {
  extractPlanStepsFromToolInput,
  extractPlanStepsFromToolOutput,
  isZCodeModelRetryRecoveryProgressPayload,
  isZCodeFileStreamingToolInputPreviewTool,
  parseZCodeBackgroundTaskNotificationText,
  resolveZCodeBackgroundTaskControlKind,
  WORKFLOW_REFINE_PERMISSION_OPTION_ID,
  ZCODE_FILE_STREAMING_TOOL_INPUT_PREVIEW_MIN_INTERVAL_MS,
  zcodeBackgroundTaskNotificationToolUpdateStatus,
} from "@zcode/shared";
import type {
  AssistantTextRow,
  ApiRetryState,
  BackgroundWorkSummary,
  CuaAppIdentity,
  ConversationDelta,
  ConversationRow,
  ConversationRowTarget,
  ConversationSnapshot,
  GoalState,
  HookExecutionProjection,
  HookInvocationRow,
  PendingInteraction,
  ReasoningRow,
  SessionControl,
  StatePatch,
  SubagentRow,
  TimelineMarkerPayload,
  TimelineMarkerRow,
  ToolCallDisplay,
  ToolCallRow,
  SessionUsageState,
  RunningSubagentSummary,
  SubagentProjectionState,
  TurnHeaderRow,
  TurnWorkSegment,
  UserInputRow,
  UserInputQuestionPayload,
  QueueItem,
  MutableConversationSnapshotAccumulator,
  RowActions,
  WorkflowRunProgressEnvelope,
} from "@zcode/shared/zcode-protocol-v4";
import {
  parseListAppsSnapshot,
  readOfficialCuaAction,
  resolveCuaAppIdentity,
} from "./cua-app-snapshot.js";
import {
  PROTOCOL_V4_LIMITS,
  applyConversationDeltas,
  applyConversationDeltasMutable,
  createMutableConversationSnapshotAccumulator,
  reduceWorkflowRunsState,
  workspaceHookReviewRequestPayloadSchema,
} from "@zcode/shared/zcode-protocol-v4";
import {
  buildToolOutput,
  buildTurnHeaderRow,
  mapCompactMarkerOrigin,
  mapCompactMarkerStatus,
  mapGoalStatus,
  mapTurnResultToHeaderState,
} from "./projection-rows.js";
import {
  HYDRATION_TRACE_ID,
  computeAvailability,
  computeInputRouting,
  createInitialConversationSnapshot,
  deltaBumpsRevision,
} from "./projection-state.js";
import {
  normalizeConversationEvent,
  type CanonicalAssistantSegmentFact,
  type CanonicalModelStream,
  type CanonicalConversationFact,
  type CanonicalOpenSegmentIdentity,
  type CanonicalUserIntentFact,
  type ConversationNormalizationDiagnostic,
} from "./event-normalizer.js";
import {
  buildProtocolPermissionOptions,
  SESSION_ALLOW_PERMISSION_OPTION_KIND,
} from "../permission-options.js";
import { shouldHideInvalidToolCallFromProduct } from "../tool-call-product-visibility.js";

type HookInvocationRowContent = Omit<
  HookInvocationRow,
  | "actions"
  | "createdAt"
  | "createdAtSeq"
  | "entityId"
  | "productTurnId"
  | "rowId"
  | "turnId"
  | "visibility"
>;

interface PendingSessionHookInvocation {
  firstEvent: SessionEvent;
  content: HookInvocationRowContent;
}

const HOOK_SCRIPT_RUNNERS = new Set([
  "bash",
  "bun",
  "deno",
  "node",
  "node.exe",
  "powershell",
  "pwsh",
  "python",
  "python3",
  "ruby",
  "sh",
  "zsh",
]);
const USER_PROMPT_HOOK_BLOCK_ERROR_TYPE = "hooks_prompt_block";

function unquoteHookDisplayToken(token: string): string {
  if (token.startsWith('"') && token.endsWith('"')) {
    try {
      return JSON.parse(token) as string;
    } catch {
      return token.slice(1, -1);
    }
  }
  if (token.startsWith("'") && token.endsWith("'")) return token.slice(1, -1);
  return token;
}

function hookCommandLabel(commandDisplay: string): string | undefined {
  const tokens = commandDisplay.match(/"(?:\\.|[^"])*"|'[^']*'|\S+/gu) ?? [];
  const executableToken = tokens[0];
  if (!executableToken) return undefined;
  const executable = unquoteHookDisplayToken(executableToken).split(/[\\/]/u).at(-1);
  if (!executable) return undefined;
  const scriptToken = tokens[1];
  if (!HOOK_SCRIPT_RUNNERS.has(executable.toLowerCase()) || !scriptToken) return executable;
  const script = unquoteHookDisplayToken(scriptToken);
  if (!script || script.startsWith("-")) return executable;
  const scriptName = script.split(/[\\/]/u).at(-1);
  return scriptName ? `${executable} · ${scriptName}` : executable;
}

function hookExecutionDisplayName(
  descriptor: NonNullable<HookRunLifecyclePayload["descriptor"]>,
  hookIndex: number,
): string {
  const executable = hookCommandLabel(descriptor.commandDisplay);
  return (
    descriptor.statusMessage?.trim() ||
    (descriptor.pluginName && executable
      ? `${descriptor.pluginName} · ${executable}`
      : descriptor.pluginName || executable) ||
    `Hook #${hookIndex + 1}`
  );
}

/**
* config seed: initial value injected from runtime true value after projection initialization/cold recovery.
* Relationship with the event writing path (ModelSelected / SessionModeChanged): the seed only fills in "The event has not yet
* Touch" field - the log replay value always takes precedence ("the final value is subject to the log").
 */
export interface SessionConfigSeed {
  permissionGrant?: { interactionId: string };
  planEnabled?: boolean;
  modelSelection?: ModelSelectedPayload["modelSelection"];
  provider?: string;
  model?: string;
  thought?: string;
  thoughtLevels?: readonly string[];
  mode?: string;
}

export interface SessionUsageSeed {
  contextWindow: Omit<NonNullable<SessionUsageState["contextWindow"]>, "maxTokens"> & {
    maxTokens: number | null;
  };
  cumulative?: Partial<SessionUsageState["cumulative"]>;
}

interface ContextWindowProjectionState {
  maxTokens: number | null;
  touchedByEvent: boolean;
  usedTokens: number;
}

export interface SessionSubagentsSeed {
  revision: number;
  childSessionIds: string[];
  running: RunningSubagentSummary[];
}

export interface StableForkCandidate {
  productTurnId: string;
  transcriptTurnId: string;
  startMessageId: string | null;
  boundaryMessageId: string;
}

function cloneSparseModelSelection(
  selection: ModelSelectedPayload["modelSelection"],
): ModelSelectedPayload["modelSelection"] {
  return {
    providerId: selection.providerId,
    modelId: selection.modelId,
    ...(selection.options ? { options: { ...selection.options } } : {}),
  };
}

function sameSparseModelSelection(
  left: ModelSelectedPayload["modelSelection"] | undefined,
  right: ModelSelectedPayload["modelSelection"] | undefined,
): boolean {
  if (!left || !right) return left === right;
  return (
    left.providerId === right.providerId &&
    left.modelId === right.modelId &&
    left.options?.reasoningLevel === right.options?.reasoningLevel
  );
}

export type StableForkCandidateResolution =
  | { ok: true; candidate: StableForkCandidate }
  | {
      ok: false;
      reasonCode:
        | "guard.forkAssistantOnly"
        | "guard.forkTargetNotStable"
        | "guard.forkTargetAmbiguous"
        | "guard.compactOperationLock";
    };

export interface ConversationEditTarget {
  entityId: string;
  productTurnId: string;
  transcriptMessageId: string;
  coveredByStableCompact: boolean;
  intent: {
    kind: "sendText" | "sendGoalCommand";
    text: string;
    sourceCommandId?: string;
    clientId?: string;
    attachments?: CanonicalUserIntentFact["attachments"];
    queueItemId?: string;
    admissionSeq?: number;
    admittedAt?: number;
    requestedDelivery?: "auto" | "startNow" | "queue" | "guide";
    admittedDelivery?: "startNow" | "queue" | "guide";
    fallbackReasonCode?: string;
    modelSelection?: TurnInputIntentMetadata["modelSelection"];
    mode?: TurnInputIntentMetadata["mode"];
    planEnabled?: boolean;
    provenance?: CanonicalUserIntentFact["provenance"];
  };
}

export type ConversationRowTargetAction =
  | "forkAssistant"
  | "editUserQuery"
  | "retryTurn"
  | "applyFileRewind"
  | "fileChanges"
  | "fileRewindPreview"
  | "setAssistantFeedback";

export type ConversationRowTargetResolution =
  | {
      ok: true;
      action: ConversationRowTargetAction;
      row: ConversationRow;
      editTarget?: ConversationEditTarget;
      messageId?: string;
      messageIds?: string[];
    }
  | {
      ok: false;
      status: "stale" | "rejected";
      reasonCode: "proto.staleTarget" | "guard.actionUnavailable";
    };

// Old events do not have retryable fields; retain the retryable semantics of the historical UI, but new events must respect explicit false.
const LEGACY_TURN_ERROR_RECOVERABLE_FALLBACK = true;

function modelRetryReasonCode(
  reason: Extract<ModelNetworkStatusPayload, { type: "model_retry_scheduled" }>["reason"],
): string {
  switch (reason) {
    case "rate_limited":
      return "fault.provider.rateLimited";
// Off-peak queuing (429/3105) semantically means "the upstream lets us wait", and the UI falls into the current limiting and recoverable state.
    case "offpeak_queued":
      return "fault.provider.rateLimited";
    case "provider_overloaded":
    case "server_error":
      return "fault.provider.serverError";
    case "timeout":
      return "fault.network.timeout";
    case "stream_idle_timeout":
      return "fault.network.sseStalled";
    case "stale_connection":
      return "fault.network.sseDisconnected";
    case "network_error":
      return "fault.network.unreachable";
    case "auth_refresh":
    case "reasoning_signature_repair":
      return "fault.provider.requestFailed";
  }
}

function streamRecoveryReasonCode(
  failureKind: StreamRecoveryStartedPayload["failureKind"],
): string {
  switch (failureKind) {
    case "provider_timeout":
      return "fault.network.timeout";
    case "provider_network_error":
      return "fault.network.unreachable";
    case "provider_stream_error":
      return "fault.network.sseDisconnected";
    case "provider_turn_failed":
    case "unknown":
      return "fault.provider.requestFailed";
  }
}

function positiveInteger(value: number, fallback: number): number {
  return Number.isFinite(value) && value > 0 ? Math.floor(value) : fallback;
}

function nonNegativeInteger(value: number, fallback: number): number {
  return Number.isFinite(value) && value >= 0 ? Math.floor(value) : fallback;
}

/**
* Bug root cause (2026-09-21 large session lag tracking): command row actions are derivatives of row status.
* materializeCommandRowActions used to do two JSON.stringify comparisons for each candidate row. During streaming each
* The 30ms flush frame must pay this serialization cost for all turnHeader/userInput/assistantText lines in the entire window.
* The comparison result of most rows is "unchanged" - this is the largest constant term in a single frame in a long session.
 *
* actions are small objects (the key set is fixed by rowActionsSchema, and the values ​​are literals/short enumerations), shallow comparison and serialization
* Comparison is equivalent under the same construction path. Compare rather than enumerate keys by **keyset**: rowActionsSchema when action keys are added in the future
* There is no need to go back and synchronize this function - missing synchronization will make the new action always judged as "unchanged" and not be issued. `key in right` guaranteed key set
* Contracts of the same size but different contents will not be misjudged to be equal.
 */
function conversationRowActionsEqual(
  left: RowActions | undefined,
  right: RowActions | undefined,
): boolean {
  if (left === right) return true;
  if (left === undefined || right === undefined) return false;
  const leftKeys = Object.keys(left);
  const rightKeys = Object.keys(right);
  if (leftKeys.length !== rightKeys.length) return false;
  return leftKeys.every(
    (key) => key in right && left[key as keyof RowActions] === right[key as keyof RowActions],
  );
}

/**
* Whether the delta reduced in this frame may change the input of command row actions.
 *
* actions only depend on: row status and fileChanges (turnHeader), latest editable realUser row, latest retryable
* Snapshot-level facts such as assistant rows, compact activation state, and pendingInteractions. row.delta append-only by protocol
* text/inputText/output.text/summaryText (apply.ts's appendToRow), does not touch any of the above
* input, so the materialized result of a pure row.delta frame must be the same byte-for-byte as last time (all hits on the "unchanged" branch).
* Skipping is not an approximation, but an empty result of the same reducer when no input changes.
 *
* Empty delta sets are treated as "possible changes": this is the **conservative default** (does not exclude actions when reduction yields no deltas)
* Input changes), regardless of the caller of this function. The cold recovery closing completeHydrationReplay does not go through this function——
* It calls materializeCommandRowActions([]) directly (see there), during hydration materializeActions
* If false, actions will be delayed until the end is materialized.
 */
function deltasMayAffectCommandRowActions(deltas: readonly ConversationDelta[]): boolean {
  if (deltas.length === 0) return true;
  return deltas.some((delta) => delta.op !== "row.delta");
}

interface FileToolInputPreviewState {
  lastPublishedAt: number | null;
  pendingAppend: string;
}

type TurnModelBaseline =
  | { kind: "silentInitial" }
  | { kind: "sourceLess" }
  | { kind: "known"; provider: string; model: string; thought: string };

export class ProductProjection {
  private snapshot: ConversationSnapshot;
// The rowId search inside the reducer must be synchronized with rows.window; cold recovery used to scan the entire table every time find was performed.
// tool/turn The more final states there are, the more obvious the degradation will be. Ordinary reduction incremental maintenance, rewind to rebuild.
  private rowIndexById = new Map<number, number>();
  private hydrationAccumulator: MutableConversationSnapshotAccumulator | null = null;
  private nextRowId = 1;
  private streamingTextRowId: number | null = null;
  private streamingReasoningRowId: number | null = null;
// output-token Continue is a request-level resume within the same product turn and should not leak into a new body line.
// Only the last text row that meets the length/zero-tool/visual proximity condition is retained here, and any real boundaries will be cleared.
  private outputContinuationTextRowId: number | null = null;
  private toolRowIdByCallId = new Map<string, number>();
  private latestListAppsSnapshot = new Map<number, CuaAppIdentity>();
// snapshot is the authoritative state; this Set is just the derived index of TurnComplete's final state to avoid scanning the entire table in each round.
  private openForegroundToolCallIds = new Set<string>();
  private fileToolInputPreviewByCallId = new Map<string, FileToolInputPreviewState>();
  private subagentRowIdByAgentId = new Map<string, number>();
  private hookRowIdByInvocationId = new Map<string, number>();
//resume SessionStart has no turnId; first keep it in CLI projection, then the next real user-intent
// Allocate rowId/turnId after TurnStarted arrives. May not construct session-hooks:* synthetic turn.
  private pendingSessionHookInvocations = new Map<string, PendingSessionHookInvocation>();
// The terminal of async Hook may still be late after rewind; retain the invocation tombstone to avoid deletion of old branches
// Because the original row cannot be found, it is re-appended by the terminal-only compatible path.
  private rewoundHookInvocationIds = new Set<string>();
// Cold recovery transcript may contain ghost children where the old version is released first and then fails to be persisted. After store seed
// Must be continuously excluded, rather than just overwriting the snapshot once; otherwise the next unrelated event will materialize it from the historical row.
  private invalidSubagentChildSessionIds = new Set<string>();
// rowId → authoritative messageId side table. The command payload of forkAssistant/editUserQuery uses rowId
// Positioning, but old fork/rewind operations use messageId (history target) - the bridge layer is translated by this table.
//Do not enter row schema (the client only sends rowId, and messageId is the internal anchor point of the server to avoid contaminating the frozen row structure).
  private messageIdByRowId = new Map<number, string>();
// Continue After reusing rowId, the action anchor is advanced to the last assistant message; the old partial messageId
// Still need to be able to hit the same row for compact coverage, rewind and full round file fact recovery.
  private outputContinuationRowIdByMessageId = new Map<string, number>();
  private entityIdByRowId = new Map<number, string>();
// canonical command target is only addressed by stable entity identity; rowId is only for this materialization
// Transient lookup, changes after refresh/replay will not change the target identity.
  private editTargetByEntityId = new Map<string, ConversationEditTarget>();
  private currentEditableEntityId: string | null = null;
  private stableCompactCoverageBoundaryRowId: number | null = null;
  private turnHeaderRowIdByTurnId = new Map<string, number>();
  private compactMarkerRowIdByOperationId = new Map<string, number>();
// goal verify boundary identity = targetId_goalIteration
// (verificationId only attempt alias - same as iteration retry carrying new verificationId,
// The old implementation will grow a second marker by verificationId keying).
  private goalVerifyMarkerRowIdByLifecycleKey = new Map<string, number>();
// queue drain cuts out new ones within the same runtimeTurn
// product turn. runtimeTurnId → current productTurnId mapping; subsequent events pass through turnIdOf
// Fall under the latest productTurn. steer(guide) does not cut the wheel, inline the current wheel.
  private productTurnIdByRuntimeTurnId = new Map<string, string>();
  private runtimeTurnIdByProductTurnId = new Map<string, string>();
  private productTurnSplitOrdinalByRuntimeTurnId = new Map<string, number>();
  private currentProductTurnStartedAtMs: number | null = null;
// Delivery semantic side table: TurnSteerQueued is recorded according to the event payload (or followupMode),
// When draining, it is decided whether to cut the wheel or inline; after the ledger is implemented, the ledger will prevail.
  private deliveryByPendingInputId = new Map<string, "guide" | "queue">();
  private currentTurnId: string | null = null;
// Whether the current runtime turn is created by model-only TurnStarted (manual /compact,
// goal continuation and other maintenance turns). The pending placement of the SessionStart digest must not be maintained
// turn is the closing target and must wait for the next user-visible real turn.
  private currentTurnStartedModelOnly = false;
// When contextWindow=null, the protocol does not expose the denominator and usage, but the reducer still needs to retain the latest context usage.
// So that when the registry subsequently restores usage to a known capacity, it will be rebuilt atomically instead of zeroing out on error.
  private contextWindowState: ContextWindowProjectionState = {
    maxTokens: null,
    touchedByEvent: false,
    usedTokens: 0,
  };
// modelChange marker is generated "at the beginning of the next turn".
// silentInitial keeps the first round of normal Main silent; sourceLess means explicit ∅→X; known saves the previous round
//The actual provider/model used. thought is only recorded with the baseline and does not trigger model identity changes.
  private lastTurnModel: TurnModelBaseline = { kind: "silentInitial" };
// Seed guard: config blocks touched by events (authoritative logs) no longer accept seed coverage.
  private configModelTouchedByEvent = false;
// The old ModelSelected does not contain capability sets; independent guards allow runtime seed to fill in old logs.
// This also prevents subsequent seeds from covering the model capabilities of atomically released new events.
  private configThoughtLevelsTouchedByEvent = false;
  private configModeTouchedByEvent = false;
// assistant conservation: count of text streams rejected during non-runtime (gateway sets stale accordingly).
  private droppedContentStreamEventCount = 0;
// The legacy fallback must be observable during the reading period; otherwise, the normalizer will still degrade to "visible but not addressable" after missing fields.
  private normalizationDiagnostics: ConversationNormalizationDiagnostic[] = [];

  constructor(sessionId: string, logEpoch: string) {
    this.snapshot = createInitialConversationSnapshot(sessionId, logEpoch);
  }

  getSnapshot(): ConversationSnapshot {
    return this.snapshot;
  }

/** assistant conservation: the number of rejected text stream events (>0 = the projection may be missing segments and needs to be rehydrated). */
  getDroppedContentStreamEventCount(): number {
    return this.droppedContentStreamEventCount;
  }

  getNormalizationDiagnostics(): readonly ConversationNormalizationDiagnostic[] {
    return this.normalizationDiagnostics;
  }

/** Bounded incremental estimation for publisher only; returning null indicates that candidate snapshots must be accurately verified. */
  establishedStreamingAppend(event: SessionEvent): string | null {
    if (event.type !== SessionEventType.ModelStreaming || !this.isRunning()) return null;
    const payload = event.payload as ModelStreamingPayload;
    if (payload.kind === "text_delta" && this.streamingTextRowId !== null) return payload.delta;
    if (payload.kind === "reasoning_delta" && this.streamingReasoningRowId !== null) {
      return payload.delta;
    }
    if (
      payload.kind === "tool_input_delta" &&
      this.toolRowIdByCallId.has(String(payload.toolCallId))
    ) {
      const state = this.fileToolInputPreviewByCallId.get(String(payload.toolCallId));
      if (state) {
        if (
          state.lastPublishedAt !== null &&
          this.ms(event) - state.lastPublishedAt <
            ZCODE_FILE_STREAMING_TOOL_INPUT_PREVIEW_MIN_INTERVAL_MS
        ) {
          return "";
        }
// The upper bound estimate must include the cumulative suffix within the window; only calculating the current delta will underestimate the next wire snapshot.
        return `${state.pendingAppend}${payload.delta}`;
      }
      return payload.delta;
    }
    return null;
  }

  /**
* config seed injection.
   *
* The initial snapshot config was written to be empty provider/model +
* mode="build", and ModelSelected is only reissued after switchModelConfig, and SessionCreated is deliberately
* Does not produce delta - runtime true value (start default model/project persistence mode/historical session last selection) from beginning to end
*Cannot enter the projection. Consequences: ① The new session model selector is displayed empty; ② The UI is displayed when project persistence mode=yolo
* Build, click yolo and hit the handler with the same value no-op (the true value of runtime is judged), the UI can never converge——
* Breaks the CAS invariant of "revision unchanged ⇔ no state change".
   *
* Why this revision: the seed changes snapshot.config directly without generating delta or incrementing revision/seq——
* The draft "no visible delta" ruling is not destroyed; blocks touched by the event are skipped (replay sequence
* Log value takes precedence when following the seed). Idempotent: Can be called repeatedly after ensurePublisher / hydration.
   */
  seedConfig(seed: SessionConfigSeed): void {
    const config = { ...this.snapshot.config };
    let changed = false;
    if (!config.permissionGrant && seed.permissionGrant) {
      config.permissionGrant = seed.permissionGrant;
      changed = true;
    }
    if (!this.configModelTouchedByEvent) {
      if (
        Object.hasOwn(seed, "modelSelection") &&
        !sameSparseModelSelection(config.modelSelection, seed.modelSelection)
      ) {
// The restored empty selection also has clear semantics, and the old selection in the historical event cannot be retained because of falsy.
        config.modelSelection = seed.modelSelection
          ? cloneSparseModelSelection(seed.modelSelection)
          : undefined;
        changed = true;
      }
      if (seed.provider !== undefined && config.provider !== seed.provider) {
        config.provider = seed.provider;
        changed = true;
      }
      if (seed.model !== undefined && config.model !== seed.model) {
        config.model = seed.model;
        changed = true;
      }
      if (seed.thought !== undefined && config.thought !== seed.thought) {
        config.thought = seed.thought;
        changed = true;
      }
    }
    const seedThoughtLevels = seed.thoughtLevels;
    if (
      !this.configThoughtLevelsTouchedByEvent &&
      seedThoughtLevels !== undefined &&
      (config.thoughtLevels.length !== seedThoughtLevels.length ||
        config.thoughtLevels.some((value, index) => value !== seedThoughtLevels[index]))
    ) {
      config.thoughtLevels = [...seedThoughtLevels];
      changed = true;
    }
    if (!this.configModeTouchedByEvent && seed.mode && config.mode !== seed.mode) {
      config.mode = seed.mode;
      changed = true;
    }
    if (
      !this.configModeTouchedByEvent &&
      seed.planEnabled !== undefined &&
      config.planEnabled !== seed.planEnabled
    ) {
      config.planEnabled = seed.planEnabled;
      changed = true;
    }
    if (changed) {
      this.snapshot = { ...this.snapshot, config };
    }
  }

  /**
* Import the source read-only seed of the sharing context.
   *
* shared_context is a provider-only message and should not be materialized as a user bubble; the source tag passes
* The snapshot additive field is delivered for Desktop to display a persistent prompt after opening a new session. This field
* Does not belong to conversation rows, nor does it increment revision/seq, to ​​avoid forging a conversation.
   */
  seedSharedContextImport(
    source: ConversationSnapshot["sharedContextImport"] | null | undefined,
  ): void {
    const title = source?.title.trim();
    if (!title) return;
    if (
      this.snapshot.sharedContextImport?.title === title &&
      (source as { contextId?: string }).contextId ===
        (this.snapshot.sharedContextImport as { contextId?: string }).contextId &&
      (source as { status?: string }).status ===
        (this.snapshot.sharedContextImport as { status?: string }).status
    ) {
      return;
    }
    this.snapshot = {
      ...this.snapshot,
      sharedContextImport: {
        ...source,
        title,
      },
    };
  }

  seedUsage(seed: SessionUsageSeed): void {
    const current = this.snapshot.usage;
    const currentContextWindow = current.contextWindow;
    if (currentContextWindow) {
      this.contextWindowState.usedTokens = currentContextWindow.usedTokens;
    }
// Unknown capacities also have internal usage facts; late recovery seeds cannot cover the true ModelComplete/Compact water level.
    if (this.contextWindowState.usedTokens > 0) {
      return;
    }
    const seededContextWindow = seed.contextWindow;
    if (!Number.isFinite(seededContextWindow.usedTokens) || seededContextWindow.usedTokens <= 0) {
      return;
    }
    const cumulative = {
      inputTokens: seed.cumulative?.inputTokens ?? current.cumulative.inputTokens,
      outputTokens: seed.cumulative?.outputTokens ?? current.cumulative.outputTokens,
      cacheReadTokens: seed.cumulative?.cacheReadTokens ?? current.cumulative.cacheReadTokens,
      cacheWriteTokens: seed.cumulative?.cacheWriteTokens ?? current.cumulative.cacheWriteTokens,
    };
    if (this.contextWindowState.touchedByEvent) {
// Similar guards: explicit ModelSelected.contextWindow (including null) is the log authoritative capacity,
// Hydration seed can only make up for more accurate token facts, and must not overwrite maxTokens or redisplay null.
      this.contextWindowState.usedTokens = seededContextWindow.usedTokens;
      this.snapshot = {
        ...this.snapshot,
        usage: {
          contextWindow: currentContextWindow
            ? {
                ...currentContextWindow,
                usedTokens: seededContextWindow.usedTokens,
              }
            : null,
          cumulative,
        },
      };
      return;
    }
    if (
      seededContextWindow.maxTokens !== null &&
      (!Number.isFinite(seededContextWindow.maxTokens) || seededContextWindow.maxTokens <= 0)
    ) {
      return;
    }

// Synthetic historical events only have zero usage space; the seed fills the real water level, and the unknown capacity does not prevent the internal retention of tokens.
    this.contextWindowState.usedTokens = seededContextWindow.usedTokens;
    this.contextWindowState.maxTokens = seededContextWindow.maxTokens;
    this.snapshot = {
      ...this.snapshot,
      usage: {
        contextWindow:
          seededContextWindow.maxTokens === null
            ? null
            : { ...seededContextWindow, maxTokens: seededContextWindow.maxTokens },
        cumulative,
      },
    };
  }

  /**
* Cold recovery subagent store verification seed. transcript can restore visible rows, but only session
* store can prove that child has been persisted as subagent_child; therefore, before candidate publisher publishes
* Replace the manifest with this seed as a whole. The ghost children left over from the old version are not allowed to enter the UI authoritative state.
   */
  seedSubagents(seed: SessionSubagentsSeed): void {
    const childSessionIds = [...new Set(seed.childSessionIds)];
    const allowed = new Set(childSessionIds);
    this.invalidSubagentChildSessionIds = new Set(
      this.snapshot.rows.window.flatMap((row) =>
        row.kind === "subagent" && row.childSessionId && !allowed.has(row.childSessionId)
          ? [row.childSessionId]
          : [],
      ),
    );
    const running = seed.running.filter((item) => allowed.has(item.childSessionId));
    this.snapshot = {
      ...this.snapshot,
      subagents: {
// The non-empty cold manifest must start at least 1; the renderer uses 0 to distinguish that the authoritative state has not yet been established.
// Otherwise, stateRevision=0 of the old session will cause the child tab invalidation synchronization to be permanently skipped.
        revision: Math.max(childSessionIds.length > 0 ? 1 : 0, Math.floor(seed.revision)),
        childSessionIds,
        running,
        endedTotal: Math.max(0, childSessionIds.length - running.length),
      },
    };
  }

  /**
* rowId → authoritative messageId. When the bridge layer executes forkAssistant/editUserQuery, the command load
* Internal rowId is translated into messageId required by core. Unknown rowId (non-assistant/user row,
* or late) returns null, and the bridge layer returns rejected accordingly.
   */
  getMessageIdForRow(rowId: number): string | null {
    return this.messageIdByRowId.get(rowId) ?? null;
  }

  getEntityIdForRow(rowId: number): string | null {
    return this.entityIdByRowId.get(rowId) ?? null;
  }

  resolveEditTarget(rowId: number): ConversationEditTarget | null {
    if (!this.isLatestEditableUserRow(rowId)) return null;
    const entityId = this.entityIdByRowId.get(rowId);
    return entityId ? this.resolveEditTargetByEntityId(entityId) : null;
  }

  resolveEditTargetByEntityId(entityId: string): ConversationEditTarget | null {
    if (entityId !== this.currentEditableEntityId) return null;
    const target = this.editTargetByEntityId.get(entityId);
    return target ? { ...target, intent: { ...target.intent } } : null;
  }

  /**
* The only parser for V3 actions. Display rowId and stable entityId must hit the current projection at the same time;
* Action availability directly reads row.actions, handler/preview generated by the same materialization
* No longer recalculate individually by position, phase or text.
   */
  resolveRowActionTarget(
    target: ConversationRowTarget,
    action: ConversationRowTargetAction,
  ): ConversationRowTargetResolution {
    const row = this.findRow(target.rowId);
    if (!row || this.entityIdByRowId.get(target.rowId) !== target.entityId) {
      return { ok: false, status: "stale", reasonCode: "proto.staleTarget" };
    }
    if (action === "editUserQuery") {
      const editTarget = this.resolveEditTargetByEntityId(target.entityId);
      if (row.actions?.canEdit !== true || !row.actions.editDisposition || !editTarget) {
        return {
          ok: false,
          status: "rejected",
          reasonCode: "guard.actionUnavailable",
        };
      }
      return { ok: true, action, row, editTarget };
    }
    if (action === "retryTurn") {
      const messageId = this.messageIdByRowId.get(row.rowId);
      const userRow = this.snapshot.rows.window.find(
        (candidate) =>
          candidate.turnId === row.turnId &&
          candidate.kind === "userInput" &&
          candidate.origin === "realUser",
      );
      const userEntityId = userRow ? this.entityIdByRowId.get(userRow.rowId) : undefined;
      const editTarget = userEntityId ? this.editTargetByEntityId.get(userEntityId) : undefined;
      if (row.actions?.canRetry !== true || !messageId || !editTarget) {
        return {
          ok: false,
          status: "rejected",
          reasonCode: "guard.actionUnavailable",
        };
      }
      return { ok: true, action, row, messageId, editTarget };
    }
    if (action === "forkAssistant") {
      const messageId = this.messageIdByRowId.get(row.rowId);
      if (row.actions?.canFork !== true || !messageId) {
        return {
          ok: false,
          status: "rejected",
          reasonCode: "guard.actionUnavailable",
        };
      }
      return { ok: true, action, row, messageId };
    }
    if (action === "setAssistantFeedback") {
      const messageId = this.messageIdByRowId.get(row.rowId);
      if (row.kind !== "assistantText" || !messageId) {
        return {
          ok: false,
          status: "rejected",
          reasonCode: "guard.actionUnavailable",
        };
      }
      return { ok: true, action, row, messageId };
    }
    if (row.kind !== "turnHeader") {
      return {
        ok: false,
        status: "rejected",
        reasonCode: "guard.actionUnavailable",
      };
    }
    if (
      (action === "applyFileRewind" || action === "fileRewindPreview") &&
      (!row.fileChanges || row.actions?.canRewindFiles !== true)
    ) {
      return {
        ok: false,
        status: "rejected",
        reasonCode: "guard.actionUnavailable",
      };
    }
    return {
      ok: true,
      action,
      row,
      messageIds: this.getMessageIdsForTurnRow(row.rowId),
    };
  }

  /**
* File summary revocation uses turn rowId as the entry point, and the server parses all files in the same product turn
* messageId, covering multiple assistants/multiple checkpoints; the UI does not expose the internal messageId.
   */
  getMessageIdsForTurnRow(rowId: number): string[] {
    const row = this.findRow(rowId);
    if (!row) return [];
    const messageIds = new Set<string>();
    const runtimeTurnId = this.runtimeTurnIdByProductTurnId.get(row.turnId);
// Bug reason: model-only turn does not generate visible userInput rows. In the past, only scanning rows would miss them.
// Hidden user messageId used by checkpoint. New turn when there is a persistent message productTurnId
// This is the messageId; after using it as a precise anchor point, there is no need to expand the scope of the runtime turn.
    if (runtimeTurnId && runtimeTurnId !== row.turnId) {
      messageIds.add(row.turnId);
    }
    for (const candidate of this.snapshot.rows.window) {
      if (candidate.turnId !== row.turnId) continue;
      const messageId = this.messageIdByRowId.get(candidate.rowId);
      if (messageId) messageIds.add(messageId);
    }
    for (const [messageId, continuationRowId] of this.outputContinuationRowIdByMessageId) {
      const continuationRow = this.findRow(continuationRowId);
      if (continuationRow?.turnId === row.turnId) messageIds.add(messageId);
    }
    return [...messageIds];
  }

  /**
* Core side strong verification:
* Whether rowId is the last assistantText of the productTurn it belongs to. UI (after tiling) has only been
* The last section exposes the fork entrance, which is the defense gate - the direct command surface/old client is not allowed to fork the middle section.
   */
  isLatestAssistantSegmentRow(rowId: number): boolean {
    const row = this.findRow(rowId);
    if (row?.kind !== "assistantText") return false;
    for (let index = this.snapshot.rows.window.length - 1; index >= 0; index -= 1) {
      const candidate = this.snapshot.rows.window[index]!;
      if (candidate.kind === "assistantText" && candidate.turnId === row.turnId) {
        return candidate.rowId === rowId;
      }
    }
    return false;
  }

  /**
* Synchronous projection gate for running fork: only row/product-turn and message boundaries are parsed here; complete
* orderedMessageIds are sequentially completed and persisted by the host using session store authoritative anchor.
   */
  resolveStableForkCandidate(rowId: number): StableForkCandidateResolution {
    if (this.snapshot.control.activeWorks.some((work) => work.kind === "compact")) {
      return { ok: false, reasonCode: "guard.compactOperationLock" };
    }
    const row = this.findRow(rowId);
    if (row?.kind !== "assistantText") {
      return { ok: false, reasonCode: "guard.forkAssistantOnly" };
    }
    const headerRowId = this.turnHeaderRowIdByTurnId.get(row.turnId);
    const header = headerRowId === undefined ? undefined : this.findRow(headerRowId);
    if (
      row.state !== "complete" ||
      row.actions?.canFork !== true ||
      header?.kind !== "turnHeader" ||
      header.state !== "completedSuccess" ||
      !this.isLatestAssistantSegmentRow(rowId)
    ) {
      return { ok: false, reasonCode: "guard.forkTargetNotStable" };
    }
    const boundaryMessageId = this.messageIdByRowId.get(rowId);
    if (!boundaryMessageId) {
      return { ok: false, reasonCode: "guard.forkTargetAmbiguous" };
    }
    const startMessageId =
      this.snapshot.rows.window
        .filter((candidate) => candidate.turnId === row.turnId && candidate.kind === "userInput")
        .map((candidate) => this.messageIdByRowId.get(candidate.rowId))
        .find((messageId): messageId is string => Boolean(messageId)) ?? null;
    return {
      ok: true,
      candidate: {
        productTurnId: row.turnId,
        transcriptTurnId: this.runtimeTurnIdByProductTurnId.get(row.turnId) ?? row.turnId,
        startMessageId,
        boundaryMessageId,
      },
    };
  }

/** latestAssistantRetryOnly: retry can only point to the latest assistantText in the entire timeline and with realUser cause. */
  isLatestRetryAssistantRow(rowId: number): boolean {
    const row = this.findRow(rowId);
    return Boolean(
      row?.kind === "assistantText" &&
      row.actions?.canRetry === true &&
      this.messageIdByRowId.has(rowId),
    );
  }

/** latestQueryEditOnly: Only the last realUser userInput row in the current projection can be edited. */
  isLatestEditableUserRow(rowId: number): boolean {
    const row = this.findRow(rowId);
    return Boolean(
      row?.kind === "userInput" &&
      row.origin === "realUser" &&
      row.actions?.canEdit === true &&
      this.messageIdByRowId.has(rowId),
    );
  }

/** rowId → product turnId (command layer running edit is used to check the store when there is no assistant anchor). */
  getTurnIdForRow(rowId: number): string | null {
    return this.findRow(rowId)?.turnId ?? null;
  }

/** Applies an authoritative event and returns the (possibly empty) delta sequence produced by the event. */
  applyEvent(event: SessionEvent): ConversationDelta[] {
    return this.applyEventInternal(event, true);
  }

  /**
* Cold recovery batch paths are only allowed on candidate projections that have not yet been released. begin after rows.window
* Advance in place to avoid copying and growing the array for each event; the publisher will not adopt the candidate before the complete verification is passed.
   */
  beginHydrationReplay(): void {
    if (this.hydrationAccumulator) throw new Error("hydration replay already active");
    this.hydrationAccumulator = createMutableConversationSnapshotAccumulator(this.snapshot);
    this.snapshot = this.hydrationAccumulator.snapshot;
    this.rowIndexById = this.hydrationAccumulator.rowIndexById;
  }

  applyHydrationEvent(event: SessionEvent): ConversationDelta[] {
    if (!this.hydrationAccumulator) throw new Error("hydration replay is not active");
    return this.applyEventInternal(event, false);
  }

  /**
* Convergence delayed command actions during the batch to the current snapshot. actions are derived from the same reducer
* materialization, does not increment revision separately; the structure/guard event that triggered its change has already been accounted for.
   */
  completeHydrationReplay(): ConversationDelta[] {
    if (!this.hydrationAccumulator) throw new Error("hydration replay is not active");
    const deltas = this.materializeCommandRowActions([]);
    applyConversationDeltasMutable(this.hydrationAccumulator, deltas);
    this.hydrationAccumulator = null;
    return deltas;
  }

  private applyEventInternal(
    event: SessionEvent,
    materializeActions: boolean,
  ): ConversationDelta[] {
    if (event.type === SessionEventType.SubagentSpawned) {
      const childSessionId = this.stringPayload(
        event.payload as Record<string, unknown>,
        "childSessionId",
      );
// The live spawn has passed the core persist-before-publish gate; if it is a legal resume of the old ghost,
//Restore qualification with new event. During the hydration period, the seed has not yet established an exclusion set, and historical references will not be misplaced.
      if (childSessionId) this.invalidSubagentChildSessionIds.delete(childSessionId);
    }
    const runtimeTurnId = String(event.turnId ?? this.currentTurnId ?? "turn-unknown");
    const productTurnId =
      event.type === SessionEventType.TurnStarted
        ? undefined
        : (this.productTurnIdByRuntimeTurnId.get(runtimeTurnId) ?? runtimeTurnId);
    const reduced =
      event.type === SessionEventType.AssistantFeedbackUpdated
        ? this.onAssistantFeedbackUpdated(event)
        : (() => {
            const fact = normalizeConversationEvent(event, {
              productTurnId,
              openAssistantSegments: this.openAssistantSegments(),
            });
            this.normalizationDiagnostics.push(...fact.diagnostics);
            return this.reduce(fact);
          })();
    const subagentDeltas = this.shouldMaterializeSubagentProjection(reduced)
      ? this.materializeSubagentProjection(reduced)
      : [];
    const reducedWithSubagents = [...reduced, ...subagentDeltas];
// row, command target and actions must belong to the same materialization transaction.
// The old implementation only maintains side-map/latest row judgment, UI action is inferred from elsewhere, cold/tool-only/failed
// The wheel will appear "The entrance is visible but the target cannot be resolved", and the old entrance will not be revoked after the new target appears.
    //
// Pure row.delta frame (text append) does not change any input of actions: the whole window materialization will only hit all
// "unchanged". Skipping it causes the frame-by-frame cost to be related to the rows that change in this frame, rather than growing with the total number of rows in the session.
    const shouldMaterializeActions =
      materializeActions && deltasMayAffectCommandRowActions(reducedWithSubagents);
    const deltas = shouldMaterializeActions
      ? [...reducedWithSubagents, ...this.materializeCommandRowActions(reducedWithSubagents)]
      : reducedWithSubagents;
    const finalDeltas = this.attachRevision(clearSettledOutputPreviews(deltas));
    if (this.hydrationAccumulator) {
      applyConversationDeltasMutable(this.hydrationAccumulator, finalDeltas);
      this.snapshot.seq = event.sequenceNumber;
    } else {
      const previousRowsLength = this.snapshot.rows.window.length;
      this.snapshot = {
        ...applyConversationDeltas(this.snapshot, finalDeltas),
        seq: event.sequenceNumber,
      };
      this.updateRowIndexAfterImmutableApply(previousRowsLength, finalDeltas);
    }
    this.updateToolIndexesAfterDeltas(finalDeltas);
    return finalDeltas;
  }

  /**
* Reduce events on independent candidate projections and submit them atomically only after passing the verification.
   *
* When projection exceeds the upper limit of logical frame assembly, if you modify the current instance first and then wait
* The wire encoder reports an error, and the authoritative memory state will permanently stop in the "unable to send snapshot" state. Candidate instances simultaneously
* Isolate various side-maps of snapshot and reducer; the current instance is completely unchanged when rejected, and the client can still access it from
* The last transportable snapshot is restored.
   */
  applyEventAtomically(
    event: SessionEvent,
    accept: (snapshot: ConversationSnapshot) => boolean,
  ): ConversationDelta[] | null {
    const candidate = this.cloneProjection();
    const deltas = candidate.applyEvent(event);
    if (!accept(candidate.snapshot)) return null;
    this.adoptProjection(candidate);
    return deltas;
  }

  private cloneProjection(): ProductProjection {
    const clone = Object.create(ProductProjection.prototype) as ProductProjection;
    clone.snapshot = this.snapshot;
    clone.rowIndexById = new Map(this.rowIndexById);
    clone.hydrationAccumulator = null;
    clone.nextRowId = this.nextRowId;
    clone.streamingTextRowId = this.streamingTextRowId;
    clone.streamingReasoningRowId = this.streamingReasoningRowId;
    clone.outputContinuationTextRowId = this.outputContinuationTextRowId;
    clone.toolRowIdByCallId = new Map(this.toolRowIdByCallId);
// Publish event-by-event atomic clone in real time; missing this side table will cause the successful list_apps snapshot to be lost during submission.
    clone.latestListAppsSnapshot = new Map(this.latestListAppsSnapshot);
    clone.openForegroundToolCallIds = new Set(this.openForegroundToolCallIds);
    clone.fileToolInputPreviewByCallId = new Map(
      [...this.fileToolInputPreviewByCallId].map(([toolCallId, state]) => [
        toolCallId,
        { ...state },
      ]),
    );
    clone.subagentRowIdByAgentId = new Map(this.subagentRowIdByAgentId);
    clone.hookRowIdByInvocationId = new Map(this.hookRowIdByInvocationId);
    clone.pendingSessionHookInvocations = new Map(
      [...this.pendingSessionHookInvocations].map(([invocationId, pending]) => [
        invocationId,
        {
          firstEvent: pending.firstEvent,
          content: {
            ...pending.content,
            executions: pending.content.executions.map((execution) => ({ ...execution })),
          },
        },
      ]),
    );
    clone.rewoundHookInvocationIds = new Set(this.rewoundHookInvocationIds);
    clone.invalidSubagentChildSessionIds = new Set(this.invalidSubagentChildSessionIds);
    clone.messageIdByRowId = new Map(this.messageIdByRowId);
    clone.outputContinuationRowIdByMessageId = new Map(this.outputContinuationRowIdByMessageId);
    clone.entityIdByRowId = new Map(this.entityIdByRowId);
    clone.editTargetByEntityId = new Map(this.editTargetByEntityId);
    clone.currentEditableEntityId = this.currentEditableEntityId;
    clone.stableCompactCoverageBoundaryRowId = this.stableCompactCoverageBoundaryRowId;
    clone.turnHeaderRowIdByTurnId = new Map(this.turnHeaderRowIdByTurnId);
    clone.compactMarkerRowIdByOperationId = new Map(this.compactMarkerRowIdByOperationId);
    clone.goalVerifyMarkerRowIdByLifecycleKey = new Map(this.goalVerifyMarkerRowIdByLifecycleKey);
    clone.productTurnIdByRuntimeTurnId = new Map(this.productTurnIdByRuntimeTurnId);
    clone.runtimeTurnIdByProductTurnId = new Map(this.runtimeTurnIdByProductTurnId);
    clone.productTurnSplitOrdinalByRuntimeTurnId = new Map(
      this.productTurnSplitOrdinalByRuntimeTurnId,
    );
    clone.currentProductTurnStartedAtMs = this.currentProductTurnStartedAtMs;
    clone.deliveryByPendingInputId = new Map(this.deliveryByPendingInputId);
    clone.currentTurnId = this.currentTurnId;
    clone.currentTurnStartedModelOnly = this.currentTurnStartedModelOnly;
    clone.contextWindowState = { ...this.contextWindowState };
    clone.lastTurnModel = { ...this.lastTurnModel };
    clone.configModelTouchedByEvent = this.configModelTouchedByEvent;
    clone.configThoughtLevelsTouchedByEvent = this.configThoughtLevelsTouchedByEvent;
    clone.configModeTouchedByEvent = this.configModeTouchedByEvent;
    clone.droppedContentStreamEventCount = this.droppedContentStreamEventCount;
    clone.normalizationDiagnostics = [...this.normalizationDiagnostics];
    return clone;
  }

  private adoptProjection(candidate: ProductProjection): void {
    this.snapshot = candidate.snapshot;
    this.rowIndexById = candidate.rowIndexById;
    this.hydrationAccumulator = null;
    this.nextRowId = candidate.nextRowId;
    this.streamingTextRowId = candidate.streamingTextRowId;
    this.streamingReasoningRowId = candidate.streamingReasoningRowId;
    this.outputContinuationTextRowId = candidate.outputContinuationTextRowId;
    this.toolRowIdByCallId = candidate.toolRowIdByCallId;
    this.latestListAppsSnapshot = candidate.latestListAppsSnapshot;
    this.openForegroundToolCallIds = candidate.openForegroundToolCallIds;
    this.fileToolInputPreviewByCallId = candidate.fileToolInputPreviewByCallId;
    this.subagentRowIdByAgentId = candidate.subagentRowIdByAgentId;
    this.hookRowIdByInvocationId = candidate.hookRowIdByInvocationId;
    this.pendingSessionHookInvocations = candidate.pendingSessionHookInvocations;
    this.rewoundHookInvocationIds = candidate.rewoundHookInvocationIds;
    this.invalidSubagentChildSessionIds = candidate.invalidSubagentChildSessionIds;
    this.messageIdByRowId = candidate.messageIdByRowId;
    this.outputContinuationRowIdByMessageId = candidate.outputContinuationRowIdByMessageId;
    this.entityIdByRowId = candidate.entityIdByRowId;
    this.editTargetByEntityId = candidate.editTargetByEntityId;
    this.currentEditableEntityId = candidate.currentEditableEntityId;
    this.stableCompactCoverageBoundaryRowId = candidate.stableCompactCoverageBoundaryRowId;
    this.turnHeaderRowIdByTurnId = candidate.turnHeaderRowIdByTurnId;
    this.compactMarkerRowIdByOperationId = candidate.compactMarkerRowIdByOperationId;
    this.goalVerifyMarkerRowIdByLifecycleKey = candidate.goalVerifyMarkerRowIdByLifecycleKey;
    this.productTurnIdByRuntimeTurnId = candidate.productTurnIdByRuntimeTurnId;
    this.runtimeTurnIdByProductTurnId = candidate.runtimeTurnIdByProductTurnId;
    this.productTurnSplitOrdinalByRuntimeTurnId = candidate.productTurnSplitOrdinalByRuntimeTurnId;
    this.currentProductTurnStartedAtMs = candidate.currentProductTurnStartedAtMs;
    this.deliveryByPendingInputId = candidate.deliveryByPendingInputId;
    this.currentTurnId = candidate.currentTurnId;
    this.currentTurnStartedModelOnly = candidate.currentTurnStartedModelOnly;
    this.contextWindowState = candidate.contextWindowState;
    this.lastTurnModel = candidate.lastTurnModel;
    this.configModelTouchedByEvent = candidate.configModelTouchedByEvent;
    this.configThoughtLevelsTouchedByEvent = candidate.configThoughtLevelsTouchedByEvent;
    this.configModeTouchedByEvent = candidate.configModeTouchedByEvent;
    this.droppedContentStreamEventCount = candidate.droppedContentStreamEventCount;
    this.normalizationDiagnostics = candidate.normalizationDiagnostics;
  }

  /**
* Generate edit/retry actions based on the prospective rows atoms after reduction of this event.
* action=true must contain a persistent message target that can be parsed under the same revision as the command layer; the latest target
* When changing, upsert the old and new lines at the same time. The client does not need to make inferences based on the array position.
   */
  private materializeCommandRowActions(reduced: ConversationDelta[]): ConversationDelta[] {
    const prospective = applyConversationDeltas(this.snapshot, reduced);
    const rows = prospective.rows.window;
    const rowById = new Map(rows.map((row) => [row.rowId, row]));
    const latestAssistantRowIdByTurn = new Map<string, number>();
    for (const row of rows) {
      if (row.kind !== "assistantText") continue;
      const current = latestAssistantRowIdByTurn.get(row.turnId);
      if (current === undefined || row.rowId > current) {
        latestAssistantRowIdByTurn.set(row.turnId, row.rowId);
      }
    }
    const compactActive = prospective.control.activeWorks.some((work) => work.kind === "compact");
    const completionBlockingActive = prospective.control.activeWorks.length > 0;
    let latestEditable: ConversationRow | undefined;
    let latestAssistant: AssistantTextRow | undefined;
    for (let index = rows.length - 1; index >= 0; index -= 1) {
      const row = rows[index]!;
      if (
        !latestEditable &&
        !compactActive &&
        row.kind === "userInput" &&
        row.origin === "realUser"
      ) {
        latestEditable = row;
      }
      if (!latestAssistant && row.kind === "assistantText") {
        latestAssistant = row;
      }
      if (latestEditable && latestAssistant) break;
    }
// The old logic only selects retry and background result based on "latest complete assistant"
// synthetic turn will therefore get the wrong entrance; if you only filter synthetic in the find condition, it will be skipped
//The latest background assistant resurrects the retry of the earlier real user round. The entire timeline must be locked here first
// The latest assistant, recheck the realUser canonical cause in the same round to ensure that ordinary retry will not roll back across rounds.
    const latestRetryable = (() => {
      if (
        completionBlockingActive ||
        !latestAssistant ||
        latestAssistant.state !== "complete" ||
        !this.messageIdByRowId.has(latestAssistant.rowId)
      ) {
        return undefined;
      }
      const headerId = this.turnHeaderRowIdByTurnId.get(latestAssistant.turnId);
      const header = headerId === undefined ? undefined : rowById.get(headerId);
      if (header?.kind !== "turnHeader" || header.state === "running") return undefined;
      const canonicalUserRow = rows.find(
        (row) =>
          row.turnId === latestAssistant.turnId &&
          row.kind === "userInput" &&
          row.origin === "realUser",
      );
      const canonicalUserEntityId = canonicalUserRow
        ? this.entityIdByRowId.get(canonicalUserRow.rowId)
        : undefined;
      if (!canonicalUserEntityId || !this.editTargetByEntityId.has(canonicalUserEntityId)) {
        return undefined;
      }
      return latestAssistant;
    })();
    const latestEditableEntityId =
      latestEditable === undefined
        ? null
        : (this.entityIdByRowId.get(latestEditable.rowId) ?? null);
// edit action and command resolver must share the canonical target authority. In the past the drain branch only
// Register the messageId, so the UI displays Edit, but the submission must be rejected by the resolver as actionUnavailable.
    const latestEditableRowId =
      latestEditable &&
      latestEditableEntityId &&
      this.messageIdByRowId.has(latestEditable.rowId) &&
      this.editTargetByEntityId.has(latestEditableEntityId)
        ? latestEditable.rowId
        : null;
// The entity target history table will retain old records; simply revoking the row action is not enough to prevent
// Direct checking of entityId bypasses latest-only semantics. The currently editable authority and actions are at the same time
//Updated in materialization, the resolver no longer traverses rows and does not treat rowId as a canonical key.
    this.currentEditableEntityId = latestEditableRowId === null ? null : latestEditableEntityId;
    const latestRetryableRowId = latestRetryable?.rowId ?? null;
    const deltas: ConversationDelta[] = [];

    for (const row of rows) {
      if (row.kind !== "turnHeader" && row.kind !== "userInput" && row.kind !== "assistantText")
        continue;
      const nextActions = { ...row.actions };
      if (row.kind === "turnHeader") {
        const canRewindFiles =
          !completionBlockingActive &&
          prospective.pendingInteractions.length === 0 &&
          row.state !== "running" &&
          row.fileChanges?.state === "active";
        if (canRewindFiles) nextActions.canRewindFiles = true;
        else delete nextActions.canRewindFiles;
      } else if (row.kind === "userInput") {
        if (row.rowId === latestEditableRowId) {
          nextActions.canEdit = true;
          nextActions.editDisposition = "rewind";
        } else {
          delete nextActions.canEdit;
          delete nextActions.editDisposition;
        }
      } else {
        if (row.rowId === latestRetryableRowId) nextActions.canRetry = true;
        else delete nextActions.canRetry;
        const headerId = this.turnHeaderRowIdByTurnId.get(row.turnId);
        const header = headerId === undefined ? undefined : rowById.get(headerId);
        const canFork =
          !compactActive &&
          row.state === "complete" &&
          header?.kind === "turnHeader" &&
          header.state === "completedSuccess" &&
          latestAssistantRowIdByTurn.get(row.turnId) === row.rowId &&
          this.messageIdByRowId.has(row.rowId);
        if (canFork) nextActions.canFork = true;
        else delete nextActions.canFork;
      }
      const actions = Object.keys(nextActions).length > 0 ? nextActions : undefined;
      if (conversationRowActionsEqual(actions, row.actions)) continue;
      const nextRow: ConversationRow = { ...row, actions };
      if (!actions) delete nextRow.actions;
      deltas.push({ op: "row.upserted", row: nextRow });
    }
    return deltas;
  }

// revision progression: this event contains any structural delta → revision +1,
// And the carrying rules require that deltas must contain state.updated.revision.
  private attachRevision(deltas: ConversationDelta[]): ConversationDelta[] {
    if (!deltas.some(deltaBumpsRevision)) return deltas;
    const revision = this.snapshot.revision + 1;
    const last = deltas[deltas.length - 1];
    if (last?.op === "state.updated") {
      return [...deltas.slice(0, -1), { op: "state.updated", patch: { ...last.patch, revision } }];
    }
    return [...deltas, { op: "state.updated", patch: { revision } }];
  }

  private reduce(fact: CanonicalConversationFact): ConversationDelta[] {
    const event = fact.event;
    switch (event.type) {
      case SessionEventType.SessionCreated:
        return this.onSessionCreated(event);
      case SessionEventType.SessionResumed:
        return this.onSessionResumed(event);
      case SessionEventType.SessionTitleUpdated:
        return this.onSessionTitleUpdated(event);
      case SessionEventType.TurnStarted:
        if (fact.semanticKind !== "userIntent") return [];
        return [
          ...this.onTurnStarted(fact),
// model-only maintenance turns (manual /compact, goal continuation) are not eligible
// Carrying the SessionStart summary; pending remains until the next user-visible real turn.
          ...(this.currentTurnStartedModelOnly
            ? []
            : this.flushPendingSessionHookInvocations(fact.productTurnId)),
        ];
      case SessionEventType.ModelStreaming: {
        if (fact.semanticKind !== "assistantSegment") return [];
        const shouldClearApiRetry =
          this.acceptsActiveModelEvent(event) &&
          isZCodeModelRetryRecoveryProgressPayload(
            event.payload as unknown as Record<string, unknown>,
          );
        const streamingDeltas = this.onModelStreaming(fact);
        return shouldClearApiRetry
          ? [...streamingDeltas, ...this.setApiRetry(null)]
          : streamingDeltas;
      }
      case SessionEventType.ModelNetworkStatus:
        return this.onModelNetworkStatus(event);
      case SessionEventType.StreamRecoveryStarted:
        return this.onStreamRecoveryStarted(event);
      case SessionEventType.StreamRecoveryTailDiscarded:
        return this.onStreamRecoveryTailDiscarded(event);
      case SessionEventType.StreamRecoveryRetryStarted:
        return this.onStreamRecoveryRetryStarted(event);
      case SessionEventType.ModelSelected:
        return this.onModelSelected(event);
      case SessionEventType.ModelComplete:
        return this.onModelComplete(event);
      case SessionEventType.ToolCallScheduled:
        return this.onToolCallScheduled(event);
      case SessionEventType.ToolCallStarted:
      case SessionEventType.ToolCallProgress:
        return this.onToolCallActivity(event);
      case SessionEventType.ToolCallResult:
        return this.onToolCallResult(event);
      case SessionEventType.ToolCallError:
        return this.onToolCallError(event);
      case SessionEventType.PermissionRequested:
        return this.onPermissionRequested(event);
      case SessionEventType.PermissionResolved:
        return this.onPermissionResolved(event);
      case SessionEventType.PermissionDenied:
        return this.onPermissionDenied(event);
      case SessionEventType.UserInputAutoResolutionUpdated:
        return this.onUserInputAutoResolutionUpdated(event);
      case SessionEventType.WorkspaceHookReviewRequested:
        return this.onWorkspaceHookReviewRequested(event);
      case SessionEventType.WorkspaceHookReviewSettled:
        return this.onWorkspaceHookReviewSettled(event);
      case SessionEventType.WorkspaceHookReviewSuperseded:
        return this.onWorkspaceHookReviewSuperseded(event);
      case SessionEventType.WorkspaceHookAdmissionUpdated:
        return this.onWorkspaceHookAdmissionUpdated(event);
      case SessionEventType.HookRunStarted:
      case SessionEventType.HookRunProgress:
      case SessionEventType.HookRunCompleted:
      case SessionEventType.HookRunFailed:
      case SessionEventType.HookRunBlocked:
        return this.onHookRunLifecycle(event);
      case SessionEventType.TurnSteerQueued:
        return this.onTurnSteerQueued(event);
      case SessionEventType.TurnSteerDeliveryChanged:
        return this.onTurnSteerDeliveryChanged(event);
      case SessionEventType.TurnSteerDispatchChanged:
        return this.onTurnSteerDispatchChanged(event);
      case SessionEventType.TurnSteerDrained:
        return this.onTurnSteerDrained(event);
      case SessionEventType.TurnSteerDiscarded:
        return this.onTurnSteerDiscarded(event);
      case SessionEventType.SessionInputPromoted:
        return this.onSessionInputPromoted(event);
      case SessionEventType.TurnSteerReordered:
        return this.onTurnSteerReordered(event);
      case SessionEventType.QueueAutoDrainChanged:
        return this.onQueueAutoDrainChanged(event);
      case SessionEventType.FollowupModeChanged:
        return this.onFollowupModeChanged(event);
      case SessionEventType.SessionModeChanged:
        return this.onSessionModeChanged(event);
      case SessionEventType.TurnComplete:
        return this.onTurnComplete(event);
      case SessionEventType.TurnError:
        return this.onTurnError(event);
      case SessionEventType.CompactStarted:
      case SessionEventType.CompactCompleted:
      case SessionEventType.CompactFailed:
        return this.onCompactLifecycle(event);
      case SessionEventType.TargetChanged:
        return this.onTargetChanged(event);
      case SessionEventType.TargetCompletionVerification:
        return this.onTargetVerification(event);
      case SessionEventType.SessionForked:
        return this.onSessionForked(event);
      case SessionEventType.RewindTriggered:
        return this.onRewindTriggered(event);
      case SessionEventType.BackgroundTaskStarted:
      case SessionEventType.BackgroundTaskUpdated:
      case SessionEventType.BackgroundTaskCompleted:
        return this.onBackgroundTaskLifecycle(event);
      case SessionEventType.DynamicWorkflowRunProgress:
        return this.onDynamicWorkflowRunProgress(event);
      case SessionEventType.SubagentSpawned:
        return this.onSubagentSpawned(event);
      case SessionEventType.SubagentMessage:
        return this.onSubagentMessage(event);
      case SessionEventType.SubagentStopped:
        return this.onSubagentStopped(event);
      default:
        return [];
    }
  }

  /** A persisted started-only Hook cannot still be running after a real runtime resume. */
  private onSessionResumed(event: SessionEvent): ConversationDelta[] {
    const endedAt = this.ms(event);
    const deltas: ConversationDelta[] = [];
// Session Hooks that have not been returned before the Runtime epoch switch must not be attached to the next round of the new epoch;
// The new Runtime will regenerate its own resume SessionStart lifecycle.
    this.pendingSessionHookInvocations.clear();
    for (const row of this.snapshot.rows.window) {
      if (row.kind !== "hookInvocation" || row.state !== "running") continue;
      const executions = row.executions.map(
        (execution): HookExecutionProjection =>
          execution.state === "running"
            ? {
                ...execution,
                state: "failed",
                outcome: "cancelled",
                endedAt,
                durationMs: Math.max(0, endedAt - execution.startedAt),
              }
            : execution,
      );
      deltas.push({
        op: "row.upserted",
        row: {
          ...row,
          state: "failed",
          executions,
          endedAt,
          durationMs: Math.max(0, endedAt - row.startedAt),
        },
      });
    }
    const pendingInteractions = this.snapshot.pendingInteractions.filter(
      (interaction) => interaction.payload.kind !== "workspaceHookReview",
    );
    if (pendingInteractions.length !== this.snapshot.pendingInteractions.length) {
// reviewFlowId/generation is only monotonic within a single Runtime controller.
// After the Runtime is restarted, the old Requested will be replayed first, and the new flow will start from generation=1;
// SessionResumed is a clear boundary of the new Runtime epoch, and audits that can no longer be parsed by the old Runtime must be eliminated first.
      deltas.push({ op: "state.updated", patch: { pendingInteractions } });
    }
//Soft access control: activate will re-report the admission status after resume.
// Set to null during epoch cleanup to prevent the prompt bar of the old Runtime from remaining before the new Runtime takes over.
    if (this.snapshot.workspaceHookAdmission !== null) {
      deltas.push({ op: "state.updated", patch: { workspaceHookAdmission: null } });
    }
    return deltas;
  }

  private onHookRunLifecycle(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as HookRunLifecyclePayload;
    const hookInvocationId = payload.hookInvocationId;
    const hookCount = payload.hookCount;
    if (
      !hookInvocationId ||
      !payload.hookRunId ||
      !Number.isInteger(hookCount) ||
      (hookCount ?? 0) <= 0 ||
      !Number.isInteger(payload.hookIndex) ||
      payload.hookIndex < 0
    ) {
      return [];
    }
    if (this.rewoundHookInvocationIds.has(hookInvocationId)) return [];

    const rowId = this.hookRowIdByInvocationId.get(hookInvocationId);
    const existing = rowId === undefined ? undefined : this.findRow(rowId);
    const existingRow = existing?.kind === "hookInvocation" ? existing : undefined;
    const pending = this.pendingSessionHookInvocations.get(hookInvocationId);
    const previousExecutions = existingRow?.executions ?? pending?.content.executions ?? [];
    const previousExecution = previousExecutions.find(
      (execution) => execution.hookRunId === payload.hookRunId,
    );
    const descriptor = payload.descriptor;
    if (
      !previousExecution &&
      (descriptor?.clientVisible !== true || descriptor.sourceKind === "internal")
    ) {
      return [];
    }
    const state = this.hookExecutionState(event.type);
    const startedAt =
      typeof payload.startedAt === "number" && Number.isFinite(payload.startedAt)
        ? payload.startedAt
        : (previousExecution?.startedAt ?? this.ms(event));
    const endedAt = state === "running" ? undefined : this.ms(event);
    const durationMs =
      typeof payload.durationMs === "number" && Number.isFinite(payload.durationMs)
        ? Math.max(0, payload.durationMs)
        : endedAt === undefined
          ? undefined
          : Math.max(0, endedAt - startedAt);
    const outcome = this.hookExecutionOutcome(event.type, payload.outcome);
    const didExecute =
      previousExecution?.didExecute === true || event.type === SessionEventType.HookRunStarted;
    const sourceKind = previousExecution?.sourceKind ?? descriptor?.sourceKind;
    if (sourceKind === undefined || sourceKind === "internal") return [];
    const blockReason = payload.blockReason ?? previousExecution?.blockReason;
    const execution: HookExecutionProjection = {
      hookRunId: String(payload.hookRunId),
      hookIndex: payload.hookIndex,
      didExecute,
      state,
      ...(outcome ? { outcome } : {}),
      ...(blockReason ? { blockReason } : {}),
      startedAt,
      ...(endedAt !== undefined ? { endedAt } : {}),
      ...(durationMs !== undefined ? { durationMs } : {}),
      displayName:
        previousExecution?.displayName ??
        (descriptor
          ? hookExecutionDisplayName(descriptor, payload.hookIndex)
          : `Hook #${payload.hookIndex + 1}`),
      sourceKind,
      ...(previousExecution?.pluginName || descriptor?.pluginName
        ? { pluginName: previousExecution?.pluginName ?? descriptor?.pluginName }
        : {}),
      ...(payload.toolName || previousExecution?.toolName
        ? { toolName: payload.toolName ?? previousExecution?.toolName }
        : {}),
    };
    const byRunId = new Map(
      previousExecutions.map((candidate) => [candidate.hookRunId, candidate]),
    );
    byRunId.set(execution.hookRunId, execution);
    const executions = [...byRunId.values()].toSorted(
      (left, right) => left.hookIndex - right.hookIndex,
    );
    const rowState = this.hookInvocationState(executions, hookCount as number);
    const invocationStartedAt = Math.min(...executions.map((candidate) => candidate.startedAt));
    const invocationEndedAt =
      rowState === "running"
        ? undefined
        : Math.max(...executions.map((candidate) => candidate.endedAt ?? candidate.startedAt));

    const content: HookInvocationRowContent = {
      kind: "hookInvocation",
      hookInvocationId,
      hookEventName: payload.hookEventName,
      hookCount: hookCount as number,
      state: rowState,
      startedAt: invocationStartedAt,
      ...(invocationEndedAt !== undefined
        ? {
            endedAt: invocationEndedAt,
            durationMs: Math.max(0, invocationEndedAt - invocationStartedAt),
          }
        : {}),
      lane: this.hookInvocationLane(payload.hookEventName),
      ...(payload.toolCallId ? { anchorToolCallId: String(payload.toolCallId) } : {}),
      executions,
    };

    if (existingRow) {
      const blockErrorDelta = this.hookBlockErrorDelta(event, payload, didExecute, blockReason);
      return [
        {
          op: "row.upserted",
          row: {
            ...existingRow,
            ...content,
          },
        },
        ...(blockErrorDelta ? [blockErrorDelta] : []),
      ];
    }

    if (
      pending ||
      !event.turnId ||
//Maintain turn exclusion only belongs to SessionStart - when the first input is /compact
//SessionStart Hook arrives with compact turnId. It cannot be connected directly. It enters pending first and so on.
// true turn. model-only ≠ maintenance turn: background_task / subagent_message /
// The goal continuation wheels are also model-only, but they are agent wheels that actually run the tool.
// Its PreToolUse/PostToolUse/Stop must be directly connected to the original wheel according to event.turnId (with cold
// merge belongs to alignment), otherwise it will be swallowed by pending and errors will be piled up in the next user round.
      (payload.hookEventName === "SessionStart" &&
        (this.currentTurnId === null || this.currentTurnStartedModelOnly))
    ) {
// Although startup SessionStart may already carry runtime turnId, TurnStarted has not yet been established at this time.
// runtimeTurnId -> productTurnId mapping; append in advance will split it into independent footers.
      this.pendingSessionHookInvocations.set(hookInvocationId, {
        firstEvent: pending?.firstEvent ?? event,
        content,
      });
      return [];
    }

    const turnId = this.turnIdOf(event);
    const rowBase = this.rowBase(event, turnId, hookInvocationId);
    const row: HookInvocationRow = {
      ...rowBase,
      ...content,
    };
    this.hookRowIdByInvocationId.set(hookInvocationId, row.rowId);
    const blockErrorDelta = this.hookBlockErrorDelta(event, payload, didExecute, blockReason);
    return [{ op: "row.appended", row }, ...(blockErrorDelta ? [blockErrorDelta] : [])];
  }

  /**
* The executed block of UserPromptSubmit is a visible error in the current input, but not a task failure.
* Project it to transient lastError and let ChatErrorBanner show the cause directly; next round TurnStarted
* It will be cleaned according to the existing life cycle. Admission-only blocks and tool boundary blocks remain only in Hook digests.
   */
  private hookBlockErrorDelta(
    event: SessionEvent,
    payload: HookRunLifecyclePayload,
    didExecute: boolean,
    blockReason: string | undefined,
  ): ConversationDelta | null {
    if (
      event.type !== SessionEventType.HookRunBlocked ||
      payload.hookEventName !== "UserPromptSubmit" ||
      !didExecute ||
      !blockReason
    ) {
      return null;
    }
    const diagnosticMessage = [payload.stderrPreview, payload.errorMessage, payload.stdoutPreview]
      .map((value) => value?.trim())
      .find((value) => value && value !== blockReason);
    const displayReason = diagnosticMessage ?? blockReason;
    const message =
      displayReason === USER_PROMPT_HOOK_BLOCK_ERROR_TYPE
        ? USER_PROMPT_HOOK_BLOCK_ERROR_TYPE
        : `${USER_PROMPT_HOOK_BLOCK_ERROR_TYPE}: ${displayReason}`;
    const detail = [
      `Hook block reason: ${blockReason}`,
      ...(diagnosticMessage ? [`Hook error: ${diagnosticMessage}`] : []),
    ].join("\n");
    return {
      op: "state.updated",
      patch: this.controlPatch({
        lastError: {
          code: "fault.runtime.hookBlocked",
          message,
          recoverable: false,
          at: this.ms(event),
          source: "runtime",
          traceId: String(event.traceId),
          ...(detail ? { detail } : {}),
          attribution: {
            source: "runtime",
            reason: "hook_blocked",
          },
        },
      }),
    };
  }

  private flushPendingSessionHookInvocations(turnId: string): ConversationDelta[] {
    if (this.pendingSessionHookInvocations.size === 0) return [];
    const deltas: ConversationDelta[] = [];
    for (const [hookInvocationId, pending] of this.pendingSessionHookInvocations) {
      const row: HookInvocationRow = {
        ...this.rowBase(pending.firstEvent, turnId, hookInvocationId),
        ...pending.content,
      };
      this.hookRowIdByInvocationId.set(hookInvocationId, row.rowId);
      deltas.push({ op: "row.appended", row });
    }
    this.pendingSessionHookInvocations.clear();
    return deltas;
  }

  private hookExecutionState(eventType: SessionEvent["type"]): HookExecutionProjection["state"] {
    if (eventType === SessionEventType.HookRunFailed) return "failed";
    if (
      eventType === SessionEventType.HookRunCompleted ||
      eventType === SessionEventType.HookRunBlocked
    ) {
      return "completed";
    }
    return "running";
  }

  private hookExecutionOutcome(
    eventType: SessionEvent["type"],
    outcome: HookRunLifecyclePayload["outcome"],
  ): HookExecutionProjection["outcome"] {
    if (outcome) return outcome;
    if (eventType === SessionEventType.HookRunCompleted) return "success";
    if (eventType === SessionEventType.HookRunBlocked) return "blocked";
    if (eventType === SessionEventType.HookRunFailed) return "failed";
    return undefined;
  }

  private hookInvocationState(
    executions: readonly HookExecutionProjection[],
    hookCount: number,
  ): HookInvocationRow["state"] {
    if (
      executions.length < hookCount ||
      executions.some((execution) => execution.state === "running")
    ) {
      return "running";
    }
    return executions.some((execution) => execution.state === "failed") ? "failed" : "completed";
  }

  private hookInvocationLane(
    eventName: HookRunLifecyclePayload["hookEventName"],
  ): HookInvocationRow["lane"] {
    if (eventName === "PreToolUse" || eventName === "PermissionRequest") return "toolBefore";
    if (eventName === "PostToolUse" || eventName === "PostToolUseFailure") return "toolAfter";
    return "assistantWork";
  }

  /**
* rewind/edit/retry's live projection truncation (editUserQuery/retryTurn's
* `row.removed(target)`). RewindTriggered with targetMessageId → Reverse check rowId →
* Remove the entire paragraph from the first line (turnHeader) of the turn to which the line belongs, so that live subscribers can see the truncation immediately.
* The subsequent editRerun new turn will be added along the existing event path. Cold subscribe/refresh truncated transcript
* Reconstructed from transcript synthesis and hydration.
* When the messageId cannot be found (the user row has no messageId, or is late), it returns empty and will not be deleted by mistake.
   */
  private onRewindTriggered(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as {
      targetMessageId?: string;
      scope?: string;
      branchCutAfterMessageId?: string;
      branchGeneration?: number;
      createdMessageId?: string;
      reason?: string;
    };
    if (payload.scope === "workspace" && payload.reason === "file_summary_rewind") {
      const targetMessageId = payload.targetMessageId;
      if (!targetMessageId) return [];
      const targetRowId = this.rowIdForMessageId(targetMessageId);
      if (targetRowId === null) return [];
      const targetRow = this.findRow(targetRowId);
      if (!targetRow) return [];
      const headerRowId = this.turnHeaderRowIdByTurnId.get(targetRow.turnId);
      const headerRow = headerRowId !== undefined ? this.findRow(headerRowId) : undefined;
      if (headerRow?.kind !== "turnHeader" || !headerRow.fileChanges) return [];
      return [
        {
          op: "row.upserted",
          row: {
            ...headerRow,
            fileChanges: {
              ...headerRow.fileChanges,
              state: "reverted",
            },
          },
        },
      ];
    }
// New semantics only consume committed conversations with branchGeneration/cut rewind; createdMessageId
// Only compatible with old transcripts. Failures/conflicts do not trigger events, so false UI truncation is not created.
    const applied =
      (payload.branchGeneration !== undefined && payload.branchCutAfterMessageId !== undefined) ||
      payload.createdMessageId !== undefined;
    if ((payload.scope !== "conversation" && payload.scope !== "both") || !applied) return [];
    const targetMessageId = payload.targetMessageId;
    if (!targetMessageId) return [];
    const targetRowId = this.rowIdForMessageId(targetMessageId);
    if (targetRowId === null) return [];
    const targetRow = this.findRow(targetRowId);
    if (!targetRow) return [];
//Remove from the first line of the turn to which this line belongs (the entire turn is replaced by rewind/edit/retry).
    const turnHeaderRowId = this.turnHeaderRowIdByTurnId.get(targetRow.turnId) ?? targetRowId;
    const fromRowId = Math.min(turnHeaderRowId, targetRowId);
// Clean up the messageId/tool ​​index of the removed rows to avoid hanging mappings.
    for (const [rowId] of this.messageIdByRowId) {
      if (rowId >= fromRowId) this.messageIdByRowId.delete(rowId);
    }
    for (const [messageId, rowId] of this.outputContinuationRowIdByMessageId) {
      if (rowId >= fromRowId) this.outputContinuationRowIdByMessageId.delete(messageId);
    }
    for (const [rowId, entityId] of this.entityIdByRowId) {
      if (rowId >= fromRowId) {
        this.entityIdByRowId.delete(rowId);
        this.editTargetByEntityId.delete(entityId);
      }
    }
    for (const [hookInvocationId, rowId] of this.hookRowIdByInvocationId) {
      if (rowId >= fromRowId) {
        this.rewoundHookInvocationIds.add(hookInvocationId);
        this.hookRowIdByInvocationId.delete(hookInvocationId);
      }
    }
    return [{ op: "row.removed", fromRowId }];
  }

/** messageId → rowId reverse search (reverse linear scan of messageIdByRowId; the number of rows is bounded, no additional indexing is required). */
  private rowIdForMessageId(messageId: string): number | null {
    const continuationRowId = this.outputContinuationRowIdByMessageId.get(messageId);
    if (continuationRowId !== undefined) return continuationRowId;
    for (const [rowId, mid] of this.messageIdByRowId) {
      if (mid === messageId) return rowId;
    }
    return null;
  }

  /**
* Any rowId → the rewind anchor messageId of its turn. New live/cold user row should be
* Directly carry persistent user messageId; the same as turn assistant, only reserved for old event compatibility fallback.
* `canEdit` is not allowed to rely on this fallback and must be driven by the exact target of the user row itself.
   */
  getTurnRewindAnchor(rowId: number): string | null {
    return this.rewindAnchorForRows(this.snapshot.rows.window, rowId);
  }

  private rewindAnchorForRows(rows: readonly ConversationRow[], rowId: number): string | null {
    const row = rows.find((candidate) => candidate.rowId === rowId);
    if (!row) return null;
    const turnId = row.turnId;
    for (const [candidateRowId, messageId] of this.messageIdByRowId) {
      const candidate = rows.find((item) => item.rowId === candidateRowId);
      if (candidate && candidate.turnId === turnId) return messageId;
    }
    return null;
  }

  /**
* The display identity and command identity of real-user row must be registered atomically.
* TurnSteerDrained once only wrote messageId/entityId and missed the edit target.
* Causes the UI action and editUserQuery resolver to reach opposite conclusions on the same row.
   */
  private registerCanonicalUserRowTarget(
    rowId: number,
    entityId: string,
    editTarget?: ConversationEditTarget,
  ): void {
    this.entityIdByRowId.set(rowId, entityId);
    if (!editTarget) return;
    this.messageIdByRowId.set(rowId, editTarget.transcriptMessageId);
    this.editTargetByEntityId.set(entityId, editTarget);
  }

// ── life cycle ──

  private onSessionCreated(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as { contextWindow?: number };
    this.contextWindowState.maxTokens = payload.contextWindow ?? null;
// Draft semantics: session entity already exists, no row; phase remains draft, no visible delta.
    return [];
  }

// renameSession / Automatic title: SessionTitleUpdated(title, source) → Update meta.
// custom (user rename) has the highest priority, and will no longer be overwritten by generated after custom (consistent with core titleSource).
  private onSessionTitleUpdated(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as {
      title?: string;
      source?: string;
    };
    const title = payload.title ?? "";
// The titleSource of core has 4 values ​​(default/first_input/generated/custom); the projection meta is normalized to
// default/generated/custom (first_input is classified as generated: both are "non-user explicit").
    const source: "default" | "generated" | "custom" =
      payload.source === "custom"
        ? "custom"
        : payload.source === "default"
          ? "default"
          : "generated";
    const prev = this.snapshot.meta;
    if (prev.titleSource === "custom" && source === "generated") return [];
    if (prev.title === title && prev.titleSource === source) return [];
    return [
      {
        op: "state.updated",
        patch: { meta: { title, titleSource: source } },
      },
    ];
  }

  private onTurnStarted(fact: CanonicalUserIntentFact): ConversationDelta[] {
    const event = fact.event;
    const runtimeTurnId = fact.runtimeTurnId;
    const turnId = fact.productTurnId;
    this.currentTurnId = runtimeTurnId;
    this.currentTurnStartedModelOnly = fact.visibility === "modelOnly";
// New runtimeTurn: product turn mapping is reset to zero (1:1), working hour base = starting point of this round.
    this.productTurnIdByRuntimeTurnId.delete(runtimeTurnId);
    if (turnId !== runtimeTurnId) this.productTurnIdByRuntimeTurnId.set(runtimeTurnId, turnId);
    this.runtimeTurnIdByProductTurnId.set(turnId, runtimeTurnId);
    this.productTurnSplitOrdinalByRuntimeTurnId.delete(runtimeTurnId);
    this.currentProductTurnStartedAtMs = this.ms(event);
    this.streamingTextRowId = null;
    this.streamingReasoningRowId = null;
    this.outputContinuationTextRowId = null;

// The ToolCallResult of the background Agent is just launch ACK. First, close the tool line as
// success; the real final state of the child Agent is then used as a model-only task-notification to start a new round.
// V4 did not consume this authoritative fact according to tool-use-id in the past, so the card will permanently stop at completed after 429.
    const deltas: ConversationDelta[] = this.applyBackgroundTaskNotification(fact);
    const sharedContextRef = fact.sharedContextRefs?.[0];
    if (
      sharedContextRef &&
      this.snapshot.sharedContextImport &&
      "contextId" in this.snapshot.sharedContextImport &&
      this.snapshot.sharedContextImport.contextId === sharedContextRef.context_id &&
      (this.snapshot.sharedContextImport.status === "pending" ||
        this.snapshot.sharedContextImport.status === "reserved")
    ) {
      const sharedContextImport = {
        ...this.snapshot.sharedContextImport,
        status: "attached" as const,
      };
      this.snapshot = { ...this.snapshot, sharedContextImport };
      deltas.push({ op: "state.updated", patch: { sharedContextImport } });
    }
// marker timing: only when
// When the identity of the provider/model actually used in this round is different from the previous round, it will be dropped before turnHeader.
// modelChange marker. Ordinary first round silentInitial does not produce marker; explicit sourceLess boundary
// Generate the "in use" marker. Thought depth changes only update config.thought, not model identity changes.
// Bug background: The old implementation drops the marker in onModelSelected (when switching actions), and the draft state warms up the session
//Cut the model once and [modelChange] will be displayed above the first message.
    const config = this.snapshot.config;
    const hasModel = config.provider !== "" && config.model !== "";
    if (hasModel && this.lastTurnModel.kind === "sourceLess") {
      deltas.push({
        op: "row.appended",
        row: {
          ...this.rowBase(
            event,
            turnId,
            `model-initial:${turnId}:${config.provider}/${config.model}`,
          ),
          kind: "timelineMarker",
          lane: "lightBoundary",
          marker: {
            type: "modelChange",
            toProvider: config.provider,
            toModel: config.model,
            toThought: config.thought,
          },
        },
      });
    } else if (
      hasModel &&
      this.lastTurnModel.kind === "known" &&
      (this.lastTurnModel.provider !== config.provider || this.lastTurnModel.model !== config.model)
    ) {
      deltas.push({
        op: "row.appended",
        row: {
          ...this.rowBase(
            event,
            turnId,
            `model-change:${turnId}:${this.lastTurnModel.provider}/${this.lastTurnModel.model}->${config.provider}/${config.model}`,
          ),
          kind: "timelineMarker",
// Lane is determined by the projection (the UI must not infer the placement semantics by itself based on the marker type).
          lane: "lightBoundary",
          marker: {
            type: "modelChange",
            fromProvider: this.lastTurnModel.provider,
            fromModel: this.lastTurnModel.model,
            toProvider: config.provider,
            toModel: config.model,
            toThought: config.thought,
          },
        },
      });
    }
    if (hasModel) {
      this.lastTurnModel = {
        kind: "known",
        provider: config.provider,
        model: config.model,
        thought: config.thought,
      };
    }
    const headerBase = this.rowBase(event, turnId, turnId);
    const header: TurnHeaderRow = {
      ...headerBase,
      kind: "turnHeader",
      origin: fact.turnHeaderOrigin,
      executionKind: fact.executionKind,
      ...(fact.sourceCommandId ? { sourceCommandId: fact.sourceCommandId } : {}),
      ...(fact.originMeta ? { originMeta: fact.originMeta } : {}),
      ...(fact.workflowLaunch ? { workflowLaunch: fact.workflowLaunch } : {}),
      state: "running",
      startedAt: headerBase.createdAt,
    };
    this.turnHeaderRowIdByTurnId.set(turnId, header.rowId);
    deltas.push({ op: "row.appended", row: header });

// model-only inputs (goal continuations, etc.) do not produce visible userInput rows.
    if (fact.visibility === "visible") {
      const rowBase = this.rowBase(event, turnId, fact.entityId);
      const rootSourceCommandId = fact.provenance?.sourceCommandId ?? fact.sourceCommandId;
      const attachments = fact.attachments?.map((attachment, index) => ({
        ...attachment,
        ref: attachment.ref ?? `turn-attachment/${rowBase.rowId}/${index}`,
      }));
      const row: UserInputRow = {
        ...rowBase,
        kind: "userInput",
        text: fact.input,
        origin: fact.origin,
        ...(fact.sourceCommandId ? { sourceCommandId: fact.sourceCommandId } : {}),
        ...(rootSourceCommandId ? { rootSourceCommandId } : {}),
        ...(fact.clientId ? { clientId: fact.clientId } : {}),
        ...(fact.workflowLaunch ? { workflowLaunch: fact.workflowLaunch } : {}),
        ...(fact.epilogueStart === undefined ? {} : { epilogueStart: fact.epilogueStart }),
        ...(attachments && attachments.length > 0 ? { attachments } : {}),
      };
// workspace checkpoint uses user messageId as targetMessageId.
// Ordinary TurnStarted also needs to register the internal anchor point of userInput row, otherwise the file summary query
// Only the assistant messageId can be found. Expanding the list will not find the checkpoint of this round.
      this.registerCanonicalUserRowTarget(
        row.rowId,
        fact.entityId,
        fact.transcriptMessageId
          ? {
              entityId: fact.entityId,
              productTurnId: fact.productTurnId,
              transcriptMessageId: fact.transcriptMessageId,
              coveredByStableCompact: false,
              intent: {
                kind: fact.intentKind,
                text: fact.intentText,
                ...(fact.sourceCommandId ? { sourceCommandId: fact.sourceCommandId } : {}),
                ...(fact.clientId ? { clientId: fact.clientId } : {}),
                ...(fact.attachments ? { attachments: fact.attachments } : {}),
                ...(fact.queueItemId ? { queueItemId: fact.queueItemId } : {}),
                ...(fact.admissionSeq !== undefined ? { admissionSeq: fact.admissionSeq } : {}),
                ...(fact.admittedAt !== undefined ? { admittedAt: fact.admittedAt } : {}),
                ...(fact.requestedDelivery ? { requestedDelivery: fact.requestedDelivery } : {}),
                ...(fact.admittedDelivery ? { admittedDelivery: fact.admittedDelivery } : {}),
                ...(fact.fallbackReasonCode ? { fallbackReasonCode: fact.fallbackReasonCode } : {}),
                ...(fact.modelSelection ? { modelSelection: fact.modelSelection } : {}),
                ...(fact.mode ? { mode: fact.mode } : {}),
                ...(fact.planEnabled !== undefined ? { planEnabled: fact.planEnabled } : {}),
                ...(fact.provenance ? { provenance: fact.provenance } : {}),
              },
            }
          : undefined,
      );
      deltas.push({
        op: "row.appended",
        row,
      });
    }

    if (fact.executionKind === "agent") {
      deltas.push({
        op: "state.updated",
        patch: this.controlPatch({
          phase: "running",
          sessionEnded: false,
          canStop: true,
          stopState: "stoppable",
          stopTargetKind: "assistant",
          activeWorks: [
            {
              kind: fact.origin === "goalContinuation" ? "goalContinuation" : "primaryTurn",
              ...(fact.foregroundExecutionId
                ? { foregroundExecutionId: fact.foregroundExecutionId }
                : {}),
              startedAt: this.ms(event),
            },
          ],
// After a new round is accepted, the old error is no longer the current fact (same verdict as the old reducer).
          lastError: null,
          apiRetry: null,
        }),
      });
    }
// The visible query of /goal uses controlOnly turn to establish the live timeline identity.
// But the actual execution belongs to the goalContinuation that follows. If the control wheel also advances running, the continuous link
// A second copy of activeWorks will be generated briefly, and a fake work life cycle will appear when restoring the projection.
    return deltas;
  }

  private applyBackgroundTaskNotification(fact: CanonicalUserIntentFact): ConversationDelta[] {
    const parsed = parseZCodeBackgroundTaskNotificationText(fact.input);
    if (!parsed) return [];
    const row = this.findToolRow(parsed.toolUseId);
    if (!row) return [];

    const notificationStatus = zcodeBackgroundTaskNotificationToolUpdateStatus(
      parsed.notification.status,
    );
    const status: ToolCallRow["status"] =
      notificationStatus === "failed"
        ? "error"
        : notificationStatus === "stopped"
          ? "cancelled"
          : "success";
    const content =
      parsed.notification.result ?? parsed.notification.summary ?? parsed.notification.error;
    const next: ToolCallRow = {
      ...row,
      status,
      ...(content
        ? {
            output: buildToolOutput({ success: status === "success", content }, parsed.toolUseId),
          }
        : {}),
      endedAt: this.ms(fact.event),
    };
    if (status === "error") {
      next.error = {
        code: "fault.runtime.backgroundTaskFailed",
        message: parsed.notification.error ?? content ?? "Background task failed.",
      };
    } else {
      delete next.error;
    }
    return [{ op: "row.upserted", row: next }];
  }

  private onTurnComplete(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as TurnCompletePayload;
    this.outputContinuationTextRowId = null;
    const headerState = mapTurnResultToHeaderState(payload.resultType);
    const header = this.turnHeaderForEvent(event);
    if (header?.executionKind === "controlOnly") {
// controlOnly does not have Agent working hours; in particular, duration=0 cannot be sent to the old UI, which will
// For readability format 0 seconds as "1 second worked". Only the visible rounds are closed here, and the session control is not touched——
// Except for draft departures (see leaveDraftAfterControlOnlyTurn).
      const deltas = [
        ...this.upsertTurnHeader(event, headerState, undefined, payload.historyRoundCount),
        ...this.leaveDraftAfterControlOnlyTurn(
          payload.resultType === "success" ? "completedSuccess" : "completedInterrupted",
        ),
      ];
      this.currentTurnId = null;
// After the turn is closed, the model-only mark will become invalid to avoid affecting the next ownership judgment.
      this.currentTurnStartedModelOnly = false;
      return deltas;
    }
    const phase: SessionControl["phase"] =
      payload.resultType === "success"
        ? "completedSuccess"
        : payload.resultType === "cancelled"
          ? "completedInterrupted"
          : "error";
    const streamClose = payload.resultType === "success" ? "complete" : "interrupted";

// stopPausesActiveGoalTarget: when stop acts on any foreground work,
// The active/verifying goal is forced to enter paused, waiting for explicit resumeGoal.
    const goal = this.snapshot.goal;
    const pausedGoal: GoalState | undefined =
      payload.resultType === "cancelled" &&
      (goal?.status === "active" || goal?.status === "verifying")
        ? { ...goal, status: "paused" }
        : undefined;

// stopKeepsQueueAndDisablesAutoDrain (stop effect): the queue remains intact after interruption
// And does not consume automatically → forms a pause queue; pauseReason is only used in the UI to explain the reasons and does not participate in routing decisions.
    const heldQueue =
      payload.resultType === "cancelled" &&
      payload.preserveQueueAutoDrainOnCancel !== true &&
      this.snapshot.queue.items.length > 0 &&
      (this.snapshot.queue.autoDrain || this.snapshot.queue.pauseReason !== "stopped")
        ? {
            ...this.snapshot.queue,
            autoDrain: false,
            pauseReason: "stopped" as const,
          }
        : undefined;

    const deltas: ConversationDelta[] = [
      ...this.closeStreamingRows(streamClose),
// The final state of turn closes the flying foreground tool row (closing invariant: profile
// Filtered inputText streams must be contained by unfilterable row.upserted, see profiles.ts).
      ...this.closeOpenToolRows(event, payload.resultType === "cancelled" ? "cancelled" : "error"),
      ...this.upsertTurnHeader(
        event,
        headerState,
        this.activeMsForCompletion(event, payload.duration),
        payload.historyRoundCount,
      ),
      ...(payload.resultType === "success" ? this.markStableForkAssistant(event) : []),
      {
        op: "state.updated",
        patch: this.controlPatch(
          {
            phase,
            sessionEnded: phase !== "error",
            canStop: false,
            stopState: "idle",
            stopTargetKind: "unknown",
            activeWorks: [],
// The old V4 reducer does not consume ModelNetworkStatus, if turn after supplementary projection
// Directly entering the final state without cleaning it will leave "reconnecting" in the next round.
            apiRetry: null,
          },
          pausedGoal,
          heldQueue,
        ),
      },
    ];
    this.currentTurnId = null;
// After the turn is closed, the model-only mark will become invalid to avoid affecting the next ownership judgment.
    this.currentTurnStartedModelOnly = false;
    return deltas;
  }

  /**
* There is only one way to exit the draft: first-round closing. The definition of phase `draft` is "pure memory, never real content, CLI restart
* Disappears"; once a controlOnly round is closed, the session has a persistent visible history, and calling draft again will be the same as
* Cold recovery contradiction - the store seed will give it a final phase, but the live projection stops at draft. Center starts directly
* The session has only one controlOnly startup wheel, the active projection phase is always draft, and the sessions-index summary is therefore replaced by task-index
* syncer When the draft is discarded, the sidebar will not appear until restarting. Therefore, the controlOnly closure only advances the phase when the session is still draft.
* (success → completedSuccess, cancel → completedInterrupted, failure → error); control wheel on non-draft session
* Still do not touch session control (the visible query wheel of the goal must not fake running / working hours, see onTurnStarted).
   */
  private leaveDraftAfterControlOnlyTurn(
    phase: Exclude<SessionControl["phase"], "draft" | "prewarming" | "running">,
  ): ConversationDelta[] {
    if (this.snapshot.control.phase !== "draft") return [];
    return [
      {
        op: "state.updated",
        patch: this.controlPatch({
          phase,
          sessionEnded: phase !== "error",
          canStop: false,
          stopState: "idle",
          stopTargetKind: "unknown",
          activeWorks: [],
        }),
      },
    ];
  }

  private onTurnError(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as TurnErrorPayload;
    this.outputContinuationTextRowId = null;
    if (this.turnHeaderForEvent(event)?.executionKind === "controlOnly") {
      const deltas = [
        ...this.upsertTurnHeader(event, "failed"),
        ...this.leaveDraftAfterControlOnlyTurn("error"),
      ];
      this.currentTurnId = null;
// After the turn is closed, the model-only mark will become invalid to avoid affecting the next ownership judgment.
      this.currentTurnStartedModelOnly = false;
      return deltas;
    }
// TurnError ends with the current turn,
// Not an accepted future input. The old reducer does not have terminal queue patch, core for
// To prevent error post-suspending, TurnSteerDiscarded can only be sent first, causing user messages to be lost; now the existing queue
//Convert to error-paused as is, waiting for explicit setAutoDrain(true) to restore FIFO.
    const heldQueue =
      this.snapshot.queue.items.length > 0
        ? {
            ...this.snapshot.queue,
            autoDrain: false,
            pauseReason: "error" as const,
          }
        : undefined;
    return [
      ...this.closeStreamingRows("interrupted"),
      ...this.closeOpenToolRows(event, "error"),
      ...this.upsertTurnHeader(event, "failed"),
      {
        op: "state.updated",
        patch: this.controlPatch(
          {
            phase: "error",
            sessionEnded: false,
            canStop: false,
            stopState: "idle",
            stopTargetKind: "unknown",
            activeWorks: [],
// The event side does not yet carry the fault.* classification. The error type is transparently transmitted first, and the mapping is refined after the classification is completed.
            lastError: {
              code: payload.error.code ?? payload.error.type ?? "fault.runtime.unknown",
              message: payload.error.message,
              recoverable: payload.error.retryable ?? LEGACY_TURN_ERROR_RECOVERABLE_FALLBACK,
              at: this.ms(event),
// The old projection writes all TurnErrors as runtime, losing the fact that the adapter recognizes the provider/network.
              source: payload.error.attribution?.source ?? "runtime",
              traceId: String(event.traceId),
              ...(payload.error.detail ? { detail: payload.error.detail } : {}),
              ...(payload.error.underlyingErrorMessage
                ? { underlyingErrorMessage: payload.error.underlyingErrorMessage }
                : {}),
              ...(payload.error.underlyingErrorDetail
                ? { underlyingErrorDetail: payload.error.underlyingErrorDetail }
                : {}),
              ...(payload.error.attribution ? { attribution: payload.error.attribution } : {}),
            },
//Same as onTurnComplete: The final state is the bottom-clearing boundary of the retry life cycle.
            apiRetry: null,
          },
          undefined,
          heldQueue,
        ),
      },
    ];
  }

// ── Streaming output ──

  private onModelNetworkStatus(event: SessionEvent): ConversationDelta[] {
    if (!this.acceptsActiveModelEvent(event)) return [];
    const payload = event.payload as ModelNetworkStatusPayload;
    switch (payload.type) {
      case "model_retry_scheduled": {
        const attempt = positiveInteger(payload.attempt, 1);
        const maxAttempts = Math.max(
          positiveInteger(payload.maxAttempts, attempt + 1),
          attempt + 1,
        );
        return this.setApiRetry({
          attempt,
          maxAttempts,
          nextRetryAt: this.ms(event) + nonNegativeInteger(payload.delayMs, 0),
          reasonCode: modelRetryReasonCode(payload.reason),
        });
      }
      case "model_request_started":
        if (payload.streamRecovery) {
          return this.setApiRetry(
            this.streamRecoveryApiRetry(
              payload.streamRecovery.retryNumber,
              payload.streamRecovery.maxRetries,
              this.ms(event),
              this.snapshot.control.apiRetry?.reasonCode ?? "fault.network.sseDisconnected",
            ),
          );
        }
// adapter attempt=2+ only indicates that the retry request has been issued, but does not mean that the connection has been restored;
// Keep the current state and wait for the first valid text/reasoning/tool ​​to progress before cleaning it to avoid label crashes.
        return positiveInteger(payload.attempt, 1) <= 1 ? this.setApiRetry(null) : [];
      case "model_request_completed":
        return this.setApiRetry(null);
      case "model_request_failed":
        return payload.retryable ? [] : this.setApiRetry(null);
      case "model_stream_stalled":
      case "model_first_provider_event":
      case "model_first_content":
      case "model_first_text":
// Both ends of admission waiting are runtime observations, not UI status:
// Not mapped to retry/wait labels.
      case "model_request_queued":
      case "model_request_admitted":
        return [];
    }
  }

  private onStreamRecoveryStarted(event: SessionEvent): ConversationDelta[] {
    if (!this.acceptsActiveModelEvent(event)) return [];
    const payload = event.payload as StreamRecoveryStartedPayload;
    return this.setApiRetry(
      this.streamRecoveryApiRetry(
        payload.retryNumber,
        payload.maxRetries,
        this.ms(event),
        streamRecoveryReasonCode(payload.failureKind),
      ),
    );
  }

  private onStreamRecoveryTailDiscarded(event: SessionEvent): ConversationDelta[] {
    if (!this.acceptsActiveModelEvent(event)) return [];
// Bug reason: Core has used tail_discarded to cut off the failed assistant attempt, but the old V4 projection ignores the event.
// The next time reasoning/text arrives, the old line will be incorrectly closed as complete. Here must be marked interrupted first,
// Let the resume flow open new rows with the new assistant identity to avoid the UI looking like one continuous complete output.
// Bug reason: Tool lines that have been opened by tool_input_start when the stream is cut off, but have not yet been finalized by tool_call, also belong to
// The invalidated tail——core only synthesizes the final state for the submitted tool, and no one closes these lines; the recovery request will use the new one
// toolCallId opens another line, and the UI then displays two "Writing Workflow" side by side. Submitted (running /
//pendingApproval) are not listed in this column, their final state is published by the executor itself.
    return [
      ...this.closeStreamingRows("interrupted"),
      ...this.closeOpenToolRows(event, "cancelled", (row) => row.status === "inputStreaming"),
    ];
  }

  private onStreamRecoveryRetryStarted(event: SessionEvent): ConversationDelta[] {
    if (!this.acceptsActiveModelEvent(event)) return [];
    const payload = event.payload as StreamRecoveryRetryStartedPayload;
    return this.setApiRetry(
      this.streamRecoveryApiRetry(
        payload.retryNumber,
        payload.maxRetries,
        this.ms(event),
        this.snapshot.control.apiRetry?.reasonCode ?? "fault.network.sseDisconnected",
      ),
    );
  }

  private streamRecoveryApiRetry(
    retryNumber: number,
    maxRetriesValue: number,
    nextRetryAt: number,
    reasonCode: string,
  ): ApiRetryState {
    const attempt = positiveInteger(retryNumber, 1);
    const maxRetries = Math.max(positiveInteger(maxRetriesValue, attempt), attempt);
    return {
      attempt,
      maxAttempts: maxRetries + 1,
      nextRetryAt,
      reasonCode,
    };
  }

  private setApiRetry(apiRetry: ApiRetryState | null): ConversationDelta[] {
    const current = this.snapshot.control.apiRetry;
    if (
      current === apiRetry ||
      (current !== null &&
        apiRetry !== null &&
        current.attempt === apiRetry.attempt &&
        current.maxAttempts === apiRetry.maxAttempts &&
        current.nextRetryAt === apiRetry.nextRetryAt &&
        current.reasonCode === apiRetry.reasonCode)
    ) {
      return [];
    }
    return [
      {
        op: "state.updated",
        patch: this.controlPatch({ apiRetry }),
      },
    ];
  }

  private acceptsActiveModelEvent(event: SessionEvent): boolean {
    if (!this.isRunning()) return false;
// stop/Old requests may be late after the new round; session-level status alone will make the old turn
// retry/progress overwrites the current input field. The current runtime turn must be isolated by turnId when it is known.
    return (
      this.currentTurnId === null ||
      event.turnId === undefined ||
      String(event.turnId) === this.currentTurnId
    );
  }

  private onModelStreaming(fact: CanonicalAssistantSegmentFact): ConversationDelta[] {
    const event = fact.event;
// Late final states will not be resurrected: streaming events arriving during non-running periods will be rejected.
// assistant conservation: text class rejection is not harmless discard - the projection is created later than
// When TurnStarted (publisher is created mid-subscription), the entire reply will disappear silently until refreshed
// (live vector of "Reply the entire paragraph and disappear"). The count is exposed to the gateway: set the stale flag,
//Next subscription forces rehydration to complete from persistent facts.
    if (!this.isRunning()) {
      const dropped = fact.stream;
      if (
        dropped.kind === "text_start" ||
        dropped.kind === "text_delta" ||
        dropped.kind === "reasoning_start" ||
        dropped.kind === "reasoning_delta"
      ) {
        this.droppedContentStreamEventCount += 1;
      }
      return [];
    }
    const payload = fact.stream;
    switch (payload.kind) {
      case "text_start":
        return this.openTextRow(event, fact);
      case "text_delta": {
        const open = this.streamingTextRowId === null ? this.openTextRow(event, fact) : [];
        return [
          ...open,
          {
            op: "row.delta",
            rowId: this.streamingTextRowId as number,
            path: "text",
            append: payload.delta,
          },
        ];
      }
      case "text_end":
        return this.closeTextRow("complete");
      case "reasoning_start":
        return this.openReasoningRow(event, fact);
      case "reasoning_delta": {
        const open =
          this.streamingReasoningRowId === null ? this.openReasoningRow(event, fact) : [];
        return [
          ...open,
          {
            op: "row.delta",
            rowId: this.streamingReasoningRowId as number,
            path: "text",
            append: payload.delta,
          },
        ];
      }
      case "reasoning_end":
        return this.closeReasoningRow();
      case "tool_input_start":
        return this.openToolRow(event, payload, fact.entityId);
      case "tool_input_delta": {
        return this.appendStreamingToolInput(event, payload);
      }
      case "tool_input_end":
        return this.flushStreamingToolInput(String(payload.toolCallId ?? ""));
      case "tool_call":
        return this.finalizeStreamingToolInput(event, payload);
      default:
        return [];
    }
  }

  private openTextRow(
    event: SessionEvent,
    fact: CanonicalAssistantSegmentFact,
  ): ConversationDelta[] {
    const close = this.closeTextRow("complete");
    const continuationRowId = this.outputContinuationTextRowId;
    this.outputContinuationTextRowId = null;
    const continuationRow =
      continuationRowId === null ? undefined : this.findRow(continuationRowId);
    const currentTurnId = this.turnIdOf(event);
    const lastVisibleRow = this.snapshot.rows.window.at(-1);
    if (
      continuationRow?.kind === "assistantText" &&
      continuationRow.turnId === currentTurnId &&
      lastVisibleRow?.rowId === continuationRow.rowId
    ) {
// The runtime's output-token Continue will create a new one for each provider request
// assistantMessageId; the old projection therefore splits a sentence into history partial + tail text. length
// Exact qualification has been provided on ModelComplete, here only the immediately adjacent same turn text row is reopened,
// Let external continuous/replayable clients only observe a continuously growing assistant.
      const {
        actions: _actions,
        assistantResponseId: _assistantResponseId,
        feedback: _feedback,
        ...continuedBase
      } = continuationRow;
      const row: AssistantTextRow = {
        ...continuedBase,
        entityId: fact.entityId,
        ...(fact.stream.assistantResponseId
          ? { assistantResponseId: fact.stream.assistantResponseId }
          : {}),
        state: "streaming",
      };
      this.streamingTextRowId = row.rowId;
      this.entityIdByRowId.set(row.rowId, fact.entityId);
      const previousMessageId = this.messageIdByRowId.get(row.rowId);
      if (previousMessageId) {
        this.outputContinuationRowIdByMessageId.set(previousMessageId, row.rowId);
      }
      if (fact.transcriptMessageId) {
        this.messageIdByRowId.set(row.rowId, fact.transcriptMessageId);
      }
      return [...close, { op: "row.upserted", row }];
    }

// Invariant: New segments that are not output-token Continue must have new rowId; existing streaming rows are closed first.
    const row: AssistantTextRow = {
      ...this.rowBase(event, this.turnIdOf(event), fact.entityId),
      kind: "assistantText",
      ...(fact.stream.assistantResponseId
        ? { assistantResponseId: fact.stream.assistantResponseId }
        : {}),
      text: "",
      state: "streaming",
    };
    this.streamingTextRowId = row.rowId;
    this.entityIdByRowId.set(row.rowId, fact.entityId);
// forkAssistant anchor: assistant row → authoritative messageId (provided in the first frame of the provider stream).
    if (fact.transcriptMessageId) {
      this.messageIdByRowId.set(row.rowId, fact.transcriptMessageId);
    }
    return [...close, { op: "row.appended", row }];
  }

  private closeTextRow(state: "complete" | "interrupted"): ConversationDelta[] {
    if (this.streamingTextRowId === null) return [];
    const row = this.findRow(this.streamingTextRowId);
    this.streamingTextRowId = null;
    if (row?.kind !== "assistantText") return [];
    return [{ op: "row.upserted", row: { ...row, state } }];
  }

  private onAssistantFeedbackUpdated(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as AssistantFeedbackUpdatedPayload;
    const row = this.snapshot.rows.window.find(
      (candidate): candidate is AssistantTextRow =>
        candidate.kind === "assistantText" && candidate.entityId === payload.entityId,
    );
    if (!row) return [];
    if (payload.feedback === null) {
      if (row.feedback === undefined) return [];
      const { feedback: _removedFeedback, ...withoutFeedback } = row;
      return [{ op: "row.upserted", row: withoutFeedback }];
    }
    if (row.feedback === payload.feedback) return [];
    return [{ op: "row.upserted", row: { ...row, feedback: payload.feedback } }];
  }

  private openReasoningRow(
    event: SessionEvent,
    fact: CanonicalAssistantSegmentFact,
  ): ConversationDelta[] {
    const close = this.closeReasoningRow();
    const row: ReasoningRow = {
      ...this.rowBase(event, this.turnIdOf(event), fact.entityId),
      kind: "reasoning",
// Bug reason: canonical stream already carries assistant response identity, but the old projection is only in the text and tool lines
// Save it so the UI cannot deterministically group reasoning with the same response into the CUA Group.
      ...(fact.stream.assistantResponseId
        ? { assistantResponseId: fact.stream.assistantResponseId }
        : {}),
      text: "",
      state: "streaming",
    };
    this.streamingReasoningRowId = row.rowId;
    this.entityIdByRowId.set(row.rowId, fact.entityId);
    return [...close, { op: "row.appended", row }];
  }

  private closeReasoningRow(state: "complete" | "interrupted" = "complete"): ConversationDelta[] {
    if (this.streamingReasoningRowId === null) return [];
    const row = this.findRow(this.streamingReasoningRowId);
    this.streamingReasoningRowId = null;
    if (row?.kind !== "reasoning") return [];
    return [{ op: "row.upserted", row: { ...row, state } }];
  }

  private closeStreamingRows(state: "complete" | "interrupted"): ConversationDelta[] {
    return [...this.closeTextRow(state), ...this.closeReasoningRow(state)];
  }

// Turn final state closes all foreground unfinalized tool rows (the late final state is not resurrected and is guaranteed by the isRunning gate);
// `only` makes stream recovery only recover the unfinalized part.
  private closeOpenToolRows(
    event: SessionEvent,
    status: "cancelled" | "error",
    only?: (row: ToolCallRow) => boolean,
  ): ConversationDelta[] {
    if (this.openForegroundToolCallIds.size === 0) return [];
    const openRows: ToolCallRow[] = [];
    for (const toolCallId of this.openForegroundToolCallIds) {
      const row = this.findToolRow(toolCallId);
// The derived index cannot become the second authoritative state; always review with the current snapshot row before closing.
      if (!row || !this.isOpenForegroundToolRow(row)) continue;
      if (only && !only(row)) continue;
      openRows.push(row);
    }
    if (openRows.length === 0) return [];
// Set may change the insertion order due to late reopening; rowId increases monotonically, and the old timeline delta order is maintained after sorting.
    if (openRows.length > 1) {
      openRows.sort((left, right) => left.rowId - right.rowId);
    }
    const deltas: ConversationDelta[] = [];
    const closedToolCallIds = new Set<string>();
    for (const row of openRows) {
      const next: ToolCallRow = {
        ...row,
        status,
        inputText: `${row.inputText ?? ""}${this.takePendingStreamingToolInput(row.toolCallId)}`,
        endedAt: this.ms(event),
      };
      delete next.approvalInteractionId;
      if (status === "error") {
// When the executor exits early or the event is missing, the old projection will only close the tool in the stop path;
// The success/error turn will leave the running status line. The cold snapshot is missing the header and is misjudged as thinking by the UI.
        next.error = {
          code: "fault.runtime.toolLifecycleIncomplete",
          message: "Tool call ended without a terminal event.",
        };
      } else {
        delete next.error;
      }
      closedToolCallIds.add(row.toolCallId);
      deltas.push({ op: "row.upserted", row: next });
    }

    const pendingInteractions = this.snapshot.pendingInteractions.filter(
      (interaction) =>
        !(
          (interaction.payload.kind === "permission" || interaction.payload.kind === "userInput") &&
          typeof interaction.payload.toolCallId === "string" &&
          closedToolCallIds.has(interaction.payload.toolCallId)
        ),
    );
    if (pendingInteractions.length !== this.snapshot.pendingInteractions.length) {
      deltas.push({ op: "state.updated", patch: { pendingInteractions } });
    }
    return deltas;
  }

  private isOpenForegroundToolRow(row: ToolCallRow): boolean {
    return (
      row.backgrounded !== true &&
      (row.status === "inputStreaming" ||
        row.status === "pendingApproval" ||
        row.status === "running")
    );
  }

  private updateToolIndexesAfterDeltas(deltas: readonly ConversationDelta[]): void {
    for (const delta of deltas) {
      if (delta.op === "row.appended" || delta.op === "row.upserted") {
        if (delta.row.kind !== "toolCall") continue;
        if (this.isOpenForegroundToolRow(delta.row)) {
          this.openForegroundToolCallIds.add(delta.row.toolCallId);
        } else {
          this.openForegroundToolCallIds.delete(delta.row.toolCallId);
        }
        continue;
      }
      if (delta.op !== "row.removed") continue;
      for (const [toolCallId, rowId] of this.toolRowIdByCallId) {
        if (rowId < delta.fromRowId) continue;
// Bug reason: rewind only deleted the rows/message index in the past, and the old toolCallId still blocked new branches.
// Reopen the streaming tool with the same ID; open tracker will also leave rows that no longer exist.
        this.toolRowIdByCallId.delete(toolCallId);
        this.openForegroundToolCallIds.delete(toolCallId);
        this.fileToolInputPreviewByCallId.delete(toolCallId);
      }
      this.pruneRemovedSubagentIndexes();
    }
  }

  private pruneRemovedSubagentIndexes(): void {
    for (const [agentId, rowId] of this.subagentRowIdByAgentId) {
      if (this.findRow(rowId)?.kind === "subagent") continue;
// Bug reason: rewind only rebuilds the rowIndex, and the old agent alias will still be used by each subsequent subagent
// materialization enumeration. Only clean once by authoritative snapshot after row.removed has been applied,
// Prevent long sessions from continuing to grow with deleted history; normal events will not scan the index.
      this.subagentRowIdByAgentId.delete(agentId);
    }
  }

// ── tool call state machine ──

  private openToolRow(
    event: SessionEvent,
    payload: CanonicalModelStream,
    entityId?: string,
  ): ConversationDelta[] {
    const toolCallId = String(payload.toolCallId ?? "");
    if (
      toolCallId === "" ||
      shouldHideInvalidToolCallFromProduct(payload.toolName) ||
      this.toolRowIdByCallId.has(toolCallId)
    ) {
      return [];
    }
    const row: ToolCallRow = {
      ...this.rowBase(event, this.turnIdOf(event), toolCallId),
      kind: "toolCall",
      ...(payload.assistantResponseId ? { assistantResponseId: payload.assistantResponseId } : {}),
      toolCallId,
      toolName: payload.toolName ?? "",
      status: "inputStreaming",
      inputText: "",
    };
    this.toolRowIdByCallId.set(toolCallId, row.rowId);
    if (isZCodeFileStreamingToolInputPreviewTool(row.toolName)) {
      this.fileToolInputPreviewByCallId.set(toolCallId, {
        lastPublishedAt: null,
        pendingAppend: "",
      });
    }
    if (entityId) this.entityIdByRowId.set(row.rowId, entityId);
    return [{ op: "row.appended", row }];
  }

  private appendStreamingToolInput(
    event: SessionEvent,
    payload: CanonicalModelStream,
  ): ConversationDelta[] {
    const toolCallId = String(payload.toolCallId ?? "");
    const rowId = this.toolRowIdByCallId.get(toolCallId);
    if (rowId === undefined) return [];
    const state = this.fileToolInputPreviewByCallId.get(toolCallId);
    if (!state) {
      return [{ op: "row.delta", rowId, path: "inputText", append: payload.delta }];
    }

    state.pendingAppend += payload.delta;
    const now = this.ms(event);
    if (
      state.lastPublishedAt !== null &&
      now - state.lastPublishedAt < ZCODE_FILE_STREAMING_TOOL_INPUT_PREVIEW_MIN_INTERVAL_MS
    ) {
      return [];
    }

    const append = state.pendingAppend;
    state.pendingAppend = "";
    state.lastPublishedAt = now;
    return append === "" ? [] : [{ op: "row.delta", rowId, path: "inputText", append }];
  }

  private flushStreamingToolInput(toolCallId: string): ConversationDelta[] {
    const append = this.takePendingStreamingToolInput(toolCallId);
    if (append === "") return [];
    const rowId = this.toolRowIdByCallId.get(toolCallId);
    return rowId === undefined ? [] : [{ op: "row.delta", rowId, path: "inputText", append }];
  }

  private takePendingStreamingToolInput(toolCallId: string): string {
    const state = this.fileToolInputPreviewByCallId.get(toolCallId);
    this.fileToolInputPreviewByCallId.delete(toolCallId);
    return state?.pendingAppend ?? "";
  }

  private finalizeStreamingToolInput(
    event: SessionEvent,
    payload: CanonicalModelStream,
  ): ConversationDelta[] {
    const toolCallId = String(payload.toolCallId ?? "");
    if (toolCallId === "") return [];
    this.fileToolInputPreviewByCallId.delete(toolCallId);
    if (shouldHideInvalidToolCallFromProduct(payload.toolName)) return [];
    const inputText = stringifyToolInput(payload.input);
    const existing = this.findToolRow(toolCallId);
    if (existing) {
      return [
        {
          op: "row.upserted",
          row: {
            ...existing,
            ...(payload.assistantResponseId
              ? { assistantResponseId: payload.assistantResponseId }
              : {}),
            toolName: existing.toolName || payload.toolName || "",
            inputText,
            input: payload.input,
          },
        },
      ];
    }

    const row: ToolCallRow = {
      ...this.rowBase(event, this.turnIdOf(event), toolCallId),
      kind: "toolCall",
      ...(payload.assistantResponseId ? { assistantResponseId: payload.assistantResponseId } : {}),
      toolCallId,
      toolName: payload.toolName ?? "",
      status: "inputStreaming",
      inputText,
      input: payload.input,
    };
    this.toolRowIdByCallId.set(toolCallId, row.rowId);
    return [{ op: "row.appended", row }];
  }

  private onToolCallScheduled(event: SessionEvent): ConversationDelta[] {
    if (this.isMirroredSubagentToolEvent(event) || !this.isRunning()) return [];
    const payload = event.payload as ToolCallScheduledPayload;
    const toolCallId = String(payload.toolCallId);
    this.fileToolInputPreviewByCallId.delete(toolCallId);
    if (shouldHideInvalidToolCallFromProduct(payload.toolName)) return [];
    const inputText = stringifyToolInput(payload.input);
    const cuaAction = readOfficialCuaAction(payload.toolName);
    const cuaApp =
      cuaAction && cuaAction !== "list_apps"
        ? resolveCuaAppIdentity(payload.input, this.latestListAppsSnapshot)
        : undefined;
    const existing = this.findToolRow(toolCallId);
    const planDeltas = this.todoPlanDeltas(
      event,
      extractPlanStepsFromToolInput({
        title: payload.toolName,
        kind: payload.toolName,
        input: payload.input,
      }),
    );
    if (existing) {
// replayable will filter row.delta(inputText), and the finalized upsert must carry the complete inputText.
// Otherwise, only the structured input can be seen after disconnection recovery, and the input text final state of v4 row is lost.
      return [
        {
          op: "row.upserted",
          row: {
            ...existing,
            ...(payload.assistantMessageId
              ? { assistantResponseId: String(payload.assistantMessageId) }
              : {}),
            inputText,
            input: payload.input,
            ...(cuaApp ? { cuaApp } : {}),
            ...(payload.display?.kind === "mcp_tool" ? { display: payload.display } : {}),
          },
        },
        ...planDeltas,
      ];
    }
    const row: ToolCallRow = {
      ...this.rowBase(event, this.turnIdOf(event), toolCallId),
      kind: "toolCall",
      ...(payload.assistantMessageId
        ? { assistantResponseId: String(payload.assistantMessageId) }
        : {}),
      toolCallId,
      toolName: payload.toolName,
      status: "inputStreaming",
      inputText,
      input: payload.input,
      ...(cuaApp ? { cuaApp } : {}),
      ...(payload.display?.kind === "mcp_tool" ? { display: payload.display } : {}),
    };
    this.toolRowIdByCallId.set(toolCallId, row.rowId);
    return [{ op: "row.appended", row }, ...planDeltas];
  }

  private onToolCallActivity(event: SessionEvent): ConversationDelta[] {
    if (this.isMirroredSubagentToolEvent(event) || !this.isRunning()) return [];
    const payload = event.payload as ToolCallStartedPayload;
    const row = this.findToolRow(String(payload.toolCallId));
    if (!row) return [];
    return projectToolActivity(event, row);
  }

  private onToolCallResult(event: SessionEvent): ConversationDelta[] {
    if (this.isMirroredSubagentToolEvent(event) || !this.isRunning()) return [];
    const payload = event.payload as ToolCallResultPayload;
    const toolCallId = String(payload.toolCallId);
    const row = this.findToolRow(toolCallId);
    if (!row) return [];
    const success = payload.result.success;
    if (success && readOfficialCuaAction(row.toolName) === "list_apps") {
// Digest identity must come from successful facts observed by the Agent; failure results cannot clear old snapshots.
      const snapshot = parseListAppsSnapshot(payload.result.content, payload.result.display);
      if (snapshot) this.latestListAppsSnapshot = snapshot;
    }
    const display = toProtocolToolCallDisplay(payload.result.display);
    const next: ToolCallRow = {
      ...row,
      status: success ? "success" : "error",
      output: buildToolOutput(payload.result, toolCallId),
      ...(display ? { display } : {}),
      endedAt: this.ms(event),
    };
    if (!success) {
      next.error = {
        code: payload.result.error?.type ?? "fault.runtime.toolFailed",
        message: payload.result.error?.message ?? "Tool execution failed.",
      };
    }
    const planDeltas = success
      ? this.todoPlanDeltas(
          event,
          extractPlanStepsFromToolOutput({
            title: row.toolName,
            kind: row.toolName,
            output: payload.result.content,
          }),
        )
      : [];
    return [{ op: "row.upserted", row: next }, ...planDeltas];
  }

  /**
* TodoWrite simultaneously projects the live plan and the current goal iteration.
* Before V4, only the tool row was retained, and the summary in the upper right corner could not reconstruct each round of action/status after live/cold recovery.
* The round is only advanced by the verifier boundary; TodoWrite only updates the currently open round and cannot add a round by itself.
   */
  private todoPlanDeltas(
    event: SessionEvent,
    steps: ReturnType<typeof extractPlanStepsFromToolInput>,
  ): ConversationDelta[] {
    if (!steps) return [];
    const items = steps.map((step, index) => ({
      id: step.id || `todo-${index + 1}`,
      content: step.title,
      status:
        step.status === "in_progress"
          ? ("inProgress" as const)
          : step.status === "completed"
            ? ("completed" as const)
            : ("pending" as const),
    }));
    const updatedAt = this.ms(event);
    const goal = this.snapshot.goal;
    if (!goal) {
      return [{ op: "state.updated", patch: { plan: { items, updatedAt } } }];
    }

    const iteration =
      goal.status === "verifying" || goal.status === "verified" || goal.status === "failed"
        ? Math.max(1, goal.iteration)
        : Math.max(1, goal.iteration + 1);
    const iterations = [
      ...goal.iterations.filter((entry) => entry.iteration !== iteration),
      { iteration, items, updatedAt },
    ].sort((left, right) => left.iteration - right.iteration);
    return [
      {
        op: "state.updated",
        patch: {
          goal: { ...goal, iterations },
          plan: { items, updatedAt },
        },
      },
    ];
  }

  private onToolCallError(event: SessionEvent): ConversationDelta[] {
    if (this.isMirroredSubagentToolEvent(event) || !this.isRunning()) return [];
    const payload = event.payload as ToolCallErrorPayload;
    const row = this.findToolRow(String(payload.toolCallId));
    if (!row) return [];
    const cancelled =
      payload.error.type === CoreErrorType.ToolCancelled || payload.error.code === "TOOL_CANCELLED";
    return [
      {
        op: "row.upserted",
        row: {
          ...row,
// Stop will generate tool_cancelled first, and then cancelled turn; if the tool is
//The final state is written as error, and the turn reducer that only closes the running row cannot be corrected to stopped.
          status: cancelled ? "cancelled" : "error",
          ...(cancelled
            ? { error: undefined }
            : { error: { code: payload.error.type, message: payload.error.message } }),
          endedAt: this.ms(event),
        },
      },
    ];
  }

//── Permission interaction (blocking interaction → status)──

  private onPermissionRequested(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as PermissionRequestedPayload;
    const toolCallId = String(payload.toolCallId);
    const interactionId = payload.requestId ?? `perm-${toolCallId}`;
    const interaction = this.createPendingInteractionFromPermissionEvent(
      event,
      payload,
      toolCallId,
      interactionId,
    );
    const deltas: ConversationDelta[] = [];
    const row = this.findToolRow(toolCallId);
    if (row) {
      deltas.push({
        op: "row.upserted",
        row: {
          ...row,
          status: "pendingApproval",
          approvalInteractionId: interactionId,
        },
      });
    }
    deltas.push({
      op: "state.updated",
      patch: {
        pendingInteractions: [...this.snapshot.pendingInteractions, interaction],
      },
    });
    return deltas;
  }

  private createPendingInteractionFromPermissionEvent(
    event: SessionEvent,
    payload: PermissionRequestedPayload,
    toolCallId: string,
    interactionId: string,
  ): PendingInteraction {
    if (isAskUserQuestionToolName(payload.toolName)) {
// The permission_requested of AskUserQuestion is just a runtime waiting state;
// v4 UI requires structured questions to backfill answers instead of Allow/Deny permission pop-ups.
      return {
        interactionId,
        kind: "userInput",
        anchorRowId: this.toolRowIdByCallId.get(toolCallId) ?? null,
        createdAt: this.ms(event),
        payload: {
          kind: "userInput",
          prompt: payload.reason,
          freeText: true,
          toolCallId,
          toolName: payload.toolName,
          traceId: event.traceId,
          input: payload.input,
          schema: { toolName: payload.toolName },
          questions: readAskUserQuestionPayloadQuestions(payload.input),
          ...(payload.origin ? { origin: payload.origin } : {}),
        },
      };
    }
    if (isExitPlanModeToolName(payload.toolName)) {
// ExitPlanMode reuses the userInput/elicitation channel to carry plan approval feedback;
// Ordinary permission payload cannot express the business semantics of approve/custom feedback.
      return {
        interactionId,
        kind: "userInput",
        anchorRowId: this.toolRowIdByCallId.get(toolCallId) ?? null,
        createdAt: this.ms(event),
        payload: {
          kind: "userInput",
          prompt: payload.reason,
          freeText: true,
          toolCallId,
          toolName: payload.toolName,
          traceId: event.traceId,
          input: payload.input,
          schema: { interaction: "plan_approval", toolName: payload.toolName },
          questions: [createExitPlanModeApprovalQuestion(payload.reason)],
          ...(payload.origin ? { origin: payload.origin } : {}),
        },
      };
    }
    const askDisplay = toProtocolToolCallDisplay(payload.display);
    return {
      interactionId,
      kind: "permission",
      anchorRowId: this.toolRowIdByCallId.get(toolCallId) ?? null,
      createdAt: this.ms(event),
      payload: {
        kind: "permission",
        toolCallId,
        toolName: payload.toolName,
        summary: payload.reason,
        detail: payload.input,
        freeText: true,
        ...(payload.fullAccessSupported === true && !payload.origin && !payload.optionsPolicy
          ? {
              fullAccessOption: {
                optionId: PERMISSION_FULL_ACCESS_OPTION_ID,
                label: "Full access",
                kind: "custom" as const,
                response: { decision: "deny" as const, reason: "Full access requires V4 approval" },
              },
            }
          : {}),
        ...(payload.origin ? { origin: payload.origin } : {}),
        ...(askDisplay ? { display: askDisplay } : {}),
        options: [
          ...buildProtocolPermissionOptions({
            input: payload.input,
            suggestedPermissionUpdates: payload.suggestedPermissionUpdates,
            ...(payload.optionsPolicy ? { optionsPolicy: payload.optionsPolicy } : {}),
            toolName: payload.toolName,
          }).map((option) => ({
            optionId:
              option.kind === "allow_once"
                ? "allowOnce"
                : option.kind === "allow_always"
                  ? "allowAlways"
                  : option.optionId,
            label: option.name,
// The confirmation-free kind of the session is mapped to the allowAlways in the closed set (the sorting slot/style is the same as always allow),
// optionId as is allowSession - broker relies on it for precise hits, GUI relies on name localization.
            kind:
              option.kind === "allow_once"
                ? ("allowOnce" as const)
                : option.kind === "allow_always" ||
                    option.kind === SESSION_ALLOW_PERMISSION_OPTION_KIND
                  ? ("allowAlways" as const)
                  : ("deny" as const),
            response: option.response,
          })),
// workflow Refine is only served in v4 (the legacy option list is intentionally not included, see session-mapper comments).
// Static response is a normal deny: anyone who doesn’t know the
// The consumption side of optionId (response without freeText) is degraded to rejection, and feedback upgrade only occurs in
//Interaction-broker's special treatment for freeText.
          ...(payload.toolName === CREATE_WORKFLOW_TOOL_NAME ||
          payload.toolName === AMEND_WORKFLOW_TOOL_NAME
            ? [
                {
                  optionId: WORKFLOW_REFINE_PERMISSION_OPTION_ID,
                  label: "Refine",
                  kind: "custom" as const,
                  response: { decision: "deny" as const, reason: "Denied" },
                },
              ]
            : []),
        ],
      },
    };
  }

  private onPermissionResolved(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as PermissionResolvedPayload;
    return this.settlePermission(
      String(payload.toolCallId),
      payload.decision === "deny" ? "cancelled" : "running",
    );
  }

  private onPermissionDenied(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as PermissionDeniedPayload;
    return this.settlePermission(String(payload.toolCallId), "cancelled");
  }

  private onWorkspaceHookReviewRequested(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as WorkspaceHookReviewRequestedPayload;
    const request = workspaceHookReviewRequestPayloadSchema.parse(payload.request);
    const current = this.snapshot.pendingInteractions.find(
      (item) => item.payload.kind === "workspaceHookReview",
    );
    if (current?.payload.kind === "workspaceHookReview") {
      const verdict = verdictWorkspaceHookReviewRequest(current.payload, request);
// Cross-flow can only take over after onSessionResumed has cleared the old review (epoch application policy is in
// onSessionResumed); other stale/replay/conflict shall not overwrite or extend the current authority.
      if (verdict !== "same_flow_advance") {
        return [];
      }
    }
    const interaction: PendingInteraction = {
      interactionId: request.interactionId,
      kind: "workspaceHookReview",
      anchorRowId: null,
      createdAt: request.createdAt,
      payload: request,
    };
// The higher generation of the same flow is the only legal replacement; cross-flow takeover for runtime restart must first go through
//SessionResumed clears the old authority. Atomic replacement is still used here to avoid multiple reviews remaining in historical abnormal states.
    const pendingInteractions = this.snapshot.pendingInteractions.filter(
      (item) => item.payload.kind !== "workspaceHookReview",
    );
    pendingInteractions.push(interaction);
    return [{ op: "state.updated", patch: { pendingInteractions } }];
  }

  private onWorkspaceHookReviewSettled(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as WorkspaceHookReviewSettledPayload;
    return this.removeWorkspaceHookReview(payload.interactionId);
  }

  private onWorkspaceHookReviewSuperseded(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as WorkspaceHookReviewSupersededPayload;
    return this.removeWorkspaceHookReview(payload.interactionId);
  }

  private removeWorkspaceHookReview(interactionId: string): ConversationDelta[] {
    const pendingInteractions = this.snapshot.pendingInteractions.filter(
      (item) =>
        !(item.payload.kind === "workspaceHookReview" && item.interactionId === interactionId),
    );
    return pendingInteractions.length === this.snapshot.pendingInteractions.length
      ? []
      : [{ op: "state.updated", patch: { pendingInteractions } }];
  }

  /**
* Soft access control: handle the WorkspaceHookAdmissionUpdated event.
   *
* pendingCount > 0 → write snapshot.workspaceHookAdmission (prompt bar appears);
* pendingCount === 0 → Set to null (the prompt bar disappears).
   */
  private onWorkspaceHookAdmissionUpdated(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as WorkspaceHookAdmissionUpdatedPayload;
    const workspaceHookAdmission =
      payload.pendingCount === 0
        ? null
        : {
            pendingCount: payload.pendingCount,
            bundleDigest: payload.bundleDigest,
            ...(payload.workspaceIdentity ? { workspaceIdentity: payload.workspaceIdentity } : {}),
          };
    return [{ op: "state.updated", patch: { workspaceHookAdmission } }];
  }

  private onUserInputAutoResolutionUpdated(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as UserInputAutoResolutionUpdatedPayload;
    let changed = false;
    const pendingInteractions = this.snapshot.pendingInteractions.map((interaction) => {
      if (
        interaction.interactionId !== payload.interactionId ||
        interaction.payload.kind !== "userInput"
      ) {
        return interaction;
      }
      changed = true;
      return {
        ...interaction,
        autoResolution: payload.autoResolution,
      };
    });
    return changed ? [{ op: "state.updated", patch: { pendingInteractions } }] : [];
  }

  private settlePermission(toolCallId: string, status: ToolCallRow["status"]): ConversationDelta[] {
    const deltas: ConversationDelta[] = [];
    const row = this.findToolRow(toolCallId);
    if (row) {
      const next: ToolCallRow = { ...row, status };
      delete next.approvalInteractionId;
      deltas.push({ op: "row.upserted", row: next });
    }
    const remaining = this.snapshot.pendingInteractions.filter(
      (item) =>
        !(
          (item.payload.kind === "permission" || item.payload.kind === "userInput") &&
          item.payload.toolCallId === toolCallId
        ),
    );
    if (remaining.length !== this.snapshot.pendingInteractions.length) {
      deltas.push({
        op: "state.updated",
        patch: { pendingInteractions: remaining },
      });
    }
    return deltas;
  }

//── turn-steer queue──

  private onTurnSteerQueued(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as TurnSteerQueuedPayload;
    const queueItemId = payload.intent?.queueItemId ?? payload.pendingInputId;
    const existingIndex = this.snapshot.queue.items.findIndex(
      (item) => item.queueItemId === queueItemId,
    );
    const existing = existingIndex >= 0 ? this.snapshot.queue.items[existingIndex] : undefined;
// The admittedDelivery of queued events can only be queue/guide. If you read the early or damaged events
// startNow must be based on the actual queue delivery, and the projection cannot be allowed to claim that the input has started immediately.
    const admittedDelivery: "queue" | "guide" =
      payload.intent?.admittedDelivery === "queue" || payload.intent?.admittedDelivery === "guide"
        ? payload.intent.admittedDelivery
        : (payload.delivery ??
          (existing?.delivery.admitted === "queue" || existing?.delivery.admitted === "guide"
            ? existing.delivery.admitted
            : this.snapshot.config.followupMode === "guide"
              ? "guide"
              : "queue"));
    const requestedDelivery =
      payload.intent?.requestedDelivery ?? existing?.delivery.requested ?? admittedDelivery;
    const fallbackReasonCode =
      payload.intent?.fallbackReasonCode ?? existing?.delivery.fallbackReasonCode;
    const nextItem: QueueItem = {
      queueItemId,
      kind:
        payload.intent?.kind === "compact" || payload.commandKind === "compact"
          ? ("compact" as const)
          : payload.intent?.kind === "sendGoalCommand" || payload.commandKind === "sendGoalCommand"
            ? ("sendGoalCommand" as const)
            : (existing?.kind ?? ("sendText" as const)),
      text: payload.input,
      sourceCommandId:
        payload.intent?.sourceCommandId ??
        existing?.sourceCommandId ??
        payload.inputId ??
        payload.pendingInputId,
      clientId: payload.intent?.clientId ?? existing?.clientId ?? "cli",
      attachments: payload.intent?.attachmentRefs ?? existing?.attachments ?? [],
// QueueItem is also an input to promotion execution, not just UI display; missing fields will cause the new Turn to inherit the old permissions/model.
// The old text editing event may have no intent, can only retain the same original fact, and cannot read the current Session complement value.
      modelSelection: payload.intent?.modelSelection ?? existing?.modelSelection,
      mode: payload.intent?.mode ?? existing?.mode,
      planEnabled: payload.intent?.planEnabled ?? existing?.planEnabled,
      sharedContextRefs: payload.intent?.sharedContextRefs ?? existing?.sharedContextRefs,
      provenance: payload.intent?.provenance ?? existing?.provenance,
      delivery: {
        requested: requestedDelivery,
        admitted: admittedDelivery,
        ...(fallbackReasonCode ? { fallbackReasonCode } : {}),
      },
      order: {
        admissionSeq:
          payload.intent?.admissionSeq ?? existing?.order.admissionSeq ?? event.sequenceNumber,
        queuePosition:
          payload.intent?.queuePosition ??
          existing?.order.queuePosition ??
          Math.max(0, (payload.queueLength ?? 1) - 1),
      },
      steer:
        !payload.intent && !payload.delivery && existing
          ? existing.steer
          : fallbackReasonCode
            ? { state: "fellBack", reasonCode: fallbackReasonCode }
            : admittedDelivery === "guide"
              ? { state: "steering" }
              : { state: "notRequested" },
      dispatch: { state: "queued" },
      ...(payload.toolDisallowlist ? { toolDisallowlist: [...payload.toolDisallowlist] } : {}),
      admittedAt: payload.intent?.admittedAt ?? existing?.admittedAt ?? this.ms(event),
    };
// Delivery semantic side table: When the payload does not contain (old runtime event), the current followupMode is used.
    this.deliveryByPendingInputId.set(payload.pendingInputId, admittedDelivery);
// Reentry with the same id = editQueueItem is updated in place (retained); new id = append. Old logic filter+append
// This will move the edit item to the end of the queue, destroying the positional semantics of queueContentIndependence.
    const items =
      existingIndex >= 0
        ? this.snapshot.queue.items.map((item, index) =>
            index === existingIndex ? nextItem : item,
          )
        : [...this.snapshot.queue.items, nextItem];
    return [
      {
        op: "state.updated",
        patch: this.queuePatch({ ...this.snapshot.queue, items }),
      },
    ];
  }

  private onTurnSteerDispatchChanged(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as TurnSteerDispatchChangedPayload;
    if (
      !this.snapshot.queue.items.some(
        (candidate) => candidate.queueItemId === payload.pendingInputId,
      )
    ) {
      return [];
    }
    const dispatch =
      payload.state === "queued"
        ? ({ state: "queued" } as const)
        : ({
            state: payload.state,
            reservationId: payload.reservationId,
          } as const);
    return [
      {
        op: "state.updated",
        patch: this.queuePatch({
          ...this.snapshot.queue,
          items: this.snapshot.queue.items.map((candidate) =>
            candidate.queueItemId === payload.pendingInputId
              ? { ...candidate, dispatch }
              : candidate,
          ),
        }),
      },
    ];
  }

  private onTurnSteerDeliveryChanged(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as TurnSteerDeliveryChangedPayload;
    const queueItemId = payload.intent?.queueItemId ?? payload.pendingInputId;
    if (!this.snapshot.queue.items.some((item) => item.queueItemId === queueItemId)) {
      return [];
    }
    this.deliveryByPendingInputId.set(payload.pendingInputId, payload.admittedDelivery);
    return [
      {
        op: "state.updated",
        patch: this.queuePatch({
          ...this.snapshot.queue,
          items: this.snapshot.queue.items.map((item) =>
            item.queueItemId === queueItemId
              ? {
                  ...item,
                  delivery: {
                    requested: payload.requestedDelivery,
                    admitted: payload.admittedDelivery,
                    fallbackReasonCode: payload.fallbackReasonCode,
                  },
                  steer: {
                    state: "fellBack",
                    reasonCode: payload.fallbackReasonCode,
                  },
                }
              : item,
          ),
        }),
      },
    ];
  }

  private onTurnSteerDrained(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as TurnSteerDrainedPayload;
    const runtimeTurnId = String(
      payload.targetTurnId ?? event.turnId ?? this.currentTurnId ?? "turn-unknown",
    );
// drain fact first comes with text/messageId (drainedInputs),
// Projection no longer relies on memory queue status to get text - the old implementation will silently continue if it cannot find the queue item.
// Even after the user input disappears from the queue, it will not be entered into the history. Old events (without drainedInputs) fallback to lookup tables.
    const items =
      payload.drainedInputs ??
      payload.pendingInputIds.flatMap((pendingInputId, index) => {
        const queueItem = this.snapshot.queue.items.find(
          (candidate) => candidate.queueItemId === pendingInputId,
        );
        if (!queueItem) return [];
        const intent: TurnInputIntentMetadata = {
          sourceCommandId: queueItem.sourceCommandId,
          queueItemId: queueItem.queueItemId,
          clientId: queueItem.clientId,
          kind: queueItem.kind,
          text: queueItem.text,
          ...(queueItem.modelSelection ? { modelSelection: queueItem.modelSelection } : {}),
          ...(queueItem.mode ? { mode: queueItem.mode } : {}),
          ...(queueItem.planEnabled !== undefined ? { planEnabled: queueItem.planEnabled } : {}),
          admissionSeq: queueItem.order.admissionSeq,
          admittedAt: queueItem.admittedAt,
          requestedDelivery: queueItem.delivery.requested,
          admittedDelivery: queueItem.delivery.admitted,
          queuePosition: queueItem.order.queuePosition,
          ...(queueItem.delivery.fallbackReasonCode
            ? { fallbackReasonCode: queueItem.delivery.fallbackReasonCode }
            : {}),
          attachmentRefs: queueItem.attachments,
        };
        return [
          {
            pendingInputId,
            messageId: payload.injectedMessageIds?.[index],
            text: queueItem.text,
            delivery: this.deliveryByPendingInputId.get(pendingInputId),
            intent,
          },
        ];
      });

    const deltas: ConversationDelta[] = [];
    for (const item of items) {
      const delivery =
        item.delivery ?? this.deliveryByPendingInputId.get(item.pendingInputId) ?? "queue";
// queue consumption = product turn boundary (one turn for each line: close the previous section
// header, open a new turnHeader, follow-up assistant returns to the new round); guide steer inlines the current round.
      if (delivery === "queue") {
        deltas.push(
          ...this.splitProductTurn(
            event,
            runtimeTurnId,
            item.messageId ? String(item.messageId) : undefined,
          ),
        );
      }
      const messageId = item.messageId ? String(item.messageId) : null;
      const entityId = messageId ?? item.pendingInputId;
      const productTurnId = this.turnIdOf(event);
      const rootSourceCommandId =
        item.intent?.provenance?.sourceCommandId ?? item.intent?.sourceCommandId;
      const row = {
        ...this.rowBase(event, productTurnId, entityId),
        kind: "userInput" as const,
        text: item.text,
        origin: "realUser" as const,
        ...(delivery === "guide" ? { guided: true as const } : {}),
        ...(item.intent?.sourceCommandId ? { sourceCommandId: item.intent.sourceCommandId } : {}),
        ...(rootSourceCommandId ? { rootSourceCommandId } : {}),
        ...(item.intent?.clientId ? { clientId: item.intent.clientId } : {}),
        ...(item.intent?.attachmentRefs?.length ? { attachments: item.intent.attachmentRefs } : {}),
      };
// The real-user row after queue/guide consumption shares the complete canonical target with the ordinary TurnStarted;
// Old events with missing messageId can still only be displayed, and unexecutable edit actions are not exposed.
      this.registerCanonicalUserRowTarget(
        row.rowId,
        entityId,
        messageId && item.intent?.kind !== "compact"
          ? {
              entityId,
              productTurnId,
              transcriptMessageId: messageId,
              coveredByStableCompact: false,
              intent: {
                kind: item.intent?.kind === "sendGoalCommand" ? "sendGoalCommand" : "sendText",
                text: item.intent?.text ?? item.text,
                ...(item.intent?.sourceCommandId
                  ? { sourceCommandId: item.intent.sourceCommandId }
                  : {}),
                ...(item.intent?.clientId ? { clientId: item.intent.clientId } : {}),
                ...(item.intent?.attachmentRefs ? { attachments: item.intent.attachmentRefs } : {}),
                ...(item.intent?.queueItemId ? { queueItemId: item.intent.queueItemId } : {}),
                ...(item.intent?.admissionSeq !== undefined
                  ? { admissionSeq: item.intent.admissionSeq }
                  : {}),
                ...(item.intent?.admittedAt !== undefined
                  ? { admittedAt: item.intent.admittedAt }
                  : {}),
                ...(item.intent?.requestedDelivery
                  ? { requestedDelivery: item.intent.requestedDelivery }
                  : {}),
                ...(item.intent?.admittedDelivery
                  ? { admittedDelivery: item.intent.admittedDelivery }
                  : {}),
                ...(item.intent?.fallbackReasonCode
                  ? { fallbackReasonCode: item.intent.fallbackReasonCode }
                  : {}),
                ...(item.intent?.modelSelection
                  ? { modelSelection: item.intent.modelSelection }
                  : {}),
                ...(item.intent?.mode ? { mode: item.intent.mode } : {}),
                ...(item.intent?.planEnabled !== undefined
                  ? { planEnabled: item.intent.planEnabled }
                  : {}),
                ...(item.intent?.provenance ? { provenance: item.intent.provenance } : {}),
              },
            }
          : undefined,
      );
      if (delivery === "guide") {
        deltas.push(...this.openGuidedWorkSegment(event, row.entityId ?? item.pendingInputId));
      }
      deltas.push({ op: "row.appended", row });
      this.deliveryByPendingInputId.delete(item.pendingInputId);
    }
    return [...deltas, ...this.removeQueueItems(payload.pendingInputIds)];
  }

  /**
* queue drain boundary = product turn boundary (within the same runtimeTurn).
* Close the header of the previous section of productTurn (working hours are split according to boundaries, sum = total working hours),
* Map runtimeTurnId → new productTurnId, open new turnHeader.
   */
  private splitProductTurn(
    event: SessionEvent,
    runtimeTurnId: string,
    promotedUserMessageId?: string,
  ): ConversationDelta[] {
    const deltas: ConversationDelta[] = [];
    const previousProductTurnId =
      this.productTurnIdByRuntimeTurnId.get(runtimeTurnId) ?? runtimeTurnId;
    const headerRowId = this.turnHeaderRowIdByTurnId.get(previousProductTurnId);
    const headerRow = headerRowId !== undefined ? this.findRow(headerRowId) : undefined;
    if (headerRow?.kind === "turnHeader") {
      const endedAt = this.ms(event);
      deltas.push({
        op: "row.upserted",
        row: {
          ...headerRow,
          state: "completedSuccess",
          endedAt,
          activeMs: Math.max(
            0,
            endedAt - (this.currentProductTurnStartedAtMs ?? headerRow.startedAt),
          ),
          ...(headerRow.workSegments
            ? {
                workSegments: this.completeWorkSegments(headerRow.workSegments, endedAt),
              }
            : {}),
        },
      });
    }
    const ordinal = (this.productTurnSplitOrdinalByRuntimeTurnId.get(runtimeTurnId) ?? 0) + 1;
    this.productTurnSplitOrdinalByRuntimeTurnId.set(runtimeTurnId, ordinal);
// The old implementation uses runtimeTurnId + ordinal in this process to create productTurnId;
// cold hydration will use hydrate-turn-N instead, and the identity of the same queue input cannot be maintained before and after restoration.
// promotion has generated a persistent user messageId, and new product turns must use this authoritative identity directly;
// The ordinal fallback is retained only if the legacy drain lacks messageId.
    const productTurnId = promotedUserMessageId ?? `${runtimeTurnId}~q${ordinal}`;
    this.productTurnIdByRuntimeTurnId.set(runtimeTurnId, productTurnId);
    this.runtimeTurnIdByProductTurnId.set(productTurnId, runtimeTurnId);
    this.currentProductTurnStartedAtMs = this.ms(event);
    const header = buildTurnHeaderRow(this.rowBase(event, productTurnId, productTurnId), {
      turnNumber: 0,
      input: "",
    });
    this.turnHeaderRowIdByTurnId.set(productTurnId, header.rowId);
    deltas.push({ op: "row.appended", row: header });
    return deltas;
  }

//The working hours are split according to the boundary: drain cuts through the runtimeTurn of the wheel, and the last segment of productTurn's working hours
// = The last boundary is completed, and the entire runtime duration is no longer used (otherwise the sum of the two periods is super real).
  private activeMsForCompletion(event: SessionEvent, runtimeDuration?: number): number | undefined {
    const runtimeTurnId = String(event.turnId ?? this.currentTurnId ?? "turn-unknown");
// Stable user messageId mapping does not mean that queue drain segmentation has occurred; only split ordinal
//The last working hour is calculated based on the boundary time only if it exists. Otherwise, the display timestamp span of cold synthetic events is very small,
// Will incorrectly overwrite transcript's calculated entire round duration.
    if (!this.productTurnSplitOrdinalByRuntimeTurnId.has(runtimeTurnId)) return runtimeDuration;
    if (this.currentProductTurnStartedAtMs === null) return runtimeDuration;
    return Math.max(0, this.ms(event) - this.currentProductTurnStartedAtMs);
  }

  private onTurnSteerDiscarded(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as TurnSteerDiscardedPayload;
    return this.removeQueueItems(payload.pendingInputIds);
  }

  private onSessionInputPromoted(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as SessionInputPromotedPayload;
// After sendQueuedNow starts successfully, explicitly TurnSteerDiscarded(promoted)
// Might be lost at process/link boundaries, leaving the UI permanently with promoting ghost items.
// SessionInputPromoted is only generated after user message + session_input is submitted with the same transaction.
// Therefore it is a durable commit signal that can safely remove the queue projection.
    return this.removeQueueItems([payload.pendingInputId]);
  }

/** v4 queue rearrangement: rearrange queue rows by orderedPendingInputIds (unlisted items are appended in relative order). */
  private onTurnSteerReordered(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as { orderedPendingInputIds?: string[] };
    const order = payload.orderedPendingInputIds ?? [];
    const byId = new Map(this.snapshot.queue.items.map((item) => [item.queueItemId, item]));
    const ordered = order
      .map((id) => byId.get(id))
      .filter((item): item is (typeof this.snapshot.queue.items)[number] => item !== undefined);
// Items that do not appear in order (anti-lost) are appended to maintain the original relative order.
    const orderedIds = new Set(order);
    const rest = this.snapshot.queue.items.filter((item) => !orderedIds.has(item.queueItemId));
    const reordered = [...ordered, ...rest];
    const items = reordered.map((item, index) =>
      item.order.queuePosition === index
        ? item
        : { ...item, order: { ...item.order, queuePosition: index } },
    );
// If there is no change in the order, delta will not be generated (idempotent).
    if (
      items.length === this.snapshot.queue.items.length &&
      items.every((item, index) => item === this.snapshot.queue.items[index])
    ) {
      return [];
    }
    return [
      {
        op: "state.updated",
        patch: this.queuePatch({ ...this.snapshot.queue, items }),
      },
    ];
  }

// setAutoDrain: queue.autoDrain authorization bit flips. autoDrain affects held derivation
// (heldQueueInputRequiresChoice) and area A availability → use queuePatch to recalculate uniformly.
  private onQueueAutoDrainChanged(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as { autoDrain?: boolean };
    const autoDrain = payload.autoDrain ?? true;
    if (
      this.snapshot.queue.autoDrain === autoDrain &&
      (autoDrain || this.snapshot.queue.pauseReason === "manual")
    ) {
      return [];
    }
    const queue = { ...this.snapshot.queue, autoDrain };
    if (autoDrain) {
      delete queue.pauseReason;
    } else {
      queue.pauseReason = "manual";
    }
    return [
      {
        op: "state.updated",
        patch: this.queuePatch(queue),
      },
    ];
  }

// setFollowupMode: config.followupMode flip. followupMode is running when
// The route authorization bit of enqueue vs guide (computeInputRouting) → synchronously recalculate area A after changing the config.
  private onFollowupModeChanged(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as { mode?: "queue" | "guide" };
    const mode: "queue" | "guide" = payload.mode === "guide" ? "guide" : "queue";
    if (this.snapshot.config.followupMode === mode) return [];
    const nextConfig: ConversationSnapshot["config"] = {
      ...this.snapshot.config,
      followupMode: mode,
    };
    const context = this.deriveContext({});
    return [
      {
        op: "state.updated",
        patch: {
          config: nextConfig,
          availability: computeAvailability(context),
          inputRouting: computeInputRouting(context, mode),
        },
      },
    ];
  }

  /**
   * switchCollaborationMode：SessionModeChanged → config.mode。
* The event source covers the command plane (source=command) and the plan tool path (enterPlanMode/exitPlanMode,
* source=tool) - the two paths share this projection, so the UI's mode selector can also follow the tool-driven mode switch.
   */
  private onSessionModeChanged(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as {
      mode?: string;
      planEnabled?: boolean;
      source?: string;
      toolCallId?: string;
      permissionGrant?: { interactionId: string; queueItemIds: string[] };
    };
    const mode = typeof payload.mode === "string" ? payload.mode : "";
// After the log event touches mode, the seed is no longer covered (the same value return is also considered a touch - the log has an authoritative value).
    if (mode) this.configModeTouchedByEvent = true;
    if (!mode) return [];
    const planEnabled = payload.planEnabled ?? mode === "plan";
    const planTransition =
      payload.source === "tool" && payload.toolCallId
        ? { toolCallId: payload.toolCallId, planEnabled }
        : this.snapshot.config.planTransition;
    if (
      this.snapshot.config.mode === mode &&
      this.snapshot.config.planEnabled === planEnabled &&
      planTransition === this.snapshot.config.planTransition &&
      !payload.permissionGrant
    )
      return [];
    return [
      {
        op: "state.updated",
        patch: {
          ...(payload.permissionGrant
            ? this.queuePatch({
                ...this.snapshot.queue,
                items: this.snapshot.queue.items.map((item) =>
                  payload.permissionGrant!.queueItemIds.includes(item.queueItemId)
                    ? { ...item, mode: "yolo" as const }
                    : item,
                ),
              })
            : {}),
          config: {
            ...this.snapshot.config,
            mode,
            planEnabled,
            planTransition,
            ...(payload.permissionGrant
              ? { permissionGrant: { interactionId: payload.permissionGrant.interactionId } }
              : {}),
          },
        },
      },
    ];
  }

  /**
* Subagent row materializes within the same event transaction as the summary projection.
* The old UI checks session/subagents after spawn; if the query happens to fall within 7 concurrent children
* Before the last session is persisted, it will be cached permanently 6, and will not be checked again until the session is switched. now renderer
* Only consume the complete state submitted with row here, no more event/query dual clocks.
   */
  private shouldMaterializeSubagentProjection(reduced: readonly ConversationDelta[]): boolean {
//The root cause of the performance problem: cold hydration once caused unrelated events such as checkpoint to scan all historical rows.
// Here only the real input of the materializer is admitted in the batch accumulator that has not yet been released; live and
// strict fallback still follows the original path, and the store seed of authoritative manifest written directly after batch will not be affected.
    if (!this.hydrationAccumulator) return true;

    for (const delta of reduced) {
      switch (delta.op) {
        case "row.appended":
        case "row.upserted":
          if (delta.row.kind === "subagent" || this.findRow(delta.row.rowId)?.kind === "subagent") {
            return true;
          }
          break;
        case "row.delta":
          if (this.findRow(delta.rowId)?.kind === "subagent") return true;
          break;
        case "row.removed":
// Suffix deletion may also remove the subagent row; rows are not pre-scanned again for determination.
          return true;
        case "state.updated":
          if (
            delta.patch.pendingInteractions !== undefined ||
            delta.patch.backgroundWorks !== undefined
          ) {
            return true;
          }
          break;
        default: {
          const exhaustiveDelta: never = delta;
          return exhaustiveDelta;
        }
      }
    }
    return false;
  }

  private materializeSubagentProjection(
    reduced: readonly ConversationDelta[],
  ): ConversationDelta[] {
    const previous: SubagentProjectionState = this.snapshot.subagents ?? {
      revision: 0,
      childSessionIds: [],
      running: [],
      endedTotal: 0,
    };
    const previousRunningById = new Map(
      previous.running.map((item) => [item.childSessionId, item]),
    );
    const latestRowByChildId = new Map<string, SubagentRow>();
    const collectSubagentRow = (row: SubagentRow | null): void => {
      if (row?.childSessionId && !this.invalidSubagentChildSessionIds.has(row.childSessionId)) {
        latestRowByChildId.set(row.childSessionId, row);
      }
    };
// Root cause of performance issue: Old implementation copies the full rows array before scanning all normal conversation rows.
// subagentRowIdByAgentId is already the derived index used by the reducer to search; here we remove duplicates by rowId and pass
// rowIndexById restores the current timeline order; invalid aliases will be cleaned up after row.removed is applied.
    const currentRowIds = new Set(this.subagentRowIdByAgentId.values());
    for (const delta of reduced) {
      if (delta.op === "row.upserted") {
        if (delta.row.kind === "subagent" || this.findRow(delta.row.rowId)?.kind === "subagent") {
          currentRowIds.add(delta.row.rowId);
        }
      } else if (delta.op === "row.delta" && this.findRow(delta.rowId)?.kind === "subagent") {
        currentRowIds.add(delta.rowId);
      }
    }
    const currentRows: Array<{ rowIndex: number; row: ConversationRow }> = [];
    for (const rowId of currentRowIds) {
      const rowIndex = this.rowIndexById.get(rowId);
      const row = rowIndex === undefined ? undefined : this.snapshot.rows.window[rowIndex];
      if (rowIndex !== undefined && row !== undefined) currentRows.push({ rowIndex, row });
    }
    currentRows.sort((left, right) => left.rowIndex - right.rowIndex);
    for (const { row } of currentRows) {
      collectSubagentRow(this.prospectiveSubagentRow(row, reduced, 0));
    }
    for (let index = 0; index < reduced.length; index += 1) {
      const delta = reduced[index]!;
      if (delta.op !== "row.appended") continue;
      collectSubagentRow(this.prospectiveSubagentRow(delta.row, reduced, index + 1));
    }

    let pendingInteractions = this.snapshot.pendingInteractions;
    let backgroundWorks = this.snapshot.backgroundWorks;
    for (const delta of reduced) {
      if (delta.op !== "state.updated") continue;
      if (delta.patch.pendingInteractions !== undefined) {
        pendingInteractions = delta.patch.pendingInteractions;
      }
      if (delta.patch.backgroundWorks !== undefined) {
        backgroundWorks = delta.patch.backgroundWorks;
      }
    }
    const waitingChildIds = new Set<string>();
// The new hook review interaction payload of workspace-hook-trust does not have an origin field, and the guard is skipped to avoid misreading.
    for (const interaction of pendingInteractions) {
      if (!("origin" in interaction.payload)) continue;
      const origin = interaction.payload.origin;
      if (origin?.kind === "subagent") waitingChildIds.add(origin.childSessionId);
    }
    const blockedChildIds = new Set(
      backgroundWorks.flatMap((work) =>
        work.kind === "subagent" && work.status === "running" && work.blocked && work.childSessionId
          ? [work.childSessionId]
          : [],
      ),
    );

    const childSessionIds = [...latestRowByChildId.keys()];
    const running: RunningSubagentSummary[] = [];
    for (const [childSessionId, row] of latestRowByChildId) {
      if (row.status !== "running") continue;
      const previousItem = previousRunningById.get(childSessionId);
      const title = previousItem?.title || row.summaryText.trim() || row.subagentType;
      running.push({
        childSessionId,
        agentId: row.entityId,
        ...(row.parentToolCallId ? { toolCallId: row.parentToolCallId } : {}),
        subagentType: row.subagentType,
        title,
        status: waitingChildIds.has(childSessionId)
          ? "waiting"
          : blockedChildIds.has(childSessionId)
            ? "blocked"
            : "running",
        ...(row.startedAt !== undefined ? { startedAt: row.startedAt } : {}),
      });
    }
    running.sort(
      (left, right) =>
        (right.startedAt ?? 0) - (left.startedAt ?? 0) ||
        right.childSessionId.localeCompare(left.childSessionId),
    );
    const endedTotal = childSessionIds.length - running.length;
    const semanticState = { childSessionIds, running, endedTotal };
    if (
      JSON.stringify(semanticState) ===
      JSON.stringify({
        childSessionIds: previous.childSessionIds,
        running: previous.running,
        endedTotal: previous.endedTotal,
      })
    ) {
      return [];
    }
    return [
      {
        op: "state.updated",
        patch: {
          subagents: {
            revision: previous.revision + 1,
            ...semanticState,
          },
        },
      },
    ];
  }

  private prospectiveSubagentRow(
    row: ConversationRow,
    reduced: readonly ConversationDelta[],
    startIndex: number,
  ): SubagentRow | null {
    let prospective: ConversationRow | null = row;
    for (let index = startIndex; index < reduced.length && prospective; index += 1) {
      const delta = reduced[index]!;
      switch (delta.op) {
        case "row.appended":
        case "state.updated":
          break;
        case "row.upserted":
          if (delta.row.rowId === prospective.rowId) prospective = delta.row;
          break;
        case "row.delta":
          if (
            delta.rowId === prospective.rowId &&
            delta.path === "summaryText" &&
            prospective.kind === "subagent"
          ) {
            prospective = {
              ...prospective,
              summaryText: prospective.summaryText + delta.append,
            };
          }
          break;
        case "row.removed":
          if (prospective.rowId >= delta.fromRowId) prospective = null;
          break;
        default: {
          const exhaustiveDelta: never = delta;
          return exhaustiveDelta;
        }
      }
    }
    return prospective?.kind === "subagent" ? prospective : null;
  }

//── subagent row mirror──
// The schema/UI already has a subagent row, but the old reducer does not consume Subagent* events;
// cold hydration Even synthetic events cannot restore drill down rows. Here let live/cold share the same state machine.

  private onSubagentSpawned(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as Record<string, unknown>;
    const agentId = this.subagentAgentId(payload, event);
    const existing = this.findSubagentLifecycleRow(agentId, payload, event);
    if (existing) this.subagentRowIdByAgentId.set(agentId, existing.rowId);
    const childSessionId = this.stringPayload(payload, "childSessionId");
    const parentToolCallId = this.stringPayload(payload, "parentToolCallId");
    const resumedBackgroundWork = this.resumedSubagentBackgroundWorkDelta(
      event,
      payload,
      agentId,
      childSessionId,
    );
    if (
      existing?.status === "running" &&
      (!childSessionId || childSessionId === existing.childSessionId) &&
      (!parentToolCallId || parentToolCallId === existing.parentToolCallId) &&
      (payload.background !== true || existing.backgrounded === true)
    ) {
      return resumedBackgroundWork ? [resumedBackgroundWork] : [];
    }
    if (existing) {
      const row: SubagentRow = {
        ...existing,
        status: "running",
        summaryText:
          this.stringPayload(payload, "description") ??
          this.stringPayload(payload, "prompt") ??
          existing.summaryText,
// The resume event carries SendMessage call id, but parentToolCallId is the creation anchor point of row;
// Existing anchors cannot be overwritten as lifecycle fields, otherwise the UI can no longer be associated with the original Agent row.
        ...(!existing.parentToolCallId && parentToolCallId ? { parentToolCallId } : {}),
        ...(childSessionId ? { childSessionId } : {}),
        ...(payload.background === true ? { backgrounded: true as const } : {}),
        ...(payload.background === true ? { workId: agentId } : {}),
        startedAt: this.ms(event),
      };
      delete row.endedAt;
      return [
        { op: "row.upserted", row },
        ...(resumedBackgroundWork ? [resumedBackgroundWork] : []),
      ];
    }
    const row: SubagentRow = {
      ...this.rowBase(event, this.turnIdOf(event), agentId),
      kind: "subagent",
      ...(this.stringPayload(payload, "parentToolCallId")
        ? { parentToolCallId: this.stringPayload(payload, "parentToolCallId") }
        : {}),
      subagentType: this.stringPayload(payload, "agentType") ?? "subagent",
      status: "running",
      summaryText:
        this.stringPayload(payload, "description") ??
        this.stringPayload(payload, "summaryText") ??
        this.stringPayload(payload, "prompt") ??
        "",
      ...(this.stringPayload(payload, "childSessionId")
        ? { childSessionId: this.stringPayload(payload, "childSessionId") }
        : {}),
      ...(payload.background === true ? { backgrounded: true as const } : {}),
      ...(payload.background === true ? { workId: agentId } : {}),
      startedAt: this.ms(event),
    };
    this.subagentRowIdByAgentId.set(agentId, row.rowId);
    return [{ op: "row.appended", row }, ...(resumedBackgroundWork ? [resumedBackgroundWork] : [])];
  }

  private resumedSubagentBackgroundWorkDelta(
    event: SessionEvent,
    payload: Record<string, unknown>,
    agentId: string,
    childSessionId: string | undefined,
  ): ConversationDelta | undefined {
    if (payload.background !== true || payload.resumed !== true || !childSessionId) {
      return undefined;
    }

// SendMessage resume directly enters the subagent port without going through the Agent tool executor.
// Therefore, the tracker's BackgroundTaskStarted will not be generated. SubagentSpawned is already a single startup fact,
// Here, the work can be canceled within the same V4 transaction to avoid introducing a second failable event.
    const previous = this.snapshot.backgroundWorks;
    const existing = previous.find((work) => work.workId === agentId);
    const title =
      this.stringPayload(payload, "description") ??
      this.stringPayload(payload, "prompt") ??
      existing?.title ??
      agentId;
    if (
      existing?.status === "running" &&
      existing.kind === "subagent" &&
      existing.title === title &&
      existing.childSessionId === childSessionId &&
      existing.cancellable === true
    ) {
      return undefined;
    }
    const next: BackgroundWorkSummary = {
      workId: agentId,
      kind: "subagent",
      title,
      status: "running",
      startedAt: this.ms(event),
      cancellable: true,
      anchorRowId: existing?.anchorRowId ?? null,
      childSessionId,
    };
    const backgroundWorks = existing
      ? previous.map((work) => (work.workId === agentId ? next : work))
      : [...previous, next];
    return { op: "state.updated", patch: { backgroundWorks } };
  }

  private onSubagentMessage(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as Record<string, unknown>;
    const row = this.findSubagentRow(this.subagentAgentId(payload, event));
    const append =
      this.stringPayload(payload, "summaryText") ??
      this.stringPayload(payload, "text") ??
      this.stringPayload(payload, "message");
    if (!row || !append) return [];
    return [{ op: "row.delta", rowId: row.rowId, path: "summaryText", append }];
  }

  private onSubagentStopped(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as Record<string, unknown>;
    const agentId = this.subagentAgentId(payload, event);
    const existing = this.findSubagentLifecycleRow(agentId, payload, event);
    const parentToolCallId = this.stringPayload(payload, "parentToolCallId");
    const status = this.mapSubagentStatus(this.stringPayload(payload, "status"));
    const summaryText =
      this.stringPayload(payload, "summaryText") ??
      this.stringPayload(payload, "result") ??
      this.stringPayload(payload, "error") ??
      this.stringPayload(payload, "description") ??
      existing?.summaryText ??
      "";
    const row: SubagentRow = existing
      ? {
          ...existing,
          status,
          summaryText,
          endedAt: this.ms(event),
//The final state of the resumed child also belongs to the original Agent row, and is only filled in when the old row is missing anchor points.
          ...(!existing.parentToolCallId && parentToolCallId ? { parentToolCallId } : {}),
          ...(this.stringPayload(payload, "childSessionId")
            ? { childSessionId: this.stringPayload(payload, "childSessionId") }
            : {}),
        }
      : {
          ...this.rowBase(event, this.turnIdOf(event), agentId),
          kind: "subagent",
          ...(this.stringPayload(payload, "parentToolCallId")
            ? {
                parentToolCallId: this.stringPayload(payload, "parentToolCallId"),
              }
            : {}),
          subagentType: this.stringPayload(payload, "agentType") ?? "subagent",
          status,
          summaryText,
          ...(this.stringPayload(payload, "childSessionId")
            ? { childSessionId: this.stringPayload(payload, "childSessionId") }
            : {}),
          ...(payload.background === true ? { backgrounded: true as const } : {}),
          ...(payload.background === true ? { workId: agentId } : {}),
          endedAt: this.ms(event),
        };
    this.subagentRowIdByAgentId.set(agentId, row.rowId);
    return [{ op: existing ? "row.upserted" : "row.appended", row }];
  }

// cancelBackgroundWork: background task life cycle (BackgroundTaskStarted/Updated/Completed)
// → Maintain snapshot.backgroundWorks (the background work surface reads its rendering + cancel entry).
// taskId≡workId does not require translation; status is normalized to a 4-value closed enumeration of summary.
  private onBackgroundTaskLifecycle(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as {
      taskId?: string;
      toolName?: string;
      taskKind?: string;
      command?: string;
      description?: string;
      status?: string;
      cancellable?: boolean;
      blocked?: boolean;
      childSessionId?: string;
    };
    const workId = payload.taskId;
    if (!workId) return [];
    const prev = this.snapshot.backgroundWorks;
    const existing = prev.find((work) => work.workId === workId);
    const legacyKind = resolveZCodeBackgroundTaskControlKind(payload);
// New events use the explicit taskKind of the runtime; old events use the shared resolver.
// Agent/Task/subagent string branches can no longer be scattered within reducers.
// "workflow" is workflow run (previously mislabeled as bash); there is no corresponding value in the legacy resolver because
// legacy `Workflow` tools are deliberately still bash--the two are different things, and sharing classes will mix panels together.
    const kind: BackgroundWorkSummary["kind"] =
      payload.taskKind === "subagent"
        ? "subagent"
        : payload.taskKind === "bash"
          ? "bash"
          : payload.taskKind === "workflow"
            ? "workflow"
            : legacyKind === "agent"
              ? "subagent"
              : legacyKind === "bash"
                ? "bash"
                : (existing?.kind ?? "bash");
// event status (running/completed/failed/timed_out/cancelled/spawn_error/lost)
    // → summary status（running/resultPending/failed/cancelled）。
    const rawStatus = payload.status ?? "running";
    const status: "running" | "resultPending" | "failed" | "cancelled" =
      rawStatus === "running"
        ? "running"
        : rawStatus === "cancelled"
          ? "cancelled"
          : rawStatus === "completed"
            ? "resultPending"
            : "failed";
    const title =
      payload.description?.trim() ||
      payload.command?.trim() ||
      existing?.title ||
      payload.toolName ||
      workId;
    const next: BackgroundWorkSummary = {
      workId,
      kind,
      title,
      status,
      startedAt: existing?.startedAt ?? this.ms(event),
      ...(status === "running" ? {} : { endedAt: this.ms(event) }),
      ...(typeof payload.cancellable === "boolean"
        ? { cancellable: payload.cancellable }
        : existing?.cancellable !== undefined
          ? { cancellable: existing.cancellable }
          : {}),
      ...(typeof payload.blocked === "boolean"
        ? { blocked: payload.blocked }
        : existing?.blocked !== undefined
          ? { blocked: existing.blocked }
          : {}),
      anchorRowId: existing?.anchorRowId ?? null,
      ...(payload.childSessionId
        ? { childSessionId: payload.childSessionId }
        : existing?.childSessionId
          ? { childSessionId: existing.childSessionId }
          : {}),
    };
// Idempotent: the content does not change and no delta is generated.
    if (
      existing &&
      existing.status === next.status &&
      existing.title === next.title &&
      existing.kind === next.kind &&
      existing.cancellable === next.cancellable &&
      existing.blocked === next.blocked &&
      existing.childSessionId === next.childSessionId
    ) {
      return [];
    }
    const backgroundWorks = existing
      ? prev.map((work) => (work.workId === workId ? next : work))
      : [...prev, next];
    return [{ op: "state.updated", patch: { backgroundWorks } }];
  }

// ── dwf real-time running state: DynamicWorkflowRunProgress → workflowRuns status key ──
// An engine RunEvent and a session event are reduced to the authoritative state of key-level overall replacement. Go for reducer instead of side channel,
// So durable, replayable, cold recovery free (precedent: subagents key).
  //
// The reduction ontology is in the workflow-runs-reducer of @zcode/shared (cohabiting with the state schema): TUI mirroring is required
// For the same reduction, write one copy in two places, which is two clocks.
// All that's left here is the impure part of the projection - taking the payload from the event envelope and sending the new state as state.updated.
  private onDynamicWorkflowRunProgress(event: SessionEvent): ConversationDelta[] {
// First convert the bounded payload of contracts, and then assign it to the structured input parameter of shared: this line of assignment means "the shape of both sides does not drift"
// The compile-time gate (shared cannot rely on contracts in reverse, so the input parameter type can only be defined structurally).
    const envelope: WorkflowRunProgressEnvelope =
      event.payload as DynamicWorkflowRunProgressPayload;
    const workflowRuns = reduceWorkflowRunsState(this.snapshot.workflowRuns, envelope);
// null = no change in semantics (invalid event or replay of the same event): no delta is generated, revision is not raised.
    if (workflowRuns === null) return [];
    return [{ op: "state.updated", patch: { workflowRuns } }];
  }

  private removeQueueItems(ids: readonly string[]): ConversationDelta[] {
    const idSet = new Set(ids);
    for (const id of ids) this.deliveryByPendingInputId.delete(id);
    const items = this.snapshot.queue.items
      .filter((item) => !idSet.has(item.queueItemId))
      .map((item, index) =>
        item.order.queuePosition === index
          ? item
          : { ...item, order: { ...item.order, queuePosition: index } },
      );
    if (items.length === this.snapshot.queue.items.length) return [];
    return [
      {
        op: "state.updated",
        patch: this.queuePatch({ ...this.snapshot.queue, items }),
      },
    ];
  }

  // ── config / usage ──

  private onModelSelected(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as ModelSelectedPayload;
// Bug reason: After saving the fresh child identity as pending side state, atomic projection clone/adopt
// If this field is not copied, the real-time marker will disappear. Explicit null directly reuses the model baseline expression ∅→X,
// Public projection no longer recognizes Subagent identity; subsequent selection only updates config and does not overwrite the actual model of the previous round.
    if (payload.previousModelSelection === null) {
      this.lastTurnModel = { kind: "sourceLess" };
    }
    const prev = this.snapshot.config;
    const provider = payload.modelSelection.providerId;
    const model = payload.modelSelection.modelId;
    const thought =
      payload.effectiveReasoningLevel ?? payload.modelSelection.options?.reasoningLevel ?? "";
    const modelSelection = cloneSparseModelSelection(payload.modelSelection);
    const thoughtLevels = payload.supportedThoughtLevels
      ? [...payload.supportedThoughtLevels]
      : prev.thoughtLevels;
    const contextWindow =
      payload.contextWindow === null
        ? null
        : payload.contextWindow !== undefined
          ? positiveInteger(payload.contextWindow, 0) || undefined
          : undefined;
// Bug reason: The old event only updates config. Although the runtime has been switched to the new model, the maxTokens of historical usage
// Still stuck at the source model until the next ModelComplete without accidental calibration. The window belongs to the applied model capability,
// Must be submitted in the same ModelSelected; usedTokens still retain historical context facts.
    if (contextWindow !== undefined) {
      this.contextWindowState.touchedByEvent = true;
      this.contextWindowState.maxTokens =
        contextWindow !== null && contextWindow > 0 ? contextWindow : null;
    }
    const previousContextWindow = this.snapshot.usage.contextWindow;
    if (previousContextWindow) {
      this.contextWindowState.usedTokens = previousContextWindow.usedTokens;
    }
    const contextWindowChanged =
      contextWindow !== undefined &&
      (contextWindow === null
        ? previousContextWindow !== null
        : previousContextWindow === null || previousContextWindow.maxTokens !== contextWindow);
// After the log event touches the model selection, the seed is no longer covered (the same value return is also counted as a touch).
// Cold recovery of the synthesized ModelSelected(HYDRATION_TRACE_ID) exception: it's just from the message fact
// Rebuild historical selection for use by modelChange marker, not an authoritative selection action; seedConfig after replay
// Still ends with the runtime true value (resume the last/draft selection that has been written back).
    if (String(event.traceId) !== HYDRATION_TRACE_ID) {
      this.configModelTouchedByEvent = true;
      if (payload.supportedThoughtLevels !== undefined) {
        this.configThoughtLevelsTouchedByEvent = true;
      }
    }
// The selection event only updates config and does not drop modelChange during selection.
// marker——The switching action is the intention, and the marker returns to onTurnStarted by "different from the actual selection in the previous round"
// Verdict (see notes and bug background there).
    const configChanged = !(
      prev.provider === provider &&
      prev.model === model &&
      sameSparseModelSelection(prev.modelSelection, modelSelection) &&
      prev.thought === thought &&
      prev.thoughtLevels.length === thoughtLevels.length &&
      prev.thoughtLevels.every((value, index) => value === thoughtLevels[index])
    );
    const modelTransition =
      payload.origin === "registryFallback" &&
      payload.previousModelSelection != null &&
      (payload.previousModelSelection.providerId !== provider ||
        payload.previousModelSelection.modelId !== model)
        ? {
            eventId: String(event.id),
            origin: payload.origin,
            from: {
              provider: payload.previousModelSelection.providerId,
              model: payload.previousModelSelection.modelId,
            },
            to: { provider, model },
          }
        : undefined;
    if (!configChanged && !contextWindowChanged && modelTransition === undefined) {
      return [];
    }
    return [
      {
        op: "state.updated",
        patch: {
          ...(configChanged
            ? { config: { ...prev, modelSelection, provider, model, thought, thoughtLevels } }
            : {}),
// Bug reason: only projecting config will lose the source of "triggered by registry fallback",
// The renderer cannot safely distinguish between automatic recovery and explicit/history switching. Keep event IDs and start and end identities,
// Concrete toasts are still only triggered by the client at the real-time online delivery boundary.
          ...(modelTransition ? { modelTransition } : {}),
          ...(contextWindowChanged
            ? {
                usage: {
                  ...this.snapshot.usage,
// Bug reason: null is the authoritative event for clearing the explicit window in the registry, and the entire
// usage.contextWindow; if the field is missing, the old event compatible semantics will be retained.
                  contextWindow:
                    contextWindow === null
                      ? null
                      : previousContextWindow
                        ? { ...previousContextWindow, maxTokens: contextWindow }
                        : {
                            usedTokens: this.contextWindowState.usedTokens,
                            maxTokens: contextWindow,
                            autoCompactThresholdTokens: null,
                          },
                },
              }
            : {}),
        },
      },
    ];
  }

  private onModelComplete(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as ModelCompletePayload;
    const retryClearDeltas = this.acceptsActiveModelEvent(event) ? this.setApiRetry(null) : [];
// Same ruling as the old reducer: only the main session round trip can override the context water level.
    const isMainTurn =
      payload.querySource !== undefined
        ? payload.querySource === "main_turn"
        : payload.stopReason !== "tool_internal";
    if (isMainTurn) this.outputContinuationTextRowId = null;
    if (
      isMainTurn &&
      payload.stopReason?.trim().toLowerCase() === "length" &&
      payload.toolCallCount === 0
    ) {
      const lastVisibleRow = this.snapshot.rows.window.at(-1);
      if (
        lastVisibleRow?.kind === "assistantText" &&
        lastVisibleRow.turnId === this.turnIdOf(event) &&
        lastVisibleRow.state === "complete"
      ) {
        this.outputContinuationTextRowId = lastVisibleRow.rowId;
      }
    }
// The usage of subagent ModelComplete is still not the main session water level, but it carries
// fileChanges is the child session's own workspace fact and must be independently projected to the child turn header.
    const supportsFileChangeSummary = isMainTurn || payload.querySource === "subagent";
    const deltas: ConversationDelta[] = [];
    if (supportsFileChangeSummary && payload.fileChanges && payload.fileChanges.files > 0) {
      const turnId = this.turnIdOf(event);
      const headerRowId = this.turnHeaderRowIdByTurnId.get(turnId);
      const headerRow = headerRowId !== undefined ? this.findRow(headerRowId) : undefined;
      if (headerRow?.kind === "turnHeader") {
        deltas.push({
          op: "row.upserted",
          row: {
            ...headerRow,
            fileChanges: {
              additions: payload.fileChanges.additions,
              deletions: payload.fileChanges.deletions,
              files: payload.fileChanges.files,
              state: "active",
            },
          },
        });
      }
    }
    if (!isMainTurn) return [...deltas, ...retryClearDeltas];
    const usage = payload.usage as ModelUsage;
    const usedTokens = getModelUsageContextTokens(usage) ?? 0;
    this.contextWindowState.usedTokens = usedTokens;
    const maxTokens = payload.contextWindow ?? this.contextWindowState.maxTokens;
    const cumulative = this.snapshot.usage.cumulative;
    deltas.push({
      op: "state.updated",
      patch: {
        usage: {
// Bug reason: ModelComplete of contextWindow is missing when the registry has explicitly cleared the window
// In the past, the object would be reconstructed with 0, breaking unknown capacity semantics. The token continues to be updated in the side state and cumulative value.
          contextWindow:
            maxTokens === null
              ? null
              : {
                  usedTokens,
                  maxTokens,
                  autoCompactThresholdTokens:
                    this.snapshot.usage.contextWindow?.autoCompactThresholdTokens ?? null,
                  ...(payload.cacheHit ? { cache: payload.cacheHit } : {}),
                  ...(payload.contextUsageBreakdown && payload.contextUsageBreakdown.length > 0
                    ? { breakdown: payload.contextUsageBreakdown }
                    : {}),
                },
          cumulative: {
            inputTokens: cumulative.inputTokens + (usage.inputTokens ?? 0),
            outputTokens: cumulative.outputTokens + (usage.outputTokens ?? 0),
            cacheReadTokens: cumulative.cacheReadTokens + (usage.cacheReadTokens ?? 0),
            cacheWriteTokens: cumulative.cacheWriteTokens + (usage.cacheWriteTokens ?? 0),
          },
        },
      },
    });
// ModelComplete is a guarantee of success when the network completed event is missing, and the retry prompt cannot be suspended.
    deltas.push(...retryClearDeltas);
    return deltas;
  }

//── compact marker (compact command effect)──
// The same operationId occupies the same marker row throughout the life cycle: running → success/failed/noop/cancelled.
// Attribution: The marker falls at the end of the line when the event arrives, and the client has zero attribution logic.

  private onCompactLifecycle(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as CompactLifecyclePayload & {
      anchorMessageId?: string;
      tailStartMessageId?: string;
    };
    const existingRowId = this.compactMarkerRowIdByOperationId.get(payload.operationId);
    const existingRow = existingRowId !== undefined ? this.findRow(existingRowId) : undefined;
    const prev =
      existingRow?.kind === "timelineMarker" && existingRow.marker.type === "compact"
        ? existingRow.marker
        : undefined;

    const status = mapCompactMarkerStatus(payload.status);
    if (status === "success") {
      const coverageMessageId = payload.tailStartMessageId ?? payload.anchorMessageId;
      const coverageRowId = coverageMessageId ? this.rowIdForMessageId(coverageMessageId) : null;
      if (coverageRowId !== null) {
        this.stableCompactCoverageBoundaryRowId = Math.max(
          this.stableCompactCoverageBoundaryRowId ?? 0,
          coverageRowId,
        );
        for (const [rowId, entityId] of this.entityIdByRowId) {
          if (rowId > this.stableCompactCoverageBoundaryRowId) continue;
          const target = this.editTargetByEntityId.get(entityId);
          if (target && !target.coveredByStableCompact) {
            this.editTargetByEntityId.set(entityId, {
              ...target,
              coveredByStableCompact: true,
            });
          }
        }
      }
    }
    const tokensAfter =
      payload.truePostCompactTokenCount ?? payload.postCompactTokenCount ?? prev?.tokensAfter;
    const marker: TimelineMarkerPayload = {
      type: "compact",
      origin: prev?.origin ?? mapCompactMarkerOrigin(payload.trigger),
      status,
// Only final events carry token count; the known value is retained during upsert (retry is not cleared).
      ...(payload.preCompactTokenCount !== undefined || prev?.tokensBefore !== undefined
        ? { tokensBefore: payload.preCompactTokenCount ?? prev?.tokensBefore }
        : {}),
      ...(tokensAfter !== undefined ? { tokensAfter } : {}),
// Pull the full text of summary by ref (same as toolOutput/get); use summaryMessageId as placeholder.
      ...(payload.summaryMessageId !== undefined || prev?.summaryRef
        ? {
            summaryRef:
              payload.summaryMessageId !== undefined
                ? String(payload.summaryMessageId)
                : prev?.summaryRef,
          }
        : {}),
    };

    const deltas: ConversationDelta[] = [];
    if (existingRow?.kind === "timelineMarker") {
      deltas.push({
        op: "row.upserted",
        row: {
          ...existingRow,
          marker,
          ...(payload.sourceCommandId ? { sourceCommandId: payload.sourceCommandId } : {}),
        },
      });
    } else {
      const row: TimelineMarkerRow = {
        ...this.rowBase(event, this.turnIdOf(event), String(payload.operationId)),
        kind: "timelineMarker",
        lane: "assistantWork",
        marker,
        ...(payload.sourceCommandId ? { sourceCommandId: payload.sourceCommandId } : {}),
      };
      this.compactMarkerRowIdByOperationId.set(payload.operationId, row.rowId);
      deltas.push({ op: "row.appended", row });
    }

// compacting in and out of activeWorks (guard origin derived: compactOperationLock /
// compactingAcceptsFutureInput driven by this, aligned with formal-proof evaluateCompacting).
    const otherWorks = this.snapshot.control.activeWorks.filter((work) => work.kind !== "compact");
    if (status === "running") {
      deltas.push({
        op: "state.updated",
        patch: this.controlPatch({
          activeWorks: [...otherWorks, { kind: "compact", startedAt: this.ms(event) }],
          canStop: true,
          stopState: "stoppable",
          stopTargetKind: otherWorks.length > 0 ? "mixed" : "compact",
        }),
      });
    } else {
      deltas.push({
        op: "state.updated",
        patch: this.controlPatch({
          activeWorks: otherWorks,
          ...(otherWorks.length === 0
            ? {
                canStop: false,
                stopState: "idle" as const,
                stopTargetKind: "unknown" as const,
              }
            : {}),
        }),
      });
    }

// compact successful → context water level drops immediately (usage.contextWindow updated).
    if (status === "success" && tokensAfter !== undefined) {
      this.contextWindowState.usedTokens = tokensAfter;
      const maxTokens =
        this.snapshot.usage.contextWindow?.maxTokens ?? this.contextWindowState.maxTokens;
      deltas.push({
        op: "state.updated",
        patch: {
          usage: {
            ...this.snapshot.usage,
            contextWindow:
              maxTokens === null
                ? null
                : {
                    usedTokens: tokensAfter,
                    maxTokens,
                    autoCompactThresholdTokens:
                      this.snapshot.usage.contextWindow?.autoCompactThresholdTokens ?? null,
                  },
          },
        },
      });
    }
    return deltas;
  }

// ── goal state machine──

  private onTargetChanged(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as TargetChangedPayload;
    switch (payload.action) {
      case "set": {
        if (!payload.target) return [];
// New target: iteration/verifications zeroed.
// goalSet is stateOnly - no timeline row (old implementation
// The goalSet marker is an invisible line "into window rendering null", polluting the turn grouping decision),
// Goal display belongs to goal panel/status area.
        const goal: GoalState = {
          targetId: payload.target.targetID,
          objective: payload.target.objective,
          summaryTitle: payload.target.summaryTitle,
          timeUsedSeconds: payload.target.timeUsedSeconds,
          activeRunStartedAtMs: payload.target.activeRunStartedAtMs ?? null,
          status: mapGoalStatus(payload.target.status),
          iteration: 0,
          verifications: [],
          iterations: [],
        };
        return [{ op: "state.updated", patch: this.goalPatch(goal) }];
      }
      case "cleared": {
        if (!this.snapshot.goal) return [];
        return [{ op: "state.updated", patch: this.goalPatch(null) }];
      }
      default: {
        // status_updated / run_started / run_finished / usage_accounted / summary_updated：
// Synchronize refresh timing and summary title. The old implementation only compares status, which will take more than 1 second to run accounting.
//Update with summaryTitle, resulting in UI inconsistency before and after refresh.
        const goal = this.snapshot.goal;
        if (!goal || !payload.target) return [];
        const nextGoal: GoalState = {
          ...goal,
          targetId: payload.target.targetID,
          objective: payload.target.objective,
          summaryTitle: payload.target.summaryTitle,
          timeUsedSeconds: payload.target.timeUsedSeconds,
          activeRunStartedAtMs: payload.target.activeRunStartedAtMs ?? null,
          status: mapGoalStatus(payload.target.status),
        };
        if (
          nextGoal.targetId === goal.targetId &&
          nextGoal.objective === goal.objective &&
          nextGoal.summaryTitle === goal.summaryTitle &&
          nextGoal.timeUsedSeconds === goal.timeUsedSeconds &&
          nextGoal.activeRunStartedAtMs === goal.activeRunStartedAtMs &&
          nextGoal.status === goal.status
        ) {
          return [];
        }
        return [{ op: "state.updated", patch: this.goalPatch(nextGoal) }];
      }
    }
  }

  private onTargetVerification(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as TargetCompletionVerificationPayload;
    const goal = this.snapshot.goal;
// goal verify boundary does not rely on goal state being present.
// The cold recovery synthetic event stream does not have TargetChanged → goal is null, and the old implementation is discarded here.
// verification fact, goalVerify marker disappears after refresh. now marker
// Constantly generated/constantly updated; the goal status patch still only takes effect when the goal is present.

    if (payload.status === "started") {
      const iteration = payload.goalIteration ?? (goal ? goal.iteration + 1 : 1);
      const lifecycleKey = this.goalVerifyLifecycleKey(payload, iteration);
      const verifyingGoal = goal ? { ...goal, status: "verifying" as const, iteration } : undefined;
      const otherWorks = this.snapshot.control.activeWorks.filter(
        (work) => work.kind !== "goalVerifier",
      );
      const controlDelta: ConversationDelta = {
        op: "state.updated",
        patch: this.controlPatch(
          {
            phase: "running",
            sessionEnded: false,
            activeWorks: [
              ...otherWorks,
              {
                kind: "goalVerifier",
                ...(payload.foregroundExecutionId
                  ? { foregroundExecutionId: payload.foregroundExecutionId }
                  : {}),
                startedAt: this.ms(event),
              },
            ],
            canStop: true,
            stopState: "stoppable",
            stopTargetKind: otherWorks.length > 0 ? ("mixed" as const) : ("goalVerifier" as const),
            lastError: null,
            apiRetry: null,
          },
          verifyingGoal,
        ),
      };
// GV-identity: Retry with targetId+iteration (new verificationId) to reuse the same marker
//The line returns to running without growing a second marker.
      const existingRowId = this.goalVerifyMarkerRowIdByLifecycleKey.get(lifecycleKey);
      const existingRow = existingRowId !== undefined ? this.findRow(existingRowId) : undefined;
      if (existingRow?.kind === "timelineMarker") {
        return [
          {
            op: "row.upserted",
            row: {
              ...existingRow,
              marker: { type: "goalVerify", iteration, outcome: "running" },
            },
          },
          controlDelta,
        ];
      }
      const row: TimelineMarkerRow = {
        ...this.rowBase(event, this.goalVerifyTurnId(payload, event), lifecycleKey),
        kind: "timelineMarker",
        lane: "turnTailBoundary",
        marker: { type: "goalVerify", iteration, outcome: "running" },
      };
      this.goalVerifyMarkerRowIdByLifecycleKey.set(lifecycleKey, row.rowId);
      return [{ op: "row.appended", row }, controlDelta];
    }

// Final state: completed (pass/notSatisfied is a valid conclusion) / failed_closed (the verification process failed)
// / canceled (stopped: the process did not produce a conclusion → marker=failed(detail=cancelled), goal returns to paused).
    const iteration = payload.goalIteration ?? goal?.iteration ?? 1;
    const outcome: "pass" | "notSatisfied" | "failed" =
      payload.status === "completed"
        ? payload.verification?.passed
          ? "pass"
          : "notSatisfied"
        : "failed";
    const goalStatus: GoalState["status"] =
      payload.status === "cancelled"
        ? "paused"
        : payload.status === "failed_closed"
          ? "failed"
          : outcome === "pass"
            ? "verified"
            : "notSatisfied";

    const deltas: ConversationDelta[] = [];
    const lifecycleKey = this.goalVerifyLifecycleKey(payload, iteration);
    const markerRowId = this.goalVerifyMarkerRowIdByLifecycleKey.get(lifecycleKey);
    let anchorRowId: number | null = null;
    const markerRow = markerRowId !== undefined ? this.findRow(markerRowId) : undefined;
    const terminalMarker: TimelineMarkerPayload = {
      type: "goalVerify",
      iteration,
      outcome,
      ...(payload.status === "cancelled"
        ? { detail: "cancelled" }
        : payload.verification?.reason
          ? { detail: payload.verification.reason }
          : {}),
    };
    if (markerRow?.kind === "timelineMarker") {
      anchorRowId = markerRow.rowId;
      deltas.push({
        op: "row.upserted",
        row: { ...markerRow, marker: terminalMarker },
      });
    } else {
// GV-terminal-only: boundary press lifecycleKey upsert - any life cycle
// The entity can be created if the event arrives first. In the old implementation, if the started marker cannot be found in the final state, the entire marker is discarded (cold recovery
// The final state reached later, the started event and frame loss are all triggered).
      const row: TimelineMarkerRow = {
        ...this.rowBase(event, this.goalVerifyTurnId(payload, event), lifecycleKey),
        kind: "timelineMarker",
        lane: "turnTailBoundary",
        marker: terminalMarker,
      };
      this.goalVerifyMarkerRowIdByLifecycleKey.set(lifecycleKey, row.rowId);
      anchorRowId = row.rowId;
      deltas.push({ op: "row.appended", row });
    }

    const hadGoalVerifierWork = this.snapshot.control.activeWorks.some(
      (work) => work.kind === "goalVerifier",
    );
    const shouldPatchControl = hadGoalVerifierWork || this.snapshot.goal?.status === "verifying";
    const otherWorks = this.snapshot.control.activeWorks.filter(
      (work) => work.kind !== "goalVerifier",
    );
    const terminalPhase: SessionControl["phase"] =
      payload.status === "cancelled"
        ? "completedInterrupted"
        : payload.status === "failed_closed"
          ? "error"
          : "completedSuccess";
    const heldQueue =
      payload.status === "cancelled" &&
      payload.preserveQueueAutoDrainOnCancel !== true &&
      this.snapshot.queue.items.length > 0
        ? {
            ...this.snapshot.queue,
            autoDrain: false,
            pauseReason: "stopped" as const,
          }
        : undefined;

// goal is not present (cold recovery synthetic stream): the marker line must still be retained; only the current live control
// Close control only when verifier work is really in progress, to avoid terminal-only historical facts turning the draft into
// The cold recovery snapshot was mistakenly pushed to completed.
    if (!goal) {
      if (!shouldPatchControl) return deltas;
      deltas.push({
        op: "state.updated",
        patch: this.controlPatch(
          {
            phase: terminalPhase,
            sessionEnded: terminalPhase !== "error",
            activeWorks: otherWorks,
            ...(otherWorks.length === 0
              ? {
                  canStop: false,
                  stopState: "idle" as const,
                  stopTargetKind: "unknown" as const,
                }
              : {
                  stopTargetKind: "mixed" as const,
                }),
          },
          undefined,
          heldQueue,
        ),
      });
      return deltas;
    }

// verifications only record conclusions (cancelled is not a conclusion and is not included in the summary); the most recent N items.
    const verifications =
      payload.status === "cancelled"
        ? goal.verifications
        : [
            ...goal.verifications,
            {
              iteration,
              outcome,
              at: this.ms(event),
              anchorRowId,
              ...(payload.verification?.reason ? { reason: payload.verification.reason } : {}),
              ...(payload.verification?.nextAction
                ? { nextAction: payload.verification.nextAction }
                : {}),
            },
          ].slice(-PROTOCOL_V4_LIMITS.goalVerificationsRetained);

    const nextGoal = {
      ...goal,
      status: goalStatus,
      iteration,
      verifications,
    };
    deltas.push({
      op: "state.updated",
      patch: shouldPatchControl
        ? this.controlPatch(
            {
              phase: terminalPhase,
              sessionEnded: terminalPhase !== "error",
              activeWorks: otherWorks,
              ...(otherWorks.length === 0
                ? {
                    canStop: false,
                    stopState: "idle" as const,
                    stopTargetKind: "unknown" as const,
                  }
                : {
                    stopTargetKind: "mixed" as const,
                  }),
            },
            nextGoal,
            heldQueue,
          )
        : this.goalPatch(nextGoal),
    });
    return deltas;
  }

/** goal verify boundary identity: targetId_goalIteration. */
  private goalVerifyLifecycleKey(
    payload: TargetCompletionVerificationPayload,
    iteration: number,
  ): string {
    return payload.targetId ? `${payload.targetId}_${iteration}` : payload.verificationId;
  }

// Positioning: priority anchorAssistantMessageId (resolved to rendered
// The row's ownership wheel - the fork copy is followed by the remapped child local id); the second choice is anchorTurnId
// (Must be a known round, unknown id cannot be used as turnId - otherwise ghost turn grouping will appear,
//The parent runtime turnId before fork is typical); finally it is attributed by event.
  private goalVerifyTurnId(
    payload: TargetCompletionVerificationPayload,
    event: SessionEvent,
  ): string {
    const anchorMessageId = payload.anchorAssistantMessageId
      ? String(payload.anchorAssistantMessageId)
      : null;
    if (anchorMessageId) {
      const rowId = this.rowIdForMessageId(anchorMessageId);
      const row = rowId !== null ? this.findRow(rowId) : undefined;
      if (row) return row.turnId;
    }
    const anchorTurnId = payload.anchorTurnId ? String(payload.anchorTurnId) : null;
    if (anchorTurnId) {
      const mapped = this.productTurnIdByRuntimeTurnId.get(anchorTurnId) ?? anchorTurnId;
      if (this.turnHeaderRowIdByTurnId.has(mapped)) return mapped;
    }
    return this.turnIdOf(event);
  }

//── fork marker (forkAssistant command effect)──

  private onSessionForked(event: SessionEvent): ConversationDelta[] {
    const payload = event.payload as SessionForkedPayload;
    const isParent = String(payload.originalSessionId) === this.snapshot.sessionId;
    if (isParent) {
// The parent timeline does not show forkCreated - the fork relationship is only in sessions
// Tree/list representation. The old implementation uses nextRowId-1 to approximate the anchor point to generate rows, and the UI is rendered as null
// (Invisible rows pollute the turn group); the child header forkNotice remains unchanged.
      return [];
    }
// child header forkNotice (forkTimelineIsBoundary): event payload does not carry
// The rowId on the parent side is first occupied by 0; the transcript anchor point → rowId mapping is completed with the transmission shell.
    const row: TimelineMarkerRow = {
      ...this.rowBase(
        event,
        this.turnIdOf(event),
        `fork:${String(payload.originalSessionId)}:${String(payload.targetMessageId ?? "unknown")}`,
      ),
      kind: "timelineMarker",
      lane: "turnTailBoundary",
      marker: {
        type: "forkNotice",
        parentSessionId: String(payload.originalSessionId),
        parentRowId: 0,
      },
    };
    return [{ op: "row.appended", row }];
  }

// ── Internal tools ──

// Pass goal undefined = don't move goal; pass null/object = replace with this patch (availability derived from the same source).
//queue passes undefined = unchanged queue; held derived (heldQueueInputRequiresChoice) dependency
// queue.items.length + autoDrain, so any control/goal/queue changes recalculate area A from the same place.
  private controlPatch(
    control: Partial<SessionControl>,
    goal?: GoalState | null,
    queue?: ConversationSnapshot["queue"],
  ): StatePatch {
    const next: SessionControl = { ...this.snapshot.control, ...control };
    const nextGoal = goal === undefined ? this.snapshot.goal : goal;
    const nextQueue = queue ?? this.snapshot.queue;
    const context = {
      phase: next.phase,
      goalStatus: nextGoal?.status ?? null,
// compacting is not an independent phase (closed enumeration) and is derived from activeWorks.
      compacting: next.activeWorks.some((work) => work.kind === "compact"),
      goalVerifying: next.activeWorks.some((work) => work.kind === "goalVerifier"),
      queueLength: nextQueue.items.length,
      autoDrain: nextQueue.autoDrain,
    };
    return {
      control: next,
      ...(goal === undefined ? {} : { goal }),
      ...(queue === undefined ? {} : { queue }),
      availability: computeAvailability(context),
      inputRouting: computeInputRouting(context, this.snapshot.config.followupMode),
    };
  }

//Patch when goal changes alone (availability has the same origin as goal, phase/activeWorks remains unchanged).
  private goalPatch(goal: GoalState | null): StatePatch {
    return {
      goal,
      availability: computeAvailability(this.deriveContext({ goal })),
    };
  }

//Patch when queue changes alone: ​​queue length/autoDrain affects held derivation → synchronous recalculation of area A.
  private queuePatch(queue: ConversationSnapshot["queue"]): StatePatch {
    const context = this.deriveContext({ queue });
    return {
      queue,
      availability: computeAvailability(context),
      inputRouting: computeInputRouting(context, this.snapshot.config.followupMode),
    };
  }

  private deriveContext(overrides: {
    goal?: GoalState | null;
    queue?: ConversationSnapshot["queue"];
  }) {
    const goal = overrides.goal === undefined ? this.snapshot.goal : overrides.goal;
    const queue = overrides.queue ?? this.snapshot.queue;
    return {
      phase: this.snapshot.control.phase,
      goalStatus: goal?.status ?? null,
      compacting: this.snapshot.control.activeWorks.some((work) => work.kind === "compact"),
      goalVerifying: this.snapshot.control.activeWorks.some((work) => work.kind === "goalVerifier"),
      queueLength: queue.items.length,
      autoDrain: queue.autoDrain,
    };
  }

  private upsertTurnHeader(
    event: SessionEvent,
    state: "completedSuccess" | "completedInterrupted" | "failed",
    activeMs?: number,
    historyRoundCount?: number,
  ): ConversationDelta[] {
    const row = this.turnHeaderForEvent(event);
    if (!row) return [];
    const endedAt = this.ms(event);
    return [
      {
        op: "row.upserted",
        row: {
          ...row,
          state,
          endedAt,
          ...(activeMs !== undefined ? { activeMs } : {}),
          ...(historyRoundCount !== undefined ? { historyRoundCount } : {}),
          ...(row.workSegments
            ? {
                workSegments: this.completeWorkSegments(row.workSegments, endedAt),
              }
            : {}),
        },
      },
    ];
  }

  private openGuidedWorkSegment(event: SessionEvent, triggerEntityId: string): ConversationDelta[] {
    const row = this.turnHeaderForEvent(event);
    if (!row || row.executionKind === "controlOnly") return [];
    const startedAt = this.ms(event);
    const existingSegments: TurnWorkSegment[] = row.workSegments ?? [
      {
        segmentId: `${row.turnId}:initial`,
        startedAt: row.startedAt,
      },
    ];
// The old UI only maintains one folded state for the entire product turn, and the accepted guide can only
// Inserted as a normal row, independent workspace cannot be restored. Segment boundaries must be recorded by the CLI, React cannot guess by adjacent rows.
    const workSegments = [
      ...this.completeWorkSegments(existingSegments, startedAt),
      {
        segmentId: triggerEntityId,
        triggerEntityId,
        startedAt,
      },
    ];
    return [{ op: "row.upserted", row: { ...row, workSegments } }];
  }

  private completeWorkSegments(
    segments: readonly TurnWorkSegment[],
    endedAt: number,
  ): TurnWorkSegment[] {
    return segments.map((segment, index) =>
      index === segments.length - 1 && segment.endedAt === undefined
        ? {
            ...segment,
            endedAt,
            activeMs: Math.max(0, endedAt - segment.startedAt),
          }
        : segment,
    );
  }

  private turnHeaderForEvent(event: SessionEvent): TurnHeaderRow | undefined {
    const rowId = this.turnHeaderRowIdByTurnId.get(this.turnIdOf(event));
    if (rowId === undefined) return undefined;
    const row = this.findRow(rowId);
    return row?.kind === "turnHeader" ? row : undefined;
  }

  private markStableForkAssistant(event: SessionEvent): ConversationDelta[] {
    const turnId = this.turnIdOf(event);
    const rows = this.snapshot.rows.window;
    const headerRowId = this.turnHeaderRowIdByTurnId.get(turnId);
    const headerIndex = headerRowId === undefined ? undefined : this.rowIndexById.get(headerRowId);
    const startIndex = headerIndex === undefined ? 0 : headerIndex + 1;
    let row: AssistantTextRow | undefined;
// Root cause of performance problem: The old implementation copies and reverses the complete history rows for each successful turn, and cold recovery will accumulate
// Approximately O(turns * rows) allocation and scan. The current turn's line will only appear after its own header.
    for (let index = rows.length - 1; index >= startIndex; index -= 1) {
      const candidate = rows[index];
      if (candidate?.kind !== "assistantText" || candidate.turnId !== turnId) continue;
      row = candidate;
      break;
    }
    if (!row || !this.messageIdByRowId.has(row.rowId)) return [];
    return [
      {
        op: "row.upserted",
        row: {
          ...row,
          state: "complete",
          actions: { ...row.actions, canFork: true },
        },
      },
    ];
  }

  private isRunning(): boolean {
    const phase = this.snapshot.control.phase;
    return phase === "running" || phase === "prewarming";
  }

  private isMirroredSubagentToolEvent(event: SessionEvent): boolean {
    const payload = event.payload as unknown as Record<string, unknown>;
// Bug reason: The child tool lifecycle will be mirrored to the parent runtime, but it is not the tool fact of the parent session.
// V4 used to treat mirror as a normal ToolCallRow, causing the main timeline to display the child's Read/Bash.
// And let the replayable snapshot also carry dirty rows. The complete tool history should only be materialized by child topics.
    return payload.source === "subagent";
  }

  private openAssistantSegments(): Partial<
    Record<"text" | "reasoning", CanonicalOpenSegmentIdentity>
  > {
    const segments: Partial<Record<"text" | "reasoning", CanonicalOpenSegmentIdentity>> = {};
    const text = this.openSegmentIdentity(this.streamingTextRowId);
    const reasoning = this.openSegmentIdentity(this.streamingReasoningRowId);
    if (text) segments.text = text;
    if (reasoning) segments.reasoning = reasoning;
    return segments;
  }

  private openSegmentIdentity(rowId: number | null): CanonicalOpenSegmentIdentity | null {
    if (rowId === null) return null;
    const entityId = this.entityIdByRowId.get(rowId);
    if (!entityId) return null;
    return {
      entityId,
      transcriptMessageId: this.messageIdByRowId.get(rowId) ?? null,
    };
  }

  private rowBase(event: SessionEvent, turnId: string, entityId = String(event.id)) {
    const rowId = this.nextRowId++;
    this.entityIdByRowId.set(rowId, entityId);
    return {
      rowId,
      turnId,
      entityId,
      productTurnId: turnId,
      visibility: "visible" as const,
      createdAt: this.ms(event),
      createdAtSeq: event.sequenceNumber,
    };
  }

  private turnIdOf(event: SessionEvent): string {
    const runtimeTurnId = String(event.turnId ?? this.currentTurnId ?? "turn-unknown");
// After queue drain, the subsequent event lines of the same runtimeTurn are classified into the latest productTurn.
    return this.productTurnIdByRuntimeTurnId.get(runtimeTurnId) ?? runtimeTurnId;
  }

  private ms(event: SessionEvent): number {
    return event.timestamp.getTime();
  }

  private findRow(rowId: number): ConversationRow | undefined {
    const index = this.rowIndexById.get(rowId);
    return index === undefined ? undefined : this.snapshot.rows.window[index];
  }

  private updateRowIndexAfterImmutableApply(
    previousRowsLength: number,
    deltas: readonly ConversationDelta[],
  ): void {
    if (deltas.some((delta) => delta.op === "row.removed")) {
      this.rowIndexById = new Map(
        this.snapshot.rows.window.map((row, index) => [row.rowId, index]),
      );
      return;
    }
    let nextIndex = previousRowsLength;
    for (const delta of deltas) {
      if (delta.op !== "row.appended") continue;
      this.rowIndexById.set(delta.row.rowId, nextIndex);
      nextIndex += 1;
    }
  }

  private findToolRow(toolCallId: string): ToolCallRow | undefined {
    const rowId = this.toolRowIdByCallId.get(toolCallId);
    if (rowId === undefined) return undefined;
    const row = this.findRow(rowId);
    return row?.kind === "toolCall" ? row : undefined;
  }

  private findSubagentRow(agentId: string): SubagentRow | undefined {
    const rowId = this.subagentRowIdByAgentId.get(agentId);
    if (rowId === undefined) return undefined;
    const row = this.findRow(rowId);
    return row?.kind === "subagent" ? row : undefined;
  }

  private findSubagentLifecycleRow(
    agentId: string,
    payload: Record<string, unknown>,
    event: SessionEvent,
  ): SubagentRow | undefined {
    const exact = this.findSubagentRow(agentId);
    if (exact) return exact;

    const parentToolCallId = this.stringPayload(payload, "parentToolCallId");
    if (!parentToolCallId) return undefined;
    const turnId = this.turnIdOf(event);
// Late subscription hydration cannot restore the real agentId from the text tool output of the background Agent.
// Will first use toolCallId to synthesize a SubagentRow. The later live lifecycle carries the real agentId.
// The old logic therefore appends the second row, and the UI will let the synthetic row without childSessionId preempt the pairing. parent tool call
// It is a stable unique identity within the same turn. Here, the real events are merged into rows and the childSessionId is completed.
    return this.snapshot.rows.window.find(
      (row): row is SubagentRow =>
        row.kind === "subagent" &&
        row.turnId === turnId &&
        row.parentToolCallId === parentToolCallId,
    );
  }

  private subagentAgentId(payload: Record<string, unknown>, event: SessionEvent): string {
    return (
      this.stringPayload(payload, "agentId") ??
      this.stringPayload(payload, "childSessionId") ??
      this.stringPayload(payload, "parentToolCallId") ??
      `subagent-${event.sequenceNumber}`
    );
  }

  private stringPayload(payload: Record<string, unknown>, key: string): string | undefined {
    const value = payload[key];
    return typeof value === "string" && value.length > 0 ? value : undefined;
  }

  private mapSubagentStatus(status: string | undefined): SubagentRow["status"] {
    switch (status) {
      case "completed":
      case "success":
        return "success";
      case "cancelled":
      case "stopped":
        return "cancelled";
      default:
        return "failed";
    }
  }
}

function isAskUserQuestionToolName(value: string | undefined): boolean {
  return value === "AskUserQuestion";
}

/**
* Narrow the core's tool display to the kinds that can be carried by the v4 protocol.
 *
* This whitelist originally wrote an inline judgment at each projection point, which was introduced
* One of the changes was missed when creating_workflow display. The real-time projection discarded the entire image, and the hydration path was not filtered.
* So the desktop version can only be seen after reloading. After harvesting a predicate, all projection points share the same list.
 */
function toProtocolToolCallDisplay(
  display: ToolResultDisplayPayload | undefined,
): ToolCallDisplay | undefined {
  if (!display) return undefined;
  switch (display.kind) {
    case "node_repl_images":
    case "task_output":
    case "respond_to_coordinator":
    case "mcp_tool":
    case "create_workflow":
//Five display kinds of observation workflow tools——shared side toolCallDisplaySchema has been added synchronously
// Member, only after releasing here can the UI get the structured payload on row.display.
    case "get_workflow_run":
    case "list_workflow_runs":
    case "eval_workflow_snippet":
    case "saved_workflow_list":
    case "list_models":
// Resume card for ResumeWorkflowRun.
    case "resume_workflow_run":
      return display;
    default:
      return undefined;
  }
}

function stringifyToolInput(input: unknown): string {
  try {
    return JSON.stringify(input ?? {}) ?? "{}";
  } catch {
    return "{}";
  }
}

function isExitPlanModeToolName(value: string | undefined): boolean {
  return value === "ExitPlanMode";
}

function createExitPlanModeApprovalQuestion(reason: string): UserInputQuestionPayload {
  return {
    question: reason,
    header: "Plan",
    options: [
      {
        value: "approve",
        label: "Approve",
        description: "Exit plan mode and start implementation.",
      },
    ],
  };
}

function readAskUserQuestionPayloadQuestions(input: unknown): UserInputQuestionPayload[] {
  const rawQuestions = readRawAskUserQuestions(input);
  return rawQuestions
    .map(normalizeAskUserQuestionPayloadQuestion)
    .filter((question): question is UserInputQuestionPayload => question !== null);
}

function readRawAskUserQuestions(input: unknown): unknown[] {
  if (!isPlainRecord(input)) {
    return [];
  }
  if (Array.isArray(input.questions)) {
    return input.questions;
  }
  return typeof input.question === "string" && Array.isArray(input.options) ? [input] : [];
}

function normalizeAskUserQuestionPayloadQuestion(value: unknown): UserInputQuestionPayload | null {
  if (!isPlainRecord(value)) {
    return null;
  }
  const question = nonEmptyString(value.question);
  const header = nonEmptyString(value.header) ?? question;
  const rawOptions = Array.isArray(value.options) ? value.options : [];
  const options = rawOptions
    .map(normalizeAskUserQuestionPayloadOption)
    .filter((option): option is UserInputQuestionPayload["options"][number] => option !== null);
  if (!question || !header || options.length === 0) {
    return null;
  }
  return {
    question,
    header,
    options,
    ...(value.multiSelect === true ? { multiSelect: true } : {}),
  };
}

function normalizeAskUserQuestionPayloadOption(
  value: unknown,
): UserInputQuestionPayload["options"][number] | null {
  if (!isPlainRecord(value)) {
    return null;
  }
  const label = nonEmptyString(value.label) ?? nonEmptyString(value.value);
  const optionValue = nonEmptyString(value.value) ?? label;
  if (!label || !optionValue) {
    return null;
  }
  return {
    value: optionValue,
    label,
    ...(typeof value.description === "string" ? { description: value.description } : {}),
    ...(typeof value.preview === "string" ? { preview: value.preview } : {}),
  };
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}
