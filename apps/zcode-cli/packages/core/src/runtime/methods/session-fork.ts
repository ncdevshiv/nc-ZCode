/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { randomUUID } from "node:crypto";
import { resolveExecutionState, type ExecutionState } from "@zcode/shared";
import { buildExecutionStateEntry, readRuntimeExecutionState } from "../execution-state.js";
import {
  createModelId,
  createModelProviderId,
  type CreateSessionInput,
  type ForkCommitBundle,
} from "@zcode/contracts";
import { systemReminderRuntimeMetadata } from "../../agent/message-history.js";
import {
  CoreErrorType,
  RewindStrategy,
  SESSION_ENTRY_MODEL_SELECTION,
  SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
  SessionEventType,
  createCoreError,
  createMessageId,
  createPartId,
  createSessionId,
  createToolCallId,
  createTurnId,
  selectActiveConversationBranch,
  traceContextToLogContext,
} from "../deps.js";
import type {
  GoalStatus,
  MessageId,
  PartId,
  SessionEntryInfo,
  SessionGoal,
  TargetCompletionVerificationPayload,
  MessageWithParts,
  SessionId,
  SessionInfo,
  SessionStorePort,
  TraceContext,
  TurnId,
} from "../deps.js";
import {
  cloneMessageForFork,
  clonePartForFork,
  emptyTokenUsageInfo,
  formatConversationForkNoticeBody,
  slugify,
} from "../helpers/index.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { buildSyntheticUserNoticePartMetadata } from "./synthetic-notice-metadata.js";
import type {
  StableConversationForkChildMetadata,
  StableConversationForkGoalBoundary,
  StableConversationForkOptions,
  ConversationBeforeInputForkOptions,
  SelectionSideChatCreateOptions,
  StableConversationForkTarget,
  WorkspaceForkResult,
} from "../types.js";
import { cloneModelSelection } from "../model-selection.js";
import type { ModelSelection } from "@zcode/contracts";

function stableForkError(message: string, context: Record<string, unknown> = {}): Error {
  return createCoreError(CoreErrorType.InvalidStateTransition, message, {
    context,
    recoverable: true,
  });
}

const MODEL_SELECTION_ENTRY_SUFFIX = ":runtime-model-selection";

function modelSelectionFromMessage(message: MessageWithParts): ModelSelection | undefined {
  if (message.info.role === "user") {
    return message.info.modelSelection && cloneModelSelection(message.info.modelSelection);
  }
  if (!message.info.modelId || !message.info.providerId) return undefined;
  return {
    modelId: message.info.modelId,
    providerId: message.info.providerId,
    ...(message.info.reasoningLevel
      ? { options: { reasoningLevel: message.info.reasoningLevel } }
      : {}),
  };
}

function resolveForkModelSelection(
  runtime: AgentRuntimeInternal,
  messages: readonly MessageWithParts[],
  explicit?: ModelSelection,
): ModelSelection | undefined {
  if (explicit) return cloneModelSelection(explicit);
  const historical = [...messages].reverse().map(modelSelectionFromMessage).find(Boolean);
  const runtimeSelection = runtime.getSessionModelSelection();
  const identity = historical ?? runtimeSelection;
  if (!identity) return undefined;
  const historicalOptions = historical?.options;
  const reasoningLevel =
    historicalOptions?.reasoningLevel ?? runtimeSelection?.options?.reasoningLevel;
  return {
    modelId: identity.modelId,
    providerId: identity.providerId,
    ...(reasoningLevel !== undefined
      ? {
          options: {
            ...(reasoningLevel !== undefined ? { reasoningLevel } : {}),
          },
        }
      : {}),
  };
}

function buildModelSelectionEntry(
  childSessionId: SessionId,
  modelSelection: ModelSelection | undefined,
): SessionEntryInfo {
  const timestamp = Date.now();
  return {
    id: `${childSessionId}${MODEL_SELECTION_ENTRY_SUFFIX}`,
    sessionID: childSessionId,
    type: SESSION_ENTRY_MODEL_SELECTION,
    touchSession: false,
    time: { created: timestamp, updated: timestamp },
    data: modelSelection ? cloneModelSelection(modelSelection) : null,
  };
}

/** stable/compact-edit fork a one-time preallocated full child-local identity. */
interface ForkIdentityMap {
  parentSessionId: SessionId;
  childSessionId: SessionId;
  messageIds: Map<MessageId, MessageId>;
  partIds: Map<PartId, PartId>;
  turnIds: Map<string, string>;
  productTurnIds: Map<string, string>;
  targetIds: Map<string, string>;
  verifierEntryIds: Map<string, string>;
  verificationIds: Map<string, string>;
  toolCallIds: Map<string, string>;
  notice: {
    hiddenMessageId: MessageId;
    hiddenPartId: PartId;
    messageId: MessageId;
    partId: PartId;
    turnId: TurnId;
    productTurnId: string;
  };
}

function buildForkedSessionInput(
  runtime: AgentRuntimeInternal,
  parentSession: SessionInfo,
  forkedSessionId: SessionId,
  kind: "fork" | "selection_side_chat" = "fork",
): CreateSessionInput {
  const now = Date.now();
  return {
    id: forkedSessionId,
    projectID: parentSession.projectID,
    workspaceID: parentSession.workspaceID,
    parentID: runtime.sessionId,
    traceID: runtime.rootTraceContext.traceId,
    taskType: kind,
    slug: `${slugify(parentSession.slug)}-${kind}-${now.toString(36)}`.slice(0, 120),
    directory: parentSession.directory,
    path: parentSession.path,
    title:
      kind === "selection_side_chat" ? "Selection side chat" : `Fork of ${parentSession.title}`,
    titleSource: "generated",
    version: parentSession.version,
    permission: parentSession.permission,
    time: {
      created: now,
      updated: now,
    },
  };
}

function collectForkGoalSnapshots(
  messages: readonly MessageWithParts[],
  boundary: StableConversationForkGoalBoundary,
): SessionGoal[] {
  const byId = new Map<string, SessionGoal>();
  for (const message of messages) {
    const goalBoundary = message.info.anchor?.goalBoundary;
    if (goalBoundary?.kind === "snapshot") {
      byId.set(goalBoundary.target.targetID, goalBoundary.target);
    }
  }
  if (boundary.kind === "snapshot") byId.set(boundary.target.targetID, boundary.target);
  return [...byId.values()];
}

