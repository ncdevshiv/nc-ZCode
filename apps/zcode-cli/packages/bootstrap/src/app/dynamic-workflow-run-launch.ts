// ============================================================
// The startup execution body of Dynamic Workflow Run (the shared half of submit and resume)
// ============================================================
// submit (new runId) and resume (existing runId, the engine will
//resume branch) is completely isomorphic in the section "Assembly driver → Run runWorkflowScript", and this file is shared:
// Each of the two entries maintains a copy. Sooner or later, one of them forgets to intercept the event sequence or rehydrate the actor.
//
// This file holds three things:
// 1. Journal sequence interception (emit side gets the sequence of the event just appended);
// 2. RunEvent → Session event payload (bounded + two derived fields);
// 3. Actor runtime access: new (drop session line + task link) or rehydrated (resumeFromStore).

import { randomUUID } from "node:crypto";
import {
  boundDynamicWorkflowRunEventPayload,
  CoreErrorType,
  ZCODE_DWF_CHILD_COMMAND,
  type CreateSessionTaskLinkInput,
  type DynamicWorkflowRunEvent,
  type DynamicWorkflowRunProgressPayload,
  type SessionId,
} from "@zcode/contracts";
import type { AgentRuntime } from "@zcode/core";
import { parseModelPickerValue, type ModelSelection } from "@zcode/shared/model-selection";
import {
  type ActorSubmitProfile,
  refToString,
  validate,
  type ActorRef,
  type AskSpec,
  type Caps,
  type ImportedRunCache,
  type JournalStorePort,
  type JsonSchema,
  type RunEvent,
  type RunSettlement,
  type ValidateFn,
} from "@zcode/dynamic-workflow";
import { runWorkflowScript } from "@zcode/dynamic-workflow-runtime";
import { createJournalSequenceCapture } from "./dynamic-workflow-run-sequence-capture.js";
import { isResumableSettlement } from "./dynamic-workflow-run-observation.js";
import {
  readRunLaunchAnchor,
  readRunSubagentModel,
  type RunLaunch,
} from "./dynamic-workflow-run-launch-anchor.js";
import { resolveWorkflowConcurrencyCeiling } from "./workflow-concurrency-ceiling.js";
import { createAgentRuntimeWorkflowDriver, mintActorSessionId } from "./workflow-driver.js";
import type { WorkflowEscalationRegistry } from "./workflow-escalation-registry.js";
import type { DynamicWorkflowRunServiceDeps } from "./dynamic-workflow-run-service.js";

/** Adapt the validator in the adaptation package to the ValidateFn contract of the engine (launch is the only call point for runWorkflowScript). */
const validateFn: ValidateFn = (schema, value) => validate(schema as JsonSchema, value);

/** All products of one compilation. Four consumers share the same ts.Program (compiled once, see run service invariant 2). */
export interface CompiledDynamicWorkflowScript {
  lowered: string;
  scriptHash: string;
  askSpecs: Map<string, AskSpec>;
/** Set of approved commands for world.run (compile-time literal collection). */
  declaredRunCommands: ReadonlySet<string>;
/** Submit profile for each actor site. */
  actorSubmitProfiles: ReadonlyMap<string, ActorSubmitProfile>;
}

