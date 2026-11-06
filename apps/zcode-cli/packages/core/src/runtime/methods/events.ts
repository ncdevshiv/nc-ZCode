/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import type { WorkspaceId } from "@zcode/contracts";
import { buildExecutionStateEntry, readRuntimeExecutionState } from "../execution-state.js";
import {
  SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
  SESSION_ENTRY_USER_INPUT_AUTO_RESOLUTION,
  SessionEventType,
  createMessageId,
  createPartId,
  createSessionEvent,
  traceContextToLogContext,
} from "../deps.js";
import type {
  MessageId,
  PartId,
  SessionEvent,
  SessionId,
  TargetCompletionVerificationPayload,
  TraceContext,
  TurnInputIntentMetadata,
  UserInputAutoResolutionUpdatedPayload,
} from "../deps.js";
import { titleFromInput, slugify, projectIdFromDirectory } from "../helpers/index.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { buildPersistedConversationInputIntent } from "./input-intent-persistence.js";
import { recordToolUsageFromEvent } from "./usage-observability.js";
import { persistSessionShellEnvironmentSnapshot } from "./session-shell-environment.js";
import { persistRuntimeModelSelection } from "./turn-model.js";
import {
  persistWorkspaceCheckpointEntry,
  persistWorkspaceFileRewindEntry,
} from "./workspace-checkpoint-persistence.js";

const SESSION_EVENT_APPEND_SUMMARY_FLUSH_COUNT = 100;

const SUMMARY_SESSION_EVENT_TYPES = new Set<SessionEventType>([
  SessionEventType.ModelStreaming,
  SessionEventType.ModelNetworkStatus,
  SessionEventType.StreamingToolLedgerUpdated,
  SessionEventType.ToolCallProgress,
]);

// The production environment only records low-frequency events that will change the Turn/Session life cycle; stream/progress is still controlled by
// Existing debug aggregate log coverage prevents diagnostic logs and message streams from being flushed at the same time.
const LIFECYCLE_SESSION_EVENT_TYPES = new Set<SessionEventType>([
  SessionEventType.SessionTitleUpdated,
  SessionEventType.TurnStarted,
  SessionEventType.ModelRequest,
  SessionEventType.ModelComplete,
  SessionEventType.TurnComplete,
  SessionEventType.TurnError,
]);

interface SessionEventAppendAggregate {
  eventCount: number;
  eventType: SessionEventType;
  firstEventId: string;
  firstSessionEventSequenceNumber: number;
  lastEventId: string;
  lastSessionEventSequenceNumber: number;
  payloadBytes: number;
  payloadKinds: Record<string, number>;
}

const sessionEventAppendAggregates = new WeakMap<
  AgentRuntimeInternal,
  Map<string, SessionEventAppendAggregate>
>();

export function createEvent(
  this: AgentRuntimeInternal,
  type: SessionEventType,
  payload: unknown,
  traceContext: TraceContext,
): SessionEvent {
  return createSessionEvent(type, this.sessionId, payload, {
    turnId: traceContext.turnId,
    traceId: traceContext.traceId,
  });
}

