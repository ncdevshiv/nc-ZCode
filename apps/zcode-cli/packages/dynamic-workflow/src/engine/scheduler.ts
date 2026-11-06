/**
* ask scheduler: separate "per actor FIFO + hold rules + concurrent upper limit dispatch + ask settlement" from the engine main body,
* Make both files focused and readable (and meet the maximum number of lines in a single file). The engine injects it via {@link SchedulerHost}
* Dependencies (driver, caps, sequence number allocation, run-level failure), the scheduler is only responsible for the ask life cycle, regardless of the run life cycle.
 *
* See the description at the top of engine.ts for key rules: hit short circuit, actorSeq admission order, replay hold rules, serial actors.
 *
* The internal types and SchedulerHost are in scheduler-types.ts; the upward return processing of submit / turn is in scheduler-submit.ts
* (Reason for splitting: oxlint max-lines limit is 400 lines). SchedulerHost is exported here again, and the import path remains unchanged.
 */

import { inputHash } from "./hash.js";
import { importedAskRecord, type ImportedActorState } from "./imported-cache.js";
import { defer, type Actor, type AskNode, type Deferred, type SchedulerHost } from "./scheduler-types.js";
import { handleSubmitAttempted, handleTurnEnded, type SubmitSeam } from "./scheduler-submit.js";
import type {
  ActorId,
  ActorRef,
  AskMessage,
  AskSpec,
  AskStats,
  InstanceRef,
  JournalStorePort,
  NodeRecord,
  PersonaSpec,
  SessionRef,
} from "./types.js";
import {
  INSTRUCTIONS_HEAD_MAX_CHARS,
  NUDGE_ATTEMPTS,
  refToString,
  REPAIR_ATTEMPTS,
  WorkflowError,
} from "./types.js";

export type { SchedulerHost } from "./scheduler-types.js";

export class AskScheduler {
  private readonly actors = new Map<ActorId, Actor>();
  private readonly actorOrder: Actor[] = [];
  private readonly liveNodes = new Map<string, AskNode>();
  private activeAsks = 0;

/** The free function of scheduler-submit.ts checks the live node and settles according to the result (see {@link SubmitSeam}). */
  private readonly submitSeam: SubmitSeam;

  constructor(private readonly host: SchedulerHost) {
    this.submitSeam = {
      host,
      liveNode: (instance) => this.liveNodes.get(refToString(instance)),
      settleOk: (node, artifact) => this.settleOk(node, artifact),
      settleFailed: (node, error) => this.settleFailed(node, error),
    };
  }

  private get journal(): JournalStorePort {
    return this.host.driver.journal;
  }

  hasActor(id: ActorId): boolean {
    return this.actors.has(id);
  }

