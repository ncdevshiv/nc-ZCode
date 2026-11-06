// ============================================================
//Import cache of amend-resume
// ============================================================
// Removed from engine/types.ts (the file has reached the oxlint max-lines limit): pure data structure, run service reads the precursor
// Journal construction, the engine only looks up the table.

import type { AskStats, NodeKind, PersonaSpec } from "./types.js";

/**
* An importable completed ask: the result of the `actorSeq`th question and answer of a named actor in the predecessor run.
* The array subscript is actorSeq (see {@link ImportedActorCandidate.entries}), so seq does not exist here.
 */
export interface ImportedAskEntry {
/** The inputHash of the predecessor record (for the command body). Compare the hash of this ask with the hash of this ask one by one during the runtime. Only if they are consistent will the hit be achieved. */
  inputHash: string;
  result: unknown;
  stats?: AskStats;
  /**
* The number of messages in the source session after this ask is resolved ({@link NodeRecord.messageBoundary}). **Required**:
* The "no marker precursor overall rejection" gate of the run service ensures that each guideable entry has a boundary, so the engine side
* No need for absent branches - seed truncation is impossible without boundaries.
   */
  messageBoundary: number;
}

/**
* An importable prefix for a named actor in the predecessor run. Built from the predecessor journal by run service (purely deterministic, rebuildable).
 */
export interface ImportedActorCandidate {
/** The normalized persona of the predecessor record - used for comparison of createActor during runtime (if inconsistent, the candidate will be discarded). */
  persona: PersonaSpec;
/** The longest fully completed ask prefix, indexed by actorSeq 0..n-1. */
  entries: ImportedAskEntry[];
  /**
* The transcript source session id parsed by the `resumed_from` chain. service is guaranteed to be present - it does not have any ancestors on the chain
* The candidate holding the actor session has been discarded on the service side (downgraded to a brand new actor), so this is not optional.
   */
  transcriptSourceSessionId: string;
  /**
* The model pin parsed by the precursor is brought to the driver along with the seed. **Only takes effect if the transcription is actually imported**: None of them hit
* Actor uses new parsing without pin (see ensureSession of scheduler).
   */
  resolvedModel?: string;
}

/** An importable world node (world-read / world-run), matched in order of content + appearance. */
export interface ImportedWorldEntry {
  inputHash: string;
  kind: NodeKind;
  result: unknown;
}

/**
* Inject the engine's import cache (acceleration structure of amend-resume). **Pure Data**: The engine maintains zero I/O, this table is represented by
* run service reads the precursor journal build.
* It is not a true source - each hit drops a row of true dwf_node, which can be reconstructed from `resumed_from` if lost.
 */
export interface ImportedRunCache {
/** keys = valid actor names (those that are unique and non-empty within the predecessor). */
  actors: ReadonlyMap<string, ImportedActorCandidate>;
  /**
* key = `inputHash({op, args})`; value = queue arranged in insertion order of predecessor listNodes - the same
* The nth occurrence of `{op,args}` is for the nth record, and the head of the queue is the next hit.
   */
  world: ReadonlyMap<string, ImportedWorldEntry[]>;
}