interface LaunchDynamicWorkflowRunInput {
  caps: Caps;
  compiled: CompiledDynamicWorkflowScript;
  cwd: string;
  deps: DynamicWorkflowRunServiceDeps;
/** The display name of the run (optional `input.name` of `CreateWorkflow`): dropped by EngineConfig at createRun dwf_run.name. */
  name?: string;
  /**
* The actual parameters of this run (the verified parameter bag of saved workflow). Two places to go: drop with EngineConfig
* `dwf_run.args_json`, and the `args` global for spawn payload injection into the sandbox.
   *
* The resume branch passes the copy read back from the journal, not the one newly given by the caller - see run service
* resume annotation.
   */
  args?: Record<string, unknown>;
  parentSessionId?: string;
  runId: string;
  scriptText: string;
  signal: AbortSignal;
  toolCallId?: string;
  /**
* Revise the lineage pointer of resume running (`dwf_run.resumed_from`) and the import cache. Both appear in pairs:
* The pointer is a row-level fact (the UI is "continued from run
   *
* The same path as the rest of the metadata - launch → harness → EngineConfig, zero processing in the middle. Build (read predecessor journal,
* Walking the `resumed_from` chain, parsing the transcription source) all happens in run service: this is already the execution side, put the build in
* This is equivalent to letting resume and submit be constructed once each (and they must be two calls of the same pure function).
   */
  resumedFrom?: string;
  importedCache?: ImportedRunCache;
  /**
* The starting point of usage when building a run: `spentTokens` after the precursor is settled.
* The same path as the lineage pointer - launch → harness → EngineConfig, zero processing in the middle; the amend path is given,
* New submit and resume are absent (the latter's usage is resumed from existing rows).
   */
  inheritedTokens?: number;
  /**
* The anchor point of the round where the run is initiated. The submit path is given (the engine is under construction run
* That `run-launched`); resume path is absent, this function reads back from journal - both paths use the same value
* `actor-created` / `run-settled` progress events derive `launchInputId`. The submit path is also declared with a script.
* The phase table (`phaseNames`) and the subagent model of this run (`subagentModel`, standard picker string) are the same.
* Only when building the run journal, the resume path will be read back from that event.
   */
  launch?: RunLaunch;
  /**
* Upgraded the docking registry for Q&A. One is held by run service,
* Across all flying runs under its name, the two entries (submit/resume) pass the **same object** - the registration form is based on the complete qid
* Index, holding one in each of the two entries will make the run after resume unable to answer the question you just asked.
   */
  escalationRegistry: WorkflowEscalationRegistry;
}

/**
* Start (or resume) a run: assembly sequence intercept → emit hook → real driver → runWorkflowScript.
* The semantics of fire-and-forget are determined by the caller (this function only returns the settlement promise and does not do registry bookkeeping).
 */
