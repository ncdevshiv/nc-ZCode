// Core capability contract of v4 native command layer (native reworked version).
//
// Layered discipline (no bridging):
// - This directory is the native implementation of the command: decision logic (steer diversion / draft promotion / abort life cycle /
// goal-pause barrier) directly drives core (ZCodeApp/runtime) in the handler without going through the old protocol op.
// - The session registry is still owned by the host: passthrough the old
// ZCodeProtocolSessionRecord (same object reference, field changes are visible in both directions, no second registration form is generated).
// - environment capabilities (model ready/legacy broadcast/shell parsing) are injected hooks: legacy protocol binder in transition
// Provide implementation and have the same life cycle as the old protocol - each hook is marked with a transition destination, and new hooks must be marked.
import type {
  SessionTaskType,
  StableForkGoalBoundaryMetadata,
  TraceContext,
} from "@zcode/contracts";
import type {
  CommandAck,
  CommandEnvelope,
  CommandPayloadMap,
  ConversationInputIntent,
  QueueItem,
  StableForkTarget,
  StableForkTargetResolution,
  ConversationRowTarget,
} from "@zcode/shared/zcode-protocol-v4";
import type { ZCodeApp } from "../../app/types.js";
import type { V4InteractionRegistry } from "../interaction-registry.js";
import type {
  ConversationEditTarget,
  ConversationRowTargetAction,
  ConversationRowTargetResolution,
} from "../product-projection.js";

/** Minimum log area (structured fields are directly transmitted to the host logger). */
export interface V4CommandLogger {
  info?(message: string, fields?: Record<string, unknown>): void;
  warn?(message: string, fields?: Record<string, unknown>): void;
}

export type V4QueueItemCommand = QueueItem;

export type V4StableForkTargetResolution =
  | {
      ok: true;
      target: StableForkTarget;
      goalBoundary: StableForkGoalBoundaryMetadata;
    }
  | Extract<StableForkTargetResolution, { ok: false }>;

/**
* Structured narrow view of the old ZCodeProtocolSessionRecord (only declares fields required by the command layer).
* Structural compatibility: binder directly transparently transmits old record objects; v4 provides isomorphic objects after maintaining its own session registry.
 */
export interface V4SessionRecordView {
  app: ZCodeApp;
  /**
* Session root traceContext (traceId is located above sessionId, corresponding to the entire task chain).
* When the command layer adjusts the runtime method/reissues the core event, the transparent transmission is still used, and a new trace is not allowed in the middle.
* The old record already carries this field, and the narrow view is directly transmitted transparently.
   */
  traceContext: TraceContext;
  workspace: { workspacePath: string };
/** Draft semantics: deferred = if the first message is not sent, sqlite is not entered; when the first message is sent, immediate is raised. */
  persistence: "immediate" | "deferred";
/** active turn lock: exists = turn is running (sendText takes the steer diversion, stop has the target). */
  activeAbortController?: AbortController;
/** The ready lock has been released but the state mutation wrapper is still using the record/runtime reference count. */
  residencyFinalizationCount?: number;
/** The currently executing automation dispatches a turn; only exists while the turn is running. */
  activeAutomationId?: string;
/** The idle dispatch turn currently being executed; only exists while the turn is running. */
  activeOffPeakTaskId?: string;
/** Recovery failure warning: reject new turn if it exists (history damage cannot be continued silently). */
  restoreWarning?: { message: string; type: string };
  taskType?: SessionTaskType;
}

