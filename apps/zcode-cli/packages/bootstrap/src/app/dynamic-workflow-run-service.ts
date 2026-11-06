// ============================================================
// Dynamic Workflow Run Service (production implementation of DynamicWorkflowRunPort)
// ============================================================
// This service is responsible for:
// Compile once → register AbortController → start the engine (fire-and-forget, the execution body is
// dynamic-workflow-run-launch.ts)→ Put the observation surface of run (snapshot/wait/cancel/restore/enumerate/
//Event paging) is handed over through the narrow port.
//
// There are seven invariants. Violation of any one of them will be expressed as "far from the cause", so write it in the file header:
//
// 1. **Engine exclusive createRun**. This service never pre-inserts dwf_run lines. The pre-existing line will flip the engine constructor into
//resume branch (engine.ts): node count is recalculated based on completed records, budget is restored from records, status is
// updateRunStatus override. A brand new run will not report an error when taking the resume branch, but will only silently start from an empty journal.
//      "recover". resume, in turn, relies on this mechanism: it relaunches the existing runId, allowing the engine to hit
// Existing row. The two entrances are in opposite directions and never share a door.
// 2. **Compile exactly once**. A ts.Program feeds site tables, schema synthesis and lowering at the same time; scriptHash is provided by
// This service does not count (harness intentionally does not count - if it hashes "the text it sees", the lowered path will be stored in the library)
// The hash of lowered function body, the comparison object of resume verification is silently wrong).
// 3. **This service will not be constructed if there is no journal**. Durability is a prerequisite for run: a run that silently loses persistence
// Worse than no run (no data for resume, no source for details page, no record after cancellation). When narrowing fails
// The caller of {@link createDynamicWorkflowRunService} gets undefined, so CreateWorkflow
// Return to the placeholder diagnostic path - this is a visible degradation, not a broken feature.
// 4. **Converge the orphan run of this session during construction, and only converge the ** of this session. See {@link reconcileOrphanRuns}:
// The `running` line left by the dead process can only be determined at this moment, and "this session" is the only safe scope.
// Convergence is written as `stopped(interrupted)` (carrying `Interrupted` failure code), which is recoverable like other stopped.
// 5. **resume first replaces the registry entries and then starts**. `waitForTask` returns directly to the runId that is not in the registry
// Journal snapshot - the entry is one step later, and the notification watcher of the heavy arm will immediately settle against the old final state.
// 6. **Register as a resident blocking job in the same sync slice that is started every time**. The engine is a closure of the parent session App and does not enter
// runtime task registry; the resident pool therefore only sees "registry retired" and replaces the ones with the flying engine
// The App is closed by idle (10 minutes), and resume starts the second engine. There is only one registration point——
// {@link trackSettlement}, shared by submit / amend / resume - counting in the same sync slice of launch
// Added and finally released at settlement.
// 7. **close stops every run it owns, and the engine writes that one by itself**. When the App is closed, the service will be
// `"interrupted"` abort each in-flight entry and **wait for them to settle** (not timeout, not polling), harness
// This reason is normalized to `engine.stop("interrupted", Interrupted)`, so the normal finishRun of the engine is passed
// The tail is completed and can be resumed `stopped(interrupted)`. service never writes its own final line: bypassing the engine and writing it, it will
// Create a second writer of dwf_run (same argument as invariant 1), and there will be no corresponding run-settled in the journal.
// After closing, submit / amend / resume and throw it directly - the closing gate of the resident pool has blocked the command. When you get here,
// Bad wiring, shouldn't let contracts' rejection enum widen for it (same argument as invariant 1).

