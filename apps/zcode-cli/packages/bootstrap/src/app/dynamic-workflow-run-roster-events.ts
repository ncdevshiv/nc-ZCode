// ============================================================
// **Event index** of the situation section: scan the journal events in one go and get all "how long ago"
// ============================================================
// Three groups of readings: stage table, sub-agent roster, and health
// There is only one source for the required time - `StoredEvent.timeCreated`, which is the moment when the event is dropped into the journal. Node row
// (`NodeRecord`) does not have any time column, so "when did this ask start" and "when did it last move"
// Can only be taken from the event track.
//
// This module is that scan, and only that scan: pure function, no I/O, and does not recognize the port type. three consumers
// (-roster-phases / -roster-subagents / -roster) share its products, so getRunDetail is used once
// Only read the event once - the longer the run is, the more expensive this reading will be. Reading it twice will cost twice as much.

import type { StoredEvent } from "@zcode/dynamic-workflow";

/**
* The types of events that count when calculating "Are you still moving?": the script moves forward by itself, or an ask moves forward.
 *
* `phase-entered` and `log` are among them, because they are evidence that the **script** is moving: a string is run between two ask
* The world read script does not send any ask events. If they are excluded, that period of time will be falsely reported as stalled.
 *
* `node-waiting` is deliberately **not** included: waiting for slots and waiting for backoff is the stagnation itself, count it as progress,
* `stalledSince` will never be present - and that is the most typical form of stagnation. `concurrency-changed` with
* `run-stalled` does not count for the same reason: they are **observations** about immobility, not movement.
 */
const PROGRESS_EVENT_TYPES: ReadonlySet<string> = new Set([
  "node-queued",
  "node-dispatched",
  "node-executing",
  "node-settled",
  "node-progress",
  "phase-entered",
  "log",
  "report",
  "artifact-published",
  "usage-updated",
]);

/**
* Node ** life cycle ** events (phase migration). `node-progress` is not among them - one round of parsing is not a life cycle
* Transition, literally identical to the discipline on the reducer side; the subagent's `waiting` criterion is therefore not interrupted by progress events.
 */
const NODE_LIFECYCLE_EVENT_TYPES: ReadonlySet<string> = new Set([
  "node-queued",
  "node-dispatched",
  "node-executing",
  "node-waiting",
  "node-repairing",
  "node-nudged",
  "node-settled",
]);

/** Instance key: `siteId@ordinal`, the same shape as the engine's `refToString`. */
export function instanceKey(siteId: string, ordinal: number): string {
  return `${siteId}@${ordinal}`;
}

/** What ask is currently waiting for (the last observation of `node-waiting` + the moment when this wait is entered). */
export interface RosterWaitTrace {
  cause: "slot" | "backoff";
  reason?: string;
  retryAfterMs?: number;
  since?: number;
}

/** The trace left by a node instance on the event track. */
export interface RosterNodeTrace {
/** The last `node-dispatched` moment in this life (`node-queued` will be cleared: requeuing is a new life). */
  dispatchedAt?: number;
/** The type of the last life cycle event; the `waiting` criterion of the subagent only depends on whether it is `node-waiting`. */
  lastLifecycleType?: string;
/** Only present while waiting (any other lifecycle events will clear it). */
  wait?: RosterWaitTrace;
/** The moment of the last `node-progress` with lastTool**. */
  lastToolAt?: number;
/** The moment of the last progress event of this instance. */
  lastActivityAt?: number;
}

/** The latest ** entry and exit moments of a stage. */
export interface RosterPhaseTrace {
  enteredAt?: number;
  exitedAt?: number;
}

/** Once the node is settled (in the order of events), the health loss streak and cache hits are calculated from this string. */
export interface RosterSettlement {
  key: string;
  outcome: string;
  cached: boolean;
}

/** All products of a scan. */
export interface RosterEventIndex {
  nodes: ReadonlyMap<string, RosterNodeTrace>;
  phases: ReadonlyMap<string, RosterPhaseTrace>;
  settlements: readonly RosterSettlement[];
/** Full run The moment of the last progress event. */
  lastProgressAt?: number;
/** The last `run-stalled` moment, and there is no progress event after it; otherwise it is absent. */
  stalledSince?: number;
/** The reason and time of the last `concurrency-changed` (the upper limit of the value is given by the reducer side). */
  concurrencyReason?: string;
  concurrencySince?: number;
}

/**
* Scan the event to get all the moments needed for the situation cross-section.
 *
* `now` is the time of this reading, only **clamping** is done: the journal can be taken to another machine for reading, and one is later than "now"
* will be rendered as a negative age - that reads like a broken tool, not like a clock that is off. Events with missing timestamps (old memory
* Journal stand-in) Always leave the corresponding fields absent, and never use `now`: that will mark the entire history of a week ago as "just now".
 */