function collectVerifierEntryIds(
  messages: readonly MessageWithParts[],
  boundary: StableConversationForkGoalBoundary,
): Set<string> {
  const ids = new Set<string>();
  for (const message of messages) {
    const goalBoundary = message.info.anchor?.goalBoundary;
    if (goalBoundary?.kind === "snapshot") {
      for (const id of goalBoundary.verificationEntryIds) ids.add(id);
    }
  }
  if (boundary.kind === "snapshot") {
    for (const id of boundary.verificationEntryIds) ids.add(id);
  }
  return ids;
}

function createForkIdentityMap(options: {
  childSessionId: SessionId;
  entries: readonly SessionEntryInfo[];
  goalSnapshots: readonly SessionGoal[];
  messages: readonly MessageWithParts[];
  parentSessionId: SessionId;
}): ForkIdentityMap {
  const messageIds = new Map<MessageId, MessageId>();
  const partIds = new Map<PartId, PartId>();
  const turnIds = new Map<string, string>();
  const productTurnIds = new Map<string, string>();
  const targetIds = new Map<string, string>();
  const verifierEntryIds = new Map<string, string>();
  const verificationIds = new Map<string, string>();
  const toolCallIds = new Map<string, string>();
  const noticeHiddenMessageId = createMessageId();
  const notice = {
    hiddenMessageId: noticeHiddenMessageId,
    hiddenPartId: createPartId(),
    messageId: createMessageId(),
    partId: createPartId(),
    turnId: createTurnId(),
    productTurnId: String(noticeHiddenMessageId),
  };
  const addTurn = (id: unknown) => {
    if (typeof id === "string" && id && !turnIds.has(id)) {
      turnIds.set(id, String(createTurnId()));
    }
  };
  const addProductTurn = (id: unknown) => {
    if (typeof id !== "string" || !id || productTurnIds.has(id)) return;
    const messageId = messageIds.get(id as MessageId);
    productTurnIds.set(id, String(messageId ?? createTurnId()));
  };

  for (const message of options.messages) {
    messageIds.set(message.info.id, createMessageId());
    for (const part of message.parts) {
      partIds.set(part.id, createPartId());
      if (part.type === "tool") {
        toolCallIds.set(part.callID, String(createToolCallId()));
        if (part.state.status === "completed") {
          for (const attachment of part.state.attachments ?? []) {
            partIds.set(attachment.id, createPartId());
          }
        }
      }
    }
  }
  for (const message of options.messages) {
    addTurn(message.info.anchor?.turnId);
    addProductTurn(message.info.anchor?.productTurnId);
    for (const part of message.parts) {
      if (part.type === "timeline") {
        addTurn(part.anchorTurnId);
        if (part.timelineType === "goal_verification") {
          if (!targetIds.has(part.targetId)) {
            targetIds.set(part.targetId, `fork_target_${randomUUID()}`);
          }
          if (!verificationIds.has(part.verificationId)) {
            verificationIds.set(part.verificationId, `fork_verify_${randomUUID()}`);
          }
        }
      }
      if (part.type === "compaction") addTurn(part.compactBoundary?.turnId);
    }
  }
  for (const goal of options.goalSnapshots) {
    if (!targetIds.has(goal.targetID)) {
      targetIds.set(goal.targetID, `fork_target_${randomUUID()}`);
    }
  }
  for (const entry of options.entries) {
    verifierEntryIds.set(entry.id, `fork_goal_verify_${randomUUID()}`);
    const payload = asRecord(asRecord(entry.data).payload);
    if (
      typeof payload.verificationId === "string" &&
      !verificationIds.has(payload.verificationId)
    ) {
      verificationIds.set(payload.verificationId, `fork_verify_${randomUUID()}`);
    }
    addTurn(payload.anchorTurnId);
  }
  return {
    parentSessionId: options.parentSessionId,
    childSessionId: options.childSessionId,
    messageIds,
    partIds,
    turnIds,
    productTurnIds,
    targetIds,
    verifierEntryIds,
    verificationIds,
    toolCallIds,
    notice,
  };
}

function mapForkIdentity(map: ReadonlyMap<string, string>, id: string, field: string): string {
  const mapped = map.get(id);
  if (!mapped) throw stableForkError(`Stable fork cannot remap ${field}`, { id });
  return mapped;
}

function remapGoalForFork(goal: SessionGoal, identities: ForkIdentityMap): SessionGoal {
  return {
    ...goal,
    sessionID: identities.childSessionId,
    targetID: mapForkIdentity(identities.targetIds, goal.targetID, "goal target"),
    activeInputId: null,
    activeRunStartedAtMs: null,
    activeRunLastSeenAtMs: null,
  };
}

function cloneVerifierEntryForAtomicFork(
  entry: SessionEntryInfo,
  identities: ForkIdentityMap,
): { entry: SessionEntryInfo; payload: TargetCompletionVerificationPayload } {
  const data = asRecord(entry.data);
  const payload = asRecord(data.payload);
  if (typeof payload.targetId !== "string" || typeof payload.verificationId !== "string") {
    throw stableForkError("Stable fork verifier entry has invalid identity", { entryId: entry.id });
  }
  const clonedPayload: Record<string, unknown> = {
    ...payload,
    targetId: mapForkIdentity(identities.targetIds, payload.targetId, "verifier target"),
    verificationId: mapForkIdentity(
      identities.verificationIds,
      payload.verificationId,
      "verification id",
    ),
  };
  if (typeof payload.anchorAssistantMessageId === "string") {
    clonedPayload.anchorAssistantMessageId = mapForkIdentity(
      identities.messageIds,
      payload.anchorAssistantMessageId,
      "verifier assistant anchor",
    );
  }
  if (typeof payload.anchorTurnId === "string") {
    clonedPayload.anchorTurnId = mapForkIdentity(
      identities.turnIds,
      payload.anchorTurnId,
      "verifier turn anchor",
    );
  }
  const nextEntryId = mapForkIdentity(identities.verifierEntryIds, entry.id, "verifier entry");
  const nextPayload = clonedPayload as unknown as TargetCompletionVerificationPayload;
  return {
    entry: {
      ...entry,
      id: nextEntryId,
      sessionID: identities.childSessionId,
      data: {
        ...data,
        eventId: randomUUID(),
        payload: nextPayload,
        forkOrigin: {
          entryId: entry.id,
          eventId: data.eventId,
          verificationId: payload.verificationId,
        },
      },
    },
    payload: nextPayload,
  };
}

