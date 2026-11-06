/**
* Deterministic state machine for the execution engine (implementation of Boundary A + downward drive to Boundary B + upward return sink).
 *
* The core does not do any I/O, does not read the clock, and does not use random numbers: decisions that change with time/scheduling are either recorded by the journal or
* The set of deterministic rules "per-site sequence number + actorSeq per actor FIFO" is fixed so that the first execution is verbatim with the replay.
 *
*Key invariants:
* - Instance identity = site id × execution sequence number of each site (nth ask#3 = ask#3@n). Actor identity = creation site × serial number.
* - journal decision: check (siteId, ordinal) first for each host call; if hit, it will be short-circuited (no driver call) and defensive
* Verify inputHash - if inconsistent, the entire run will fail loudly (structural error, never silent offset).
* - Per-actor FIFO + actorSeq admission order + hold rules for replay: see scheduler.ts (this file delegates the ask lifecycle to it).
 *
* This document focuses on the run life cycle: host API entry, usage accounting, run settlement and event recording. User interface products, reports,
* The method bodies of the world node, import cache, and run final state are each in the sibling module (engine-artifacts.ts / engine-report.ts /
* engine-world.ts / engine-settlement.ts), read and write here through the {@link EngineState} seam of engine-state.ts
* Private state; only thin delegates are left on this class (reason for splitting: oxlint max-lines limit of 400 lines).
 */

import { ImportedWorldQueue, matchImportedActor } from "./imported-cache.js";
import type { ArtifactContentOp, ArtifactPresetOp } from "../facade/registry.js";
import { AskScheduler, type SchedulerHost } from "./scheduler.js";
import type { ArtifactIdState, EngineState, RunSettlement } from "./engine-state.js";
import {
  declarePresetArtifact,
  publishContentArtifact,
  rememberArtifactRow,
} from "./engine-artifacts.js";
import { publishReport } from "./engine-report.js";
import { closeImportCache, readWorld, recoverImportClosure } from "./engine-world.js";
import { settleCompleted, settleFailed, settleStopped } from "./engine-settlement.js";
import type {
  ActorId,
  ArtifactRef,
  AskProgress,
  AskSpec,
  AskStats,
  AskWaitInfo,
  Caps,
  ConcurrencyChange,
  ImportedRunCache,
  InstanceRef,
  JournalStorePort,
  PersonaSpec,
  RunEvent,
  RunStallInfo,
  RunStatus,
  RunStopReason,
  ValidateFn,
  WorkflowDriver,
  WorkflowHostApi,
  WorkflowReportSink,
  WorldReadOp,
} from "./types.js";
import { refToString, WorkflowError } from "./types.js";
import { runLaunchedEvent, type RunLaunchConfig } from "./engine-launch.js";