export async function appendEvent(
  this: AgentRuntimeInternal,
  event: SessionEvent,
  traceContext: TraceContext,
): Promise<void> {
// The live sink used to get the default sequenceNumber=0 of createSessionEvent.
//The replay/read path gets the events after the eventStore's complement number, resulting in two sets of sequence facts for the same session.
// Only events that have been dropped are published here, so that the eventSeq of live, replay, and snapshot all come from the same event store.
  const shouldLogLifecycle = LIFECYCLE_SESSION_EVENT_TYPES.has(event.type);
  const startedAt = Date.now();
  if (shouldLogLifecycle) {
    this.logger?.info("Session event persistence started", {
      ...traceContextToLogContext(traceContext),
      event: "session.event.persistence.started",
      module: "core.runtime",
      sessionEventType: event.type,
      status: "started",
    });
  }

  let phase = "event_store.append";
  try {
    const storedEvent = await this.eventStore.append(event);
    phase = "session_event.persist_durable";
    await persistDurableSessionEvent.call(this, storedEvent, traceContext);
    phase = "session_event.record_usage";
    await recordToolUsageFromEvent(this, storedEvent, traceContext);
    phase = "session_event.notify_sinks";
    await this.notifyEventSinks(storedEvent, traceContext);
    if (shouldLogLifecycle) {
      this.logger?.info("Session event persistence completed", {
        ...traceContextToLogContext(traceContext),
        durationMs: Date.now() - startedAt,
        event: "session.event.persistence.completed",
        module: "core.runtime",
        sessionEventSequenceNumber: storedEvent.sequenceNumber,
        sessionEventType: storedEvent.type,
        status: "completed",
      });
    }
    if (recordSessionEventAppendAggregate.call(this, storedEvent, traceContext)) {
      return;
    }
    flushSessionEventAppendAggregates.call(this, traceContext, "low_frequency_event");
    this.logger?.debug("Session event appended", {
      ...traceContextToLogContext(traceContext),
      event: "event_store.appended",
      module: "core.runtime",
      sessionEventSequenceNumber: storedEvent.sequenceNumber,
      sessionEventType: storedEvent.type,
    });
  } catch (error) {
    if (shouldLogLifecycle) {
      this.logger?.warn("Session event persistence failed", {
        ...traceContextToLogContext(traceContext),
        durationMs: Date.now() - startedAt,
        errorMessage: error instanceof Error ? error.message : String(error),
        event: "session.event.persistence.failed",
        module: "core.runtime",
        phase,
        sessionEventType: event.type,
        status: "failed",
      });
    }
    throw error;
  }
}

function recordSessionEventAppendAggregate(
  this: AgentRuntimeInternal,
  event: SessionEvent,
  traceContext: TraceContext,
): boolean {
  if (!SUMMARY_SESSION_EVENT_TYPES.has(event.type)) {
    return false;
  }

  const aggregateKey = `${traceContext.turnId ?? "session"}:${event.type}`;
  const aggregateMap = getSessionEventAppendAggregateMap(this);
  const payloadKind = getPayloadKind(event.payload);
  const existing = aggregateMap.get(aggregateKey);
  if (existing) {
    existing.eventCount += 1;
    existing.lastEventId = String(event.id);
    existing.lastSessionEventSequenceNumber = event.sequenceNumber;
    existing.payloadBytes += measureJsonBytes(event.payload);
    existing.payloadKinds[payloadKind] = (existing.payloadKinds[payloadKind] ?? 0) + 1;
    if (existing.eventCount >= SESSION_EVENT_APPEND_SUMMARY_FLUSH_COUNT) {
      flushSessionEventAppendAggregate.call(
        this,
        aggregateKey,
        existing,
        traceContext,
        "count_threshold",
      );
    }
    return true;
  }

  aggregateMap.set(aggregateKey, {
    eventCount: 1,
    eventType: event.type,
    firstEventId: String(event.id),
    firstSessionEventSequenceNumber: event.sequenceNumber,
    lastEventId: String(event.id),
    lastSessionEventSequenceNumber: event.sequenceNumber,
    payloadBytes: measureJsonBytes(event.payload),
    payloadKinds: { [payloadKind]: 1 },
  });
  return true;
}

function flushSessionEventAppendAggregates(
  this: AgentRuntimeInternal,
  traceContext: TraceContext,
  reason: "count_threshold" | "low_frequency_event",
): void {
  const aggregateMap = sessionEventAppendAggregates.get(this);
  if (!aggregateMap || aggregateMap.size === 0) {
    return;
  }
  for (const [aggregateKey, aggregate] of aggregateMap) {
    flushSessionEventAppendAggregate.call(this, aggregateKey, aggregate, traceContext, reason);
  }
}

