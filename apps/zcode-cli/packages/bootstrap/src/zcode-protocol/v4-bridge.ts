/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { readBackgroundBashOutputFromOwner } from "./background-work-owner.js";
// v4 gateway binder.
// Positioning: ConversationV4Gateway is a domain-independent channel runtime. This file binds it to the protocol server context:
// - frame egress = context.notify(stdio NDJSON notification, coexisting with the same pipe as the old session/event);
// - Command execution = V4CommandExecutor (zcode-protocol-v4/commands/, native direct drive core);
// 20 commands are all native, supports() misses (unknown command) → notImplemented.
// - Transition hooks (ensureModelReady/afterLegacyStateMutation/closeSession/
// createSessionRecord / child record registration / resumePersistedSession) inject the old protocol implementation here and delete it along with the old protocol.
//
// No bridging: The dependency direction only allows old directory → v4 directory.
// This file is in the old directory, and it is legal to import v4 executor; reverse import of any module in this directory is prohibited in the v4 directory.
import {
  isConversationRealUserTurnStarter,
  parseRemoteWorkspaceIdentity,
  type ZCodeSessionContextUsage,
  type ZCodeWorkspaceRef,
} from "@zcode/shared";
import { createExternalTurnFaultError } from "@zcode/core";
import {
  V4_NOTIFICATIONS,
  conversationInputIntentSchema,
  type AttachmentRef,
  type CommandEnvelope,
  type ConversationInputIntent,
  type V4ConversationFileChangesResult,
  type V4ConversationFileRewindPreviewResult,
  type SessionSummary,
} from "@zcode/shared/zcode-protocol-v4";
import { V4CommandExecutor } from "../zcode-protocol-v4/commands/executor.js";
import { V4QueuePromotionLeaseUnavailableError } from "../zcode-protocol-v4/commands/handlers/queue.js";
import { V4CapabilityUnsupportedError } from "../zcode-protocol-v4/commands/handlers/interaction-background.js";
import {
  buildColdFileChangeSummaries,
  readConversationFileChangesFromEvents,
} from "../zcode-protocol-v4/cold-file-change-summaries.js";
import {
  loadPersistedConversationMaterialization,
  mergeColdConversationEvents,
} from "../zcode-protocol-v4/cold-event-merge.js";
import { lookupGlobalCreateSessionCommand } from "../zcode-protocol-v4/create-session-command-fact.js";
import type { V4CommandCoreHost } from "../zcode-protocol-v4/commands/types.js";
import type {
  ConversationRowTargetResolution,
  SessionUsageSeed,
} from "../zcode-protocol-v4/product-projection.js";
import { PersistentCommandIndex } from "../zcode-protocol-v4/persistent-command-index.js";
import { queueItemIdForCommand } from "../zcode-protocol-v4/command-inbox.js";
import { resolveStableForkTargetFromTranscript } from "../zcode-protocol-v4/stable-fork-target.js";
import { shouldAutoDrainV4QueueHead } from "../zcode-protocol-v4/queue-auto-drain.js";
import { persistAssistantFeedback } from "../zcode-protocol-v4/assistant-feedback-persistence.js";
import {
  TASK_LIST_SESSION_TYPES,
  isTaskListSessionType,
} from "../zcode-protocol-v4/task-list-session-membership.js";
import {
  loadPersistentCommandFacts,
  savePersistentCommandFact,
} from "../zcode-protocol-v4/persistent-command-facts.js";
import {
  ConversationV4Gateway,
  V4CommandNotImplementedError,
} from "../zcode-protocol-v4/v4-gateway.js";
import {
  SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
  SessionEventType,
  createEventId,
  createSessionId,
} from "@zcode/contracts";
import type {
  CollaborationMode,
  DynamicWorkflowRunProgressPayload,
  EventId,
  ForkCommitBundle,
  GoalStatus,
  MessageId,
  ModelSelection,
  SessionEvent,
  SessionId,
  StableForkGoalBoundaryMetadata,
  TraceId,
  TurnId,
  WorkspaceId,
} from "@zcode/contracts";
import { HYDRATION_TRACE_ID } from "../zcode-protocol-v4/projection-state.js";
import { resolveWorkspaceRefFromId } from "./mapper.js";
import { buildLiveWorkspaceConfigStateV4 } from "./v4-workspace-config.js";
import {
  hasSessionModelProvider,
  resolveSessionModelContextWindow,
} from "./workspace-model-runtime.js";
import {
  afterStateMutation,
  activateSessionForResume,
  createSessionRecordForV4,
  ensureSessionModelAvailableForNextTurn,
  listSessionSubagents,
  registerForkedSession,
  readSessionContextUsage,
} from "./server-operations.js";
import type {
  ZCodeProtocolAgentServerContext,
  ZCodeProtocolSessionRecord,
} from "./server-types.js";
import { createProtocolLogger } from "./server-types.js";

function normalizeStoredTitleSource(
  source: string | undefined,
): NonNullable<SessionSummary["titleSource"]> {
  if (source === "custom") return "custom";
  if (source === "default") return "default";
  return "generated";
}

function sessionUsageSeedFromRuntimeContextUsage(
  contextUsage: ZCodeSessionContextUsage | undefined,
  contextWindowOverride?: number,
): SessionUsageSeed | null {
  if (!contextUsage || contextUsage.used <= 0) {
    return null;
  }
  return {
    contextWindow: {
      usedTokens: contextUsage.used,
      maxTokens: contextWindowOverride ?? null,
      autoCompactThresholdTokens: null,
      ...(contextUsage.cache ? { cache: contextUsage.cache } : {}),
      ...(contextUsage.breakdown ? { breakdown: contextUsage.breakdown } : {}),
    },
  };
}

const STABLE_FORK_MODES = new Set<CollaborationMode>(["plan", "build", "edit", "yolo", "auto"]);

function stableForkMode(value: string, fallback: CollaborationMode): CollaborationMode {
  return STABLE_FORK_MODES.has(value as CollaborationMode)
    ? (value as CollaborationMode)
    : fallback;
}

function modelSelectionWithOptionFallback(
  selection: ModelSelection | undefined,
  fallback: ModelSelection | undefined,
): ModelSelection | undefined {
  if (!selection) return fallback && cloneModelSelection(fallback);
// Compatibility with old fork messages may lack reasoning; the output budget belongs to a single request, not to Selection.
  const reasoningLevel = selection.options?.reasoningLevel ?? fallback?.options?.reasoningLevel;
  return {
    providerId: selection.providerId,
    modelId: selection.modelId,
    ...(reasoningLevel !== undefined
      ? {
          options: {
            ...(reasoningLevel !== undefined ? { reasoningLevel } : {}),
          },
        }
      : {}),
  };
}

function cloneModelSelection(
  selection: ReturnType<ZCodeProtocolSessionRecord["app"]["runtime"]["getSessionModelSelection"]>,
): ModelSelection | undefined {
  if (!selection) return undefined;
  return {
    providerId: selection.providerId,
    modelId: selection.modelId,
    ...(selection.options ? { options: { ...selection.options } } : {}),
  };
}

async function readConversationFileChanges(
  record: ZCodeProtocolSessionRecord,
  sessionId: string,
  messageIds: readonly string[],
  targetTurnId?: TurnId | null,
): Promise<V4ConversationFileChangesResult> {
  const events = await record.eventStore.getEvents(sessionId as SessionId);
  return readConversationFileChangesFromEvents({
    events,
    messageIds,
    readArtifact: async (snapshotRef) =>
      (await record.app.readToolResultArtifact(snapshotRef)).content,
    ...(targetTurnId ? { targetTurnId } : {}),
  });
}

/**
* When cold materializing, play back the workflow run of this session from the journal into the `DynamicWorkflowRunProgress` session event.
 *
* - Only reseed the parent session that directly hits the record: the child session (actor) that falls back to the parent record via parentID
* transcript) is not added - journal is created according to the parent session, and the projection of the child session should not grow beyond the run of the parent session;
* - The runId that has appeared in the memory event is handed over to the CLI for elimination (the run events run by this process are all in the memory store,
* Progress events do not have turnId and are not eliminated by turn-window), so they are warm materialized and have zero duplication;
* - If the replay fails, only the log will be recorded, and the return will be empty: the observation surface will never allow the cold open to fail.
 *
* Event id / traceId is a synthetic event according to transcript hydration; sequenceNumber is uniformly rearranged by cold merge.
 */
async function replayDynamicWorkflowRunEvents(
  context: ZCodeProtocolAgentServerContext,
  sessionId: string,
  record: ZCodeProtocolSessionRecord,
  memoryEvents: readonly SessionEvent[],
): Promise<SessionEvent[]> {
  if (context.sessions.get(sessionId) !== record) return [];
  const replay = record.app.replayDynamicWorkflowRuns;
  if (!replay) return [];
  const excludeRunIds = new Set<string>();
  for (const event of memoryEvents) {
    if (event.type !== SessionEventType.DynamicWorkflowRunProgress) continue;
    const runId = (event.payload as { runId?: unknown } | undefined)?.runId;
    if (typeof runId === "string") excludeRunIds.add(runId);
  }
  let payloads: DynamicWorkflowRunProgressPayload[];
  try {
    payloads = await replay({ excludeRunIds });
  } catch (error) {
    context.logger?.warn("v4 hydrate dynamic workflow replay failed", {
      error: error instanceof Error ? error.message : String(error),
      event: "zcode_protocol.v4.hydrate_workflow_replay_failed",
      module: "bootstrap.zcode_protocol",
      sessionId,
    });
    return [];
  }
  return payloads.map((payload, index) => ({
    id: `dwf-replay-${index + 1}` as EventId,
    sessionId: sessionId as SessionId,
    type: SessionEventType.DynamicWorkflowRunProgress,
    timestamp: new Date(0),
    traceId: HYDRATION_TRACE_ID as TraceId,
    sequenceNumber: 0,
    payload,
  }));
}