/** Engine construction configuration. */
export interface EngineConfig {
  runId: string;
  driver: WorkflowDriver;
  caps: Caps;
  /**
* Static specification (typed + schema) per ask site. **Must cover every ask site in the script**——
* The site table and schema synthesis come from the same compilation, so the absence can only be a wiring error, and the engine handles it as a hard error
* (MissingAskSpec). Untyped sites should be explicitly marked as `{ typed: false }`.
   */
  askSpecs: ReadonlyMap<string, AskSpec>;
/** Injected schema validator (core does not import schema implementation). */
  validate: ValidateFn;
/** run metadata (dropped into dwf_run, only written during this createRun; records will not be overwritten during resume). */
  scriptText?: string;
  /**
* Display name for run (optional `input.name` for `CreateWorkflow`). The engine does not read it, only when building run
* `scriptText` is dropped into the library together - the host's enumeration labels the run according to it. See {@link RunRecord.name}.
   */
  name?: string;
  /**
* Hash of script text. Compare it with the value in the journal record when resume: If both sides are present and different, this resume will be rejected.
* (V1 resume is only effective for scripts that are byte-for-byte identical).
   */
  scriptHash?: string;
  /**
* Actual parameters of this run (verified and backfilled). The engine does not read it and only drops it with `scriptText` when building the run;
* The injection on the sandbox side uses the spawn payload of the harness without going through the engine. See {@link RunRecord.args}.
   */
  args?: Record<string, unknown>;
  parentSessionId?: string;
  cwd?: string;
  /**
* The CreateWorkflow tool call id that initiated the run (see {@link RunRecord.toolCallId}).
* The engine does not read it, it is just dropped with the rest of the metadata during createRun.
   */
  toolCallId?: string;
  /**
* Which predecessor run this run was revised from (see {@link RunRecord.resumedFrom}). The engine doesn't read it,
* Only drop the library along with the rest of the metadata at createRun - the import cache is built in the run service, not in the core.
   */
  resumedFrom?: string;
  /**
* The anchor point of the round where the run is initiated. The engine doesn't read it and only builds run that life
* Immediately after the first `run-started`, remember a `run-launched`; when resume hits an existing row, it will not be remembered (the anchor point is unique across the life cycle).
* `phaseNames` follows the anchor point: the phase table declared by the script is also not read by the engine, but only dropped into the journal. `phaseAlongside`
* Align with it by position (the subscript points to the same table), the same car and the same rules.
* `subagentModel` is also the same: the selection of this run subagent (standard picker string), the engine also does not read - the entire model surface
* On the host side (bootstrap's workflow-actor-model.ts), the host reads it back from this event, zero SQL.
* `scriptPath` is the same as the same car: which file (absolute path) the script of this run comes from, the engine does not read it, and the host starts from this event
* Read it back to the model surface.
   */
  launch?: RunLaunchConfig;
  /**
* The starting point for usage when building a run: the `spentTokens` of the predecessor run. amend path given, new submit absent (= starting from scratch),
* The resume path is useless - when an existing row is hit, the usage is restored from the row.
   *
* The semantics is "this run reports the cost of the entire lineage": the number of each predecessor is already a cumulative value, so the chain revision
* Sum by construction, no one needs to walk the `resumed_from` chain. No more money will be added if the cache is hit (the account is in this inherited value),
* Only the current live turn will be added.
   */
  inheritedTokens?: number;
  /**
* Import cache for amend-resume ({@link ImportedRunCache}). **Pure Data Injection** - The core therefore remains
* Zero I/O deterministic state machine: reading the predecessor journal, walking the `resumed_from` chain, and parsing the transcription source, all occur in
* run service, the engine only gets a built table and sorts it according to the runtime identity (actor name + persona,
* `{op,args}` content + appearance order) comparison. Absence means that this is not a revised continuation.
   */
  importedCache?: ImportedRunCache;
}

/** The final settlement of run (the definition is moved to engine-state.ts along with the settlement module, and then exported here). */
export type { RunSettlement } from "./engine-state.js";

interface SettledDeferred {
  promise: Promise<RunSettlement>;
  resolve: (value: RunSettlement) => void;
}

export class WorkflowEngine implements WorkflowHostApi, WorkflowReportSink {
  private readonly runId: string;
  private readonly driver: WorkflowDriver;
  private readonly journal: JournalStorePort;
  private readonly caps: Caps;
  private readonly askSpecs: ReadonlyMap<string, AskSpec>;
  private readonly validate: ValidateFn;
  private readonly scheduler: AskScheduler;

/** Per-site execution sequence number counter (shared by ask/world-read/actor, site ids are different from each other). */
  private readonly ordinals = new Map<string, number>();
  /**
*Number of stage entries, by name. **Not with**
* `ordinals` table: the stage name is an arbitrary string by the author, a stage that happens to be called `report#1` cannot move that
* The node number of the site (site-id stability) and vice versa.
   */
  private readonly phaseOrdinals = new Map<string, number>();
  /**
* The name of the phase the control flow is currently in (maintained by `enterPhase`; the first token was previously undefined). Only used at casting points
* Read - see {@link nextOrdinal}.
   */
  private currentPhase: string | undefined;
  /**
* instance(`siteId@ordinal`) → the stage name at which it was **born**. Stamping funnel written at casting point, read at {@link record}: Static cause and effect diagram copied by stages
* Site, runtime instances must have the same coordinates, otherwise the instances in one lane will be copied by every stage of that site
* Simultaneous claims (shape of this bug: 20 subagents in each of the five stages, 100 displayed on each of the five cards).
   */
  private readonly instancePhases = new Map<string, string>();