function flushSessionEventAppendAggregate(
  this: AgentRuntimeInternal,
  aggregateKey: string,
  aggregate: SessionEventAppendAggregate,
  traceContext: TraceContext,
  reason: "count_threshold" | "low_frequency_event",
): void {
  const aggregateMap = sessionEventAppendAggregates.get(this);
  aggregateMap?.delete(aggregateKey);
//Reason for log management: model streaming/progress events are of the same frequency as the token stream.
// Writing the default log one by one will copy the eventStore index into a huge daily log; the seq range and kind distribution are reserved here for positioning.
  this.logger?.debug("Session event append summary", {
    ...traceContextToLogContext(traceContext),
    event: "event_store.appended.summary",
    eventCount: aggregate.eventCount,
    firstEventId: aggregate.firstEventId,
    firstSessionEventSequenceNumber: aggregate.firstSessionEventSequenceNumber,
    flushReason: reason,
    lastEventId: aggregate.lastEventId,
    lastSessionEventSequenceNumber: aggregate.lastSessionEventSequenceNumber,
    module: "core.runtime",
    payloadBytes: aggregate.payloadBytes,
    payloadKinds: aggregate.payloadKinds,
    sessionEventType: aggregate.eventType,
  });
}

function getSessionEventAppendAggregateMap(
  runtime: AgentRuntimeInternal,
): Map<string, SessionEventAppendAggregate> {
  let aggregateMap = sessionEventAppendAggregates.get(runtime);
  if (!aggregateMap) {
    aggregateMap = new Map();
    sessionEventAppendAggregates.set(runtime, aggregateMap);
  }
  return aggregateMap;
}

function getPayloadKind(payload: unknown): string {
  if (payload && typeof payload === "object" && !Array.isArray(payload)) {
    const kind = (payload as Record<string, unknown>).kind;
    if (typeof kind === "string" && kind.length > 0) {
      return kind;
    }
  }
  return "<missing>";
}

function measureJsonBytes(value: unknown): number {
  try {
    return Buffer.byteLength(JSON.stringify(value) ?? "null", "utf8");
  } catch {
    return 0;
  }
}

