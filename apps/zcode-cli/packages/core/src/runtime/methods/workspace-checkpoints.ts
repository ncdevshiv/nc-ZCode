import {
  CoreErrorType,
  RewindScope,
  RewindStrategy,
  SessionEventType,
  createCoreError,
  createMessageId,
  createPartId,
  getCurrentTraceContext,
  parseCheckpointCreatedPayload,
  parseWorkspaceCheckpointArtifact,
} from "../deps.js";
import type {
  MessageId,
  MessageWithParts,
  SessionId,
  TraceContext,
  WorkspaceCheckpointArtifact,
} from "../deps.js";
import {
  selectCheckpointForRewind,
  previewTextFromMessage,
  formatWorkspaceForkNoticeBody,
  cloneMessageForFork,
  clonePartForFork,
} from "../helpers/index.js";
import {
  buildForkHistoryMessages,
  copyGoalStateForFork,
  createForkedSession,
  forkSourceMessagesForSession,
  resolveForkHistoryEndIndex,
} from "./session-fork.js";
import { forkWorkspaceAtMessage, restoreWorkspaceCheckpointFiles } from "./workspace-fork.js";
import type {
  WorkspaceRewindRestoredFile,
  WorkspaceForkResult,
  WorkspaceCheckpointSummary,
} from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";

export async function restoreWorkspaceCheckpointArtifact(
  this: AgentRuntimeInternal,
  artifact: WorkspaceCheckpointArtifact,
  traceContext: TraceContext,
  abortSignal?: AbortSignal,
): Promise<WorkspaceRewindRestoredFile[]> {
  return await restoreWorkspaceCheckpointFiles(this, artifact.files, traceContext, abortSignal);
}

export async function copySessionMessagesForFork(
  this: AgentRuntimeInternal,
  options: {
    forkedSessionId: SessionId;
    messages: MessageWithParts[];
    traceContext: TraceContext;
  },
): Promise<{
  copiedMessageCount: number;
  messageIdMap: Map<MessageId, MessageId>;
}> {
  if (!this.sessionStore) {
    return { copiedMessageCount: 0, messageIdMap: new Map() };
  }

  const messageIdMap = new Map<MessageId, MessageId>();
  let copied = 0;
  for (const message of options.messages) {
    const nextMessageId = createMessageId();
    messageIdMap.set(message.info.id, nextMessageId);
    await this.persistMessage(
      cloneMessageForFork(message.info, {
        forkedSessionId: options.forkedSessionId,
        messageIdMap,
        nextMessageId,
      }),
      options.traceContext,
      { sessionID: message.info.sessionID, id: message.info.id },
    );

    for (const part of message.parts) {
      await this.persistPart(
        clonePartForFork(part, {
          forkedSessionId: options.forkedSessionId,
          nextMessageId,
          messageIdMap,
        }),
        options.traceContext,
        { sessionID: part.sessionID, id: part.id },
      );
    }
    copied += 1;
  }

  return { copiedMessageCount: copied, messageIdMap };
}

export async function listWorkspaceCheckpoints(
  this: AgentRuntimeInternal,
  options: { limit?: number } = {},
): Promise<WorkspaceCheckpointSummary[]> {
  const sessionEvents = await this.eventStore.getEvents(this.sessionId);
  const previews = await this.loadCheckpointMessagePreviews();
  const summaries = sessionEvents
    .filter((event) => event.type === SessionEventType.CheckpointCreated)
    .map((event) => {
      const checkpoint = parseCheckpointCreatedPayload(event.payload);
      return { checkpoint, timestamp: event.timestamp };
    })
    .filter(
      ({ checkpoint }) =>
        checkpoint.scope === RewindScope.Workspace || checkpoint.scope === RewindScope.Both,
    )
    .map(({ checkpoint, timestamp }) => ({
      checkpointId: checkpoint.checkpointId,
      compactBoundaryId: checkpoint.compactBoundaryId,
      coveredByCompact: checkpoint.coveredByCompact,
      createdAt: timestamp,
      diffRef: checkpoint.diffRef,
      fileCount: checkpoint.fileCount,
      messageId: checkpoint.messageId,
      targetMessageId: checkpoint.targetMessageId,
      toolMessageId: checkpoint.toolMessageId,
      preview: previews.get(checkpoint.targetMessageId ?? checkpoint.messageId),
      scope: checkpoint.scope,
      snapshotRef: checkpoint.snapshotRef,
    }))
    .reverse();

  return options.limit && options.limit > 0 ? summaries.slice(0, options.limit) : summaries;
}

