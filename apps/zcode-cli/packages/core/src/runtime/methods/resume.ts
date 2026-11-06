/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { restorePermissionGrantMarker } from "../helpers/permission-grant-resume.js";
import { executionStateSchema, resolveExecutionState } from "@zcode/shared";
import { SESSION_ENTRY_EXECUTION_STATE } from "@zcode/contracts";
import {
  CoreErrorType,
  HookEventName,
  SessionEventType,
  createCoreError,
  traceContextToLogContext,
  formatGoalStateForModel,
  activeSessionMessages,
  hydrateReadFileStateFromSession,
  hydrateMessageHistoryFromSession,
  MessageHistoryImpl,
} from "../deps.js";
import type {
  EnvInfo,
  MessageWithParts,
  SessionEvent,
  SessionGoal,
  SessionInfo,
  SessionTitleSource,
  TodoItem,
  TraceContext,
  TurnId,
  TurnState,
  ToolSchedule,
} from "../deps.js";
import { getLatestActiveSessionMessageId } from "../helpers/index.js";
import type { ResumeSessionOptions, ResumeSessionResult } from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";
import {
  announceSessionShellEnvironmentNoticeAfterResume,
  getSessionShellSelection,
  restoreSessionShellEnvironmentSelectionForResume,
} from "./session-shell-environment.js";
import { repairPersistedRemoteSessionPaths } from "../helpers/persisted-remote-session-path-repair.js";
import {
  restoreWorkspaceCheckpointEntries,
  restoreWorkspaceFileRewindEntries,
} from "./workspace-checkpoint-persistence.js";
import { mainTurnCacheHitAggregateFromMessages } from "./turn-model-step-usage.js";

export function toScheduleState(
  this: AgentRuntimeInternal,
  schedule: ToolSchedule,
): TurnState["scheduledTools"] {
  return {
    items: schedule.items.map((item) => ({
      toolCallId: item.toolCallId,
      dependencies: item.dependencies,
      canRunParallel: item.canRunParallel,
    })),
    parallelGroups: schedule.parallelGroups,
    executionOrder: schedule.executionOrder,
  };
}