  /**
* The **non-null valid actor name** that has been claimed in this run → the actor who claimed it (used to refer to the previous place in the failure message).
* See the security instructions for duplication checking and replay of {@link createActor}.
   */
  private readonly actorNames = new Map<string, string>();

/** The import cache of amend-resume (pure data, absent means this is not a revised continuation). */
  private readonly importedCache?: ImportedRunCache;
/** World imports the consumption cursor of the queue (nth occurrence vs. nth item). */
  private readonly importedWorld: ImportedWorldQueue;
  /**
* Whether the import cache is **closed**: set true before the first write and never reopen
* ——A live subagent is about to execute the rewriting tool (the driver reports askMutating), or a world.run live execution.
* After closing, the world node and ask with tool no longer ask about the import table (the hash only compares text, and cannot compare that the workspace has been rewritten);
* Pure ask (toolCalls 0) hits as usual. Recover from the `import-cache-closed` event when resume, see {@link recoverImportClosure}.
   */
  private importClosed = false;
/** The "ask instance that was live before the crash" (`siteId@ordinal`) recovered from the event during resume; non-resume is empty. */
  private liveAskInstances: ReadonlySet<string> = new Set();

  /**
* The number of reports published by this run (counter of REPORT_CAPS.maxItemsPerRun). Press journal when resume
* Kind: "report" row count recovery - the upper limit is run level, and must be counted continuously across resumes, otherwise a repeat
* resume's run can report infinitely.
   */
  private reportCount = 0;
  /**
* The status of each **user interface product** id of this run. with `reportCount`
* The same recovery method: rebuild from the `kind: "artifact"` line of the journal during resume, and then maintain it in memory——
* The upper limit (32 ids / 16 versions per id) and the fact that the version numbers are both run-level, must be continuous across resumes, otherwise one
* Repeated resume runs can be released indefinitely.
   *
* ⚠ Terminology: artifact = user surface artifact, not `RunSettlement.artifact` (top-level return value).
   */
  private readonly artifacts = new Map<string, ArtifactIdState>();
/** Cumulative token usage (observation side: only accounting, only broadcasting, never let the run fail). */
  private spentTokens = 0;
  private runSettled = false;
  private runFailure?: WorkflowError;

  private readonly settledDeferred: SettledDeferred;
/** The seam between sibling module free functions for reading and writing private states (configured with arrow closures in the constructor, see engine-state.ts). */
  private readonly state: EngineState;

