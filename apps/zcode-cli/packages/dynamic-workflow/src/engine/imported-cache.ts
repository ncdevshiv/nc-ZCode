/**
* amend-resume imports the cached **consumer state**.
 *
* The injected {@link ImportedRunCache} is read-only data constructed by the caller - run service reads the predecessor journal to get it,
* After a crash, resume uses the same pure function to rebuild the same table. Therefore, "Where is the consumption?" cannot be written back to the table on the spot, it must be the engine.
* Own state: This module is this state and all the judgments surrounding it (hit/divergence/seed).
 *
* Separate scheduler.ts and engine.ts for the same reason as when scheduler was separated from engine: three files respectively
* Focused, readable, and meets the maximum number of lines in a single file.
 */

import { canonicalJson } from "./hash.js";
import type {
  ActorRef,
  ActorSessionSeed,
  ImportedActorCandidate,
  ImportedAskEntry,
  ImportedRunCache,
  ImportedWorldEntry,
  InstanceRef,
  NodeRecord,
  PersonaSpec,
} from "./types.js";

/**
* Import consumption state of an attached candidate actor: consumption cursor + divergence flag.
 *
* **Diverged Monotone**: Once `diverged` is true, the cache will never be looked back, and the imported suffix will be discarded and cannot be revived.
 */
export class ImportedActorState {
/** Number of imported entries consumed = next hitable actorSeq (also the subscript of the transcription truncation boundary + 1). */
  private consumed = 0;
  private diverged = false;

  constructor(private readonly candidate: ImportedActorCandidate) {}

  /**
* fresh ask asks the cache on seq: a hit returns the entry (and advances the cursor), otherwise sets the divergence and returns undefined.
   *
* Two misses are treated equally: hash mismatch (instruction changed) and `seq >= entries.length` (new script on this actor
* Expanded with new ask). Different **cascades** are free and dynamic - upstream ask to live to get new results, and downstream uses it to interpolate
* The instruction hash must be changed, so downstream automatically diverges without any explicit propagation.
   */
  take(seq: number, hash: string): ImportedAskEntry | undefined {
    if (this.diverged) return undefined;
    const entry = this.candidate.entries[seq];
    if (entry === undefined || entry.inputHash !== hash) {
      this.diverged = true;
      return undefined;
    }
    this.consumed = seq + 1;
    return entry;
  }

  /**
* Question after caching is turned off: Only **pure** entries (the predecessor noted `worldToolCalls === 0`) can still hit. Pure ask relies only on directives and
* The transcription prefix - both are in the hash chain - is independent of the workspace, so closing the door does not affect its answer. Entries that have touched the outside world are the same as the hash
* Nor: the workspace it read may have been overwritten; the ask turns live, and the actor diverges from it (the transcription is no longer consistent with the predecessor,
* Suffix entries cannot be revived). Old entries that do not have stats, or do not have this key in stats are treated as "touched" (conservative).
   */
  takeIfPure(seq: number, hash: string): ImportedAskEntry | undefined {
    if (this.diverged) return undefined;
    const entry = this.candidate.entries[seq];
    if (entry === undefined || entry.inputHash !== hash || entry.stats?.worldToolCalls !== 0) {
      this.diverged = true;
      return undefined;
    }
    this.consumed = seq + 1;
    return entry;
  }

  /**
* According to a row of **recorded** ask re-pushing the divergence status (revised when run resumes after a crash).
   *
* The different states will not be saved in the library, and the resume of the revised run will rebuild the entire import cache table. If the divergence point is not re-recommended based on the journal row,
* For an actor that has diverged from seq k in the original execution, its fresh ask of seq k+n may happen to hit the imported entry.
* Hash is **incorrectly imported** - that is equivalent to stuffing back a piece of history that has nothing to do with the actual transcription of this run. Get record rows by seq
* The inputHash is compared with the imported entry, and the judgment at the time of the original execution is reconstructed (admission is in ascending order of seq, and the hold rule guarantees
* this), so this recalculation is exact, not a conservative approximation. `wasLive` complements the kind of live that the hash cannot see.
* (ask with tools after caching is turned off, see scheduler's tryImportedSettle).
   */
  reconcileRecorded(seq: number, recordedHash: string, wasLive: boolean): void {
    if (this.diverged) return;
// After caching is turned off, an ask with a tool actor will work even with the import
// Entries and hashes are also run live - counting the hash as "consumed" will cause the seed boundary to be taken from the predecessor entry, and the current session's
// The real transcription at that location is not those messages at all. Live or not is a fact in the event (node-queued), not a hash
// can be derived.
    if (wasLive) {
      this.diverged = true;
      return;
    }
    const entry = this.candidate.entries[seq];
    if (entry === undefined || entry.inputHash !== recordedHash) {
      this.diverged = true;
      return;
    }
    this.consumed = Math.max(this.consumed, seq + 1);
  }