export async function resumeFromStore(
  this: AgentRuntimeInternal,
  options?: ResumeSessionOptions,
): Promise<ResumeSessionResult> {
  if (!this.sessionStore) {
    throw createCoreError(
      CoreErrorType.ConfigurationError,
      "Cannot resume session without a session store",
      { recoverable: false },
    );
  }

  const traceContext = options?.traceContext ?? this.rootTraceContext;
  const persistedSession = await this.sessionStore.getSession(this.sessionId);
  if (!persistedSession || persistedSession.time.archived !== undefined) {
    throw createCoreError(CoreErrorType.SessionNotFound, `Session not found: ${this.sessionId}`, {
      context: { sessionId: this.sessionId },
      recoverable: true,
    });
  }
  const session = await repairPersistedRemoteSessionPaths(this.sessionStore, persistedSession, {
    onPersistenceFailure: (error) => {
      this.logger?.warn("Session path repair persistence failed; using in-memory repair", {
        ...traceContextToLogContext(traceContext),
        errorMessage: error instanceof Error ? error.message : String(error),
        event: "session.path_repair.persist_failed",
        module: "core.runtime",
        sessionId: this.sessionId,
      });
    },
  });

  const messages =
    options?.persistedMessages ??
    (await this.sessionStore.messages({
      sessionID: this.sessionId,
    }));
  const rewindTargetMessageId = session.revert?.targetMessageID;
  const rewindCreatedMessageId = session.revert?.createdMessageID;
  const rewindKeptMessageIds = session.revert?.keptMessageIDs;
  const branchCutAfterMessageId = session.revert?.branchCutAfterMessageID;
  this.branchGeneration = session.revert?.branchGeneration ?? 0;
  this.runtimeTaskRegistry.setActiveBranchGeneration?.(this.branchGeneration);
  if (messages.length === 0) {
    this.logger?.warn("Session resume loaded zero persisted messages", {
      ...traceContextToLogContext(traceContext),
      directory: session.directory,
      event: "session.resume.persisted_messages_zero",
      module: "core.runtime",
      rewindCreatedMessageId,
      rewindKeptMessageCount: rewindKeptMessageIds?.length ?? 0,
      rewindTargetMessageId,
      sessionId: this.sessionId,
    });
  }
  const persistedEnvInfo = extractPersistedEnvInfo(messages);
  if (persistedEnvInfo) {
    this.config.envInfo = persistedEnvInfo;
  }
  const shellRestore = await restoreSessionShellEnvironmentSelectionForResume(this, {
    currentSelection: getSessionShellSelection(this),
    traceContext,
  });

  this.workingDirectory = session.directory;
  this.config.taskType = session.taskType;
  if (this.config.memory) {
// Memory root must use the workspace identity when the session is dropped, and cannot use the process to start the workspace.
    this.config.memory.workspaceIdentity = session.workspaceID
      ? String(session.workspaceID)
      : undefined;
  }
  this.messageHistory = new MessageHistoryImpl();
  this.contextBuilder = null;
  this.contextInitialized = false;
  this.lastEmittedLocalDate = undefined;
// The historical hydration of cold resume will first clear the runtime-local read-state; it must be in
// Completed before Context initialization, ensuring that the state of MEMORY.md subsequently loaded in a single time is consistent with what the provider sees.
  const readFileStateHydration = await hydrateReadFileStateFromSession({
    branchCutAfterMessageId,
    messages,
    readFileState: this.readFileState,
    rewindCreatedMessageId,
    rewindKeptMessageIds,
    rewindTargetMessageId,
    workingDirectory: this.workingDirectory,
    workspaceRoot: this.workspaceRoot,
  });
  await this.ensureContextInitialized(traceContext);
  const recoveredCompactTimelineCount = await this.recoverInterruptedCompactTimelines(
    messages,
    traceContext,
  );
  const hydration = await hydrateMessageHistoryFromSession({
    artifactStore: this.artifactStore,
    branchCutAfterMessageId,
    history: this.messageHistory,
    messages,
    rewindCreatedMessageId,
    rewindKeptMessageIds,
    rewindTargetMessageId,
  });
  announceSessionShellEnvironmentNoticeAfterResume(this, {
    persistedEnvInfo,
    restore: shellRestore,
  });
  const activeMessages = activeSessionMessages(messages, {
    branchCutAfterMessageId,
    rewindCreatedMessageId,
    rewindKeptMessageIds,
    rewindTargetMessageId,
  });
// The compact preserved segment will insert the pre-compact message back into the provider context.
// But it is not the latest anchor of the timeline after compaction and cannot be used for subsequent compaction parentID.
  const timelineActiveMessages = activeSessionMessages(messages, {
    branchCutAfterMessageId,
    includeCompactPreservedSegment: false,
    rewindCreatedMessageId,
    rewindKeptMessageIds,
    rewindTargetMessageId,
  });
  const latestAssistant = [...timelineActiveMessages]
    .reverse()
    .find((message) => message.info.role === "assistant");
  this.latestConversationMessageId = getLatestActiveSessionMessageId(timelineActiveMessages);
  this.latestAssistantMessageId = latestAssistant?.info.id;
  this.latestAssistantTurnId = latestAssistant?.info.anchor?.turnId as TurnId | undefined;
  this.lastAssistantCompletedAtMs =
    latestAssistant && "completed" in latestAssistant.info.time
      ? latestAssistant.info.time.completed
      : undefined;

  await restoreWorkspaceCheckpointEntries(this, traceContext);
  await restoreWorkspaceFileRewindEntries(this, traceContext);
  const restoredEvents = await this.eventStore.getEvents(this.sessionId);
  const restoredModeEvents = restoredEvents.filter(
    (event) =>
      event.type === SessionEventType.SessionCreated ||
      event.type === SessionEventType.SessionModeChanged,
  );
  const restoredMode =
    restoredModeEvents.length > 0 ? this.eventReducer.reduce(restoredModeEvents).mode : undefined;
  const resolvedMode = options?.modeOverride ?? restoredMode ?? session.permission?.mode;
  if (resolvedMode !== undefined) {
// cold resume will first restore local events such as checkpoint/rewind to the new memory eventStore.
// These events do not carry mode. If you just press "any event exists" reduce, the default build will overwrite headless yolo.
// Only authoritative mode events can restore historical values; the explicit/default mode of this invocation still maintains the highest priority.
    Object.assign(this.config, resolveExecutionState({ mode: resolvedMode }));
  }
// The session's own new records take precedence over project preferences; old records are only compatible with reading, not batch backfilling.
  const executionEntries = await this.sessionStore.sessionEntries?.({
    sessionID: this.sessionId,
    type: SESSION_ENTRY_EXECUTION_STATE,
  });
  const savedExecution = executionStateSchema.safeParse(executionEntries?.at(-1)?.data);
  if (savedExecution.success && options?.modeOverride === undefined) {
    Object.assign(this.config, savedExecution.data);
  }

  await restorePermissionGrantMarker(this, traceContext);

  this.mainTurnCacheHitAggregate = mainTurnCacheHitAggregateFromMessages({
    activeMessages,
    persistedMessages: messages,
  });
  this.turnNumber = activeMessages.filter(
    (message) => message.info.role === "user" && !message.info.summary,
  ).length;
  this.sessionPersisted = true;
  await syncPersistedSessionTitleForResume.call(this, {
    restoredEvents,
    session,
    traceContext,
  });
  await this.discardPersistedPendingSteerInputs(traceContext);
  const recoveredSteerInputCount = 0;
  const resumedTodos = await this.readSessionTodosForContext(traceContext);
  const resumedTarget = await this.readSessionTargetForContext(traceContext);
  this.injectTargetStateIntoMessageHistory(resumedTarget);

  const resumedEvent = this.createEvent(
    SessionEventType.SessionResumed,
    {
      directory: session.directory,
      interruptedToolCount: hydration.interruptedToolCount,
      messageCount: hydration.messageCount,
      partCount: hydration.partCount,
      recoveredCompactTimelineCount,
      recoveredSteerInputCount,
      resumedTodoCount: resumedTodos.length,
      resumedTarget: resumedTarget?.status,
    },
    traceContext,
  );
  await this.appendEvent(resumedEvent, traceContext);
  const sessionStartHookResult = await this.runSessionStartHooks(
    "resume",
    traceContext,
    options?.abortSignal,
  );
  this.injectHookAdditionalContextIntoMessageHistory(
    HookEventName.SessionStart,
    sessionStartHookResult.additionalContexts,
  );

  if (messages.length > 0 && activeMessages.length === 0) {
    this.logger?.warn("Session resume produced zero active messages", {
      ...traceContextToLogContext(traceContext),
      activeMessageCount: activeMessages.length,
      appliedMessageCount: hydration.appliedMessageCount,
      directory: session.directory,
      event: "session.resume.active_messages_zero",
      hydrationMessageCount: hydration.messageCount,
      module: "core.runtime",
      persistedMessageCount: messages.length,
      recoveredCompactTimelineCount,
      rewindCreatedMessageId,
      rewindKeptMessageCount: rewindKeptMessageIds?.length ?? 0,
      sessionId: this.sessionId,
      rewindTargetMessageId,
    });
  }
  if (activeMessages.length > 0 && hydration.appliedMessageCount === 0) {
    this.logger?.warn("Session resume applied zero history messages", {
      ...traceContextToLogContext(traceContext),
      activeMessageCount: activeMessages.length,
      directory: session.directory,
      event: "session.resume.applied_messages_zero",
      hydrationMessageCount: hydration.messageCount,
      module: "core.runtime",
      persistedMessageCount: messages.length,
      recoveredCompactTimelineCount,
      rewindCreatedMessageId,
      rewindKeptMessageCount: rewindKeptMessageIds?.length ?? 0,
      rewindTargetMessageId,
      sessionId: this.sessionId,
    });
  }

  this.logger?.info("Session resumed", {
    ...traceContextToLogContext(traceContext),
    appliedMessageCount: hydration.appliedMessageCount,
    directory: session.directory,
    event: "session.resumed",
    interruptedToolCount: hydration.interruptedToolCount,
    messageCount: hydration.messageCount,
    module: "core.runtime",
    partCount: hydration.partCount,
    readFileStateRestoredCount: readFileStateHydration.restoredCount,
    readFileStateSkippedRangeReadCount: readFileStateHydration.skippedRangeReadCount,
    readFileStateSkippedUnreadableEditCount: readFileStateHydration.skippedUnreadableEditCount,
    recoveredCompactTimelineCount,
    resumedTodoCount: resumedTodos.length,
    resumedTargetStatus: resumedTarget?.status,
    sessionId: this.sessionId,
    status: "completed",
  });

  return {
    ...hydration,
    directory: session.directory,
// Interrupting compact recovery will write back the timeline part; bootstrap cannot continue to restore the previous
// Messages are handed over to V4, otherwise the first frame will briefly revive the started/retrying state.
    persistedMessagesReloadRequired: recoveredCompactTimelineCount > 0,
    readFileStateRestoredCount: readFileStateHydration.restoredCount,
    readFileStateSkippedRangeReadCount: readFileStateHydration.skippedRangeReadCount,
    readFileStateSkippedUnreadableEditCount: readFileStateHydration.skippedUnreadableEditCount,
    traceId: traceContext.traceId,
  };
}