export function launchDynamicWorkflowRun(
  input: LaunchDynamicWorkflowRunInput,
): Promise<RunSettlement> {
  const {
    args,
    caps,
    compiled,
    cwd,
    deps,
    escalationRegistry,
    importedCache,
    name,
    parentSessionId,
    resumedFrom,
    runId,
    scriptText,
    signal,
    toolCallId,
  } = input;
  const childSpawn = dynamicWorkflowChildSpawn();
// Anchor point: given by submit (this build run) or in journal (resume). pre-upgrade run has neither → absent,
// The progress event does not have launchInputId, and the subagent does not report it.
  const launch = input.launch ?? readRunLaunchAnchor(deps.journal, runId);
// lineage pointer: submit/amend path is given by the input parameter; resume path input parameter is absent (createRun has been hard-coded), from
// Journal line readback - `run-started` payloads for both paths are therefore identical.
  const lineageFrom = resumedFrom ?? deps.journal.getRun(runId)?.resumedFrom;
// Concurrency ceiling: second host-derived field of `run-started` payload. Counted once for each launch instead of for each event
// Once - it is a process fact, the number of cores will not change after a run reaches half of the cores, and `availableParallelism()` is a system call.
  const concurrencyCeiling = resolveWorkflowConcurrencyCeiling(deps.availableParallelism);
// Subagent model: given by submit (same as anchor) or in journal (resume is read back from the same run-launched).
// The same argument as the anchor, so the two paths are isomorphic; pre-upgrade run has neither side → absent = runs on the session model.
  const subagentModel = input.launch?.subagentModel ?? readRunSubagentModel(deps.journal, runId);
// String → selection, parsed once per launch. The progress payload takes the original string (toProgressPayload below),
//The actor runtime factory wants structured selection (including reasoning gear, the two identities of pin cannot be brought back).
  const runSubagentModel =
    subagentModel === undefined ? undefined : parseModelPickerValue(subagentModel);

// The journal sequence of events is only known by appendEvent, and the engine is in record()
// `journal.appendEvent(...)` is **synchronized** immediately followed by `driver.emit(...)` and discards the returned
// StoredEvent(engine.ts). So here is a layer of journal to cut off the assigned sequence:
// What emit gets must be the one just now. The alternatives are worse - the local incrementing counter will be in resume(sequence
// The overall offset is when continuing from the existing maximum value), and checking the journal once for each event is a waste of IO.
// The premise of "append follows emit, one-to-one correspondence" is nailed by the test: the sequence sequence seen by the hook must be consistent with
// The items returned by listEvents are equal one by one.
  const sequenceCapture = createJournalSequenceCapture(deps.journal);

  const makeDriver = createAgentRuntimeWorkflowDriver({
    journal: sequenceCapture.journal,
    emit: (event) => {
// Event fanout must not hang up run: the hook is an observer, and exceptions are swallowed at this boundary and logged.
      try {
        if (deps.onRunEvent === undefined) return;
        deps.onRunEvent(
          toProgressPayload({
            event,
            runId,
            sequence: sequenceCapture.sequenceOf(event),
            ...(toolCallId === undefined ? {} : { toolCallId }),
            ...(launch === undefined ? {} : { launchInputId: launch.inputId }),
            ...(lineageFrom === undefined ? {} : { resumedFrom: lineageFrom }),
            concurrencyCeiling,
            ...(subagentModel === undefined ? {} : { subagentModel }),
          }),
// Separate routing and payload: the event must fall in the session that initiated the run, and parentSessionId is
//The only basis for judging "whether it is that session".
          parentSessionId === undefined ? {} : { parentSessionId },
        );
      } catch (error) {
        deps.logger?.warn?.("Dynamic workflow run event hook failed", {
          errorMessage: error instanceof Error ? error.message : String(error),
          event: "dynamic_workflow.run_event.hook_failed",
          module: "bootstrap.app",
          runId,
        });
      }
    },
    executionPort: deps.executionPort,
    fileSystemPort: deps.fileSystemPort,
    escalationRegistry,
    cwd,
//The placement point of the user interface product. Session scope fetch** this service
// The ** parent session (= the session of this app) instead of the optional parentSessionId in the launch input parameter: both are
// The same value in production (run start passes the runtime's own sessionId), but only the former is required, and
// "Which session directory the bytes are written to" should not have an undefined branch. When store is absent, the entire pair will not be transmitted.
// The driver side therefore rejects it loudly as ArtifactStoreUnavailable.
    ...(deps.artifactStore === undefined
      ? {}
      : {
          artifactStore: deps.artifactStore,
          parentSessionId: deps.parentSessionId as SessionId,
        }),
    declaredRunCommands: compiled.declaredRunCommands,
//Which type of submit_result (typed/generic/none) should be used by each actor site is determined during compilation.
    actorSubmitProfiles: compiled.actorSubmitProfiles,
    runId,
//Both boundary accounting and seed replication both read and write actor session messages (on the driver side, see the file header of workflow-driver.ts).
    ...(deps.actorTranscriptStore === undefined
      ? {}
      : { actorTranscriptStore: deps.actorTranscriptStore }),
    ...(deps.logger === undefined ? {} : { logger: deps.logger }),
// Narrow port of the process-level concurrency manager: when present driver gives each
// actor runtime is a request-level access port (the runtimeFactory is downloaded as is below); its absence means that the actor is not subject to the gate.
    ...(deps.concurrency === undefined ? {} : { concurrency: deps.concurrency }),
// Test the injected driver clock (fault matrix); in the absence of production, the driver takes the real time.
    ...(deps.driverClock === undefined ? {} : { clock: deps.driverClock }),
    runtimeFactory: async ({
      sessionId,
      actor,
      persona,
      escalatePort,
      seed,
      submitPort,
      submitProfile,
      modelRequestAdmission,
    }) => {
      const runtime = deps.createActorRuntime({
        runId,
        sessionId,
        actor,
        persona,
        submitPort,
//The factory determines whether the port is injected and whether the declaration is typed (createActorRuntime of create-app.ts) based on the profile.
        submitProfile,
//The request-level admission port and the two tool ports are downloaded to the runtime deps via the same path.
        ...(modelRequestAdmission === undefined ? {} : { modelRequestAdmission }),
// The upgrade port and the submit port are transmitted through the same path: the registration gate on the core side is subject to the existence of the port, so it is always transmitted.
        escalatePort,
//Resume pin: Which model this actor ran on last time. Must be read before **creating the runtime,
// Because the following line journalActorResolvedModel will write this round of parsing results back to the same field.
        //
// The pin brought by the seed is **inherit** (amend-resume): when the run is first distributed, the journal of this run is still there
// There is no analysis result, and the pin can only come from the precursor - the silent change model under transcription continuation is exactly the identity mutation that pin needs to prevent. Both are there
//Resume in this run (revised after run crash) is based on the record of this run: that is the actual situation of this actor in **this run**
// The run model is more specific than the precursor, and the two should be equal. The loud failure of misshapen pins follows the established routine.
// (WorkflowActorPinnedModelError of workflow-actor-model.ts), no forking here.
        pinnedModel:
          pinnedActorModel({ actor, journal: deps.journal, runId }) ?? seed?.resolvedModel,
// Subagent model for this run: above pin ** (priority table in workflow-actor-model.ts). It is the user's
// The explicit statement of run this time (AmendWorkflow with subagent_model is the explicit statement of "resume when changing model"
//Determine), pin just respects the implicit default without it. Unlike pin, it goes down the whole line with reasoning gear——
// The pin of the journal only remembers two paragraphs of identity.
        ...(runSubagentModel === undefined ? {} : { runSubagentModel }),
      });
// Which model the persona's model gear is actually parsed into can only be determined accurately by the built runtime (see gear mapping)
// workflow-actor-model.ts). Drop the database first and then connect to the session: a failed session persistence will cause the ask to fail.
// But the audit fact "which model was selected at that time" is still left in the journal. The rehydrate path must also be written——
// When the pin is absent (the old run before the upgrade), this round is the first time that the parsing results can be recorded.
      journalActorResolvedModel({
        actor,
        journal: deps.journal,
        selection: requireActorModelSelection(runtime, actor),
        runId,
      });
      const attached = await attachActorSession({
        actor,
        deps,
        runId,
        runtime,
        sessionId,
      });
      if (attached === "rehydrated") return runtime;
      await persistActorSession({
        actor,
        deps,
        parentSessionId,
        runId,
        runtime,
        sessionId,
      });
      return runtime;
    },
  });

  return runWorkflowScript({
    askSpecs: compiled.askSpecs,
    caps,
// The spawn strategy must be changed under SEA, and will not be passed under non-SEA.
    ...(childSpawn === undefined ? {} : { childSpawn }),
    cwd,
    lowered: compiled.lowered,
    makeDriver,
// When the entry file cannot be written into the project `.zcode/`, the harness falls back to the OS temporary directory and reports a sound - run and starts as usual.
// But this log is the only clue to troubleshoot "why there is no workflow-runs archive in the project".
    onWarning: (warning) => {
      deps.logger?.warn?.("Dynamic workflow entry file fell back to the OS temp dir", {
        event: "dynamic_workflow.entry_file.fallback",
        module: "bootstrap.app",
        runId,
        ...warning,
      });
    },
// name and scriptText are in the same metadata path: harness is transferred to EngineConfig as it is (resume journal
// Hit short-circuit createRun, passing it is harmless and keeps the submit/resume two launch inputs in the same shape).
    ...(name === undefined ? {} : { name }),
// Actual participation name / scriptText The same metadata path, but one more destination: harness is transferred to EngineConfig
// (fall args_json), also put spawn payload into the sandbox.
    ...(args === undefined ? {} : { args }),
    ...(parentSessionId === undefined ? {} : { parentSessionId }),
    runId,
// What is included in the library is the author's original text and its hash, not the lowered function body (the comparison benchmark of resume is the original text).
    scriptHash: compiled.scriptHash,
    scriptText,
// The associated anchor point is dropped with run: after restarting, the tool card join and resume notifications can only restore it from dwf_run.
    ...(toolCallId === undefined ? {} : { toolCallId }),
//Revision and continuation: drop the lineage pointer into the library (createRun is written once) and import the cache injection engine (pure data, zero core I/O).
    ...(resumedFrom === undefined ? {} : { resumedFrom }),
    ...(importedCache === undefined ? {} : { importedCache }),
// The usage starting point is the same as the cache: the engine writes it as the initial value of spent_tokens when creatingRun, and ignores it when it hits an existing row.
    ...(input.inheritedTokens === undefined ? {} : { inheritedTokens: input.inheritedTokens }),
// The anchor point is only used when building the journal (the door on the engine side) during run. It is harmless to pass it when resume.
    ...(launch === undefined ? {} : { launch }),
    signal,
    validate: validateFn,
  });
}