  constructor(config: EngineConfig) {
    this.runId = config.runId;
    this.driver = config.driver;
    this.journal = config.driver.journal;
    this.caps = config.caps;
    this.askSpecs = config.askSpecs;
    this.validate = config.validate;
    this.importedCache = config.importedCache;
    this.importedWorld = new ImportedWorldQueue(config.importedCache?.world ?? new Map());

    let resolve!: (value: RunSettlement) => void;
    const promise = new Promise<RunSettlement>((res) => {
      resolve = res;
    });
    this.settledDeferred = { promise, resolve };

    this.state = {
      runId: this.runId,
      driver: this.driver,
      journal: this.journal,
      artifacts: this.artifacts,
      importedCache: this.importedCache,
      importedWorld: this.importedWorld,
      isRunSettled: () => this.runSettled,
      runError: () => this.runError(),
      failRun: (error) => this.failRun(error),
      record: (event) => this.record(event),
      nextOrdinal: (siteId) => this.nextOrdinal(siteId),
      reportCount: () => this.reportCount,
      countReport: () => {
        this.reportCount++;
      },
      importClosed: () => this.importClosed,
      closeImport: () => {
        this.importClosed = true;
      },
      markSettled: (failure) => {
        this.runSettled = true;
        if (failure !== undefined) this.runFailure = failure;
      },
      abortInFlight: (error, emitCancelled) => this.scheduler.abortInFlight(error, emitCancelled),
      resolveSettled: (settlement) => this.settledDeferred.resolve(settlement),
    };

    const host: SchedulerHost = {
      runId: this.runId,
      caps: this.caps,
      driver: this.driver,
      validate: this.validate,
      nextOrdinal: (siteId) => this.nextOrdinal(siteId),
      record: (event) => this.record(event),
      isRunSettled: () => this.runSettled,
      runError: () => this.runError(),
      failRun: (error) => this.failRun(error),
      importCacheClosed: () => this.importClosed,
      wasLiveBeforeResume: (instance) => this.liveAskInstances.has(refToString(instance)),
    };
    this.scheduler = new AskScheduler(host);

    const existing = this.journal.getRun(this.runId);
    if (existing === undefined) {
//Revise run to start accounting from the cumulative value of the predecessor;
// Brand new run is 0 if absent. The normalization (non-finite values ​​and negative numbers are rounded by absence and decimal) is here: this number must fall
// `spent_tokens` column, the protocol schema requires non-negative integers, and it is pure data beyond the harness boundary - a NaN
// It will poison the column value and the projected patch at the same time, so the dirty value must die before writing to the library, rather than dying in the projected patch verification.
      const inherited = config.inheritedTokens ?? 0;
      this.spentTokens = Number.isFinite(inherited) ? Math.max(0, Math.floor(inherited)) : 0;
      this.journal.createRun({
        runId: this.runId,
        caps: this.caps,
        spentTokens: this.spentTokens,
        status: "running",
// Metadata is only saved when creating a run; the incoming script and session fields are saved for recovery and script_hash verification.
// name is the display metadata later added to the same path. Default fields remain absent (undefined keys are not implemented).
        ...(config.scriptText === undefined ? {} : { scriptText: config.scriptText }),
// Actually participates in the same metadata path of scriptText, and is only written once when building run: resume reads back from here
// Replay, never accept new arguments given by the caller.
        ...(config.args === undefined ? {} : { args: config.args }),
        ...(config.name === undefined ? {} : { name: config.name }),
        ...(config.scriptHash === undefined ? {} : { scriptHash: config.scriptHash }),
        ...(config.parentSessionId === undefined
          ? {}
          : { parentSessionId: config.parentSessionId }),
        ...(config.cwd === undefined ? {} : { cwd: config.cwd }),
        ...(config.toolCallId === undefined ? {} : { toolCallId: config.toolCallId }),
// lineage is also only written once when building a run: the revision is supersede (new run), and the predecessor line is zero-touched.
        ...(config.resumedFrom === undefined ? {} : { resumedFrom: config.resumedFrom }),
      });
    } else {
// Preconditions for resume: The script must be the same byte by byte as when the run was created. Hash
// Inconsistency means that the caller reused the same runId with another script - the site id is the key of the journal, continue
// The site of the new script will be matched with the results of the old script. Reject **this resume** (thrown synchronously) instead of accepting it first
// Then failRun: The latter will cover failed to a record that can still be resumed with the correct script.
      if (
        existing.scriptHash !== undefined &&
        config.scriptHash !== undefined &&
        existing.scriptHash !== config.scriptHash
      ) {
        throw new WorkflowError(
          "ScriptHashMismatch",
          `Cannot resume run ${this.runId}: its script changed (recorded hash ${existing.scriptHash}, ` +
            `got ${config.scriptHash}). Resume needs the byte-identical script; to run a revised ` +
            `script, start a new run with resume_from instead.`,
          { mismatch: { expected: existing.scriptHash, got: config.scriptHash } },
        );
      }
//resume: The report count is resumed according to the number of rows of kind:"report" in the journal; the usage is resumed from the record (continuous across the life cycle).
      const nodes = this.journal.listNodes(this.runId);
      this.reportCount = nodes.filter((n) => n.kind === "report").length;
// Product status and reportCount are restored together: id ownership (type, preset spec) and number of successful versions are all determined by
// The journal line is derived, so version 3 after crash recovery is still version 3, rather than counting back from 1.
      for (const node of nodes) {
        if (node.kind !== "artifact" || node.status !== "completed") continue;
        rememberArtifactRow(this.state, node.artifactId, node.result);
      }
      this.spentTokens = existing.spentTokens;
// Revise the crash recovery of run: the import table is completely rebuilt, and "whether the door is closed" is not dropped into the database - accurate recovery from the event.
      if (this.importedCache !== undefined) {
        const recovered = recoverImportClosure(this.journal, this.runId);
        this.liveAskInstances = recovered.live;
        this.importClosed = recovered.closed;
      }
      this.journal.updateRunStatus(this.runId, "running");
    }
    this.record({ type: "run-started", runId: this.runId, caps: this.caps });
//The anchor point is remembered only when the run is created, and placed after run-started: the projection treats run-started as an entry creation event.
// The anchor event just raises the water level for it (the default branch of the reducer).
    if (existing === undefined && config.launch !== undefined) {
      this.record(runLaunchedEvent(config.launch, config));
    }
// The usage from amend or resume is reissued here: projection treats `run-started` as
// "Usage reset" (workflow-runs-started.ts peels off the settlement residual image of the previous life), so if it is not reissued, the card will be
// The first live turn displays 0 before landing - and "this run has not been spent yet" is false. I don't have any live ask when
// Worse: the projection will stay at 0 until the cold replay matches the upstream value.
// Zero is not sent: reset itself has already said zero, and sending another one is a noise event that must be paid for every new run.
    if (this.spentTokens > 0) this.record({ type: "usage-updated", spentTokens: this.spentTokens });
  }

/** run settlement promise: fulfilled on completion/failure/cancellation. */
  get settled(): Promise<RunSettlement> {
    return this.settledDeferred.promise;
  }

/** Current run status. */
  status(): RunStatus {
    return this.journal.getRun(this.runId)?.status ?? "running";
  }

