/**
* Execution engine boundary types.
 *
* This module is a vocabulary of pure types + structural errors + constants, and all three boundaries are contracted by it:
* - Boundary A (host API): The facade shim of the sandbox script calls {@link WorkflowHostApi}.
* - Boundary B (driver port): The engine core (deterministic state machine) drives down {@link WorkflowDriver},
* The driver reports progress upward through {@link WorkflowReportSink}.
* - Journal: {@link JournalStorePort} exposes run/actor/node/event records in a warehousing method,
* The in-memory implementation will naturally fall into place with future SQLite implementations (the synchronization method fits node:sqlite's DatabaseSync and keeps the core deterministic).
 */

import type {
  ArtifactContentOp,
  ArtifactOp,
  ArtifactPresetOp,
  WorldReadOp,
} from "../facade/registry.js";
import type { AskProgress, AskStats } from "./ask-observation-types.js";

// ————————————————————————————————————————————————————————————————
// identity
// ————————————————————————————————————————————————————————————————

/**
* The identity of an ask instance: site id × execution sequence number (ordinal) of each site.
* The nth call to `ask("ask#3", …)` is the instance `ask#3@n` (ordinal starts from 1).
 */
export interface InstanceRef {
  siteId: string;
  ordinal: number;
}

/** Actor identity: creation site × sequence number (isomorphic to InstanceRef, but the semantics is actor instead of ask). */
export interface ActorRef {
  siteId: string;
  ordinal: number;
}

/**
* Opaque actor handle passed to the script. The `Agent` on the script side just holds a thin wrapper around it.
* The engine internally maps ActorId back to {@link ActorRef}; the format is invisible to scripts, but needs to be stable and parsable.
 */
export type ActorId = string;

/** Render InstanceRef / ActorRef to stable string `site@ordinal` for logs and handles. */
export function refToString(ref: InstanceRef | ActorRef): string {
  return `${ref.siteId}@${ref.ordinal}`;
}

// ————————————————————————————————————————————————————————————————
// persona / ask message
// ————————————————————————————————————————————————————————————————

/**
* Frozen actor persona, handed to {@link WorkflowDriver.createActorSession}.
 *
* persona only has identity (name + system prompt). Model level (`model?: "main" | "lite"`)
* and tool-level (`tools?: "default" | "readonly" | "none"`) have been deprecated: the host has no lite model source, and
* The only thing you can buy in the tool stall is "Referees cannot change files" - ordinary sub-agents don't rely on stalls to guarantee this, just make it clear in the ask text.
* Subagents always run on the current model of the parent session, with the complete working toolset minus interactive tools that will hang/override their authority.
* Whether to register submit_result is determined by the driver combined with the site map (fully untyped actors are not registered),
* Not expressed here - keep persona only describing identity.
 */
export interface PersonaSpec {
  name?: string;
  system?: string;
}

/**
* The message issued by a single ask: the command text + whether it is typed + the schema when typed (the structure is opaque to the core,
* Only transparently passed to {@link ValidateFn} and driver's schema endnote). schema is produced by the schema synthesis side,
* The core never interprets its shape.
 */
export interface AskMessage {
  instructions: string;
  typed: boolean;
  schema?: unknown;
}

// ————————————————————————————————————————————————————————————————
// validation
// ————————————————————————————————————————————————————————————————

/**
* A single verification violation is designed to be readable by the model in tool_result of repair: JSON path + expectation + actual result.
* Share the same type with the schema submodule (declared separately during parallel development and unified to schema/types after implementation).
 */
export type { Violation } from "../schema/types.js";
import type { Violation } from "../schema/types.js";

/**
* Injected verification function: `(schema, value) => violation list` (an empty list means passing). The real validator is another
* Parallel implementation of agents (src/schema/). The core only relies on this minimum contract and never imports its implementation.
 */
export type ValidateFn = (schema: unknown, value: unknown) => Violation[];

/** Static specifications for each ask site: whether it is typed and its schema. untyped site has no schema. */
export interface AskSpec {
  typed: boolean;
  schema?: unknown;
}

// ————————————————————————————————————————————————————————————————
// Statistics / Budget
// ————————————————————————————————————————————————————————————————

/** There are two reasons why the import cache is closed: subagent rewriting tool and live `world.run`. */
export type ImportCloseCause = "mutating-tool" | "world-run";

/**
* The capacity upper limit of the run level is fixed and stored in dwf_run when submitting; currently there is only the concurrency upper bound,
* Cancellation is the only control surface.
 */
export interface Caps {
  maxConcurrency: number;
}

// Structural details related to final states (ProviderStop details, run-level stop observations) live in run-terminal.ts, from here
// Re-export as is: this file has reached 400 lines, and their consumers are still fetched from types by convention.
import type { ProviderStopDetails, RunStallInfo } from "./run-terminal.js";

export type { ProviderStopDetails, RunStallInfo };

// ————————————————————————————————————————————————————————————————
// Structured errors (errors are first-class)
// ————————————————————————————————————————————————————————————————

/**
* Stable error code. Distinguish between node-level (rejecting the promise of a single ask) and run-level (failing the entire run):
* - node level: ValidationFailed / ResultNotSubmitted / DriverError / Cancelled / ContextLimit /
* WorldReadCapExceeded - A world read exceeds the upper limit of its op (`files.grep`: 2000 hits or
* 256KB serialized, first come first served; `git.diff`: 512KB; see `facade/world-read-caps.ts`). It is node level
* instead of run level, relying on **denied channel** rather than severity: world read returns a script that can `catch`
* promise, so "narrowing the pattern or adding a glob" is a real way for scripts to go. Also deliberately **not** folding in
* DriverError - The upper limit is that the script can rewrite its own contract, and the matching message text distinguishes the two,
* Exactly what this union type exists to prevent.
* - run level: InputHashMismatch / UnknownActor / MissingAskSpec /
* DuplicateActorName - createActor twice in the same run gets the same **non-null** effective name
* (effective name = `spec.name` after normalizePersona, persona.name overrides the name argument). rule pair
* **All** runs take effect not just revision runs: named actors are amend-resume cached imported identity keys, while any
* run are all potential precursors for future revisions. Duplicate names in the precursors will cause import matching ambiguity. Anonymous (name absent or empty string) is not checked or banned - the price is that it is not eligible for caching.
* There is a compile-time courtesy diagnosis (analysis/actor-names.ts) for duplicate literal names, but dynamic names can only be checked at runtime.
* So this is the real door.
* ReportCapExceeded - A run exceeds 256 reports, or a single item serialization exceeds 32KB
* (see `facade/report-caps.ts`). It is run level instead of node level, which is the same as WorldReadCapExceeded above.
* The demarcation is the same, but the conclusion is opposite: `report` returns `void`, the script has no catch channel, and there is nowhere to put it except run.
* It is for this reason that these two numbers must be so wide that a reasonable script will never touch them - the script author cannot write a recovery path.
* Also deliberately not folded into DriverError: The upper limit is the contract by which a script can rewrite itself.
* - Construction period (run has not started yet, the engine constructor throws synchronously): ScriptHashMismatch
* - Host level (**Engine never produces**): Interrupted - the process owning the run died before settlement, by the host
* The next time it is constructed, the row will always stop at the running record. It must be **standalone code** and not reused DriverError: script
* If you throw an error yourself, it will also be encoded as DriverError (`dynamic-workflow-runtime/src/harness.ts`). If the two have the same code,
* "The process was killed" and "the script really failed" can only be distinguished by the message text - and this is what this union type wants to avoid.
* ProviderStop - A subagent (or tool-side) model request hits a **deterministic** model-side error (authentication failure,
* The model is not in the package, the quota is exhausted...), the driver lets
* run is stopped with `stopped(provider)` instead of failing the node; structured details in `providerStop`. it is host level
* Another one: the engine is only dropped in `stop("provider", error)` as is.
* The code here will always be used for process judgment and will never match error text.
 */