/**
* The spawn strategy of the sandbox process: SEA hides subcommands from re-exec when going down, otherwise it will not express its position (harness defaults
 * `node --max-old-space-size=… <entry>`）。
 *
* SEA single file binary does not interpret Node CLI flags, harness default argv
* `--max-old-space-size` will fall into the strict parseArgs of CLI as it is, and the child process will immediately report an error and exit - **Everyone under SEA
*workflow run must fail**. The revision method is the same as the official plugin host
* (`officialPluginHostPrefixArgs` of official-plugin-runtime.ts).
 *
* SEA determines to stay in bootstrap without sinking to harness: harness is app-free (only depends on
* `@zcode/dynamic-workflow` and node built-in), neither can get the subcommand constants of contracts nor should they know it
* Which host you are packaged by. `isSea` can be injected only for testability - the default probe must return false in the test process.
* So "non-SEA cannot bring argsPrefix" is also an assertable fact.
 */
export function dynamicWorkflowChildSpawn(
  isSea: boolean = isSeaRuntime(),
): { argsPrefix: readonly string[] } | undefined {
  return isSea ? { argsPrefix: [ZCODE_DWF_CHILD_COMMAND] } : undefined;
}

/** SEA runtime probe (local image of official-plugin-runtime.ts private helper with the same name, deliberately not reused across files). */
function isSeaRuntime(): boolean {
  const getBuiltinModule = process.getBuiltinModule as
    | ((id: "node:sea") => { isSea(): boolean })
    | undefined;
  try {
    return getBuiltinModule?.("node:sea").isSea() === true;
  } catch {
    return false;
  }
}