  // ——————————————————————————————— Boundary A ———————————————————————————————

  /**
* Build an actor. **Valid name** is `normalizePersona` followed by `spec.name` (persona.name trumps name
* Actual parameter), if it is not empty, it must be unique within this run - duplicate names will cause the entire run to fail loudly (DuplicateActorName).
   *
* Why is it a run-level failure instead of a rename/warning: named actors are amend-resume cached imported identity keys, and any
* run are all potential precursors for future revisions. Duplicate names in the precursors will cause import matching ambiguity.
* The rule therefore takes effect for all runs, not just revision runs. Anonymous (absent or empty string) never checked, any number of legal -
* Price adjudicated: Not eligible for caching.
   *
* **replay security**: The duplication check table is purely memory-based and is generated with the engine instance. byte-for-byte resume of the same script in a **new**
* Rerun the same series of createActor calls from the beginning in the engine instance, so each name is registered exactly once, and the previous round will not be changed.
* Your own registration will be treated as a duplicate name. This is also the reason why it cannot be dropped into the journal - the dropped table will collide with itself during resume.
   *
* **The attachment of the imported cache is also here** (amend-resume): check `importedCache.actors` by effective name,
* After the persona is standardized, the candidate will be linked to the actor state of the scheduler. The reason why the comparison is done during **runtime** instead of
* Static comparison of two scripts when submitting: name and persona are both run-time values ​​(the actual parameters of `agent()` can be dynamic expressions),
* Static comparison is the second truth, which is what this package needs to prevent (see the `matchImportedActor` of imported-cache.ts for judgment).
   */
  createActor(siteId: string, name?: string, persona?: string | PersonaSpec): ActorId {
    this.assertRunning("createActor");
    const ordinal = this.nextOrdinal(siteId);
    const ref = { siteId, ordinal };
    const id = refToString(ref);
    const spec = normalizePersona(name, persona);
    const effectiveName = spec.name;
    if (effectiveName !== undefined && effectiveName !== "") {
      const claimed = this.actorNames.get(effectiveName);
      if (claimed !== undefined) {
// assertRunning synchronization surface with the same attitude: createActor has no promise to reject, it can only throw it.
        const err = new WorkflowError(
          "DuplicateActorName",
          `Subagent name "${effectiveName}" is used twice in this run (${claimed} and ${id}). ` +
            `A named subagent is the identity an amended re-run matches its cache by, so names must ` +
            `be unique; give this one its own name or drop the name to make it anonymous.`,
        );
        this.failRun(err);
        throw err;
      }
      this.actorNames.set(effectiveName, id);
    }
    this.scheduler.registerActor(ref, id, name, spec, matchImportedActor(this.importedCache, spec));
// putActor is the replacement of the entire record, and sessionId / resolvedModel is the field owned by **driver**
// (The former is written by ensureSession, and the latter is written by the runtime factory on the host side). Here you must bring the existing value as it is.
// Otherwise, when replay hits the same (siteId, ordinal), both will be erased.
    const existing = this.journal.getActor(this.runId, siteId, ordinal);
    this.journal.putActor({
      runId: this.runId,
      siteId,
      ordinal,
      name,
      persona: spec,
      sessionId: existing?.sessionId,
      resolvedModel: existing?.resolvedModel,
    });
    this.record({ type: "actor-created", actor: ref, name, persona: spec });
    return id;
  }