async function persistDurableSessionEvent(
  this: AgentRuntimeInternal,
  event: SessionEvent,
  traceContext: TraceContext,
): Promise<void> {
  if (!this.sessionStore) return;

  if (event.type === SessionEventType.CheckpointCreated) {
    await persistWorkspaceCheckpointEntry(this, event, traceContext);
    return;
  }

  if (event.type === SessionEventType.RewindTriggered) {
    await persistWorkspaceFileRewindEntry(this, event, traceContext);
    return;
  }

  if (event.type === SessionEventType.UserInputAutoResolutionUpdated) {
    const payload = event.payload as UserInputAutoResolutionUpdatedPayload;
    try {
      await this.sessionStore.saveSessionEntry?.({
        id: `user-input-auto-resolution:${payload.interactionId}`,
        sessionID: event.sessionId,
        type: SESSION_ENTRY_USER_INPUT_AUTO_RESOLUTION,
        time: {
          created: payload.autoResolution.startedAt,
          updated: event.timestamp.getTime(),
        },
        data: {
          interactionId: payload.interactionId,
          toolCallId: payload.toolCallId,
          autoResolution: payload.autoResolution,
          eventId: event.id,
          sequenceNumber: event.sequenceNumber,
          traceId: event.traceId,
          ...(event.turnId ? { turnId: event.turnId } : {}),
        },
      });
    } catch (error) {
// Reason: If the automatic end absolute time is only in the memory eventStore, the CLI restart will mistakenly reopen the five-minute window.
// session entry uses interactionId to stably overwrite the latest stage, and only reads the final state when restoring.
      this.logger?.warn("Failed to persist user input auto-resolution state", {
        ...traceContextToLogContext(traceContext),
        errorMessage: error instanceof Error ? error.message : String(error),
        event: "user_input_auto_resolution.persist_failed",
        interactionId: payload.interactionId,
        module: "core.runtime",
        status: "failed",
      });
    }
    return;
  }

// ── session_input ledger: queue/steer life cycle centralized accounting ──
// Handled in the event sink (not at each emission point): TurnSteerQueued/Discarded has 5+ emission points
// (steer/edit resend/single delete/clear/resume cleaning), single-point wiring ensures no leakage. promotion in
// drain is completed atomically at persistence (persistUserPrompt sessionInputId path), not through here.
  if (event.type === SessionEventType.TurnSteerQueued) {
    const payload = event.payload as {
      pendingInputId: string;
      input: string;
      commandKind?: string;
      delivery?: "guide" | "queue";
      intent?: TurnInputIntentMetadata;
    };
    const conversationInputIntent = buildPersistedConversationInputIntent(
      payload.input,
      payload.intent,
      "queued",
    );
    try {
      await this.sessionStore.saveSessionInput?.({
        id: payload.pendingInputId,
        sessionID: event.sessionId,
        kind: payload.intent?.kind ?? payload.commandKind ?? "sendText",
        delivery: payload.delivery ?? "queue",
        payload: {
          text: payload.input,
          ...(payload.intent ? { intent: payload.intent } : {}),
          ...(conversationInputIntent ? { conversationInputIntent } : {}),
        },
      });
    } catch (error) {
      this.logger?.warn("Failed to admit session input to ledger", {
        ...traceContextToLogContext(traceContext),
        errorMessage: error instanceof Error ? error.message : String(error),
        event: "session_input.admit_failed",
        module: "core.runtime",
        pendingInputId: payload.pendingInputId,
        status: "failed",
      });
    }
    return;
  }
  if (event.type === SessionEventType.TurnSteerDeliveryChanged) {
    const payload = event.payload as {
      admittedDelivery: "queue";
      intent?: TurnInputIntentMetadata;
      pendingInputId: string;
    };
    try {
      await this.sessionStore.updateSessionInputs?.({
        sessionID: event.sessionId,
        updates: [
          {
            delivery: payload.admittedDelivery,
            id: payload.pendingInputId,
            ...(payload.intent ? { intent: payload.intent } : {}),
          },
        ],
      });
    } catch (error) {
      this.logger?.warn("Failed to persist session input delivery fallback", {
        ...traceContextToLogContext(traceContext),
        errorMessage: error instanceof Error ? error.message : String(error),
        event: "session_input.delivery_change_failed",
        module: "core.runtime",
        pendingInputId: payload.pendingInputId,
        status: "failed",
      });
    }
    return;
  }
  if (event.type === SessionEventType.TurnSteerDiscarded) {
    const payload = event.payload as {
      pendingInputIds: string[];
      reason?: string;
    };
// sendQueuedNow only removes the item from the queue projection after the execution right has been reserved; at this time, if the ledger
// Marked as canceled, it will create a crash and loss window between remove→background user message promotion.
// Keep admitted, and then promoted atomically by persistUserPrompt; if the process exits first, cleaning will resume
// Will explicitly set it to discarded/session_resumed.
    if (payload.reason === "promoted") return;
// session_resumed=Restart without retaining the queue (ruling, leaving traces without silence); other user actions are cancelled.
    const status = payload.reason === "session_resumed" ? "discarded" : "cancelled";
    for (const pendingInputId of payload.pendingInputIds) {
      try {
        await this.sessionStore.settleSessionInput?.({
          id: pendingInputId,
          sessionID: event.sessionId,
          status,
          reason: payload.reason,
        });
      } catch (error) {
        this.logger?.warn("Failed to settle session input in ledger", {
          ...traceContextToLogContext(traceContext),
          errorMessage: error instanceof Error ? error.message : String(error),
          event: "session_input.settle_failed",
          module: "core.runtime",
          pendingInputId,
          status: "failed",
        });
      }
    }
    return;
  }

  if (event.type !== SessionEventType.TargetCompletionVerification) {
    return;
  }

  try {
    const timestamp = event.timestamp.getTime();
    const payload = event.payload as TargetCompletionVerificationPayload;
    const timelinePartId = targetCompletionVerificationTimelinePartId(payload);
    const existingTimeline = await readExistingTimelineTiming.call(this, {
      partID: timelinePartId,
      sessionID: event.sessionId,
    });
    const created = existingTimeline?.messageCreated ?? timestamp;
    const startedAt = existingTimeline?.partStarted ?? timestamp;
    if (this.sessionStore.saveSessionEntry) {
      await this.sessionStore.saveSessionEntry({
        id: String(event.id),
        sessionID: event.sessionId,
        type: SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
        time: {
          created: timestamp,
          updated: timestamp,
        },
// The goal verifier life cycle is a business fact that restores UI turns and dividing lines;
// Writing only memory eventStore will cause goal iteration/todo grouping to be lost after cold start.
        data: {
          eventId: event.id,
          payload: event.payload,
          sequenceNumber: event.sequenceNumber,
          traceId: event.traceId,
          ...(event.turnId ? { turnId: event.turnId } : {}),
        },
      });
    }
    await this.persistAssistantTimelinePartForSession({
      sessionId: event.sessionId,
      messageID: targetCompletionVerificationTimelineMessageId(payload),
      partID: timelinePartId,
      parentID: payload.anchorAssistantMessageId,
      created,
      completed: payload.status === "started" ? undefined : timestamp,
      finish: payload.status,
      timeline: {
        timelineType: "goal_verification",
        display: "separator",
        status: payload.status,
        anchorMessageId: payload.anchorAssistantMessageId,
        anchorTurnId: payload.anchorTurnId,
        targetId: payload.targetId,
        verificationId: payload.verificationId,
        goalIteration: payload.goalIteration,
        verification: payload.verification,
        time: {
          start: startedAt,
          end: payload.status === "started" ? undefined : timestamp,
        },
      },
      traceContext,
    });
  } catch (error) {
    this.logger?.warn("Failed to persist target completion verification event", {
      ...traceContextToLogContext(traceContext),
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "session_entry.target_completion_verification.persist_failed",
      module: "core.runtime",
      sessionEventType: event.type,
      status: "failed",
    });
  }
}