import type { DwfRunSessionListItem } from "@zcode/adapters/storage";
import type {
  TraceContext,
  DynamicWorkflowRunEvent,
  DynamicWorkflowRunArtifact,
  DynamicWorkflowRunArtifactBytes,
  DynamicWorkflowRunArtifactItem,
  DynamicWorkflowRunArtifactItemPage,
  DynamicWorkflowRunWorkspaceNode,
  DynamicWorkflowRunWorkspaceNodeResult,
  DynamicWorkflowRunWorkspaceNodeResultQuery,
  DynamicWorkflowRunEventPage,
  DynamicWorkflowResolveQuestionResult,
  DynamicWorkflowRunPort,
  DynamicWorkflowRunProgressPayload,
  DynamicWorkflowRunResumeResult,
  DynamicWorkflowRunSessionSummary,
  DynamicWorkflowRunSnapshot,
  DynamicWorkflowRunAmendRequest,
  DynamicWorkflowRunAmendResult,
  DynamicWorkflowRunCancelInitiator,
  DynamicWorkflowRunSubmitRequest,
  DynamicWorkflowRunSubmitResult,
  ExecutionPort,
  FileSystemPort,
  Logger,
  SessionId,
  ModelRequestAdmission,
  ModelSelection,
  ToolArtifactStorePort,
  WorkflowEscalatePort,
  WorkflowSubmitPort,
} from "@zcode/contracts";
import type { AgentRuntime } from "@zcode/core";
import { WORKFLOW_RUNS_LIMITS } from "@zcode/shared/zcode-protocol-v4";
import type {
  ActorSubmitProfile,
  ActorRef,
  Caps,
  JournalStorePort,
  PersonaSpec,
} from "@zcode/dynamic-workflow";
import { toProtocolEvent } from "./dynamic-workflow-run-launch.js";
import { readWorkflowArtifactBytes } from "./dynamic-workflow-run-artifact-read.js";
import { replayRunProgress } from "./dynamic-workflow-run-replay.js";
import { listArtifactItemsFrom } from "./dynamic-workflow-run-artifact-queries.js";
import {
  supportsRunEnumeration,
  supportsRunIntrospection,
  type DynamicWorkflowTaskLinkStore,
} from "./dynamic-workflow-run-journal.js";
import { createRunIntrospectionMethods } from "./dynamic-workflow-run-introspection.js";
import { reconcileOrphanRuns } from "./dynamic-workflow-run-reconcile.js";
import {
  resumeDynamicWorkflowRun,
  amendDynamicWorkflowRun,
  submitDynamicWorkflowRun,
  type DynamicWorkflowRunEntryContext,
} from "./dynamic-workflow-run-submit.js";
import {
  listWorkspaceNodesFrom,
  readWorkspaceNodeResultFrom,
} from "./dynamic-workflow-run-workspace.js";
import {
  artifactsOf,
  settleOrAbort,
  snapshotOf,
  toSessionSummary,
  type RunRegistryEntry,
} from "./dynamic-workflow-run-observation.js";
import {
  createRunServiceLifecycle,
  type DynamicWorkflowRunSettledNotice,
} from "./dynamic-workflow-run-lifecycle.js";
import type { ActorTranscriptStore } from "./workflow-actor-transcript.js";
import { resolveWorkflowConcurrencyCeiling } from "./workflow-concurrency-ceiling.js";
import type { WorkflowConcurrencyPort } from "./workflow-concurrency-governor.js";
import type { AgentRuntimeWorkflowDriverDeps } from "./workflow-driver-types.js";
import {
  createWorkflowEscalationRegistry,
  type WorkflowEscalationRegistry,
} from "./workflow-escalation-registry.js";

/** The default/upper limit number of listRunsForSession (the enumeration surface is bounded, never unbounded library scanning). */
const DEFAULT_LIST_RUNS_LIMIT = 16;
const MAX_LIST_RUNS_LIMIT = 64;

/**
* The requested concurrency upper bound → the actual upper bound of this run.
 *
* **Clamp rather than reject**: This knob is only used to reduce concurrency. An excessively large value expresses the intention of "don't restrict me". Turn it into
* A tool failure will only cause the model to guess how many cores the machine has. Absent / non-finite number also read as "unlimited" = ceiling, non-integer
* Rounding down (it doesn't make sense to ask for "3.7 questions on the fly", and rounding up would sneak past the number the user said).
 */
function clampRunConcurrency(requested: number | undefined, ceiling: number): number {
  if (requested === undefined || !Number.isFinite(requested)) return ceiling;
  return Math.max(1, Math.min(ceiling, Math.floor(requested)));
}

