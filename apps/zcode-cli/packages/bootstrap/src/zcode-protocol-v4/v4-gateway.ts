/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { LocalTtftRecorder } from "./local-ttft.js";
import { localTtftNow, localTtftFactsSchema } from "@zcode/shared/zcode-protocol-v4";
import {
  backgroundBashOutputResultSchema,
  v4BackgroundBashOutputParamsSchema,
  type BackgroundBashOutputResult,
} from "@zcode/shared/zcode-protocol-v4";
// V4 conversation gateway (host channel layer CLI side).
// Responsibilities: per-session ConversationTopicPublisher registry + flushWindowMs scheduled scheduling
// + v4/command → CommandInbox → The inbox of the host executor.
//
// Hierarchical discipline:
// - This class does not do network IO: the physical frame is handed over to the host (stdio notification / test collector) via host.emitWireFrame.
// - Command side effects are not implemented in this class: after the inbox ruling is passed, the host operation is called via host.executeCommand
// Unimplemented commands are returned via structured errors.
// - The flush timer is the only time source, the publisher itself remains pure pushing (bounds unchanged).
import type {
  DynamicWorkflowRunArtifact,
  DynamicWorkflowRunArtifactBytes,
  DynamicWorkflowRunArtifactItem,
  DynamicWorkflowRunWorkspaceNode,
  DynamicWorkflowRunWorkspaceNodeResult,
  DynamicWorkflowRunEvent,
  DynamicWorkflowRunSessionSummary,
  MessageWithParts,
  SessionEvent,
  TargetChangedPayload,
  TurnId,
  FileSystemErrorCode,
} from "@zcode/contracts";
import type { ConversationSnapshot } from "@zcode/shared/zcode-protocol-v4";
import { SessionEventType, isFileSystemPortError } from "@zcode/contracts";
import type { ZCodeWorkspaceRef } from "@zcode/shared";
import { extractMarkdownArtifactImageRefs } from "@zcode/shared";
import type {
  CommandAck,
  AttachmentRef,
  CommandEnvelope,
  CommandKey,
  CommandResult,
  ConversationRowTarget,
  CommandsQueryResult,
  ConversationInputIntent,
  ConversationTopicFrame,
  QueueItem,
  RoutedTopicFrame,
  RoutedTopicWireFrame,
  SessionSummary,
  SessionsIndexTopicFrame,
  SubscribeAck,
  V4AttachmentBeginResult,
  V4AttachmentChunkResult,
  V4AttachmentCommitResult,
  V4AttachmentPreviewSourceResult,
  V4AttachmentReadResult,
  V4ConversationAttachmentReadResult,
  V4ConversationAttachmentStatResult,
  V4ConversationFileChangesResult,
  V4ConversationFileRewindPreviewResult,
  V4ConversationPlansResult,
  V4ConversationWorkflowRunArtifactDataResult,
  V4ConversationWorkflowRunArtifactReadResult,
  V4ConversationWorkflowRunArtifactsResult,
  V4ConversationWorkflowRunNodeResultResult,
  V4ConversationWorkflowRunWorkspaceResult,
  V4ConversationWorkflowRunEventsResult,
  V4ConversationWorkflowRunsResult,
  V4ConversationRowsRangeResult,
  WorkspaceConfigState,
  WorkspaceConfigTopicFrame,
  ConversationTelemetryFact,
  CuaPermissionObservation,
  ConversationOpenTiming,
} from "@zcode/shared/zcode-protocol-v4";
import {
  DELIVERY_PROFILES,
  PROTOCOL_V4_LIMITS,
  ZCODE_ATTACHMENT_FAULT_CODES,
  ZCodeAttachmentFaultError,
  readZCodeAttachmentFaultCode,
  encodeTopicWireFrames,
  commandsQueryParamsSchema,
  commandsQueryResultSchema,
  parseCommandEnvelope,
  measureTopicNotificationEnvelopeBytes,
  parseConversationTopic,
  parseSessionsIndexTopic,
  parseWorkspaceConfigTopic,
  v4AttachmentAbortParamsSchema,
  v4AttachmentBeginParamsSchema,
  v4AttachmentChunkParamsSchema,
  v4AttachmentCommitParamsSchema,
  v4AttachmentPreviewSourceParamsSchema,
  v4AttachmentPreviewSourceResultSchema,
  v4AttachmentReadParamsSchema,
  v4ConversationAttachmentReadParamsSchema,
  v4ConversationAttachmentReadResultSchema,
  v4ConversationAttachmentStatParamsSchema,
  v4ConversationAttachmentStatResultSchema,
  v4ConnectionFlowParamsSchema,
  v4ConversationFileChangesParamsSchema,
  v4ConversationFileRewindPreviewParamsSchema,
  v4ConversationPlansParamsSchema,
  WORKFLOW_ARTIFACT_LIMITS,
  v4ConversationWorkflowRunArtifactDataParamsSchema,
  v4ConversationWorkflowRunArtifactDataResultSchema,
  v4ConversationWorkflowRunArtifactReadParamsSchema,
  v4ConversationWorkflowRunArtifactReadResultSchema,
  v4ConversationWorkflowRunArtifactsParamsSchema,
  v4ConversationWorkflowRunArtifactsResultSchema,
  v4ConversationWorkflowRunNodeResultParamsSchema,
  v4ConversationWorkflowRunNodeResultResultSchema,
  v4ConversationWorkflowRunWorkspaceParamsSchema,
  v4ConversationWorkflowRunWorkspaceResultSchema,
  WORKFLOW_WORKSPACE_LIMITS,
  v4ConversationWorkflowRunEventsParamsSchema,
  v4ConversationWorkflowRunEventsResultSchema,
  v4ConversationWorkflowRunsParamsSchema,
  v4ConversationWorkflowRunsResultSchema,
  v4ConversationRowsRangeParamsSchema,
  v4ConversationResyncParamsSchema,
  v4ConversationSubscribeParamsSchema,
  v4ConversationUnsubscribeParamsSchema,
} from "@zcode/shared/zcode-protocol-v4";
import { AttachmentUploadRegistry } from "./attachment-upload-registry.js";
import {
  ColdSessionResumeCoordinator,
  type ColdSessionResumeOutcome,
} from "./cold-session-resume.js";
import { CommandInbox } from "./command-inbox.js";
import {
  ConversationTopicPublisher,
  ProjectionPayloadTooLargeError,
} from "./conversation-topic-publisher.js";
import type {
  SessionConfigSeed,
  SessionSubagentsSeed,
  SessionUsageSeed,
} from "./product-projection.js";
import type { ConversationRowTargetAction } from "./product-projection.js";
import { SessionsIndexPublisher } from "./sessions-index-publisher.js";
import { SessionsIndexPublisherRegistry } from "./sessions-index-publisher-registry.js";
import { WorkspaceConfigPublisher } from "./workspace-config-publisher.js";
import type { TopicFrameReservation } from "./topic-frame-reservation.js";
import { ConversationTelemetryFactNormalizer } from "./conversation-telemetry-facts.js";
import { CuaPermissionObservationNormalizer } from "./cua-permission-observation.js";
import { V4CapabilityUnsupportedError } from "./commands/handlers/interaction-background.js";

function toRuntimeTurnId(turnId: string | null): TurnId | null {
// conversation projection saves product turnId as string for row index;
// When leaving the gateway to adjust runtime file summary/rollback capabilities, the brand type of contracts needs to be restored.
  return turnId as TurnId | null;
}

function rowTargetActionForCommand(
  type: CommandEnvelope["type"],
): ConversationRowTargetAction | null {
  switch (type) {
    case "forkAssistant":
    case "editUserQuery":
    case "retryTurn":
    case "applyFileRewind":
    case "setAssistantFeedback":
      return type;
    default:
      return null;
  }
}

interface PersistedEventsLoadResult {
  events: SessionEvent[];
  synthesized: boolean;
/** The store-verified child manifest injected before the live buffer is replenished after durable transcript replay. */
  subagentsSeed?: SessionSubagentsSeed;
/** shared_context does not generate visible rows; it only sends desensitized handover metadata. */
  sharedContextImport?: ConversationSnapshot["sharedContextImport"];
/** The memory eventStore contains the raw sequence water level when taking the snapshot. */
  sourceEventSeq?: number;
/** Use the same capacity seed as this historical event; null means that the historical water level has been queried but there is no historical water level. */
  usageSeed?: SessionUsageSeed | null;
}

type V4GatewayErrorContext = Record<string, unknown>;

/**
* A **whole byte** that has been read back for block read multiplexing.
 *
* Two families share this table: preview of sent attachments (`attachmentRead`) and bytes of dwf user plane products
* (`workflowRunArtifactRead`). Sharing is intentional - the invalidation rules for both are verbatim (TTL, byte budget,
* The oldest first, clear by `sessionId` when the session is destroyed), and split into two tables will get two **respective** byte budgets,
* So the constraint "how many bytes can be cached at most" can no longer be explained clearly.
 *
* Key spaces are distinguished by **first segment tag** (`att` / `dwfart`), not by the number or content of fields - the keys of both families are
* Four or five segments separated by NUL. The number of segments is the same and the contents may collide (a product id called "1" and an attachmentIndex
* 1 will look the same), only a first segment that cannot be equal is provably isolated.
 *
* `bytes` is null, indicating that the read is still flying: at this time it is not included in the budget, and will not be evicted according to the budget (evict a file that is being
* The await entry simply causes the next block to reread the entire file, which is what this table is trying to eliminate).
 */
interface BinaryReadCacheEntry {
  sessionId: string;
  accessedAt: number;
  bytes: number | null;
  payload: Promise<{ bytes: Uint8Array; mediaType: string }>;
}

export interface V4GatewayHost {
  cliVersion?: string;
/** Whether the session is active in the host registry (inbox's sessionNotFound rule based on this). */
  sessionExists(sessionId: string): boolean;
  /**
* V4 cold recovery hook. gateway wraps runtime activation and projection with the same READY promise
* hydration; the host is only responsible for restoring the record.
   */
  resumePersistedSession?(
    sessionId: string,
    resumeThoughtLevel?: string,
    workspace?: ZCodeWorkspaceRef,
  ): Promise<ColdSessionResumeOutcome>;
  /**
* Downlink physical frame export (the host is responsible for delivery: stdio notification / MessagePort / ws).
   *
* Logical frame fallback bypasses 1MiB cap, sharding, and receiver atomic assembly boundaries; therefore production
* Both the host and the test host must explicitly receive physical wire, and the type layer no longer allows logical frames to be returned.
   */
  emitWireFrame(frame: RoutedTopicWireFrame): void;
/** No text fact for the current process live ingest; no caching, no topic replay. */
  emitConversationTelemetryFact?(fact: ConversationTelemetryFact): void;
  emitLocalTtftFacts?(facts: import("@zcode/shared").LocalTtftFacts): void;
/** Current process live request_access permission fact; no caching, no entry into topic replay. */
  emitCuaPermissionObservation?(observation: CuaPermissionObservation): void;
  /**
* sessions-index: Session → WorkspaceId to which it belongs (bucket key of list topic).
* Not implemented (old host) → the sessions-index path is not activated as a whole (no-op) and does not affect conversation.
   */
  getSessionWorkspaceId?(sessionId: string): string | null;
/** sessions-index: meta information for session list (createdAt/parent session/last activity time). */
  getSessionIndexMeta?(sessionId: string): {
    createdAt: number;
    lastActivityAt: number;
    parentSessionId?: string;
  } | null;
  /**
* config seed: the current truth value of the session runtime (model selection/thinking depth/collaboration mode).
* The initial value of the projection cannot be written as a null value - the default startup model of the runtime, the project persistence mode preference, and the historical session
* The last selection restored by resume only lives in the runtime (ModelSelected only after switchModelConfig
*Reissue, it may not be in the log at all), seeds are their only channel for projection.
* Returns null if the session is not registered (gateway skips, keeping empty initial value); not implemented (old host/test stub) same.
   */
  getSessionConfigSeed?(sessionId: string): SessionConfigSeed | null;
/** Read-only App switch during session creation, does not read real-time settings or infer Memory tool usage. */
  getSessionMemoryEnabled?(sessionId: string): boolean | undefined;
  /**
* Cold recovery usage seed: transcript synthesis path may only generate 0/default window placeholders
* ModelComplete; the host can provide real water levels from persistent assistant tokens / runtime snapshot.
* Preserve event log reduction results when not implemented.
   */
  getSessionUsageSeed?(
    sessionId: string,
    persistedMessages?: MessageWithParts[],
  ): Promise<SessionUsageSeed | null> | SessionUsageSeed | null;
/** sessions-index: The session ID currently registered in a workspace (used for cold start snapshot). */
  listWorkspaceSessionIds?(workspaceId: string): string[];
  /**
* sessions-index: draft judgment - deferred sessions that are persistent and have not sent the first input are not included in the list.
* The old workspace prepare path will pre-build the deferred session (list reading sqlite,
* deferred is not visible so it is not placed); after sessions-index is derived from the live registry, these ghost drafts will
* Appears in the sidebar as "New Task". After the first sendText upgrades persistence to immediate,
* The event stream naturally triggers fanOutToIndex to enqueue the session. Not implemented (old host/test stub) → no filtering.
   */
  isDraftSession?(sessionId: string): boolean;
  /**
* sessions-index: Construct a lightweight summary (cold start seed) of all sessions in a workspace directly from the persistence store.
* Unloaded (no live publisher) sessions are listed by it; loaded sessions are overwritten by the gateway with live projection.
* Store reading is asynchronous, so Promise is allowed to be returned (await when subscribing to gateway; synchronous stub returns array directly).
   */
  getStoredSessionSummaries?(workspaceId: string): Promise<SessionSummary[]> | SessionSummary[];
  /**
* 3.3.6 Remote history compatibility: use precise task allowlist idempotent claim to return strict identity digest.
* Non-remote workspace returns null; failure can be downgraded to an empty array, and subsequent subscriptions carrying allowlist will be retried.
   */
  refreshLegacySessionSummaries?(
    workspaceId: string,
    legacyTaskIds: readonly string[],
  ): Promise<SessionSummary[] | null> | SessionSummary[] | null;
  /**
* workspace-config: The configuration directory of a certain workspace (config options + slash command).
* The seed and invalidateWorkspaceConfig re-pull during subscription go here.
* Not implemented (old hosting/test stub) → workspace-config path degrades to empty directory snapshot.
   */
  getWorkspaceConfig?(
    workspaceId: string,
  ): Promise<WorkspaceConfigState | null> | WorkspaceConfigState | null;
  readBackgroundBashOutput?(sessionId: string, workId: string): Promise<BackgroundBashOutputResult>;
/** Side effects of executing the accepted command; the return value is entered into ACK.result (fork/createSession with sessionId). */
  executeCommand(
    envelope: CommandEnvelope,
    admission?: { admissionSeq: number; admittedAt: number; queueItemId: string },
  ): Promise<CommandResult | undefined>;
/** Enter durable admission before executing the command; return the same complete intent for inbox pin. */
  admitCommandInput?(
    envelope: CommandEnvelope,
    admission: { admissionSeq: number; admittedAt: number; queueItemId: string },
  ): Promise<ConversationInputIntent | null>;
  cancelCommandInput?(
    envelope: CommandEnvelope,
    queueItemId: string,
    reason: string,
  ): Promise<void>;
/** Terminate the current turn when projection exceeds 16MiB during operation; the same fault cycle is guaranteed to be called only once by the gateway. */
  terminateTurnForProjectionFault?(
    sessionId: string,
    reasonCode: "proto.payloadTooLarge",
  ): Promise<void> | void;
/** commands/query persistence fallback; four sources must be hit exactly by sourceCommandId. */
  lookupTranscriptCommand?(key: CommandKey): Promise<CommandAck | null> | CommandAck | null;
  lookupTimelineCommand?(key: CommandKey): Promise<CommandAck | null> | CommandAck | null;
  lookupChildCommand?(key: CommandKey): Promise<CommandAck | null> | CommandAck | null;
  lookupDiscardedCommand?(key: CommandKey): Promise<CommandAck | null> | CommandAck | null;
/** transcript After atomic promotion, the old lazy seed is synchronized and invalidated, and then the CommandInbox live pin is released. */
  invalidatePersistentCommandFacts?(sessionId: string): void;
/** canonical goal complete has entered projection; host side effects must be detached and blocking ingest is prohibited. */
  onTargetCompleted?(sessionId: string, event: SessionEvent): void;
/** Write session artifact once after complete chunk transaction commit. */
  putSessionAttachment?(
    sessionId: string,
    input: { fileName: string; mime: string; bytes: Uint8Array },
  ): Promise<{ ref: string }>;
/** Image/video/PDF read-only query has been sent; gateway is allowed to enter the host after completing row/ref authorization. */
  readSessionAttachment?(
    sessionId: string,
    input: {
      ref: string;
      mime: string;
      maxBytes: number;
      messageId?: string;
      attachmentIndex?: number;
    },
  ): Promise<{ bytes: Uint8Array; mediaType: string }>;
/** Share userInput attachment metadata stat in the selection stage; gateway completes row/index authorization first. */
  statSessionAttachment?(
    sessionId: string,
    input: {
      ref: string;
      mime: string;
      messageId?: string;
      attachmentIndex?: number;
    },
  ): Promise<{ totalBytes: number; mediaType: string; mtimeMs?: number }>;
/** Desktop local has sent the video path; gateway is allowed to enter the host after completing row/index authorization. */
  resolveSessionAttachmentPreviewSource?(
    sessionId: string,
    input: {
      ref: string;
      mime: string;
      messageId?: string;
      attachmentIndex?: number;
    },
  ): Promise<V4AttachmentPreviewSourceResult>;
  getConversationFileChanges?(
    sessionId: string,
    targetRowId: number,
    messageIds: string[],
    targetTurnId: TurnId | null,
  ): Promise<V4ConversationFileChangesResult>;
  previewConversationFileRewind?(
    sessionId: string,
    targetRowId: number,
    messageIds: string[],
    targetTurnId: TurnId | null,
  ): Promise<V4ConversationFileRewindPreviewResult>;
  /**
* Event log pagination of workflow run (details page audit page). Absent = the session runtime does not have this capability
* (dwf journal is unavailable → run service is not structured at all), the gateway returns an error that the structuring capability is not supported accordingly.
   */
  listDynamicWorkflowRunEvents?(
    sessionId: string,
    input: { runId: string; afterSequence?: number; limit?: number },
  ): Promise<DynamicWorkflowRunEvent[]>;
  /**
* Enumeration side of dwf run (discovery query after restart).
* The absence condition is the same as {@link listDynamicWorkflowRunEvents}.
   */
  listDynamicWorkflowRuns?(
    sessionId: string,
    input: { limit?: number },
  ): Promise<DynamicWorkflowRunSessionSummary[]>;
  /**
* Read the **user interface product** of workflow run. Sanjo was present together,
* Absent together (registration with the same condition on the app side). The absence condition is the same as {@link listDynamicWorkflowRunEvents}.
   *
* ⚠ Terminology: artifact = the output of a script published to the user via `artifact.*`, not the top-level return value of run.
   */
  listDynamicWorkflowRunArtifacts?(
    sessionId: string,
    input: { runId: string },
  ): Promise<readonly DynamicWorkflowRunArtifact[] | undefined>;
  listDynamicWorkflowRunArtifactItems?(
    sessionId: string,
    input: { runId: string; artifactId: string; afterSequence?: number; limit: number },
  ): Promise<readonly DynamicWorkflowRunArtifactItem[]>;
  readDynamicWorkflowRunArtifact?(
    sessionId: string,
    input: { runId: string; artifactId: string; version: number },
  ): Promise<DynamicWorkflowRunArtifactBytes | undefined>;
  /**
* workspace transcript of workflow run: two items present together,
* Absent together. Authorization is on the host side (run belongs to this session); rejection and unknown return `undefined`.
   */
  listDynamicWorkflowRunWorkspaceNodes?(
    sessionId: string,
    input: { runId: string },
  ): Promise<readonly DynamicWorkflowRunWorkspaceNode[] | undefined>;
  readDynamicWorkflowRunNodeResult?(
    sessionId: string,
    input: { runId: string; siteId: string; ordinal: number; maxBytes: number },
  ): Promise<DynamicWorkflowRunWorkspaceNodeResult | undefined>;
  /**
* Hydration: Read session persistence events for cold subscription reconstruction projection (fork child/resume/
*app-restart). `synthesized=true` means that the event log cannot cover transcript, and events are from
* Transcript is reverse synthesized - even if there is a cold publisher (fork resume's ingest takes precedence)
* Built) must also be rebuilt, otherwise history will not be reflected. `synthesized=false` (full event log) is retained
* Existing live publisher (streaming cannot be interrupted by rebuilding). Unimplemented (old host) cold subscriptions degrade to empty projections.
   */
  loadPersistedEvents?(
    sessionId: string,
    persistedMessages?: MessageWithParts[],
  ): Promise<PersistedEventsLoadResult>;
/** Only used for low-frequency life cycle and recovery decisions; high-frequency event/stream trace is prohibited from taking production logs. */
  onDebug?(message: string): void;
  onError?(scope: string, error: unknown, context?: V4GatewayErrorContext): void;
}