  /**
* Register an actor (create site × serial number). `imported` is an import candidate for amend-resume: the engine is already in
* Matched by effective name + normalized persona in createActor, the scheduler is only responsible for consuming it.
   */
  registerActor(
    ref: ActorRef,
    id: ActorId,
    name: string | undefined,
    persona: PersonaSpec,
    imported?: ImportedActorState,
  ): Actor {
    const recordedCount = this.journal
      .listNodes(this.host.runId)
      .filter((n) => n.kind === "ask" && n.actorSiteId === ref.siteId && n.actorOrdinal === ref.ordinal).length;
    const actor: Actor = {
      ref,
      id,
      persona,
      name,
      recordedCount,
      nextAdmitSeq: 0,
      pendingRecorded: new Map(),
      pendingLive: [],
      liveQueue: [],
      ...(imported === undefined ? {} : { imported }),
    };
// The sessionId recorded in the journal cannot be pre-parsed into actor.session/sessionPromise:
// Let ensureSession after resume skip driver.createActorSession. But the session table of the production driver
// **Only** fill in createActorSession, startAsk checks the session according to the table - so resume redistributes each
// ask all failed with "unknown session". Both pure replay (zero dispatch) and fake driver (startAsk does not look up the table) are exposed
// Can't do it. The session identity is owned by the driver (the production casting function is purely determined by (runId, actorRef), and when re-hanging
// cast the same id); the journal's dwf_actor.session_id is a record, not an authority, so it will never be used here.
//Journal short-circuit session - the first dispatch will always go through driver.createActorSession (see ensureSession).
    this.actors.set(id, actor);
    this.actorOrder.push(actor);
    return actor;
  }

/** Accept an ask once: the completion hit is short-circuited (follow the hold rule), and the running hit or miss will be distributed live. */
  admitAsk(siteId: string, actorId: ActorId, instructions: string, spec: AskSpec): Promise<unknown> {
    const actor = this.actors.get(actorId)!;
    const ordinal = this.host.nextOrdinal(siteId);
    const instance: InstanceRef = { siteId, ordinal };
    const hash = inputHash(instructions);
    const deferred = defer<unknown>();

    const recorded = this.journal.getNode(this.host.runId, siteId, ordinal);
    if (recorded !== undefined) {
// A hit is a defensive check inputHash - inconsistency indicates that the purity contract is broken, and the run fails loudly.
      if (recorded.inputHash !== hash) {
        const err = hashMismatch(instance, recorded.inputHash, hash);
        this.host.failRun(err);
        return Promise.reject(err);
      }
      const seq = recorded.actorSeq ?? 0;
      if (recorded.status === "running") {
// Crash in execution: re-dispatch live according to the recorded actorSeq position (the hold rule guarantees its admission order).
        actor.pendingRecorded.set(seq, () => {
          actor.imported?.reconcileRecorded(seq, recorded.inputHash, this.host.wasLiveBeforeResume(instance));
          this.admitLive(instance, actor, seq, instructions, hash, spec, deferred);
        });
      } else {
// completed / failed: short circuit settlement, no driver call.
        actor.pendingRecorded.set(seq, () => {
          actor.imported?.reconcileRecorded(seq, recorded.inputHash, this.host.wasLiveBeforeResume(instance));
          this.releaseCachedAsk(instance, recorded, deferred);
        });
      }
      this.drainAdmission(actor);
      return deferred.promise;
    }

// Miss (fresh): Register to pendingLive, admit and allocate new actorSeq in order of arrival after the record node is drained.
// After assigning to seq, first ask the import cache (amend-resume): if the hit is cached, it will not live.
    actor.pendingLive.push(() => {
      const seq = actor.nextAdmitSeq++;
      if (this.tryImportedSettle(instance, actor, seq, hash, deferred)) return;
      this.admitLive(instance, actor, seq, instructions, hash, spec, deferred);
    });
    this.drainAdmission(actor);
    this.pumpAll();
    return deferred.promise;
  }

/** Admit an ask as a live node: create the node, drop the running record after admission, join the queue and record the event. */
  private admitLive(
    instance: InstanceRef,
    actor: Actor,
    seq: number,
    instructions: string,
    hash: string,
    spec: AskSpec,
    deferred: Deferred<unknown>,
  ): void {
    const node: AskNode = {
      instance,
      actor,
      actorSeq: seq,
      instructions,
      hash,
      spec,
      deferred,
      repairsRemaining: REPAIR_ATTEMPTS,
      nudgesRemaining: NUDGE_ATTEMPTS,
      settled: false,
      dispatched: false,
    };
    this.liveNodes.set(refToString(instance), node);
// Fall after admission running (with actorSeq + inputHash): the node that crashes during execution resume can be redistributed accordingly.
    this.journal.putNode({
      runId: this.host.runId,
      siteId: instance.siteId,
      ordinal: instance.ordinal,
      kind: "ask",
      actorSiteId: actor.ref.siteId,
      actorOrdinal: actor.ref.ordinal,
      actorSeq: seq,
      inputHash: hash,
      status: "running",
    });
    actor.liveQueue.push(node);
// The beginning of the instruction is derailed with the birth event: the instructions here are still
// Author's original text - driver quality / schema endnotes are only added in startAsk, so the engine words will not be mixed into the summary.
    const instructionsHead = headOfInstructions(instructions);
    this.host.record({
      type: "node-queued",
      instance,
      kind: "ask",
      actor: actor.ref,
      actorSeq: seq,
      ...(instructionsHead === undefined ? {} : { instructionsHead }),
    });
// Switch to live without turning off the import cache. Once closed here: any ask live
// This means that the workspace may be overwritten. That's using a clock instead of a dependency - after the first miss in the same Promise.all
// Brothers ask are all invalidated, and there is no dependence between them (actually measured 50-way fan-out lost 6 hits, 5-way fan-out
// Discard the only 1). What is closed now is the **first write**: reported by the driver when the subagent is about to execute the rewrite tool
// askMutating, or a world.run live execution; the world before this is the same as the world left by the predecessor, and cache hits are established.
  }