/** Input to the actor runtime factory. runId is included, because session id and task link both have run scope. */
export interface DynamicWorkflowActorRuntimeInput {
  runId: string;
  sessionId: SessionId;
  actor: ActorRef;
  persona: PersonaSpec;
  submitPort: WorkflowSubmitPort;
  /**
* The actor's submit profile: `untyped` is not injected
* submitPort (no tools), `mono` injects the port + typed declaration, `generic` only injects the port. The factory is mapped this way
* The only landing point (same seam as the fixed disallowlist in the subagent toolface).
   */
  submitProfile: ActorSubmitProfile;
  /**
* Session-level upgrade port: Injection is to register `escalate` for the actor session
* Tool, completely isomorphic to submitPort. **Always present**, no opt-in - the actor most likely to hit an unforeseen wall
* Exactly the one not marked by the author.
   */
  escalatePort: WorkflowEscalatePort;
  /**
* The last model parsed by this actor (`resolvedModel`, `providerId/modelId` in the journal),
* Only resume will take it. Factory without {@link DynamicWorkflowActorRuntimeInput.runSubagentModel}
* It must be adopted in preference to the parent session model: see reason for pin (persona freeze invariant) in `workflow-actor-model.ts`
* the persistent half).
   */
  pinnedModel?: string;
  /**
* This run's own subagent model (`subagent_model` of `CreateWorkflow` / `AmendWorkflow`, from
* The journal's `run-launched` event is parsed back). The entire selection, including reasoning gears.
   *
* **Highest** priority, above {@link DynamicWorkflowActorRuntimeInput.pinnedModel} and the parent session model
* (`workflowActorModelPolicy` of workflow-actor-model.ts): This item is the user’s explicit request for this run
* Expression, pin only adheres to the implicit default without it. Only the sub-agents are controlled, the main agent is not affected. Absence is running on pin or session model.
   */
  runSubagentModel?: ModelSelection;
  /**
* The model request admission port of this actor runtime:
* The driver is given when the manager port is present; the factory is put into the runtime deps as is. Absence means freedom from the gate.
   */
  modelRequestAdmission?: ModelRequestAdmission;
}

