import {
  CoreErrorType,
  RewindStrategy,
  SessionEventType,
  createCoreError,
  createMessageId,
  createPartId,
  parseWorkspaceCheckpointArtifact,
} from "../deps.js";
import type {
  MessageId,
  SessionId,
  TraceContext,
  WorkspaceCheckpointArtifact,
} from "../deps.js";
import {
  selectCheckpointsForMessages,
  formatWorkspaceForkAtMessageNoticeBody,
  throwIfTurnAborted,
} from "../helpers/index.js";
import {
  buildForkHistoryMessages,
  copyGoalStateForFork,
  createForkedSession,
  forkConversationFromMessage,
  forkSourceMessagesForSession,
  resolveForkHistoryEndIndex,
} from "./session-fork.js";
import type { WorkspaceRewindRestoredFile, WorkspaceForkResult } from "../types.js";
import type { AgentRuntimeInternal } from "../internal.js";

export async function restoreWorkspaceCheckpointFiles(
  runtime: AgentRuntimeInternal,
  files: WorkspaceCheckpointArtifact["files"],
  traceContext: TraceContext,
  abortSignal?: AbortSignal,
): Promise<WorkspaceRewindRestoredFile[]> {
  if (!runtime.fileSystemPort) {
    throw createCoreError(CoreErrorType.ConfigurationError, "FileSystemPort is not configured", {
      recoverable: true,
    });
  }

  const restoredFiles: WorkspaceRewindRestoredFile[] = [];
  for (const file of files) {
    throwIfTurnAborted(abortSignal);
    if (!file.existedBefore || file.beforeContent === null) {
      await runtime.fileSystemPort.removeFile(
        {
          path: file.path,
          missingOk: true,
          trace: traceContext,
        },
        { signal: abortSignal },
      );
      restoredFiles.push({
        action: "delete",
        path: file.path,
      });
      continue;
    }

    const write = await runtime.fileSystemPort.writeTextFile(
      {
        path: file.path,
        content: file.beforeContent,
        createParents: true,
        atomic: true,
        trace: traceContext,
      },
      { signal: abortSignal },
    );
    restoredFiles.push({
      action: "restore",
      bytesWritten: write.bytesWritten,
      path: file.path,
    });
  }

  return restoredFiles;
}