async function resolveConversationBackingRecord(
  context: ZCodeProtocolAgentServerContext,
  sessionId: string,
): Promise<ZCodeProtocolSessionRecord | undefined> {
  const direct = context.sessions.get(sessionId);
  if (direct) return direct;

// The running subagent has an independent child event log, but no independent bootstrap record.
// The file summary only needs to share the event/artifact store, so find the parent record by persisting parentID as
// artifact reader, still explicitly uses childSessionId when reading events; cannot query for read-only cold resume
// Second child runtime.
  const stored = await context.deps.sessionStore?.getSession(sessionId as SessionId);
  const parentSessionId = stored?.parentID ? String(stored.parentID) : null;
  return parentSessionId ? context.sessions.get(parentSessionId) : undefined;
}

async function previewConversationFileRewind(
  record: ZCodeProtocolSessionRecord,
  messageIds: readonly string[],
  targetTurnId?: TurnId | null,
): Promise<V4ConversationFileRewindPreviewResult> {
  return record.app.runtime.previewWorkspaceFileRewind({
    targetMessageIds: messageIds as MessageId[],
    ...(targetTurnId ? { targetTurnId } : {}),
  });
}

interface InputCommandForAdmission {
  kind: ConversationInputIntent["kind"];
  text: string;
  attachments: readonly AttachmentRef[];
  sharedContextRefs?: ConversationInputIntent["sharedContextRefs"];
  requestedDelivery?: ConversationInputIntent["delivery"]["requested"];
  admittedDelivery?: ConversationInputIntent["delivery"]["admitted"];
  fallbackReasonCode?: string;
  provenance?: ConversationInputIntent["provenance"];
}

type ResolveAdmissionRowTarget = (
  sessionId: string,
  target: { rowId: number; entityId: string },
  action: "editUserQuery" | "retryTurn",
) => ConversationRowTargetResolution | null;

function admissionAttachmentRefs(
  attachments: NonNullable<
    Extract<ConversationRowTargetResolution, { ok: true }>["editTarget"]
  >["intent"]["attachments"],
): AttachmentRef[] {
  return (
    attachments?.flatMap((attachment) =>
      attachment.ref
        ? [
            {
              ref: attachment.ref,
              fileName: attachment.fileName,
              mime: attachment.mime,
              bytes: attachment.bytes,
              ...(attachment.previewRef ? { previewRef: attachment.previewRef } : {}),
            },
          ]
        : [],
    ) ?? []
  );
}

/**
* admission only persists commands that actually produce input. edit/retry cannot guess the intent from the payload;
* The canonical target of the projection must be reused and the old source folded into provenance.
 */
function resolveInputCommandForAdmission(
  envelope: CommandEnvelope,
  admissionSessionId: string,
  resolveRowTarget: ResolveAdmissionRowTarget,
): InputCommandForAdmission | null {
  if (envelope.type === "createSession") {
    const firstInput = (
      envelope.payload as {
        firstInput?: { text: string; attachments?: AttachmentRef[] };
      }
    ).firstInput;
    return firstInput
      ? {
          kind: "sendText",
          text: firstInput.text,
          attachments: firstInput.attachments ?? [],
        }
      : null;
  }
  if (envelope.type === "createSelectionSideSession") {
    const firstInput = (
      envelope.payload as {
        firstInput?: { text: string };
      }
    ).firstInput;
    return firstInput
      ? {
          kind: "sendText",
          text: firstInput.text,
          attachments: [],
        }
      : null;
  }
  if (envelope.type === "sendText" || envelope.type === "sendGoalCommand") {
    const payload = envelope.payload as {
      text: string;
      attachments?: AttachmentRef[];
      context_refs?: ConversationInputIntent["sharedContextRefs"];
    };
    return {
      kind: envelope.type,
      text: payload.text,
      attachments: payload.attachments ?? [],
      ...(payload.context_refs ? { sharedContextRefs: payload.context_refs } : {}),
    };
  }
  if (envelope.type === "compact") {
    return { kind: "compact", text: "/compact", attachments: [] };
  }
  if (envelope.type !== "editUserQuery" && envelope.type !== "retryTurn") return null;
  if (!envelope.sessionId) return null;
  const payload = envelope.payload as {
    target: { rowId: number; entityId: string };
    newText?: string;
    attachments?: AttachmentRef[];
  };
  const resolution = resolveRowTarget(envelope.sessionId, payload.target, envelope.type);
  if (!resolution?.ok || !resolution.editTarget) return null;
  const canonical = resolution.editTarget;

// The append-only branch cut will be submitted first, and no hidden child will be created for edit.
  const originalSourceCommandId =
    canonical.intent.provenance?.sourceCommandId ?? canonical.intent.sourceCommandId;
  return {
    kind: canonical.intent.kind,
    text:
      envelope.type === "editUserQuery"
        ? (payload.newText ?? canonical.intent.text)
        : canonical.intent.text,
    attachments:
      envelope.type === "editUserQuery" && payload.attachments
        ? payload.attachments
        : admissionAttachmentRefs(canonical.intent.attachments),
    ...(canonical.intent.requestedDelivery
      ? { requestedDelivery: canonical.intent.requestedDelivery }
      : {}),
    ...(canonical.intent.admittedDelivery
      ? { admittedDelivery: canonical.intent.admittedDelivery }
      : {}),
    ...(canonical.intent.fallbackReasonCode
      ? { fallbackReasonCode: canonical.intent.fallbackReasonCode }
      : {}),
    ...(originalSourceCommandId
      ? {
          provenance: canonical.intent.provenance ?? {
            sourceCommandId: originalSourceCommandId,
            ...(canonical.intent.queueItemId ? { queueItemId: canonical.intent.queueItemId } : {}),
            ...(canonical.intent.clientId ? { clientId: canonical.intent.clientId } : {}),
          },
        }
      : {}),
  };
}

function isConversationInputAdmissionCommand(type: CommandEnvelope["type"]): boolean {
  return (
    type === "sendText" ||
    type === "sendGoalCommand" ||
    type === "compact" ||
    type === "editUserQuery" ||
    type === "retryTurn"
  );
}

function buildForkInitialInput(
  envelope: CommandEnvelope,
  childSessionId: string,
  admission: { admissionSeq: number; admittedAt: number; queueItemId: string },
  input: InputCommandForAdmission,
): ForkCommitBundle["initialInput"] {
  const requested = input.requestedDelivery ?? "startNow";
  const fallbackReasonCode = input.fallbackReasonCode;
  const admitted =
    input.admittedDelivery ??
    (fallbackReasonCode ? "queue" : requested === "auto" ? "startNow" : requested);
  const intent = conversationInputIntentSchema.parse({
    sourceCommandId: envelope.commandId,
    queueItemId: admission.queueItemId,
    clientId: envelope.clientId || "cli",
    kind: input.kind,
    text: input.text,
    attachments: input.attachments,
    delivery: {
      requested,
      admitted,
      ...(fallbackReasonCode ? { fallbackReasonCode } : {}),
    },
    order: { admissionSeq: admission.admissionSeq },
    steer: fallbackReasonCode
      ? { state: "fellBack", reasonCode: fallbackReasonCode }
      : { state: "notRequested" },
    dispatch: { state: "admitted" },
    admittedAt: admission.admittedAt,
    ...(input.provenance ? { provenance: input.provenance } : {}),
  });
  return {
    id: admission.queueItemId,
    sessionID: childSessionId as SessionId,
    kind: intent.kind,
    delivery: intent.delivery.admitted,
    payload: {
      text: intent.text,
      conversationInputIntent: intent,
      attachments: intent.attachments,
      sourceCommandType: envelope.type,
    },
  };
}

async function recordForkStartFailureBestEffort(
  context: ZCodeProtocolAgentServerContext,
  sessionId: string,
  command: Pick<CommandEnvelope, "commandId">,
  error: unknown,
  details: { parentSessionId?: string; registrationRequired?: boolean } = {},
): Promise<void> {
  const store = context.deps.sessionStore;
  const record = context.sessions.get(sessionId);
  const now = Date.now();
  const message = error instanceof Error ? error.message : String(error);
  const warn = (stage: string, failure: unknown) => {
    try {
      context.logger?.warn("fork child post-commit failure recording degraded", {
        commandId: command.commandId,
        error: failure instanceof Error ? failure.message : String(failure),
        forkedSessionId: sessionId,
        parentSessionId: details.parentSessionId,
        stage,
      });
    } catch {
// Log sink failure also belongs to post-commit; durable child/fact must not be reversed accordingly.
    }
  };

  try {
    await store?.settleSessionInput?.({
      id: queueItemIdForCommand(command.commandId),
      sessionID: sessionId as SessionId,
      status: "failed",
      reason: "fault.command.childStartFailed",
    });
  } catch (failure) {
    warn("ledger", failure);
  }
  try {
    await store?.saveSessionEntry?.({
      id: `v4_fork_start_failure:${command.commandId}`,
      sessionID: sessionId as SessionId,
      type: "v4/fork_start_failure",
      time: { created: now, updated: now },
      data: {
        commandId: command.commandId,
        forkedSessionId: sessionId,
        parentSessionId: details.parentSessionId,
        ...(details.registrationRequired ? { registrationRequired: true } : {}),
        retryable: true,
        status: "failed",
        reasonCode: "fault.command.childStartFailed",
        message,
      },
    });
  } catch (failure) {
    warn("entry", failure);
  }
  if (!record) return;
  try {
    const event: SessionEvent = {
      id: createEventId(),
      sessionId: sessionId as SessionId,
      type: SessionEventType.TurnError,
      timestamp: new Date(now),
      traceId: record.traceContext.traceId,
      sequenceNumber: (await record.eventStore.getLatestSequenceNumber(sessionId as SessionId)) + 1,
      payload: {
        inputId: command.commandId,
        turnPhase: "fork_child_start",
        error: {
          type: "fault.command.childStartFailed",
          message,
          retryable: true,
        },
      },
    };
    const persisted = await record.eventStore.append(event);
    context.v4Gateway?.ingest(sessionId, persisted);
  } catch (failure) {
    warn("event", failure);
  }
}

/**
* Fork bundle commit is the command PONR; subsequent catalog/model/resume/snapshot only restores runtime reachability.
* Failure at this stage must leave a retryable fact and warning, but durable accepted child cannot be reversed into failed.
 */