function buildAtomicForkNotice(
  runtime: AgentRuntimeInternal,
  options: {
    identities: ForkIdentityMap;
    modelSelection?: ModelSelection;
    executionState?: ExecutionState;
    sourceCommandId: string;
    targetMessageId: MessageId;
  },
): MessageWithParts[] {
  const created = Date.now();
  const {
    hiddenMessageId: hiddenId,
    hiddenPartId,
    messageId: noticeId,
    partId: noticePartId,
    turnId: noticeTurnId,
    productTurnId,
  } = options.identities.notice;
  const anchorMessageId = options.identities.messageIds.get(options.targetMessageId) ?? hiddenId;
  const anchor = {
    turnId: noticeTurnId,
    productTurnId,
    orderedMessageIds: [hiddenId, noticeId],
    boundaryMessageId: noticeId,
  };
  const forkOrigin = {
    parentSessionId: runtime.sessionId,
    targetMessageId: options.targetMessageId,
  };
  const runtimeSelection = runtime.getSessionModelSelection();
  const modelSelection = options.modelSelection ?? runtimeSelection;
  return [
    {
      info: {
        id: hiddenId,
        sessionID: options.identities.childSessionId,
        role: "user",
        time: { created },
        agent: runtime.config.agentName ?? "zcode-agent",
        modelSelection: modelSelection && cloneModelSelection(modelSelection),
        synthetic: true,
        source: "fork",
        visibility: "model-only",
        semantics: {
          origin: "system",
          kind: "fork_notice",
          uiVisibility: "hidden",
          providerVisibility: "visible",
          transcriptVisibility: "hidden",
        },
        anchor,
        metadata: { forkOrigin },
      },
      parts: [
        {
          id: hiddenPartId,
          sessionID: options.identities.childSessionId,
          messageID: hiddenId,
          type: "text",
          text: formatConversationForkNoticeBody(forkOrigin),
          synthetic: true,
          time: { start: created, end: created },
// hydrate read-only part metadata; independent source retains fork boundaries and does not change checkpoint's MCS behavior.
          metadata: buildSyntheticUserNoticePartMetadata("fork", "model-only", {
            forkOrigin,
            runtimeMessage: systemReminderRuntimeMetadata("conversation_fork"),
          }),
        },
      ],
    },
    {
      info: {
        id: noticeId,
        sessionID: options.identities.childSessionId,
        role: "assistant",
        time: { created, completed: created },
        parentID: hiddenId,
        modelId: modelSelection && createModelId(modelSelection.modelId),
        providerId: modelSelection && createModelProviderId(modelSelection.providerId),
        ...(modelSelection?.options?.reasoningLevel
          ? { reasoningLevel: modelSelection.options.reasoningLevel }
          : {}),
// The branch tip itself also belongs to the new branch history and must be consistent with the persistence state of the branch.
        ...(options.executionState ?? readRuntimeExecutionState(runtime)),
        agent: runtime.config.agentName ?? "zcode-agent",
        path: { cwd: runtime.workingDirectory, root: runtime.workspaceRoot },
        cost: 0,
        tokens: emptyTokenUsageInfo(),
        finish: "completed",
        semantics: {
          origin: "system",
          kind: "timeline_event",
          uiVisibility: "visible",
          providerVisibility: "hidden",
          transcriptVisibility: "visible",
        },
        anchor,
        metadata: { forkOrigin },
      },
      parts: [
        {
          id: noticePartId,
          sessionID: options.identities.childSessionId,
          messageID: noticeId,
          type: "timeline",
          timelineType: "session_fork",
          display: "separator",
          status: "completed",
          anchorMessageId,
          anchorTurnId: noticeTurnId,
          sourceCommandId: options.sourceCommandId,
          parentSessionId: runtime.sessionId,
          targetMessageId: options.targetMessageId,
          restoredFileCount: 0,
          time: { start: created, end: created },
        },
      ],
    },
  ];
}

const SELECTION_SIDE_CHAT_BOUNDARY = [
  "The preceding conversation was inherited from the parent task for reference only.",
  "Do not continue the parent's active work automatically; answer only new questions sent in this side chat.",
  "Modify the workspace only when the user explicitly asks you to do so in this side chat.",
].join(" ");

function buildSelectionSideChatBoundary(
  runtime: AgentRuntimeInternal,
  childSessionId: SessionId,
  modelSelection: ModelSelection | undefined,
): MessageWithParts {
  const created = Date.now();
  const messageId = createMessageId();
  const turnId = createTurnId();
  return {
    info: {
      id: messageId,
      sessionID: childSessionId,
      role: "user",
      time: { created },
      agent: runtime.config.agentName ?? "zcode-agent",
      modelSelection: modelSelection && cloneModelSelection(modelSelection),
      synthetic: true,
      source: "selection_side_chat",
      visibility: "model-only",
      semantics: {
        origin: "system",
        kind: "system_reminder",
        source: "selection_side_chat",
        uiVisibility: "hidden",
        providerVisibility: "visible",
        transcriptVisibility: "hidden",
      },
      anchor: {
        turnId,
        productTurnId: String(messageId),
        orderedMessageIds: [messageId],
        boundaryMessageId: messageId,
        origin: "synthetic",
      },
    },
    parts: [
      {
        id: createPartId(),
        sessionID: childSessionId,
        messageID: messageId,
        type: "text",
        text: SELECTION_SIDE_CHAT_BOUNDARY,
        synthetic: true,
        time: { start: created, end: created },
// hydrate reads part metadata; writing only info.source will degenerate into ordinary user text.
        metadata: buildSyntheticUserNoticePartMetadata(
          "selection_side_chat",
          "model-only",
          undefined,
        ),
      },
    ],
  };
}

function withoutSelectionSideChatGoalBoundary(message: MessageWithParts): MessageWithParts {
  const anchor = message.info.anchor;
  if (!anchor?.goalBoundary) return message;
  const anchorWithoutGoalBoundary = { ...anchor };
  delete anchorWithoutGoalBoundary.goalBoundary;
  return {
    ...message,
    info: {
      ...message.info,
      anchor: anchorWithoutGoalBoundary,
    },
  };
}