interface ConversationV4GatewayOptions {
  now?: () => number;
/** logEpoch generator (default is random within the process; test injects fixed values ​​to ensure determinism). */
  createLogEpoch?: (sessionId: string) => string;
}

interface FlushState {
  sessionId: string;
  topic: string;
  subscriptionId: string;
  connectionId: string;
  deliveryProfile: "continuous" | "replayable";
  flushWindowMs: number;
  timer: ReturnType<typeof setTimeout> | null;
}

interface HydrationBuffer {
  cancelled: boolean;
  eventIds: Set<string>;
  rawEvents: SessionEvent[];
}

interface RawSequenceState {
/** The runtime raw cursor that has been consumed by cold snapshot or live replay. */
  sourceEventSeq: number;
/** transportSeq = rawSeq + offset; it will be corrected forward when sequence=0 is encountered. */
  offset: number;
  lastTransportSeq: number;
  seenEventIds: Set<string>;
/** Events that publisher has successfully applied; events that have been seen by the runtime sink but are still in the gap buffer are not included in this collection. */
  appliedEventIds: Set<string>;
/** The fact that publisher apply failed; the temporary sink must reject immediately when it is late to register the waiter. */
  failedEventById: Map<string, Error>;
/** notify sink can be out of order; it can only be drained if it is continuous from sourceEventSeq+1. */
  pendingByRawSeq: Map<number, SessionEvent>;
/** synthesized hydration When reconstructing the projection, compensate for the raw facts that have been reached after the persistent read boundary. */
  recentRawEventsById: Map<string, SessionEvent>;
}

interface ProjectionEventCommitWaiter {
  resolve(): void;
  reject(error: Error): void;
}

const PROJECTION_EVENT_COMMIT_TIMEOUT_MS = 25_000;
const MAX_TELEMETRY_EVENT_IDS = 2_000;
/** The detached subagent child is the retention time before the publisher is released by a low-frequency tick when there are no subscribers after the final state. */
const DETACHED_CHILD_PUBLISHER_GRACE_MS = 120_000;

class ProjectionEventCommitWaitError extends Error {
  constructor(
    readonly reasonCode:
      | "fault.projectionEventCommit.aborted"
      | "fault.projectionEventCommit.applyFailed"
      | "fault.projectionEventCommit.disposed"
      | "fault.projectionEventCommit.gatewayDisposed"
      | "fault.projectionEventCommit.rehydrated"
      | "fault.projectionEventCommit.timeout",
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options);
    this.name = "ProjectionEventCommitWaitError";
  }
}

/**
* Server internal dispatch results: initial frame is only for request-scoped post-response outbox
* Consumed, public JSON-RPC result schema is always strictly `{ ack }`.
 */
interface V4SubscribeDispatchResult<TFrame> {
  ack: SubscribeAck & { openTiming?: ConversationOpenTiming };
  initialFrame: TFrame | null;
  initialWires: RoutedTopicWireFrame[];
  commit(): boolean;
}

function encodeReservedTopicFrame(
  reservation: TopicFrameReservation<RoutedTopicFrame>,
): RoutedTopicWireFrame[] {
  return encodeTopicWireFrames(reservation.frame, {
    deliveryKind: reservation.deliveryKind,
    topic: reservation.frame.topic,
    subscriptionId: reservation.frame.subscriptionId,
    logicalFrameId: reservation.logicalFrameId,
    logicalFrameOrdinal: reservation.logicalFrameOrdinal,
    measurePhysicalFrameBytes: (wire) => measureTopicNotificationEnvelopeBytes(wire).maxBytes,
  }) as RoutedTopicWireFrame[];
}

function subscriptionRouteKey(topic: string, subscriptionId: string, connectionId: string): string {
  return `${topic}\0${subscriptionId}\0${connectionId}`;
}

function defaultLogEpoch(): string {
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function artifactRefBelongsToSession(ref: string, sessionId: string): boolean {
  return ref.startsWith(`zcode-artifact://${encodeURIComponent(sessionId)}/`);
}

/** A collection of error codes for attachments that are "determined not to exist" at the file system layer. */
const MISSING_ATTACHMENT_FS_CODES = new Set<FileSystemErrorCode>([
  "not_found",
  "is_directory",
  "not_file",
]);

/**
* Convert errors thrown by host/FileSystemPort into attachment faults with stable codes.
* When the host has given a structured fault code, it is transparently transmitted as it is, and the rest is determined according to FileSystemPortError.code;
* If there is no match, the original error will be maintained and the upper layer will handle it as "unknown" instead of guessing a definite classification.
 */
function toShareStatFault(error: unknown): unknown {
  if (readZCodeAttachmentFaultCode(error)) return error;
  if (isFileSystemPortError(error) && MISSING_ATTACHMENT_FS_CODES.has(error.code)) {
    return new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.shareStatNotFound, {
      cause: error,
    });
  }
  return error;
}

export class ConversationV4Gateway {
  private readonly publishers = new Map<string, ConversationTopicPublisher>();
/** sessions-index: workspaceId → list publisher (parallel to conversation, independent of seq/logEpoch). */
  private readonly indexPublishers = new SessionsIndexPublisherRegistry();
/** workspace-config: workspaceId → configuration directory publisher (conflated overall replacement state). */
  private readonly configPublishers = new Map<string, WorkspaceConfigPublisher>();
/** The session that has completed the first hydration (to avoid repeated reconstruction/double counting, see hydratePublisher). */
  private readonly hydratedSessions = new Set<string>();
/** The first hydration is performed individually according to the session; concurrent panes share the same reconstruction result. */
  private readonly hydrationInFlight = new Map<string, Promise<ConversationTopicPublisher>>();
/** Cold activation reaches the READY water level of hydration; only blocks command/query during this recovery period. */
  private readonly readyFlights = new Map<string, Promise<ConversationTopicPublisher>>();
/** load await raw accepted events in the window; fill in cursor/eventId after reconstruction. */
  private readonly hydrationBuffers = new Map<string, HydrationBuffer>();
/** Per-session monotonic mapping between transcript synthetic sequences and runtime raw sequences. */
  private readonly rawSequenceStates = new Map<string, RawSequenceState>();
/** connection-independent; command admission is decoupled from the transport subscription life cycle. */
  private readonly projectionEventCommitWaiters = new Map<
    string,
    Map<string, Set<ProjectionEventCommitWaiter>>
  >();
/** A live child that does not have an independent bootstrap record, but whose raw events are continuously forwarded by the parent runtime. */
  private readonly detachedLiveSessions = new Set<string>();
  /**
* Detached subagent child's parent record ownership and final state time. The child does not have its own record, and the publisher can only be released with the parent record.
* Or it will be released by a low-frequency tick after the turn ends and there are no subscribers and exceeds grace; otherwise, it will stay until the process exits.
   */
  private readonly detachedChildParent = new Map<string, string>();
  private readonly detachedChildrenByParent = new Map<string, Set<string>>();
  private readonly detachedTerminalAt = new Map<string, number>();
/** Cold recovery coordinator (existing activation solo + error type). */
  private readonly coldResume: ColdSessionResumeCoordinator;
/** Subscription → flush scheduling status (publisher does not hold a timer internally, scheduling belongs to the gateway). */
  private readonly flushStates = new Map<string, FlushState>();
/** ACK/outbox control reservations that have not yet been admitted are prohibited from being preemptively sent by online flush. */
  private readonly controlReservations = new WeakSet<object>();
/** transport high-water pause only isolates by trusted connectionId and does not change the true value of ingest/publisher. */
  private readonly pausedConnections = new Set<string>();
/** A runtime stop is only triggered once in an out-of-bounds cycle; it is released after the final event arrives. */
  private readonly projectionFaultedSessions = new Set<string>();
  private readonly inbox: CommandInbox;
  private readonly attachmentUploads: AttachmentUploadRegistry;
  private readonly binaryReadCache = new Map<string, BinaryReadCacheEntry>();
  private binaryReadCacheBytes = 0;
  private readonly localTtft = new LocalTtftRecorder(
    localTtftNow,
    () => {
      this.host.onError?.(
        "v4.localTtft.completedCapacity",
        new Error("TTFT completed record capacity exceeded"),
      );
    },
    (facts) => {
      const parsed = localTtftFactsSchema.safeParse(facts);
      if (parsed.success) this.host.emitLocalTtftFacts?.(parsed.data);
    },
  );
  private readonly attachmentPruneTimer: ReturnType<typeof setInterval>;
  private readonly now: () => number;
  private readonly createLogEpoch: (sessionId: string) => string;
  private readonly telemetryNormalizer = new ConversationTelemetryFactNormalizer();
  private readonly cuaPermissionNormalizer = new CuaPermissionObservationNormalizer();
  private readonly telemetryEventIds = new Set<string>();
  private disposed = false;

/** Lightweight metadata update after session entry status change, without replaying conversation event. */
  updateSharedContextImport(
    sessionId: string,
    source: ConversationSnapshot["sharedContextImport"],
  ): void {
    const publisher = this.publishers.get(sessionId);
    if (!publisher) return;
    publisher.seedSharedContextImport(source);
    for (const [routeKey, state] of this.flushStates) {
      if (state.sessionId === sessionId) this.scheduleFlush(routeKey, state, publisher);
    }
  }

  constructor(
    private readonly host: V4GatewayHost,
    options: ConversationV4GatewayOptions = {},
  ) {
    this.now = options.now ?? Date.now;
    this.createLogEpoch = options.createLogEpoch ?? defaultLogEpoch;
    this.coldResume = new ColdSessionResumeCoordinator(host);
    this.inbox = new CommandInbox({
      getRevision: (sessionId) => {
        if (!this.host.sessionExists(sessionId)) return null;
// Session is known but no events yet → projection is not built, revision is considered 0 (draft starting point).
        return this.publishers.get(sessionId)?.getSnapshot().revision ?? 0;
      },
      getLogEpoch: (sessionId) => this.publishers.get(sessionId)?.getSnapshot().logEpoch ?? null,
      validateRowTarget: (envelope) => {
        const action = rowTargetActionForCommand(envelope.type);
        if (!action || envelope.sessionId === null) return { verdict: "allow" };
        const target = (envelope.payload as { target?: ConversationRowTarget }).target;
        if (!target) return { verdict: "reject", reasonCode: "proto.invalidPayload" };
        const resolution = this.publishers
          .get(envelope.sessionId)
          ?.resolveRowActionTarget(target, action);
        if (!resolution) return { verdict: "stale", reasonCode: "proto.staleTarget" };
        if (resolution.ok) return { verdict: "allow" };
        return resolution.status === "stale"
          ? { verdict: "stale", reasonCode: resolution.reasonCode }
          : { verdict: "reject", reasonCode: resolution.reasonCode };
      },
      lookupTranscriptCommand: (key) => this.host.lookupTranscriptCommand?.(key) ?? null,
      lookupTimelineCommand: (key) => this.host.lookupTimelineCommand?.(key) ?? null,
      lookupChildCommand: (key) => this.host.lookupChildCommand?.(key) ?? null,
      lookupDiscardedCommand: (key) => this.host.lookupDiscardedCommand?.(key) ?? null,
      now: this.now,
    });
    this.attachmentUploads = new AttachmentUploadRegistry({
      now: this.now,
      putSessionAttachment: async (sessionId, input) => {
        if (!this.host.putSessionAttachment) {
          throw new Error("fault.attachment.putUnsupported");
        }
        return this.host.putSessionAttachment(sessionId, input);
      },
    });
    this.attachmentPruneTimer = setInterval(
      () => this.attachmentUploads.pruneExpired(),
      Math.min(30_000, PROTOCOL_V4_LIMITS.attachmentUploadTtlMs),
    );
    (
      this.attachmentPruneTimer as ReturnType<typeof setInterval> & { unref?: () => void }
    ).unref?.();
  }

  setConnectionFlowState(rawParams: unknown): void {
    const params = v4ConnectionFlowParamsSchema.parse(rawParams);
    if (params.state === "closed") {
      this.pausedConnections.delete(params.connectionId);
      this.clearConnectionFlushTimers(params.connectionId);
      this.attachmentUploads.clearConnection(params.connectionId);
      return;
    }
    if (params.state === "saturated") {
      if (this.pausedConnections.has(params.connectionId)) return;
      this.pausedConnections.add(params.connectionId);
      this.clearConnectionFlushTimers(params.connectionId);
      return;
    }
    if (!this.pausedConnections.delete(params.connectionId)) return;
    this.flushConnection(params.connectionId);
  }

  private clearConnectionFlushTimers(connectionId: string): void {
    for (const state of this.flushStates.values()) {
      if (state.connectionId !== connectionId || state.timer === null) continue;
      clearTimeout(state.timer);
      state.timer = null;
    }
  }

  private flushConnection(connectionId: string): void {
    for (const [routeKey, state] of this.flushStates) {
      if (state.connectionId !== connectionId) continue;
      if (state.timer) clearTimeout(state.timer);
      state.timer = null;
      const publisher = this.publishers.get(state.sessionId);
      if (!publisher?.hasSubscription(state.subscriptionId, connectionId)) {
        this.flushStates.delete(routeKey);
        continue;
      }
      const reservation = publisher.reserveFlush(state.subscriptionId);
      if (!reservation) continue;
      try {
        this.emitReservation(reservation);
      } catch (error) {
        this.host.onError?.("v4.frame.emit", error);
      }
    }
    for (const workspaceId of this.indexPublishers.keys()) {
      this.flushIndex(workspaceId, connectionId);
    }
    for (const workspaceId of this.configPublishers.keys()) {
      this.flushConfig(workspaceId, connectionId);
    }
  }

/** Authoritative event entry: projection advancement + each subscriber schedules frames according to profile.flushWindowMs. */
  ingest(sessionId: string, event: SessionEvent): void {
    if (this.disposed) return;
    const hydrationBuffer = this.hydrationBuffers.get(sessionId);
    if (hydrationBuffer) {
      const eventId = String(event.id);
      if (hydrationBuffer.eventIds.has(eventId)) return;
// Remember the raw fact first; it may not enter the publisher temporarily because the preamble has not yet arrived.
      hydrationBuffer.eventIds.add(eventId);
      hydrationBuffer.rawEvents.push(event);
    }
    this.emitLiveTelemetryFact(sessionId, event);
    for (const normalizedEvent of this.normalizeRuntimeEventSequence(sessionId, event)) {
      try {
        this.localTtft.event(sessionId, normalizedEvent);
      } catch (error) {
        try {
          this.host.onError?.("v4.localTtft.observe", error);
        } catch {
/* Diagnostic callbacks cannot block actual content either. */
        }
      }
      this.ingestNormalizedEvent(sessionId, normalizedEvent);
    }
  }