export type WorkflowErrorCode =
  | "ValidationFailed"
  | "ResultNotSubmitted"
  | "DriverError"
  | "WorldReadCapExceeded"
  | "Cancelled"
  | "ContextLimit"
  | "ReportCapExceeded"
  | "InputHashMismatch"
  | "UnknownActor"
  | "MissingAskSpec"
  | "DuplicateActorName"
  | "ScriptHashMismatch"
  | "Interrupted"
  | "ProviderStop"
// —————————— User interface products ————————————
// ⚠ Terminology: This batch of artifacts are all **user-side products** (outputs released by the script for users to see), and
// `RunSettlement.artifact` (the top-level return value) is irrelevant.
  //
// The channel is split by **member family**, which is the same argument as the WorldReadCapExceeded/ReportCapExceeded boundary:
// Content members (`file`/`markdown`) return promise, the script can be caught, so it is rejected at the node level; preset members
// Returns void, there is nothing to reject in, so the same thing is true for failRun in that family. Three driver side
// Code (Missing/Outside/TooLarge/StoreUnavailable) can only come from content members, so it is always node level.
  | "ArtifactSourceMissing"
  | "ArtifactPathOutsideWorkspace"
  | "ArtifactTooLarge"
  | "ArtifactStoreUnavailable"
  | "ArtifactVersionCapExceeded"
  | "ArtifactKindMismatch"
  | "ArtifactCapExceeded"
  | "ArtifactSpecInvalid"
  | "ArtifactRedeclared"
  | "ArtifactUndeclared"
//The second id wants to be primary: the content member is node-level rejection,
// The preset member is failRun - the same boundary as the above.
  | "ArtifactPrimaryConflict";

/**
* Structured alignment where values ​​do not match (which side changed). The value in the record is `expected`, and the value passed in this time is `got`.
* What you need to check those who have been rejected by resume is these two values, rather than extracting them from the message - Process Judgment and Display
* Neither should rely on error text.
 */
export interface WorkflowErrorMismatch {
  expected: string;
  got: string;
}

/** The serializable form of the error, dropped into journal (dwf_node.error_json / dwf_run.failure_json). */
export interface WorkflowErrorJson {
  code: WorkflowErrorCode;
  message: string;
  violations?: Violation[];
  finalText?: string;
  mismatch?: WorkflowErrorMismatch;
/** Only present when `code === "ProviderStop"``. */
  providerStop?: ProviderStopDetails;
}

/**
* Structural errors thrown across Boundary A. With stable code and optional violations / finalText / mismatch,
* Make the script side try/catch and the upper layer process according to the structure without relying on string matching.
 */
export class WorkflowError extends Error {
  readonly code: WorkflowErrorCode;
  readonly violations?: Violation[];
  readonly finalText?: string;
  readonly mismatch?: WorkflowErrorMismatch;
  readonly providerStop?: ProviderStopDetails;

  constructor(
    code: WorkflowErrorCode,
    message: string,
    extra?: {
      violations?: Violation[];
      finalText?: string;
      mismatch?: WorkflowErrorMismatch;
      providerStop?: ProviderStopDetails;
      cause?: unknown;
    },
  ) {
    super(message);
    this.name = "WorkflowError";
    this.code = code;
    if (extra?.violations !== undefined) this.violations = extra.violations;
    if (extra?.finalText !== undefined) this.finalText = extra.finalText;
    if (extra?.mismatch !== undefined) this.mismatch = extra.mismatch;
    if (extra?.providerStop !== undefined) this.providerStop = extra.providerStop;
    if (extra?.cause !== undefined) (this as { cause?: unknown }).cause = extra.cause;
  }

/** Convert to serializable form and drop journal. */
  toJSON(): WorkflowErrorJson {
    const json: WorkflowErrorJson = { code: this.code, message: this.message };
    if (this.violations !== undefined) json.violations = this.violations;
    if (this.finalText !== undefined) json.finalText = this.finalText;
    if (this.mismatch !== undefined) json.mismatch = this.mismatch;
    if (this.providerStop !== undefined) json.providerStop = this.providerStop;
    return json;
  }

/** Rebuild from journal records (used when replay hits a failed node). */
  static fromJSON(json: WorkflowErrorJson): WorkflowError {
    return new WorkflowError(json.code, json.message, {
      violations: json.violations,
      finalText: json.finalText,
      mismatch: json.mismatch,
      providerStop: json.providerStop,
    });
  }
}

// ————————————————————————————————————————————————————————————————
// Boundary A: host API (sandbox script call)
// ————————————————————————————————————————————————————————————————

/**
* World read operation (read-only, journalable). The vocabulary is derived from the world-read registry - adding a primitive gives
* Add a line to `facade/registry.ts`, no need to change it here (see container key description at the top of this module).
 */
export type { WorldReadOp };

/**
* The op of the user plane product (vocabulary is derived from the product registry, see `facade/registry.ts`).
* `ArtifactContentOp` goes {@link WorkflowHostApi.publishArtifact} (effect, via driver),
* `ArtifactPresetOp` takes {@link WorkflowHostApi.declareArtifact} (declared without driver).
 */