  ask(siteId: string, actorId: ActorId, instructions: string): Promise<unknown> {
    if (this.runSettled) return Promise.reject(this.runError());
    if (!this.scheduler.hasActor(actorId)) {
// Unknown actor: The script/lowering contract is broken and the entire run fails loudly.
      const err = new WorkflowError("UnknownActor", `Unknown subagent handle: ${actorId}.`);
      this.failRun(err);
      return Promise.reject(err);
    }
    const spec = this.askSpecs.get(siteId);
    if (spec === undefined) {
// The site table and schema are synthesized from the same compilation, so the miss can only be a wiring error (the two compilation products are
// put together). `?? { typed: false }` will silently downgrade typed ask to untyped:
//Do not register submit_result, use the last round of text as the result, and the schema verification disappears - a downgrade to
// A type system that "results are always valid" is worse than no type system at all, so it's better to have the entire run fail loudly.
      const err = new WorkflowError(
        "MissingAskSpec",
        `Ask site ${siteId} has no spec: askSpecs is incomplete (compile output mismatch).`,
      );
      this.failRun(err);
      return Promise.reject(err);
    }
    return this.scheduler.admitAsk(siteId, actorId, instructions, spec);
  }

/** world node (world-read / world-run): The method body is in readWorld of engine-world.ts. */
  worldRead(siteId: string, op: WorldReadOp, args: unknown[]): Promise<unknown> {
    return readWorld(this.state, siteId, op, args);
  }

  log(message: string): void {
    if (this.runSettled) return;
    this.record({ type: "log", message });
  }

  /**
* Control flow passes through a `phase("...")` marker (Boundary A's `enterPhase`). Same posture as `log`: synchronized, no return value,
* After settlement no-op, **do not drop journal line** - mark is not a site, there is no `dwf_node` to write.
   *
* ordinal counts by name, using its own table ({@link phaseOrdinals}): the phase name and site id do not consume each other's
* Serial number (site-id stability).
* **Each evaluation is sent**: The second round of `for { phase("B"); ask }` is the second entry of B; if "is the same as the current phase
* If it has the same name, it will not be issued", the round of the single-stage loop body will be lost. Resume will send the prefix again - we deliberately do not repeat it here.
* (Deduplication requires the engine to read the event table), the reducer uses `rounds = max(rounds, ordinal)` to monotonic reduction.
   */
  enterPhase(name: string): void {
    if (this.runSettled) return;
    const trimmed = name.trim();
    if (trimmed.length === 0) return;
    const ordinal = (this.phaseOrdinals.get(trimmed) ?? 0) + 1;
    this.phaseOrdinals.set(trimmed, ordinal);
// Subsequent instances of casting are all born in this stage.
    this.currentPhase = trimmed;
    this.record({ type: "phase-entered", name: trimmed, ordinal });
  }

/** Publish an intermediate result (`report` of Boundary A): the method body is in publishReport of engine-report.ts. */
  report(siteId: string, item: unknown, artifactId?: string): void {
    publishReport(this.state, siteId, item, artifactId);
  }

  /**
* Publish a **content artifact** (`artifact.file` / `artifact.markdown`): the method body is in engine-artifacts.ts
* publishContentArtifact.
   */
  publishArtifact(siteId: string, op: ArtifactContentOp, args: unknown[]): Promise<ArtifactRef> {
    return publishContentArtifact(this.state, siteId, op, args);
  }

  /**
* Declare a **preset product** (`artifact.chart` / `table` / `metrics` / `board`): the method body is in
* declarePresetArtifact in engine-artifacts.ts.
   */
  declareArtifact(siteId: string, op: ArtifactPresetOp, args: unknown[]): void {
    declarePresetArtifact(this.state, siteId, op, args);
  }

// ———————————————————————— Externally driven run life cycle ——————————————————————

/** The sandbox script successfully returns the top-level artifact, and the settlement is completed: the method body is settledCompleted in engine-settlement.ts. */
  complete(artifact: unknown): void {
    settleCompleted(this.state, artifact);
  }

  /**
* External stop: abort on the fly ask (deferred to Cancelled reject, reissue node-settled(cancelled)), run
* Settlement `stopped(reason)`; completed journal entries are retained (can be resumed). Four reasons to take the same path:
* `user` / `model` (initiator passed in from the cancel entry), `interrupted` (sandbox failure of harness),
* `provider` (deterministic model-side error reported by driver via `stopRun`). `error` is only present for the latter two.
* first-wins: Ignored after settlement. The method body is settledStopped in engine-settlement.ts.
   */
  stop(reason: RunStopReason, error?: WorkflowError, supersededBy?: string): void {
    settleStopped(this.state, reason, error, supersededBy);
  }