async function commitAtomicConversationFork(
  runtime: AgentRuntimeInternal,
  options: {
    commandFact?: ForkCommitBundle["commandFact"];
    modelSelection?: ModelSelection;
    forkedSessionId?: SessionId;
    goalBoundary: StableConversationForkGoalBoundary;
    initialInput?: ForkCommitBundle["initialInput"];
    messages: readonly MessageWithParts[];
    parentSession: SessionInfo;
    revisionAtDecision?: number;
    sourceCommandId: string;
    targetMessageId: MessageId;
    target?: StableConversationForkTarget;
    traceContext: TraceContext;
    kind?: "fork" | "selection_side_chat";
  },
): Promise<WorkspaceForkResult> {
  const store = runtime.sessionStore;
  if (!store?.commitForkBundle) {
    throw stableForkError("Stable fork requires commitForkBundle");
  }
  const childSessionId = options.forkedSessionId ?? createSessionId();
  const kind = options.kind ?? "fork";
  const currentExecutionState = readRuntimeExecutionState(runtime);
  const historicalInfo = [...options.messages]
    .reverse()
    .find((message) => message.info.role === "assistant")?.info;
// Keep the historical permission selection of the original stable fork, and do not allow new entries to overwrite it with the current permissions of the parent task.
  const executionState =
    kind === "selection_side_chat" || historicalInfo?.role !== "assistant"
      ? currentExecutionState
      : resolveExecutionState(historicalInfo);
// The auxiliary dialogue explicitly does not copy Goal target/verifier entries and cannot still
// The goalBoundary of the parent message is handed over to strict fork clone, otherwise any Goal state will require that it does not exist.
// child-local identity. Only remove the boundary for goal recovery, leaving the parent conversation body as model context.
  const sourceMessages =
    kind === "selection_side_chat"
      ? options.messages.map(withoutSelectionSideChatGoalBoundary)
      : options.messages;
  const modelSelection = resolveForkModelSelection(runtime, sourceMessages, options.modelSelection);
  const referencedEntryIds =
    kind === "selection_side_chat"
      ? new Set<string>()
      : collectVerifierEntryIds(sourceMessages, options.goalBoundary);
  const allEntries = referencedEntryIds.size
    ? await store.sessionEntries?.({
        sessionID: runtime.sessionId,
        type: SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
      })
    : [];
  if (referencedEntryIds.size && !allEntries) {
    throw stableForkError("Stable fork verifier boundary cannot be loaded");
  }
  const entryById = new Map((allEntries ?? []).map((entry) => [entry.id, entry]));
  const entries = [...referencedEntryIds].map((id) => {
    const entry = entryById.get(id);
    if (!entry) {
      throw stableForkError("Stable fork verifier boundary references missing entries", {
        verificationEntryId: id,
      });
    }
    return entry;
  });
  const goalSnapshots =
    kind === "selection_side_chat"
      ? []
      : collectForkGoalSnapshots(sourceMessages, options.goalBoundary);
  const identities = createForkIdentityMap({
    childSessionId,
    entries,
    goalSnapshots,
    messages: sourceMessages,
    parentSessionId: runtime.sessionId,
  });
  const copiedMessages = sourceMessages.map((message) => {
    const nextMessageId = identities.messageIds.get(message.info.id)!;
    const cloned = {
      info: cloneMessageForFork(message.info, {
        forkedSessionId: childSessionId,
        messageIdMap: identities.messageIds,
        nextMessageId,
        turnIdMap: identities.turnIds,
        productTurnIdMap: identities.productTurnIds,
        targetIdMap: identities.targetIds,
        verificationEntryIdMap: identities.verifierEntryIds,
        strictLocalReferences: true,
      }),
      parts: message.parts.map((part) =>
        clonePartForFork(part, {
          forkedSessionId: childSessionId,
          nextMessageId,
          nextPartId: identities.partIds.get(part.id),
          partIdMap: identities.partIds,
          messageIdMap: identities.messageIds,
          turnIdMap: identities.turnIds,
          targetIdMap: identities.targetIds,
          verificationIdMap: identities.verificationIds,
          toolCallIdMap: identities.toolCallIds,
          strictLocalReferences: true,
        }),
      ),
    };
    if (kind !== "selection_side_chat") return cloned;
// The inheritance history of the secondary screen is for model reference only; the UI starts from a blank secondary screen to avoid mistaking it for a normal fork.
    return {
      ...cloned,
      info: {
        ...cloned.info,
        visibility: "model-only" as const,
        semantics: {
          origin: cloned.info.semantics?.origin ?? "migration",
          kind: cloned.info.semantics?.kind ?? "system_reminder",
          ...(cloned.info.semantics?.source ? { source: cloned.info.semantics.source } : {}),
          uiVisibility: "hidden" as const,
          providerVisibility: "visible" as const,
          transcriptVisibility: "hidden" as const,
        },
      },
    };
  });
  const clonedEntries = entries.map((entry) => cloneVerifierEntryForAtomicFork(entry, identities));
  const modelSelectionEntry = buildModelSelectionEntry(childSessionId, modelSelection);
  if (kind === "selection_side_chat") {
    copiedMessages.push(buildSelectionSideChatBoundary(runtime, childSessionId, modelSelection));
  } else {
    copiedMessages.push(
      ...buildAtomicForkNotice(runtime, {
        identities,
        modelSelection,
        executionState,
        sourceCommandId: options.sourceCommandId,
        targetMessageId: options.targetMessageId,
      }),
    );
  }
  const commandFact = options.commandFact ?? {
    parentSessionId: String(runtime.sessionId),
    sourceCommandId: options.sourceCommandId,
    ack: {
      commandId: options.sourceCommandId,
      status: "accepted" as const,
      revisionAtDecision: options.revisionAtDecision ?? 0,
      result: {
        type: kind === "selection_side_chat" ? "createSelectionSideSession" : "forkAssistant",
        sessionId: String(childSessionId),
      },
    },
    metadata: {
      forkOrigin: {
        parentSessionId: String(runtime.sessionId),
        targetMessageId: String(options.targetMessageId),
      },
      ...(options.target ? { forkTarget: options.target } : {}),
    },
  };
  const goal =
    kind !== "selection_side_chat" && options.goalBoundary.kind === "snapshot"
      ? {
          source: remapGoalForFork(options.goalBoundary.target, identities),
          status: options.goalBoundary.target.status,
        }
      : undefined;
  const committedChild = await store.commitForkBundle({
    child: buildForkedSessionInput(runtime, options.parentSession, childSessionId, kind),
    messages: copiedMessages,
    copySources: {
      messages: Object.fromEntries(
        [...identities.messageIds].map(([source, target]) => [target, source]),
      ),
      parts: Object.fromEntries(
        [...identities.partIds].map(([source, target]) => [target, source]),
      ),
    },
//Select entry and child/message/verifier are submitted in the same transaction; otherwise, the child can be read when it is registered for the first time
// In running state, cold recovery will return to the workspace default thought. The disk packaging of the entry is handled by the adapter.
// Plan must go into the atomic child bundle along with the permissions, and cannot just copy the old permissions when created.
    entries: [
      ...clonedEntries.map((item) => item.entry),
      modelSelectionEntry,
      buildExecutionStateEntry(childSessionId, executionState),
    ],
    ...(goal ? { goal } : {}),
    ...(options.initialInput ? { initialInput: options.initialInput } : {}),
    commandFact,
  });
  const forkedSessionId = committedChild.id;
  if (kind !== "selection_side_chat") {
    const forkedEvent = runtime.createEvent(
      SessionEventType.SessionForked,
      {
        originalSessionId: runtime.sessionId,
        forkedSessionId,
        forkPoint: options.messages.length,
        targetMessageId: options.targetMessageId,
        restoredFileCount: 0,
        strategy: RewindStrategy.ForkRequired,
      },
      options.traceContext,
    );
    try {
      await runtime.appendEvent(forkedEvent, options.traceContext);
    } catch (error) {
      runtime.logger?.warn("Parent fork event append failed after durable fork commit", {
        ...traceContextToLogContext(options.traceContext),
        error: error instanceof Error ? error.message : String(error),
        event: "session.fork.parent_event.failed_after_commit",
        forkedSessionId,
        module: "core.runtime",
        parentSessionId: runtime.sessionId,
      });
    }
  }
  return {
    copiedMessageCount: options.messages.length,
    forkedSessionId,
    parentSessionId: runtime.sessionId,
    targetMessageId: options.targetMessageId,
    restoredFiles: [],
    response:
      kind === "selection_side_chat"
        ? `Created selection side chat ${forkedSessionId}.`
        : `Forked session ${forkedSessionId} from message ${options.targetMessageId}: copied ${options.messages.length} messages.`,
  };
}