async function syncPersistedSessionTitleForResume(
  this: AgentRuntimeInternal,
  input: {
    restoredEvents: SessionEvent[];
    session: SessionInfo;
    traceContext: TraceContext;
  },
): Promise<void> {
  const title = input.session.title.trim();
  if (!title) return;
  const source = input.session.titleSource ?? "generated";
  if (hasRestoredTitleEvent(input.restoredEvents, title, source)) return;

// When the fork child is created, the title has been written into the sessionStore, but the copy history will not copy the parent session's
//SessionTitleUpdated event. The v4 live projection only consumes the event stream. If this event is missing, the list title will be downgraded to "New Task".
  await this.appendEvent(
    this.createEvent(
      SessionEventType.SessionTitleUpdated,
      {
        previousTitle: "",
        source,
        title,
      },
      input.traceContext,
    ),
    input.traceContext,
  );
}

function hasRestoredTitleEvent(
  events: readonly SessionEvent[],
  title: string,
  source: SessionTitleSource,
): boolean {
  return events.some((event) => {
    if (event.type !== SessionEventType.SessionTitleUpdated) return false;
    const payload = event.payload as { source?: unknown; title?: unknown };
    return payload.title === title && payload.source === source;
  });
}

function extractPersistedEnvInfo(messages: MessageWithParts[]): EnvInfo | undefined {
  for (const message of messages) {
    if (message.info.role !== "user") {
      continue;
    }

    const envInfo = message.info.contextSnapshot?.envInfo;
    if (envInfo) {
      return envInfo;
    }
  }

  return undefined;
}