export type { ArtifactContentOp, ArtifactOp, ArtifactPresetOp };

/**
* The artifact reference given to the script (facade's `ArtifactRef`): id + the version number cast in this release.
* Content member's promise is fulfilled with it.
 */
export interface ArtifactRef {
  id: string;
  version: number;
}

/**
* The drop-in state of a product version (the form of `dwf_node.result_json`; there is a zod mirror on the shared side).
 *
* Content members and preset members share the same shape, and each only fills in its own half: the content includes `bytes`/`uri`/`sourcePath`,
* Preset with `spec`. Bytes never go into the journal - `uri` points to the tool-artifact store, which is the home of bytes.
 */
export interface ArtifactVersionRecord {
  id: string;
  kind: ArtifactOp;
/** Starting from 1. Preset declarations are always 1 (a declaration has no version, it only exists once). */
  version: number;
  title?: string;
  description?: string;
/** The MIME type of the content member (extension table or `opts.contentType`). */
  contentType?: string;
/** The number of bytes in the content member. */
  bytes?: number;
/** The `zcode-artifact://…` (content member) returned by the store. */
  uri?: string;
/** The original path relative to the workspace (only available for `file`, origin and "display in workspace"). */
  sourcePath?: string;
/** The spec of the preset member (normalized shape; the engine compares and declares repeatedly according to canonicalJson). */
  spec?: unknown;
/** Release time (epoch milliseconds). driver/host writes - the engine has no clock. */
  publishedAt?: number;
  /**
* This id is the deliverable of this run. **Engine** stamped,
* Not driver: stick by id - once a certain version is brought, all subsequent versions will be brought, regardless of whether `primary` is written again when publishing.
   */
  primary?: true;
}

/**
* The driver executes a content product release request (Boundary B).
 *
* `version` is calculated by the **engine** before dispatch (the number of completed rows with this id in the journal + 1) and passed down.
* Instead of letting the driver count by itself: the driver should not know the shape of the journal, but it should be carried in the `toolCallId` of the store
* (one byte per version). Choose one of `path` / `content` and press `op` to determine; the actual parameter verification belongs to the driver.
 */
export interface ArtifactPublishRequest {
  runId: string;
  siteId: string;
  ordinal: number;
  op: ArtifactContentOp;
  id: string;
  version: number;
/** `file`: relative path to the workspace. */
  path?: string;
/** `markdown`: text. */
  content?: string;
/** `opts` given by the script (transparent transmission as it is, shape verification belongs to driver). */
  opts?: unknown;
}

/**
* host API: Facade shim for lowered scripts called across sandbox boundaries. All serializable and asynchronous.
* `createActor` returns an opaque handle synchronously; session creation is completed lazily during the first live dispatch.
 *
* `worldRead` brings **positional actual parameter group**, not a payload object customized by op: `files.grep(pattern, glob?)`
* and `git.diff(base?, path?)` have multiple parameters, `git.status()` has no parameters, and a single string signature cannot be supported; and the payload object
* Will move "by op branch" into lowering - that is exactly the part of the new primitive that should be changed without any changes. lowering only does transparent transmission as it is.
* The arity and checksum of each op belong to the driver (it is the side that knows what a base ref looks like).
 */
export interface WorkflowHostApi {
  createActor(siteId: string, name?: string, persona?: string | PersonaSpec): ActorId;
  ask(siteId: string, actor: ActorId, instructions: string): Promise<unknown>;
  worldRead(siteId: string, op: WorldReadOp, args: unknown[]): Promise<unknown>;
  /**
* Publish an intermediate result. **Send it and forget it (synchronous signature, no return value) but there is a site and it needs to be logged in the journal**——This combination
* is the whole design of report, both halves are forced: the script publishes a discovery, there is nothing to await; and
* The run panel and completion notification cannot display the same finding twice after resume, so they must be journaled.
   *
* No driver round trip: The engine ends after landing the node in the core and sending an event (or silently skipping when replay hits).
   */
  report(siteId: string, item: unknown, artifactId?: string): void;
  /**
* The control flow passes through a `phase("...")` mark. Synchronous, no return value, no driver round trip, **no journal line**:
* The engine only sends a `phase-entered` event and ends. `name` is taken from the literal by lowering and stripped of both ends.
   */
  enterPhase(name: string): void;
  /**
* Publish a **content artifact** (`artifact.file` / `artifact.markdown`). Effect: Copy bytes in via driver
* store, drop a line of journal, successfully cash {@link ArtifactRef}, failure ** can be rejected with catch **.
   *
* Same as `worldRead`: `args` is the positional argument of the script call point (`[id, path, opts]`), lowering is as is
* Packaging, this layer only takes the id that the engine needs to use (the upper limit and version number require it), and the rest is transparently transmitted to the driver.
   */
  publishArtifact(siteId: string, op: ArtifactContentOp, args: unknown[]): Promise<ArtifactRef>;
  /**
* Declare a **preset product** (`artifact.chart` / `table` / `metrics` / `board`). Synchronous, no return value,
* **Without driver**: A statement that there is nothing to wait for, the engine verifies the spec, drops the node, and sends the event in the core and ends.
   *
* Failure (spec is illegal / same id but different spec / exceeds the upper limit) always failRun——void returns no rejected channel, which is the same as
* `report` same argument.
   */
  declareArtifact(siteId: string, op: ArtifactPresetOp, args: unknown[]): void;
  log(message: string): void;
}

// ————————————————————————————————————————————————————————————————
// Boundary B：driver port
// ————————————————————————————————————————————————————————————————

/** An opaque reference to an actor session (zcode session id on the production side). */
export interface SessionRef {
  readonly id: string;
}

/** The core’s decision on a submit attempt is sent to the driver via respondToSubmit. */
export type SubmitVerdict =
  | { kind: "accept" }
  | { kind: "reject"; violations: Violation[] }
  | { kind: "nudge" };

/**
* Downward port: The core requests side effects of driver execution. The core is a purely deterministic state machine, and the side effects and progress returns are all in the driver.
* Journal and emit are hung on the driver: the core is done through `driver.journal`
* Journal decision, Boundary C fanout via `driver.emit`.
 */