  /**
* External failure: the failure dual of complete. Harness uses it to subprocess `{complete, ok:false, error}` (script throws),
* Child process crash, wall clock timeout, NDJSON parsing failure, etc. are all classified as run failures.
* Instead of pretending to be cancel - otherwise the failure of the journal will diverge from the results seen by the caller.
* Take the internal failRun path: first-wins (for complete/cancel/internal failure), driver side cancel on the fly ask,
* Journal run status failed + failure_json, run-failure event is sent, settled is settled as failed.
   */
  fail(error: WorkflowError): void {
    this.failRun(error);
  }

// ——————————————————————————— Boundary B (Upward Return) ———————————————————————————

  askSubmitAttempted(instance: InstanceRef, payload: unknown): void {
    this.scheduler.submitAttempted(instance, payload);
  }

  askTurnEnded(instance: InstanceRef, finalText: string): void {
    this.scheduler.turnEnded(instance, finalText);
  }

  /**
* Progress observation within ask: **Pure passthrough**,
* Same attitude as askStats → usage-updated - no scheduling changes, no inputHash input, replay no comparison.
* In the same way as usage, late observations that arrive after settlement only lose events (there are no accounts to be entered here), and do not give long tails to completed runs.
   */
  askProgress(instance: InstanceRef, progress: AskProgress): void {
    if (this.runSettled) return;
    this.record({ type: "node-progress", instance, ...progress });
  }

  askStats(instance: InstanceRef, stats: AskStats): void {
    this.scheduler.noteStats(instance, stats);
    this.spentTokens += stats.tokens;
// Usage cannot be dropped to the database only once in createRun(0): cumulative non-writeback will cause resume to restore zero usage.
// Persist immediately after accumulation, and earlier than the event - the event payload and column value are generated in the same synchronization step, and they are always equal.
    this.journal.updateRunUsage(this.runId, this.spentTokens);
// The straggler stats arrived after settlement (the real actor usage is not known until turn parsing, the last
// ask's stats (always arrive later than complete/cancel) cannot send events as usual - events are appended after run-settled,
// Downstream projection is not expected (run-settled must be the last event stream). Only accounting after settlement (usage line + noteStats
// The node backfills are journal row updates, not events), no more events will be sent. The account still needs to be credited: the run usage that can be resumed
//Continuous across life cycles.
    if (this.runSettled) return;
    this.record({ type: "usage-updated", spentTokens: this.spentTokens });
  }

  askFailed(instance: InstanceRef, error: WorkflowError): void {
    this.scheduler.failed(instance, error);
  }

  /**
* Deterministic model-side error: Node is not settled,
* The entire run is stopped with `stopped(provider)`. Calls arriving after run has resolved are ignored by `stop`'s first-wins.
   */
  stopRun(error: WorkflowError): void {
    this.stop("provider", enrichProviderStopPhase(error, this.instancePhases));
  }

/** Run-level stall observation: record only; calls arriving after run has been resolved are ignored. */
  runStalled(info: RunStallInfo): void {
    if (this.runSettled) return;
    this.record({ type: "run-stalled", ...info });
  }

// Three pure observations of adaptive concurrency: the engine only records,
// Don't make any decisions based on them. run calls that arrive after the node has settled or the node has settled are ignored - that's a late observation,
// Dropping the journal will only cause a completed run to grow a tail.
  askWaiting(instance: InstanceRef, info: AskWaitInfo): void {
    if (this.runSettled || !this.scheduler.isLive(instance)) return;
    this.record({ type: "node-waiting", instance, ...info });
  }

  askExecuting(instance: InstanceRef): void {
    if (this.runSettled || !this.scheduler.isLive(instance)) return;
    this.record({ type: "node-executing", instance });
  }

  /**
* The only driver observation based on which the engine makes decisions: on the fly ask's subagent
* Workspace is about to be rewritten ⇒ Turn off import cache. Late observations (run or node resolved) are ignored, same pose as the two above.
   */
  askMutating(instance: InstanceRef): void {
    if (this.runSettled || !this.scheduler.isLive(instance)) return;
    closeImportCache(this.state, instance, "mutating-tool", this.scheduler.liveActorName(instance));
  }