/**
* resume rehydration: journal has recorded the session id of the actor ⇒ This is a re-hang (the session lines and messages have already been dropped,
* `mintActorSessionId` is purely deterministic, so the sessionId is the current one, and there is mutual verification on the driver side).
* `resumeFromStore` rebuilds messageHistory from the dropped message/part line - interrupted tool calls will be
* Hydrator is nailed to "[Tool execution was interrupted before resume]", which is the correct semantics of killed ask.
 *
* `SessionNotFound` (session line cleared) is not an error but a documented exception: fall back to a new persistence path and the actor starts over with an empty context - more honest than letting the entire run freeze.
* The rest of the exceptions are thrown up as they are (the lower layer does not swallow errors, house rule).
 */
async function attachActorSession(input: {
  actor: ActorRef;
  deps: DynamicWorkflowRunServiceDeps;
  runId: string;
  runtime: AgentRuntime;
  sessionId: SessionId;
}): Promise<"fresh" | "rehydrated"> {
  const { actor, deps, runId, runtime } = input;
  const journaledSessionId = deps.journal.getActor(runId, actor.siteId, actor.ordinal)?.sessionId;
  if (journaledSessionId === undefined) return "fresh";
  try {
    await runtime.resumeFromStore();
// Both the session line and the task link were dropped in the previous life (both were upsert), and will not be rebuilt if they are hung again.
    return "rehydrated";
  } catch (error) {
    if (!isSessionNotFound(error)) throw error;
    deps.logger?.warn?.("Dynamic workflow actor session pruned; starting fresh", {
      actor: refToString(actor),
      event: "dynamic_workflow.actor.rehydrate_fallback",
      module: "bootstrap.app",
      runId,
      sessionId: input.sessionId,
    });
    return "fresh";
  }
}

/** SessionNotFound of structured judgment core (does not rely on error text for process judgment). */
function isSessionNotFound(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    (error as { type?: unknown }).type === CoreErrorType.SessionNotFound
  );
}

