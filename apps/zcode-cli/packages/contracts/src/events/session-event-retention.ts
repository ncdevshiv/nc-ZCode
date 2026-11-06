import type { SessionEvent } from "./session.events.js";
import { SessionEventType } from "./session.events.js";

/**
* Transient events with the same frequency as the message flow.
* They are still allocated seq by the event store and handed over to the live sink, but they can be eliminated from memory after the turn ends and is replaced by the next turn:
* The text of completed turns is resynthesized from persistent messages, and reducer / rewind / fork / checkpoint do not consume these types.
 */
export const TRANSIENT_SESSION_EVENT_TYPES: ReadonlySet<SessionEventType> = new Set([
  SessionEventType.ModelStreaming,
  SessionEventType.ToolCallProgress,
  SessionEventType.StreamingToolLedgerUpdated,
  SessionEventType.ModelNetworkStatus,
]);

/**
* sealed turn is the time limit when there is no subsequent turn.
* The subagent sub-session has only one turn and can never wait for the next turn_started.
* The last sub-session of the real machine leaves about 10,000 deltas until the record is recycled by the pool;
* `collectExpired` is called with a 60s sample beat, and sealed turns that exceed grace are also eliminated.
 */
export const SEALED_TURN_TRANSIENT_GRACE_MS = 120_000;

export function isTransientSessionEvent(event: Pick<SessionEvent, "type">): boolean {
  return TRANSIENT_SESSION_EVENT_TYPES.has(event.type);
}

export type SessionEventRetentionMode = "unbounded" | "turn-window";

export interface SessionEventRetentionPolicy {
  /**
* Called once after each append. The return value is a list of turnIds whose transient events should be evicted from memory; an empty array means no eviction.
* The strategy only looks at the event sequence itself and does not rely on the persistence results.
   */
  onAppend(event: Pick<SessionEvent, "type" | "turnId">, nowMs: number): readonly string[];
  /**
* Time summary: Return a list of sealed turnIds that ended before `nowMs - graceMs` and have no subsequent turns so far.
* and remove them from the set to be eliminated. Called by low-frequency ticks.
   */
  collectExpired(nowMs: number, graceMs: number): readonly string[];
}

const NO_EVICTION: readonly string[] = [];

function createUnboundedRetention(): SessionEventRetentionPolicy {
  return { onAppend: () => NO_EVICTION, collectExpired: () => NO_EVICTION };
}

/**
* turn window strategy: `turn_complete` / `turn_error` only marks the turn as sealed (sealed),
* Wait until the next `turn_started` arrives and then eliminate all sealed turn transient events at once (one turn lag).
* Reason for lag: the message may not have been placed on the disk when the turn has just ended, and cold recovery still relies on the spelling of memory transient events;
* The beginning of the next turn means that the user has sent another message, and the previous turn must have been persisted.
 */
function createTurnWindowRetention(): SessionEventRetentionPolicy {
  const openTurns = new Set<string>();
  const sealedAt = new Map<string, number>();
  return {
    onAppend(event, nowMs) {
      const turnId = event.turnId;
      if (!turnId) {
        return NO_EVICTION;
      }
      switch (event.type) {
        case SessionEventType.TurnStarted: {
          const evict = [...sealedAt.keys()].filter((sealed) => sealed !== turnId);
          sealedAt.clear();
          openTurns.add(turnId);
          return evict;
        }
        case SessionEventType.TurnComplete:
        case SessionEventType.TurnError: {
          openTurns.delete(turnId);
          if (!sealedAt.has(turnId)) {
            sealedAt.set(turnId, nowMs);
          }
          return NO_EVICTION;
        }
        default:
          return NO_EVICTION;
      }
    },
    collectExpired(nowMs, graceMs) {
      const expired: string[] = [];
      for (const [turnId, at] of sealedAt) {
        if (nowMs - at >= graceMs) {
          expired.push(turnId);
        }
      }
      for (const turnId of expired) {
        sealedAt.delete(turnId);
      }
      return expired;
    },
  };
}

export function createSessionEventRetentionPolicy(
  mode: SessionEventRetentionMode,
): SessionEventRetentionPolicy {
  return mode === "unbounded" ? createUnboundedRetention() : createTurnWindowRetention();
}