  private emitLiveTelemetryFact(sessionId: string, event: SessionEvent): void {
    const eventId = String(event.id);
// The main session and detached child maintain event sequences respectively, and eventId cannot be assumed to span
// session is globally unique. The old deduplication only uses eventId, which will misjudge the child's event with the same number as the main session replay.
// Causes the foreground Subagent's true turn facts to be silently discarded.
    const telemetryEventKey = `${sessionId}\0${eventId}`;
    if (this.telemetryEventIds.has(telemetryEventKey)) return;
    this.telemetryEventIds.add(telemetryEventKey);
    if (this.telemetryEventIds.size > MAX_TELEMETRY_EVENT_IDS) {
      const oldest = this.telemetryEventIds.values().next().value;
      if (typeof oldest === "string") this.telemetryEventIds.delete(oldest);
    }
    try {
      const config =
        this.publishers.get(sessionId)?.getSnapshot().config ??
        this.host.getSessionConfigSeed?.(sessionId) ??
        undefined;
      const fact = this.telemetryNormalizer.normalize(sessionId, event, {
        memoryEnabled: this.host.getSessionMemoryEnabled?.(sessionId),
        modelName: config?.model,
        modelProvider: config?.provider,
      });
      if (fact) {
        this.host.emitConversationTelemetryFact?.(fact);
      }
    } catch (error) {
// Turn facts must not back-block conversation projections; only diagnostics are logged on strict schema failures.
      this.host.onError?.("v4.telemetry.normalize", error);
    }
    try {
      const observation = this.cuaPermissionNormalizer.normalize(sessionId, event);
      if (observation) this.host.emitCuaPermissionObservation?.(observation);
    } catch (error) {
//Permission observation is only a live UI prompt, schema or projection exceptions cannot block the conversation main link.
      this.host.onError?.("v4.cuaPermissionObservation.normalize", error);
    }
  }

/** Wait for the specified raw event to actually complete reorder drain + publisher projection apply. */
  waitForProjectionEventCommit(
    sessionId: string,
    eventId: string,
    options: { signal?: AbortSignal } = {},
  ): Promise<void> {
    if (this.disposed) {
      return Promise.reject(
        new ProjectionEventCommitWaitError(
          "fault.projectionEventCommit.gatewayDisposed",
          "conversation gateway is disposed",
        ),
      );
    }
    const state = this.getOrCreateRawSequenceState(sessionId);
    if (state.appliedEventIds.has(eventId)) return Promise.resolve();
    const failed = state.failedEventById.get(eventId);
    if (failed) return Promise.reject(failed);
    if (options.signal?.aborted) {
      return Promise.reject(
        new ProjectionEventCommitWaitError(
          "fault.projectionEventCommit.aborted",
          `projection event commit wait aborted: ${eventId}`,
          { cause: options.signal.reason },
        ),
      );
    }
    return new Promise<void>((resolve, reject) => {
      const byEvent = this.projectionEventCommitWaiters.get(sessionId) ?? new Map();
      this.projectionEventCommitWaiters.set(sessionId, byEvent);
      const waiters = byEvent.get(eventId) ?? new Set();
      byEvent.set(eventId, waiters);
      let settled = false;
      const cleanup = () => {
        clearTimeout(timeout);
        options.signal?.removeEventListener("abort", onAbort);
        waiters.delete(waiter);
        if (waiters.size === 0) byEvent.delete(eventId);
        if (byEvent.size === 0) this.projectionEventCommitWaiters.delete(sessionId);
      };
      const waiter: ProjectionEventCommitWaiter = {
        resolve: () => {
          if (settled) return;
          settled = true;
          cleanup();
          resolve();
        },
        reject: (error) => {
          if (settled) return;
          settled = true;
          cleanup();
          reject(error);
        },
      };
      const onAbort = () => {
// Only rejecting the current waiter will keep the TurnStarted in the raw gap alive;
// Fill the gap after the command has been cancelled, and late events will still enter the canonical projection.
//Event failure must be solidified into the sequence state. Subsequent drain will only advance the cursor and no longer apply.
        this.rejectProjectionEventCommit(
          sessionId,
          eventId,
          new ProjectionEventCommitWaitError(
            "fault.projectionEventCommit.aborted",
            `projection event commit wait aborted: ${eventId}`,
            { cause: options.signal?.reason },
          ),
        );
      };
      const timeout = setTimeout(() => {
        this.rejectProjectionEventCommit(
          sessionId,
          eventId,
          new ProjectionEventCommitWaitError(
            "fault.projectionEventCommit.timeout",
            `projection event commit wait timed out: ${eventId}`,
          ),
        );
      }, PROJECTION_EVENT_COMMIT_TIMEOUT_MS);
      timeout.unref?.();
      waiters.add(waiter);
      options.signal?.addEventListener("abort", onAbort, { once: true });
    });
  }

/** Authorization has been submitted to the task transaction. Failure to retry must replay the authoritative log, and cannot elevate the authority again or discard the submission fact. */
  async waitForPermissionGrantCommit(sessionId: string, eventId: string): Promise<void> {
    const state = this.getOrCreateRawSequenceState(sessionId);
    if (state.failedEventById.has(eventId)) {
      const event = state.recentRawEventsById.get(eventId);
      if (
        event?.type !== SessionEventType.SessionModeChanged ||
        !(event.payload as { permissionGrant?: unknown }).permissionGrant
      ) {
        throw new Error("Permission grant event unavailable for recovery");
      }
      await this.hydrationInFlight.get(sessionId);
      this.hydratedSessions.delete(sessionId);
      await this.hydratePublisher(sessionId, undefined, true);
    }
    await this.waitForProjectionEventCommit(sessionId, eventId);
  }

  private ingestNormalizedEvent(sessionId: string, event: SessionEvent): void {
    const publisher = this.ensurePublisher(sessionId);
    const promotedQueueRemoval =
      event.type === SessionEventType.TurnSteerDiscarded &&
      (event.payload as { reason?: string }).reason === "promoted";
    const removedQueueItems =
      event.type === SessionEventType.TurnSteerDrained ||
      event.type === SessionEventType.TurnSteerDiscarded
        ? ((event.payload as { pendingInputIds?: string[] }).pendingInputIds ?? []).flatMap(
            (queueItemId) => {
              const item = publisher
                .getSnapshot()
                .queue.items.find((candidate) => candidate.queueItemId === queueItemId);
              return item ? [item] : [];
            },
          )
        : [];
    try {
      publisher.ingest(event);
    } catch (error) {
      const commitError =
        error instanceof ProjectionEventCommitWaitError
          ? error
          : new ProjectionEventCommitWaitError(
              "fault.projectionEventCommit.applyFailed",
              `projection failed to apply event ${String(event.id)}`,
              { cause: error },
            );
      this.rejectProjectionEventCommit(sessionId, String(event.id), commitError);
      if (!(error instanceof ProjectionPayloadTooLargeError)) throw error;
      this.host.onError?.("v4.projection.payloadTooLarge", error);
      if (!this.projectionFaultedSessions.has(sessionId)) {
        this.projectionFaultedSessions.add(sessionId);
        void Promise.resolve(
          this.host.terminateTurnForProjectionFault?.(sessionId, error.reasonCode),
        ).catch((terminateError) => {
          this.host.onError?.("v4.projection.terminate", terminateError);
        });
      }
      return;
    }
    this.resolveProjectionEventCommit(sessionId, String(event.id));
    if (
      event.type === SessionEventType.TargetChanged &&
      (event.payload as TargetChangedPayload).target?.status === "complete"
    ) {
      try {
        this.host.onTargetCompleted?.(sessionId, event);
      } catch (error) {
        this.host.onError?.("v4.projection.targetCompleted", error);
      }
    }
    if (event.type === SessionEventType.TurnComplete || event.type === SessionEventType.TurnError) {
      this.projectionFaultedSessions.delete(sessionId);
    }
    if (event.type === SessionEventType.TurnSteerQueued) {
      const queueItemId = (event.payload as { pendingInputId?: string }).pendingInputId;
      const item = queueItemId
        ? publisher
            .getSnapshot()
            .queue.items.find((candidate) => candidate.queueItemId === queueItemId)
        : undefined;
      if (item) this.inbox.pinLiveInput(sessionId, item);
    }
    if (!promotedQueueRemoval) {
      if (removedQueueItems.length > 0) {
// delete/clear has written durable session_input as canceled first, but this session
// The persistent command index may have cached old empty results. You must first invalidate the live pin before releasing it.
// Otherwise, the same commandId query may still be unknown and be executed repeatedly after LRU is eliminated.
        this.host.invalidatePersistentCommandFacts?.(sessionId);
      }
      for (const item of removedQueueItems) {
        this.inbox.releaseLiveInput({
          sessionId,
          commandId: item.sourceCommandId,
        });
      }
    }
    if (event.type === SessionEventType.SessionInputPromoted) {
      const sourceCommandId = (event.payload as { sourceCommandId?: string }).sourceCommandId;
      if (sourceCommandId) {
// The persistent index may have been queried earlier than this user message; invalidate it first and then unpin it.
// Only when the subsequent LRU is eliminated and returned to the source can the newly submitted transcript be re-read, instead of hitting the old empty seed.
        this.host.invalidatePersistentCommandFacts?.(sessionId);
        this.inbox.releaseLiveInput({ sessionId, commandId: sourceCommandId });
      }
    }
// assistant conservation: the projection rejected the text stream (publisher was created mid-subscription, missed
//Typical form of TurnStarted) → Cancel the hydrated mark, and the next subscription is forced to restart from the persistent fact
// hydration fills in the missing segments - silent loss will cause the content to be missing until the user manually refreshes it.
    if (publisher.getDroppedContentStreamEventCount() > 0 && this.hydratedSessions.has(sessionId)) {
      this.hydratedSessions.delete(sessionId);
      this.host.onError?.(
        "v4.assistantConservation",
        new Error(
          `projection dropped content stream events for session ${sessionId}; scheduling re-hydration`,
        ),
      );
    }
    for (const [routeKey, state] of this.flushStates) {
      if (state.sessionId !== sessionId) continue;
      this.scheduleFlush(routeKey, state, publisher);
    }
// sessions-index fan-out (defensive: no exceptions can interrupt the main conversation path).
    this.fanOutToIndex(sessionId, event);
  }

  /**
* The subagent child uses the external sink of the parent record, but retains an independent session topic. Explicitly register this class
* Detached live session to avoid misjudgment of any occasional cold publisher as a running child.
   */
  ingestDetachedLiveSession(
    sessionId: string,
    event: SessionEvent,
    parentSessionId?: string,
  ): void {
    if (!this.detachedLiveSessions.has(sessionId)) {
      this.host.onDebug?.(`register detached live child publisher session=${sessionId}`);
    }
    this.detachedLiveSessions.add(sessionId);
    if (parentSessionId && parentSessionId !== sessionId) {
      this.detachedChildParent.set(sessionId, parentSessionId);
      let children = this.detachedChildrenByParent.get(parentSessionId);
      if (!children) {
        children = new Set();
        this.detachedChildrenByParent.set(parentSessionId, children);
      }
      children.add(sessionId);
    }
// child is a one-time session, with no record and no subsequent turn. The publisher stayed until the process exited.
// Record the final state time for pruneDetachedChildPublishers to release after grace; it will be canceled if the child turns again.
    if (event.type === SessionEventType.TurnComplete || event.type === SessionEventType.TurnError) {
      this.detachedTerminalAt.set(sessionId, Date.now());
    } else if (event.type === SessionEventType.TurnStarted) {
      this.detachedTerminalAt.delete(sessionId);
    }
    this.ingest(sessionId, event);
  }

  /**
* Low-frequency tick cover: release the detached child publisher that is finalized, has no subscribers, and does not have its own record.
* After being released and then subscribed, there is a cold resume (child is persisted in the session store as subagent_child). Returns the number of releases.
   */
  pruneDetachedChildPublishers(
    nowMs: number = Date.now(),
    graceMs: number = DETACHED_CHILD_PUBLISHER_GRACE_MS,
  ): number {
    let released = 0;
    for (const [childId, terminalAt] of [...this.detachedTerminalAt]) {
      if (nowMs - terminalAt < graceMs) continue;
      if (this.host.sessionExists(childId)) continue;
      if (this.publishers.get(childId)?.hasSubscribers()) continue;
      this.releaseDetachedChild(childId);
      released += 1;
    }
    return released;
  }

  private releaseDetachedChild(childId: string): void {
    this.host.onDebug?.(`release detached live child publisher session=${childId}`);
    this.cleanupSessionRuntime(childId, { clearCommandInbox: false, notifyIndexRemoved: false });
  }

  /**
* Push the latest summary of a session to the sessions-index publisher of its workspace and flush it to list subscribers.
* Projection must continue to advance even when there are no list subscribers to ensure that the next snapshot reads the authoritative current state;
* High-frequency streaming increment (ModelStreaming) does not trigger list recalculation to avoid jitter (the preview is updated when the turn closes/other events).
* No getSessionWorkspaceId for old hosts → overall no-op.
   */
  private fanOutToIndex(sessionId: string, event: SessionEvent): void {
    if (event.type === SessionEventType.ModelStreaming) return;
    this.publishCurrentSummaryToIndex(sessionId);
  }

  /**
* Publish the current complete projection to sessions-index.
   *
* The resume of fork child will first use a small amount of live events to create a transient draft publisher.
* Subsequent synthesized hydration completes the inheritance history. If you fan-out only during ingest(event),
* If there is no next runtime event after hydration is completed, the child will always stop at the draft baseline.
* task-index syncer cannot observe draft→visible, so the sidebar task row will not be created.
   */
  private publishCurrentSummaryToIndex(sessionId: string): void {
    const getWorkspaceId = this.host.getSessionWorkspaceId;
    if (!getWorkspaceId) return;
    try {
      const workspaceId = getWorkspaceId.call(this.host, sessionId);
      if (!workspaceId) return;
      if (this.host.isDraftSession?.(sessionId)) return;
      const indexPublisher = this.indexPublishers.get(workspaceId);
      if (!indexPublisher) return;
      const conversationPublisher = this.publishers.get(sessionId);
      if (!conversationPublisher) return;
      const changed = indexPublisher.ingestConversation(
        conversationPublisher.getSnapshot(),
        this.resolveIndexMeta(sessionId),
      );
      if (changed) this.flushIndex(workspaceId);
    } catch (error) {
      this.host.onError?.("v4.sessionsIndex.ingest", error);
    }
  }

/** Session list meta information (default host hook: createdAt=0, lastActivityAt=now). */
  private resolveIndexMeta(sessionId: string): {
    createdAt: number;
    lastActivityAt: number;
    parentSessionId?: string;
  } {
    const meta = this.host.getSessionIndexMeta?.(sessionId);
    return {
      createdAt: meta?.createdAt ?? 0,
      lastActivityAt: meta?.lastActivityAt ?? this.now(),
      ...(meta?.parentSessionId ? { parentSessionId: meta.parentSessionId } : {}),
    };
  }

/** Push unsent incremental frames of a workspace index publisher to all list subscribers. */
  private flushIndex(workspaceId: string, onlyConnectionId?: string): void {
    const publisher = this.indexPublishers.get(workspaceId);
    if (!publisher) return;
    for (const subscriptionId of publisher.subscriptionIds()) {
      const connectionId = publisher.connectionIdForSubscription(subscriptionId);
      if (
        connectionId === null ||
        this.pausedConnections.has(connectionId) ||
        (onlyConnectionId !== undefined && connectionId !== onlyConnectionId)
      ) {
        continue;
      }
      const reservation = publisher.reserveFlush(subscriptionId);
      if (!reservation) continue;
      try {
        this.emitReservation(reservation);
      } catch (error) {
        this.host.onError?.("v4.sessionsIndex.emit", error);
      }
    }
  }

  /**
* sessions-index subscription: Subscribe to the session list of a workspace (parallel with conversation subscribe,
* The same RPC method is dispatched by topic prefix). Cold start: store digest seed + loaded session live projection override.
   */
  async subscribeSessionsIndex(
    rawParams: unknown,
  ): Promise<V4SubscribeDispatchResult<SessionsIndexTopicFrame>> {
    const dispatch = await this.subscribeSessionsIndexReserved(rawParams);
    dispatch.commit();
    return dispatch;
  }

  async subscribeSessionsIndexReserved(
    rawParams: unknown,
  ): Promise<V4SubscribeDispatchResult<SessionsIndexTopicFrame>> {
    const params = v4ConversationSubscribeParamsSchema.parse(rawParams);
    const workspaceId = parseSessionsIndexTopic(params.topic);
    if (workspaceId === null) {
      throw new Error(`Not a sessions-index topic: ${params.topic}`);
    }
    const publisher = await this.ensureIndexPublisher(workspaceId, params.legacyTaskIds);
// ensure internally may cross asynchronous store/claim; dispose occurs before await returns and prohibits continued registration of subscriptions.
    this.indexPublishers.ensureActive();
    const result = publisher.subscribeReserved(params.connectionId, params.base);
    try {
      return this.subscribeDispatch(
        {
          subscriptionId: result.subscriptionId,
          mode: result.mode,
          logEpoch: publisher.logEpoch,
        },
        result.reservation,
        () => this.flushIndex(workspaceId),
      );
    } catch (error) {
// The initial logical frame can fail due to the 16MiB upper limit in the physical encode stage;
// If the registered subscription/in-flight reservation fails, it must be rolled back, otherwise it will leave a ghost owner who can never unsubscribe.
      result.rollback();
      throw error;
    }
  }

/** Create/get the index publisher of a certain workspace; store summary seed + live projection coverage when building. */
  private async ensureIndexPublisher(
    workspaceId: string,
    legacyTaskIds?: readonly string[],
  ): Promise<SessionsIndexPublisher> {
    const existing = this.indexPublishers.get(workspaceId);
    const shouldRefreshLegacy =
      Boolean(legacyTaskIds?.length) && Boolean(this.host.refreshLegacySessionSummaries);
    if (existing && !shouldRefreshLegacy) return existing;

    return this.indexPublishers.runExclusive(workspaceId, () =>
      this.ensureIndexPublisherExclusive(workspaceId, legacyTaskIds),
    );
  }

/** Same as workspace serial area: retryable claim/reread and publisher constructs must observe the same final snapshot. */
  private async ensureIndexPublisherExclusive(
    workspaceId: string,
    legacyTaskIds?: readonly string[],
  ): Promise<SessionsIndexPublisher> {
    const refreshed =
      legacyTaskIds && legacyTaskIds.length > 0
        ? ((await this.host.refreshLegacySessionSummaries?.(workspaceId, legacyTaskIds)) ?? null)
        : null;
    const existing = this.indexPublishers.get(workspaceId);
    if (existing) {
// The claim cannot be bound to the first construction: once the empty seed enters the Map, it will permanently block retries.
// Re-read only fills in missing items to prevent the cold storage default state from overwriting the existing live projection.
      if (refreshed && existing.mergeMissingStoredSummaries(refreshed)) {
        this.flushIndex(workspaceId);
      }
      return existing;
    }
    const publisher = new SessionsIndexPublisher(
      workspaceId,
      this.createLogEpoch(`sessions-index/${workspaceId}`),
      this.now,
    );
// Seed 1: A lightweight summary of all sessions in the store (unloaded ones are listed by this).
    const stored = refreshed ?? (await this.host.getStoredSessionSummaries?.(workspaceId)) ?? [];
    for (const summary of stored) publisher.seed(summary);
// Seed 2: Loaded session overlay with live projection (more accurate phase/preview/backgroundWork).
    const liveIds = this.host.listWorkspaceSessionIds?.(workspaceId) ?? [...this.publishers.keys()];
    for (const sessionId of liveIds) {
      const conversationPublisher = this.publishers.get(sessionId);
      if (!conversationPublisher) continue;
//draft (the first deferred is not sent) does not enter the cold start seed, which is consistent with the filtering of fanOutToIndex.
      if (this.host.isDraftSession?.(sessionId)) continue;
      publisher.ingestConversation(
        conversationPublisher.getSnapshot(),
        this.resolveIndexMeta(sessionId),
      );
    }
    this.indexPublishers.set(workspaceId, publisher);
    return publisher;
  }

  /**
* workspace-config subscription: subscribe to the configuration directory of a workspace (parallel with conversation subscribe,
* The same RPC method is dispatched by topic prefix). When subscribing, the current configuration is pulled through the host hook as a seed.
   */
  async subscribeWorkspaceConfig(
    rawParams: unknown,
  ): Promise<V4SubscribeDispatchResult<WorkspaceConfigTopicFrame>> {
    const dispatch = await this.subscribeWorkspaceConfigReserved(rawParams);
    dispatch.commit();
    return dispatch;
  }

