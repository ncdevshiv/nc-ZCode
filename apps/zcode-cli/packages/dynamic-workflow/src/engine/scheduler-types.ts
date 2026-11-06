/**
* scheduler.ts reaches the upper limit of oxlint max-lines (400 lines), and changes the scheduler’s internal type (Deferred/
*AskNode / Actor) and the engine-injected dependency surface SchedulerHost are split into this file; the public surface is still exported from scheduler.ts
* (SchedulerHost is exported in place, and the import path of engine.ts remains unchanged).
 *
* The reason for separate files is not just the number of lines: the free functions in scheduler-submit.ts also need to get AskNode / SchedulerHost,
* Import from here, there is no need to reversely import the scheduler itself on both sides.
 */

import type { ImportedActorState } from "./imported-cache.js";
import type {
  ActorId,
  ActorRef,
  AskSpec,
  AskStats,
  Caps,
  InstanceRef,
  PersonaSpec,
  RunEvent,
  SessionRef,
  ValidateFn,
  WorkflowDriver,
  WorkflowError,
} from "./types.js";

/** A promise that can be settled externally. */
export interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (reason: unknown) => void;
}

export function defer<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** The dependency surface injected by the engine into the scheduler. */
export interface SchedulerHost {
  readonly runId: string;
  readonly caps: Caps;
  readonly driver: WorkflowDriver;
  readonly validate: ValidateFn;
/** Assigns the next execution sequence number of a site (shares a set of counters with world-read/actor). */
  nextOrdinal(siteId: string): number;
/** Events both fall into the journal and fan out (Boundary C). */
  record(event: RunEvent): void;
  isRunSettled(): boolean;
/** Error used to reject when run has been settled. */
  runError(): WorkflowError;
/** Run level failed. */
  failRun(error: WorkflowError): void;
  /**
* Whether the import cache is turned off (amend-resume). The engine closes the door by itself
* (The driver reports askMutating, or live's world-run), the scheduler only reads this bit - an ask turns live itself
* **Not** closed: it hasn't changed anything yet.
   */
  importCacheClosed(): boolean;
/** Whether the record line was run live before the crash (when resume, the engine resumes from the event; when not resume, it is always false). */
  wasLiveBeforeResume(instance: InstanceRef): boolean;
}

/** A live ask node (which needs to be actually dispatched and executed). */
export interface AskNode {
  instance: InstanceRef;
  actor: Actor;
  actorSeq: number;
  instructions: string;
  hash: string;
  spec: AskSpec;
  deferred: Deferred<unknown>;
  repairsRemaining: number;
  nudgesRemaining: number;
  settled: boolean;
  dispatched: boolean;
  lastStats?: AskStats;
}

/** Actor running state maintained by the scheduler. */
export interface Actor {
  ref: ActorRef;
  id: ActorId;
  persona: PersonaSpec;
  name?: string;
/** The number of ask nodes recorded by the actor in the journal - during replay, the live nodes must wait for all of them to be admitted before being released. */
  recordedCount: number;
/** The next actorSeq to be admitted. */
  nextAdmitSeq: number;
/** The recording node that has arrived but is not admitted releases the action and is suspended according to actorSeq (hold rule). */
  pendingRecorded: Map<number, () => void>;
/** Live node release actions that have arrived but are waiting for the recording node to be emptied, in order of arrival. */
  pendingLive: Array<() => void>;
/** The live nodes to be dispatched have been admitted (FIFO = admission order). */
  liveQueue: AskNode[];
/** The live node being executed (actor serial, at most one). */
  current?: AskNode;
/** Sessions are created lazily, caching their promises (once per actor). */
  sessionPromise?: Promise<SessionRef>;
  session?: SessionRef;
  /**
* Import consumption state of amend-resume (the engine hangs after matching by name + persona in createActor, see
* `matchImportedActor` of imported-cache.ts). If absent, the actor will be rerun.
   */
  imported?: ImportedActorState;
}