export interface WorkflowDriver {
  /**
* Cast an actor session. `seed` **Only present if this actor has consumed ≥1 imported ask entries**
* (transcription truncation of amend-resume, see {@link ActorSessionSeed}): The driver must truncate the source session before returning
* The first `messageCount` messages are copied into the newly minted session, and this step must be **idempotent** - skipped if the target session already has content.
* That's the case when resume resumes with the same session id after a crash (the session id is purely deterministically cast by the driver by (runId, actorRef)).
   *
* Optional parameters: existing driver implementations with one less formal parameter are legal in TS, drivers without seed semantics (fake,
* Pure replay testing) can simply ignore it.
   */
  createActorSession(
    actor: ActorRef,
    persona: PersonaSpec,
    seed?: ActorSessionSeed,
  ): Promise<SessionRef>;
  startAsk(session: SessionRef, instance: InstanceRef, message: AskMessage): void;
  respondToSubmit(instance: InstanceRef, verdict: SubmitVerdict): void;
  cancelAsk(instance: InstanceRef): void;
  /**
* Perform a world read. `args` is the **positional argument** of the script call point, lowering is packaged as it is without any checking;
* The checksum and actual parameters of each op belong to the driver (see {@link WorkflowHostApi.worldRead}).
   */
  executeWorldRead(op: WorldReadOp, args: unknown[]): Promise<unknown>;
  /**
* Publish the bytes of a content product: verify the shape of the actual parameters,
* Parse the relative path of the workspace (rejection when out of bounds), read bytes (cap+1 detection, rejection without truncation when exceeding the limit), define by extension
* contentType, write tool-artifact store, and return the drop-in record.
   *
* **Optional**: Pure replay/fake assembly with no store and no filesystem. In its absence, the engine
* `ArtifactStoreUnavailable` rejects the node - a loud naming failure, not a silent downgrade (preset members are not
* Affected, they are nothing more than drivers).
   */
  executeArtifactPublish?(request: ArtifactPublishRequest): Promise<ArtifactVersionRecord>;
  journal: JournalStorePort;
  emit(event: RunEvent): void;
  /**
* Resource release after run settlement: The engine records `run-settled` in three final state paths (complete/cancel/fail)
* Adjust exactly once after that. The production driver runs the same shutdown chain of app shutdown sessions here for each actor runtime.
* (runtime's `closeBrowserSession`: beginShutdown + node_repl session release + browser session close)
* and clear the session table; the on-the-fly ask has been aborted by cancelAsk at this time. Optional: fake / pure replay assembly has no releasable
* something.
   */
  dispose?(): void;
}

/**
* A model request on the fly is **waiting**:
* - `cause: "slot"`: The next request is queued in front of the process-level admission gate (driver's `tryAdmit` misses); no other fields.
* - `cause: "backoff"`: runner has scheduled retries (`model_retry_scheduled`) with reason / attempt / delayMs / retryAfterMs.
* `reason` is the value of contracts `ModelRetryReason` (`rate_limited` / `provider_overloaded` / `server_error` /
 *   `network_error` / `timeout` / `stream_idle_timeout` / `stale_connection` / `offpeak_queued` …），
* The pure package does not import contracts, so it is an open string. `delayMs` is a relative quantity: the engine has no clock.
 */
export interface AskWaitInfo {
  cause: "slot" | "backoff";
  reason?: string;
  attempt?: number;
  delayMs?: number;
  retryAfterMs?: number;
}

/** Reasons for process-level concurrency cap changes. */
export type ConcurrencyChangeReason =
  | "rate_limited"
  | "provider_overloaded"
  | "offpeak_queued"
  | "recovered"
  | "idle_reset";

/**
* The manager adjusts the cap of a provider key and fans it out to every run that has the key running or waiting for ask.
* (It is acceptable redundancy to have multiple copies of process-level facts per run - the journal can explain "why this run is slow").
 */
export interface ConcurrencyChange {
  /** provider key（`${providerId}/${modelId}`）。 */
  key: string;
  previous: number;
  next: number;
  reason: ConcurrencyChangeReason;
  lastGood?: number;
  lastBad?: number;
/** Current limiting with Retry-After: the length of time (relative amount) that new distribution is frozen. */
  cooldownMs?: number;
}

/**
* Upward surface: The driver reports session progress to the core. The core implements this interface (the production side driver holds its reference;
* In phase one testing, the test directly acts as the "model" and calls these methods).
 */
export interface WorkflowReportSink {
/** A submit attempt; after core verification, a decision will be returned via respondToSubmit. */
  askSubmitAttempted(instance: InstanceRef, payload: unknown): void;
/** A turn ends and is not submitted: the core decides nudge, or settles untyped ask according to finalText. */
  askTurnEnded(instance: InstanceRef, finalText: string): void;
  /**
* Progress observation during one turn parsing: the engine only
* `record()` becomes `node-progress` and does not make any decisions based on it. **Must be called before askStats on the same turn**
*——The sequence of the two events is the reading contract. Calls arriving after run has settled are ignored.
   */
  askProgress(instance: InstanceRef, progress: AskProgress): void;
/** Usage statistics: accumulate run usage and broadcast usage/instance updates. */
  askStats(instance: InstanceRef, stats: AskStats): void;
/** Unrecoverable error on driver side, settlement of this ask is failed. */
  askFailed(instance: InstanceRef, error: WorkflowError): void;
  /**
* Deterministic model-side error: **not** settling
* node, but let the entire run stop with `stopped(provider)` (on the fly, ask is always abort, the same as cancel).
* `error.code` must be `ProviderStop`. Calls arriving after run has settled are ignored.
   */
  stopRun(error: WorkflowError): void;
/** run-level stall observation: engine only `record()`. */
  runStalled(info: RunStallInfo): void;
  /**
* Three pure observations: the engine only `record()`, does not make any decisions.
* run has been settled / calls arriving after the node has been settled are ignored.
   */
/** The next model request for this ask is waiting for: process-level admission gate (slot) or runner backoff (backoff). */
  askWaiting(instance: InstanceRef, info: AskWaitInfo): void;
/** A model request for this ask is actually issued (`model_request_started`, and is not executing before entering). */
  askExecuting(instance: InstanceRef): void;
  /**
* The ask subagent is about to execute a tool that will overwrite the workspace:
* The engine turns off the import cache accordingly. This is the only driver observation that the engine relies on to make decisions - the cached answer is the answer to the precursor world
* As I said, that world ceased to exist after the first writing. Report before the tool is started (after PreToolUse, before handler),
* So the door is closed before the first byte is placed. run Calls arriving after the node has settled / have been settled are ignored.
   */
  askMutating(instance: InstanceRef): void;
/** The process-level manager adjusts the cap of the provider key where this run is located. */
  concurrencyChanged(change: ConcurrencyChange): void;
}