  concurrencyChanged(change: ConcurrencyChange): void {
    if (this.runSettled) return;
    this.record({ type: "concurrency-changed", ...change });
  }

// ——————————————————————————————— Internal ——————————————————————————————

/** Run level failure (first-wins): The method body is settledFailed in engine-settlement.ts. */
  private failRun(error: WorkflowError): void {
    settleFailed(this.state, error);
  }

  /**
* The **unique** casting point for the site serial number (createActor/admitAsk first line/worldRead/report/artifact full
* synchronized through it), and therefore the only recording point of the instance birth phase: this line is always executed in the same tick of the script call,
* The event may not be - the `node-queued` of ask will be postponed by the hold rule until the next mark.
   */
  private nextOrdinal(siteId: string): number {
    const next = (this.ordinals.get(siteId) ?? 0) + 1;
    this.ordinals.set(siteId, next);
    if (this.currentPhase !== undefined)
      this.instancePhases.set(refToString({ siteId, ordinal: next }), this.currentPhase);
    return next;
  }

  private assertRunning(op: string): void {
    if (this.runSettled)
      throw (
        this.runFailure ??
        new WorkflowError("Cancelled", `Run already settled; ${op} is no longer accepted.`)
      );
  }

  private runError(): WorkflowError {
    return this.runFailure ?? new WorkflowError("Cancelled", "Run already settled.");
  }

  /**
* Each event is both dropped into the journal (dwf_event, persistent) and fanned out via driver.emit (Boundary C, real-time).
   *
* The only funnel for all events, the birth stage is also added here ({@link stampBirthPhase}). Got it via two routes
* Must be **the same object**: bootstrap's `createJournalSequenceCapture` checks sequence numbers according to reference equality.
   */
  private record(event: RunEvent): void {
    const stamped = this.stampBirthPhase(event);
    this.journal.appendEvent(this.runId, stamped);
    this.driver.emit(stamped);
  }

  /**
* Actor's `actor-created` looks up the table by actor,
* The `node-queued` of the node is looked up in the table by instance, and the `node-settled { cached: true }` that hits the cache is also pressed.
* instance——The hit node is not queued, and the settle is its birth event. The remaining events are returned unchanged:
* The ten emission points of the scheduler have been changed. The reducer follows the precedent of `actorSiteId` and carries it forward.
   */
  private stampBirthPhase(event: RunEvent): RunEvent {
    if (event.type === "actor-created") {
      const phaseName = this.instancePhases.get(refToString(event.actor));
      return phaseName === undefined ? event : { ...event, phaseName };
    }
    if (event.type === "node-queued") {
      const phaseName = this.instancePhases.get(refToString(event.instance));
      return phaseName === undefined ? event : { ...event, phaseName };
    }
    if (event.type === "node-settled" && event.cached === true) {
      const phaseName = this.instancePhases.get(refToString(event.instance));
      return phaseName === undefined ? event : { ...event, phaseName };
    }
    return event;
  }
}

/** persona normalization: the string is treated as system prompt; the display name falls to persona.name. */ function normalizePersona(
  name: string | undefined,
  persona: string | PersonaSpec | undefined,
): PersonaSpec {
  const base: PersonaSpec =
    typeof persona === "string" ? { system: persona } : persona ? { ...persona } : {};
  if (base.name === undefined && name !== undefined) base.name = name;
  return base;
}

/**
* Add the **birth phase** of the subagent that triggers the stop to `ProviderStop`: the driver only knows the actor ref, the phase is only known by the engine (`instancePhases`,
* The same table as `phaseName` on the event stream). There is no providerStop, no subagent, or the ref was born in
* Before any `phase()` mark → return unchanged.
 */
function enrichProviderStopPhase(
  error: WorkflowError,
  instancePhases: ReadonlyMap<string, string>,
): WorkflowError {
  const details = error.providerStop;
  if (details === undefined || details.subagent === undefined || details.phase !== undefined) {
    return error;
  }
  const phase = instancePhases.get(details.subagent);
  if (phase === undefined) return error;
  return new WorkflowError(error.code, error.message, {
    providerStop: { ...details, phase },
    cause: (error as { cause?: unknown }).cause,
  });
}
