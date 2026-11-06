import type { SessionEventStorePort, SessionEventStoreStats } from "../interfaces/session.port.js";
import type { SessionId } from "../interfaces/shared.js";
import type { SessionEvent } from "./session.events.js";
import {
  createSessionEventRetentionPolicy,
  isTransientSessionEvent,
  SEALED_TURN_TRANSIENT_GRACE_MS,
  type SessionEventRetentionMode,
  type SessionEventRetentionPolicy,
} from "./session-event-retention.js";

export interface InMemorySessionEventStoreOptions {
/** Default `turn-window`; `unbounded` for rollback and control testing. Custom policy factories (created by session) can also be injected. */
  retention?: SessionEventRetentionMode | (() => SessionEventRetentionPolicy);
/** Clock injected for testing; Date.now for production. */
  now?: () => number;
}

interface SessionEventState {
  events: SessionEvent[];
  latestSequenceNumber: number;
  policy: SessionEventRetentionPolicy;
  evictedEvents: number;
}

/**
* In-process session event store.
 *
* It is the only source of live/replay/snapshot sequence numbers, so all events are assigned seq via `append`;
* But transient events only reside in the turn window, and buckets that have been completed and replaced by the next turn will be eliminated from memory.
* append If the array is copied in full every time and never eliminated, the long session memory will grow linearly according to the token.
 */
export class InMemorySessionEventStore implements SessionEventStorePort {
  private readonly sessions = new Map<SessionId, SessionEventState>();
  private readonly createPolicy: () => SessionEventRetentionPolicy;
  private readonly now: () => number;

  constructor(options: InMemorySessionEventStoreOptions = {}) {
    const retention = options.retention ?? "turn-window";
    this.createPolicy =
      typeof retention === "function"
        ? retention
        : () => createSessionEventRetentionPolicy(retention);
    this.now = options.now ?? (() => Date.now());
  }

  private stateFor(sessionId: SessionId): SessionEventState {
    let state = this.sessions.get(sessionId);
    if (!state) {
      state = {
        events: [],
        latestSequenceNumber: 0,
        policy: this.createPolicy(),
        evictedEvents: 0,
      };
      this.sessions.set(sessionId, state);
    }
    return state;
  }

  async append(event: SessionEvent): Promise<SessionEvent> {
    const state = this.stateFor(event.sessionId);
    const sequenceNumber =
      event.sequenceNumber > 0 ? event.sequenceNumber : state.latestSequenceNumber + 1;
// The counter only increases but does not decrease: elimination does not allow subsequent getLatestSequenceNumber()+1 to generate duplicate seqs.
    state.latestSequenceNumber = Math.max(state.latestSequenceNumber, sequenceNumber);
    const storedEvent = { ...event, sequenceNumber };
    state.events.push(storedEvent);
    const evictTurnIds = state.policy.onAppend(storedEvent, this.now());
    if (evictTurnIds.length > 0) {
      this.evictTransientEvents(state, new Set(evictTurnIds));
    }
    return storedEvent;
  }

  /**
* Time bottom-out (called by 60s low-frequency tick): Eliminate transient events of sealed turns that end beyond grace and have no subsequent turns.
* Cover one-time sessions such as subagent sub-session. Returns the number of items eliminated this time.
   */
  pruneTransientEvents(
    nowMs: number = this.now(),
    graceMs: number = SEALED_TURN_TRANSIENT_GRACE_MS,
  ): number {
    let evicted = 0;
    for (const state of this.sessions.values()) {
      const expired = state.policy.collectExpired(nowMs, graceMs);
      if (expired.length === 0) {
        continue;
      }
      const before = state.evictedEvents;
      this.evictTransientEvents(state, new Set(expired));
      evicted += state.evictedEvents - before;
    }
    return evicted;
  }

  private evictTransientEvents(state: SessionEventState, turnIds: ReadonlySet<string>): void {
    const retained: SessionEvent[] = [];
    for (const event of state.events) {
      if (isTransientSessionEvent(event) && event.turnId && turnIds.has(event.turnId)) {
        state.evictedEvents += 1;
        continue;
      }
      retained.push(event);
    }
    state.events = retained;
  }

  async getEvents(sessionId: SessionId): Promise<SessionEvent[]> {
    return [...(this.sessions.get(sessionId)?.events ?? [])];
  }

  async getEventsAfter(sessionId: SessionId, sequenceNumber: number): Promise<SessionEvent[]> {
    return (this.sessions.get(sessionId)?.events ?? []).filter(
      (event) => event.sequenceNumber > sequenceNumber,
    );
  }

  async getLatestSequenceNumber(sessionId: SessionId): Promise<number> {
    return this.sessions.get(sessionId)?.latestSequenceNumber ?? 0;
  }

  async deleteSession(sessionId: SessionId): Promise<void> {
    this.sessions.delete(sessionId);
  }

/** The resident size for memory diagnostic log reading; only the length is counted, not copied. */
  getStats(): SessionEventStoreStats {
    let events = 0;
    let evictedEvents = 0;
    let retainedTransient = 0;
    for (const state of this.sessions.values()) {
      events += state.events.length;
      evictedEvents += state.evictedEvents;
      for (const event of state.events) {
        if (isTransientSessionEvent(event)) {
          retainedTransient += 1;
        }
      }
    }
    return { sessions: this.sessions.size, events, evictedEvents, retainedTransient };
  }
}

export function createInMemorySessionEventStore(
  options?: InMemorySessionEventStoreOptions,
): InMemorySessionEventStore {
  return new InMemorySessionEventStore(options);
}