async function registerCommittedForkBestEffort(
  context: ZCodeProtocolAgentServerContext,
  record: ZCodeProtocolSessionRecord,
  fork: Parameters<typeof registerForkedSession>[2],
  options: Parameters<typeof registerForkedSession>[3] & { commandId: string },
  register: typeof registerForkedSession = registerForkedSession,
): Promise<void> {
  const { commandId, ...registrationOptions } = options;
  try {
    await register(context, record, fork, registrationOptions);
  } catch (error) {
    const forkedSessionId = String(fork.forkedSessionId);
    const parentSessionId = String(fork.parentSessionId ?? record.app.sessionId);
    await recordForkStartFailureBestEffort(context, forkedSessionId, { commandId }, error, {
      parentSessionId,
      registrationRequired: true,
    });
    try {
      context.logger?.warn("fork child registration failed after durable commit", {
        commandId,
        error: error instanceof Error ? error.message : String(error),
        forkedSessionId,
        parentSessionId,
        retryable: true,
      });
    } catch {
// The logger's own exception used to bubble up across the PONR, causing the gateway error to settle as failed.
    }
  }
}

export function createConversationV4Gateway(
  context: ZCodeProtocolAgentServerContext,
): ConversationV4Gateway {
  const log = createProtocolLogger(context.deps)?.child({
    module: "bootstrap.zcode_protocol_v4_gateway",
  });
  const persistentCommands = new PersistentCommandIndex({
    loadSession: async (sessionId) => {
      const live = context.sessions.get(sessionId);
      const stored = await context.deps.sessionStore?.getSession(sessionId as SessionId);
      if (!live && !stored) return null;
      const workspacePath = live?.workspace.workspacePath ?? stored?.directory;
      if (!workspacePath) return null;
      const workspaceIdentity = live?.workspace.workspaceIdentity ?? stored?.workspaceID;
      const facts = context.deps.sessionStore
        ? await loadPersistentCommandFacts(context.deps.sessionStore, sessionId as SessionId, {
            discardAdmittedOnLoad: !live,
          })
        : undefined;
      return {
        workspacePath,
        ...(workspaceIdentity ? { workspaceIdentity: String(workspaceIdentity) } : {}),
        ...(facts ? { facts } : {}),
      };
    },
  });
  let nativeExecutor: V4CommandExecutor;
  const autoDrainRetryTimers = new Map<string, ReturnType<typeof setTimeout>>();
  const autoDrainV4QueueIfReady = async (record: ZCodeProtocolSessionRecord): Promise<void> => {
    const head = context.v4Gateway?.getQueueHead(record.app.sessionId);
    if (!head) {
// The outer recovery FIFO has been consumed to empty; the guide is still only consumed within the core tool-batch boundary.
      record.app.completeExternalQueueDrain();
      return;
    }
    const coreForegroundBusy = record.app.runtime.getActiveForegroundExecutionId() !== undefined;
    if (
      head.autoDrain &&
      head.dispatchState === "queued" &&
      (record.activeAbortController !== undefined || coreForegroundBusy)
    ) {
// Bootstrap controller does not override model-only notification; old auto-drain
// Only look at the outer lock, so the "consumption after idle" error is executed as preemption. Don't touch reservation when busy.
      scheduleAutoDrainRetry(record);
      return;
    }
    let targetStatus: GoalStatus | null = null;
    if (head.autoDrain && head.dispatchState === "queued" && !record.activeAbortController) {
      try {
        targetStatus = (await record.app.readTarget())?.status ?? null;
      } catch (error) {
// When target reading fails, it will be treated as "unknown and unfinished"; direct promotion will make
// Ordinary queue sneaks out when the persistent final state of goal verification has not yet been verified.
        context.logger?.warn("v4 auto-drain held because target state could not be read", {
          error: error instanceof Error ? error.message : String(error),
          queueItemId: head.queueItemId,
          sessionId: record.app.sessionId,
        });
        return;
      }
    }
    if (
      !shouldAutoDrainV4QueueHead({
        autoDrain: head.autoDrain,
        dispatchState: head.dispatchState,
        sessionBusy: Boolean(record.activeAbortController) || coreForegroundBusy,
        targetStatus,
      })
    ) {
      return;
    }
// After the pause queue is restored, the old item only exists in the projection and not in the new activeTurn memory; normal text must also
// Like typed /goal, /compact, go to the head of the fully projected queue to prevent new input from crossing the old pause item.
    try {
      await nativeExecutor.execute(
        {
          baseRevision: 0,
          clientId: "v4-auto-drain",
          commandId: `auto-${head.kind}-${Date.now()}-${head.queueItemId}`,
          issuedAt: Date.now(),
          payload: { queueItemId: head.queueItemId },
          sessionId: record.app.sessionId,
          type: "sendQueuedNow",
        },
        undefined,
        { autoDrainPromotion: true },
      );
    } catch (error) {
      if (error instanceof V4QueuePromotionLeaseUnavailableError) {
// New notifications may be added to the queue between precheck and handler; idle-only is the final atomic criterion.
        scheduleAutoDrainRetry(record);
        return;
      }
// If the automatic promotion fails, the FIFO barrier cannot be crossed; pause again and retain the original item, and let the user try again.
      await record.app.setQueueAutoDrain(false);
      context.logger?.warn("v4 auto-drain failed and queue was paused", {
        error: error instanceof Error ? error.message : String(error),
        queueItemId: head.queueItemId,
        sessionId: record.app.sessionId,
      });
    }
  };
  const scheduleAutoDrainRetry = (record: ZCodeProtocolSessionRecord): void => {
    const sessionId = record.app.sessionId;
    if (autoDrainRetryTimers.has(sessionId)) return;
    const timer = setTimeout(() => {
      autoDrainRetryTimers.delete(sessionId);
      if (context.sessions.get(sessionId) !== record) return;
      void autoDrainV4QueueIfReady(record).catch((error: unknown) => {
        context.logger?.warn("v4 auto-drain idle reevaluation failed", {
          error: error instanceof Error ? error.message : String(error),
          sessionId,
        });
      });
    }, 100);
    timer.unref?.();
    autoDrainRetryTimers.set(sessionId, timer);
  };
  const coreHost: V4CommandCoreHost = {
// Reference to the same registry object: view is a structured narrow view of the old record, and field changes are visible in both directions.
    getRecord: (sessionId) => context.sessions.get(sessionId),
//Same registration table instance: broker (old directory) registers reverse request deferred,
// v4 resolveInteraction handler delivers response via this (v4 native infrastructure, not transition hook).
    interactions: context.v4Interactions,
    logger: {
      info: (message, fields) => context.logger?.info(message, fields),
      warn: (message, fields) => context.logger?.warn(message, fields),
    },
// v4 native capability (non-transition hook): sendQueuedNow must read the complete intent in the v4 projection.
// When the command is executed, context.v4Gateway has been injected by the server (createConversationV4Gateway
//Return value backfill), lazy access here avoids self-reference during construction.
    getQueueItem: (sessionId, queueItemId) =>
      context.v4Gateway?.getQueueItem(sessionId, queueItemId) ?? null,
    hasQueueItemKind: (sessionId, kind) =>
      context.v4Gateway?.hasQueueItemKind(sessionId, kind) ?? false,
    hasQueuedDelivery: (sessionId, delivery) =>
      context.v4Gateway?.hasQueuedDelivery(sessionId, delivery) ?? false,
    getQueueLength: (sessionId) => context.v4Gateway?.getQueueLength(sessionId) ?? 0,
    waitForProjectionEventCommit: (sessionId, eventId, options) => {
      const gateway = context.v4Gateway;
      if (!gateway) {
        return Promise.reject(new Error("v4 gateway unavailable for projection commit wait"));
      }
      return gateway.waitForProjectionEventCommit(sessionId, eventId, options);
    },
    admitInputCommand: async (envelope, sessionId, admission) => {
      if (!context.deps.sessionStore?.saveSessionInput) return null;
      const input = resolveInputCommandForAdmission(
        envelope,
        sessionId,
        (sourceSessionId, target, action) =>
          context.v4Gateway?.resolveRowActionTarget(sourceSessionId, target, action) ?? null,
      );
      if (!input) return null;
      const kind = input.kind;
      const record = context.sessions.get(sessionId);
      if (record?.persistence === "deferred") {
// session_input has session foreign key; draft will not be persisted until startPromptTurn background stage.
// If you write the ledger first, FK will fail directly, and there will still be no authoritative record before accepted. Therefore admission
// First go to the unified initial persistence boundary of the runtime, and then drop the ledger, and then the handler is only responsible for execution.
        await record.app.runtime.ensureSessionPersistedForExternalActivity(input.text ?? "", {
          traceContext: record.traceContext,
        });
        record.persistence = "immediate";
      }
      const routingMode = context.v4Gateway?.getInputRoutingMode(sessionId) ?? null;
// This is the "estimated delivery boundary" of the ledger before execution; TurnSteerQueued will use the actual delivery/fallback reason
//Impotent update of the same record. startNow cannot be disguised as a queue, otherwise the diagnostic facts of restart discarded will be distorted.
      const requestedDelivery =
        input.requestedDelivery ??
        (kind === "compact" && routingMode !== null && routingMode !== "startNow"
          ? "queue"
          : routingMode === "enqueue"
            ? "queue"
            : routingMode === "guide" && kind === "sendText"
              ? "guide"
              : "startNow");
      const attachmentRefs = input.attachments;
      const fallbackReasonCode =
        input.fallbackReasonCode ??
        (requestedDelivery === "guide" && attachmentRefs.length > 0
          ? "guide.attachmentsUnsupported"
          : undefined);
      const admittedDelivery =
        input.admittedDelivery ??
        (fallbackReasonCode
          ? "queue"
          : requestedDelivery === "auto"
            ? "startNow"
            : requestedDelivery);
      const conversationInputIntent = conversationInputIntentSchema.parse({
        sourceCommandId: envelope.commandId,
        queueItemId: admission.queueItemId,
        clientId: envelope.clientId || "cli",
        kind,
        text: input.text ?? "",
        attachments: attachmentRefs,
        ...(input.sharedContextRefs ? { sharedContextRefs: input.sharedContextRefs } : {}),
        delivery: {
          requested: requestedDelivery,
          admitted: admittedDelivery,
          ...(fallbackReasonCode ? { fallbackReasonCode } : {}),
        },
        order: { admissionSeq: admission.admissionSeq },
        steer: fallbackReasonCode
          ? { state: "fellBack", reasonCode: fallbackReasonCode }
          : requestedDelivery === "guide"
            ? { state: "submitting" }
            : { state: "notRequested" },
        dispatch: { state: "admitted" },
        admittedAt: admission.admittedAt,
        ...(input.provenance ? { provenance: input.provenance } : {}),
      });
      await context.deps.sessionStore.saveSessionInput({
        id: admission.queueItemId,
        sessionID: sessionId as SessionId,
        kind,
        delivery: conversationInputIntent.delivery.admitted,
        payload: {
          text: conversationInputIntent.text,
          intent: {
            sourceCommandId: conversationInputIntent.sourceCommandId,
            queueItemId: conversationInputIntent.queueItemId,
            clientId: conversationInputIntent.clientId,
            kind: conversationInputIntent.kind,
            admissionSeq: admission.admissionSeq,
            admittedAt: admission.admittedAt,
            requestedDelivery: conversationInputIntent.delivery.requested,
            admittedDelivery: conversationInputIntent.delivery.admitted,
            ...(fallbackReasonCode ? { fallbackReasonCode } : {}),
            attachmentRefs,
            ...(conversationInputIntent.sharedContextRefs
              ? { sharedContextRefs: conversationInputIntent.sharedContextRefs }
              : {}),
          },
          conversationInputIntent,
          attachments: attachmentRefs,
          ...(conversationInputIntent.sharedContextRefs
            ? { sharedContextRefs: conversationInputIntent.sharedContextRefs }
            : {}),
          sourceCommandType: envelope.type,
        },
      });
      if (
        conversationInputIntent.sharedContextRefs?.length &&
        conversationInputIntent.delivery.admitted !== "startNow"
      ) {
        const reference = conversationInputIntent.sharedContextRefs[0]!;
        const reserved = await context.deps.sessionStore.transitionSharedContextImport?.({
          sessionID: sessionId as SessionId,
          contextId: reference.context_id,
          expectedStatus: "pending",
          status: "reserved",
          sourceId: admission.queueItemId,
        });
        if (!reserved) {
          await context.deps.sessionStore.settleSessionInput?.({
            id: admission.queueItemId,
            sessionID: sessionId as SessionId,
            status: "failed",
            reason: "shared_context_not_attachable",
          });
          throw new Error("fault.command.sharedContextNotAttachable");
        }
        const entry = (
          await context.deps.sessionStore.sessionEntries?.({
            sessionID: sessionId as SessionId,
            type: "v4/shared_context_import",
          })
        )?.find((candidate) => {
          const data = candidate.data;
          return Boolean(
            data &&
            typeof data === "object" &&
            !Array.isArray(data) &&
            (data as Record<string, unknown>).contextId === reference.context_id,
          );
        });
        const data = entry?.data;
        const session = await context.deps.sessionStore.getSession(sessionId as SessionId);
        if (
          data &&
          typeof data === "object" &&
          !Array.isArray(data) &&
          typeof (data as Record<string, unknown>).shareUrl === "string" &&
          session?.title
        ) {
          context.v4Gateway?.updateSharedContextImport(sessionId, {
            contextId: reference.context_id,
            title: session.title,
            shareUrl: String((data as Record<string, unknown>).shareUrl),
            status: "reserved",
          });
        }
      }
      return conversationInputIntent;
    },
    cancelInputCommand: async (sessionId, queueItemId, reason) => {
      await context.deps.sessionStore?.settleSessionInput?.({
        id: queueItemId,
        sessionID: sessionId as SessionId,
        status: "cancelled",
        reason,
      });
      const store = context.deps.sessionStore;
      const entries = await store?.sessionEntries?.({
        sessionID: sessionId as SessionId,
        type: "v4/shared_context_import",
      });
      const reserved = entries?.find((entry) => {
        const data = entry.data;
        return Boolean(
          data &&
          typeof data === "object" &&
          !Array.isArray(data) &&
          (data as Record<string, unknown>).status === "reserved" &&
          (data as Record<string, unknown>).sourceId === queueItemId,
        );
      });
      const contextId =
        reserved?.data && typeof reserved.data === "object"
          ? (reserved.data as Record<string, unknown>).contextId
          : undefined;
      if (typeof contextId === "string") {
        await store?.transitionSharedContextImport?.({
          sessionID: sessionId as SessionId,
          contextId,
          expectedStatus: "reserved",
          status: "pending",
          sourceId: queueItemId,
        });
      }
    },
    discardSharedContext: async (sessionId, contextId) => {
      const store = context.deps.sessionStore;
      if (!store?.transitionSharedContextImport) return false;
      const updated = await store.transitionSharedContextImport({
        sessionID: sessionId as SessionId,
        contextId,
        expectedStatus: "pending",
        status: "discarded",
      });
      if (updated) {
        const entry = (
          await store.sessionEntries?.({
            sessionID: sessionId as SessionId,
            type: "v4/shared_context_import",
          })
        )?.find((candidate) => {
          const data = candidate.data;
          return Boolean(
            data &&
            typeof data === "object" &&
            !Array.isArray(data) &&
            (data as Record<string, unknown>).contextId === contextId,
          );
        });
        const data = entry?.data;
        const session = await store.getSession(sessionId as SessionId);
        if (
          data &&
          typeof data === "object" &&
          !Array.isArray(data) &&
          typeof (data as Record<string, unknown>).shareUrl === "string" &&
          typeof (data as Record<string, unknown>).contextId === "string" &&
          session?.title
        ) {
          context.v4Gateway?.updateSharedContextImport(sessionId, {
            contextId: String((data as Record<string, unknown>).contextId),
            title: session.title,
            shareUrl: String((data as Record<string, unknown>).shareUrl),
            status: "discarded",
          });
        }
      }
      return updated;
    },
    recordPersistentCommandFact: async (sessionId, source, ack, metadata) => {
      const store = context.deps.sessionStore;
      const live = context.sessions.get(sessionId);
      const stored = await store?.getSession(sessionId as SessionId);
      if (!store || (!live && !stored)) {
        throw new Error("fault.command.persistentFactSessionNotFound");
      }
      await savePersistentCommandFact(store, sessionId as SessionId, source, ack, metadata);
      const workspacePath = live?.workspace.workspacePath ?? stored?.directory;
      if (!workspacePath) throw new Error("fault.command.persistentFactWorkspaceMissing");
      const workspaceIdentity = live?.workspace.workspaceIdentity ?? stored?.workspaceID;
      await persistentCommands.record(
        {
          workspacePath,
          ...(workspaceIdentity ? { workspaceIdentity: String(workspaceIdentity) } : {}),
        },
        sessionId,
        source,
        ack,
      );
    },
// held choice ruling (heldQueueInputRequiresChoice): read projection inputRouting.mode.
    getInputRoutingMode: (sessionId) => context.v4Gateway?.getInputRoutingMode(sessionId) ?? null,
// rowId→messageId translation surface (positioning decision of fork/edit/retry, data source = v4 projection):
//Lazy go through the projection lookup table of the gateway.
    getMessageIdForRow: (sessionId, rowId) =>
      context.v4Gateway?.getMessageIdForRow(sessionId, rowId) ?? null,
    resolveRowActionTarget: (sessionId, target, action) =>
      context.v4Gateway?.resolveRowActionTarget(sessionId, target, action) ?? null,
    getMessageIdsForTurnRow: (sessionId, rowId) =>
      context.v4Gateway?.getMessageIdsForTurnRow(sessionId, rowId) ?? [],
    isLatestAssistantSegmentRow: (sessionId, rowId) =>
      context.v4Gateway?.isLatestAssistantSegmentRow(sessionId, rowId) ?? null,
    resolveStableForkTarget: async (sessionId, rowId) => {
      const candidate = context.v4Gateway?.resolveStableForkCandidate(sessionId, rowId) ?? null;
      if (!candidate) return { ok: false, reasonCode: "guard.forkTargetAmbiguous" };
      if (!candidate.ok) return candidate;
      const store = context.deps.sessionStore;
      if (!store) return { ok: false, reasonCode: "guard.forkTargetAmbiguous" };
      const messages = await store.messages({ sessionID: sessionId as SessionId });
      return await resolveStableForkTargetFromTranscript({
        candidate: candidate.candidate,
        messages,
        store,
      });
    },
    isLatestRetryAssistantRow: (sessionId, rowId) =>
      context.v4Gateway?.isLatestRetryAssistantRow(sessionId, rowId) ?? null,
    isLatestEditableUserRow: (sessionId, rowId) =>
      context.v4Gateway?.isLatestEditableUserRow(sessionId, rowId) ?? null,
    getTurnIdForRow: (sessionId, rowId) =>
      context.v4Gateway?.getTurnIdForRow(sessionId, rowId) ?? null,
// restoreWarning timing self-healing probe: App's model view comes directly from the process Registry.
    hasUsableRuntimeModelTarget: (record) => record.app.listModels().length > 0,
    getTurnRewindAnchor: (sessionId, rowId) =>
      context.v4Gateway?.getTurnRewindAnchor(sessionId, rowId) ?? null,
    resolveUserMessageIdForRow: async (sessionId, rowId) => {
      const turnId = context.v4Gateway?.getTurnIdForRow(sessionId, rowId) ?? null;
      const sessionStore = context.deps.sessionStore;
      if (!turnId || !sessionStore) return null;
      const messages = await sessionStore.messages({
        sessionID: sessionId as SessionId,
      });
      const user = messages
        .filter(
          (message) =>
            message.info.role === "user" &&
            String(message.info.anchor?.turnId ?? "") === turnId &&
            isConversationRealUserTurnStarter(message),
        )
        .at(-1);
      return user ? String(user.info.id) : null;
    },
// retryTurn original prompt parsing: assistant messageId → parentID (user message) → text.
//Data source = core sessionStore (transcript authority); implement binder only because of deps injection point
// On the host (natively held by host). Returns null if not found → handler only truncates and does not resend.
    resolveTurnUserPrompt: async (sessionId, assistantMessageId) => {
      const sessionStore = context.deps.sessionStore;
      if (!sessionStore) return null;
      const messages = await sessionStore.messages({
        sessionID: sessionId as SessionId,
      });
      const assistantInfo = messages.find(
        (message) => message.info.id === assistantMessageId,
      )?.info;
      if (assistantInfo?.role !== "assistant") return null;
      const user = messages.find((message) => message.info.id === assistantInfo.parentID);
      if (!user || user.info.role !== "user") return null;
      const text = user.parts
        .filter(
          (part): part is Extract<(typeof user.parts)[number], { type: "text" }> =>
            part.type === "text" && part.ignored !== true,
        )
        .map((part) => part.text)
        .join("");
      return text.length > 0 ? text : null;
    },
    setAssistantFeedback: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      const sessionStore = context.deps.sessionStore;
      if (!record || !sessionStore) throw new Error("proto.sessionNotFound");
// Reason: The feedback must be dropped first, and the CLI can be restored from cold hydration after restarting;
// eventStore/projection is then advanced, and failed retries can still be completed idempotently from the same persistent fact.
      await persistAssistantFeedback({
        sessionStore,
        eventStore: record.eventStore,
        sessionId,
        messageId: input.messageId,
        entityId: input.entityId,
        feedback: input.feedback,
        traceId: String(record.traceContext.traceId),
        onPersistedEvent: (persisted) => context.v4Gateway?.ingest(sessionId, persisted),
        onLiveProjectionError: (error) =>
          context.logger?.warn("v4 assistant feedback live projection failed", {
            error: error instanceof Error ? error.message : String(error),
            sessionId,
          }),
      });
    },
//── Transition hook─────────────────────────────
    ensureModelReady: (record) =>
      ensureSessionModelAvailableForNextTurn(context, record as ZCodeProtocolSessionRecord),
// Confirm that the target Provider already exists in the current Environment Registry before cutting the model. Ordinary model commands only submit
// Selection; Provider facts are always interpreted by the Worker's own Registry.
    ensureProviderAvailable: async (sessionId, providerId) => {
      const record = context.sessions.get(sessionId);
      if (!record) return { available: false, reason: "session_not_found" };
      if (!hasSessionModelProvider(context, record, providerId)) {
        return { available: false, reason: "provider_not_in_registry" };
      }
      return { available: true };
    },
    afterLegacyStateMutation: async (record, reason) => {
      await afterStateMutation(context, record as ZCodeProtocolSessionRecord, reason);
      await autoDrainV4QueueIfReady(record as ZCodeProtocolSessionRecord);
    },
//Execution surface of deleteSession: 4 steps of inlining the old closeSession op (without importing the old op——
// The semantics are aligned with server-operations.ts closeSession, and will be compiled after the session registry is returned to v4).
    closeSession: async (sessionId) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
// The existence of the handler has been verified; only concurrency race conditions are covered here (duplicate deletion is idempotent and successful).
        return;
      }
      record.unsubscribe?.();
      await record.app.close?.();
