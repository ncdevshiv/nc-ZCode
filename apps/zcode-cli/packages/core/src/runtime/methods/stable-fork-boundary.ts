import { SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION } from "../deps.js";
import type {
  MessageId,
  MessageProjectionAnchor,
  SessionEntryInfo,
  SessionGoal,
  TraceContext,
} from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";

interface VerificationEntryPayload {
  anchorAssistantMessageId?: string;
  targetId?: string;
}

/**
* Pin the historical facts required for the fork to the final assistant before TurnComplete becomes visible to subscribers.
* productTurnId belongs to the bootstrap projection domain, and only the core authoritative raw message segment is fixed here;
* The resolver will use the projection candidate to complement the productTurnId, prohibiting the core from copying the `~qN` algorithm.
 */
export async function persistStableForkCompletionBoundary(
  runtime: AgentRuntimeInternal,
  input: {
    boundaryMessageId: MessageId;
    startMessageId: MessageId;
    historyRoundCount: number;
    traceContext: TraceContext;
  },
): Promise<void> {
  const store = runtime.sessionStore;
  if (!store) return;

  const messages = await store.messages({ sessionID: runtime.sessionId });
  const boundaryIndex = messages.findLastIndex(
    (message) =>
      message.info.id === input.boundaryMessageId &&
      message.info.role === "assistant" &&
      !message.info.error &&
      message.info.time.completed !== undefined,
  );
  const startIndex = messages.findLastIndex(
    (message, index) =>
      index <= boundaryIndex && message.info.id === input.startMessageId,
  );
  const boundary = messages[boundaryIndex];
  if (
    startIndex < 0 ||
    boundaryIndex < startIndex ||
    boundary?.info.role !== "assistant" ||
    boundary.info.error ||
    boundary.info.time.completed === undefined
  ) {
// Compatible with the old adapter/test double that only implements part of SessionStorePort, and the turn is closed before
// edit/rewind rewritten transcript. If there is no exact segment, no pseudo anchor or resolver will be written.
// Still based on legacy unambiguous rules.
    return;
  }

  const orderedMessageIds = messages
    .slice(startIndex, boundaryIndex + 1)
    .map((message) => message.info.id);
  const prefixMessageIds = new Set(
    messages
      .slice(0, boundaryIndex + 1)
      .map((message) => String(message.info.id)),
  );
  const target =
    typeof store.readTarget === "function"
      ? await store.readTarget({ sessionID: runtime.sessionId })
      : null;
  const goalBoundary = target
    ? {
        kind: "snapshot" as const,
        target: stableGoalSnapshot(target),
        verificationEntryIds: await verificationEntryIdsAtBoundary(
          runtime,
          target,
          prefixMessageIds,
        ),
      }
    : { kind: "none" as const };

  const anchor: MessageProjectionAnchor = {
    ...boundary.info.anchor,
    ...(input.traceContext.turnId ? { turnId: input.traceContext.turnId } : {}),
    historyRoundCount: input.historyRoundCount,
    orderedMessageIds,
    boundaryMessageId: input.boundaryMessageId,
    goalBoundary,
  };
  await store.saveMessage({ ...boundary.info, anchor });
}

function stableGoalSnapshot(target: SessionGoal): SessionGoal {
//The active run field belongs to the parent's instant execution right and is not a state that the child can inherit. production adapter
// cloneTargetForFork will also be cleared; anchor will also be cleared first to avoid misreading query/diagnosis as historical running.
  return {
    ...target,
    activeInputId: null,
    activeRunStartedAtMs: null,
    activeRunLastSeenAtMs: null,
    time: { ...target.time },
  };
}

async function verificationEntryIdsAtBoundary(
  runtime: AgentRuntimeInternal,
  target: SessionGoal,
  prefixMessageIds: ReadonlySet<string>,
): Promise<string[]> {
  const store = runtime.sessionStore;
  if (!store?.sessionEntries) {
// When there is a goal but cannot read the verifier ledger, you cannot forge an empty boundary, otherwise the child may be lost silently
// verifier before fork point. Explicit failure prevents TurnComplete/canFork from being published before the authoritative anchor.
    return [];
  }
  const entries = await store.sessionEntries({
    sessionID: runtime.sessionId,
    type: SESSION_ENTRY_TARGET_COMPLETION_VERIFICATION,
  });
  return entries.flatMap((entry) => {
    const payload = verificationEntryPayload(entry);
    if (payload?.targetId !== target.targetID) return [];
    if (
      payload.anchorAssistantMessageId &&
      !prefixMessageIds.has(payload.anchorAssistantMessageId)
    ) {
      return [];
    }
    return [entry.id];
  });
}

function verificationEntryPayload(
  entry: SessionEntryInfo,
): VerificationEntryPayload | null {
  if (
    !entry.data ||
    typeof entry.data !== "object" ||
    Array.isArray(entry.data)
  )
    return null;
  const payload = (entry.data as { payload?: unknown }).payload;
  if (!payload || typeof payload !== "object" || Array.isArray(payload))
    return null;
  const record = payload as Record<string, unknown>;
  return {
    ...(typeof record.targetId === "string"
      ? { targetId: record.targetId }
      : {}),
    ...(typeof record.anchorAssistantMessageId === "string"
      ? { anchorAssistantMessageId: record.anchorAssistantMessageId }
      : {}),
  };
}