/**
* Actor session realization: drop the session line + create a task link (a demonstration of the legacy path is in
 * script-workflow-runtime.ts）。
 *
* The order is payload-friendly: `session_task_link.child_session_id` has FK for `session(id)`,
* So you must first drop the session line and then create the link. This is why ActorRuntimeFactory allows returning Promises.
 *
* Here ** no longer ** subscribes to the events of the actor runtime: the live broadcast channel is installed during the ** construction period ** of the child runtime
* (eventSink of script-workflow-child-runtime.ts → external sink set of parent runtime). old
* The `subscribeEvents` channel is installed after the following line `ensureSessionPersistedForExternalActivity`,
* And it will write SessionTitleUpdated as sequenceNumber 1 - v4 gateway only drains consecutive seq,
* So the subscription starts from seq 2 and waits forever for a seq 1 that will never come again, and the transcript is permanently blank.
 */
async function persistActorSession(input: {
  actor: ActorRef;
  deps: DynamicWorkflowRunServiceDeps;
  parentSessionId?: string;
  runId: string;
  runtime: AgentRuntime;
  sessionId: SessionId;
}): Promise<void> {
  const { actor, deps, parentSessionId, runId, runtime, sessionId } = input;
  const title = `workflow subagent ${refToString(actor)}`;

  await runtime.ensureSessionPersistedForExternalActivity(title);

  if (deps.taskLinkStore) {
    await deps.taskLinkStore.createSessionTaskLink({
      childSessionId: sessionId,
      id: `tasklink_${randomUUID()}`,
// rootWorkflowRunId is intentionally left blank. **Verified Fact** (not a guess): The column is declared as
      // `root_workflow_run_id text references workflow_run(id)`（migration 0007，
// See session_task_link table creation), pointing to the **legacy** workflow_run table, and the records of workflow run are in
// dwf_run; migration runner is on pragma foreign_keys = on. Detected against the real store:
// Calling with workflow runId will result in `FOREIGN KEY constraint failed`, if omitted, it will succeed. So run as identity
//The following path column without FK. Don't "fix" it back.
      //
// path is a mini-contract: `dwf/<runId>/<siteId>@<ordinal>`. runId is character set safe; siteId segment
// Keep original `#`/`@` (free text column, and sanitized form in session id). **No code today
// Parse it** - it serves human reading and possible future prefix scanning ("all actor sessions for this run").
// If you want to build an index query based on run id in the future, a reconstruction is required
// The migration of session_task_link changes or removes FK.
      path: `dwf/${runId}/${refToString(actor)}`,
      ...(parentSessionId === undefined ? {} : { parentSessionId: parentSessionId as SessionId }),
// Deliberately differentiate from legacy "workflow_agent" (script-workflow-runtime.ts) instead of reusing:
// The two are different groups of people. The root_workflow_run_id of the legacy row points to workflow_run and is not empty, the dwf row
// This column is always empty and the run identity is in path. Sharing a role value will cause "fetch rows by role and then dereference"
// The consumer of root_workflow_run_id" gets null from the dwf row. The column is `text not null`, no CHECK,
// There is no enum, and there is no zod on the contract side (checked), so the new value is legal.
      role: "workflow_actor",
      status: "running",
    } satisfies CreateSessionTaskLinkInput);
  }
}

/**
* Write the model on which the actor actually runs into the journal (`ActorRecord.resolvedModel`, which falls into dwf_actor
* resolved_model column).
 *
* Why it is necessary to **read and modify**: `putActor` is the replacement of the entire record, and several other fields of this record (name /
*persona/sessionId) is not part of this function; directly writing a record with only resolvedModel will replace what the engine just wrote
* Freeze persona erase.
 *
* Why is driver profile: Which model the subagent runs on is the host fact (the choice of the parent session at the time), the engine is in createActor
* The persona cannot be seen at the moment when time synchronizes with it. The two putActors on the engine side will bring this field as it is, see
* engine.ts/scheduler.ts for dynamic-workflow.
 */