export async function forkWorkspaceFromCheckpoint(
  this: AgentRuntimeInternal,
  options: {
    abortSignal?: AbortSignal;
    forkedSessionId?: SessionId;
    targetCheckpointId?: string;
    targetMessageId?: MessageId;
    traceContext?: TraceContext;
  } = {},
): Promise<WorkspaceForkResult> {
  const traceContext = options.traceContext ?? getCurrentTraceContext() ?? this.rootTraceContext;

// The fork semantics of the message target are "The history contains the target turn, and the workspace stops at the fork point moment".
// Restoring the beforeContent of the target round's own checkpoint will roll back/delete the file just produced in that round——
// fork shares the workspace directory with the parent session, and the parent session's artifacts are lost (most noticeable when forking on the latest reply).
// message fork only revokes checkpoints after the fork point; explicit checkpoint targets (/fork latest,
// targetCheckpointId) still retains the rewind-style semantics of "going back to before the checkpoint was modified".
  if (options.targetMessageId && !options.targetCheckpointId) {
    return await forkWorkspaceAtMessage.call(this, {
      abortSignal: options.abortSignal,
      forkedSessionId: options.forkedSessionId,
      targetMessageId: options.targetMessageId,
      traceContext,
    });
  }

  const sessionEvents = await this.eventStore.getEvents(this.sessionId);
  const checkpoint = selectCheckpointForRewind(sessionEvents, options.targetCheckpointId);

  if (!checkpoint) {
    throw createCoreError(
      CoreErrorType.InvalidStateTransition,
      options.targetCheckpointId
        ? `Checkpoint not found: ${options.targetCheckpointId}`
        : "No workspace checkpoint is available yet.",
      {
        context: {
          targetCheckpointId: options.targetCheckpointId,
          targetMessageId: options.targetMessageId,
        },
        recoverable: true,
      },
    );
  }

  if (!this.sessionStore || !this.artifactStore || !this.fileSystemPort) {
    throw createCoreError(
      CoreErrorType.ConfigurationError,
      "Fork requires session, artifact, and file-system adapters.",
      {
        context: {
          hasArtifactStore: Boolean(this.artifactStore),
          hasFileSystemPort: Boolean(this.fileSystemPort),
          hasSessionStore: Boolean(this.sessionStore),
        },
        recoverable: true,
      },
    );
  }

  const parentSession = await this.sessionStore.getSession(this.sessionId);
  if (!parentSession) {
    throw createCoreError(CoreErrorType.SessionNotFound, `Session not found: ${this.sessionId}`, {
      context: {
        sessionId: this.sessionId,
      },
      recoverable: true,
    });
  }

  const read = await this.artifactStore.readToolResultArtifact(
    {
      uri: checkpoint.snapshotRef,
      trace: traceContext,
    },
    { signal: options.abortSignal },
  );
  const artifact = parseWorkspaceCheckpointArtifact(JSON.parse(read.content));
  const parentMessages = await this.sessionStore.messages({ sessionID: this.sessionId });
// After editing and resending, the session store will retain the old branch that was rewinded.
// The fork copy history must be based on the active branch, otherwise the child session will bring back the old prompt and canceled assistant.
  const forkSourceMessages = forkSourceMessagesForSession(parentMessages, parentSession);
  const targetMessageIdResolved = checkpoint.targetMessageId ?? checkpoint.messageId;
  const targetIndex = forkSourceMessages.findIndex(
    (message) => message.info.id === targetMessageIdResolved,
  );
  if (targetIndex < 0) {
    throw createCoreError(
      CoreErrorType.InvalidStateTransition,
      `Checkpoint message not found in session store: ${targetMessageIdResolved}`,
      {
        context: {
          checkpointId: checkpoint.checkpointId,
          messageId: targetMessageIdResolved,
        },
        recoverable: true,
      },
    );
  }

  const forkedSessionId = await createForkedSession(this, {
    forkedSessionId: options.forkedSessionId,
    parentSession,
  });

  const forkHistoryEndIndex = resolveForkHistoryEndIndex(forkSourceMessages, targetIndex, false);
  const forkHistoryMessages = buildForkHistoryMessages(
    parentMessages,
    forkSourceMessages,
    targetIndex,
    forkHistoryEndIndex,
  );
  const { copiedMessageCount, messageIdMap } = await this.copySessionMessagesForFork({
    forkedSessionId,
    messages: forkHistoryMessages,
    traceContext,
  });
  await copyGoalStateForFork.call(this, {
    forkedSessionId,
    messageIdMap,
    traceContext,
  });
  const restoredFiles = await this.restoreWorkspaceCheckpointArtifact(
    artifact,
    traceContext,
    options.abortSignal,
  );
  const copiedTargetMessageId = messageIdMap.get(targetMessageIdResolved);
  const forkTimelineCreated = Date.now();
  await this.persistAssistantTimelinePartForSession({
    sessionId: forkedSessionId,
    messageID: createMessageId(),
    partID: createPartId(
      `fork_${String(this.sessionId)}_${String(targetMessageIdResolved)}_${checkpoint.checkpointId}_timeline`,
    ),
    parentID: copiedTargetMessageId,
    created: forkTimelineCreated,
    completed: forkTimelineCreated,
    finish: "completed",
    timeline: {
      timelineType: "session_fork",
      display: "separator",
      status: "completed",
      anchorMessageId: copiedTargetMessageId,
      parentSessionId: this.sessionId,
      targetMessageId: targetMessageIdResolved,
      targetCheckpointId: checkpoint.checkpointId,
      restoredFileCount: restoredFiles.length,
      time: {
        start: forkTimelineCreated,
        end: forkTimelineCreated,
      },
    },
    traceContext,
  });
  await this.persistSyntheticUserNoticeForSession({
    messageID: createMessageId(),
    sessionId: forkedSessionId,
    source: "fork",
    text: formatWorkspaceForkNoticeBody({
      checkpoint,
      parentSessionId: this.sessionId,
      restoredFiles,
    }),
    metadata: {
      forkContext: {
        kind: "session_fork",
        parentSessionId: this.sessionId,
        targetMessageId: targetMessageIdResolved,
        targetCheckpointId: checkpoint.checkpointId,
        restoredFileCount: restoredFiles.length,
      },
    },
    traceContext,
  });

  const forkedEvent = this.createEvent(
    SessionEventType.SessionForked,
    {
      originalSessionId: this.sessionId,
      forkedSessionId,
      forkPoint: forkHistoryEndIndex,
      targetMessageId: targetMessageIdResolved,
      targetCheckpointId: checkpoint.checkpointId,
      restoredSnapshotRef: checkpoint.snapshotRef,
      restoredFileCount: restoredFiles.length,
      strategy: RewindStrategy.ForkRequired,
    },
    traceContext,
  );
  await this.appendEvent(forkedEvent, traceContext);

  const response = `Forked session ${forkedSessionId} from checkpoint ${checkpoint.checkpointId}: copied ${copiedMessageCount} messages and restored ${restoredFiles.length} file${restoredFiles.length === 1 ? "" : "s"}.`;
  return {
    checkpoint,
    copiedMessageCount,
    forkedSessionId,
    parentSessionId: this.sessionId,
    targetMessageId: targetMessageIdResolved,
    targetCheckpointId: checkpoint.checkpointId,
    restoredFiles,
    response,
  };
}

export async function loadCheckpointMessagePreviews(
  this: AgentRuntimeInternal,
): Promise<Map<MessageId, string>> {
  if (!this.sessionStore) return new Map();

  const messages = await this.sessionStore.messages({ sessionID: this.sessionId });
  const messagesById = new Map(messages.map((message) => [message.info.id, message]));
  const previews = new Map<MessageId, string>();

  for (const message of messages) {
    const source =
      message.info.role === "assistant" ? messagesById.get(message.info.parentID) : message;
    const preview = source ? previewTextFromMessage(source) : undefined;
    if (preview) {
      previews.set(message.info.id, preview);
    }
  }

  return previews;
}
