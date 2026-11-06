/**
* Memory version {@link JournalStorePort}: Phase 1 is used for fake-driver testing and replay/resume.
* Pure memory, synchronization; deep copy in and out to prevent references held by the caller from being accidentally changed by subsequent writes (simulating storage boundaries).
* The production implementation falls on node:sqlite (DatabaseSync, synchronization) of zcode session store, sharing JournalStorePort;
* Transactions are not on the port surface and are written by the driver combination journal+session.
 */

import type {
  ActorRecord,
  JournalStorePort,
  ListEventsOptions,
  NodeRecord,
  RunEvent,
  RunRecord,
  RunSettlementRecord,
  RunStatus,
  StoredEvent,
} from "./types.js";

/** Structured deep copy: isolate references on both sides of the storage boundary. Values ​​are either JSON-compatible or pure data such as PersonaSpec. */
function clone<T>(value: T): T {
  return structuredClone(value);
}

/** Composite key for actor/node. */
function key(siteId: string, ordinal: number): string {
  return `${siteId}@${ordinal}`;
}

export class InMemoryJournalStore implements JournalStorePort {
// Divide buckets by runId to fit the storage semantics of "one store can host multiple runs".
  private readonly runs = new Map<string, RunRecord>();
  private readonly actors = new Map<string, Map<string, ActorRecord>>();
  private readonly nodes = new Map<string, Map<string, NodeRecord>>();
  private readonly events = new Map<string, StoredEvent[]>();

  createRun(record: RunRecord): void {
    if (this.runs.has(record.runId)) {
      throw new Error(`journal: run ${record.runId} already exists`);
    }
    this.runs.set(record.runId, clone(record));
    this.actors.set(record.runId, new Map());
    this.nodes.set(record.runId, new Map());
    this.events.set(record.runId, []);
  }

  getRun(runId: string): RunRecord | undefined {
    const r = this.runs.get(runId);
    return r === undefined ? undefined : clone(r);
  }

  updateRunStatus(runId: string, status: RunStatus, settlement?: RunSettlementRecord): void {
    const r = this.runs.get(runId);
    if (r === undefined) throw new Error(`journal: unknown run ${runId}`);
    r.status = status;
    if (status === "pending" || status === "running") {
// Non-final state = None settlement: resume When turning run back to running, the residue of the previous life must be cleared, otherwise
// The failure_json written by orphan convergence will coexist with running (journal snapshot reading will report "running" and
// "Failed"). Contradictory settlement bags (non-final state but carrying failure/result) are also treated as cleared.
      delete r.failure;
      delete r.result;
      delete r.stopReason;
      delete r.supersededBy;
      return;
    }
// Three-piece set of failures (failure/stopReason/supersededBy) **Overall rewrite**: The settlement bag failed at this moment
// The whole truth, there is no failure in absence. Under the old "absent = no touch" semantics, external writing failed + Interrupted
// Will survive subsequent completed settlements, and the line will say both "completed" and "interrupted"
// (The SQLite side is the same coalesce, and the two implementations have the same semantics).
    if (settlement?.stopReason === undefined) delete r.stopReason;
    else r.stopReason = settlement.stopReason;
    if (settlement?.supersededBy === undefined) delete r.supersededBy;
    else r.supersededBy = settlement.supersededBy;
    if (settlement?.failure === undefined) delete r.failure;
    else r.failure = clone(settlement.failure);
// Product is the opposite: an absent key means "no touch", and a repeated settlement without a product will not erase the settled product.
// `result: null` is a legal product, only undefined is considered absent.
    if (settlement?.result !== undefined) r.result = clone(settlement.result);
  }

  updateRunUsage(runId: string, spentTokens: number): void {
    const r = this.runs.get(runId);
    if (r === undefined) throw new Error(`journal: unknown run ${runId}`);
    r.spentTokens = spentTokens;
  }

  putActor(record: ActorRecord): void {
    const bucket = this.requireActorBucket(record.runId);
    bucket.set(key(record.siteId, record.ordinal), clone(record));
  }

  getActor(runId: string, siteId: string, ordinal: number): ActorRecord | undefined {
    const a = this.actors.get(runId)?.get(key(siteId, ordinal));
    return a === undefined ? undefined : clone(a);
  }

  listActors(runId: string): ActorRecord[] {
    const bucket = this.actors.get(runId);
    return bucket === undefined ? [] : [...bucket.values()].map(clone);
  }

  putNode(record: NodeRecord): void {
    const bucket = this.requireNodeBucket(record.runId);
    bucket.set(key(record.siteId, record.ordinal), clone(record));
  }

  getNode(runId: string, siteId: string, ordinal: number): NodeRecord | undefined {
    const n = this.nodes.get(runId)?.get(key(siteId, ordinal));
    return n === undefined ? undefined : clone(n);
  }

  listNodes(runId: string): NodeRecord[] {
    const bucket = this.nodes.get(runId);
    return bucket === undefined ? [] : [...bucket.values()].map(clone);
  }

  appendEvent(runId: string, event: RunEvent): StoredEvent {
// Orphan events (run has not createdRun yet) are contract violations like putActor/putNode: silent bucket creation
// A batch of events that will never belong to any run will be written; there is FK on the SQLite side, and this sentence on the memory side.
    const list = this.events.get(runId);
    if (list === undefined) throw new Error(`journal: unknown run ${runId}`);
// The appended time has the same semantics as `dwf_event.time_created` on the SQLite side (the two implementations share a contract test): in the event log
// All "how long ago" can only be calculated from it, allowing readers to retrieve Date.now() now will mark the entire history of cold replay as "just now".
    const stored: StoredEvent = {
      sequence: list.length,
      event: clone(event),
      timeCreated: Date.now(),
    };
    list.push(stored);
    return clone(stored);
  }

  listEvents(runId: string, opts?: ListEventsOptions): StoredEvent[] {
    const list = this.events.get(runId);
    if (list === undefined) return [];
// sequence and array subscript are identical in the memory implementation (appendEvent is allocated using list.length), but here it is still
// Sequence comparison instead of offset by subscript: the semantics of cursor is "strictly greater than the sequence", and the same is true on the SQLite side
// `where sequence > ?`. Both sides share the same contract test, and the semantics must be the same word-for-word.
    const after = opts?.afterSequence;
    const filtered = after === undefined ? list : list.filter((e) => e.sequence > after);
    const limited = opts?.limit === undefined ? filtered : filtered.slice(0, Math.max(0, opts.limit));
    return limited.map(clone);
  }

  private requireActorBucket(runId: string): Map<string, ActorRecord> {
    const bucket = this.actors.get(runId);
    if (bucket === undefined) throw new Error(`journal: unknown run ${runId}`);
    return bucket;
  }

  private requireNodeBucket(runId: string): Map<string, NodeRecord> {
    const bucket = this.nodes.get(runId);
    if (bucket === undefined) throw new Error(`journal: unknown run ${runId}`);
    return bucket;
  }
}