/**
* The secondary screen is created using the stable drop boundary of the parent active transcript. Keep only submitted rounds while generating
* real-user input, excluding subsequent assistant/tool ​​increments; goal, queue and blocking running state are not copied.
 */
export async function createSelectionSideConversation(
  this: AgentRuntimeInternal,
  options: SelectionSideChatCreateOptions,
): Promise<WorkspaceForkResult> {
  if (!options.sourceCommandId.trim()) {
    throw stableForkError("Selection side chat sourceCommandId must not be empty");
  }
  if (!this.sessionStore?.commitForkBundle) {
    throw stableForkError("Selection side chat requires commitForkBundle");
  }
  const parentSession = await this.sessionStore.getSession(this.sessionId);
  if (!parentSession) throw stableForkError(`Session not found: ${this.sessionId}`);
  const parentMessages = await this.sessionStore.messages({ sessionID: this.sessionId });
  const activeMessages = forkSourceMessagesForSession(parentMessages, parentSession);
  const history = selectionSideChatHistoryMessages(activeMessages, this.activeTurn?.turnId);
  const targetMessageId = history.at(-1)?.info.id ?? createMessageId();
  return await commitAtomicConversationFork(this, {
    modelSelection: options.modelSelection,
    goalBoundary: { kind: "none" },
    kind: "selection_side_chat",
    messages: history,
    parentSession,
    revisionAtDecision: options.revisionAtDecision,
    sourceCommandId: options.sourceCommandId,
    targetMessageId,
    traceContext: options.traceContext ?? this.rootTraceContext,
  });
}

function selectionSideChatHistoryMessages(
  activeMessages: readonly MessageWithParts[],
  activeTurnId?: TurnId,
): MessageWithParts[] {
  if (!activeTurnId) return [...activeMessages];
  const activeUserIndex = activeMessages.findIndex(
    (message) =>
      message.info.role === "user" &&
      message.info.anchor?.turnId === activeTurnId &&
      message.info.anchor.origin === "realUser",
  );
  if (activeUserIndex >= 0) return activeMessages.slice(0, activeUserIndex + 1);
  const activeTurnStart = activeMessages.findIndex(
    (message) => message.info.anchor?.turnId === activeTurnId,
  );
  return activeTurnStart >= 0 ? activeMessages.slice(0, activeTurnStart) : [...activeMessages];
}

export async function createForkedSession(
  runtime: AgentRuntimeInternal,
  options: {
    parentSession: SessionInfo;
    forkedSessionId?: SessionId;
    stableForkMetadata?: StableConversationForkChildMetadata;
  },
): Promise<SessionId> {
  if (!runtime.sessionStore) {
    throw createCoreError(CoreErrorType.ConfigurationError, "Fork requires a session adapter.", {
      context: {
        hasSessionStore: false,
      },
      recoverable: true,
    });
  }

  const forkedSessionId = options.forkedSessionId ?? createSessionId();
  const input = buildForkedSessionInput(runtime, options.parentSession, forkedSessionId);
// legacy workspace fork compatible branch. The V4 stable/compact-edit entry directly builds the complete bundle.
// The child-only metadata primitive here must not be passed, otherwise the item-by-item complement window will be reintroduced.
  if (options.stableForkMetadata) {
    if (!runtime.sessionStore.createForkedSessionWithMetadata) {
      throw stableForkError("Stable fork requires atomic child metadata persistence", {
        forkedSessionId,
        sourceCommandId: options.stableForkMetadata.sourceCommandId,
      });
    }
    const persisted = await runtime.sessionStore.createForkedSessionWithMetadata(
      input,
      options.stableForkMetadata,
    );
    return persisted.id;
  } else {
    await runtime.sessionStore.createSession(input);
  }

  await runtime.sessionStore.saveSessionEntry?.(
    buildExecutionStateEntry(forkedSessionId, readRuntimeExecutionState(runtime)),
  );
  return forkedSessionId;
}