  /**
* Session seed for different actors: source session + number of messages copied + inherited model pin.
   *
* The boundary takes the accounting value of the last imported entry that was consumed: the difference in seq k means that 0..k-1 questions and answers have been cached
* To settle, the new session will be exactly where it is after the k complete exchanges (including their repair / nudge rounds).
* There is no seed without consumption - a new session, a new model analysis, without a pin: the pin is for "no silence under transcription continuation"
*Change model" exists, but without continuity there would be no use for it.
   */
  seed(): ActorSessionSeed | undefined {
    if (this.consumed === 0) return undefined;
    const last = this.candidate.entries[this.consumed - 1];
    if (last === undefined) return undefined;
    const seed: ActorSessionSeed = {
      sourceSessionId: this.candidate.transcriptSourceSessionId,
      messageCount: last.messageBoundary,
    };
    if (this.candidate.resolvedModel !== undefined) seed.resolvedModel = this.candidate.resolvedModel;
    return seed;
  }
}

/**
* The import queue consumption state of the world node: one cursor for each content hash (nth occurrence for nth record).
* In the same way as {@link ImportedActorState}, the cursor is the state of the engine, and the injected queue remains read-only.
 */
export class ImportedWorldQueue {
  private readonly cursors = new Map<string, number>();

  constructor(private readonly world: ReadonlyMap<string, ImportedWorldEntry[]>) {}

/** Get the next record of the content hash, which is undefined if it is exhausted or has never been recorded (the caller switches to live). */
  take(hash: string): ImportedWorldEntry | undefined {
    const queue = this.world.get(hash);
    if (queue === undefined) return undefined;
    const cursor = this.cursors.get(hash) ?? 0;
    const entry = queue[cursor];
    if (entry === undefined) return undefined;
    this.cursors.set(hash, cursor + 1);
    return entry;
  }
}

/**
* Find import candidates for a newly created actor: look up the table by **effective name**, and accept only if the normalized persona is consistent.
 *
* Use `canonicalJson` for persona comparison - it skips undefined members and object key sorting, which is exactly the normalization you want
* (`{name:"a"}` has the same value as `{name:"a", system: undefined}`). Inconsistency means discarding the entire candidate and the actor
* Brand new rerun: This is doubly correct under full fidelity transcription - the transcription produced by the old system prompt and the new persona is a confusion of identity.
* And the intention of "I fixed the persona" is to run again.
 *
* Anonymous actors never attach: the name is the cached identity key, without a name there are no coordinates to compare against (the cost is adjudicated).
* The reason why the two scripts are compared statically at **runtime** instead of submission: name and persona are both runtime values.
* (The actual parameters of `agent()` can be dynamic expressions). Static comparison is the second truth, which is what this package needs to guard against.
 */
export function matchImportedActor(
  cache: ImportedRunCache | undefined,
  spec: PersonaSpec,
): ImportedActorState | undefined {
  if (cache === undefined) return undefined;
  const name = spec.name;
  if (name === undefined || name === "") return undefined;
  const candidate = cache.actors.get(name);
  if (candidate === undefined) return undefined;
  if (canonicalJson(spec) !== canonicalJson(candidate.persona)) return undefined;
  return new ImportedActorState(candidate);
}

/**
* One ask cache hit to drop the **true** dwf_node row (new siteId, copied result/stats/bounds).
 *
* The boundary values ​​must be copied together, otherwise **this** run itself cannot be revised - the chain revision relies on count offset
* Copy across prefixes unchanged.
 */
export function importedAskRecord(
  runId: string,
  instance: InstanceRef,
  actor: ActorRef,
  seq: number,
  hash: string,
  entry: ImportedAskEntry,
): NodeRecord {
  const record: NodeRecord = {
    runId,
    siteId: instance.siteId,
    ordinal: instance.ordinal,
    kind: "ask",
    actorSiteId: actor.siteId,
    actorOrdinal: actor.ordinal,
    actorSeq: seq,
    inputHash: hash,
    status: "completed",
    result: entry.result,
    messageBoundary: entry.messageBoundary,
  };
  if (entry.stats !== undefined) record.stats = entry.stats;
  return record;
}