// v4 channel: The session is closed and the publisher/subscription schedule is cleared at the same time; when the session is reopened, snapshot cold start is performed.
// disposeSession must be called before the registry is deleted——
// The gateway relies on getSessionWorkspaceId (read context.sessions) to locate the workspace.
// session.removed is pushed to sessions-index subscribers; delete first and then dispose workspaceId
// Always null, the sidebar list items will never disappear after deleting the session (caught by e2e conversation-session-v4-sidebar).
      context.v4Gateway?.disposeSession(sessionId);
      context.sessions.delete(sessionId);
    },
// The execution side of createSession: record creation/event wiring/catalog synchronization/failure self-cleaning are all in the old
//In createSession op (the recycling order of semi-initialized records has been fixed with bugs and will not be implemented repeatedly).
// Semantic decision (draft persistence / firstInput takes native prompt turn) in native handler.
    createSessionRecord: async ({
      workspaceId,
      mcpServers,
      offPeakToolEnabled,
      dynamicWorkflowEnabled,
    }) => {
// workspaceId double form (Workspace Identity constraint):
// - local workspace = workspacePath (identity default fallback);
// - remote pane (split screen across workspaces) = remote identity
// (remote:ssh/wsl/docker:...:<path>, UI buildRemoteWorkspaceIdentity construct).
//Restore the real workspacePath as workingDirectory through the unified parsing tool——CLI originally runs in
// On the remote machine, path is the local path; identity is retained as is in workspace ref
// (workspaceKey = identity, sessions-index topic / isolation semantics unchanged).
// shared parser is uniformly compatible with WSL legacy and explicit user identity; continue to press for non-remote formats
// Local workspacePath handling.
      const created = await createSessionRecordForV4(context, {
        workspace: resolveWorkspaceRefFromId(workspaceId),
// Always deferred (draft does not enter sqlite); the promotion time returns to the native prompt-turn.
        persistence: "deferred",
// MCP is a runtime creation configuration; v4 createSession must be the same as legacy
// session/create is equivalent to transparent transmission, otherwise the created session will never start these tools.
        mcpServers,
// The Off-Peak tool surface flag is also configured during runtime creation and must be entered into the record with create.
        ...(offPeakToolEnabled === true ? { offPeakToolEnabled: true } : {}),
// The dynamic workflow grayscale gate is also configured during runtime creation:
// v4 createSession must be transparently transmitted equivalently to legacy session/create, otherwise there will be no interface created session
// Will bypass the host's grayscale determination, leaving only the process-level default.
        ...(dynamicWorkflowEnabled === true ? { dynamicWorkflowEnabled: true } : {}),
      });
      return { sessionId: created.sessionId };
    },
    createSelectionSideSession: async (sessionId, options) => {
      const record = context.sessions.get(sessionId);
      if (!record) throw new Error("proto.sessionNotFound");
      const modelSelection = cloneModelSelection(
        options.modelSelection ?? record.app.runtime.getSessionModelSelection(),
      );
      const fork = await record.app.runtime.createSelectionSideConversation({
        modelSelection,
        sourceCommandId: options.sourceCommandId,
        revisionAtDecision: options.revisionAtDecision,
        traceContext: record.traceContext,
      });
      await registerCommittedForkBestEffort(context, record, fork, {
        commandId: options.sourceCommandId,
        runtimeConfig: {
          mode: record.app.getMode(),
          model: modelSelection ? `${modelSelection.providerId}/${modelSelection.modelId}` : "",
          ...(modelSelection?.options?.reasoningLevel
            ? { thoughtLevel: modelSelection.options.reasoningLevel }
            : {}),
          followupMode: context.v4Gateway?.getSessionFollowupMode(sessionId) ?? "queue",
        },
        inheritLatestTarget: false,
      });
      return { sessionId: String(fork.forkedSessionId) };
    },