/** legacy workspace/checkpoint fork; calling is prohibited in V4 stable and compact-edit. */
export async function forkConversationFromMessage(
  this: AgentRuntimeInternal,
  options: {
    forkedSessionId?: SessionId;
    targetMessageId: MessageId;
    traceContext: TraceContext;
    beforeTarget?: true;
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

  const parentMessages = await this.sessionStore.messages({
    sessionID: this.sessionId,
  });
// Fork will occur after editing is retransmitted and compressed, and the copy source must be UI transcript semantics.
// activeSessionMessages is the model recovery semantics, and the old worklog will be cut off according to the compact boundary;
// The fork child needs to retain the visible history before the fork point, but still exclude the old branches after rewind/edit.
  const forkSourceMessages = forkSourceMessagesForSession(parentMessages, parentSession);
  const targetIndex = forkSourceMessages.findIndex(
    (message) => message.info.id === options.targetMessageId,
  );
  if (targetIndex < 0) {
    throw stableForkError(
      `Fork target message not found in session store: ${options.targetMessageId}`,
      { messageId: options.targetMessageId },
    );
  }

  const legacyForkHistoryEndIndex = resolveForkHistoryEndIndex(
    forkSourceMessages,
    targetIndex,
    true,
  );
  const forkHistoryMessages = options.beforeTarget
    ? conversationHistoryBeforeInput(forkSourceMessages, options.targetMessageId)
    : buildForkHistoryMessages(
        parentMessages,
        forkSourceMessages,
        targetIndex,
        legacyForkHistoryEndIndex,
      );

  const forkedSessionId = await createForkedSession(this, {
    forkedSessionId: options.forkedSessionId,
    parentSession,
  });
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
// Pure conversational fork does not have workspace checkpoint, but the UI still needs a structured fork notice rendering dividing line.
// Previously, only historical messages were copied, resulting in the source boundary not being visible on the first screen of the forked session.
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
      restoredFileCount: 0,
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
    text: formatConversationForkNoticeBody({
      parentSessionId: this.sessionId,
      targetMessageId: options.targetMessageId,
    }),
    metadata: {
      forkContext: {
        kind: "session_fork",
        parentSessionId: this.sessionId,
        targetMessageId: options.targetMessageId,
        restoredFileCount: 0,
      },
    },
    traceContext: options.traceContext,
  });
  this.logger?.debug("Conversation fork notice persisted", {
    ...traceContextToLogContext(options.traceContext),
    event: "session.fork.notice.persisted",
    forkedSessionId,
    module: "core.runtime",
    parentSessionId: this.sessionId,
    status: "completed",
    targetMessageId: options.targetMessageId,
  });

  const forkedEvent = this.createEvent(
    SessionEventType.SessionForked,
    {
      originalSessionId: this.sessionId,
      forkedSessionId,
      forkPoint: legacyForkHistoryEndIndex,
      targetMessageId: options.targetMessageId,
      restoredFileCount: 0,
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
    restoredFiles: [],
    response: `Forked session ${forkedSessionId} from message ${options.targetMessageId}: copied ${copiedMessageCount} messages.`,
  };
}

/** V4 running stable fork public entrance: pure transcript copy, does not read/restore workspace checkpoint. */
export async function forkStableConversationAtMessage(
  this: AgentRuntimeInternal,
  options: StableConversationForkOptions,
): Promise<WorkspaceForkResult> {
  if (!options.sourceCommandId.trim()) {
    throw stableForkError("Stable fork sourceCommandId must not be empty");
  }
  if (!this.sessionStore?.commitForkBundle) {
    throw stableForkError("Stable fork requires commitForkBundle");
  }
  const parentSession = await this.sessionStore.getSession(this.sessionId);
  if (!parentSession) throw stableForkError(`Session not found: ${this.sessionId}`);
  const parentMessages = await this.sessionStore.messages({ sessionID: this.sessionId });
  const activeMessages = forkSourceMessagesForSession(parentMessages, parentSession);
  const history = stableForkHistoryMessages(activeMessages, options.target);
  return await commitAtomicConversationFork(this, {
    modelSelection: options.modelSelection,
    forkedSessionId: options.forkedSessionId,
    goalBoundary: options.goalBoundary,
    messages: history,
    parentSession,
    revisionAtDecision: options.revisionAtDecision,
    sourceCommandId: options.sourceCommandId,
    target: options.target,
    targetMessageId: options.target.boundaryMessageId as MessageId,
    traceContext: options.traceContext ?? this.rootTraceContext,
  });
}

/** compact-covered edit: Copy the active conversation prefix before the target's real user input. */
export async function forkConversationBeforeMessage(
  this: AgentRuntimeInternal,
  options: ConversationBeforeInputForkOptions,
): Promise<WorkspaceForkResult> {
  if (!this.sessionStore?.commitForkBundle) {
    throw stableForkError("Fork requires a session adapter");
  }
  const parentSession = await this.sessionStore.getSession(this.sessionId);
  if (!parentSession) throw stableForkError(`Session not found: ${this.sessionId}`);
  const parentMessages = await this.sessionStore.messages({
    sessionID: this.sessionId,
  });
  const activeMessages = forkSourceMessagesForSession(parentMessages, parentSession);
  const prefix = conversationHistoryBeforeInput(activeMessages, options.targetMessageId);
  return await commitAtomicConversationFork(this, {
    commandFact: options.commandFact,
    modelSelection: options.modelSelection,
    forkedSessionId: options.forkedSessionId,
    goalBoundary: options.goalBoundary,
    initialInput: options.initialInput,
    messages: prefix,
    parentSession,
    sourceCommandId: options.sourceCommandId,
    targetMessageId: options.targetMessageId,
    traceContext: options.traceContext ?? this.rootTraceContext,
  });
}

function conversationHistoryBeforeInput(
  activeMessages: readonly MessageWithParts[],
  targetMessageId: MessageId,
): MessageWithParts[] {
  const targetIndex = activeMessages.findIndex((message) => message.info.id === targetMessageId);
  if (targetIndex < 0) {
    throw stableForkError(`Fork target input not found: ${targetMessageId}`, {
      targetMessageId,
    });
  }
  const target = activeMessages[targetIndex];
  if (target?.info.role !== "user") {
    throw stableForkError("Fork-before-input target is not a user message", {
      targetMessageId,
    });
  }
  return activeMessages.slice(0, targetIndex);
}

/**
* The stable resolver has given the only segment of the target product turn. core is reserved before the starting point of segment
* active transcript prefix, and requires ordered ids to be strictly continuous in the active branch; no longer
* parentID or "same assistant turn" expands after the boundary.
 */