  async subscribeWorkspaceConfigReserved(
    rawParams: unknown,
  ): Promise<V4SubscribeDispatchResult<WorkspaceConfigTopicFrame>> {
    const params = v4ConversationSubscribeParamsSchema.parse(rawParams);
    const workspaceId = parseWorkspaceConfigTopic(params.topic);
    if (workspaceId === null) {
      throw new Error(`Not a workspace-config topic: ${params.topic}`);
    }
    const publisher = await this.ensureConfigPublisher(workspaceId);
    const result = publisher.subscribeReserved(params.connectionId, params.base);
    try {
      return this.subscribeDispatch(
        {
          subscriptionId: result.subscriptionId,
          mode: result.mode,
          logEpoch: publisher.logEpoch,
        },
        result.reservation,
        () => this.flushConfig(workspaceId),
      );
    } catch (error) {
// Same atomic boundary as sessions-index: encode failed = subscribe not admitted.
      result.rollback();
      throw error;
    }
  }

  /**
* Configuration directory publishing entry (called by the host after the provider registry application/workspace default item is changed,
* Directly carry the built directory without going back and re-pulling the host to avoid repeating the temporary app cost of buildWorkspaceState).
* The conflation is completed in the publisher (no frame is generated without changes); when there is no publisher, an empty seed is created simultaneously
* The publisher saves the latest status, and subsequent subscribers can get the complete snapshot accordingly.
   */
  publishWorkspaceConfig(workspaceId: string, state: WorkspaceConfigState): void {
    if (this.disposed) return;
    let publisher = this.configPublishers.get(workspaceId);
    if (!publisher) {
      publisher = new WorkspaceConfigPublisher(
        workspaceId,
        this.createLogEpoch(`workspace-config/${workspaceId}`),
        this.now,
      );
      this.configPublishers.set(workspaceId, publisher);
    }
    try {
      if (publisher.publish(state)) this.flushConfig(workspaceId);
    } catch (error) {
      this.host.onError?.("v4.workspaceConfig.publish", error);
    }
  }

  private async pullWorkspaceConfig(workspaceId: string): Promise<WorkspaceConfigState | null> {
    if (!this.host.getWorkspaceConfig) return null;
    return (await this.host.getWorkspaceConfig(workspaceId)) ?? null;
  }

/** Create/get the config publisher of a certain workspace; when building, pull the current directory through the host hook as a seed. */
  private async ensureConfigPublisher(workspaceId: string): Promise<WorkspaceConfigPublisher> {
    const existing = this.configPublishers.get(workspaceId);
    if (existing) return existing;
    const publisher = new WorkspaceConfigPublisher(
      workspaceId,
      this.createLogEpoch(`workspace-config/${workspaceId}`),
      this.now,
    );
    const seed = await this.pullWorkspaceConfig(workspaceId).catch((error) => {
      this.host.onError?.("v4.workspaceConfig.seed", error);
      return null;
    });
    if (seed) publisher.publish(seed);
// Concurrent subscriptions during await may have been registered with the workspace publisher → whichever is registered first.
    const raced = this.configPublishers.get(workspaceId);
    if (raced) return raced;
    this.configPublishers.set(workspaceId, publisher);
    return publisher;
  }

/** Push unsent incremental frames of a certain workspace config publisher to all subscribers. */
  private flushConfig(workspaceId: string, onlyConnectionId?: string): void {
    const publisher = this.configPublishers.get(workspaceId);
    if (!publisher) return;
    for (const subscriptionId of publisher.subscriptionIds()) {
      const connectionId = publisher.connectionIdForSubscription(subscriptionId);
      if (
        connectionId === null ||
        this.pausedConnections.has(connectionId) ||
        (onlyConnectionId !== undefined && connectionId !== onlyConnectionId)
      ) {
        continue;
      }
      const reservation = publisher.reserveFlush(subscriptionId);
      if (!reservation) continue;
      try {
        this.emitReservation(reservation);
      } catch (error) {
        this.host.onError?.("v4.workspaceConfig.emit", error);
      }
    }
  }

/** v4/conversation/subscribe: Judgment + server internal initial frame, the public response is only ACK taken by the server. */
  async subscribe(rawParams: unknown): Promise<V4SubscribeDispatchResult<ConversationTopicFrame>> {
    const dispatch = await this.subscribeReserved(rawParams);
    dispatch.commit();
    return dispatch;
  }

  async subscribeReserved(
    rawParams: unknown,
  ): Promise<V4SubscribeDispatchResult<ConversationTopicFrame>> {
    const params = v4ConversationSubscribeParamsSchema.parse(rawParams);
    const sessionId = parseConversationTopic(params.topic);
    if (sessionId === null) {
      throw new Error(`Unsupported topic: ${params.topic}`);
    }
    const isLiveConversation = this.hasLiveConversation(sessionId);
    this.host.onDebug?.(
      `subscribe conversation session=${sessionId} coldResume=${String(!isLiveConversation)}`,
    );
// Hydration: Rebuild the projection from the authoritative source when first subscribing.
// - None publisher(cold) → build + replay.
// - There is a publisher but the event log cannot cover the transcript (fork child: resume's ingest takes precedence
// Create a cold publisher containing only fork events) → Use transcript synthesis to **rebuild**.
// - publisher is present and event log is complete (streaming live) → retained, replay will double count and interrupt the stream.
    const restoreStartedAt = performance.now();
    const existingReady = this.readyFlights.get(sessionId);
    const publisher = existingReady
      ? await existingReady
      : !isLiveConversation
        ? await this.ensureColdReadyPublisher(
            sessionId,
            params.resumeThoughtLevel,
            params.workspace,
          )
        : await this.hydratePublisher(sessionId);
    const cliSessionRestoreMs = !isLiveConversation
      ? Math.max(0, Math.round(performance.now() - restoreStartedAt))
      : undefined;
// The old entrance allows the UI to select deliveryProfile, and it will default to the deliveryProfile if the desktop call is missed.
//replayable. Now only trusted clientMode injected by host attachment is recognized.
    const profileName = params.clientMode === "desktop-continuous" ? "continuous" : "replayable";
// subscribeReserved wire projection has been constructed; if the timing starts after it, the large session
// Line filtering/window truncation will fall outside the restore and encode sections. The starting point must cover the build with physical encode.
    const initialFrameEncodeStartedAt = performance.now();
    const result = publisher.subscribeReserved({
      connectionId: params.connectionId,
      base: params.base,
      deliveryProfile: profileName,
    });
    const routeKey = subscriptionRouteKey(
      params.topic,
      result.ack.subscriptionId,
      params.connectionId,
    );
    let dispatch: V4SubscribeDispatchResult<ConversationTopicFrame>;
    try {
      dispatch = this.subscribeDispatch(result.ack, result.reservation, () => {
        const state = this.flushStates.get(routeKey);
        if (state) this.scheduleFlush(routeKey, state, publisher);
      });
      dispatch.ack = {
        ...dispatch.ack,
        openTiming: {
          version: 1,
          ...(cliSessionRestoreMs !== undefined ? { cliSessionRestoreMs } : {}),
          initialFrameEncodeMs: Math.max(
            0,
            Math.round(performance.now() - initialFrameEncodeStartedAt),
          ),
          sessionRuntimeState: isLiveConversation ? "warm" : "cold",
          snapshotRowCount: publisher.getSnapshot().rows.window.length,
        },
      };
    } catch (error) {
// When reordering initial encode fails, the client still holds the old subId; replacement is required
// Atomic rollback, old publisher subscription and flush timer are still valid.
      result.rollback();
      throw error;
    }
// Replacement is admitted only after encode is successful; at this time, the old scheduling status is cleared, and the failed path does not touch the old owner.
    for (const [staleRouteKey, staleState] of this.flushStates) {
      if (staleState.sessionId !== sessionId) continue;
      if (publisher.hasSubscription(staleState.subscriptionId, staleState.connectionId)) {
        continue;
      }
      if (staleState.timer) clearTimeout(staleState.timer);
      this.flushStates.delete(staleRouteKey);
    }
    this.flushStates.set(routeKey, {
      sessionId,
      topic: params.topic,
      subscriptionId: result.ack.subscriptionId,
      connectionId: params.connectionId,
      deliveryProfile: profileName,
      flushWindowMs: DELIVERY_PROFILES[profileName].flushWindowMs,
      timer: null,
    });
    return dispatch;
  }

  /**
* v4/conversation/resync: Accurately hit existing subscription by owned topic/connection,
* Keep subId/profile unchanged and resume/snapshot from client base.
   */
  resyncReserved(rawParams: unknown): V4SubscribeDispatchResult<RoutedTopicFrame> {
    const params = v4ConversationResyncParamsSchema.parse(rawParams);
    const request = {
      base: params.base,
      ...(params.forceSnapshot !== undefined ? { forceSnapshot: params.forceSnapshot } : {}),
    };
    const sessionId = parseConversationTopic(params.topic);
    if (sessionId !== null) {
      const publisher = this.publishers.get(sessionId);
      if (!publisher?.hasSubscription(params.subscriptionId, params.connectionId)) {
        throw new Error("fault.subscription.notOwned");
      }
      const routeKey = subscriptionRouteKey(
        params.topic,
        params.subscriptionId,
        params.connectionId,
      );
      const flushState = this.flushStates.get(routeKey);
      if (flushState?.timer) {
        clearTimeout(flushState.timer);
        flushState.timer = null;
      }
      const result = publisher.resyncReserved(params.subscriptionId, request);
      if (!result) throw new Error("fault.subscription.notOwned");
      try {
        return this.subscribeDispatch(result.ack, result.reservation, () => {
          const state = this.flushStates.get(routeKey);
          if (state) this.scheduleFlush(routeKey, state, publisher);
        });
      } catch (error) {
// same-sub recovery when physical encode fails before ACK admission
// Cannot leave new inFlight or cancel old online flush; rehang timer after atomically restoring old state.
        result.rollback();
        if (flushState) this.scheduleFlush(routeKey, flushState, publisher);
        throw error;
      }
    }

    const indexWorkspaceId = parseSessionsIndexTopic(params.topic);
    if (indexWorkspaceId !== null) {
      const publisher = this.indexPublishers.get(indexWorkspaceId);
      if (!publisher?.hasSubscription(params.subscriptionId, params.connectionId)) {
        throw new Error("fault.subscription.notOwned");
      }
      const result = publisher.resyncReserved(params.subscriptionId, request);
      if (!result) throw new Error("fault.subscription.notOwned");
      try {
        return this.subscribeDispatch(
          {
            subscriptionId: result.subscriptionId,
            mode: result.mode,
            logEpoch: publisher.logEpoch,
          },
          result.reservation,
          () => this.flushIndex(indexWorkspaceId),
        );
      } catch (error) {
        result.rollback();
        throw error;
      }
    }

    const configWorkspaceId = parseWorkspaceConfigTopic(params.topic);
    if (configWorkspaceId !== null) {
      const publisher = this.configPublishers.get(configWorkspaceId);
      if (!publisher?.hasSubscription(params.subscriptionId, params.connectionId)) {
        throw new Error("fault.subscription.notOwned");
      }
      const result = publisher.resyncReserved(params.subscriptionId, request);
      if (!result) throw new Error("fault.subscription.notOwned");
      try {
        return this.subscribeDispatch(
          {
            subscriptionId: result.subscriptionId,
            mode: result.mode,
            logEpoch: publisher.logEpoch,
          },
          result.reservation,
          () => this.flushConfig(configWorkspaceId),
        );
      } catch (error) {
        result.rollback();
        throw error;
      }
    }
    throw new Error(`Unsupported topic: ${params.topic}`);
  }

  /**
* v4/conversation/rowsRange: Press the beforeRowId cursor to take a window upwards
* History line. Read-only query, no subscription is created; data source = the session projects all rows - cold session (open directly after restart
* History) Reuse the same cold recovery + hydration pipeline as subscribe to build the projection first.
   */
  async rowsRange(rawParams: unknown): Promise<V4ConversationRowsRangeResult> {
    const params = v4ConversationRowsRangeParamsSchema.parse(rawParams);
    const existingReady = this.readyFlights.get(params.sessionId);
    const publisher = existingReady
      ? await existingReady
      : !this.hasLiveConversation(params.sessionId)
        ? await this.ensureColdReadyPublisher(params.sessionId)
        : await this.hydratePublisher(params.sessionId);
    return publisher.getRowsRange(
      {
        ...(params.beforeRowId !== undefined ? { beforeRowId: params.beforeRowId } : {}),
        limit: params.limit,
      },
// clientMode determines the row visibility filtering gear: desktop continuous (default) / disconnection recovery replayable.
      params.clientMode === "desktop-continuous" ? "continuous" : "replayable",
    );
  }

/** Final state plan directory of complete and valid projection; cold session reuse subscription hydration. */
  async plans(rawParams: unknown): Promise<V4ConversationPlansResult> {
    const params = v4ConversationPlansParamsSchema.parse(rawParams);
    const existingReady = this.readyFlights.get(params.sessionId);
    const publisher = existingReady
      ? await existingReady
      : !this.hasLiveConversation(params.sessionId)
        ? await this.ensureColdReadyPublisher(params.sessionId)
        : await this.hydratePublisher(params.sessionId);
    return publisher.getPlans();
  }

  /**
* Workflow run event log paging reading (cursor = journal sequence).
   *
* Same family as rows/range and plans: read-only, stateless, timeout retransmission safe. Deliberately **not** v4 command——
* The ACK result of command is the closed "change result" judgment combination, and the one-page read-only event does not belong to that vocabulary.
   *
* `hasMore` is determined by the "full limit": reading one more item to confirm that there is more behind it is better than letting the renderer rely on "this page is just full"
* Guessing is more reliable (you won’t turn an empty page when you get it right).
   */
  async workflowRunEvents(rawParams: unknown): Promise<V4ConversationWorkflowRunEventsResult> {
    const params = v4ConversationWorkflowRunEventsParamsSchema.parse(rawParams);
    if (!this.host.listDynamicWorkflowRunEvents) {
      throw new V4CapabilityUnsupportedError("listDynamicWorkflowRunEvents", params.sessionId);
    }
    await this.ensureHostRecordForJournalRead(params.sessionId);
    const limit = params.limit;
    const events = await this.host.listDynamicWorkflowRunEvents(params.sessionId, {
      runId: params.runId,
      ...(params.afterSequence === undefined ? {} : { afterSequence: params.afterSequence }),
// Take one more item just to determine hasMore; it does not enter the results page.
      ...(limit === undefined ? {} : { limit: limit + 1 }),
    });
    const hasMore = limit !== undefined && events.length > limit;
    return v4ConversationWorkflowRunEventsResultSchema.parse({
      events: hasMore ? events.slice(0, limit) : events,
      hasMore,
    });
  }

  /**
* Enumeration query for dwf run. and
* Same family as workflowRunEvents: read-only, stateless, timeout retransmission safe. The default and clamping of limit are on the CLI side
* (run service), here only transparent transmission is performed; `resumable` is calculated by the CLI according to the same predicate of the resume gate.
   */
  async workflowRuns(rawParams: unknown): Promise<V4ConversationWorkflowRunsResult> {
    const params = v4ConversationWorkflowRunsParamsSchema.parse(rawParams);
    if (!this.host.listDynamicWorkflowRuns) {
      throw new V4CapabilityUnsupportedError("listDynamicWorkflowRuns", params.sessionId);
    }
    await this.ensureHostRecordForJournalRead(params.sessionId);
    const runs = await this.host.listDynamicWorkflowRuns(params.sessionId, {
      ...(params.limit === undefined ? {} : { limit: params.limit }),
    });
    return v4ConversationWorkflowRunsResultSchema.parse({ runs });
  }

  /**
* List of **user interface products** for workflow run.
* Same family as workflowRunEvents: read-only, stateless, timeout retransmission safe.
   *
* ⚠ Terminology: The artifact here is the output of the script published to the user through `artifact.*`, not the top level of run
* Return value (the same name for the latter within the engine).
   *
* Unknown runId returns empty list instead of error: a run that has been eliminated/never existed has no artifacts, this is a
* Fact rather than bug - see event log handling of out-of-bounds cursor for the same gesture.
   */
  async workflowRunArtifacts(
    rawParams: unknown,
  ): Promise<V4ConversationWorkflowRunArtifactsResult> {
    const params = v4ConversationWorkflowRunArtifactsParamsSchema.parse(rawParams);
    if (!this.host.listDynamicWorkflowRunArtifacts) {
      throw new V4CapabilityUnsupportedError("listDynamicWorkflowRunArtifacts", params.sessionId);
    }
    await this.ensureHostRecordForJournalRead(params.sessionId);
    const artifacts = await this.host.listDynamicWorkflowRunArtifacts(params.sessionId, {
      runId: params.runId,
    });
    return v4ConversationWorkflowRunArtifactsResultSchema.parse({ artifacts: artifacts ?? [] });
  }

  /**
* The access side of the preset kanban board: feed the `report` entry pagination of a certain product.
   *
* The **defaults and clamps of `limit` are all here** (the storage layer is implemented accurately, never creates the page size by itself, and never clamps again); `hasMore` takes an additional judgment according to the convention of workflowRunEvents - the criterion must not be "this page is exactly full",
* That will cause a false alarm when the number of entries is exactly equal to the limit, causing the Kanban board to turn to a page of non-existent data.
   */
  async workflowRunArtifactData(
    rawParams: unknown,
  ): Promise<V4ConversationWorkflowRunArtifactDataResult> {
    const params = v4ConversationWorkflowRunArtifactDataParamsSchema.parse(rawParams);
    if (!this.host.listDynamicWorkflowRunArtifactItems) {
      throw new V4CapabilityUnsupportedError(
        "listDynamicWorkflowRunArtifactItems",
        params.sessionId,
      );
    }
    await this.ensureHostRecordForJournalRead(params.sessionId);
    const limit = Math.max(
      1,
      Math.min(
        params.limit ?? WORKFLOW_ARTIFACT_LIMITS.defaultItemsPerPage,
        WORKFLOW_ARTIFACT_LIMITS.maxItemsPerPage,
      ),
    );
    const items = await this.host.listDynamicWorkflowRunArtifactItems(params.sessionId, {
      runId: params.runId,
      artifactId: params.artifactId,
      ...(params.afterSequence === undefined ? {} : { afterSequence: params.afterSequence }),
// Take one more item just to determine hasMore; it does not enter the results page.
      limit: limit + 1,
    });
    const hasMore = items.length > limit;
    return v4ConversationWorkflowRunArtifactDataResultSchema.parse({
      items: hasMore ? items.slice(0, limit) : items,
      hasMore,
    });
  }