// ————————————————————————————————————————————————————————————————
// Boundary C: run event
// ————————————————————————————————————————————————————————————————

/**
* Node type: ask, world-read, world-run or report (the latter three have no actor and no actorSeq).
* `world-run` is the journalized command execution of world.run - the mechanism is completely isomorphic to world-read, and is a separate one
* is for audit honesty (the effect should not be disguised as reading).
* `report` has a journal line but is not a node in either graph - it sends the progress rather than the order.
* `artifact` is a release or a declaration of **user interface product**, and
* `report` There are no nodes in the same clan: the deliverable is not a step that others can wait for. ⚠ with `RunSettlement.artifact`
* (top-level return value) is irrelevant.
 */
export type NodeKind = "ask" | "world-read" | "world-run" | "report" | "artifact";

/**
* A **bounded** input for a world-read / world-run node (`dwf_node.input_json`). `args` as-is by position
* Reserved, but when serialization exceeds {@link WORLD_READ_INPUT_MAX_BYTES}, it will be cut into strings one by one for preview and concatenation.
* `truncated` - The audit team would rather honestly say "truncated" than stuff a 200 KB `node -e` code string into it
* A table for high-frequency reading.
 */
export interface WorldReadInput {
  op: string;
  args: unknown[];
  truncated?: true;
}

/** `WorldReadInput` Upper limit of serialized bytes (4 KB). */
export const WORLD_READ_INPUT_MAX_BYTES = 4096;

/** Semantic results of node settlement. Replay hits are distinguished by the `cached` flag on the event and are not incorporated here. */
export type NodeOutcome = "ok" | "failed" | "cancelled";

/**
* run life cycle state. Three final states:
* - `completed`: script return;
* - `errored`: script error (script error, engine contract violation) - cannot be resumed, can only be revised by `resume_from`;
* - `stopped`: run is stopped (`RunStopReason`), **can always be resume**.
 *
* The physical column `dwf_run.status` still has the old five values ​​(`failed` / `cancelled`) and is not migrated: here is the **logical** vocabulary,
* The mapping between old and new only exists in the codecs of the SQLite repository (adapters `dwf-journal-codecs.ts`).
 */
export type RunStatus = "pending" | "running" | "completed" | "errored" | "stopped";

/**
* Why run stopped: `user` (user canceled) / `model` (main agent TaskStop) / `provider` (deterministic model side
* Error, see `ProviderStop`) / `interrupted` (holding process death, sandbox crash / timeout / protocol damage - host side,
* A rerun will probably do the trick) / `superseded` (stopped and replaced by an AmendWorkflow, successor id in `supersededBy`;
* **Not resume** - the living is the successor).
 */
export type RunStopReason = "user" | "model" | "provider" | "interrupted" | "superseded";

/**
* run event vocabulary (Boundary C). Each observable state transition emits an event.
* `compaction` reserved for long context compaction - v1 is never emitted, only holds space for future non-destructive additions.
 */
export type RunEvent =
  | { type: "run-started"; runId: string; caps: Caps }
  /**
* The round in which run is initiated: `inputId` is initiated in the parent session.
* inputId of run's turn (hub directly launches as cast UUID v7). **Only build run once in that life**, follow the first one
* `run-started`; resume will not send it again when sending `run-started` - the sub-agent step with the same run will always hang on the same
* under message. Zero SQL: The anchor lives in the journal event, not on the dwf_run column (no migration intentionally).
* `phaseNames` is the phase table (declaration order) of the script declaration, which is filled in by the submitter from the cause and effect diagram; the sidebar mini-track draws the front accordingly.
* site. Also only remember once,
* Free rebuild for cold replay; absent when script has no `phase()` flag.
* `subagentModel` is the model on which the subagent of this run runs, and the standard form is `providerId/modelId[$reasoningLevel]`. The same host metadata as the anchor point and stage table: the engine never reads it,
* This event is only recorded once in the life of the build run; the host reads back the same string from the event header on the resume and two reading surfaces.
* (The sub-agent session is selected according to "This field > resume pin > Parent session model", bootstrap's
* workflow-actor-model.ts). Also zero SQL - not on the `dwf_run` column. Absent means that the subagent runs on the session model.
* `phaseAlongside` and `phaseNames` **aligned by position**: `phaseAlongside[i]` is when entering `phaseNames[i]`
* strand is the index of other phases that are still running (the index falls in the same `phaseNames`). Accordingly, the sidebar draws the two parallel stations as
* Double line segment; absent when there is no parallel phase, the absence means "this track is a straight line".
* `scriptPath` is the file from which the script of this run comes ** (absolute path). Same rules as `subagentModel`: the engine never reads, only builds the run.
* Remember once, zero SQL, the host reads back the same string from the event header on two reading surfaces. Absence means that this run has no editable script
* files (projects that cannot be written in draft, runs initiated before this feature), the model surface therefore returns to the old saying of inline resubmission.
   */
  | {
      type: "run-launched";
      inputId: string;
      toolCallId?: string;
      parentSessionId?: string;
      phaseNames?: string[];
      subagentModel?: string;
      scriptPath?: string;
      phaseAlongside?: number[][];
    }
  /**
* `phaseName` records the phase name specified by the latest `enterPhase` when the instance was born.
* Appears only on **birth events** - actor's `actor-created`,
* Node's `node-queued` and hit cache (replay / amend-resume) instead of queued
* `node-settled { cached: true }`; the other `node-*` events do not have it, and the reducer uses `actorSiteId`
* The precedent is carried forward. Marks the entire key absent from the previously born instance.
   *
* It is **not** the current phase when this event is emitted: node-queued may be postponed to the next token by a hold rule
* is issued later, and the moment of birth is in the previous stage.
   */
  | {
      type: "actor-created";
      actor: ActorRef;
      name?: string;
      persona?: PersonaSpec;
      phaseName?: string;
    }
  | {
      type: "node-queued";
      instance: InstanceRef;
      kind: NodeKind;
      actor?: ActorRef;
      actorSeq?: number;
      phaseName?: string;
      /**
* The **beginning** of the author's instruction is {@link INSTRUCTIONS_HEAD_MAX_CHARS} characters (remove the blanks at both ends, without adding ellipses),
* Only present on the ask node. The only answer to "what is this subagent doing" on the event track: the complete command only lives in
* In `dwf_node.input_json`, the reading surface (run details, GetWorkflowRun) does not read that table for a one-line summary.
       *
* The instruction taken is the **access time**, which is the one before the engine endnote is appended - if the endnote is the engine's, not the author's.
       */
      instructionsHead?: string;
    }
  | { type: "node-dispatched"; instance: InstanceRef }
  | { type: "node-repairing"; instance: InstanceRef; attempt: number; violations: Violation[] }
  | { type: "node-nudged"; instance: InstanceRef }
  /**
* Three observation events for adaptive concurrency. Same family as escalation:
* The engine core does not make any decisions based on them, they are just the facts observed by the driver/manager and dropped by the engine `record()`
* journal + fanout. `node-waiting` ⇄ `node-executing` is the self-loop after dispatched and before settled
* (`executing` thus becomes an **observable** phase), does not change the node's journal record
* (do not write dwf_node).
   */
  | ({ type: "node-waiting"; instance: InstanceRef } & AskWaitInfo)
  | { type: "node-executing"; instance: InstanceRef }
  | ({ type: "concurrency-changed" } & ConcurrencyChange)
  | {
      type: "node-settled";
      instance: InstanceRef;
      outcome: NodeOutcome;
      cached?: boolean;
      error?: WorkflowErrorJson;
/** Only present when `cached: true`: the hit node is not queued, and this is its birth event. */
      phaseName?: string;
    }
  /**
* The progress of an ask in parsing a turn.
* Same family as escalation/concurrency observations: **The engine core does not interpret it**, only `record()` - it does not change any scheduling
* Decision-making, inputHash, replay are not compared to it. The two event sequences of the same turn are load-bearing:
* This article precedes `usage-updated`, so anyone who reads the new usage must have read the progress that earned it.
   */
  | ({ type: "node-progress"; instance: InstanceRef } & AskProgress)