function targetCompletionVerificationTimelineMessageId(
  payload: TargetCompletionVerificationPayload,
): MessageId {
  return createMessageId(`goal_verify_${targetCompletionVerificationTimelineKey(payload)}`);
}

function targetCompletionVerificationTimelinePartId(
  payload: TargetCompletionVerificationPayload,
): PartId {
  return createPartId(`goal_verify_${targetCompletionVerificationTimelineKey(payload)}_timeline`);
}

function targetCompletionVerificationTimelineKey(
  payload: TargetCompletionVerificationPayload,
): string {
  return payload.goalIteration !== undefined
    ? `${payload.targetId}_${payload.goalIteration}`
    : payload.verificationId;
}

async function readExistingTimelineTiming(
  this: AgentRuntimeInternal,
  input: { partID: PartId; sessionID: SessionId },
): Promise<{ messageCreated?: number; partStarted?: number } | undefined> {
  const messages = await this.sessionStore?.messages({ sessionID: input.sessionID });
  if (!messages) return undefined;
  for (const message of messages) {
    const part = message.parts.find((candidate) => candidate.id === input.partID);
    if (part?.type !== "timeline") continue;
    return {
      messageCreated: message.info.time.created,
      partStarted: part.time?.start,
    };
  }
  return undefined;
}

export async function notifyEventSinks(
  this: AgentRuntimeInternal,
  event: SessionEvent,
  traceContext: TraceContext,
): Promise<void> {
  for (const sink of this.eventSinks) {
    try {
      await sink.onSessionEvent(event);
    } catch (error) {
      this.logger?.warn("Session event sink failed", {
        ...traceContextToLogContext(traceContext),
        errorMessage: error instanceof Error ? error.message : String(error),
        event: "session_event_sink.failed",
        module: "core.runtime",
        sessionEventType: event.type,
        status: "failed",
      });
    }
  }
}

/**
* Whether the session has entered the persistent store (any path of first input/external activity/direct startup wheel/cold recovery has fallen through the line).
* The session record of the protocol layer uses it as the source of fact for draft judgment (bootstrap `onSessionEvent` is aligned once for each event),
* No longer rely on each command handler to translate `record.persistence` independently.
 */
export function isSessionPersisted(this: AgentRuntimeInternal): boolean {
  return this.sessionPersisted;
}