export async function readSessionTodosForContext(
  this: AgentRuntimeInternal,
  traceContext: TraceContext,
): Promise<TodoItem[]> {
  if (!this.sessionStore) {
    return [];
  }

  try {
    return await this.sessionStore.readTodos({ sessionID: this.sessionId });
  } catch (error) {
    // Todo state is continuity context. If the store cannot read it, resume/compact can still
    // proceed from transcript history while surfacing the degradation in structured logs.
    this.logger?.warn("Failed to read session todos for context", {
      ...traceContextToLogContext(traceContext),
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "todo.context.read.failed",
      module: "core.runtime",
      status: "failed",
    });
    return [];
  }
}

export async function readSessionTargetForContext(
  this: AgentRuntimeInternal,
  traceContext: TraceContext,
): Promise<SessionGoal | null> {
  if (!this.sessionStore) {
    return null;
  }

  try {
    return await this.sessionStore.readTarget({ sessionID: this.sessionId });
  } catch (error) {
    // Goal state is continuity context. Resume should still work from transcript history
    // if goal storage is temporarily unavailable.
    this.logger?.warn("Failed to read session goal for context", {
      ...traceContextToLogContext(traceContext),
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "target.context.read.failed",
      module: "core.runtime",
      status: "failed",
    });
    return null;
  }
}

export function injectTargetStateIntoMessageHistory(
  this: AgentRuntimeInternal,
  target: SessionGoal | null,
): void {
  const targetState = formatGoalStateForModel(target);
  if (!targetState) {
    return;
  }

  this.messageHistory.addAttachment(
    "resume_goal_state",
    [
      "The current session goal state was restored from session storage.",
      targetState,
      "Use it as the authoritative long-running objective unless a later GoalRead result or runtime goal event updates it.",
      "Do not mark the goal complete unless real evidence shows the objective has been achieved.",
      "A completed plan, todo list, checklist, or planning phase is not completion evidence unless the objective was only to produce that artifact.",
    ].join("\n"),
  );
}