export interface DynamicWorkflowRunServiceDeps {
/** durable journal (dwf_* table). If missing, the service will not be constructed, see file header invariant 3. */
  journal: JournalStorePort;
  /**
* Analysis of the launch anchor point: give the parent runtime activity wheel at the moment of submit
* inputId; `trace.turnId` is inconsistent with the active round or returns `undefined` (submit side pocket bottom cast value) when there is no active round.
* Absence means that the host does not have the concept of "current round" (CLI, test assembly).
   */
  resolveLaunchInputId?: (trace: TraceContext) => string | undefined;
  /**
* The parent session id of this service instance (= the session of this app, see create-app.ts).
   *
* It is the scope of **Orphan Convergence and Enumeration**, so it is not optional: there are only two ways left - global cleanup (which will merge the same process
* The running flag of the brother session is dead, both share the same sqlite, each has its own memory registry) or simply does not converge.
* (That's the "run always stops running" bug). Rather let wiring errors occur at compile time.
   */
  parentSessionId: string;
/** The file system port that world-read (files.glob/files.read/files.grep) falls on. */
  fileSystemPort: FileSystemPort;
/** git.* The child process execution port where world-read falls (cwd = run's workspace). */
  executionPort: ExecutionPort;
  /**
* The byte placement point of the user plane product (`artifact.file` / `artifact.markdown`) is transferred to the driver as it is. ⚠ The artifact here refers to what is delivered to the user for viewing
*Output**, not the top-level return value inside the engine.
   *
* **Optional**: Assembly without store (test, minimal stub) can still run, but the content members will be named after
* `ArtifactStoreUnavailable` rejection - a script catchable failure, not a silent downgrade. session scope
* The {@link DynamicWorkflowRunServiceDeps.parentSessionId} of this service is used (= the session of this app,
* That is the parent session).
   */
  artifactStore?: ToolArtifactStorePort;
/** Create an actor's child AgentRuntime (production wrapper createScriptWorkflowAgentRuntime). */
  createActorRuntime: (input: DynamicWorkflowActorRuntimeInput) => AgentRuntime;
/** The task link of the actor session is dropped in the library; if it is absent, link creation is skipped (the session itself is still dropped in the library). */
  taskLinkStore?: DynamicWorkflowTaskLinkStore;
  /**
* Transcript access side of actor sessions (production is the session store itself). The driver uses it to do two things: ask for boundary accounting
* Count, with amend-resume divergence actors' transcription truncated copies.
   *
* When absent, the boundary accounting is overall absent - the run is completed as usual, but **can no longer be used as the precursor for revision** (service's
* "No marker front-wheel drive overall rejection" door will block). Optional rather than required because it is not included in the assembly without session storage.
* Transcription is countable; seeded session creation fails loudly by the driver in its absence.
   */
  actorTranscriptStore?: ActorTranscriptStore;
  /**
* Engine event hook: What is handed over is the **prepared session event payload** (bounded payload + journal sequence +
* Two derived fields), the caller is only responsible for appending it to the parent session (create-app connects to the runtime's record method).
   *
* Why is it prepared by this service instead of letting the caller spell it out: both sequence and spentTokens can only be read from the journal.
* And journal is a dependency of this service; actor session id can only be calculated by the function that casts it. push these three things to
* The caller is equivalent to copying three contracts to a layer without journal.
   *
* The second parameter is **routing** information, deliberately separated from the payload: `parentSessionId` determines which session the event should fall to,
* But it does not belong to the payload itself (the event is already in that session, and saving another copy is redundant). The caller uses it as an identity gate.
* See {@link createDynamicWorkflowRunProgressSink}.
   */
  onRunEvent?: (
    progress: DynamicWorkflowRunProgressPayload,
    routing: { parentSessionId?: string },
  ) => void;
  logger?: Logger;
/** Injects concurrency probe for caps default value testing fixed dual core (floor must be 1). */
  availableParallelism?: () => number;
  /**
* Narrow port of the process-level concurrency manager. Pass it over to the driver as is:
* Effective concurrency = min (caps.maxConcurrency of this run, shared live cap of this provider key). Absence means only
* per-run upper bound (test assembly, host without manager).
   */
  concurrency?: WorkflowConcurrencyPort;
  /**
* Register a startup as a **resident blocking job** for the parent runtime.
   *
* The engine lives in the closure of the session App and does not enter the runtime task registry, while the resident pool is
* Read-only registry - a running run is read as idle, the App is closed, and resume starts the second engine. Register and go
* The only port of the runtime (`trackResidencyBlockingWork`), the resident pool so there is no need to guess the sidecar.
   *
* Absence means that the host has no concept of resident (CLI one-time execution, test assembly): run will be completed as usual, but it will not block the shutdown.
   */
  registerResidencyBlockingWork?: (work: Promise<unknown>) => void;
  /**
* Driver's clock and timer: run level
* Stall clock and transient failure backoff redrive both read it. **Injected for testing only** (Fault matrix combines the 2s→60s redrive curve with
* The 20-minute stall window is reduced to milliseconds); the production assembly is never set, and the absence means that the driver uses real time.
   */
  driverClock?: AgentRuntimeWorkflowDriverDeps["clock"];
}

export {
  isDynamicWorkflowTaskLinkStore,
  resolveDynamicWorkflowJournalStore,
  supportsRunIntrospection,
  type DynamicWorkflowTaskLinkStore,
} from "./dynamic-workflow-run-journal.js";

/**
* Complete aspect of this service: {@link DynamicWorkflowRunPort} (for engine/tool ​​layer) plus two **session-level** life cycles
* Read the surface. The only consumer of the latter is the host's provider registry security boundary:
* The child agents share the live adapter of the parent session, and a running run is an active loop of the parent session——
* registry replace must wait for it to settle.
 */
