// ============================================================
// AgentRuntime-backed WorkflowDriver: Shared type
// ============================================================
// workflow-driver.ts reaches the upper limit of oxlint max-lines (400 lines), and adds the driver’s dependency package
// (AgentRuntimeWorkflowDriverDeps), runtime factory signature (ActorRuntimeFactory) and each actor session
// The running state (SessionState) is split into this file for workflow-driver.ts / workflow-driver-escalation.ts /
// workflow-driver-helpers.ts is shared among three places; the public side (ActorRuntimeFactory) is still exported from workflow-driver.ts.
// Only types here, zero runtime code.

import type {
  ExecutionPort,
  FileSystemPort,
  Logger,
  ModelRequestAdmission,
  SessionId,
  SubmitVerdict as ContractsSubmitVerdict,
  ToolArtifactStorePort,
  WorkflowEscalatePort,
  WorkflowSubmitPort,
} from "@zcode/contracts";
import type { AgentRuntime } from "@zcode/core";
import type {
  ActorSubmitProfile,
  ActorRef,
  ActorSessionSeed,
  InstanceRef,
  JournalStorePort,
  PersonaSpec,
  RunEvent,
  SessionRef,
} from "@zcode/dynamic-workflow";
import type { ActorTranscriptStore } from "./workflow-actor-transcript.js";
import type { WorkflowConcurrencyPort } from "./workflow-concurrency-governor.js";
import type { ActorModelActivity, WorkflowClock } from "./workflow-driver-concurrency.js";
import type { WorkflowEscalationRegistry } from "./workflow-escalation-registry.js";

/** A promise that can be settled externally. */
export interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (reason: unknown) => void;
}

/**
* Actor runtime factory: given session id / actor / persona / session-level submit port, output an assembled
* child AgentRuntime. Injecting submitPort registers the submit_result tool for the session (the registration gate of the core is subject to the existence of the port).
 *
* Extract "how to construct runtime" into an injection point: production side packaging createScriptWorkflowAgentRuntime (with real model adapter
* with a full set of deps); test side with minimal deps bag + scripted model adapter. The driver ontology only cares about life cycle and bridging.
 *
* Allow returning Promise: The production side must drop the actor session and create a task link before returning, and
* `session_task_link.child_session_id` has an FK for `session(id)`, so "build session line" must precede "build link".
* The engine's ensureSession will await createActorSession, so await is naturally safe here - first ask
* Before dispatch, the session has been persisted.
 */
export type ActorRuntimeFactory = (input: {
  sessionId: SessionId;
  actor: ActorRef;
  persona: PersonaSpec;
  submitPort: WorkflowSubmitPort;
  /**
* The actor's submit profile. The factory decides which submit_result to register based on this: `untyped` → **not injected** submitPort (core's
* The registration gate is the port present, so there is no tool); `mono` → injection port + `workflowSubmitSchema` (typed statement);
* `generic` → Inject only ports (generic statement today). The driver is checked by actor site from deps.actorSubmitProfiles,
* Absence means generic.
   */
  submitProfile: ActorSubmitProfile;
  /**
* Session level upgrade port. The injection method is completely isomorphic to submitPort:
* The presence of the port means that the `escalate` tool is registered for the session (the core registration door is subject to the existence of the port), ** constant injection, no
* opt-in** - The actor most likely to hit an unforeseen wall is the one the author has not marked.
   */
  escalatePort: WorkflowEscalatePort;
  /**
* Session seed of amend-resume, present only when this actor consumes ≥1 imported ask entries.
   *
* The factory only needs one thing from it: `resolvedModel` is the **model pin** parsed by the predecessor, which should be like the pin in the journal
* Override the gear and re-analyze (changing models silently under transcription continuation is exactly the identity mutation that pin should prevent). Transcription copying itself is not owned by the factory - that's what the driver does after the factory returns (the session line needs to exist first,
* `message.session_id` has FK for `session(id)`).
   */
  seed?: ActorSessionSeed;
  /**
* The model request admission port of this actor runtime:
* Only present when the driver gets the manager port; the factory puts it into the `modelRequestAdmission` of the runtime deps,
* runner Each model request attempt passes through the gate first. Its absence means that the runtime is not gated.
   */
  modelRequestAdmission?: ModelRequestAdmission;
}) => AgentRuntime | Promise<AgentRuntime>;