  /**
* Bytes of content product, **verbatim attachmentRead**: one piece at a time, ≤ 512 KiB (schema limit has been pinned)
* upper bound), if `nextOffset` is null, read to the end.
   *
* **Authorization is all on the host side** (port implementation): The run must belong to the session `sessionId` ∧ in the journal
* The completed line of `(artifactId, version)`, and then take the ** uri on the ** line to the store to read. The gateway only does
* Parameter verification and chunking - it does not have a journal, and there should not be a second authorization criterion (one judgment in each place, the same id
* Sooner or later it will get different interpretations on both levels). The host returns `undefined` = no such version / not your run /
* This is a kanban board (without bytes). The three are the same business fact to the caller. They are unified here into structured not found.
   *
* `offset` out of bounds is not an error: return an empty block + `nextOffset: null`, the same form as reading to the end.
   */
  async workflowRunArtifactRead(
    rawParams: unknown,
  ): Promise<V4ConversationWorkflowRunArtifactReadResult> {
    const params = v4ConversationWorkflowRunArtifactReadParamsSchema.parse(rawParams);
    if (!this.host.readDynamicWorkflowRunArtifact) {
      throw new V4CapabilityUnsupportedError("readDynamicWorkflowRunArtifact", params.sessionId);
    }
    await this.ensureHostRecordForJournalRead(params.sessionId);
    const artifact = await this.readWorkflowArtifactPayload(params);
    const totalBytes = artifact.bytes.byteLength;
    const start = Math.min(params.offset, totalBytes);
    const end = Math.min(start + params.limit, totalBytes);
    const chunk = artifact.bytes.subarray(start, end);
    return v4ConversationWorkflowRunArtifactReadResultSchema.parse({
      dataBase64: Buffer.from(chunk).toString("base64"),
      mediaType: artifact.mediaType,
      totalBytes,
      nextOffset: end < totalBytes ? end : null,
    });
  }

  /**
* List of workspace transcripts: a run
* `files.*` / `git.*` / `world.run` lines, without text.
   *
* The host returns `undefined` (unknown run / not your run) and gets an empty list instead of an error: the same behavior as the product list,
* It is also a requirement of the authorization chain to "not tell the person who has exceeded the authority which half guessed correctly". List exceeds maxNodes truncated and concatenated `truncated`——
* Running `world.run` three thousand times in a loop should not burst the side panels.
   */
  async workflowRunWorkspace(
    rawParams: unknown,
  ): Promise<V4ConversationWorkflowRunWorkspaceResult> {
    const params = v4ConversationWorkflowRunWorkspaceParamsSchema.parse(rawParams);
    if (!this.host.listDynamicWorkflowRunWorkspaceNodes) {
      throw new V4CapabilityUnsupportedError(
        "listDynamicWorkflowRunWorkspaceNodes",
        params.sessionId,
      );
    }
    await this.ensureHostRecordForJournalRead(params.sessionId);
    const nodes =
      (await this.host.listDynamicWorkflowRunWorkspaceNodes(params.sessionId, {
        runId: params.runId,
      })) ?? [];
    const truncated = nodes.length > WORKFLOW_WORKSPACE_LIMITS.maxNodes;
    return v4ConversationWorkflowRunWorkspaceResultSchema.parse({
      nodes: truncated ? nodes.slice(0, WORKFLOW_WORKSPACE_LIMITS.maxNodes) : nodes,
      ...(truncated ? { truncated: true } : {}),
    });
  }

  /**
* The text of a workspace node is conformally bounded by `maxBytes` (the default and upper limit are both resultMaxBytes, clamped here).
* Authorization is all on the host side; the host returns `undefined` = no such node / not your run / not world line, normalized
* Structured not found.
   */
  async workflowRunNodeResult(
    rawParams: unknown,
  ): Promise<V4ConversationWorkflowRunNodeResultResult> {
    const params = v4ConversationWorkflowRunNodeResultParamsSchema.parse(rawParams);
    if (!this.host.readDynamicWorkflowRunNodeResult) {
      throw new V4CapabilityUnsupportedError("readDynamicWorkflowRunNodeResult", params.sessionId);
    }
    await this.ensureHostRecordForJournalRead(params.sessionId);
    const maxBytes = Math.max(
      1,
      Math.min(
        params.maxBytes ?? WORKFLOW_WORKSPACE_LIMITS.resultMaxBytes,
        WORKFLOW_WORKSPACE_LIMITS.resultMaxBytes,
      ),
    );
    const result = await this.host.readDynamicWorkflowRunNodeResult(params.sessionId, {
      runId: params.runId,
      siteId: params.siteId,
      ordinal: params.ordinal,
      maxBytes,
    });
    if (result === undefined) {
      throw new Error(
        `fault.workflowRunNodeResult.notFound: ${params.runId}/${params.siteId}@${params.ordinal}`,
      );
    }
    return v4ConversationWorkflowRunNodeResultResultSchema.parse(result);
  }

  /**
* The **entire** bytes of a production version, cached.
   *
* The port's `readArtifact` returns the entire byte, while
* `workflowRunArtifactRead` is a **chunked** query - without caching, a 20 MiB PDF is 512 KiB
* If fetched in 40 blocks, the entire file will be read from the store 40 times (800 MiB I/O), and each block must be read again.
* journal authorization chain. `attachmentRead` already has this table, reuse it here (see {@link BinaryReadCacheEntry}
* Argument about two families sharing a table).
   *
* The **promise is cached instead of the result**, and is written into the table before being initiated: multiple chunks fetched concurrently are therefore shared
* Read at the same time, instead of initiating each and then writing to the cache.
   *
* Authorization is not bypassed due to caching: the key contains `sessionId`, and `sessionId` is the authorization chain of the port
* (the parentSessionId of run must be equal to it) comparison object - changing a session means another key, and must be re-
* Walk the port once. When the session is destroyed, press `sessionId` to clear the entire session, which is the same rule as attachments.
   *
* The host returns `undefined` (not your run / no such version / a kanban board) here **throws an error** instead of being cached:
* Use the existing catch branch to delete the entry, so that a race condition of "the release has just been dropped and the read is a little early" will not be eliminated
* Negative cache pegs for 30 seconds.
   */
  private readWorkflowArtifactPayload(params: {
    sessionId: string;
    runId: string;
    artifactId: string;
    version: number;
  }): Promise<{ bytes: Uint8Array; mediaType: string }> {
    const now = this.now();
    this.pruneBinaryReadCache(now);
// The first paragraph tag `dwfart`: shares the same table with the attachment preview and is isolated by the first paragraph (see BinaryReadCacheEntry).
    const key = `dwfart\u0000${params.sessionId}\u0000${params.runId}\u0000${params.artifactId}\u0000${params.version}`;
    const cached = this.binaryReadCache.get(key);
    if (cached) {
      cached.accessedAt = now;
      return cached.payload;
    }

    const payload = this.host.readDynamicWorkflowRunArtifact!(params.sessionId, {
      runId: params.runId,
      artifactId: params.artifactId,
      version: params.version,
    })
      .then((artifact) => {
        if (artifact === undefined) {
          throw new Error(
            `fault.workflowRunArtifactRead.notFound: ${params.runId}/${params.artifactId}@${params.version}`,
          );
        }
        const current = this.binaryReadCache.get(key);
        if (current) {
          current.bytes = artifact.bytes.byteLength;
          this.binaryReadCacheBytes += artifact.bytes.byteLength;
          this.pruneBinaryReadCache(this.now());
        }
// contentType is normalized to the mediaType vocabulary in the table; the value is still the one in the journal record
// (The exact matching contract of the UI dispatch renderer), not the one pushed by the store by file name.
        return { bytes: artifact.bytes, mediaType: artifact.contentType };
      })
      .catch((error: unknown) => {
        this.deleteBinaryReadCacheEntry(key);
        throw error;
      });
    this.binaryReadCache.set(key, {
      sessionId: params.sessionId,
      accessedAt: now,
      bytes: null,
      payload,
    });
    return payload;
  }

  /**
* The host record frontend for the two journal reading surfaces of dwf.
   *
* Both queries use the app to read the journal, and the host searches for the record by sessionId - history
* Session records are only activated by the **subscription** path. In the renderer, it is found that the effect statement of the query is in lease/subscription
* before effect, while the CLI dispatched requests strictly serially (`zcode-protocol/transport.ts`, only session/stop
* Crossing the queue), so when "opening the historical session after restarting", it must be processed before the subscription and must get sessionNotFound:
* The entire join block of the tool card disappears, the card returns to the compiled state, and the interrupted run does not even have an entrance.
   *
* Only pull records, **not** build READY publisher: journal has nothing to do with conversation log, read one page run
* No projection is required (for the same judgment, see that these two queries deliberately do not include atSeq/atLogEpoch). Idioms with attachmentBegin
* Literally the same; `ensureResumed` comes with per-session solo flight and shares the same activation with concurrent subscriptions.
   *
* The activity determination must be the same as subscribe
* `hasLiveConversation`, cannot just look at `sessionExists`. dwf actor transcript is detached live
* Session - the real runtime lives in the run service, the host deliberately does not record; the discovery of nested SessionPane
* The query comes here with the actor id, and the old decision materializes a second (ghost) for a **running** session.
* runtime: It appends SessionResumed, discards pending steer, and replays resume hooks to the same event log.
* Double writing messes up the sequence account, and the transcript freezes (the symptom is that the live broadcast freezes at "xx seconds worked").
   */
  private async ensureHostRecordForJournalRead(sessionId: string): Promise<void> {
    if (this.hasLiveConversation(sessionId)) return;
    await this.coldResume.ensureResumed(sessionId);
  }

  async fileChanges(rawParams: unknown): Promise<V4ConversationFileChangesResult> {
    const params = v4ConversationFileChangesParamsSchema.parse(rawParams);
    if (!this.host.getConversationFileChanges) {
      throw new Error("fault.fileChanges.unsupported");
    }
    const existingReady = this.readyFlights.get(params.sessionId);
    const publisher = existingReady
      ? await existingReady
      : !this.hasLiveConversation(params.sessionId)
        ? await this.ensureColdReadyPublisher(params.sessionId)
        : await this.hydratePublisher(params.sessionId);
    const resolution = this.resolveQueryRowTarget(publisher, params, "fileChanges");
    const messageIds = resolution.messageIds ?? [];
    const targetTurnId = toRuntimeTurnId(resolution.row.turnId);
    return this.host.getConversationFileChanges(
      params.sessionId,
      params.target.rowId,
      messageIds,
      targetTurnId,
    );
  }

  async backgroundBashOutput(rawParams: unknown): Promise<BackgroundBashOutputResult> {
    const { sessionId, workId } = v4BackgroundBashOutputParamsSchema.parse(rawParams);
// Observation queries cannot hydrate/restore cold sessions; tasks are authorized by the existing runtime.
    if (!this.host.readBackgroundBashOutput) return { kind: "unsupported", workId };
    return backgroundBashOutputResultSchema.parse(
      await this.host.readBackgroundBashOutput(sessionId, workId),
    );
  }

  async fileRewindPreview(rawParams: unknown): Promise<V4ConversationFileRewindPreviewResult> {
    const params = v4ConversationFileRewindPreviewParamsSchema.parse(rawParams);
    if (!this.host.previewConversationFileRewind) {
      throw new Error("fault.fileRewindPreview.unsupported");
    }
    const existingReady = this.readyFlights.get(params.sessionId);
    const publisher = existingReady
      ? await existingReady
      : !this.host.sessionExists(params.sessionId)
        ? await this.ensureColdReadyPublisher(params.sessionId)
        : await this.hydratePublisher(params.sessionId);
    const resolution = this.resolveQueryRowTarget(publisher, params, "fileRewindPreview");
    const messageIds = resolution.messageIds ?? [];
    const targetTurnId = toRuntimeTurnId(resolution.row.turnId);
    return this.host.previewConversationFileRewind(
      params.sessionId,
      params.target.rowId,
      messageIds,
      targetTurnId,
    );
  }

  private resolveQueryRowTarget(
    publisher: ConversationTopicPublisher,
    params: {
      target: ConversationRowTarget;
      baseRevision: number;
      baseLogEpoch: string;
    },
    action: "fileChanges" | "fileRewindPreview",
  ): Extract<ReturnType<ConversationTopicPublisher["resolveRowActionTarget"]>, { ok: true }> {
    const snapshot = publisher.getSnapshot();
    if (params.baseLogEpoch !== snapshot.logEpoch) throw new Error("proto.staleLogEpoch");
    if (params.baseRevision !== snapshot.revision) throw new Error("proto.staleRevision");
    const resolution = publisher.resolveRowActionTarget(params.target, action);
    if (!resolution.ok) throw new Error(resolution.reasonCode);
    return resolution;
  }

/** begin only admits metadata, does not decode/stage the full payload. */
  async attachmentBegin(rawParams: unknown): Promise<V4AttachmentBeginResult> {
    const params = v4AttachmentBeginParamsSchema.parse(rawParams);
    if (!this.host.putSessionAttachment) {
      throw new Error("fault.attachment.putUnsupported");
    }
    if (!this.host.sessionExists(params.sessionId)) {
      await this.coldResume.ensureResumed(params.sessionId);
    }
    return this.attachmentUploads.begin(params);
  }

  async attachmentChunk(rawParams: unknown): Promise<V4AttachmentChunkResult> {
    return this.attachmentUploads.chunk(v4AttachmentChunkParamsSchema.parse(rawParams));
  }

  attachmentCommit(rawParams: unknown): Promise<V4AttachmentCommitResult> {
    return this.attachmentUploads.commit(v4AttachmentCommitParamsSchema.parse(rawParams));
  }

  async attachmentAbort(rawParams: unknown): Promise<void> {
    await this.attachmentUploads.abort(v4AttachmentAbortParamsSchema.parse(rawParams));
  }

  async attachmentRead(rawParams: unknown): Promise<V4AttachmentReadResult> {
    const params = v4AttachmentReadParamsSchema.parse(rawParams);
    if (!this.host.readSessionAttachment) {
      throw new Error("fault.attachment.readUnsupported");
    }
    const existingReady = this.readyFlights.get(params.sessionId);
    const publisher = existingReady
      ? await existingReady
      : !this.host.sessionExists(params.sessionId)
        ? await this.ensureColdReadyPublisher(params.sessionId)
        : await this.hydratePublisher(params.sessionId);
    const resolution = this.resolveReadableMediaAttachment(
      publisher,
      params.sessionId,
      params.ref,
      params.target,
      params.attachmentIndex,
    );
    if (!resolution) {
// The ref passed by renderer cannot directly become a file path; it must first be passed by the current session
// The authoritative user row proves ownership and avoids reading across sessions or arbitrary paths.
      throw new Error("fault.attachment.previewRefNotAuthorized");
    }

    const payload = await this.readAttachmentPayload(
      params.sessionId,
      params.ref,
      resolution.attachment.mime,
      resolution.messageId,
      resolution.attachmentIndex,
    );
    if (params.offset > payload.bytes.byteLength) {
      throw new Error("fault.attachment.previewRangeInvalid");
    }
    const end = Math.min(payload.bytes.byteLength, params.offset + params.limit);
    const chunk = payload.bytes.subarray(params.offset, end);
    return {
      dataBase64: Buffer.from(chunk).toString("base64"),
      mediaType: payload.mediaType,
      totalBytes: payload.bytes.byteLength,
      nextOffset: end < payload.bytes.byteLength ? end : null,
    };
  }

  async conversationAttachmentRead(
    rawParams: unknown,
  ): Promise<V4ConversationAttachmentReadResult> {
    const params = v4ConversationAttachmentReadParamsSchema.parse(rawParams);
    if (!this.host.readSessionAttachment) {
      throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.readUnsupported);
    }
    const existingReady = this.readyFlights.get(params.sessionId);
    const publisher = existingReady
      ? await existingReady
      : !this.host.sessionExists(params.sessionId)
        ? await this.ensureColdReadyPublisher(params.sessionId)
        : await this.hydratePublisher(params.sessionId);
    const row = publisher
      .getSnapshot()
      .rows.window.find(
        (candidate) =>
          candidate.rowId === params.target.rowId && candidate.entityId === params.target.entityId,
      );
    if (row?.kind !== "userInput") {
      throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.shareReadNotAuthorized);
    }
    const attachment = row.attachments?.[params.attachmentIndex];
    if (!attachment || (attachment.ref !== params.ref && attachment.previewRef !== params.ref)) {
      throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.shareReadNotAuthorized);
    }
    const messageId = publisher.getMessageIdForRow(row.rowId) ?? undefined;
    let payload: { bytes: Uint8Array; mediaType: string };
    try {
      payload = await this.readAttachmentPayload(
        params.sessionId,
        params.ref,
        attachment.mime,
        messageId,
        params.attachmentIndex,
        true,
      );
    } catch (error) {
      throw toShareStatFault(error);
    }
    if (params.offset > payload.bytes.byteLength) {
      throw new Error("fault.attachment.previewRangeInvalid");
    }
    const end = Math.min(payload.bytes.byteLength, params.offset + params.limit);
    const chunk = payload.bytes.subarray(params.offset, end);
    return v4ConversationAttachmentReadResultSchema.parse({
      dataBase64: Buffer.from(chunk).toString("base64"),
      mediaType: payload.mediaType,
      totalBytes: payload.bytes.byteLength,
      nextOffset: end < payload.bytes.byteLength ? end : null,
    });
  }

  async conversationAttachmentStat(
    rawParams: unknown,
  ): Promise<V4ConversationAttachmentStatResult> {
    const params = v4ConversationAttachmentStatParamsSchema.parse(rawParams);
    if (!this.host.statSessionAttachment) {
      throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.statUnsupported);
    }
    const existingReady = this.readyFlights.get(params.sessionId);
    const publisher = existingReady
      ? await existingReady
      : !this.host.sessionExists(params.sessionId)
        ? await this.ensureColdReadyPublisher(params.sessionId)
        : await this.hydratePublisher(params.sessionId);
    const row = publisher
      .getSnapshot()
      .rows.window.find(
        (candidate) =>
          candidate.rowId === params.target.rowId && candidate.entityId === params.target.entityId,
      );
    if (row?.kind !== "userInput") {
      throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.shareStatNotAuthorized);
    }
    const attachment = row.attachments?.[params.attachmentIndex];
    if (!attachment || (attachment.ref !== params.ref && attachment.previewRef !== params.ref)) {
      throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.shareStatNotAuthorized);
    }
    const messageId = publisher.getMessageIdForRow(row.rowId) ?? undefined;
    let result: { totalBytes: number; mediaType: string; mtimeMs?: number };
    try {
      result = await this.host.statSessionAttachment(params.sessionId, {
        ref: params.ref,
        mime: attachment.mime,
        ...(messageId ? { messageId } : {}),
        attachmentIndex: params.attachmentIndex,
      });
    } catch (error) {
// "The attachment is indeed no longer there" is the only category that the share pre-check can definitely determine as skipped, and must be thrown up with a stable code;
// Otherwise the service can only guess the error text.
      throw toShareStatFault(error);
    }