export async function forkWorkspaceAtMessage(
  this: AgentRuntimeInternal,
  options: {
    abortSignal?: AbortSignal;
    forkedSessionId?: SessionId;
    targetMessageId: MessageId;
    traceContext: TraceContext;
  },
): Promise<WorkspaceForkResult> {
  if (!this.sessionStore) {
    throw createCoreError(CoreErrorType.ConfigurationError, "Fork requires a session adapter.", {
      context: {
        hasSessionStore: false,
      },
      recoverable: true,
    });
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

  const parentMessages = await this.sessionStore.messages({ sessionID: this.sessionId });
  const forkSourceMessages = forkSourceMessagesForSession(parentMessages, parentSession);
  const targetIndex = forkSourceMessages.findIndex(
    (message) => message.info.id === options.targetMessageId,
  );
  if (targetIndex < 0) {
    throw createCoreError(
      CoreErrorType.InvalidStateTransition,
      `Fork target message not found in session store: ${options.targetMessageId}`,
      {
        context: {
          messageId: options.targetMessageId,
        },
        recoverable: true,
      },
    );
  }

  const forkHistoryEndIndex = resolveForkHistoryEndIndex(forkSourceMessages, targetIndex, true);
  const sessionEvents = await this.eventStore.getEvents(this.sessionId);
// Only the checkpoint after the fork point needs to be revoked. The checkpoint of the target round itself belongs to the copied history,
// Its products must be retained; a checkpoint may be hung on the assistant / tool message at the same time, and any hit
//Historical prefixes are processed as "before the fork point" to avoid cross-border turns being accidentally rolled back.
  const historyMessageIds = forkSourceMessages
    .slice(0, forkHistoryEndIndex)
    .map((message) => message.info.id);
  const historyCheckpointIds = new Set(
    selectCheckpointsForMessages(sessionEvents, historyMessageIds).map(
      (checkpoint) => checkpoint.checkpointId,
    ),
  );
  const laterMessageIds = forkSourceMessages
    .slice(forkHistoryEndIndex)
    .map((message) => message.info.id);
  const laterCheckpoints = selectCheckpointsForMessages(sessionEvents, laterMessageIds).filter(
    (checkpoint) => !historyCheckpointIds.has(checkpoint.checkpointId),
  );

  if (laterCheckpoints.length === 0) {
// There are no file changes after the fork point, and the workspace is already in the fork point state: equivalent to a pure conversational fork, without touching any files.
    return await forkConversationFromMessage.call(this, {
      forkedSessionId: options.forkedSessionId,
      targetMessageId: options.targetMessageId,
      traceContext: options.traceContext,
    });
  }

  if (!this.artifactStore || !this.fileSystemPort) {
    throw createCoreError(
      CoreErrorType.ConfigurationError,
      "Fork requires session, artifact, and file-system adapters.",
      {
        context: {
          hasArtifactStore: Boolean(this.artifactStore),
          hasFileSystemPort: Boolean(this.fileSystemPort),
          hasSessionStore: true,
        },
        recoverable: true,
      },
    );
  }

// Read all snapshots first before causing any side effects: any artifact missing will cause the fork to fail as a whole.
// Prevent the file from stopping halfway through writing in an intermediate state that is neither a fork point nor the current state.
  const artifacts: WorkspaceCheckpointArtifact[] = [];
  for (const checkpoint of laterCheckpoints) {
    throwIfTurnAborted(options.abortSignal);
    const read = await this.artifactStore.readToolResultArtifact(
      {
        uri: checkpoint.snapshotRef,
        trace: options.traceContext,
      },
      { signal: options.abortSignal },
    );
    artifacts.push(parseWorkspaceCheckpointArtifact(JSON.parse(read.content)));
  }

//The status of the file at the fork point = the before status of the first change record after the fork point;
// When the same file is changed multiple times, only the earliest one will be applied, and all subsequent ones will be overwritten by it.
  const earliestFileByPath = new Map<string, WorkspaceCheckpointArtifact["files"][number]>();
  for (const artifact of artifacts) {
    for (const file of artifact.files) {
      if (!earliestFileByPath.has(file.path)) {
        earliestFileByPath.set(file.path, file);
      }
    }
  }

  const forkedSessionId = await createForkedSession(this, {
    forkedSessionId: options.forkedSessionId,
    parentSession,
  });
  const forkHistoryMessages = buildForkHistoryMessages(
    parentMessages,
    forkSourceMessages,
    targetIndex,
    forkHistoryEndIndex,
  );
  const { copiedMessageCount, messageIdMap } = await this.copySessionMessagesForFork({
    forkedSessionId,
    messages: forkHistoryMessages,
    traceContext: options.traceContext,
  });
  await copyGoalStateForFork.call(this, {
    forkedSessionId,
    messageIdMap,
    traceContext: options.traceContext,
  });
  const restoredFiles = await restoreWorkspaceCheckpointFiles(
    this,
    Array.from(earliestFileByPath.values()),
    options.traceContext,
    options.abortSignal,
  );
// Unify the three paths: forkWorkspaceAtMessage cannot only write synthetic notice, but not write
// session_fork timeline part (inconsistent with the other two fork paths, cold recovery fork boundary shape drifts).
  const copiedTargetMessageId = messageIdMap.get(options.targetMessageId);
  const forkTimelineCreated = Date.now();
  await this.persistAssistantTimelinePartForSession({
    sessionId: forkedSessionId,
    messageID: createMessageId(),
    partID: createPartId(
      `fork_${String(this.sessionId)}_${String(options.targetMessageId)}_timeline`,
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
      targetMessageId: options.targetMessageId,
      restoredFileCount: restoredFiles.length,
      time: {
        start: forkTimelineCreated,
        end: forkTimelineCreated,
      },
    },
    traceContext: options.traceContext,
  });
  await this.persistSyntheticUserNoticeForSession({
    messageID: createMessageId(),
    sessionId: forkedSessionId,
    source: "fork",
    text: formatWorkspaceForkAtMessageNoticeBody({
      parentSessionId: this.sessionId,
      restoredFiles,
      targetMessageId: options.targetMessageId,
      undoneCheckpointCount: laterCheckpoints.length,
    }),
    metadata: {
      forkContext: {
        kind: "session_fork",
        parentSessionId: this.sessionId,
        targetMessageId: options.targetMessageId,
        restoredFileCount: restoredFiles.length,
      },
    },
    traceContext: options.traceContext,
  });

  const forkedEvent = this.createEvent(
    SessionEventType.SessionForked,
    {
      originalSessionId: this.sessionId,
      forkedSessionId,
      forkPoint: forkHistoryEndIndex,
      targetMessageId: options.targetMessageId,
      restoredFileCount: restoredFiles.length,
      strategy: RewindStrategy.ForkRequired,
    },
    options.traceContext,
  );
  await this.appendEvent(forkedEvent, options.traceContext);

  return {
    copiedMessageCount,
    forkedSessionId,
    parentSessionId: this.sessionId,
    targetMessageId: options.targetMessageId,
    restoredFiles,
    response: `Forked session ${forkedSessionId} from message ${options.targetMessageId}: copied ${copiedMessageCount} messages and restored ${restoredFiles.length} file${restoredFiles.length === 1 ? "" : "s"} to the fork point.`,
  };
}