  /**
* fresh ask asks once to import the cache (amend-resume) during admission. Hits **cached settle** and returns true
* (The caller is no longer live); judgment and disagreement accounting are all in {@link ImportedActorState}.
   *
* Write a line if hit **true** dwf_node, only send `node-settled cached:true` - hit with replay
* {@link releaseCachedAsk} The same attitude: no node-queued / node-dispatched,
* Does not enter liveQueue, does not create a session, and does not occupy `actor.current`.
   */
  private tryImportedSettle(
    instance: InstanceRef,
    actor: Actor,
    seq: number,
    hash: string,
    deferred: Deferred<unknown>,
  ): boolean {
// Caching is turned off ⇒ Only put **pure** ask (precursor notes toolCalls === 0): it only relies on the command and transcription prefix, and the workspace
// irrelevant, so the answer to the old world is also true to the new world; entries with tools are also transferred to live - even if they have the same hash - it has read
// The workspace may have been overwritten. The ask that transferred live makes the actor different (the transcription will be different from now on), and the consumed prefix is ​​still the seed
// The basis for the boundary.
    const entry = this.host.importCacheClosed()
      ? actor.imported?.takeIfPure(seq, hash)
      : actor.imported?.take(seq, hash);
    if (entry === undefined) return false;
    this.journal.putNode(importedAskRecord(this.host.runId, instance, actor.ref, seq, hash, entry));
    this.host.record({ type: "node-settled", instance, outcome: "ok", cached: true });
    deferred.resolve(entry.result);
    return true;
  }

// ————————————————————————————————— Return upwards————————————————————————————————

/** submit_result arrives: The method body is handleSubmitAttempted in scheduler-submit.ts. */
  submitAttempted(instance: InstanceRef, payload: unknown): void {
    handleSubmitAttempted(this.submitSeam, instance, payload);
  }

/** Turn ends (without submit): The method body is in handleTurnEnded of scheduler-submit.ts. */
  turnEnded(instance: InstanceRef, finalText: string): void {
    handleTurnEnded(this.submitSeam, instance, finalText);
  }

  noteStats(instance: InstanceRef, stats: AskStats): void {
    const node = this.liveNodes.get(refToString(instance));
    if (node !== undefined) {
      node.lastStats = stats;
      return;
    }
// The node has left liveNodes - typed-accept dominant path: submit stops turn and is dropped when settling, while the real
//The actor usage is not known until the turn is parsed (after submit), so stats arrive after settlement. Backfill settled
// Journal record: only add/overwrite stats, retain status/result/actorSeq/inputHash/error/kind/actor identity.
// Best effort and idempotent; budget deductions are still in engine.askStats (only journal integrity is supplemented here).
    const recorded = this.journal.getNode(this.host.runId, instance.siteId, instance.ordinal);
    if (recorded === undefined) return;
    this.journal.putNode({ ...recorded, stats });
  }

  failed(instance: InstanceRef, error: WorkflowError): void {
    const node = this.liveNodes.get(refToString(instance));
    if (node === undefined || node.settled) return;
    this.settleFailed(node, error);
  }

/** Whether the instance is still flying (admitted, unsettled) live ask - current limiting observation events are only meaningful for such nodes. */
  isLive(instance: InstanceRef): boolean {
    const node = this.liveNodes.get(refToString(instance));
    return node !== undefined && !node.settled;
  }

/** The valid name of the subagent to which ask belongs when in flight (used for naming in the closing event); when not in flight, it is undefined. */
  liveActorName(instance: InstanceRef): string | undefined {
    const node = this.liveNodes.get(refToString(instance));
    return node === undefined || node.settled ? undefined : node.actor.persona.name;
  }

/** Abort all in-flight ask:run when canceling/failing. Reissue node-settled(cancelled) when emitCancelled is true. */
  abortInFlight(error: WorkflowError, emitCancelled: boolean): void {
    for (const node of this.liveNodes.values()) {
      if (node.settled) continue;
      node.settled = true;
      if (node.dispatched) this.host.driver.cancelAsk(node.instance);
      if (emitCancelled) {
        this.host.record({ type: "node-settled", instance: node.instance, outcome: "cancelled" });
      }
      node.deferred.reject(error);
    }
    this.liveNodes.clear();
    this.activeAsks = 0;
  }

// ————————————————————————————————— Internal: Admission/Distribution —————————————————————————————

