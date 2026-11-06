// The real-life adaptation and deactivation execution surface of the Session resident pool.

import type { SessionId } from "@zcode/contracts";
import type {
  SessionDeactivationDecision,
  SessionResidentPoolHost,
} from "./session-resident-pool.js";
import type { ZCodeProtocolAgentServerContext } from "./server-types.js";

interface SessionResidencyFinalizationOwner {
  residencyFinalizationCount?: number;
}

/**
* The residency lease only covers the closing window of the Bootstrap detached runner to avoid pool recycling that is still in use.
* record. Prompt admission's busy/idle authority has been relegated to Core; activeAbortController is legacy command only
* The path's compatible cancellation handle can no longer be used as a prompt dispatch lock.
 */
function acquireSessionResidencyFinalization(
  record: SessionResidencyFinalizationOwner,
): () => void {
  record.residencyFinalizationCount = (record.residencyFinalizationCount ?? 0) + 1;
  let released = false;
  return () => {
    if (released) return;
    released = true;
    record.residencyFinalizationCount = Math.max(0, (record.residencyFinalizationCount ?? 1) - 1);
  };
}

/** Register the lease synchronously and then start the detached runner; both synchronous error throwing and asynchronous final state are guaranteed to be released. */
export function runWithSessionResidencyFinalization<T>(
  record: SessionResidencyFinalizationOwner,
  run: () => Promise<T>,
): Promise<T> {
  const release = acquireSessionResidencyFinalization(record);
  try {
    return run().finally(release);
  } catch (error) {
    release();
    return Promise.reject(error);
  }
}

/**
* Only release the resident runtime and do not delete any persistent session/task facts.
 *
* If you await app.close first and then remove it from the registry, the new cold subscribe will be in await
* The window hits an old runtime that has entered shutdown; if the record is deleted first but the pool gate is not set, the old and new apps will
* The same session resource will be touched concurrently. Therefore, synchronous removal and asynchronous close must be performed by the in-flight gate of the pool.
* Combined into a life cycle transaction.
 */
async function deactivateSessionRecord(
  context: ZCodeProtocolAgentServerContext,
  sessionId: string,
): Promise<void> {
  const record = context.sessions.get(sessionId);
  if (!record) return;
// CommandInbox pin rejection occurred after unsubscribe, the exception will remain in the registry
// But the half-clear record of the runtime event cannot be received. All preflightable rejections must precede the first side effect.
  context.v4Gateway?.assertSessionRuntimeDeactivatable(sessionId);
  record.unsubscribe?.();
  context.v4Gateway?.deactivateSession(sessionId);
  context.sessions.delete(sessionId);
  await record.app.close?.();
// The memory event store after deactivation must be equivalent to "never loaded".
  await record.eventStore.deleteSession(sessionId as SessionId);
}

export function createSessionResidentPoolHost(
  context: ZCodeProtocolAgentServerContext,
): SessionResidentPoolHost {
  return {
    deactivate: (sessionId) => deactivateSessionRecord(context, sessionId),
    listSessionIds: () => [...context.sessions.keys()],
    readResidencyFacts: (sessionId) => {
      const record = context.sessions.get(sessionId);
      if (!record) return null;
      return {
        hasPendingInteractions: context.v4Interactions.hasPendingForSession(sessionId),
        hasQueuedCommands: context.v4Gateway?.hasResidencyBlockingCommands(sessionId) ?? false,
        hasLegacySubscriber: record.legacyStreamSubscribed === true,
// active/queue and registry background task cannot cover title, MCP, memory
// Wait for detached work; unified query is maintained by runtime, and protocol finalization only supplements protocol ownership.
        hasResidencyBlockingWork:
          record.activeAbortController !== undefined ||
          (record.residencyFinalizationCount ?? 0) > 0 ||
          record.app.runtime.hasResidencyBlockingWork(),
        hasSubscribers: context.v4Gateway?.hasConversationSubscribers(sessionId) ?? false,
        lastActivityAt: record.updatedAt,
        persisted: record.persistence === "immediate",
      };
    },
    onDeactivated: (sessionId, decision) => {
      context.logger?.info("Session resident runtime deactivated", {
        ...deactivationLogContext(decision),
        event: "zcode_protocol.session.resident_deactivated",
        sessionId,
      });
    },
    onError: (sessionId, error, decision) => {
      context.logger?.warn("Session resident deactivation failed", {
        ...deactivationLogContext(decision),
        error: error instanceof Error ? error.message : String(error),
        event: "zcode_protocol.session.resident_deactivation_failed",
        sessionId,
      });
    },
  };
}

function deactivationLogContext(
  decision: SessionDeactivationDecision,
): Record<string, number | string> {
  return {
    highWaterCount: decision.highWaterCount,
    idleMs: decision.idleMs,
    idleTimeoutMs: decision.idleTimeoutMs,
    reason: decision.reason,
    residentCountBefore: decision.residentCountBefore,
    targetCount: decision.targetCount,
  };
}