/** Dependencies required to construct the AgentRuntime-backed driver (journal and emit are provided and held by the caller/harness). */
export interface AgentRuntimeWorkflowDriverDeps {
  journal: JournalStorePort;
  emit: (event: RunEvent) => void;
/** The file system port that world-read (files.glob/files.read/files.grep) falls on. */
  fileSystemPort: FileSystemPort;
  /**
* The child process execution port where * git.* world-read falls (cwd = workspace root).
   *
* **Required rather than optional**: Optional will give a silent downgrade running path - `git.*` can be used in production, but forgotten somewhere
* The wired assembly silently becomes "not a git repository", and the two failures look exactly the same in the script. Rather let the wiring go wrong
* Appears at compile time (this is also the existing approach of this package to fileSystemPort).
   */
  executionPort: ExecutionPort;
  /**
* Upgraded the docking registry for Q&A. The driver registers parked issues here,
* run service's `resolveQuestion` returns the answer via the same table.
   *
* **Required rather than optional**, the reason is the same as the executionPort above: optional will give a silently degraded running path - no table
* when `escalate` either hangs permanently (worst case), or silently degrades to "this ability does not exist", which is not the same as "the author did not give
* Actor upgrade rights" look exactly the same in the eyes of the model. Rather let wiring errors occur at compile time.
   */
  escalationRegistry: WorkflowEscalationRegistry;
/** world-read Path resolution base directory (workspace root) shared with product releases. */
  cwd: string;
  /**
* User interface products (`artifact.file` / `artifact.markdown`)
* Byte drop point. ⚠ The artifact here is the output delivered to the user, not the artifact inside the engine
* (The top-level return value of `RunSettlement.artifact`, which is for the model).
   *
* **Optional**, same argument as `actorTranscriptStore`: assembly without store (pure replay, fake driver,
* Minimal stub) There is no place to put bytes in the first place. Absence ** is not a silent downgrade ** - content members named after
* `ArtifactStoreUnavailable` rejects the node (the script can catch, and the node falls into the journal as failed) instead of
* Returning to the writing workspace does not mean releasing an empty product. Preset members (chart/table/…) are not affected: they are declarations,
* No driver required at all.
   */
  artifactStore?: ToolArtifactStorePort;
  /**
* The parent session id of this run, **only used for product release**: writes to the store are accounted for according to the session scope
* (`zcode-artifact://<session>/<id>`), this scope is the parent session.
   *
* Paired with `artifactStore` (production assembly is given simultaneously by run service, both from the same app
* session); publishing also fails loudly with `ArtifactStoreUnavailable` when giving only half of it, see
* {@link ArtifactPublishDeps}. The actor session id does not contain this field - it is mintActorSessionId.
   */
  parentSessionId?: SessionId;
  /**
* Set of approved commands for world.run (compile-time literal collection).
* Structurally falls into {@link WorldReadDeps}: absence means world.run fails-closed.
   */
  declaredRunCommands?: ReadonlySet<string>;
  /**
* The submit profile of each actor site is calculated by `deriveActorSubmitProfiles` during compilation (run submission path
*compileOnce). Optional: absent = each actor is generic (historical behavior), not counting profile's assembly
* (snippet, fake driver, existing tests) therefore remain unchanged.
   */
  actorSubmitProfiles?: ReadonlyMap<string, ActorSubmitProfile>;
  runtimeFactory: ActorRuntimeFactory;
  /**
* Narrow port of the process-level concurrency manager. when present
* The driver gives each actor runtime a `ModelRequestAdmission` (passed down to the runtime via the runtimeFactory parameter)
* deps): runner's **each model request attempt** passes through the gate first; and subscribes to the cap change fanout of this run.
* `concurrency-changed`. Absence means the actor is not gated (fake/pure replay assembly with zero changes). The engine side is indifferent to this
* (`acquireSlot` was removed from v1).
   */
  concurrency?: WorkflowConcurrencyPort;
  /**
* Transcript access side of actor sessions (production is the session store itself). Two uses share it, and must be the same:
* ask boundary accounting for counting, copying with seed truncation (see module description in workflow-actor-transcript.ts).
   *
* Optional, since assembly without session storage (pure replay tests, minimal stub runtime) inherently has no transcription to count:
* In the absence of the boundary accounting, the entire boundary accounting is absent. The price is that ** the run can no longer be used as the precursor of the revision ** ("no marker" on the service side
* Precursor overall rejection" door will block) instead of a false boundary. Session creation with a seed then fails loudly in absence -
* The engine has determined that it needs to receive transcription based on the import fact. If there is no access interface at this time, it is a wiring error.
   */
  actorTranscriptStore?: ActorTranscriptStore;
  logger?: Logger;
  /**
* The id of this run. The actor session id is scoped to it - the solution without runId will allow two simultaneous runs to
* site×ordinal actor collides with the same session id (`createSessionId("wf-actor-" + refToString(actor))`
* This is the bug). The default "run" is just to not break existing test rigs.
   */
  runId?: string;
  /**
* Clocks and timers: Run-level stall clock and transient failure backoff
* Use it for all reboots. Can inject false time just for testing; `stallAfterMs` defaults to 20 minutes; `random` for backoff jitter.
   */
  clock?: WorkflowClock & { stallAfterMs?: number; random?: () => number };
}