/** Run-level token usage update: directly carry the total amount spent (journal’s spent_tokens is written synchronously, the two are always equal). */
  | { type: "usage-updated"; spentTokens: number }
  | { type: "log"; message: string }
  /**
* The import cache of amend-resume is turned off: a live subagent is about to
* Override the workspace (`cause: "mutating-tool"`, `actorName` is its valid name), or a `world.run` live execution
* (`cause: "world-run"`). Each run can have at most one; after the revised run crashes, resume will resume "the door is closed" according to it - this is
* The **only** source of fact for the closure is no longer inferred from "there was an ask live". Non-revised run has no table to close and will not be sent.
   */
  | { type: "import-cache-closed"; instance: InstanceRef; cause: ImportCloseCause; actorName?: string }
  /**
* The control flow passes through a `phase("...")` mark. **No sites, no journal lines, no driver roundtrips** - tagging is not a one-step job;
* A scale of "where have you run?" `name` is the author's original word (with whitespace removed and the same key as the parser casting phase id);
* `ordinal` counts by name, **issued every time it is evaluated**: the re-entry of the same name is +1, and the analyzer also reads the second tag of the same name as
* Back to the edge. When resume is executed, the prefix will be sent again when the script is re-run (there are no journal lines to remove duplicates), and the consumer is immune to monotonic reduction.
   */
  | { type: "phase-entered"; name: string; ordinal: number }
  /**
* A published intermediate result, each **not skipped** report is called exactly once (replay hits are silently skipped,
* No resending). takes the exact same route as other run events (progress sink → bounded session event → projected reducer),
* The end point is the Results area of ​​the run panel.
   *
* `artifactId` is present, that is, this item is fed to the preset product (`report(item, "perf")`) at the same time - the projection is based on it
* Add one to the `itemCount` of the product, and the Kanban hook uses the count change as an increment to take the signal.
   */
  | { type: "report"; instance: InstanceRef; item: unknown; artifactId?: string }
  /**
* A new version of the **User Interface Product** is in place: one post every time a content member is successfully posted, and one post every time a preset member is actually dropped
* Statement (idempotent no-op is not issued). Same as report - replay hits will not be retransmitted, and cold recovery will be read from the journal.
   *
* The successful path** only sends this one**, and does not send any node life cycle events (node-queued / dispatched / settled).
* The same argument as `report`: the product is a deliverable, not a step that others can wait for. It is not a node in the two pictures.
   */
  | { type: "artifact-published"; instance: InstanceRef; artifact: ArtifactVersionRecord }
  /**
* A content product release failed (the node dropped into the journal as `failed`, and the script side got a catchable rejection).
   *
* **Intentionally not reused `node-settled{failed}`**: readers of that event will reduce it into `nodes[]` and ask the site to
* There are corresponding nodes in the graph, but the product site ** does not enter any graph ** - a node-settled falls on such a site, and when run
* There is an "unknown node" that cannot be explained in the panel, and the layer will also report an unknown site id. Published failed therefore
* Needs its own event: it takes enough to render a failed card (id/kind/structured error) without pretending to be a step
* Work. replay **does not resend** when hitting a failed record (same as report/artifact-published).
   */
  | {
      type: "artifact-failed";
      instance: InstanceRef;
      id: string;
      op: ArtifactContentOp;
      error: WorkflowErrorJson;
    }
  | { type: "compaction"; actor: ActorRef }
  /**
* An actor escalates the blocking problem to the main agent and stays in his ask waiting for the answer.
   *
* **These two events are never emitted by the engine core** - they are emitted by the driver within the boundaries of executing ask (the same as repair /
* nudge round), zero I/O state machine is not aware of upgrades. The reason why they still live in this vocabulary: they follow the same path as
* Other run events have the same two tracks (journal's dwf_event + driver.emit's real-time fanout), and those two
* The shape of the rail is defined by `RunEvent`. Upgrading does not write the dwf_node line (waiting is not work).
   */
  | {
      type: "escalation-raised";
      qid: string;
      actor: ActorRef;
      /**
* The **effective name** of the actor (= `spec.name` after `normalizePersona`, which is `actor-created`
* The one on the event, and the one amend-resume uses as the cache identity key). Anonymous actor is absent,
* There is no hidden label - consumers decide how to render "nameless" individually.
       */
      actorName?: string;
      question: string;
      context?: string;
      /**
* Question time (epoch milliseconds). **Required**: The first question readers have to answer is always "How long have you been waiting for this question?"
* There is no other clock available on the event track - `dwf_event.time_created` is only available on the durable track, the real-time track
* (driver.emit → progress sink → sidebar) If you can't get it, let the UI use "the local time when the event was received" to find out.
* Show each question as if it was just asked when replaying the entire history on cold start.
       *
* **Same moment** as the snapshot `pendingQuestions[].askedAt` (the driver only takes `Date.now()` once,
* Events are shared with parked records), so the waiting time on the two reading surfaces is always the same.
       */
      askedAt: number;
    }
  | { type: "escalation-resolved"; qid: string; answer: string }