// The stat result was stuck by the 30MiB schema upper limit, and a ZodError was thrown here for oversized attachments.
// So the share preflight downgrades the "known capacity exceeds limit" definite block to deferred and silently discards the content.
// After the upper limit is relaxed, an explicit exit is still needed: a stable code is given when it really exceeds the expressible range of the protocol.
    if (result.totalBytes > PROTOCOL_V4_LIMITS.attachmentStatMaxBytes) {
      throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.shareStatTooLarge);
    }
    return v4ConversationAttachmentStatResultSchema.parse(result);
  }

  async attachmentPreviewSource(rawParams: unknown): Promise<V4AttachmentPreviewSourceResult> {
    const params = v4AttachmentPreviewSourceParamsSchema.parse(rawParams);
    const existingReady = this.readyFlights.get(params.sessionId);
    const publisher = existingReady
      ? await existingReady
      : !this.host.sessionExists(params.sessionId)
        ? await this.ensureColdReadyPublisher(params.sessionId)
        : await this.hydratePublisher(params.sessionId);
    const resolution = this.resolveReadableMediaAttachment(
      publisher,
      params.sessionId,
      params.ref,
      params.target,
      params.attachmentIndex,
    );
    if (!resolution) {
      throw new Error("fault.attachment.previewRefNotAuthorized");
    }
    if (
      params.clientMode !== "desktop-continuous" ||
      !resolution.attachment.mime.startsWith("video/") ||
      !this.host.resolveSessionAttachmentPreviewSource
    ) {
      return { kind: "chunked" };
    }
    const result = await this.host.resolveSessionAttachmentPreviewSource(params.sessionId, {
      ref: params.ref,
      mime: resolution.attachment.mime,
      ...(resolution.messageId ? { messageId: resolution.messageId } : {}),
      ...(resolution.attachmentIndex !== undefined
        ? { attachmentIndex: resolution.attachmentIndex }
        : {}),
    });
    return v4AttachmentPreviewSourceResultSchema.parse(result);
  }

  private resolveReadableMediaAttachment(
    publisher: ConversationTopicPublisher,
    sessionId: string,
    ref: string,
    target?: { rowId: number; entityId: string },
    attachmentIndex?: number,
  ): { attachment: AttachmentRef; messageId?: string; attachmentIndex?: number } | null {
    const isPreviewable = (attachment: AttachmentRef) => {
      const mime = attachment.mime.split(";", 1)[0]?.trim().toLowerCase() ?? "";
      return mime.startsWith("image/") || mime.startsWith("video/") || mime === "application/pdf";
    };
    const matchesRef = (attachment: AttachmentRef) =>
      attachment.ref === ref || attachment.previewRef === ref;
    if (target && attachmentIndex !== undefined) {
      const row = publisher
        .getSnapshot()
        .rows.window.find(
          (candidate) => candidate.rowId === target.rowId && candidate.entityId === target.entityId,
        );
      if (row?.kind !== "userInput") return null;
      const attachment = row.attachments?.[attachmentIndex];
      if (!attachment || !isPreviewable(attachment) || !matchesRef(attachment)) {
        return null;
      }
// The hot renderer may still hold the original ref, but the authoritative row after hydrate has been filled in
// previewRef; both belong to the same row/index, and the authorization cannot be misjudged as cross-row reading due to different projection timings.
      const messageId = publisher.getMessageIdForRow(row.rowId);
      return {
        attachment,
        attachmentIndex,
        ...(messageId ? { messageId } : {}),
      };
    }

// The old renderer has no row target and cannot locate persistent artifacts by message; once
// If previewRef exists, only the durable ref can be authorized, and the variable original path cannot be re-released.
    for (const row of publisher.getSnapshot().rows.window) {
      if (row.kind === "userInput") {
        for (const attachment of row.attachments ?? []) {
          if (!isPreviewable(attachment)) continue;
          if ((attachment.previewRef ?? attachment.ref) === ref) return { attachment };
        }
      }
      if (
        row.kind === "assistantText" &&
        artifactRefBelongsToSession(ref, sessionId) &&
        extractMarkdownArtifactImageRefs(row.text).includes(ref)
      ) {
// assistant Markdown can reference the session artifact produced by the tool,
// But the old authorization only checks userInput.attachments, causing legitimate images to be harden after reaching the UI.
//Intercept. Still use the authoritative projection of the current session for accurate ref authorization, never accept renderer
// Any self-reported artifact/path. Markdown is model-controllable text, so URI authority
// It must also exactly match the current request session; just "appearing in the current projection" does not prove that it has the right
// Read artifacts from another session.
        return {
          attachment: {
            ref,
            fileName: "assistant-image",
            mime: "image/*",
            bytes: 0,
          },
        };
      }
    }
    return null;
  }

  /**
* Read all bytes of attachment (with TTL/capacity cache).
   *
* Note the semantics: the offset/limit of conversationAttachmentRead is **slicing**, not streaming reading——
* Each first request will materialize the entire attachment into memory and then slice it, and subsequent chunks will hit the same cache.
* The access party should not treat the chunk protocol as "on-demand segmented pull" to plan very large files; real range reading
* Requires host-side readBinaryFile to support offset (not yet implemented).
   */
  private readAttachmentPayload(
    sessionId: string,
    ref: string,
    mime: string,
    messageId?: string,
    attachmentIndex?: number,
    allowGeneric = false,
  ): Promise<{ bytes: Uint8Array; mediaType: string }> {
    const now = this.now();
    this.pruneBinaryReadCache(now);
// First section tag `att`: This table is shared with dwf product bytes (see BinaryReadCacheEntry), two key spaces
// Can only be isolated by an impossible first paragraph.
    const key = `att\u0000${sessionId}\u0000${messageId ?? "legacy"}\u0000${attachmentIndex ?? -1}\u0000${ref}`;
    const cached = this.binaryReadCache.get(key);
    if (cached) {
      cached.accessedAt = now;
      return cached.payload;
    }

// Preview reads the 20MiB total limit of reused uploads; video use has a global input limit,
// image and upload transactions continue to maintain the original boundaries.
    const maxBytes = allowGeneric
      ? PROTOCOL_V4_LIMITS.attachmentPreviewMaxBytes
      : mime.startsWith("video/")
        ? PROTOCOL_V4_LIMITS.attachmentPreviewMaxBytes
        : PROTOCOL_V4_LIMITS.attachmentMaxBytes;
    const payload = this.host.readSessionAttachment!(sessionId, {
      ref,
      mime,
      maxBytes,
      ...(messageId ? { messageId } : {}),
      ...(attachmentIndex !== undefined ? { attachmentIndex } : {}),
    })
      .then((result) => {
        const resultMime = result.mediaType.split(";", 1)[0]?.trim().toLowerCase() ?? "";
        if (
          !allowGeneric &&
          !resultMime.startsWith("image/") &&
          !resultMime.startsWith("video/") &&
          resultMime !== "application/pdf"
        ) {
          throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.previewNotMedia);
        }
        if (result.bytes.byteLength > maxBytes) {
          throw new ZCodeAttachmentFaultError(ZCODE_ATTACHMENT_FAULT_CODES.previewTooLarge);
        }
        const current = this.binaryReadCache.get(key);
        if (current) {
          current.bytes = result.bytes.byteLength;
          this.binaryReadCacheBytes += result.bytes.byteLength;
          this.pruneBinaryReadCache(this.now());
        }
        return result;
      })
      .catch((error) => {
        this.deleteBinaryReadCacheEntry(key);
        throw error;
      });
    this.binaryReadCache.set(key, { sessionId, accessedAt: now, bytes: null, payload });
    return payload;
  }

  private pruneBinaryReadCache(now = this.now()): void {
    for (const [key, entry] of this.binaryReadCache) {
      if (now - entry.accessedAt > PROTOCOL_V4_LIMITS.attachmentReadCacheTtlMs) {
        this.deleteBinaryReadCacheEntry(key);
      }
    }
    if (this.binaryReadCacheBytes <= PROTOCOL_V4_LIMITS.attachmentReadCacheMaxBytes) return;
    const oldest = [...this.binaryReadCache.entries()]
      .filter(([, entry]) => entry.bytes !== null)
      .sort((left, right) => left[1].accessedAt - right[1].accessedAt);
    for (const [key] of oldest) {
      this.deleteBinaryReadCacheEntry(key);
      if (this.binaryReadCacheBytes <= PROTOCOL_V4_LIMITS.attachmentReadCacheMaxBytes) break;
    }
  }

  private deleteBinaryReadCacheEntry(key: string): void {
    const entry = this.binaryReadCache.get(key);
    if (!entry) return;
    this.binaryReadCache.delete(key);
    this.binaryReadCacheBytes = Math.max(0, this.binaryReadCacheBytes - (entry.bytes ?? 0));
  }

  /** v4/conversation/unsubscribe。 */
  unsubscribe(rawParams: unknown): void {
    const params = v4ConversationUnsubscribeParamsSchema.parse(rawParams);
    const sessionId = parseConversationTopic(params.topic);
    if (sessionId === null) {
      const workspaceId = parseSessionsIndexTopic(params.topic);
      if (workspaceId !== null) {
        this.indexPublishers
          .get(workspaceId)
          ?.unsubscribe(params.subscriptionId, params.connectionId);
        return;
      }
      const configWorkspaceId = parseWorkspaceConfigTopic(params.topic);
      if (configWorkspaceId !== null) {
        this.configPublishers
          .get(configWorkspaceId)
          ?.unsubscribe(params.subscriptionId, params.connectionId);
      }
      return;
    }
    const routeKey = subscriptionRouteKey(params.topic, params.subscriptionId, params.connectionId);
    const state = this.flushStates.get(routeKey);
    if (!state) return;
    if (state?.timer) clearTimeout(state.timer);
    this.flushStates.delete(routeKey);
//Bare subscriptionId can collide in different topics/connections; the old gateway first presses the subId
// Back-check and then cast a wide net on the three types of publishers, and other links will be deleted. topic + connection must hit at the same time.
    this.publishers.get(sessionId)?.unsubscribe(params.subscriptionId, params.connectionId);
  }

  /**
* v4/command: inbox six-state ruling; when accepted, perform side effects and return the final state with the response.
   *
* Here we have "immediately return initial ACK, background settle",
* As a result, the caller of createSession/forkAssistant cannot get result.sessionId (settle only backfills
* Idempotent table, can only be read by retrying with the same commandId) - violation
* "Accepted immediately brings result". The command side effect itself returns quickly (sendPrompt turns in the background),
* await will not hang RPC until the end of the entire turn, so it will wait for the final state synchronously.
* settle still solidifies the results for duplicate replay.
   */
  async handleCommand(rawParams: unknown): Promise<CommandAck> {
    let ttftCapacityRejected = false;
    const ttftCommand =
      typeof rawParams === "object" && rawParams !== null && "ttft" in rawParams
        ? parseCommandEnvelope(rawParams)
        : undefined;
    if (ttftCommand?.ok && ttftCommand.envelope.ttft) {
      const sessionId = ttftCommand.envelope.sessionId;
      const control = sessionId ? this.publishers.get(sessionId)?.getSnapshot().control : undefined;
      ttftCapacityRejected = !this.localTtft.receive(
        ttftCommand.envelope,
        control?.canStop === true,
      );
    }
// READY only exists in the cold recovery window; normal commands enter the inbox directly to avoid repeated envelope parsing.
    if (this.readyFlights.size > 0) {
      const parsed = parseCommandEnvelope(rawParams);
      const sessionId = parsed.ok ? parsed.envelope.sessionId : null;
      const ready = sessionId === null ? undefined : this.readyFlights.get(sessionId);
      if (ready) await ready;
    }

    const outcome = await this.inbox.handle(rawParams);
    if (outcome.kind === "ack")
      return {
        ...outcome.ack,
        ...(ttftCapacityRejected ? { ttftExcluded: "capacity" as const } : {}),
      };
    this.localTtft.admitted(outcome.envelope.commandId);
    let durableInputIntent: ConversationInputIntent | null = null;
    let settledAck: CommandAck | null = null;
    type CommandFinal = Parameters<typeof outcome.settle>[0];
    const reportError = (scope: string, error: unknown): void => {
      try {
        this.host.onError?.(scope, error);
      } catch {
// The error observer cannot reversely destroy the closing of command final and session FIFO.
      }
    };
    const settleOnce = (final: CommandFinal): CommandAck => {
      if (settledAck) return settledAck;
      const ack = {
        ...outcome.ack,
        ...final,
        ...(ttftCapacityRejected ? { ttftExcluded: "capacity" as const } : {}),
      };
      outcome.settle(final);
      settledAck = ack;
      return ack;
    };
    const cancelDurableInput = async (reason: string): Promise<void> => {
      if (!durableInputIntent) return;
      try {
        await this.host.cancelCommandInput?.(outcome.envelope, outcome.queueItemId, reason);
      } catch (cancelError) {
// The original command ACK must retain the actual execution result; if the ledger cancel fails, a separate alarm will be issued and the original error cannot be overwritten.
        reportError("v4.command.input.cancel", cancelError);
      }
    };
    const releaseDurableInput = (
      final: Pick<CommandAck, "status" | "reasonCode" | "message" | "result">,
    ) => {
      if (!durableInputIntent || outcome.envelope.sessionId === null) return;
      try {
        this.inbox.releaseLiveInput(
          {
            sessionId: outcome.envelope.sessionId,
            commandId: durableInputIntent.sourceCommandId,
          },
          { ...outcome.ack, ...final },
        );
      } catch (releaseError) {
        reportError("v4.command.input.release", releaseError);
      }
    };
    try {
      const admission = {
        admissionSeq: outcome.admissionSeq,
        admittedAt: outcome.admittedAt,
        queueItemId: outcome.queueItemId,
      };
      const admissionPublisher =
        outcome.envelope.type === "createSession"
          ? new ConversationTopicPublisher(`pending-${outcome.envelope.commandId}`, "admission", {
              now: this.now,
            })
          : outcome.envelope.sessionId === null
            ? null
            : this.ensurePublisher(outcome.envelope.sessionId);
      const admissionProjectionBytes = admissionPublisher?.measureInputAdmissionProjectionBytes(
        outcome.envelope,
        admission,
      );
      if (
        admissionProjectionBytes !== null &&
        admissionProjectionBytes !== undefined &&
        admissionProjectionBytes > PROTOCOL_V4_LIMITS.logicalFrameAssemblyMaxBytes
      ) {
        return settleOnce({
          status: "failed",
          reasonCode: "proto.payloadTooLarge",
          message: `conversation projection would exceed ${PROTOCOL_V4_LIMITS.logicalFrameAssemblyMaxBytes} bytes`,
        });
      }
      durableInputIntent =
        (await this.host.admitCommandInput?.(outcome.envelope, admission)) ?? null;
      if (durableInputIntent && outcome.envelope.sessionId !== null) {
        this.inbox.pinLiveInput(outcome.envelope.sessionId, durableInputIntent);
      }
      const result = await this.host.executeCommand(outcome.envelope, admission);
// The new/side chat command uses the switch of the result session to avoid mistakenly recording the parent session or current App settings to the new session.
      const telemetrySessionId =
        result?.type === "createSession" || result?.type === "createSelectionSideSession"
          ? result.sessionId
          : outcome.envelope.sessionId;
      const memoryEnabled = telemetrySessionId
        ? this.host.getSessionMemoryEnabled?.(telemetrySessionId)
        : undefined;
      const final = {
        status: "accepted" as const,
        ...(result ? { result } : {}),
        ...(memoryEnabled !== undefined ? { memoryEnabled } : {}),
      };
      return settleOnce(final);
    } catch (error) {
// The noop is not a failure (the same value switch is closed): no onError is entered, and the noop ACK is returned.
      if (error instanceof V4CommandNoopError) {
        await cancelDurableInput(error.reasonCode);
        const final = {
          status: "noop" as const,
          reasonCode: error.reasonCode,
        };
        releaseDurableInput(final);
        return settleOnce(final);
      }
      reportError("v4.command.execute", error);
// Domain error carrying reasonCode (V4PromptRejectedError / heldQueueDispositionRequired, etc.)
//Upload as it is, the client can be diverted according to the guard error code; otherwise, it will be normalized to executionFailed.
      const domainReasonCode =
        typeof (error as { reasonCode?: unknown } | null)?.reasonCode === "string"
          ? String((error as { reasonCode: string }).reasonCode)
          : null;
      const final = {
        status: "failed" as const,
        reasonCode:
          error instanceof V4CommandNotImplementedError
            ? "fault.command.notImplemented"
            : (domainReasonCode ?? "fault.command.executionFailed"),
        message: error instanceof Error ? error.message : String(error),
      };
      await cancelDurableInput(final.reasonCode);
      releaseDurableInput(final);
      return settleOnce(final);
    } finally {
      if (!settledAck) {
// publisher/measure/admission Any sync exception used to skip settle,
// This will cause the same command to wait forever and the same session FIFO to be unable to continue admission.
        const final = {
          status: "failed" as const,
          reasonCode: "fault.command.executionFailed",
          message: "command admission terminated before a durable final was recorded",
        };
        releaseDurableInput(final);
        settleOnce(final);
      }
    }
  }

/** v4/commands/query: Shared CommandInbox gate with key and handleCommand. */
  async queryCommands(rawParams: unknown): Promise<CommandsQueryResult> {
    const receivedAt = localTtftNow();
    const params = commandsQueryParamsSchema.parse(rawParams);
// Calibration is a pure clock detection and cannot trigger command ledger query, recovery or admission gate.
    if (params.clock)
      return {
        results: params.commands.map((key) => ({ key, result: "unknown" as const })),
        clock: { instanceId: this.localTtft.instanceId, receivedAt, sentAt: localTtftNow() },
      };
    await Promise.all(
      params.commands.map((key) => {
        const ready = key.sessionId === null ? undefined : this.readyFlights.get(key.sessionId);
        return ready;
      }),
    );
    return commandsQueryResultSchema.parse({
      results: await this.inbox.query(params.commands),
    });
  }

  getQueueItem(sessionId: string, queueItemId: string): QueueItem | null {
    const snapshot = this.publishers.get(sessionId)?.getSnapshot();
    const item = snapshot?.queue.items.find((candidate) => candidate.queueItemId === queueItemId);
    return item ?? null;
  }

  hasQueueItemKind(sessionId: string, kind: QueueItem["kind"]): boolean {
    return Boolean(
      this.publishers
        .get(sessionId)
        ?.getSnapshot()
        .queue.items.some((candidate) => candidate.kind === kind),
    );
  }

  hasQueuedDelivery(sessionId: string, delivery: "guide" | "queue"): boolean {
    return Boolean(
      this.publishers
        .get(sessionId)
        ?.getSnapshot()
        .queue.items.some((candidate) => candidate.delivery.admitted === delivery),
    );
  }

  getQueueLength(sessionId: string): number {
    return this.publishers.get(sessionId)?.getSnapshot().queue.items.length ?? 0;
  }