function stableForkHistoryMessages(
  activeMessages: readonly MessageWithParts[],
  target: StableConversationForkTarget,
): MessageWithParts[] {
  if (
    target.orderedMessageIds.length === 0 ||
    target.orderedMessageIds.at(-1) !== target.boundaryMessageId
  ) {
    throw stableForkError("Stable fork target has an invalid boundary", {
      boundaryMessageId: target.boundaryMessageId,
    });
  }
  if (new Set(target.orderedMessageIds).size !== target.orderedMessageIds.length) {
    throw stableForkError("Stable fork target contains duplicate message ids");
  }

  const indexById = new Map(
    activeMessages.map((message, index) => [String(message.info.id), index]),
  );
  const segmentStartIndex = indexById.get(target.orderedMessageIds[0]!);
  if (segmentStartIndex === undefined) {
    throw stableForkError("Stable fork target is not an active transcript segment", {
      messageId: target.orderedMessageIds[0],
    });
  }
  for (const [offset, messageId] of target.orderedMessageIds.entries()) {
    const actual = activeMessages[segmentStartIndex + offset];
    if (String(actual?.info.id) !== messageId) {
      throw stableForkError("Stable fork target is not a contiguous active transcript segment", {
        messageId,
      });
    }
  }
  const selectedSegment = activeMessages.slice(
    segmentStartIndex,
    segmentStartIndex + target.orderedMessageIds.length,
  );
  const boundary = selectedSegment.at(-1);
  if (boundary?.info.role !== "assistant" || boundary.info.error) {
    throw stableForkError("Stable fork boundary is not a completed assistant message", {
      boundaryMessageId: target.boundaryMessageId,
    });
  }
  return [...activeMessages.slice(0, segmentStartIndex), ...selectedSegment];
}

export async function copyGoalStateForFork(
  this: AgentRuntimeInternal,
  options: {
    forkedSessionId: SessionId;
    goalBoundary?: StableConversationForkGoalBoundary;
    messageIdMap: Map<MessageId, MessageId>;
    traceContext: TraceContext;
  },
): Promise<void> {
  const sessionStore = this.sessionStore;
  if (!sessionStore?.cloneTargetForFork) {
    return;
  }

  if (options.goalBoundary?.kind === "none") {
    return;
  }

  const parentTarget =
    options.goalBoundary?.kind === "snapshot"
      ? options.goalBoundary.target
      : await sessionStore.readTarget({ sessionID: this.sessionId });
  if (!parentTarget) return;
  if (
    options.goalBoundary?.kind === "snapshot" &&
    String(parentTarget.sessionID) !== String(this.sessionId)
  ) {
    throw stableForkError("Stable fork goal snapshot belongs to another session", {
      goalSessionId: parentTarget.sessionID,
      parentSessionId: this.sessionId,
    });
  }

  const copiedVerificationPayloads = await copyGoalVerificationEntriesForFork(sessionStore, {
    forkedSessionId: options.forkedSessionId,
    messageIdMap: options.messageIdMap,
    parentSessionId: this.sessionId,
    parentTargetId: parentTarget.targetID,
    ...(options.goalBoundary?.kind === "snapshot"
      ? {
          verificationEntryIds: new Set(options.goalBoundary.verificationEntryIds),
        }
      : {}),
  });
  const forkedStatus =
    options.goalBoundary?.kind === "snapshot"
      ? parentTarget.status
      : deriveForkedGoalStatusFromCopiedVerifications(
          parentTarget.status,
          copiedVerificationPayloads,
        );
  await sessionStore.cloneTargetForFork({
    sessionID: options.forkedSessionId,
    source: parentTarget,
    status: forkedStatus,
  });

  this.logger?.debug("Forked session goal state copied", {
    ...traceContextToLogContext(options.traceContext),
    copiedGoalVerificationCount: copiedVerificationPayloads.length,
    event: "session.fork.goal_state.copied",
    forkedSessionId: options.forkedSessionId,
    module: "core.runtime",
    parentSessionId: this.sessionId,
    targetId: parentTarget.targetID,
  });
}

async function copyGoalVerificationEntriesForFork(
  sessionStore: SessionStorePort,
  options: {
    forkedSessionId: SessionId;
    messageIdMap: Map<MessageId, MessageId>;
    parentSessionId: SessionId;
    parentTargetId: string;
    verificationEntryIds?: ReadonlySet<string>;
  },
): Promise<TargetCompletionVerificationPayload[]> {
  if (!sessionStore.sessionEntries || !sessionStore.saveSessionEntry) {
    if (options.verificationEntryIds?.size) {
      throw stableForkError("Stable fork verifier boundary cannot be loaded", {
        verificationEntryIds: [...options.verificationEntryIds],
      });
    }
    return [];
  }

  const parentEntries = await sessionStore.sessionEntries({
    sessionID: options.parentSessionId,
    type: SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
  });
  const copiedPayloads: TargetCompletionVerificationPayload[] = [];
  const remainingEntryIds = options.verificationEntryIds
    ? new Set(options.verificationEntryIds)
    : null;
  for (const parentEntry of parentEntries) {
    if (options.verificationEntryIds && !options.verificationEntryIds.has(parentEntry.id)) {
      continue;
    }
    remainingEntryIds?.delete(parentEntry.id);
    const cloned = cloneGoalVerificationEntryForFork(parentEntry, options);
    if (!cloned) {
      if (options.verificationEntryIds) {
        throw stableForkError("Stable fork verifier is outside the fixed transcript cut", {
          verificationEntryId: parentEntry.id,
        });
      }
      continue;
    }
    await sessionStore.saveSessionEntry(cloned.entry);
    copiedPayloads.push(cloned.payload);
  }
  if (remainingEntryIds?.size) {
    throw stableForkError("Stable fork verifier boundary references missing entries", {
      verificationEntryIds: [...remainingEntryIds],
    });
  }
  return copiedPayloads;
}