  private drainAdmission(actor: Actor): void {
    let progressed = true;
    while (progressed) {
      progressed = false;
      const release = actor.pendingRecorded.get(actor.nextAdmitSeq);
      if (release !== undefined) {
        actor.pendingRecorded.delete(actor.nextAdmitSeq);
        actor.nextAdmitSeq++;
        release();
        progressed = true;
        continue;
      }
      if (actor.nextAdmitSeq >= actor.recordedCount && actor.pendingLive.length > 0) {
        const admit = actor.pendingLive.shift()!;
        admit();
        progressed = true;
      }
    }
    this.pumpActor(actor);
  }

  private releaseCachedAsk(instance: InstanceRef, recorded: NodeRecord, deferred: Deferred<unknown>): void {
    if (recorded.status === "completed") {
      this.host.record({ type: "node-settled", instance, outcome: "ok", cached: true });
      deferred.resolve(recorded.result);
    } else {
// Recorded failures are also subject to short-circuit replay: the script may have try/catched it and branched accordingly, and replay must replay the same rejection.
      this.host.record({ type: "node-settled", instance, outcome: "failed", cached: true, error: recorded.error });
      deferred.reject(WorkflowError.fromJSON(recorded.error!));
    }
  }

  private pumpAll(): void {
    for (const actor of this.actorOrder) this.pumpActor(actor);
  }

  private pumpActor(actor: Actor): void {
    if (this.host.isRunSettled()) return;
    if (actor.current !== undefined) return;
    if (actor.liveQueue.length === 0) return;
    if (this.activeAsks >= this.host.caps.maxConcurrency) return;
    const node = actor.liveQueue.shift()!;
    actor.current = node;
    this.activeAsks++;
    void this.dispatch(node);
  }

  private async dispatch(node: AskNode): Promise<void> {
    let session: SessionRef;
    try {
      session = await this.ensureSession(node.actor);
    } catch (cause) {
      if (this.host.isRunSettled() || node.settled) return;
//Bring the text of cause into message: WorkflowError.toJSON only falls into code/message, cause does not enter journal either
// No one keeps logs, so "Failed to create actor session" becomes an undiagnosable black box in the GUI/journal
// (The bottom layer on the real machine is actually the FOREIGN KEY constraint failed of session_task_link).
      this.settleFailed(
        node,
        new WorkflowError(
          "DriverError",
          `Failed to create the subagent session: ${describeCause(cause)}`,
          { cause },
        ),
      );
      return;
    }
    if (this.host.isRunSettled() || node.settled) return;
// node-dispatched after the session is ready. Process level concurrency gate
// Not here: it is admitted by **model request** and lives in the runtime deps under the driver; the scheduler only
// per-run ask-level upper bound.
    this.host.record({ type: "node-dispatched", instance: node.instance });
    node.dispatched = true;
    const message: AskMessage = {
      instructions: node.instructions,
      typed: node.spec.typed,
      schema: node.spec.schema,
    };
    this.host.driver.startAsk(session, node.instance, message);
  }

  private ensureSession(actor: Actor): Promise<SessionRef> {
    if (actor.sessionPromise !== undefined) return actor.sessionPromise;
// The seed is only known at the divergence point (discovered during runtime), so it must be handed over to the driver by the engine here - the engine holds it
//In the import state, the driver holds the session store. This signature is the minimum convergence point of the two.
    const seed = actor.imported?.seed();
    const promise = this.host.driver.createActorSession(actor.ref, actor.persona, seed).then((session) => {
      actor.session = session;
// resolvedModel is written **inside** createActorSession by the runtime factory on the host side (it only knows
// Which model "lite" falls into). putActor is the replacement of the entire record, so this write must read back the value just written
// Bring it over, otherwise it will be erased here - the audit and resume basis for gear parsing will be lost.
      const resolvedModel = this.journal.getActor(
        this.host.runId,
        actor.ref.siteId,
        actor.ref.ordinal,
      )?.resolvedModel;
      this.journal.putActor({
        runId: this.host.runId,
        siteId: actor.ref.siteId,
        ordinal: actor.ref.ordinal,
        name: actor.name,
        persona: actor.persona,
        sessionId: session.id,
        resolvedModel,
      });
      return session;
    });
    actor.sessionPromise = promise;
    return promise;
  }

// ——————————————————————————————— Internal: Settlement ——————————————————————————————