// running stable fork: only take core transcript copy, and then register child record. parent runtime,queue,
// Background/continuation inbox and shared workspace do not read, stop, or copy.
    forkStableConversation: async (sessionId, options) => {
      const { goalBoundary, revisionAtDecision, sourceCommandId, target } = options;
      const record = context.sessions.get(sessionId);
      if (!record) throw new Error("proto.sessionNotFound");
      const store = context.deps.sessionStore;
      if (!store) throw new Error("fault.command.stableForkStoreUnavailable");
      const messages = await store.messages({ sessionID: sessionId as SessionId });
      const boundary = messages.find(
        (message) => String(message.info.id) === target.boundaryMessageId,
      );
      if (boundary?.info.role !== "assistant") {
        throw new Error("guard.forkTargetAmbiguous");
      }
      const modelSelection = modelSelectionWithOptionFallback(
        boundary.info.providerId && boundary.info.modelId
          ? {
              providerId: boundary.info.providerId,
              modelId: boundary.info.modelId,
              ...(boundary.info.reasoningLevel
                ? { options: { reasoningLevel: boundary.info.reasoningLevel } }
                : {}),
            }
          : undefined,
        cloneModelSelection(record.app.runtime.getSessionModelSelection()),
      );
      const fork = await record.app.runtime.forkStableConversationAtMessage({
        modelSelection,
        target,
        goalBoundary,
        sourceCommandId,
        revisionAtDecision,
        traceContext: record.traceContext,
      });
      await registerCommittedForkBestEffort(context, record, fork, {
        commandId: sourceCommandId,
        runtimeConfig: {
          mode: stableForkMode(boundary.info.mode, record.app.getMode()),
          model: modelSelection ? `${modelSelection.providerId}/${modelSelection.modelId}` : "",
          ...(modelSelection?.options?.reasoningLevel
            ? { thoughtLevel: modelSelection.options.reasoningLevel }
            : {}),
        },
// The core has copied the goal according to the copied message/verifier boundary; it is forbidden to overwrite it with the parent current target.
        inheritLatestTarget: false,
      });
      return { forkedSessionId: String(fork.forkedSessionId) };
    },
    forkConversationBeforeInput: async (sessionId, { editTarget, envelope, admission }) => {
      const record = context.sessions.get(sessionId);
      if (!record) throw new Error("proto.sessionNotFound");
      const store = context.deps.sessionStore;
      if (!store) throw new Error("fault.command.stableForkStoreUnavailable");
      const messages = await store.messages({
        sessionID: sessionId as SessionId,
      });
      const targetMessage = messages.find(
        (message) => String(message.info.id) === editTarget.transcriptMessageId,
      );
      if (targetMessage?.info.role !== "user") {
        throw new Error("guard.latestQueryEditOnly");
      }
      const modelSelection = modelSelectionWithOptionFallback(
        cloneModelSelection(targetMessage.info.modelSelection),
        cloneModelSelection(record.app.runtime.getSessionModelSelection()),
      );
      const events = await record.eventStore.getEvents(sessionId as SessionId);
      const targetStarted = events.find(
        (event) =>
          event.type === SessionEventType.TurnStarted &&
          String((event.payload as { messageId?: unknown }).messageId ?? "") ===
            editTarget.transcriptMessageId,
      );
      const priorTargetChange = targetStarted
        ? events
            .filter(
              (event) =>
                event.sequenceNumber < targetStarted.sequenceNumber &&
                event.type === SessionEventType.TargetChanged,
            )
            .at(-1)
        : undefined;
      const forkedSessionId = String(createSessionId());
      const input = resolveInputCommandForAdmission(
        envelope,
        forkedSessionId,
        (sourceSessionId, target, action) =>
          context.v4Gateway?.resolveRowActionTarget(sourceSessionId, target, action) ?? null,
      );
      if (!input) throw new Error("fault.command.forkInputAdmissionMissing");
      const initialInput = buildForkInitialInput(envelope, forkedSessionId, admission, input);
      let goalBoundary: StableForkGoalBoundaryMetadata | null = priorTargetChange
        ? (() => {
            const target = (priorTargetChange.payload as { target?: unknown }).target;
            return target
              ? {
                  kind: "snapshot" as const,
                  target: target as Extract<
                    StableForkGoalBoundaryMetadata,
                    { kind: "snapshot" }
                  >["target"],
                  verificationEntryIds: [],
                }
              : { kind: "none" as const };
          })()
        : null;
      if (goalBoundary?.kind === "snapshot" && store.sessionEntries && targetStarted) {
        const targetId = goalBoundary.target.targetID;
        const boundaryTime = targetStarted.timestamp.getTime();
        const entries = await store.sessionEntries({
          sessionID: sessionId as SessionId,
          type: SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
        });
        goalBoundary = {
          ...goalBoundary,
          verificationEntryIds: entries.flatMap((entry) => {
            const data = entry.data as { payload?: { targetId?: unknown } };
            return entry.time.updated <= boundaryTime && data.payload?.targetId === targetId
              ? [entry.id]
              : [];
          }),
        };
      }
      if (!goalBoundary) {
        const targetIndex = messages.indexOf(targetMessage);
        const previousAssistant = messages
          .slice(0, targetIndex)
          .reverse()
          .find((message) => message.info.role === "assistant");
        goalBoundary = previousAssistant?.info.anchor?.goalBoundary ?? null;
        if (!previousAssistant) goalBoundary = { kind: "none" };
      }
      if (!goalBoundary) {
        throw new Error("guard.forkTargetAmbiguous");
      }
      const fork = await record.app.runtime.forkConversationBeforeMessage({
        modelSelection,
        forkedSessionId: forkedSessionId as SessionId,
        targetMessageId: targetMessage.info.id,
        targetProductTurnId: editTarget.productTurnId,
        targetTranscriptTurnId: String(
          targetMessage.info.anchor?.turnId ?? editTarget.productTurnId,
        ),
        sourceCommandId: envelope.commandId,
        initialInput,
        commandFact: {
          parentSessionId: sessionId,
          sourceCommandId: envelope.commandId,
          ack: {
            commandId: envelope.commandId,
            status: "accepted",
            revisionAtDecision: envelope.baseRevision ?? 0,
            result: {
              type: "editUserQuery",
              disposition: "fork",
              sessionId: forkedSessionId,
            },
          },
          metadata: {
            parentSessionId: sessionId,
            sourceCommandId: envelope.commandId,
            editTarget,
          },
        },
// Strictly take the TargetChanged before TurnStarted or the previous stable assistant anchor; prohibited
// Pretend parent's current target (which may be being written by the edited goal) as the input's previous state.
        goalBoundary,
        traceContext: record.traceContext,
      });
      await registerCommittedForkBestEffort(context, record, fork, {
        commandId: envelope.commandId,
        runtimeConfig: {
          mode: record.app.getMode(),
          model: modelSelection ? `${modelSelection.providerId}/${modelSelection.modelId}` : "",
          ...(modelSelection?.options?.reasoningLevel
            ? { thoughtLevel: modelSelection.options.reasoningLevel }
            : {}),
        },
        inheritLatestTarget: false,
      });
      return { forkedSessionId: String(fork.forkedSessionId) };
    },
    recordForkStartFailure: async (sessionId, envelope, error) => {
      await recordForkStartFailureBestEffort(context, sessionId, envelope, error, {
        parentSessionId: String(envelope.sessionId ?? ""),
      });
    },
  };
  nativeExecutor = new V4CommandExecutor(coreHost);
  const loadStoredSessionSummaries = async (
    workspaceId: string,
    legacyTaskIds?: readonly string[],
  ) => {
//Lightweight summary of the unloaded session: store meta information → SessionSummary (phase takes the idle completion state by default,
// sessionEnded=true aligns with the "successful round closing is true" caliber; accurate after loading
// phase/preview/backgroundWork is overridden by gateway with live projection).
// The local fallback of workspaceKey = workspacePath, so use it as directory filtering for listSessions.
    if (!context.deps.sessionStore) return [];
    try {
// The workspaceId of the remote sessions-index is the isolated identity, and the session store's
// directory is the actual file path. The query must contain both path and identity; otherwise, other queries under the same path
// The authority's session will be mislabeled as the current workspace. legacy empty identity cannot be based on path alone
// claim, only the precise taskId ownership certificate given by host task-index is allowed.
      const parsedRemote = parseRemoteWorkspaceIdentity(workspaceId);
      const persistedWorkspacePath = parsedRemote?.workspacePath ?? workspaceId;
      if (
        parsedRemote &&
        legacyTaskIds &&
        legacyTaskIds.length > 0 &&
        context.deps.sessionStore.claimLegacySessionWorkspace
      ) {
        try {
          const claimedCount = await context.deps.sessionStore.claimLegacySessionWorkspace({
            sessionIDs: legacyTaskIds as SessionId[],
            directory: persistedWorkspacePath,
            workspaceID: workspaceId as WorkspaceId,
          });
          if (claimedCount > 0) {
            context.logger?.info("legacy remote sessions claimed by task-index allowlist", {
              claimedCount,
              event: "zcode_protocol.v4.sessions_index_legacy_remote_claimed",
              module: "bootstrap.zcode_protocol",
              workspaceId,
            });
          }
        } catch (error) {
// claim is only an old data compatibility step; if it fails, the session with full identity will still be read.
// Subsequent subscriptions carrying allowlist will enter here again, and the migration cannot be blocked with failure results.
          context.logger?.warn("legacy remote sessions claim failed; continuing strict load", {
            error: error instanceof Error ? error.message : String(error),
            event: "zcode_protocol.v4.sessions_index_legacy_remote_claim_failed",
            module: "bootstrap.zcode_protocol",
            workspaceId,
          });
        }
      }
      const stored = await context.deps.sessionStore.listSessions({
        directory: persistedWorkspacePath,
        includeArchived: false,
        limit: 200,
// parentID only expresses the session level and cannot be used as left-hand task membership.
// Explicit fork must have parentID, but taskType should still be projected into sessions-index after restart.
        taskTypes: [...TASK_LIST_SESSION_TYPES],
        workspaceID: parsedRemote ? (workspaceId as WorkspaceId) : null,
      });
      return stored.map((session) => ({
        sessionId: String(session.id),
        workspaceId,
        ...(session.parentID ? { parentSessionId: String(session.parentID) } : {}),
        title: session.title ?? "",
        titleSource: normalizeStoredTitleSource(session.titleSource),
        phase: "completedSuccess" as const,
        sessionEnded: true,
        hasBackgroundWork: false,
        lastActivityAt: session.time?.updated ?? 0,
        createdAt: session.time?.created ?? 0,
      }));
    } catch (error) {
      context.logger?.warn("sessions-index stored summaries failed", {
        error: error instanceof Error ? error.message : String(error),
        event: "zcode_protocol.v4.sessions_index_stored_failed",
        module: "bootstrap.zcode_protocol",
      });
      return [];
    }
  };
  return new ConversationV4Gateway({
    cliVersion: context.deps.version,
    sessionExists: (sessionId) => context.sessions.has(sessionId),
    onDebug: (message) => log?.debug(message),
    onTargetCompleted: (sessionId) => {
      const record = context.sessions.get(sessionId);
      if (!record) return;
// The goal verifier of background task-notification does not pass through v4 prompt.
// finally/afterLegacyStateMutation; TargetChanged(complete) Although it has been submitted, the future queue
// So there is no next mutation to re-evaluate. Here only detached triggers the existing gate and cannot block projection.
      void Promise.resolve()
        .then(() => autoDrainV4QueueIfReady(record))
        .catch((error: unknown) => {
          context.logger?.warn("v4 auto-drain reevaluation after target completion failed", {
            error: error instanceof Error ? error.message : String(error),
            sessionId,
          });
        });
    },
// Gateway's single READY promise is responsible for concurrency and water level; binder only restores runtime.
    resumePersistedSession: async (
      sessionId,
      resumeThoughtLevel,
      workspace?: ZCodeWorkspaceRef,
    ) => {
      const persisted = await context.deps.sessionStore?.getSession(sessionId as SessionId);
      if (!persisted) {
        context.logger?.warn("ZCode Protocol v4 cold resume has no persisted session", {
          activeSessionCount: context.sessions.size,
          event: "zcode_protocol.v4.resume_persisted_missing",
          module: "bootstrap.zcode_protocol",
          sessionId,
        });
        return { status: "notFound" };
      }
      const activated = await activateSessionForResume(
        context,
        {
          sessionId,
// session.path may be the normalized execution cwd and cannot overwrite the current attachment
// Known workspace identity. When the old session does not have an attachment context, the original persistence fallback will still be used.
          ...(workspace ? { workspace } : {}),
          ...(resumeThoughtLevel ? { thoughtLevel: resumeThoughtLevel } : {}),
        },
        { reusePersistedMessages: true },
      );
      return {
        status: "resumed",
        persistedMessages: activated.persistedMessages,
      };
    },
    emitWireFrame: (wire) =>
      context.notify({
        method: V4_NOTIFICATIONS.conversationFrame,
        params: wire,
      }),
    emitLocalTtftFacts: (facts) =>
      context.notify({ method: V4_NOTIFICATIONS.localTtftFacts, params: facts }),
    emitConversationTelemetryFact: (fact) =>
      context.notify({
        method: V4_NOTIFICATIONS.conversationTelemetryFact,
        params: fact,
      }),
    emitCuaPermissionObservation: (observation) =>
      context.notify({
        method: V4_NOTIFICATIONS.cuaPermissionObservation,
        params: observation,
      }),
// ── config seed: projection initial value = runtime true value ──────────────
// Cover three seed sources: startup default (Workspace model preference + project persistence mode),
// createSession.config (handler is applied to runtime first and then planted), historical session resume
// (App recovery results can only have model identities, and cannot be bound to semi-finished execution models for projection).
    getSessionMemoryEnabled: (sessionId) => context.sessions.get(sessionId)?.memoryEnabled,
    getSessionConfigSeed: (sessionId) => {
      const record = context.sessions.get(sessionId);
      if (!record) return null;
      const selection =
        record.app.runtime.getSessionModelSelection() ?? record.restoredModelSelection;
      return {
        modelSelection: cloneModelSelection(selection),
        provider: selection?.providerId ?? "",
        model: selection?.modelId ?? "",
        thought: selection?.options?.reasoningLevel ?? "",
        thoughtLevels: selection
          ? (record.app
              .listModels()
              .find(
                (model) =>
                  model.ref.providerId === selection.providerId &&
                  model.ref.modelId === selection.modelId,
              )
              ?.reasoning?.levels.map((level) => level.value) ?? [])
          : [],
        mode: record.app.getMode(),
        planEnabled: record.app.runtime.getPlanEnabled(),
        ...(record.app.runtime.lastPermissionGrantId
          ? { permissionGrant: { interactionId: record.app.runtime.lastPermissionGrantId } }
          : {}),
      };
    },
    getSessionUsageSeed: async (sessionId, persistedMessages) => {
      const record = context.sessions.get(sessionId);
      if (!record) return null;
      const contextUsage = await readSessionContextUsage(context, sessionId, persistedMessages);
// usage seed will overwrite the denominator of the first frame again after hydration; must be combined with the composite event
// Using the same true value of the current model registry, the window of the old runtime projection cannot be written back.
      return sessionUsageSeedFromRuntimeContextUsage(
        contextUsage,
        resolveSessionModelContextWindow(context, record),
      );
    },
//── sessions-index hooks (workspace bucketing + cold start store seed)──────────
    getSessionWorkspaceId: (sessionId) => {
      const record = context.sessions.get(sessionId);
      return !record || !isTaskListSessionType(record.taskType)
        ? null
        : record.workspace.workspaceKey;
    },
    getSessionIndexMeta: (sessionId) => {
      const record = context.sessions.get(sessionId);
      if (!record) return null;
      return {
        createdAt: record.createdAt,
        lastActivityAt: record.updatedAt,
        ...(record.parentSessionId ? { parentSessionId: String(record.parentSessionId) } : {}),
      };
    },
    listWorkspaceSessionIds: (workspaceId) =>
      [...context.sessions.values()]
        .filter(
          (record) =>
            isTaskListSessionType(record.taskType) && record.workspace.workspaceKey === workspaceId,
        )
        .map((record) => record.app.sessionId),
// Draft judgment: deferred = the first input is not sent (prompt-turn is promoted to immediate).
// Pre-built deferred sessions of the old workspace prepare must not leak into the sidebar list as "new tasks".
    isDraftSession: (sessionId) => context.sessions.get(sessionId)?.persistence === "deferred",
//── workspace-config hook (configuration directory subscription seed; live session fast path to avoid temporary apps)──
    getWorkspaceConfig: (workspaceId) => buildLiveWorkspaceConfigStateV4(context, workspaceId),
    getStoredSessionSummaries: loadStoredSessionSummaries,
    refreshLegacySessionSummaries: (workspaceId, legacyTaskIds) =>
      parseRemoteWorkspaceIdentity(workspaceId)
        ? loadStoredSessionSummaries(workspaceId, legacyTaskIds)
        : null,
// The fallback surface has been cleared (20 commands are all native): supports miss (unknown command type) →
    // notImplemented → ACK failed fault.command.notImplemented。
    executeCommand: (envelope, admission) =>
      nativeExecutor.supports(envelope.type)
        ? nativeExecutor.execute(envelope, admission)
        : Promise.reject(new V4CommandNotImplementedError(envelope.type)),
    admitCommandInput: async (envelope, admission) => {
// Merely hiding composer does not prevent old child tabs from continuing. Type access must be earlier than
// ledger/input history writing; when detached child has no record, only metadata is checked and the second runtime is not activated.
      if (
        envelope.sessionId &&
        (isConversationInputAdmissionCommand(envelope.type) ||
          envelope.type === "resumeGoal" ||
          envelope.type === "sendQueuedNow" ||
          envelope.type === "forkAssistant" ||
          envelope.type === "createSelectionSideSession")
      ) {
        const taskType =
          context.sessions.get(envelope.sessionId)?.taskType ??
          (await context.deps.sessionStore?.getSession(envelope.sessionId as SessionId))?.taskType;
        if (taskType === "subagent_child") {
          throw Object.assign(new Error("Subagent sessions are read-only"), {
            reasonCode: "guard.subagentReadOnly",
          });
        }
      }
      if (!isConversationInputAdmissionCommand(envelope.type)) return null;
      if (!envelope.sessionId) return null;
      return (await coreHost.admitInputCommand?.(envelope, envelope.sessionId, admission)) ?? null;
    },
    cancelCommandInput: async (envelope, queueItemId, reason) => {
      if (!envelope.sessionId) return;
      await coreHost.cancelInputCommand?.(envelope.sessionId, queueItemId, reason);
    },
    terminateTurnForProjectionFault: (sessionId, reasonCode) => {
      const record = context.sessions.get(sessionId);
      const controller = record?.activeAbortController;
      if (!controller || controller.signal.aborted) return;
// Continuing to generate after the projection exceeds 16MiB will only make all subsequent snapshots unable to be encoded.
// The gateway first atomically rejects the out-of-bounds event and registers a protocol fault, and then calls here a single time to terminate the model turn;
// The normal final state of abort is responsible for releasing the active lock, and TurnError cannot be forged across layers in the gateway.
      controller.abort(createExternalTurnFaultError(reasonCode));
    },
// commands/query persistence fallback: same as session first query to build lazy index, subsequent four sources
// Share this index; anchor/marker/child/discarded is written and the record is incrementally updated.
    lookupTranscriptCommand: (key) =>
      key.sessionId === null
        ? lookupGlobalCreateSessionCommand(context.deps.sessionStore, key.commandId)
        : persistentCommands.lookup("transcript", key),
    lookupTimelineCommand: (key) => persistentCommands.lookup("timeline", key),
    lookupChildCommand: (key) => persistentCommands.lookup("child", key),
    lookupDiscardedCommand: (key) => persistentCommands.lookup("discarded", key),
    invalidatePersistentCommandFacts: (sessionId) => persistentCommands.invalidate(sessionId),
// The gateway has completed piece-by-piece total/checksum verification, and only writes complete bytes atoms to the artifact.
    putSessionAttachment: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.attachment.sessionNotFound: ${sessionId}`);
      }
      return record.app.writePromptAttachment(input);
    },
    readBackgroundBashOutput: (sessionId, workId) =>
      readBackgroundBashOutputFromOwner(context, sessionId, workId),
    readSessionAttachment: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.attachment.sessionNotFound: ${sessionId}`);
      }
      return record.app.readPromptAttachment(input);
    },
    statSessionAttachment: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.attachment.sessionNotFound: ${sessionId}`);
      }
      if (!record.app.statPromptAttachment) {
        throw new Error("fault.attachment.statUnsupported");
      }
      return record.app.statPromptAttachment(input);
    },
    resolveSessionAttachmentPreviewSource: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.attachment.sessionNotFound: ${sessionId}`);
      }
      return record.app.resolvePromptAttachmentPreviewSource(input);
    },
    getConversationFileChanges: async (sessionId, _targetRowId, messageIds, targetTurnId) => {
      const record = await resolveConversationBackingRecord(context, sessionId);
      if (!record) {
        throw new Error(`fault.fileChanges.sessionNotFound: ${sessionId}`);
      }
      return readConversationFileChanges(record, sessionId, messageIds, targetTurnId);
    },