/** Resident recycling protection: neither publisher queue nor CommandInbox pinned facts can be turned off. */
  hasResidencyBlockingCommands(sessionId: string): boolean {
    return this.getQueueLength(sessionId) > 0 || this.inbox.hasPinnedSessionState(sessionId);
  }

  getQueueHead(sessionId: string): {
    autoDrain: boolean;
    dispatchState: QueueItem["dispatch"]["state"];
    kind: QueueItem["kind"];
    queueItemId: string;
    text: string;
  } | null {
    const snapshot = this.publishers.get(sessionId)?.getSnapshot();
    const item = snapshot?.queue.items[0];
    if (!snapshot || !item) return null;
    return {
      autoDrain: snapshot.queue.autoDrain,
      dispatchState: item.dispatch.state,
      kind: item.kind,
      queueItemId: item.queueItemId,
      text: item.text,
    };
  }

  /**
* Current input routing mode (v4 native capability, used by command layer host.getInputRoutingMode):
* held choice ruling (heldQueueInputRequiresChoice) read projection inputRouting.mode.
   */
  getInputRoutingMode(
    sessionId: string,
  ): "startNow" | "enqueue" | "guide" | "reject" | "choice" | null {
    return this.publishers.get(sessionId)?.getSnapshot().inputRouting.mode ?? null;
  }

  getSessionFollowupMode(sessionId: string): "queue" | "guide" | null {
    return this.publishers.get(sessionId)?.getSnapshot().config.followupMode ?? null;
  }

  /**
* rowId → authoritative messageId (v4 native capability, for forkAssistant/retryTurn to locate assistant row).
* There is no publisher in the session / the line does not exist / the non-assistant line → null (the command layer rejects accordingly and does not silently accept the issue).
   */
  getMessageIdForRow(sessionId: string, rowId: number): string | null {
    return this.publishers.get(sessionId)?.getMessageIdForRow(rowId) ?? null;
  }

  resolveRowActionTarget(
    sessionId: string,
    target: ConversationRowTarget,
    action: ConversationRowTargetAction,
  ) {
    return this.publishers.get(sessionId)?.resolveRowActionTarget(target, action) ?? null;
  }

/** rowId → all transcript messageIds in the product turn (file summary revocation/diff query). */
  getMessageIdsForTurnRow(sessionId: string, rowId: number): string[] {
    return this.publishers.get(sessionId)?.getMessageIdsForTurnRow(rowId) ?? [];
  }

/** The fork target must be the last segment of assistantText of the corresponding round (no projection → null, treated as unknown). */
  isLatestAssistantSegmentRow(sessionId: string, rowId: number): boolean | null {
    return this.publishers.get(sessionId)?.isLatestAssistantSegmentRow(rowId) ?? null;
  }

  resolveStableForkCandidate(sessionId: string, rowId: number) {
    return this.publishers.get(sessionId)?.resolveStableForkCandidate(rowId) ?? null;
  }

/** latestAssistantRetryOnly: The retry target must be the latest assistantText in the entire timeline and have realUser cause. */
  isLatestRetryAssistantRow(sessionId: string, rowId: number): boolean | null {
    return this.publishers.get(sessionId)?.isLatestRetryAssistantRow(rowId) ?? null;
  }

/** latestQueryEditOnly: The edit target must be the last realUser userInput row in the current projection. */
  isLatestEditableUserRow(sessionId: string, rowId: number): boolean | null {
    return this.publishers.get(sessionId)?.isLatestEditableUserRow(rowId) ?? null;
  }

/** rowId → product turnId (check user messageId when editUserQuery has no assistant anchor). */
  getTurnIdForRow(sessionId: string, rowId: number): string | null {
    return this.publishers.get(sessionId)?.getTurnIdForRow(rowId) ?? null;
  }

  /**
* rowId → the rewind anchor messageId of the owning turn (for editUserQuery: user row has no messageId,
* Use the messageId of the assistant line in the same turn as the `/rewind` target).
   */
  getTurnRewindAnchor(sessionId: string, rowId: number): string | null {
    return this.publishers.get(sessionId)?.getTurnRewindAnchor(rowId) ?? null;
  }

/** Session close: Clear the publisher and all its subscription schedules; clear the hydration mark (restart and rebuild after cold startup);
* and remove the session from its workspace index (session.removed is pushed to list subscribers). */
  disposeSession(sessionId: string): void {
    this.cleanupSessionRuntime(sessionId, {
      clearCommandInbox: false,
      notifyIndexRemoved: true,
    });
  }

  /**
* Resident capacity deactivation: same memory runtime cleanup as disposeSession, but not from sessions-index
* Remove the session (do not send session.removed) - deactivation is a pure memory optimization, the sidebar list items must be intact
* Reserved, subscribe again for transparent rebuild via cold recovery.
   */
  deactivateSession(sessionId: string): void {
    this.cleanupSessionRuntime(sessionId, {
      clearCommandInbox: true,
      notifyIndexRemoved: false,
    });
  }

  /**
* Resident recycling pure preflight: the caller can reject unsafe recycling before tearing down the runtime event subscription.
* The same verification is still reused in deactivateSession to prevent new callers from bypassing the execution surface preflight in the future.
   */
  assertSessionRuntimeDeactivatable(sessionId: string): void {
    if (!this.inbox.hasPinnedSessionState(sessionId)) return;
    throw new Error(`Session command inbox is still pinned: ${sessionId}`);
  }

/** Resident recycling determination: whether the session still has conversation subscribers (desktop tab / mobile phone remote). */
  hasConversationSubscribers(sessionId: string): boolean {
    return this.publishers.get(sessionId)?.hasSubscribers() ?? false;
  }

  /**
* Memory diagnostic counter. Read only size, no touch state.
* detachedLive is used to observe whether the child session publisher is released with the parent session.
   */
  collectMemoryDiagnostics(): Record<string, number> {
    return {
      publishers: this.publishers.size,
      detachedLive: this.detachedLiveSessions.size,
      detachedTerminal: this.detachedTerminalAt.size,
      rawSeqStates: this.rawSequenceStates.size,
    };
  }

  private cleanupSessionRuntime(
    sessionId: string,
    options: { clearCommandInbox: boolean; notifyIndexRemoved: boolean },
  ): void {
    if (options.clearCommandInbox) {
// Clearing the in-flight/live command will destroy idempotence and FIFO. resident facts already before recycling
// Interception; if it still hits here, it must fail before removing the publisher, and cannot leave a half-clear state.
      this.assertSessionRuntimeDeactivatable(sessionId);
    }
    this.rejectProjectionEventWaiters(
      sessionId,
      new ProjectionEventCommitWaitError(
        "fault.projectionEventCommit.disposed",
        `conversation session disposed while waiting for projection event commit: ${sessionId}`,
      ),
    );
    this.attachmentUploads.clearSession(sessionId);
    for (const [key, entry] of this.binaryReadCache) {
      if (entry.sessionId === sessionId) this.deleteBinaryReadCacheEntry(key);
    }
    if (options.notifyIndexRemoved) {
// First get the workspaceId (while the session record is still there), and push session.removed to the list subscribers.
      try {
        const workspaceId = this.host.getSessionWorkspaceId?.(sessionId) ?? null;
        if (workspaceId !== null) {
          const indexPublisher = this.indexPublishers.get(workspaceId);
// Even when there are no subscribers, the projection must be updated first to avoid existing publishers from updating the projection next time.
// Resurrection of deleted sessions in subscribe's snapshot; flushIndex is naturally no-op for empty subscriptions.
          if (indexPublisher?.removeSession(sessionId)) {
            this.flushIndex(workspaceId);
          }
        }
      } catch (error) {
        this.host.onError?.("v4.sessionsIndex.remove", error);
      }
    }
    for (const [routeKey, state] of this.flushStates) {
      if (state.sessionId !== sessionId) continue;
      if (state.timer) clearTimeout(state.timer);
      this.flushStates.delete(routeKey);
    }
    this.publishers.delete(sessionId);
    this.hydratedSessions.delete(sessionId);
    const hydrationBuffer = this.hydrationBuffers.get(sessionId);
    if (hydrationBuffer) hydrationBuffer.cancelled = true;
    this.hydrationBuffers.delete(sessionId);
    this.hydrationInFlight.delete(sessionId);
    this.readyFlights.delete(sessionId);
    this.rawSequenceStates.delete(sessionId);
    if (options.clearCommandInbox) this.inbox.clearSession(sessionId);
    this.telemetryNormalizer.clearSession(sessionId);
    this.detachedLiveSessions.delete(sessionId);
    this.projectionFaultedSessions.delete(sessionId);
// Detached child ownership cleanup: as a child, remove yourself from the parent table; as a parent, you will also release the child without a record.
    this.detachedTerminalAt.delete(sessionId);
    const parentId = this.detachedChildParent.get(sessionId);
    if (parentId !== undefined) {
      this.detachedChildParent.delete(sessionId);
      const siblings = this.detachedChildrenByParent.get(parentId);
      siblings?.delete(sessionId);
      if (siblings && siblings.size === 0) this.detachedChildrenByParent.delete(parentId);
    }
    const children = this.detachedChildrenByParent.get(sessionId);
    if (children) {
      this.detachedChildrenByParent.delete(sessionId);
      for (const childId of children) {
        this.detachedChildParent.delete(childId);
        if (this.host.sessionExists(childId)) continue;
        this.releaseDetachedChild(childId);
      }
    }
  }

  dispose(): void {
    this.disposed = true;
    this.localTtft.clear();
    for (const sessionId of this.projectionEventCommitWaiters.keys()) {
      this.rejectProjectionEventWaiters(
        sessionId,
        new ProjectionEventCommitWaitError(
          "fault.projectionEventCommit.gatewayDisposed",
          "conversation gateway disposed while waiting for projection event commit",
        ),
      );
    }
    clearInterval(this.attachmentPruneTimer);
    this.attachmentUploads.clear();
    this.binaryReadCache.clear();
    this.binaryReadCacheBytes = 0;
    for (const state of this.flushStates.values()) {
      if (state.timer) clearTimeout(state.timer);
    }
    this.flushStates.clear();
    this.publishers.clear();
    this.hydratedSessions.clear();
    for (const buffer of this.hydrationBuffers.values()) buffer.cancelled = true;
    this.hydrationBuffers.clear();
    this.hydrationInFlight.clear();
    this.readyFlights.clear();
    this.rawSequenceStates.clear();
    this.telemetryEventIds.clear();
    this.detachedLiveSessions.clear();
    this.detachedChildParent.clear();
    this.detachedChildrenByParent.clear();
    this.detachedTerminalAt.clear();
    this.projectionFaultedSessions.clear();
    this.coldResume.clear();
    this.indexPublishers.dispose();
    this.configPublishers.clear();
    this.pausedConnections.clear();
  }