interface DynamicWorkflowRunService extends DynamicWorkflowRunPort {
/** The number of runs that have not been settled at this moment (`terminal === undefined` entries in the registry). */
  countLiveRuns(): number;
  /**
* Subscription settlement: Notified exactly after each run enters the final state (completed/errored/stopped, including failure before launch)
* Once, the bookkeeping has been completed (the counting has deducted it). Return the unsubscribe function. If the listener throws an error, it will only be logged and will not affect settlement.
   */
  subscribeRunSettled(listener: (notice: DynamicWorkflowRunSettledNotice) => void): () => void;
  /**
* Stop every on-the-fly run owned by this service and wait for them to resolve (header invariant 7). **not in contracts
* {@link DynamicWorkflowRunPort}**: It is a life cycle action of the host App, not a capability of the engine and tool layer.
* Idempotent - the second call returns the same promise and no longer aborts anything.
   */
  close(): Promise<void>;
}

/**
* Create workflow run service. Return {@link DynamicWorkflowRunService}: port implementation + two session-level
* Lifecycle reading (on-the-fly run count, settlement subscription), the latter does not enter the narrow port of contracts - they serve
* The host's registry security boundary, not the engine and tool layer.
 */
export function createDynamicWorkflowRunService(
  deps: DynamicWorkflowRunServiceDeps,
): DynamicWorkflowRunService {
  const runs = new Map<string, RunRegistryEntry>();
  /**
* Upgraded Q&A docking table. **One card, across all flying runs under the name of this service**:
* This is why qid is globally unique - `resolveQuestion` only accepts one opaque token, allowing the model to pair itself when multiple runs are run concurrently.
* `(runId, qid)` is a breeding ground for mismatches. Pure memory, the same as the parked deferred (empty when the process dies, rely on resume
* After the actor re-asks for self-healing; persisting a pending table will only lie).
   */
  const escalations: WorkflowEscalationRegistry = createWorkflowEscalationRegistry();

//Construction means convergence: There are zero runs running under this instance at the moment, so the non-terminal lines of this session are the relics of the dead process.
// See header invariant 4 with {@link reconcileOrphanRuns}.
  reconcileOrphanRuns(deps);

  /**
* The concurrency ceiling of this process.
* The same implementation as the bucket ceiling of the process-level manager: the run upper bound and the bucket ceiling always match.
   *
* There are four readers in this service, all passing through this function: the caps starting point of the new run, the caps on the port
* {@link DynamicWorkflowRunPort.concurrencyCeiling} (the tool layer controls and determines whether it is worth mentioning or not),
* The `maxConcurrency` criterion for the two read planes, and the derived fields on the `run-started` payload.
   */
  const concurrencyCeiling = (): number =>
    resolveWorkflowConcurrencyCeiling(deps.availableParallelism);

// caps only contains the concurrency upper bound, no wall clock timeout; cancellation is the only way to stop.
// The upper bound **starts from the ceiling** and can only be lowered by requests and never raised——
// Absence is the ceiling, if given, it will be clamped to [1, ceiling].
  const caps = (requested?: number): Caps => {
    return { maxConcurrency: clampRunConcurrency(requested, concurrencyCeiling()) };
  };

//The introspection interface is connected according to the capability detection (those four queries are not on the engine port). In their absence, the following two optional members are not implemented at all:
// The consumer's `typeof port.listRuns === "function"` detection is therefore false, and the tool layer normalizes it to
// The business failure of "This session does not have this capability" - instead of a silent empty list.
  const introspection = supportsRunIntrospection(deps.journal) ? deps.journal : undefined;

//The implementation of life cycle bookkeeping (settlement/closing/closing the gate/external final state lines leaving traces; file header invariants 6 and 7) is in
// dynamic-workflow-run-lifecycle.ts: They only borrow the registry and journal here and pass them through narrow dependencies.
  const {
    countLiveRuns,
    subscribeRunSettled,
    trackSettlement,
    close,
    assertOpen,
    noteForeignTerminalRow,
  } = createRunServiceLifecycle({
    journal: deps.journal,
    ...(deps.logger === undefined ? {} : { logger: deps.logger }),
    parentSessionId: deps.parentSessionId,
    ...(deps.registerResidencyBlockingWork === undefined
      ? {}
      : { registerResidencyBlockingWork: deps.registerResidencyBlockingWork }),
    runs,
  });

//The implementation of the two entries (submit / resume) is in dynamic-workflow-run-submit.ts: they only borrow from here
//The registration table, docking table, caps and settlement bookkeeping are passed explicitly through ctx, and these two paragraphs will no longer be copied in this file.
  const entryContext: DynamicWorkflowRunEntryContext = {
    deps,
    runs,
    escalations,
    caps,
    trackSettlement,
  };

  return {
    countLiveRuns,
    subscribeRunSettled,
    close,

// Closing doors for three entrances. `async` just turns this wiring error into a rejection rather than a synchronous throw; the gate is related to the subsequent
// There is no await between delegates, so the "registration and launch in the same synchronization slice" of invariant 6 are not affected.
    async submit(
      request: DynamicWorkflowRunSubmitRequest,
    ): Promise<DynamicWorkflowRunSubmitResult> {
      assertOpen();
      return submitDynamicWorkflowRun(entryContext, request);
    },

    async amend(request: DynamicWorkflowRunAmendRequest): Promise<DynamicWorkflowRunAmendResult> {
      assertOpen();
      return amendDynamicWorkflowRun(entryContext, request);
    },

    /**
* The concurrency ceiling of this process (see {@link DynamicWorkflowRunPort.concurrencyCeiling} for the port contract).
* The starting point of caps is **the same** {@link concurrencyCeiling}: the value clamped out by the tool layer must be followed by the port
* The values ​​dropped into the library are equal, otherwise the number displayed in the confirmation window will not be the number that will take effect.
     */
    concurrencyCeiling,

    async resume(runId: string): Promise<DynamicWorkflowRunResumeResult> {
      assertOpen();
      return resumeDynamicWorkflowRun(entryContext, runId);
    },

    async listRunsForSession(limit?: number): Promise<DynamicWorkflowRunSessionSummary[]> {
      if (!supportsRunEnumeration(deps.journal)) {
// Query without enumeration (such as memory journal): The empty list is the honest answer - run in the memory journal is
// The process will not survive, and there is nothing that can be restored after restarting.
        return [];
      }
      const capped = Math.max(1, Math.min(limit ?? DEFAULT_LIST_RUNS_LIMIT, MAX_LIST_RUNS_LIMIT));
      let rows: DwfRunSessionListItem[];
      try {
        rows = deps.journal.listRunsByParentSession(deps.parentSessionId, capped);
      } catch (error) {
        deps.logger?.warn?.("Dynamic workflow run enumeration failed", {
          errorMessage: error instanceof Error ? error.message : String(error),
          event: "dynamic_workflow.run.list_failed",
          module: "bootstrap.app",
        });
        return [];
      }
// Live entries are handed over to the summary together: session enumeration and snapshot/list/details share the same priority (rule three),
// Otherwise, this side will display a run marked by external writing.
      return rows.map((row) => toSessionSummary(row, runs.get(row.runId)));
    },

    async replayProgressForSession(input: {
      excludeRunIds: ReadonlySet<string>;
    }): Promise<DynamicWorkflowRunProgressPayload[]> {
      if (!supportsRunEnumeration(deps.journal)) return [];
      let rows: DwfRunSessionListItem[];
      try {
// The upper bound is the same constant as the projection elimination: cold state = the state that a long-lived process will hold at this moment.
        rows = deps.journal.listRunsByParentSession(
          deps.parentSessionId,
          WORKFLOW_RUNS_LIMITS.maxRuns,
        );
      } catch (error) {
        deps.logger?.warn?.("Dynamic workflow run replay enumeration failed", {
          errorMessage: error instanceof Error ? error.message : String(error),
          event: "dynamic_workflow.run.replay_failed",
          module: "bootstrap.app",
        });
        return [];
      }
      const payloads: DynamicWorkflowRunProgressPayload[] = [];
// The latest update of the enumeration surface is first; the oldest is prioritized for playback, and the elimination of reducers is the same as the arrival order during live.
      for (const row of [...rows].reverse()) {
// Runs that already have events in the caller's memory (ran by this process) and runs that are in the registry will not be played back:
// Their events are all in the memory store. Feeding them again will only cause run-started to return the phase to the starting point.
        if (input.excludeRunIds.has(row.runId) || runs.has(row.runId)) continue;
// The ceiling and the live side have the same origin: the `run-started` payload of cold playback must be equal to the live one byte by byte.
        payloads.push(...replayRunProgress(row, deps.journal, concurrencyCeiling()));
      }
      return payloads;
    },

    async getTask(taskId: string): Promise<DynamicWorkflowRunSnapshot | undefined> {
      noteForeignTerminalRow(taskId);
      return snapshotOf(
        taskId,
        runs,
        deps.journal,
        escalations.pendingFor(taskId),
        concurrencyCeiling(),
      );
    },

    /**
* run archived script (see {@link DynamicWorkflowRunPort.getScript} for the port contract). Registry entry first:
* In the gap between submit → createRun, there are no rows in the journal yet, and the entries are the same bytes. The journal one too
* The hash of resume checks the read bytes, so `AmendWorkflow` is used, `script_unchanged` preflight and resume
* There are no two answers to "What is this run doing?"
     */
    async getScript(runId: string): Promise<string | undefined> {
      return runs.get(runId)?.scriptText ?? deps.journal.getRun(runId)?.scriptText;
    },

    async waitForTask(
      taskId: string,
      options?: { signal?: AbortSignal },
    ): Promise<DynamicWorkflowRunSnapshot | undefined> {
      const entry = runs.get(taskId);
// Not in the registry: It may be a previous run of this process (recorded in the journal) or completely unknown. Neither can wait.
      if (entry === undefined) {
        return snapshotOf(
          taskId,
          runs,
          deps.journal,
          escalations.pendingFor(taskId),
          concurrencyCeiling(),
        );
      }
      if (entry.terminal === undefined) await settleOrAbort(entry.settlement, options?.signal);
// Parked items are reprojected after **waiting**: while waiting, the question may have been answered or removed with cancellation.
      return snapshotOf(
        taskId,
        runs,
        deps.journal,
        escalations.pendingFor(taskId),
        concurrencyCeiling(),
      );
    },

    /**
* Answer a blocking question after actor upgrade. Table lookup → driver settlement → `escalate` tool result becomes this paragraph
* Answer, the actor's turn continues in place. **The run state remains unchanged during the whole process**: The upgrade is a slow tool call inside ask.
* Not a run life cycle event (by design, escalate does not terminate/freeze the run).
     *
* The judgment and copy of the three types of structured rejections are in the registry (it only knows that a qid has never existed and has been answered,
* Still removed along with ask). This method deliberately does not add a second judgment - once in each place, the same qid will sooner or later be
* Get different interpretations on both layers.
     */
    async resolveQuestion(
      qid: string,
      answer: string,
    ): Promise<DynamicWorkflowResolveQuestionResult> {
      return escalations.resolve(qid, answer);
    },

    async cancel(
      runId: string,
      initiator: DynamicWorkflowRunCancelInitiator = "user",
    ): Promise<boolean> {
      const entry = runs.get(runId);
// Unknown run (or settled) has nothing to abort. Return false to normalize the upper layer into structured not_found,
// Instead of reporting a cancellation that didn't happen.
      if (entry === undefined || entry.terminal !== undefined) return false;
// abort is the only "true stop" in the harness: abort the flying ask, kill child process, the engine passes stop(initiator)
// Settlement stopped (completed journal entries retained → can be resumed). The reason for abort is the initiator——
// Harness reads `signal.reason` to decide whether stopped(user) or stopped(model), and then the reason will be stored in the database.
// No longer only lives in the background task registry.
      entry.controller.abort(initiator);
      return true;
    },

    async listEvents(
      runId: string,
      options: DynamicWorkflowRunEventPage,
    ): Promise<DynamicWorkflowRunEvent[]> {
      const page = deps.journal.listEvents(runId, {
        ...(options.afterSequence === undefined ? {} : { afterSequence: options.afterSequence }),
        ...(options.limit === undefined ? {} : { limit: options.limit }),
      });
      return page.map((stored) => toProtocolEvent(stored.sequence, stored.event));
    },

    /**
* User interface product list for this run. UI cold recovery and hub details
* durable pronunciation: `workflowRuns.artifacts` projection is memory-only, will be empty after restart, and the version history
* The persistent home is always the `kind = "artifact"` line of the journal.
     *
* The entire merge rule is reused {@link artifactsOf} - the final state snapshot (`getTask`) and this method must give **the same copy**
* The list, merged into two parts, will branch sooner or later at the point of "does the failed line count as one version".
     *
* ⚠ Terminology: The artifact here is the output of the script published to the user, not the `output` on the port (the top level of the script
* Return value, also called artifact inside the engine).
     *
* Unknown runId and "This journal has no product reading page" both return `undefined`: they are the same business fact to the caller.
     */
    async listArtifacts(runId: string): Promise<readonly DynamicWorkflowRunArtifact[] | undefined> {
      return artifactsOf(runId, deps.journal).artifacts;
    },

    /**
* Feed the `report` entries of a certain preset product, and page them in ascending order according to the journal sequence (the access side of the Kanban board).
* Out-of-bounds cursor gets an empty page instead of an error - turning to the tail is the normal page turning ending.
     */
    async listArtifactItems(
      runId: string,
      artifactId: string,
      page: DynamicWorkflowRunArtifactItemPage,
    ): Promise<readonly DynamicWorkflowRunArtifactItem[]> {
      return listArtifactItemsFrom(deps.journal, runId, artifactId, {
        ...(page.afterSequence === undefined ? {} : { afterSequence: page.afterSequence }),
        limit: page.limit,
      });
    },

    /**
* Read the bytes of a certain product version. Authorization chain (run belongs to this session ∧ journal has completed row of this version ⇒ Get
* The entire uri on line) is in {@link readWorkflowArtifactBytes}, where the link diagram is drawn.
     *
* `parentSessionId` uses the one of **this service (= the session of this app), and deliberately does not accept parameters: the service instance itself
* Constructed according to the parent session, letting the caller pass any session is equivalent to opening a cross-session read hole - the same as `listRunsForSession`
* Argument, it’s just that there are enumerations on that side and bytes on this side.
     */
    async readArtifact(
      runId: string,
      artifactId: string,
      version: number,
    ): Promise<DynamicWorkflowRunArtifactBytes | undefined> {
      return readWorkflowArtifactBytes(
        {
          journal: deps.journal,
          parentSessionId: deps.parentSessionId,
          ...(deps.artifactStore === undefined ? {} : { artifactStore: deps.artifactStore }),
        },
        runId,
        artifactId,
        version,
      );
    },

    /**
* List and text of the workspace transcript. authorization chain
* Same as readArtifact (run belongs to this session), both are taken - the args in the list are already the path and command line.
* The entire section is in dynamic-workflow-run-workspace.ts.
     */
    async listWorkspaceNodes(
      runId: string,
    ): Promise<readonly DynamicWorkflowRunWorkspaceNode[] | undefined> {
      return listWorkspaceNodesFrom(
        { journal: deps.journal, parentSessionId: deps.parentSessionId },
        runId,
      );
    },

    async readWorkspaceNodeResult(
      runId: string,
      siteId: string,
      ordinal: number,
      query: DynamicWorkflowRunWorkspaceNodeResultQuery,
    ): Promise<DynamicWorkflowRunWorkspaceNodeResult | undefined> {
      return readWorkspaceNodeResultFrom(
        { journal: deps.journal, parentSessionId: deps.parentSessionId },
        runId,
        siteId,
        ordinal,
        query,
      );
    },

    ...(introspection === undefined
      ? {}
      : createRunIntrospectionMethods({
          introspection,
          journal: deps.journal,
          parentSessionId: deps.parentSessionId,
          runs,
          escalations,
          concurrencyCeiling,
        })),
  };
}