/**
* The running state of each actor session. per-actor FIFO (engine guaranteed) → at most one in-flight ask per session, so
* currentInstance / pendingSubmit / accepted / canceled are all "single-previous instance" semantics and do not need to be subdivided by instance.
 */
export interface SessionState {
  readonly ref: SessionRef;
/** The same value as `ref.id`, except that the brand type is retained (the transcription access side is retrieved according to SessionId). */
  readonly sessionId: SessionId;
  readonly runtime: AgentRuntime;
  /**
* The submit_result form actually registered for this session. Created from a static profile; the **only** path that changes is the runtime guard
* (ensureSubmitProfileFits of workflow-driver.ts): When the mono declaration does not match the actual ask schema, it will be downgraded to
* generic, will not be upgraded again thereafter.
   */
  submitProfile: ActorSubmitProfile;
/** Currently flying ask instance; startAsk setting. */
  currentInstance?: InstanceRef;
/** Whether the current ask is typed (untyped does not use the submit bridge). */
  currentTyped: boolean;
/** The submit_result handler blocks the verdict deferred on it; at most one (single front instance). */
  pendingSubmit?: Deferred<ContractsSubmitVerdict>;
  /**
* Parked upgrade Q&A, key = qid. Different from pendingSubmit **must be multiple**: models can be issued in parallel in one round
* Several escalate tool calls, each is an independent question and answer (the upper limit is managed by escalationsUsed).
   */
  readonly pendingEscalations: Map<string, Deferred<string>>;
  /**
* The number of upgrades that have been used in this ask (including the ones that were short-circuited by the upper limit are not included - see makeEscalatePort).
* Per-ask count, startAsk is reset to zero; nudge round ** is not reset to zero ** (nudge is still in the same ask).
   */
  escalationsUsed: number;
/** The current ask's turn cancels the controller; startAsk is rebuilt every ask. */
  abortController?: AbortController;
/** The current ask has been accepted by the engine: askTurnEnded (has been resolved by the engine) will no longer be reported when it turns resolve. */
  accepted: boolean;
/** The current ask is actively canceled by the engine: turn reject does not report askFailed (the engine has settled failed/cancelled). */
  cancelled: boolean;
  /**
* Turns initiated on this session. There is only one purpose: to determine whether the engine has started again after `askTurnEnded`
* (nudge is taking this path), so as to determine whether the exchange of ask is really over - the boundary accounting requires the entire paragraph
* The end of the exchange, not the end of any turn.
   */
  turnGeneration: number;
  /**
* The ending chain of the currently flying turn (executeTurn together with onTurnResolved / onTurnRejected). Only one reader:
* Dispose needs to wait until it is settled before closing the runtime - the askStats of the accept path will arrive after settle (turn after submit
* will be resolved later), the synchronization level will compete with this tail.
   */
  turn?: Promise<void>;
  /**
* The actor's model active side: admission port + session event observation (source of waiting / executing logo).
   */
  modelActivity: ActorModelActivity;
/** The actor identity of this session (the ProviderStop detail names the subagent that triggered the stop). */
  readonly actor: ActorRef;
  readonly actorName: string | undefined;
  /**
* The number of transient redrives on the driver side in the current ask: if the transient failed by the runner (flow recovery is exhausted, etc.), the node will not be settled, and another round will be started according to the backoff curve.
* startAsk is reset to zero.
   */
  transientAttempts: number;
/** Back off the waiting redrive alarm clock; cancelAsk / dispose to cancel. */
  cancelRedrive?: () => void;
}