  private settleOk(node: AskNode, artifact: unknown): void {
    if (node.settled) return;
    node.settled = true;
    this.journal.putNode(this.nodeRecordFor(node, { status: "completed", result: artifact }));
    this.host.record({ type: "node-settled", instance: node.instance, outcome: "ok" });
    this.finishLiveNode(node);
    node.deferred.resolve(artifact);
  }

  private settleFailed(node: AskNode, error: WorkflowError): void {
    if (node.settled) return;
    node.settled = true;
// Settlement failure must fall into the journal (overriding running during admission): failure is "complete", and the script may have observed the rejection
// and branch accordingly, replay must reproduce it - journalization failure is a requirement for replay correctness, not optional.
    this.journal.putNode(this.nodeRecordFor(node, { status: "failed", error: error.toJSON() }));
    this.host.record({ type: "node-settled", instance: node.instance, outcome: "failed", error: error.toJSON() });
    this.finishLiveNode(node);
// If the node fails, only the ask will be rejected, and the entire run will not fail (the script can be try/catch).
    node.deferred.reject(error);
  }

  private finishLiveNode(node: AskNode): void {
    this.liveNodes.delete(refToString(node.instance));
    if (node.actor.current === node) {
      node.actor.current = undefined;
      this.activeAsks--;
    }
    this.pumpAll();
  }

  private nodeRecordFor(
    node: AskNode,
    outcome: { status: "completed"; result: unknown } | { status: "failed"; error: NodeRecord["error"] },
  ): NodeRecord {
    const record: NodeRecord = {
      runId: this.host.runId,
      siteId: node.instance.siteId,
      ordinal: node.instance.ordinal,
      kind: "ask",
      actorSiteId: node.actor.ref.siteId,
      actorOrdinal: node.actor.ref.ordinal,
      actorSeq: node.actorSeq,
      inputHash: node.hash,
      status: outcome.status,
    };
    if (outcome.status === "completed") record.result = outcome.result;
    else record.error = outcome.error;
    if (node.lastStats !== undefined) record.stats = node.lastStats;
    return record;
  }
}

/** replay hits but inputHash is inconsistent - purity contract broken, run fails loudly. */
export function hashMismatch(instance: InstanceRef, expected: string, got: string): WorkflowError {
  return new WorkflowError(
    "InputHashMismatch",
    `Replay hit at ${refToString(instance)} but inputHash differs (expected ${expected}, got ` +
      `${got}): the script is not deterministic, so the journal cannot be replayed.`,
// Structured mismatch aligns with ScriptHashMismatch: two hash inconsistency errors share the same field,
// The reader no longer has to extract the hash from the message text.
    { mismatch: { expected, got } },
  );
}

/** cause → A line of bounded text (Error takes message, the rest is String(); if empty, placeholder is given). */
function describeCause(cause: unknown): string {
  const text = cause instanceof Error ? cause.message : String(cause);
  const trimmed = text.trim();
  if (trimmed.length === 0) return "unknown error";
  return trimmed.length > 300 ? `${trimmed.slice(0, 300)}…` : trimmed;
}

/**
* The beginning of the author directive ({@link INSTRUCTIONS_HEAD_MAX_CHARS} characters, stripped of whitespace, **without ellipses**).
* Empty commands return undefined: an absent key is more honest than an empty string - the reader returns "I don't know what it was told" accordingly.
 */
function headOfInstructions(instructions: string): string | undefined {
  const trimmed = instructions.trim();
  if (trimmed.length === 0) return undefined;
  return trimmed.length <= INSTRUCTIONS_HEAD_MAX_CHARS
    ? trimmed
    : trimmed.slice(0, INSTRUCTIONS_HEAD_MAX_CHARS);
}