export interface V4CommandCoreHost {
/** Session lookup (reference to the same registry object; if not present returns undefined → handler rejects). */
  getRecord(sessionId: string): V4SessionRecordView | undefined;
  logger?: V4CommandLogger;

//── v4 native ability (non-transition hook)────────────────────────────────
/** Queue item complete authoritative intent; sendQueuedNow prohibits degenerating into text-only resending. */
  getQueueItem?(sessionId: string, queueItemId: string): V4QueueItemCommand | null;
/** The typed maintenance command removes duplicates; the criteria come from the same projection queue and the bypass set is not maintained. */
  hasQueueItemKind?(sessionId: string, kind: QueueItem["kind"]): boolean;
/** guide eligibility: only blocks existing ordinary queues; existing guides are still allowed to continue FIFO admission. */
  hasQueuedDelivery?(sessionId: string, delivery: "guide" | "queue"): boolean;
  getQueueLength?(sessionId: string): number;
/** timeline/child This type of successful side effect without user message persists the duplication check fact. */
  recordPersistentCommandFact?(
    sessionId: string,
    source: "timeline" | "child",
    ack: CommandAck,
    metadata?: Record<string, unknown>,
  ): Promise<void>;
/** createSession.firstInput / selection side firstInput's durable admission boundary shared with normal send. */
  admitInputCommand?(
    envelope: CommandEnvelope,
    sessionId: string,
    admission: { admissionSeq: number; admittedAt: number; queueItemId: string },
  ): Promise<ConversationInputIntent | null>;
  cancelInputCommand?(sessionId: string, queueItemId: string, reason: string): Promise<void>;
  discardSharedContext?(sessionId: string, contextId: string): Promise<boolean>;
  /**
* Current input routing mode (data source = v4 projection inputRouting.mode).
* The held choice decision (heldQueueInputRequiresChoice) of sendText/sendGoalCommand relies on it to determine whether heldQueueDisposition must be carried.
* Session has no projection (no events yet) → null (processed as not held).
   */
  getInputRoutingMode?(
    sessionId: string,
  ): "startNow" | "enqueue" | "guide" | "reject" | "choice" | null;
/** After runtime event notification, wait for the target event to actually complete reorder drain + projection apply. */
  waitForProjectionEventCommit?(
    sessionId: string,
    eventId: string,
    options?: { signal?: AbortSignal },
  ): Promise<void>;
  /**
* rowId → authoritative messageId (data source = v4 projection rowId → messageId translation table).
* Historical compatibility query: translate the projected rowId (assistant row) into
* messageId of transcript. Not translated (non-assistant row/late rowId) → null → handler
* Reject, never silently reveal the latestCheckpoint (will fork/rewind the wrong point).
   */
  getMessageIdForRow?(sessionId: string, rowId: number): string | null;
/** The unique row target resolver shared by row.actions/CommandInbox/handler. */
  resolveRowActionTarget?(
    sessionId: string,
    target: ConversationRowTarget,
    action: ConversationRowTargetAction,
  ): ConversationRowTargetResolution | null;
  /**
* rowId → All transcript messageIds in the product turn (used for file summary cancellation).
* Multiple assistants/multiple checkpoints must be handed over to the core at one time to avoid removing only the last file.
   */
  getMessageIdsForTurnRow?(sessionId: string, rowId: number): string[];
  /**
* Core side strong verification: fork only hangs the last paragraph at the end of each round.
* true=is the wheel tail section; false=middle section (reject); null=no projection/unknown (processed as translation failure).
   */
  isLatestAssistantSegmentRow?(sessionId: string, rowId: number): boolean | null;
/** The only stable fork resolver: projection gate + transcript persistent anchor/fallback. */
  resolveStableForkTarget?(sessionId: string, rowId: number): Promise<V4StableForkTargetResolution>;
  /**
* latestAssistantRetryOnly core side defense: retryTurn can only point to the last item in the current projection
* assistantText row. false/null are rejected by the handler to prevent old clients from bypassing the UI.
   */
  isLatestRetryAssistantRow?(sessionId: string, rowId: number): boolean | null;
  /**
* latestQueryEditOnly core side defense: editUserQuery can only point to the last item in the current projection
* realUser userInput row. false/null are rejected by the handler to prevent old clients from bypassing the UI.
   */
  isLatestEditableUserRow?(sessionId: string, rowId: number): boolean | null;
/** rowId → product turnId (used to check the store when editUserQuery has no assistant anchor). */
  getTurnIdForRow?(sessionId: string, rowId: number): string | null;
  /**
* restoreWarning timing self-healing probe: whether the current process Registry has released available models.
* Binder implementation; host does not support → does not self-heal, maintain rejection.
   */
  hasUsableRuntimeModelTarget?(record: V4SessionRecordView): boolean;
  /**
* rowId → the rewind anchor messageId of the turn it belongs to (the data source is the same as above).
* editUserQuery targets user row (user row has no messageId), rewind requires messageId——
* Use the messageId of the assistant row in the same turn as the anchor point.
   */
  getTurnRewindAnchor?(sessionId: string, rowId: number): string | null;
  /**
* Running latest edit when assistant anchor has not yet appeared:
* After stopping, press the turn of the row to check the real user messageId in sessionStore.
   */
  resolveUserMessageIdForRow?(sessionId: string, rowId: number): Promise<string | null>;
  /**
* The original prompt parsing of retryTurn: assistant messageId → parentID (user message) → text.
* data source = core sessionStore (transcript authoritative), non-old protocol - binder implementation is just
* Because the deps injection point is in the host; the v4 self-sustained session registry is held natively by the host.
* Not found (no parent / no store) → null → handler only truncates and does not resend.
   */
  resolveTurnUserPrompt?(sessionId: string, assistantMessageId: string): Promise<string | null>;
/** Assistant feedback first persists the transcript metadata, and then publishes the projection event of the same entity. */
  setAssistantFeedback?(
    sessionId: string,
    input: {
      entityId: string;
      messageId: string;
      feedback: "like" | "dislike" | null;
    },
  ): Promise<void>;
  /**
* Interaction response registration form (v4 native infrastructure, non-transition hook): initiated by interaction-broker
* Register deferred and resolveInteraction commands when performing reverse requests (permission/AskUserQuestion)
*Reply via this delivery. The binder is injected into the same instance as the broker; the optional type is only convenient for testing fixtures——
* If not injected, it will be treated as a miss (idempotent and successfully closed).
   */
  interactions?: V4InteractionRegistry;

//──Transition hook (legacy compatibility window)────────────────────────
  /**
* turn model readiness check before running (credential/catalog parsing).
* Now implemented by the old protocol binder (ensureSessionModelAvailableForNextTurn);
* Destination: After the core app layer's self-sustained model is parsed, it will replace this hook.
   */
  ensureModelReady?(record: V4SessionRecordView): Promise<void>;
  /**
* The Agent process Registry verifies the target Provider before cutting the model. Ordinary model commands only carry Selection,
* Therefore Host running snapshots cannot be received or installed here.
   */
  ensureProviderAvailable?(
    sessionId: string,
    providerId: string,
  ): Promise<{ available: boolean; reason?: string }>;
  /**
* legacy broadcast (state.updated + record.stateRevision): legacy protocol consumer (sidebar/task index)
* Still rely on it to sense state changes before the sidebar migration is complete. v4 projects itself through the gateway event ingest,
* Do not rely on this hook; this hook will be closed when the old broadcast mechanism is closed.
   */
  afterLegacyStateMutation?(record: V4SessionRecordView, reason: string): Promise<void>;
  /**
* Session closure (execution side of deleteSession: unsubscribe event → app.close → registry removal → gateway clear channel).
* Transition form: Session registration performance returns to the old protocol host, binder inline implementation (sequentially aligns the old closeSession op,
* See zcode-protocol/server-operations.ts); v4 has a self-sustaining session registry and was later compiled into a native implementation.
   */
  closeSession?(sessionId: string): Promise<void>;
  /**
* Session record creation (execution side of createSession: record creation/event wiring/model catalog synchronization/
* Failed to clean up, all entangled with old protocol hosts). binder implements the old createSession op;
* After v4 has its own session registry, this hook will be replaced by native implementation.
* Semantic decisions (draft persistence / firstInput submission) remain in the native handler and do not enter hooks.
   */
  createSessionRecord?(params: {
    workspaceId: string;
    mcpServers?: CommandPayloadMap["createSession"]["mcpServers"];
/** Off-Peak tool surface access control determined by host; tools are not registered by default. */
    offPeakToolEnabled?: boolean;
    /**
* Dynamic workflow grayscale gate determined by host;
* By default, it falls back to the process-level workspace conclusion, which is still fail-closed.
     */
    dynamicWorkflowEnabled?: boolean;
  }): Promise<{ sessionId: string }>;
/** Create a hidden selection_side_chat child from the parent session stable drop boundary. */
  createSelectionSideSession?(
    sessionId: string,
    options: {
      sourceCommandId: string;
      revisionAtDecision: number;
      modelSelection?: NonNullable<
        CommandPayloadMap["createSelectionSideSession"]["firstInput"]
      >["modelSelection"];
    },
  ): Promise<{ sessionId: string }>;
/** conversation-only stable fork; must not stop parent, rewind workspace, or copy active work/queue. */
  forkStableConversation?(
    sessionId: string,
    options: {
      target: StableForkTarget;
      goalBoundary: StableForkGoalBoundaryMetadata;
      sourceCommandId: string;
      revisionAtDecision: number;
    },
  ): Promise<{ forkedSessionId: string }>;
/** @deprecated Only old host structures are compatible; new editUserQuery is never called, explicit forkAssistant is not affected. */
  forkConversationBeforeInput?(
    sessionId: string,
    options: {
      editTarget: ConversationEditTarget;
      envelope: CommandEnvelope;
      admission: { admissionSeq: number; admittedAt: number; queueItemId: string };
    },
  ): Promise<{ forkedSessionId: string }>;
/** After the bundle has been submitted, the runtime synchronization startup fails: only the child failure is recorded, and the parent fork ACK remains accepted. */
  recordForkStartFailure?(
    sessionId: string,
    envelope: CommandEnvelope,
    error: unknown,
  ): Promise<void>;
}