export async function ensureSessionPersisted(
  this: AgentRuntimeInternal,
  input: string,
  traceContext: TraceContext,
): Promise<void> {
  if (!this.sessionStore || this.sessionPersisted) return;

  const startedAt = Date.now();
  let phase = "session_store.create";
  this.logger?.info("Session persistence started", {
    ...traceContextToLogContext(traceContext),
    event: "session.persistence.started",
    module: "core.runtime",
    sessionId: this.sessionId,
    status: "started",
  });

  try {
    const directory = this.workingDirectory;
// bootstrap will use path.resolve to normalize cwd execution; the same value was written in the past
// session.path/directory, causing the `/` at the end of the local workspacePath to be lost. Cold recovery followed by precise
// When the workspaceKey is checked in the provider registry, it will fall into another identity. Persistence must preserve the protocol entry path.
    const persistedWorkspacePath = this.config.workspacePath ?? directory;
    const title = titleFromInput(input);
    const workspaceIdentity = this.config.memory?.workspaceIdentity?.trim();
    await this.sessionStore.createSession({
      id: this.sessionId,
      projectID: projectIdFromDirectory(directory),
// Memory workspaceIdentity is an opaque isolation key provided by upstream. We only do type branding here,
// Cannot call the ID generator that will rewrite the string, otherwise the restored Memory root will drift.
      workspaceID: this.config.workspaceIdentity ?? (workspaceIdentity as WorkspaceId | undefined),
      parentID: this.config.parentSessionId,
      traceID: traceContext.traceId,
      taskType: this.config.taskType,
      slug: slugify(this.sessionId),
      directory: persistedWorkspacePath,
      path: persistedWorkspacePath,
      title,
      titleSource: "first_input",
      version: this.appVersion,
      permission: {
        mode: this.config.mode ?? "build",
      },
    });
// The initial model used to only write the first user message, but not the stable session selection.
// When cold recovery starts from the end of assistant, only provider/model can be obtained, and the required reasoning will be lost.
// Subagent therefore cannot recreate the Model before hydration. Fixed complete selection synchronized during session creation,
// Subsequent explicit die cutting still reuses the same stable entry coverage.
    phase = "session_model_selection";
    const initialSelection = this.getSessionModelSelection();
    if (initialSelection) await persistRuntimeModelSelection(this, initialSelection);
    phase = "session_shell_snapshot";
    await persistSessionShellEnvironmentSnapshot(this, traceContext);
    phase = "session_execution_state";
    await this.sessionStore.saveSessionEntry?.(
      buildExecutionStateEntry(this.sessionId, readRuntimeExecutionState(this)),
    );
    this.sessionPersisted = true;
    this.logger?.debug("Session persisted", {
      ...traceContextToLogContext(traceContext),
      event: "session.persisted",
      module: "core.runtime",
      status: "completed",
    });
// Previously, only first_input title was written into sessionStore but not appendEvent.
// As a result, the downstream (task index sqlite syncer of the z-code services layer) cannot wait for session.titleUpdated.
// The sidebar displays "New session" until the background LLM generates the title. Add a source="first_input" here
//Event, let the syncer on the three ends of desktop/web/mobile take the same convergence path.
    phase = "session_title_event";
    await this.appendEvent(
      this.createEvent(
        SessionEventType.SessionTitleUpdated,
        {
          previousTitle: "",
          source: "first_input",
          title,
        },
        traceContext,
      ),
      traceContext,
    );
    this.logger?.info("Session persistence completed", {
      ...traceContextToLogContext(traceContext),
      durationMs: Date.now() - startedAt,
      event: "session.persistence.completed",
      module: "core.runtime",
      sessionId: this.sessionId,
      status: "completed",
    });
  } catch (error) {
    this.logger?.warn("Session persistence failed", {
      ...traceContextToLogContext(traceContext),
      durationMs: Date.now() - startedAt,
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "session.persistence.failed",
      module: "core.runtime",
      phase,
      sessionId: this.sessionId,
      status: "failed",
    });
    throw error;
  }
}