/** run-level stall observation (see {@link RunStallInfo}); calls arriving after run has settled are ignored. */
  | ({ type: "run-stalled" } & RunStallInfo)
  /**
* `stopReason` is only present when `status === "stopped"`; `error` is always present when `errored` is
* `stopped` is only present for `provider` (`ProviderStop`) and `interrupted` (`Interrupted`).
   */
  | {
      type: "run-settled";
      status: RunStatus;
      stopReason?: RunStopReason;
/** `stopReason === "superseded"` is present: Stop the new run created by the revision of this run. */
      supersededBy?: string;
      error?: WorkflowErrorJson;
    };

// ————————————————————————————————————————————————————————————————
// Journal storage port + record
// ————————————————————————————————————————————————————————————————

/** dwf_run record (the core only relies heavily on caps/status/failure; the rest are reserved for phase two shaping). */
export interface RunRecord {
  runId: string;
  parentSessionId?: string;
  cwd?: string;
  /**
* The human-given run name (optional `input.name` of `CreateWorkflow`). Pure display metadata: the engine doesn't read it,
* Only dropped together with scriptText / cwd during createRun, for the host's enumeration side to use as a label - otherwise across sessions
* What is listed can only be a string of bare runIds. If you are absent, you will not be named (when reading, press the first line of the script to read it, and do not write back).
   */
  name?: string;
  /**
* The CreateWorkflow tool call id that initiated this run. Pure host metadata: the engine does not read it, only in createRun
* Will be dropped together with scriptText / cwd. It is the only association anchor that remains after restart - `workflowRuns` projects across processes
* If it is not alive, the notification anchors of tool card join and resume can only be restored from here.
   */
  toolCallId?: string;
  scriptText?: string;
  scriptHash?: string;
  /**
* Actual parameters of this run (declarative parameters of saved workflow, verified and backfilled with default values).
   *
* Part of the run **identity** at the same level as `scriptText`, not display metadata: resume replays what is saved here
* Actual parameters, never accept new ones - changing actual parameters is another run, and it is time to go through a new confirmation window. Absent (NULL for old row)
* Interpreted as `{}`, no backfilling (invariant 6/7).
   */
  args?: Record<string, unknown>;
  /**
* The lineage pointer of amend-resume: the predecessor run from which this run was revised. Host metadata, the engine does not read it, it is only passed along with the rest during createRun
* Metadata is stored in the database. The revision is **supersede** - new runId, new script, zero touch on the predecessor row - so the only connection between the two sides
* This is the pointer: resume rebuilds the imported cache (`port.resume` rebuilds the ImportedCache when it is present) and
* UI "continuation from run X" are restored from here. Its absence means that this run is not a revision (the vast majority of runs).
   */
  resumedFrom?: string;
  caps: Caps;
/** Cumulative token usage (observation plane, not control plane). */
  spentTokens: number;
  status: RunStatus;
/** Present when `status === "stopped"` (absent when decoding the old row ⇒ `user`, see warehousing codecs). */
  stopReason?: RunStopReason;
/** `stopReason === "superseded"` is present: replaces the successor run of this run (which is both ends of the same edge as its `resumedFrom`). */
  supersededBy?: string;
  failure?: WorkflowErrorJson;
/** The top-level return value of the script. Present only for completed runs (`undefined` product i.e. integer field is absent). */
  result?: unknown;
}

/** dwf_actor record, unique(runId, siteId, ordinal). */
export interface ActorRecord {
  runId: string;
  siteId: string;
  ordinal: number;
  name?: string;
  persona?: PersonaSpec;
  sessionId?: string;
  /**
* The model on which the actor actually runs (`providerId/modelId`) is written by the driver side.
   *
* Why not plug in `persona`: persona is a frozen identity written synchronously by the engine when creatingActor, and the model is the host
* The fact (the parent session's selection at the time) is invisible to the engine. Divided into two fields, the identity and host facts each have an author.
* Whoever wrote it is responsible.
   *
* Why drop the library: The cost of run is therefore auditable, and resume can reattach to the same model - the parent session is used twice
* If the main model is changed between runs, the model will not be changed quietly in the second half of the same run (pin, see bootstrap
   * workflow-actor-model.ts）。
   *
* Both `sessionId` and `sessionId` belong to the "driver-owned field": the engine's putActor is only responsible for bringing the existing value as it is.
* (See createActor in engine.ts and ensureSession in scheduler.ts), never generate it yourself.
   */
  resolvedModel?: string;
}

/**
* The drop-in status of the node. The node is logged into the library as `running` (carrying actorSeq and inputHash) upon admission.
*Updated to completed/failed during settlement. Therefore, a node that crashes during execution leaves a `running` record in the journal.
* When resuming, re-dispatch live according to the recorded actorSeq position (instead of short-circuiting the completed result).
 *
* **The report node is the only exception to this "write twice" rule**: it is only written once and is `completed` when it is dropped into the library - access and
* There are no driver calls between settlements, so nothing can fail in the middle.
 */
export type NodeRecordStatus = "running" | "completed" | "failed";

/**
* dwf_node record. unique(runId, siteId, ordinal); additionally for ask
* unique(runId, actorSiteId, actorOrdinal, actorSeq). world-read and report of actor* and
* actorSeq is empty; the report line also satisfies: one write, status is always `completed`, `result` is the reported item,
* `inputHash` overwrites the item (defensive comparison on replay hit).
 */
export interface NodeRecord {
  runId: string;
  siteId: string;
  ordinal: number;
  kind: NodeKind;
  actorSiteId?: string;
  actorOrdinal?: number;
  actorSeq?: number;
  inputHash: string;
  status: NodeRecordStatus;
  result?: unknown;
  error?: WorkflowErrorJson;
  stats?: AskStats;
  /**
* **The id of the user plane artifact** (`dwf_node.artifact_id` column). Two lines will take it: the line with `kind: "artifact"`
* (the artifact it was published/declared from), and the line labeled `kind: "report"` (which prefab this item was fed to).
* The latter is exactly where the invariant "Kanban = projection of journal" falls - each point of a Kanban board is a line
* `kind = report ∧ artifact_id = ?`. The remaining rows are empty.
   */
  artifactId?: string;
  /**
* **world-read / world-run node only**: this stop
* The actual executed op and arguments, **bounded** ({@link WorldReadInput}). `inputHash` only answers "Is it the same input?",
* Can't answer "what is the input" - Workspace transcript Say *what ran*, not just *something ran*.
* Write after admission (same putNode as `inputHash`), the settled upsert is brought over as it is; the line before 0030 is the same as
* The ask/report/artifact lines are always absent.
   */
  input?: WorldReadInput;
  /**
* **ask node only**: After the complete exchange of the ask (including the repair / nudge round and the closing message after submit),
* The length of the actor session message log - a count offset, not a message id range.
   *
* Complementary fields owned by the driver (same family as `stats` backfill, also read, modified and dropped by getNode + putNode), engine
* does not produce it. The reason why count is stored instead of id range is because the key property of count is that it does not change when the prefix is ​​copied: the source session
* The first N messages are copied into a new session. The message IDs are all changed but the count remains unchanged. Therefore, the boundary value of the copied ask is in the new session.
* Valid as is. This is the basis of amend-resume's full-fidelity transcription truncation (and therefore chained revision).
   */
  messageBoundary?: number;
}