// dwf event log: The ability is on the app (only available when the run service is constructed successfully). If it is absent, it will not be left here and will become an empty page——
// The gateway will return an error that structured capabilities are not supported, allowing the renderer to distinguish between "no event" and "no such capability".
    listDynamicWorkflowRunEvents: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.workflowRunEvents.sessionNotFound: ${sessionId}`);
      }
      if (!record.app.listDynamicWorkflowRunEvents) {
        throw new V4CapabilityUnsupportedError("listDynamicWorkflowRunEvents", sessionId);
      }
// Called via app (not destructible: implementation may rely on this binding).
      return record.app.listDynamicWorkflowRunEvents(input);
    },
// workflow run enumeration: The capability conditions are the same as above (only if the run service is constructed successfully).
    listDynamicWorkflowRuns: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.workflowRuns.sessionNotFound: ${sessionId}`);
      }
      if (!record.app.listDynamicWorkflowRuns) {
        throw new V4CapabilityUnsupportedError("listDynamicWorkflowRuns", sessionId);
      }
// Called via app (not destructible: implementation may rely on this binding).
      return record.app.listDynamicWorkflowRuns(input);
    },
// The three reading aspects of dwf user interface products: the ability conditions are the same as above.
// ⚠ Terminology: artifact = the output of a script published to the user via `artifact.*`, not the top-level return value of run.
    listDynamicWorkflowRunArtifacts: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.workflowRunArtifacts.sessionNotFound: ${sessionId}`);
      }
      if (!record.app.listDynamicWorkflowRunArtifacts) {
        throw new V4CapabilityUnsupportedError("listDynamicWorkflowRunArtifacts", sessionId);
      }
      return record.app.listDynamicWorkflowRunArtifacts(input);
    },
    listDynamicWorkflowRunArtifactItems: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.workflowRunArtifactData.sessionNotFound: ${sessionId}`);
      }
      if (!record.app.listDynamicWorkflowRunArtifactItems) {
        throw new V4CapabilityUnsupportedError("listDynamicWorkflowRunArtifactItems", sessionId);
      }
      return record.app.listDynamicWorkflowRunArtifactItems(input);
    },
    readDynamicWorkflowRunArtifact: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.workflowRunArtifactRead.sessionNotFound: ${sessionId}`);
      }
      if (!record.app.readDynamicWorkflowRunArtifact) {
        throw new V4CapabilityUnsupportedError("readDynamicWorkflowRunArtifact", sessionId);
      }
      return record.app.readDynamicWorkflowRunArtifact(input);
    },
//Two reading surfaces of dwf workspace transcript: The ability conditions are the same as above.
    listDynamicWorkflowRunWorkspaceNodes: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.workflowRunWorkspace.sessionNotFound: ${sessionId}`);
      }
      if (!record.app.listDynamicWorkflowRunWorkspaceNodes) {
        throw new V4CapabilityUnsupportedError("listDynamicWorkflowRunWorkspaceNodes", sessionId);
      }
      return record.app.listDynamicWorkflowRunWorkspaceNodes(input);
    },
    readDynamicWorkflowRunNodeResult: async (sessionId, input) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.workflowRunNodeResult.sessionNotFound: ${sessionId}`);
      }
      if (!record.app.readDynamicWorkflowRunNodeResult) {
        throw new V4CapabilityUnsupportedError("readDynamicWorkflowRunNodeResult", sessionId);
      }
      return record.app.readDynamicWorkflowRunNodeResult(input);
    },
    previewConversationFileRewind: async (sessionId, _targetRowId, messageIds, targetTurnId) => {
      const record = context.sessions.get(sessionId);
      if (!record) {
        throw new Error(`fault.fileRewindPreview.sessionNotFound: ${sessionId}`);
      }
      return previewConversationFileRewind(record, messageIds, targetTurnId);
    },
// Cold subscriptions no longer XOR between eventStore / transcript. message/part
// It is the completed text authority, session_entry only complements the legacy goal, and memory events only complement
//Ephemeral states such as in-flight and queue/permission/control are not persisted.
    loadPersistedEvents: async (sessionId, persistedMessages) => {
// dwf workflow actor / subagent This type of detached live child does not have its own
// bootstrap record (the event takes the sink route of the parent record via ingestDetachedLiveSession).
// context.sessions.get cannot directly return synthesized:false when the record cannot be obtained——
// Otherwise, the performHydration subscribed for the first time will take the "preserve healthy live publisher" branch and exit early.
// durable transcript Three-source merge is never performed - projections fed only by live events will lose all
// Persistent text that does not appear as a live event. amend-resume prefixes the precursor transcript directly
// Copy into session store to seed actor sessions,
// This prefix falls into this category, so the sidebar actor transcript only has this live increment left;
// After normal crash recovery, the warm window will also not be able to see the actor messages before the crash.
// Use resolveConversationBackingRecord instead: press persistence when the child has no record itself
// parentID falls to the parent record, only borrowing its shared event/artifact store, event reading is still explicitly used
// child's own sessionId (script workflow child runtime shared parent event store,
//Events are filed by child sessionId), so sourceEventSeq is still the child's true water level.
// Cost: The denominator of contextWindow will be parsed according to the current model of the parent record instead of the actor model, which is a pure presentation layer deviation.
      const record = await resolveConversationBackingRecord(context, sessionId);
      if (!record) {
// Diagnosis: hydrate is expected to be executed after the runtime has been activated by cold-resume; even the parent record is kept secret
// When all fails, returning an empty event will disguise the real life cycle race condition as "the history is empty", and a clear scene must be left.
        context.logger?.warn("ZCode Protocol v4 hydrate has no active runtime", {
          activeSessionCount: context.sessions.size,
          event: "zcode_protocol.v4.hydrate_runtime_missing",
          module: "bootstrap.zcode_protocol",
          phase: "loadPersistedEvents",
          sessionId,
        });
        return { events: [], synthesized: false, sourceEventSeq: 0 };
      }
// During asynchronous reading of message/part and session_entry, the live sink can still receive new events.
// The gateway must know the raw cursor of the memory eventStore when taking the snapshot, so that it can only fill in the await window
// The tail of the transcript, and stably map the 1..N sequence synthesized by transcript back to the subsequent runtime raw seq.
// The memory event store will eliminate transient events that have completed the turn, and max(events.seq) will be smaller than the real
// Cursor, let the eliminated delta be replayed as the end of the await window. There is no await between the two calls, what you get is
// A consistent snapshot of the same moment.
      const [liveEvents, sourceEventSeq] = await Promise.all([
        record.eventStore.getEvents(sessionId as SessionId),
        record.eventStore.getLatestSequenceNumber(sessionId as SessionId),
      ]);
//Cold playback of workflow run: progress played back by journal
// Events are forwarded before memory events - cold merge has classified this type as memory-only authoritative (order-preserving supplements),
// The projection is reduced by the same reducer, so `workflowRuns` is consistent across restarts.
      const replayed = await replayDynamicWorkflowRunEvents(context, sessionId, record, liveEvents);
      const events = replayed.length === 0 ? liveEvents : [...replayed, ...liveEvents];
      const store = context.deps.sessionStore;
      const source = await loadPersistedConversationMaterialization({
        memoryEvents: events,
        persistedMessages,
        sessionId,
        ...(store
          ? {
              store: {
                getSession: (id) => store.getSession(id),
                messages: (input) => store.messages(input),
                readTarget: (input) => store.readTarget(input),
                ...(store.sessionEntries
                  ? {
                      sessionEntries: (input) =>
                        store.sessionEntries!(input).catch((error) => {
                          context.logger?.warn("v4 hydrate session entries read failed", {
                            error: error instanceof Error ? error.message : String(error),
                            event: "zcode_protocol.v4.hydrate_session_entries_failed",
                            module: "bootstrap.zcode_protocol",
                          });
                          return [];
                        }),
                    }
                  : {}),
              },
            }
          : {}),
      });
// live ModelComplete.fileChanges only exists in memory events; cold merge to persist
// When transcript is the authoritative text, this event will be suppressed, and transcript itself does not have a file summary field.
// workspace checkpoint + artifact is the cross-process persistence fact, here press user messageId
// Reconstruct the summary and then hand it over to transcript hydration to synthesize isomorphic ModelComplete.
      const fileChangeSummariesByMessageId = await buildColdFileChangeSummaries({
        events: source.memoryEvents,
        messageIds: source.messages.map((message) => String(message.info.id)),
        readArtifact: async (snapshotRef) =>
          (await record.app.readToolResultArtifact(snapshotRef)).content,
        onArtifactError: (messageId, error) =>
          context.logger?.warn("v4 cold file change artifact read failed", {
            error: error instanceof Error ? error.message : String(error),
            event: "zcode_protocol.v4.hydrate_file_changes_failed",
            messageId,
            module: "bootstrap.zcode_protocol",
            sessionId,
          }),
      });
// Cold recovery transcript does not save model capabilities, and the old hydration fills in 200,000 by itself;
//The provider registry has been synchronized before resume and should be accurately valued according to the current model after resume/retreat.
      const contextWindow = resolveSessionModelContextWindow(context, record);
      const usageSeed = sessionUsageSeedFromRuntimeContextUsage(
        await readSessionContextUsage(context, sessionId, source.messages),
        contextWindow,
      );
      const merged = mergeColdConversationEvents({
        contextWindow,
        fileChangeSummariesByMessageId,
        memoryEvents: source.memoryEvents,
        messages: source.messages,
        sessionId,
        goalVerificationEntries: source.goalVerificationEntries,
        ...(Object.prototype.hasOwnProperty.call(source, "target")
          ? { target: source.target }
          : {}),
      });
      for (const diagnostic of merged.diagnostics) {
        const fields = {
          ...diagnostic,
          event: "zcode_protocol.v4.hydrate_three_source_merge",
          module: "bootstrap.zcode_protocol",
          sessionId,
        };
        if (
          diagnostic.code === "cold_merge.ambiguous_legacy_turn_preserved" ||
          diagnostic.code === "cold_merge.memory_boundary_preserved" ||
          diagnostic.code === "cold_merge.unclassified_event_preserved"
        ) {
          context.logger?.warn("v4 hydrate preserved ambiguous cold fact", fields);
        } else {
          log?.debug("v4 hydrate merged duplicate cold facts", fields);
        }
      }
// transcript can restore the Agent row, but it cannot prove that the child session has been dropped.
// Here, the verification seed is generated before the gateway's raw-event buffer is filled in, which excludes old ghost references.
// This also prevents asynchronous queries from overwriting the newly arrived live spawn/stop after seed.
      const subagents = await listSessionSubagents(
        context,
        { sessionId, endedLimit: 1 },
        persistedMessages,
      );
      return {
        events: merged.events,
// Share this query result with the synthetic event; do not re-read another capacity in the subsequent backfill stage.
        usageSeed,
// The old gateway field name is still called synthesized; here it means that the projection has been replaced by durable
// transcript is materialized and needs to replace the cold publisher built first by ingest.
        synthesized: merged.usedDurableTranscript,
        subagentsSeed: {
          revision: subagents.revision,
          childSessionIds: subagents.childSessionIds,
          running: subagents.running,
        },
        ...(source.sharedContextImport ? { sharedContextImport: source.sharedContextImport } : {}),
        sourceEventSeq,
      };
    },
    onError: (scope, error, errorContext) =>
      context.logger?.warn("ZCode Protocol v4 gateway error", {
        ...errorContext,
        error: error instanceof Error ? error.message : String(error),
        event: "zcode_protocol.v4.gateway_error",
        module: "bootstrap.zcode_protocol",
        scope,
      }),
  });
}