export function journalActorResolvedModel(input: {
  actor: ActorRef;
  journal: JournalStorePort;
  selection: ModelSelection;
  runId: string;
}): void {
  const { actor, journal, selection, runId } = input;
  const existing = journal.getActor(runId, actor.siteId, actor.ordinal);
  journal.putActor({
    ...(existing ?? { runId, siteId: actor.siteId, ordinal: actor.ordinal }),
    resolvedModel: formatActorResolvedModel(selection),
  });
}

/** The way of writing `resolvedModel` in journal is: `providerId/modelId`, which is opposite to the way of reading pin (workflow-actor-model.ts). */
function formatActorResolvedModel(selection: ModelSelection): string {
  return `${selection.providerId}/${selection.modelId}`;
}

/**
* The created actor runtime must already have model selection: child inherits the current selection of the parent session (script-workflow-child-runtime.ts),
* When the parent session has no selection, it cannot even send the first model request. Fail loudly here instead of leaving "no model selected" as an empty pin.
 */
function requireActorModelSelection(runtime: AgentRuntime, actor: ActorRef): ModelSelection {
  const selection = runtime.getSessionModelSelection();
  if (selection === undefined) {
    throw new Error(
      `Actor session has no model selection and cannot record resolvedModel: ${actor.siteId}#${actor.ordinal}`,
    );
  }
  return selection;
}

/**
* The resume pin reads: the `resolvedModel` (`providerId/modelId`) recorded in the journal by this actor.
 *
* Only resume will read the value: the engine saves this field carry-forward when replaying `createActor`;
* The new run has no parsing results at the moment when the runtime factory is running, so it is naturally absent. **Must be read before building runtime**,
* Because `journalActorResolvedModel` will then write the parsing of this round back to the same field - reading it late will change the result of this round
*Mistaken as a pin from the previous round. Which one takes precedence between pin and this run's subagentModel, see workflow-actor-model.ts
* (run is selected above; pin only keeps the default when no run is selected, persona freezes the persistent half of the invariant).
 */
function pinnedActorModel(input: {
  actor: ActorRef;
  journal: JournalStorePort;
  runId: string;
}): string | undefined {
  return input.journal.getActor(input.runId, input.actor.siteId, input.actor.ordinal)
    ?.resolvedModel;
}

/**
* RunEvent → Mapping of protocol events (**This comment is the contract**): `type` takes the discriminant of the event, `payload` is the same
* The remaining fields of the event object after removing `type` are bounded by {@link boundDynamicWorkflowRunEventPayload}.
* Deliberately not reshape the field names - the reader (details page event log) interprets the payload according to the event type, and the engine's vocabulary is the schema.
 *
* Types actually emitted by the engine: run-started/actor-created/node-queued/node-dispatched/
 * node-repairing / node-nudged / node-settled / usage-updated / log / report / phase-entered /
 * run-settled。
* (`executing` is not an observable event; `compaction` v1 is never emitted.) There are two other types emitted by **driver**,
* Events that follow the same two tracks: escalation-raised / escalation-resolved (upgrade bridge of workflow-driver.ts).
 *
* Adding an event type has zero changes in **this function**. This is exactly what you get by "not reshaping the field name": `type` takes the discriminant,
* payload is the remaining fields, there are no branches by type to omit. **But downstream does have a per-kind switch**:
* `applyWorkflowRunEvent` of `zcode-protocol-v4/product-projection.ts` is reduced class by class, its
* The formal parameter of `eventType` is `string` instead of `RunEvent["type"]`. If a tsc is missing, it will not be reported - when adding the event type
* To read that switch, you cannot count on the compiler.
 */
export function toProtocolEvent(sequence: number, event: RunEvent): DynamicWorkflowRunEvent {
  const { type, ...rest } = event;
  const { payload, truncated } = boundDynamicWorkflowRunEventPayload(
    rest as Record<string, unknown>,
  );
  return { sequence, type, payload, ...(truncated ? { truncated } : {}) };
}