/** A dropped event, the sequence is monotonically allocated by appendEvent. */
export interface StoredEvent {
  sequence: number;
  event: RunEvent;
  /**
* The time (epoch milliseconds) when the storage layer appends this event. The only clock for "how long ago" everything in the event log:
* The age of the log line, the last time the subagent acted, and how long the run has been stagnant can only be calculated from it.
* `Date.now()` retrieved by the reader is completely wrong - it would mark the entire history of a week ago in a cold replay as "just".
   *
* Type-optional to allow test doubles that implement the port to continue compiling; two **real** implementations (SQLite's `time_created`
* The append timestamp of column and memory implementation must be filled in, and the contract test pins this one.
   */
  timeCreated?: number;
}

/**
* Event paging parameters (cursor = journal sequence). The running details page on the app side incrementally pulls event logs accordingly:
* Returning the entire journal at once means reading the entire journal into memory every time a page is turned.
 */
export interface ListEventsOptions {
/** Only return events whose sequence ** is strictly greater than ** this value. cursor is "the last sequence read", not an offset. */
  afterSequence?: number;
/** The maximum number of items returned on a single page; no limit by default. */
  limit?: number;
}

/**
* run settles the included drop-in content. The reason for existence is **written in one stroke**: the final state and product are UPDATE twice,
* An intermediate crash creates a `completed` but unrecoverable run. The default key means "don't touch the column"
* (The engine's `running` writing and historical rows before the column is introduced rely on this semantics).
 */
export interface RunSettlementRecord {
/** Written at the same time as `status === "stopped"`; other statuses are not included. */
  stopReason?: RunStopReason;
/** The same write as `stopReason === "superseded"` (stopped will rewrite the entire envelope and will be lost if it is divided into two writes). */
  supersededBy?: string;
  failure?: WorkflowErrorJson;
  result?: unknown;
}

/**
* Journal storage port: warehousing, synchronous method, no SQL leaks. For memory implementation, see journal-memory.ts;
* The SQLite implementation shares this port with the in-memory implementation. The old test entry has been cleaned up with Vitest to avoid production compilation relying on undeclared test frameworks.
 */
export interface JournalStorePort {
  createRun(record: RunRecord): void;
  getRun(runId: string): RunRecord | undefined;
  updateRunStatus(runId: string, status: RunStatus, settlement?: RunSettlementRecord): void;
  /**
* Separately persist the accumulated token usage (high-frequency small writes outside of run settlement). Unknown runId must throw an error;
* Never touch status / failure - usage update and run settlement are two independent write paths.
   */
  updateRunUsage(runId: string, spentTokens: number): void;

  putActor(record: ActorRecord): void;
  getActor(runId: string, siteId: string, ordinal: number): ActorRecord | undefined;
  listActors(runId: string): ActorRecord[];

  putNode(record: NodeRecord): void;
  getNode(runId: string, siteId: string, ordinal: number): NodeRecord | undefined;
  listNodes(runId: string): NodeRecord[];

  appendEvent(runId: string, event: RunEvent): StoredEvent;
  /**
* List events in ascending order of sequence. `opts` defaults to the full amount (historical form); when using cursor / limit it must be
* Storage layer filtering, it is not allowed to take the whole amount and then slice it - the reason for paging is not to read the entire journal into the memory.
* Unknown runId and out-of-bounds cursor will always return an empty array (no error will be thrown): In the race window between projection and journal,
* It is normal timing for the client to come back with a cursor that does not yet exist.
   */
  listEvents(runId: string, opts?: ListEventsOptions): StoredEvent[];
}

// The driver's observation vocabulary (usage + progress) for ask lives in ask-observation-types.ts (ditto), exported here to maintain reference paths.
export {
  INSTRUCTIONS_HEAD_MAX_CHARS,
  LAST_TOOL_NAME_MAX_CHARS,
  LAST_TOOL_TARGET_MAX_CHARS,
} from "./ask-observation-types.js";
export type { AskLastTool, AskProgress, AskStats } from "./ask-observation-types.js";

// ————————————————————————————————————————————————————————————————
// Import cache (amend-resume)
// ————————————————————————————————————————————————————————————————

//The data structure of the imported cache lives in imported-cache-types.ts (this file will be removed after reaching the max-lines limit). The export is exported here to maintain the reference path.
export type {
  ImportedActorCandidate,
  ImportedAskEntry,
  ImportedRunCache,
  ImportedWorldEntry,
} from "./imported-cache-types.js";

/**
* Session seed: handed to {@link WorkflowDriver.createActorSession} when the different actors are distributed live for the first time,
* Have new sessions begin with the **Full-fidelity transcription prefix** of the source session.
 */
export interface ActorSessionSeed {
/** Transcription source session (precursor or earlier ancestor session of this actor). */
  sourceSessionId: string;
  /**
* How many messages before copying the source session = the last {@link NodeRecord.messageBoundary} consumed and imported into ask.
* count offset is replicated across prefixes, so this value is directly available at any session-holding ancestor on the chain.
   */
  messageCount: number;
/** Inherited model pin (silent model change under transcription continuation is exactly the identity mutation that pin should prevent). */
  resolvedModel?: string;
}

// ————————————————————————————————————————————————————————————————
// Strategy constants (part of this contract)
// ————————————————————————————————————————————————————————————————

/** The maximum number of repair (rejection retries) within the node: 3 times. */
export const REPAIR_ATTEMPTS = 3;

/** The upper limit of nudge times when the turn ends without submission: 1 time. */
export const NUDGE_ATTEMPTS = 1;