function cloneGoalVerificationEntryForFork(
  entry: SessionEntryInfo,
  options: {
    forkedSessionId: SessionId;
    messageIdMap: Map<MessageId, MessageId>;
    parentTargetId: string;
  },
): { entry: SessionEntryInfo; payload: TargetCompletionVerificationPayload } | null {
  const data = asRecord(entry.data);
  const payload = asRecord(data.payload);
  if (payload.targetId !== options.parentTargetId) {
    return null;
  }
  const anchorAssistantMessageId =
    typeof payload.anchorAssistantMessageId === "string"
      ? (payload.anchorAssistantMessageId as MessageId)
      : null;
// anchor is present but not messageIdMap = verified assistant after fork point (not copied):
// This is fork history boundary filtering, correctly skipped - the child only inherits the verifier timeline before the fork point.
  if (anchorAssistantMessageId && !options.messageIdMap.has(anchorAssistantMessageId)) {
    return null;
  }
  const childAnchorAssistantMessageId = anchorAssistantMessageId
    ? options.messageIdMap.get(anchorAssistantMessageId)
    : undefined;

// When legacy entry has no anchor, the entire entry will no longer be silently skipped——verifier
// The fact is still copied (the boundary cannot be judged by anchor, and would rather be kept for traceability), the local anchor defaults to the reading end
// Process as "No anchor, drop to known end". anchorTurnId points to the parent runtime turn (child does not exist
// This round), always downgrade originAnchorTurnId.
  const clonedPayloadRecord: Record<string, unknown> = { ...payload };
  if (childAnchorAssistantMessageId) {
    clonedPayloadRecord.anchorAssistantMessageId = childAnchorAssistantMessageId;
  }
  if (typeof payload.anchorTurnId === "string") {
    delete clonedPayloadRecord.anchorTurnId;
    clonedPayloadRecord.originAnchorTurnId = payload.anchorTurnId;
  }
  const clonedPayload = clonedPayloadRecord as unknown as TargetCompletionVerificationPayload;
  const eventId = randomUUID();
  return {
    entry: {
      ...entry,
      id: `fork_goal_verify_${eventId}`,
      sessionID: options.forkedSessionId,
// verifier entry is the persistent boundary of goal iteration; it must be copied to after fork
// child session, and rewrite the anchor assistant as child message id to avoid losing the dividing line when the UI is restored.
      data: {
        ...data,
        eventId,
        payload: clonedPayload,
      },
    },
    payload: clonedPayload,
  };
}

function deriveForkedGoalStatusFromCopiedVerifications(
  parentStatus: GoalStatus,
  copiedVerificationPayloads: readonly TargetCompletionVerificationPayload[],
): GoalStatus {
  const latestCompleted = [...copiedVerificationPayloads]
    .reverse()
    .find((payload) => payload.status === "completed" && payload.verification);
  if (latestCompleted?.verification?.passed === true) {
    return "complete";
  }
  if (parentStatus === "complete") {
    return "active";
  }
  return parentStatus;
}

export function forkSourceMessagesForSession(
  parentMessages: MessageWithParts[],
  parentSession: SessionInfo,
): MessageWithParts[] {
  return activeForkTranscriptMessages(parentMessages, {
    branchCutAfterMessageId: parentSession.revert?.branchCutAfterMessageID,
    rewindCreatedMessageId: parentSession.revert?.createdMessageID,
    rewindKeptMessageIds: parentSession.revert?.keptMessageIDs,
    rewindTargetMessageId: parentSession.revert?.targetMessageID,
  });
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function activeForkTranscriptMessages(
  messages: MessageWithParts[],
  options: {
    branchCutAfterMessageId?: MessageId;
    rewindCreatedMessageId?: MessageId;
    rewindKeptMessageIds?: readonly MessageId[];
    rewindTargetMessageId?: MessageId;
  } = {},
): MessageWithParts[] {
// fork retains the complete visible transcript (without compact provider scope cropping), but rewind
// branch and runtime resume / cold projection must use the same pure selector.
  return selectActiveConversationBranch(messages, options);
}

export function resolveForkHistoryEndIndex(
  messages: MessageWithParts[],
  targetIndex: number,
  expandAssistantTurn: boolean,
): number {
  const target = messages[targetIndex];
  if (!expandAssistantTurn || target?.info.role !== "assistant") {
    return targetIndex + 1;
  }

  const parentId = target.info.parentID;
  let endIndex = targetIndex + 1;
  for (let index = targetIndex + 1; index < messages.length; index++) {
    const message = messages[index]!;
    if (message.info.role !== "assistant" || message.info.parentID !== parentId) {
      break;
    }
    endIndex = index + 1;
  }
  return endIndex;
}

function isActiveCompactionBoundaryMessage(message: MessageWithParts): boolean {
  return message.parts.some(
    (part) => part.type === "compaction" && (Boolean(part.compactBoundary) || !part.timelineStatus),
  );
}

function isRealVisibleUserMessage(message: MessageWithParts): boolean {
  return (
    message.info.role === "user" &&
    message.info.synthetic !== true &&
    message.info.visibility !== "model-only" &&
    !message.info.source &&
    !message.info.summary &&
    !isActiveCompactionBoundaryMessage(message)
  );
}

function findCompactedForkParentUserMessage(
  parentMessages: MessageWithParts[],
  forkHistoryMessages: MessageWithParts[],
  target: MessageWithParts | undefined,
): MessageWithParts | undefined {
  if (target?.info.role !== "assistant") {
    return undefined;
  }
  const parentMessageId = target.info.parentID;
  if (
    !parentMessageId ||
    forkHistoryMessages.some((message) => message.info.id === parentMessageId)
  ) {
    return undefined;
  }
  if (!forkHistoryMessages.some(isActiveCompactionBoundaryMessage)) {
    return undefined;
  }

  const parentUserMessage = parentMessages.find((message) => message.info.id === parentMessageId);
  return parentUserMessage && isRealVisibleUserMessage(parentUserMessage)
    ? parentUserMessage
    : undefined;
}

export function buildForkHistoryMessages(
  parentMessages: MessageWithParts[],
  forkSourceMessages: MessageWithParts[],
  targetIndex: number,
  forkHistoryEndIndex: number,
): MessageWithParts[] {
  const forkHistoryMessages = forkSourceMessages.slice(0, forkHistoryEndIndex);
  const compactedParentUserMessage = findCompactedForkParentUserMessage(
    parentMessages,
    forkHistoryMessages,
    forkSourceMessages[targetIndex],
  );
  if (!compactedParentUserMessage) {
    return forkHistoryMessages;
  }

// After compaction, only summary user + assistant is left in the active branch, and the summary will be filtered by the UI.
// When forking to the assistant, the real user input pointed to by its parentID must still be put back in front of the compact boundary.
// In this way, the historical visible bubbles are not lost, and resume still starts from the last compact boundary without changing the model context.
  return [compactedParentUserMessage, ...forkHistoryMessages];
}