export function indexRosterEvents(events: readonly StoredEvent[], now: number): RosterEventIndex {
  const nodes = new Map<string, RosterNodeTrace>();
  const phases = new Map<string, RosterPhaseTrace>();
  const settlements: RosterSettlement[] = [];
  let lastProgressAt: number | undefined;
  let stalledAt: number | undefined;
  let stalledPending = false;
  let concurrencyReason: string | undefined;
  let concurrencySince: number | undefined;
  let lastPhaseName: string | undefined;

  for (const stored of events) {
    const { event } = stored;
    const at = timeOf(stored, now);

    if (PROGRESS_EVENT_TYPES.has(event.type)) {
      lastProgressAt = laterOf(lastProgressAt, at);
// As long as there is any progress after stagnation observation, the stagnation has ended - even if this progress does not have a timestamp:
// "It moved again" and "When did it move" are two questions. The former does not rely on the clock.
      stalledPending = false;
    }

    if (event.type === "run-stalled") {
      stalledAt = at;
      stalledPending = true;
      continue;
    }
    if (event.type === "concurrency-changed") {
      concurrencyReason = event.reason;
      concurrencySince = at;
      continue;
    }
    if (event.type === "phase-entered") {
      lastPhaseName = trackPhase(phases, event.name, lastPhaseName, at);
      continue;
    }
    if (!("instance" in event)) continue;

    const key = instanceKey(event.instance.siteId, event.instance.ordinal);
    const trace = nodes.get(key) ?? {};
    if (PROGRESS_EVENT_TYPES.has(event.type)) {
      trace.lastActivityAt = laterOf(trace.lastActivityAt, at);
    }
    if (NODE_LIFECYCLE_EVENT_TYPES.has(event.type)) {
      trackLifecycle(trace, event.type, at);
    }
    if (event.type === "node-waiting") {
// If you are already waiting, keep **the moment you enter**: the escape ladder will send several node-waiting messages in succession, and the reader is asking
// "How long has it been stuck", not "When was the last observation sent". The latest reason and retry interval are used.
      const since = trace.wait?.since ?? at;
      trace.wait = {
        cause: event.cause,
        ...(event.reason === undefined ? {} : { reason: event.reason }),
        ...(event.retryAfterMs === undefined ? {} : { retryAfterMs: event.retryAfterMs }),
        ...(since === undefined ? {} : { since }),
      };
    }
    if (event.type === "node-progress" && event.lastTool !== undefined && at !== undefined) {
      trace.lastToolAt = at;
    }
    if (event.type === "node-settled") {
      settlements.push({ key, outcome: event.outcome, cached: event.cached === true });
    }
    nodes.set(key, trace);
  }

  return {
    nodes,
    phases,
    settlements,
    ...(lastProgressAt === undefined ? {} : { lastProgressAt }),
    ...(stalledPending && stalledAt !== undefined ? { stalledSince: stalledAt } : {}),
    ...(concurrencyReason === undefined ? {} : { concurrencyReason }),
    ...(concurrencySince === undefined ? {} : { concurrencySince }),
  };
}

/**
* Remember once `phase-entered`: it not only starts its own phase, but also ends the previous **synonymous** phase.
* Re-entry (returning to the side) with the same name only moves the entry time forward, and does not count as leaving oneself. Returns the new "last entered stage name".
 */
function trackPhase(
  phases: Map<string, RosterPhaseTrace>,
  name: string,
  lastPhaseName: string | undefined,
  at: number | undefined,
): string {
  if (lastPhaseName !== undefined && lastPhaseName !== name && at !== undefined) {
    const previous = phases.get(lastPhaseName);
    if (previous !== undefined) previous.exitedAt = at;
  }
// Take the most recent entry instead of the first time: in the stage where you have been circled three times, the question you need to ask is "how long has it been since you entered this circle?"
// Reentering clears the exit time of the previous circle - it is open again at the moment.
  phases.set(name, at === undefined ? {} : { enteredAt: at });
  return name;
}

/** Remember a life cycle event: requeuing clears the dispatch time of the previous life, and clears waiting for any non-waiting events. */
function trackLifecycle(trace: RosterNodeTrace, type: string, at: number | undefined): void {
  trace.lastLifecycleType = type;
  if (type !== "node-waiting") trace.wait = undefined;
  if (type === "node-queued") trace.dispatchedAt = undefined;
  if (type === "node-dispatched" && at !== undefined) trace.dispatchedAt = at;
}

/**
* The time when the event is dropped. Absent, non-finite and non-positive values ​​are always read as "no clock" - `0` is 1970,
* It could only be a missed fill somewhere, not a real moment.
 */
function timeOf(stored: StoredEvent, now: number): number | undefined {
  const time = stored.timeCreated;
  if (typeof time !== "number" || !Number.isFinite(time) || time <= 0) return undefined;
  return Math.min(time, now);
}

/** The later of the two available absence times. */
export function laterOf(left: number | undefined, right: number | undefined): number | undefined {
  if (left === undefined) return right;
  if (right === undefined) return left;
  return Math.max(left, right);
}