/**
* RunEvent → Session event payload. `payload` is byte-for-byte identical to {@link toProtocolEvent} (once serialized,
* two consumers), plus two **derived fields**.
 *
* The placement of the derived fields **outside** the payload is intentional: the payload must remain the same as "what the engine sent", otherwise the event log
* Just showcasing our processed products. Each of the two fields is not an observable fact, but without them, the downstream can only recreate a contract by itself:
 *
* - `actorSessionId`: Boundary C's actor-created without a session id (it is cast by the driver). let
* The renderer presses (runId, actorRef) to spell it by itself, which is equivalent to copying the sanitize contract into the UI layer; call it here
* Casting it's **the same function**, it's impossible for both sides to drift (testing is nailed for equality).
* (There used to be a second derived field `spentTokens`: the old budget-updated only sent the remaining amount. Now usage-updated
* Carrying the total amount spent by itself, it is generated in the same synchronization step as dwf_run.spent_tokens, and no longer needs to be derived. )
 */
export function toProgressPayload(input: {
  event: RunEvent;
  runId: string;
  sequence: number;
  toolCallId?: string;
/** Anchor inputId for run; only derived on actor-created / run-settled (the two moments at which the subagent is owned). */
  launchInputId?: string;
/** Revised precursor to run; only derived on `run-started` (card's "Adjusted from run X"). */
  resumedFrom?: string;
  /**
* Process concurrency ceiling at the moment this payload is cast; only derived on `run-started`
   *
* The engine event only brings its own `caps.maxConcurrency`, and "this value is not worth displaying" to compare it with the ceiling——
* The ceiling is the host facts (machine cores) that the engine neither sees nor should see. Projection basis `caps.maxConcurrency <
* concurrencyCeiling` Write down the own upper bound of this run, and then take min (shared cap, upper bound of this run) for the concurrency chip of the UI.
   */
  concurrencyCeiling?: number;
  /**
* Subagent model for this run (canonical picker string); only derived on `run-started`, and **only present** when set.
* Unlike `concurrencyCeiling`, it does not need to be compared to any default value:
* The engine doesn't know about this at all (the model is entirely on the host side), so the absence means "the subagent runs on the session model".
* Cold replay is given the same key from the same `run-launched` event, so both sides of the payload are byte-for-byte equal.
   */
  subagentModel?: string;
}): DynamicWorkflowRunProgressPayload {
  const {
    event,
    runId,
    sequence,
    toolCallId,
    launchInputId,
    resumedFrom,
    concurrencyCeiling,
    subagentModel,
  } = input;
  const protocolEvent = toProtocolEvent(sequence, event);
  return {
    runId,
    ...(toolCallId === undefined ? {} : { toolCallId }),
    sequence,
    eventType: protocolEvent.type,
// `run-settled` has one more `resumable`:
// The predicate of the resume gate is only available in the CLI. The projection and UI only carry this bit and will never deduce it by status.
// Predicate = stopped ∧ Not superseded; cold playback has converged on orphans
// Rows for the same key - two chains, one predicate (isResumableSettlement). stopReason / supersededBy is exposed as is with the event payload.
// `run-started` has more `resumedFrom`: the engine event does not have it (the engine does not read lineage), but the card needs to draw this edge.
// There is also `concurrencyCeiling` in the same crack: the engine only sends its own caps, and "is this upper bound the default value?"
// Compare it to the host's ceiling (see field comments above). The two are not related to each other, and each is absent.
    payload:
      event.type === "run-settled" && isResumableSettlement(event.status, event.stopReason)
        ? { ...protocolEvent.payload, resumable: true }
        : event.type === "run-started"
          ? {
              ...protocolEvent.payload,
              ...(resumedFrom === undefined ? {} : { resumedFrom }),
              ...(concurrencyCeiling === undefined ? {} : { concurrencyCeiling }),
              ...(subagentModel === undefined ? {} : { subagentModel }),
            }
          : protocolEvent.payload,
    ...(protocolEvent.truncated ? { truncated: true } : {}),
    ...(event.type === "actor-created"
      ? { actorSessionId: mintActorSessionId(runId, event.actor) }
      : {}),
// The third derived field: downstream only on these two events
//An anchor point is required - actor-created registers the sub-agent ownership, run-settled settles all sub-agents of the run.
    ...((event.type === "actor-created" || event.type === "run-settled") &&
    launchInputId !== undefined
      ? { launchInputId }
      : {}),
  };
}