/** Test probe: Drain a subscription immediately (bypassing the timer). */
  flushNow(subscriptionId: string): ConversationTopicFrame | null {
    const match = [...this.flushStates.entries()].find(
      ([, state]) => state.subscriptionId === subscriptionId,
    );
    const state = match?.[1];
    if (!state) return null;
    if (this.pausedConnections.has(state.connectionId)) return null;
    if (state.timer) {
      clearTimeout(state.timer);
      state.timer = null;
    }
    const publisher = this.publishers.get(state.sessionId);
    if (!publisher) return null;
    const reservation = publisher.reserveFlush(state.subscriptionId);
    if (!reservation || !reservation.commit()) return null;
    return reservation.frame;
  }

  private ensurePublisher(sessionId: string): ConversationTopicPublisher {
    let publisher = this.publishers.get(sessionId);
    if (!publisher) {
      publisher = new ConversationTopicPublisher(sessionId, this.createLogEpoch(sessionId), {
        now: this.now,
      });
      this.publishers.set(sessionId, publisher);
// config seed: Create and inject runtime true values ​​(no delta / no bump revision).
      this.seedPublisherConfig(sessionId, publisher);
    }
    return publisher;
  }

  /**
* config seed injection (defensive: seed failure does not interrupt the conversation main path).
* Idempotent and event priority (seedConfig skips fields touched by events), so when publisher creates and
* Hydration is called in both places at the end - the creation time may be earlier than the record is fully in place (createSessionRecord
* During event wiring), hydration is added once to cover the window.
   */
  private seedPublisherConfig(sessionId: string, publisher: ConversationTopicPublisher): void {
    const getSeed = this.host.getSessionConfigSeed;
    if (!getSeed) return;
    try {
      const seed = getSeed.call(this.host, sessionId);
      if (seed) publisher.seedConfig(seed);
    } catch (error) {
      this.host.onError?.("v4.configSeed", error);
    }
  }

  /**
* Cold recovery READY is only created at entries that explicitly require activation; hydratePublisher remains projection-only.
* Register promises earlier than activation to prevent concurrent command/query from exceeding the recovery water level after record is registered in advance.
   */
  private ensureColdReadyPublisher(
    sessionId: string,
    resumeThoughtLevel?: string,
    workspace?: ZCodeWorkspaceRef,
  ): Promise<ConversationTopicPublisher> {
    const existingFlight = this.readyFlights.get(sessionId);
    if (existingFlight) return existingFlight;
// Register the same READY first, and then start all time-consuming work.
    const operation = Promise.resolve().then(async () => {
      const persistedMessages = await this.coldResume.ensureResumed(
        sessionId,
        resumeThoughtLevel,
        workspace,
      );
      return this.hydratePublisher(sessionId, persistedMessages);
    });
    this.readyFlights.set(sessionId, operation);
// Both success and failure are released by the same cleanup function; no derived promises are created that would propagate rejection repeatedly.
    const clear = () => {
      if (this.readyFlights.get(sessionId) === operation) this.readyFlights.delete(sessionId);
    };
    void operation.then(clear, clear);
    return operation;
  }

  /**
* Projection reconstruction (hydration) on first subscription. See the subscribe annotation for semantics;
* Synthesized events are deduplicated according to sequenceNumber (live events that have been ingested by the publisher are not replayed).
   */
  private hydratePublisher(
    sessionId: string,
    persistedMessages?: MessageWithParts[],
    forceRebuild = false,
  ): Promise<ConversationTopicPublisher> {
    const existing = this.publishers.get(sessionId);
// hydrated live publisher: direct reuse (avoiding repeated reconstruction/double counting).
    if (existing && this.hydratedSessions.has(sessionId)) return Promise.resolve(existing);

    const inFlight = this.hydrationInFlight.get(sessionId);
    if (inFlight) return inFlight;
    const buffer: HydrationBuffer = {
      cancelled: false,
      eventIds: new Set<string>(),
      rawEvents: [],
    };
    this.hydrationBuffers.set(sessionId, buffer);
    const hydration = this.performHydration(
      sessionId,
      buffer,
      persistedMessages,
      forceRebuild,
    ).finally(() => {
      if (this.hydrationBuffers.get(sessionId) === buffer) {
        this.hydrationBuffers.delete(sessionId);
      }
      if (this.hydrationInFlight.get(sessionId) === hydration) {
        this.hydrationInFlight.delete(sessionId);
      }
    });
    this.hydrationInFlight.set(sessionId, hydration);
    return hydration;
  }

  private async performHydration(
    sessionId: string,
    buffer: HydrationBuffer,
    persistedMessages?: MessageWithParts[],
    forceRebuild = false,
  ): Promise<ConversationTopicPublisher> {
    const existingAtStart = this.publishers.get(sessionId);
    const liveSessionAtStart = this.host.sessionExists(sessionId);
    const hydrationStartedAt = performance.now();
    this.host.onDebug?.(
      `v4 hydrate started session=${sessionId} liveSessionAtStart=${String(liveSessionAtStart)} ` +
        `existingPublisherAtStart=${String(existingAtStart !== undefined)} ` +
        `persistedMessages=${String(persistedMessages?.length ?? 0)}`,
    );
    const loaded: PersistedEventsLoadResult = this.host.loadPersistedEvents
      ? await this.host.loadPersistedEvents(sessionId, persistedMessages).catch((error) => {
          this.host.onError?.("v4.hydrate", error, {
            durationMs: Math.max(0, Math.round(performance.now() - hydrationStartedAt)),
            existingPublisherAtStart: existingAtStart !== undefined,
            liveSessionAtStart,
            phase: "loadPersistedEvents",
            persistedMessages: persistedMessages?.length ?? 0,
            sessionId,
          });
          return { events: [] as SessionEvent[], synthesized: false, sourceEventSeq: 0 };
        })
      : { events: [] as SessionEvent[], synthesized: false, sourceEventSeq: 0 };

    this.host.onDebug?.(
      `v4 hydrate loaded session=${sessionId} events=${loaded.events.length} ` +
        `synthesized=${String(loaded.synthesized)} sourceEventSeq=${String(loaded.sourceEventSeq ?? 0)} ` +
        `durationMs=${String(Math.max(0, Math.round(performance.now() - hydrationStartedAt)))}`,
    );

    if (this.disposed || buffer.cancelled) {
      throw new Error(`v4 hydration cancelled for session ${sessionId}`);
    }

// Assistant conservation: Publishers that have rejected text streams are not trusted - they are built on
// After TurnStarted, the missing segment cannot be filled in with append-only replay, and can only be reconstructed as a whole.
    const latestPublisher = this.publishers.get(sessionId);
    const existingDroppedContent =
      latestPublisher !== undefined && latestPublisher.getDroppedContentStreamEventCount() > 0;
// The event log is complete (synthesized=false) and has a healthy live publisher (streaming) → retained, not replayed.
    if (
      existingAtStart &&
      latestPublisher === existingAtStart &&
      !loaded.synthesized &&
      !forceRebuild &&
      !existingDroppedContent
    ) {
// The seed may fail when created (the record has not yet been registered), and the first subscription will be replenished (idempotent, event priority).
      this.hydrationBuffers.delete(sessionId);
      this.seedPublisherConfig(sessionId, latestPublisher);
      if (loaded.sharedContextImport) {
        latestPublisher.seedSharedContextImport(loaded.sharedContextImport);
      }
      await this.seedPublisherUsage(
        sessionId,
        latestPublisher,
        persistedMessages,
        loaded.usageSeed,
      );
      this.hydratedSessions.add(sessionId);
      this.publishCurrentSummaryToIndex(sessionId);
      return latestPublisher;
    }

// Just remembering the existing reference before await is not enough: load waits for raw event during
// Will continue to advance this publisher, but delete it as a whole after synthesized returns, and queue/stream will follow
// disappear. Rebuild using sourceEventSeq as the raw snapshot boundary, and fill in the events within the waiting window.
    const publisher = latestPublisher ?? existingAtStart ?? this.ensurePublisher(sessionId);
    this.rejectProjectionEventWaiters(
      sessionId,
      new ProjectionEventCommitWaitError(
        "fault.projectionEventCommit.rehydrated",
        `conversation projection rehydrated while waiting for event commit: ${sessionId}`,
      ),
    );
    publisher.rehydrate(loaded.events, {
// Upon recovery, the transcript/event store may still contain oversized text that was rejected at runtime. cannot let the same fact
// After the CLI restarts, subscribe will be stuck again; skip the non-transmissible projection event and continue the reduction
// Subsequent persistent TurnError/TurnComplete stops the cold snapshot at the last recoverable boundary.
      onPayloadTooLarge: (error) =>
        this.host.onError?.("v4.hydrate.payloadTooLarge", error, {
          phase: "publisher.rehydrate",
          sessionId,
        }),
    });
    if (loaded.sharedContextImport) {
      publisher.seedSharedContextImport(loaded.sharedContextImport);
    }
    if (loaded.subagentsSeed) publisher.seedSubagents(loaded.subagentsSeed);
    // 同次恢复的种子先应用，再补 live buffer；较新的使用量和选模事件始终获胜。
    if (loaded.usageSeed) publisher.seedUsage(loaded.usageSeed);
    const sourceEventSeq = Math.max(
      0,
      loaded.sourceEventSeq ??
        (loaded.synthesized
          ? 0
          : loaded.events.reduce((maximum, event) => Math.max(maximum, event.sequenceNumber), 0)),
    );
    const previousSequenceState = this.rawSequenceStates.get(sessionId);
    const sequenceState: RawSequenceState = {
      sourceEventSeq,
      offset: publisher.getSnapshot().seq - sourceEventSeq,
      lastTransportSeq: publisher.getSnapshot().seq,
      seenEventIds: new Set(loaded.events.map((event) => String(event.id))),
      appliedEventIds: new Set(loaded.events.map((event) => String(event.id))),
      failedEventById: new Map(previousSequenceState?.failedEventById),
      pendingByRawSeq: new Map(),
      recentRawEventsById: new Map(),
    };
    this.rawSequenceStates.set(sessionId, sequenceState);
    // 持久读取的 sourceEventSeq 是 load 开始时的水位；hydration buffer
    // 只能记录 load 开始后的事件。若 seq=N 已在 buffer 创建前进入 live publisher，而
    // load 只读到 N-1 时，rehydrate 后不能仅重放 N+1：raw reorder 会永久等待已经被
    // 丢掉的 N，连带让 running Agent 控制行消失。保留与 publisher 相同大小的 raw tail，
    // 与 await 窗口 buffer 合并后从持久边界连续重放。
    const replayByEventId = new Map<string, SessionEvent>();
    for (const rawEvent of previousSequenceState?.recentRawEventsById.values() ?? []) {
      if (rawEvent.sequenceNumber <= 0 || rawEvent.sequenceNumber > sourceEventSeq) {
        replayByEventId.set(String(rawEvent.id), rawEvent);
      }
    }
    for (const rawEvent of buffer.rawEvents) {
      if (rawEvent.sequenceNumber <= 0 || rawEvent.sequenceNumber > sourceEventSeq) {
        replayByEventId.set(String(rawEvent.id), rawEvent);
      }
    }
    const replayEvents = [...replayByEventId.values()].sort((left, right) => {
      if (left.sequenceNumber > 0 && right.sequenceNumber > 0) {
        return left.sequenceNumber - right.sequenceNumber;
      }
      if (left.sequenceNumber > 0) return -1;
      if (right.sequenceNumber > 0) return 1;
      return 0;
    });
    for (const rawEvent of replayEvents) {
      for (const normalized of this.normalizeRuntimeEventSequence(sessionId, rawEvent)) {
        try {
          publisher.ingest(normalized);
          this.resolveProjectionEventCommit(sessionId, String(normalized.id));
        } catch (error) {
          this.rejectProjectionEventCommit(
            sessionId,
            String(normalized.id),
            new ProjectionEventCommitWaitError(
              "fault.projectionEventCommit.applyFailed",
              `projection failed to apply hydrated event ${String(normalized.id)}`,
              { cause: error },
            ),
          );
          if (!(error instanceof ProjectionPayloadTooLargeError)) throw error;
          this.host.onError?.("v4.hydrate.payloadTooLarge", error, {
            phase: "replayBufferedEvents",
            sessionId,
          });
        }
      }
    }
    // publisher 已替换且 buffer 已同步补齐；在 usage seed 的异步等待窗口内，新 raw
    // event 直接走上面的 per-session sequence state 进入新 publisher，不再需要二次 replay。
    if (this.hydrationBuffers.get(sessionId) === buffer) {
      this.hydrationBuffers.delete(sessionId);
    }
    // 冷恢复种子（重放之后）：resume 已把历史会话的上次选型写回 runtime
    // （reconcileResumedRuntimeSettings），而合成/持久化事件里可能没有 ModelSelected——
    // 种子只填事件未触碰的字段，日志有值时以日志为准（冷恢复口径）。
    this.seedPublisherConfig(sessionId, publisher);
    if (loaded.usageSeed === undefined) {
      await this.seedPublisherUsage(sessionId, publisher, persistedMessages);
    }
    for (const [routeKey, state] of this.flushStates) {
      if (state.sessionId !== sessionId) continue;
      if (!publisher.hasSubscription(state.subscriptionId, state.connectionId)) continue;
      if (state.timer) {
        clearTimeout(state.timer);
        state.timer = null;
      }
      if (this.pausedConnections.has(state.connectionId)) continue;
      const reservation = publisher.reserveFlush(state.subscriptionId);
      if (!reservation) continue;
      try {
        this.emitReservation(reservation);
      } catch (error) {
        this.host.onError?.("v4.hydrate.subscriptionResync", error, {
          phase: "subscriptionResync",
          sessionId,
        });
        this.scheduleFlush(routeKey, state, publisher);
      }
    }
    this.hydratedSessions.add(sessionId);
    this.publishCurrentSummaryToIndex(sessionId);
    return publisher;
  }

  /**
   * cold 合成会把事件重新编号为 1..N，而 runtime 仍沿用 eventStore raw seq。这里用
   * source cursor 建立 N-C 偏移；raw seq=0（live-only child）则顺延，并同步校正后续偏移。
   * eventId 兜住 snapshot/buffer 同时看见同一事件的竞态，cursor 兜住已入 snapshot 的事件。
   */
  private normalizeRuntimeEventSequence(sessionId: string, event: SessionEvent): SessionEvent[] {
    const state = this.getOrCreateRawSequenceState(sessionId);
    const eventId = String(event.id);
    if (state.seenEventIds.has(eventId)) return [];

    const rawSeq = event.sequenceNumber;
    state.recentRawEventsById.set(eventId, event);
    while (state.recentRawEventsById.size > PROTOCOL_V4_LIMITS.eventRetentionPerSession) {
      const oldestEventId = state.recentRawEventsById.keys().next().value;
      if (oldestEventId === undefined) break;
      state.recentRawEventsById.delete(oldestEventId);
    }
    if (rawSeq <= 0) {
      state.seenEventIds.add(eventId);
      state.lastTransportSeq += 1;
      return [{ ...event, sequenceNumber: state.lastTransportSeq }];
    }
    if (event.type === SessionEventType.SessionResumed) {
      // 旧 runtime 在 unsubscribe/重建窗口时可能遗漏尾部 raw event。新 runtime
      // 延续持久 eventStore 高水位时，SessionResumed 的 raw seq 会大于旧 cursor；
      // 若只处理 seq 回退，resume 和后续 TurnStarted 就会永久等待无法补齐的旧 gap。
      // SessionResumed 是明确 epoch 边界：丢弃边界前的旧 pending，同时保留可能乱序先到的
      // 新 epoch 后续事件，再从 resume 自身连续 drain。
      for (const pendingSeq of state.pendingByRawSeq.keys()) {
        if (pendingSeq <= rawSeq) state.pendingByRawSeq.delete(pendingSeq);
      }
      state.sourceEventSeq = rawSeq - 1;
      state.offset = state.lastTransportSeq - state.sourceEventSeq;
    }
    if (rawSeq <= state.sourceEventSeq) {
      state.seenEventIds.add(eventId);
      this.resolveProjectionEventCommit(sessionId, eventId);
      return [];
    }

    state.seenEventIds.add(eventId);
    if (!state.pendingByRawSeq.has(rawSeq)) state.pendingByRawSeq.set(rawSeq, event);
    const ready: SessionEvent[] = [];
    // eventStore 先编号，各事件各自 await 持久化后再 notify，
    // 因此 N+1 可以先于 N 到达。高水位过滤会把迟到 N 错判成 duplicate；
    // 必须按 raw seq 暂存，只连续 drain，才能保住 queue/stream 总序。
    for (;;) {
      const nextRawSeq = state.sourceEventSeq + 1;
      const next = state.pendingByRawSeq.get(nextRawSeq);
      if (!next) break;
      state.pendingByRawSeq.delete(nextRawSeq);
      let transportSeq = nextRawSeq + state.offset;
      if (transportSeq <= state.lastTransportSeq) {
        transportSeq = state.lastTransportSeq + 1;
        state.offset = transportSeq - nextRawSeq;
      }
      state.sourceEventSeq = nextRawSeq;
      state.lastTransportSeq = transportSeq;
      // waiter timeout/abort 只清 listener 是不够的，还要终止已在 raw gap 中的
      // event。command 返回 failed 后，缺失 seq 一到仍会把同一 TurnStarted 投影出来。
      // 失败事件仍消费 raw 序号以解除后续事件阻塞，但绝不能再成为 canonical fact。
      if (state.failedEventById.has(String(next.id))) {
        continue;
      }
      ready.push(
        transportSeq === next.sequenceNumber ? next : { ...next, sequenceNumber: transportSeq },
      );
    }
    return ready;
  }

  private getOrCreateRawSequenceState(sessionId: string): RawSequenceState {
    const existing = this.rawSequenceStates.get(sessionId);
    if (existing) return existing;
    const created: RawSequenceState = {
      sourceEventSeq: 0,
      offset: 0,
      lastTransportSeq: 0,
      seenEventIds: new Set(),
      appliedEventIds: new Set(),
      failedEventById: new Map(),
      pendingByRawSeq: new Map(),
      recentRawEventsById: new Map(),
    };
    this.rawSequenceStates.set(sessionId, created);
    return created;
  }

  private resolveProjectionEventCommit(sessionId: string, eventId: string): void {
    const state = this.getOrCreateRawSequenceState(sessionId);
    state.failedEventById.delete(eventId);
    state.appliedEventIds.add(eventId);
    const waiters = this.projectionEventCommitWaiters.get(sessionId)?.get(eventId);
    if (!waiters) return;
    for (const waiter of [...waiters]) waiter.resolve();
  }

  private rejectProjectionEventCommit(sessionId: string, eventId: string, error: Error): void {
    const state = this.getOrCreateRawSequenceState(sessionId);
    state.failedEventById.set(eventId, error);
    const waiters = this.projectionEventCommitWaiters.get(sessionId)?.get(eventId);
    if (!waiters) return;
    for (const waiter of [...waiters]) waiter.reject(error);
  }

  private rejectProjectionEventWaiters(sessionId: string, error: Error): void {
    const byEvent = this.projectionEventCommitWaiters.get(sessionId);
    if (!byEvent) return;
    for (const waiters of byEvent.values()) {
      for (const waiter of [...waiters]) waiter.reject(error);
    }
    this.projectionEventCommitWaiters.delete(sessionId);
  }

  /**
   * 运行中 subagent 没有独立 bootstrap record，但 raw child events 会先建立 publisher。
   * publisher 已存在就代表 conversation live 可订阅，不能再把同一 child cold resume 成
   * 第二个 runtime；真正的历史 session 仍由 host record / persisted resume 负责。
   */
  private hasLiveConversation(sessionId: string): boolean {
    return this.host.sessionExists(sessionId) || this.detachedLiveSessions.has(sessionId);
  }

  private async seedPublisherUsage(
    sessionId: string,
    publisher: ConversationTopicPublisher,
    persistedMessages?: MessageWithParts[],
    loadedSeed?: SessionUsageSeed | null,
  ): Promise<void> {
    if (loadedSeed !== undefined) {
      if (loadedSeed) publisher.seedUsage(loadedSeed);
      return;
    }
    const getSeed = this.host.getSessionUsageSeed;
    if (!getSeed) return;
    try {
      const seed = await getSeed.call(this.host, sessionId, persistedMessages);
      if (seed) publisher.seedUsage(seed);
    } catch (error) {
      this.host.onError?.("v4.usageSeed", error);
    }
  }

  private scheduleFlush(
    routeKey: string,
    state: FlushState,
    publisher: ConversationTopicPublisher,
  ): void {
    if (this.pausedConnections.has(state.connectionId)) return;
    if (state.timer !== null) return;
    const timer = setTimeout(() => {
      state.timer = null;
      // timer 排队后可能收到 SAT；reserve 前必须二次检查，不能产生竞态帧。
      if (this.pausedConnections.has(state.connectionId)) return;
      // 惰性清理：订阅已被替换/退订→ 删调度状态，不产帧。
      if (!publisher.hasSubscription(state.subscriptionId, state.connectionId)) {
        this.flushStates.delete(routeKey);
        return;
      }
      const reservation = publisher.reserveFlush(state.subscriptionId);
      if (!reservation) return;
      try {
        this.emitReservation(reservation);
      } catch (error) {
        this.host.onError?.("v4.frame.emit", error);
      }
    }, state.flushWindowMs);
    // CLI 进程退出不被 flush 定时器挂住。
    timer.unref?.();
    state.timer = timer;
  }

  private emitReservation<F extends RoutedTopicFrame>(
    reservation: TopicFrameReservation<F>,
  ): boolean {
    // resync/subscribe recovery 已进入 request-scoped outbox 时，online
    // flush 若复用同一 inFlight 会让 physical wire 抢在 ACK response 前出站。
    if (this.controlReservations.has(reservation)) return false;
    const sessionId = parseConversationTopic(reservation.frame.topic);
    const route = this.flushStates.get(
      subscriptionRouteKey(
        reservation.frame.topic,
        reservation.frame.subscriptionId,
        sessionId
          ? (this.publishers
              .get(sessionId)
              ?.connectionIdForSubscription(reservation.frame.subscriptionId) ?? "")
          : "",
      ),
    );
    if (
      sessionId &&
      route?.deliveryProfile === "continuous" &&
      reservation.deliveryKind === "online" &&
      reservation.frame.payload.kind === "deltas" &&
      this.localTtft.forSession(sessionId)
    ) {
      const rows = this.publishers.get(sessionId)?.getSnapshot().rows.window ?? [];
      const turns = new Set<string>();
      for (const delta of reservation.frame.payload.deltas) {
        if (delta.op === "row.appended" || delta.op === "row.upserted") turns.add(delta.row.turnId);
        else if (delta.op === "row.delta") {
          const row = rows.find((item) => item.rowId === delta.rowId);
          if (row) turns.add(row.turnId);
        }
      }
      const related = rows
        .filter((row) => row.kind === "turnHeader" && turns.has(row.turnId))
        .flatMap((header) =>
          header.kind === "turnHeader" && header.sourceCommandId
            ? [this.localTtft.forSession(sessionId, header.sourceCommandId)]
            : [],
        )
        .filter((facts) => facts !== undefined);
      const candidates = related.length ? related : [this.localTtft.forSession(sessionId)];
      const observations: import("@zcode/shared").LocalTtftFacts[] = [];
      for (const facts of candidates) {
        if (!facts || observations.some((item) => item.observationId === facts.observationId))
          continue;
        const header = rows.find(
          (row) => row.kind === "turnHeader" && row.sourceCommandId === facts.commandId,
        );
        const observation = localTtftFactsSchema.safeParse({
          ...facts,
          ...(this.host.cliVersion ? { cliVersion: this.host.cliVersion } : {}),
          ...(header ? { productTurnId: header.turnId } : {}),
        });
        // 转正前后的内容可能被同批发送；按实际 row 所属原输入携带事实，不能取最新队列项。
        if (observation.success) observations.push(observation.data);
      }
      if (observations.length) {
        (reservation.frame as ConversationTopicFrame).ttft = observations[0];
        if (observations.length > 1)
          (reservation.frame as ConversationTopicFrame).ttftRelated = observations.slice(1, 17);
      }
    }
    const wires = encodeReservedTopicFrame(reservation as TopicFrameReservation<RoutedTopicFrame>);
    for (const wire of wires) this.host.emitWireFrame(wire);
    return reservation.commit();
  }

  private subscribeDispatch<F extends RoutedTopicFrame>(
    ack: SubscribeAck,
    reservation: TopicFrameReservation<F> | null,
    afterCommit?: () => void,
  ): V4SubscribeDispatchResult<F> {
    const initialWires = reservation
      ? encodeReservedTopicFrame(reservation as TopicFrameReservation<RoutedTopicFrame>)
      : [];
    if (reservation) this.controlReservations.add(reservation);
    let afterCommitRan = false;
    return {
      ack,
      initialFrame: reservation?.frame ?? null,
      initialWires,
      commit: () => {
        if (!reservation) return true;
        this.controlReservations.delete(reservation);
        const committed = reservation.commit();
        if (committed && !afterCommitRan) {
          afterCommitRan = true;
          // control reservation 等 ACK/outbox admission 时，既有 flush timer
// Might have been triggered and suppressed by the same inFlight. After committing, you must actively re-drive the publisher.
// Otherwise, the delta accumulated during the period will not be visible until the next ingest/publish.
          afterCommit?.();
        }
        return committed;
      },
    };
  }
}

/** The host executor throws this error for unwired commands → ACK failed fault.notImplemented. */
export class V4CommandNotImplementedError extends Error {
  constructor(type: string) {
    super(`v4 command not implemented in M3: ${type}`);
    this.name = "V4CommandNotImplementedError";
  }
}

/**
* Command handler's noop closing channel ("Same value switching ACK must be distinguishable"):
* The handler determines that the command has nothing to do (such as switchModelConfig/switchCollaborationMode hits
*Thrown when runtime current value), gateway is mapped to ACK status="noop" + reasonCode——
* It must not be swallowed silently with accepted (no result), so that the client can distinguish between "effective" and "originally this value".
 */
export class V4CommandNoopError extends Error {
  constructor(
    readonly reasonCode: string,
    message?: string,
  ) {
    super(message ?? `v4 command is a no-op (${reasonCode})`);
    this.name = "V4CommandNoopError";
  }
}
