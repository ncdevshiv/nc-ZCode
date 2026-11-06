// ============================================================
// Observation surface assistance of Dynamic Workflow Run (read-only half of run service)
// ============================================================
//Composition rules for snapshots/lists/details. Unpacked from dynamic-workflow-run-service.ts: the service file carries the entrance and gate,
// This file carries the pure synthesis rules of "registry + journal → external reading" (no I/O, no state).

import type { DwfRunListItem, DwfRunSessionListItem } from "@zcode/adapters/storage";
import {
  boundDynamicWorkflowRunEventPayload,
  type DynamicWorkflowRunError,
  type DynamicWorkflowRunLogEntry,
  type DynamicWorkflowRunPendingQuestion,
  type DynamicWorkflowRunSessionSummary,
  type DynamicWorkflowRunSnapshot,
  type DynamicWorkflowRunStopReason,
  type DynamicWorkflowRunSummary,
} from "@zcode/contracts";
import type {
  JournalStorePort,
  NodeRecord,
  RunRecord,
  RunSettlement,
  RunStatus,
  RunStopReason,
  StoredEvent,
  WorkflowErrorCode,
  WorkflowErrorJson,
} from "@zcode/dynamic-workflow";
import { artifactsOf } from "./dynamic-workflow-run-artifact-projection.js";
import { readRunScriptPath, readRunSubagentModel } from "./dynamic-workflow-run-launch-anchor.js";
import { resolveDynamicWorkflowRunLabel } from "./dynamic-workflow-run-label.js";
import { lineageFields, supersededByOf } from "./dynamic-workflow-run-lineage.js";

/**
* Product merging lives in dynamic-workflow-run-artifact-projection.ts (this file reaches the 400-line limit of oxlint).
* Export as it is instead of changing each of the four call points. Import: They are looking for the "observation surface", and this split is the result of the row number constraint.
* is not a boundary change - turning it into a cross-module rename would just make git blame point to a commit that has nothing to do with the intent.
 */
export { artifactsOf };

/** Registry entry: A run that is on the fly or recently settled. */
export interface RunRegistryEntry {
  controller: AbortController;
  startedAt: Date;
  toolCallId?: string;
  parentSessionId?: string;
  /**
* Three memory copies of submit time metadata, only serving the microtask gap of **submit → createRun**: that moment
* There is no row in the journal yet, and the enumeration interface must be able to filter this run by project (cwd) and give the label (name /
* first line of script). Once the row appears, the journal is the authority among these three, and the memory copy is no longer read.
   */
  cwd: string;
  name?: string;
  scriptText: string;
  /**
* The actual concurrency upper bound of this run (memory copy of `dwf_run.caps_max_concurrency`, the same gap argument).
* submit / amend is dropped, **resume is not dropped** - that path uses the caps in the journal record, and its row is already there.
   */
  maxConcurrency?: number;
  /**
* The subagent model of this run (the specification picker string on the `run-launched` event
* An in-memory copy of `providerId/modelId[$reasoningLevel]`).
* The same gap argument: `AmendWorkflow`'s resolveInput reads snapshot and determines "what to use", while revising a just-started
* run happens to fall within the microtasks of the submit → engine event.
   *
* **Three ways to create an entry are all valid**: submit / amend uses the string just returned, resume reads the event header and copies it over.
* The value of the run under construction is written to death in that life, and the current run remains unchanged for the rest of its life, so the copy and the event cannot be forked - there is only one rule left for reading:
* If there are entries, the entries will be read. Only cold operations (this process has no entries) will scan the events. Absent means that the subagent runs on the session model.
   */
  subagentModel?: string;
  /**
* The script file for this run (a memory copy of the absolute path on the `run-launched` event). Same rules as {@link subagentModel}:
* The three ways to create an entry are all valid (submit / amend uses the one given by the input parameter, resume reads the event header and copies it),
* The value of run is written to death in that life and remains unchanged in the rest of the life, so the copy and the event cannot be forked. Absence means that this run has no files.
   */
  scriptPath?: string;
/** Revision run's precursor (memory copy of `dwf_run.resumed_from`, the only place the enumeration side can read before the journal line appears). */
  resumedFrom?: string;
  /**
* The starting point of usage for this run (`spentTokens` after precursor settlement). The same gap argument: the two reading pages before the row fall into the library can only read the usage from the entry, while the revision
* A just-started run happens to fall into those few microtasks. **Only amend path value**: The new submit starts from zero,
* The resume line is already there.
   */
  inheritedTokens?: number;
/** Settle the promise; waitForTask waits for it. The fire-and-forget chain hangs here. */
  settlement: Promise<RunSettlement>;
/** The final state when settled (the product/error is only here, the journal does not store the script return value). */
  terminal?: RunSettlement;
  completedAt?: Date;
  /**
* The "journal line is an foreign final state" log has been logged once for this entry (see {@link synthesizeRunStatus}'s
*Priority description). The tracker polls once every second, and if you don't remember this flag, you will get the same warn every second.
   */
  foreignTerminalLogged?: true;
}

/**
* The final state set of run (excluding pending / running).
* Here is the **only authority** on "what is the final state": the SQL on the journal side only does index-friendly pre-screening, and this set is used before convergence
* Judge it again, so that when adding a non-final state in the future, only this part needs to be changed.
 */
export const TERMINAL_RUN_STATUSES: ReadonlySet<RunStatus> = new Set<RunStatus>([
  "completed",
  "errored",
  "stopped",
]);

/**
* Synthesize snapshot from journal record + live registry. When neither of them has this run, undefined is returned (the upper layer is normalized to lost).
 *
* `pendingQuestions` is updated by the caller from the **memory** to the parking table projection and then passed in (the rules of this file are no I/O,
* stateless). Passing an empty array means "there are no questions to answer at this moment", and the field is completely absent - no empty array is sent.
 *
* `concurrencyCeiling` gives the same reason to the caller (reading it requires checking the process core = I/O): absence means "this reading does not determine the ceiling",
* `maxConcurrency` whole field is not output - see {@link runConcurrencyField}.
 */
export function snapshotOf(
  taskId: string,
  runs: Map<string, RunRegistryEntry>,
  journal: JournalStorePort,
  pendingQuestions: readonly DynamicWorkflowRunPendingQuestion[] = [],
  concurrencyCeiling?: number,
): DynamicWorkflowRunSnapshot | undefined {
  const entry = runs.get(taskId);
  const record = journal.getRun(taskId);
  if (entry === undefined && record === undefined) return undefined;

// The only implementation of status synthesis (see {@link synthesizeRunStatus}). The vocabulary of the snapshot surface is not pending.
// So fold in the last step: runStatusToTaskStatus reports pending and running together as running——
// This is exactly the existing behavior of "running in the registry" (without it, the run that was just submitted will be judged as lost in the first poll).
  const status: DynamicWorkflowRunSnapshot["status"] = runStatusToTaskStatus(
    synthesizeRunStatus(entry, record?.status),
  );

// The node line is only scanned **once**, and reports and artifacts are shared (both only take numbers in the final state, see their respective comments).
// Separate listNodes is to decode the full table of a 256-node run twice.
  const nodes = status === "running" ? undefined : journal.listNodes(taskId);

// True final state word + stop reason + structured failure: snapshot base class
// `status` is a common vocabulary for background task trackers (stopped is folded into canceled, errored is folded into failed), notification
// To tell the truth, you can only read these three fields. It is not taken before the final state (there is no ending to say yet).
  const runStatus = synthesizeRunStatus(entry, record?.status);
  const terminal = TERMINAL_RUN_STATUSES.has(runStatus);
  const stopReason = terminal ? stopReasonOf(entry, record) : undefined;
  const failure = terminal ? terminalErrorField(runStatus, entry, record).error : undefined;
  const error = failure?.message;
// Attribution: AmendWorkflow's
// resolveInput reads the snapshot to determine "whether it is the run of this session"; see lineageFields for the pointers at both ends of the lineage.
  const parentSessionId = entry?.parentSessionId ?? record?.parentSessionId;

  return {
    runId: taskId,
    taskId,
    startedAt: entry?.startedAt ?? new Date(0),
    status,
    ...(terminal ? { runStatus } : {}),
    ...(stopReason === undefined ? {} : { stopReason }),
    ...(parentSessionId === undefined ? {} : { parentSessionId }),
    ...lineageFields(entry?.resumedFrom ?? record?.resumedFrom, supersededByOf(entry, record)),
// The memory copy takes precedence over the journal, which is the same as each gap field above: in the microtasks of submit → createRun
// The row doesn't exist yet, and AmendWorkflow's resolveInput happens to read this snapshot at that time.
    ...runConcurrencyField(
      entry?.maxConcurrency ?? record?.caps.maxConcurrency,
      concurrencyCeiling,
    ),
// Subagent model: **If there is an entry, read the entry** (the three paths to create an entry are all invalid, see RunRegistryEntry.subagentModel),
//Only for cold runs - there are no entries in this process - the event header is scanned once (a bounded scan of eight entries, not the entire journal).
// It does not have the criterion of "whether it is worth mentioning" as the concurrency upper bound: it only exists when the user explicitly sets it, and the presence itself is everything
// information. Absent is running on the session model.
    ...runSubagentModelField(
      entry === undefined ? readRunSubagentModel(journal, taskId) : entry.subagentModel,
    ),
// Script file: The same as the sub-agent model one by one (if there is an entry, read the entry, and only scan the event header once for cold operations), the same
// "Only present if you have a demerit". The final notification says the next step is to "edit that file in place", so the snapshot must take it with it.
    ...runScriptPathField(
      entry === undefined ? readRunScriptPath(journal, taskId) : entry.scriptPath,
    ),
    ...(failure === undefined ? {} : { failure }),
    ...(entry?.completedAt === undefined ? {} : { completedAt: entry.completedAt }),
    ...(error === undefined ? {} : { error }),
// When there are zero entries, the entire field is absent (same as reports): the reading side makes the entire pending area disappear based on this, and does not render empty sections.
    ...(pendingQuestions.length === 0 ? {} : { pendingQuestions }),
    ...reportsOf(nodes),
//User surface product. ⚠ with the immediate `output`
// (`entry.terminal.artifact` = the top-level return value of the script, also called artifact inside the engine) are **two different things**:
// Here is the output of the script published to the user through `artifact.*`, and there is the return value for the model to see.
    ...(nodes === undefined ? {} : artifactsOf(taskId, journal, nodes)),
    ...(entry?.terminal?.status === "completed"
      ? { output: entry.terminal.artifact }
      : // 重启后本进程的注册表是空的，产物只能从 journal 记录取（journal 行的
// result_json). The memory final state takes priority in the previous one - it is the original value that this process just took over from the engine.
        record?.status === "completed" && record.result !== undefined
        ? { output: record.result }
        : {}),
  };
}

/**
* `maxConcurrency` on both reading surfaces (`getTask` snapshot and `getRunDetail` details)
 *
* **Only present if below the current ceiling**: There is nothing to say about running on the ceiling - it is the default behavior, and every line with
* A number equal to the default value will only cause the model to read "no limit" as "set a limit". The same applies when the ceiling is absent (the caller did not provide it)
* The entire field cannot be output: there are no criteria, and reporting a number is just guessing.
 *
* One implementation is shared by two reading planes: each judgment is made once, and "whether it is equal to the ceiling is counted as present" will branch sooner or later during a certain parameter adjustment.
 */
export function runConcurrencyField(
  applied: number | undefined,
  ceiling: number | undefined,
): { maxConcurrency?: number } {
  if (applied === undefined || ceiling === undefined || applied >= ceiling) return {};
  return { maxConcurrency: applied };
}

/**
* `subagentModel` on two reading surfaces (`getTask` snapshot and `getRunDetail` details)
 *
* There is only one rule: **Be present only after setting**. Unlike {@link runConcurrencyField}, there is no comparable default -
* "Run on session model" is not a string that can be written into this field, and filling in the current session model will cause the reader to interpret "not set"
* Pronounced as "set, exactly equal to conversation model", the two have different meanings in the three states of amend.
 *
* One implementation is shared by two reading surfaces, and it is the same argument as the concurrency upper bound.
 */
export function runSubagentModelField(subagentModel: string | undefined): {
  subagentModel?: string;
} {
  return subagentModel === undefined ? {} : { subagentModel };
}

/**
* `scriptPath` on both reads. rules and
* {@link runSubagentModelField} is verbatim: **Only present**, no comparable default - "This run does not
* "Script file" is not a path that can be written into this field. Filling in a guessed path will cause the model to edit a path similar to this run
* Unrelated files. One implementation is shared by two reading surfaces.
 */
export function runScriptPathField(scriptPath: string | undefined): { scriptPath?: string } {
  return scriptPath === undefined ? {} : { scriptPath };
}

/**
* `reports` on the final state snapshot: the node line with `kind = "report"` in the journal (write once, constant `completed`,
* The reported item is on `result`), in insertion order = reporting order.
 *
* Why read from journal instead of projection: `workflowRuns.reports` is a bounded memory-only display
* (empty after cold recovery), and these rows are the **persistent homes** for those entries. Completion notification should be on failed / canceled
* The same carrying product - a run that died on the 12th ask still completed the work of the 11th ask, and retrieving it is exactly
* The reason why `report` exists - so it must read the persistent copy.
 *
* Only read in **final state**: `getTask` will be polled repeatedly by the background tracker, while `listNodes` is a full table scan (one
* A run of 256 nodes will decode 256 lines per poll). The only consumers are final notifications and final TaskOutput,
* Reading it on the fly has no readers, only cost. This criterion is now executed by the caller - the absence of `nodes` means "on the fly, don't read",
* So that the same scan feeds {@link artifactsOf} at the same time.
 */
function reportsOf(nodes: readonly NodeRecord[] | undefined): { reports?: readonly unknown[] } {
  if (nodes === undefined) return {};
  const items = nodes.filter((node) => node.kind === "report").map((node) => node.result);
// The entire field is absent when there are zero entries: the notification end will accordingly make the entire section `<reports>` disappear and no empty sections will be sent.
  return items.length === 0 ? {} : { reports: items };
}

/**
* **The only implementation of state truth synthesis**, four levels of priority:
* Memory final state > live entry (journal state is only accepted in non-final state) > journal state > "in the registry but not yet active".
 *
* When the process engine is still alive, the final line in the journal may come from the orphan convergence of another instance, and local tracing cannot be terminated based on this.
* When the local settlement has been made, the memory final state is used first; the second stage processes the live entries that have not yet been settled, ignoring the external final state row and waiting for the engine to settle.
* Avoid notifying the model in advance and discarding the real completion results after marking the task.
 *
* It is shared by three reading surfaces (snapshot, list, details). The reason why journal's {@link RunStatus} vocabulary is returned instead of
* Snapshot: Which one is the most detailed - `pending` (submitted, the engine has not yet been built). Introspection is the difference that the model can understand, and
* Snapshot folds it into `running` only with its own vocabulary limit ({@link runStatusToTaskStatus} takes care of folding).
* On the other hand (fold it first and then find a way to restore it), you have to guess "what kind of running is this" downstream.
 */
function synthesizeRunStatus(
  entry: RunRegistryEntry | undefined,
  journalStatus: RunStatus | undefined,
): RunStatus {
// Memory final state priority: The settlement that this process just received from the engine is newer than the journal line (which may not have been written yet).
  if (entry?.terminal !== undefined) return terminalRunStatus(entry.terminal);
// Go here `entry !== undefined` is "the live run held by this service": the final state on the line can only be external writing,
// Ignore it and run as it is. Non-final lines (pending/running) are accepted as usual - they are written by the engine itself.
// Deliberately block only the final status: `journalStatus`. The gap of absence still falls into the lower `pending` (that level is finer,
// And the introspection side treats it as a difference that the model can understand).
  if (
    entry !== undefined &&
    journalStatus !== undefined &&
    TERMINAL_RUN_STATUSES.has(journalStatus)
  ) {
    return "running";
  }
  if (journalStatus !== undefined) return journalStatus;
// Registry exists, journal does not exist: submit → createRun microtask gap. `pending` is the **only honest** at this moment
// Status - The run has been accepted, but the engine has not dropped any lines yet.
  return "pending";
}

function terminalRunStatus(settlement: RunSettlement): RunStatus {
  switch (settlement.status) {
    case "completed":
      return "completed";
    case "stopped":
      return "stopped";
    default:
      return "errored";
  }
}

/**
* Logical final state → Common vocabulary for background task trackers. The tracer (shared with the bash / subagent task) is not recognized
* stopped / errored: stopped is folded into `cancelled`, errored is folded into `failed`; a snapshot of the real word
* `runStatus` / `stopReason` revealed separately.
 */
function runStatusToTaskStatus(status: RunStatus): DynamicWorkflowRunSnapshot["status"] {
  switch (status) {
    case "completed":
      return "completed";
    case "errored":
      return "failed";
    case "stopped":
      return "cancelled";
// pending / running are still running.
    default:
      return "running";
  }
}

/**
* Reason for stopping: memory final state takes priority (the settlement that this process just took over from the engine), followed by the journal line. Only meaningful for stopped;
* The rest of the status returns undefined.
 *
* Same priority as {@link synthesizeRunStatus}: **No stop reason under active entry**. The line says one, it can only be
* External writing, and the three reading surfaces must give the same answer - the status says "running",
* The reason says "interrupted", which is more difficult to find out than if both are wrong.
 */
function stopReasonOf(
  entry: RunRegistryEntry | undefined,
  record: { status?: RunStatus; stopReason?: RunStopReason } | undefined,
): DynamicWorkflowRunStopReason | undefined {
  if (entry?.terminal !== undefined) {
    return entry.terminal.status === "stopped" ? entry.terminal.reason : undefined;
  }
  if (entry !== undefined) return undefined;
  if (record?.status === "stopped") return record.stopReason ?? "user";
  return undefined;
}

/**
* journal line (+ optional memory entry) → common section for lists and details.
 *
* Timestamp **Read the journal line directly**, deliberately bypassing the `new Date(0)` of {@link snapshotOf}. The secret: the memory entry is
* The starting time after eviction is false, and the time in the introspection plane is the basis for the model to determine "how long ago".
 */
export function journalRunSummary(
  row: DwfRunListItem,
  entry: RunRegistryEntry | undefined,
  ownerSessionId: string,
): DynamicWorkflowRunSummary {
  const ownedByThisSession = entry !== undefined || row.parentSessionId === ownerSessionId;
// What is read is the ** status of **journal instead of the synthesized one: the semantics is "journal says it is not over yet, and this session
// Unable to confirm". (The two must be equal on this branch - if it is not in this session or in the registry, there is no memory final state that can be overwritten.
// journal - written literally so that this assertion will still hold when new sources of truth are added in the future. )
  const possiblyInterrupted = !TERMINAL_RUN_STATUSES.has(row.status) && !ownedByThisSession;
  const stopReason = stopReasonOf(entry, row);
  return {
    runId: row.runId,
    ...resolveDynamicWorkflowRunLabel({
      runId: row.runId,
      ...(row.name === undefined ? {} : { name: row.name }),
      ...(row.scriptText === undefined ? {} : { scriptText: row.scriptText }),
    }),
    status: synthesizeRunStatus(entry, row.status),
    ...(stopReason === undefined ? {} : { stopReason }),
    ...lineageFields(entry?.resumedFrom ?? row.resumedFrom, supersededByOf(entry, row)),
    ownedByThisSession,
// Only present if true: absence reads "no doubt", while `false` will cause each line to have a noise field.
    ...(possiblyInterrupted ? { possiblyInterrupted: true } : {}),
    createdAt: row.timeCreated,
    updatedAt: row.timeUpdated,
  };
}

/**
* Microtask gap for run with only memory entries (submit → createRun) → common section.
 *
* Attribution is always true (it is in the session's registry), so `possiblyInterrupted` is never used. The timestamp is taken from
* Registration time - this is not a complete guess, but the real submission time of this run (the same millisecond is written when the journal line is dropped
* of `Date.now()`).
 */
export function registryRunSummary(
  runId: string,
  entry: RunRegistryEntry,
): DynamicWorkflowRunSummary {
  return {
    runId,
    ...resolveDynamicWorkflowRunLabel({
      runId,
      ...(entry.name === undefined ? {} : { name: entry.name }),
      scriptText: entry.scriptText,
    }),
    status: synthesizeRunStatus(entry, undefined),
    ...(entry.terminal?.status === "stopped" ? { stopReason: entry.terminal.reason } : {}),
    ...lineageFields(entry.resumedFrom, supersededByOf(entry, undefined)),
    ownedByThisSession: true,
    createdAt: entry.startedAt.getTime(),
    updatedAt: (entry.completedAt ?? entry.startedAt).getTime(),
  };
}

/**
* Only completed run will bring the product, and `undefined` product = **all fields are absent** (same as completion notification).
 *
* The priority is the same as {@link snapshotOf}: the artifact in the final memory state is the **original value** that this process just took over from the engine.
* The journal's result_json is its state after a JSON round trip; entries are evicted before falling back to the latter.
* The value itself** is handed over as is, not serialized** - model-oriented text projection has the only implementation in core, and the port will be done again
* "The same product looks different in the notification and in the tool" appears.
 */
export function terminalResultField(
  status: RunStatus,
  entry: RunRegistryEntry | undefined,
  row?: { result?: unknown },
): { result?: unknown } {
  if (status !== "completed") return {};
  if (entry?.terminal?.status === "completed" && entry.terminal.artifact !== undefined) {
    return { result: entry.terminal.artifact };
  }
// `result: null` is a legal product (`ask<T | null>` will return it), so the criterion is `!== undefined`
// Instead of truthfulness - the decoding side also only writes out the key if the column is non-NULL.
  return row?.result === undefined ? {} : { result: row.result };
}

/**
* errored means constant failure; stopped only applies to provider / interrupted (user / model stops without failure).
* journal's `failure_json` is **authoritative**: it has structured code, `Interrupted` (process died),
* `ProviderStop` (model-side deterministic error) and `DriverError` (script true failure) can therefore be distinguished by the model - so
* The code is exposed here as it is, never collapsed into a generic failure; the structured details of `ProviderStop` are also exposed.
 */
export function terminalErrorField(
  status: RunStatus,
  entry: RunRegistryEntry | undefined,
  row?: { failure?: WorkflowErrorJson },
): { error?: DynamicWorkflowRunError } {
  if (status !== "errored" && status !== "stopped") return {};
// Memory final state priority: The settlement zone that this process just received from the engine has its original value (ProviderStop details are complete), and the journal line is
// Its state after a JSON round trip; the entry is evicted before falling back to the latter.
  const memoryError =
    entry?.terminal?.status === "errored"
      ? entry.terminal.error
      : entry?.terminal?.status === "stopped"
        ? entry.terminal.error
        : undefined;
  if (memoryError !== undefined) {
// The path that failed before engine construction (the child process cannot spawn, the construction throws an error) is a wrapped one in the registry.
// General Error - there may be no code at runtime. Return DriverError instead of coding a new one: failure of that path
// It does come from the driver layer outside the engine.
    const code = typeof memoryError.code === "string" ? memoryError.code : "DriverError";
    const providerStop = (memoryError as { providerStop?: WorkflowErrorJson["providerStop"] })
      .providerStop;
    return {
      error: {
        code,
        message: memoryError.message,
        ...(providerStop === undefined ? {} : { providerStop }),
      },
    };
  }
  if (row?.failure !== undefined) {
    return {
      error: {
        code: row.failure.code,
        message: row.failure.message,
        ...(row.failure.providerStop === undefined
          ? {}
          : { providerStop: row.failure.providerStop }),
      },
    };
  }
  return {};
}

/**
* A `log` event in a journal → the logTail item of the port.
 *
* Messages are bounded by {@link boundDynamicWorkflowRunEventPayload} rather than sliced ​​by themselves: string upper limit
* (2048) The truncation rules for surrogate security have been written once on the port side. If you copy them the second time, they will be forked during a certain parameter adjustment.
* The query has been filtered by `type='log'` pushdown, so non-log events can only come from implementation drift - in this case give an empty message instead
* Collapse the entire details page.
 */
export function toLogTailEntry(stored: StoredEvent): DynamicWorkflowRunLogEntry {
  const message = stored.event.type === "log" ? stored.event.message : "";
  const { payload } = boundDynamicWorkflowRunEventPayload({ message });
  return {
    sequence: stored.sequence,
    message: typeof payload.message === "string" ? payload.message : "",
// The drop-in time crosses the boundary as it is; it is absent from the old journal without this column (the age will not be given on the reading side accordingly).
    ...(stored.timeCreated === undefined ? {} : { at: stored.timeCreated }),
  };
}

/** Wait for settlement, but respect the caller's signal (waiting to be interrupted does not mean that the run is canceled). */
export async function settleOrAbort(
  settlement: Promise<unknown>,
  signal?: AbortSignal,
): Promise<void> {
  if (signal === undefined) {
    await settlement;
    return;
  }
  if (signal.aborted) return;
  await new Promise<void>((resolve) => {
    const onAbort = (): void => resolve();
    signal.addEventListener("abort", onAbort, { once: true });
    void settlement.then(
      () => {
        signal.removeEventListener("abort", onAbort);
        resolve();
      },
      () => {
        signal.removeEventListener("abort", onAbort);
        resolve();
      },
    );
  });
}

/**
* Failure encoding for orphan convergence writes (host-level members, never produced by the engine itself). Dropped together with `stopped(interrupted)`:
* status + stopReason has explained "process death interrupt", this code is the second evidence of the same fact (the only one in the old line).
 */
export const INTERRUPTED_FAILURE_CODE: WorkflowErrorCode = "Interrupted";

/**
* The only predicate of the resume gate: **stopped can be resumed**,
* Except for `superseded` - its unfinished work has been owned by the successor, and replaying is equivalent to doing the same thing twice for the workspace that the successor is changing;
* errored / completed is not possible. {@link DynamicWorkflowRunSessionSummary.resumable} using the same -
* If the UI presses status to re-derive by itself, the two predicates will one day be inconsistent: the button is on but the command is rejected.
 */
export function isResumableRecord(record: Pick<RunRecord, "status" | "stopReason">): boolean {
  return isResumableSettlement(record.status, record.stopReason);
}

/**
* The "settled fact" form of the same predicate: `run-settled` The `resumable` bit on the payload is calculated by it (live by
* toProgressPayload is calculated based on engine events, cold playback is calculated based on journal rows by reseeding), and the reducer only transfers.
 */
export function isResumableSettlement(status: RunStatus, stopReason?: RunStopReason): boolean {
  return status === "stopped" && stopReason !== "superseded";
}

/**
* Enumeration → Session enumeration summary. Three things are deliberately the same as elsewhere:
* - `resumable` uses the same predicate of the resume gate ({@link isResumableRecord});
* - `label` uses the derivation chain shared by the two reading surfaces ({@link resolveDynamicWorkflowRunLabel}) - the enumeration surface
* If you try to figure it out yourself, the same run will display different names in `/dwf list` and the tool card;
* - `updatedAt` reads directly the `timeUpdated` of the journal line (`RunRecord` does not carry time, so the input parameter is an enumeration).
 */
export function toSessionSummary(
  row: DwfRunSessionListItem,
  entry?: RunRegistryEntry,
): DynamicWorkflowRunSessionSummary {
  const { label } = resolveDynamicWorkflowRunLabel({
    runId: row.runId,
    ...(row.name === undefined ? {} : { name: row.name }),
    ...(row.scriptText === undefined ? {} : { scriptText: row.scriptText }),
  });
// The fourth read plane also follows the same priority ({@link synthesizeRunStatus}): the final state on the downstream line of the live entry is external writing,
//The status is reported as running, and the reason for stopping and failure are not displayed. Without this one, the conversation list will show only one that has been "stopped"
// run, and the snapshot/list/details all say it is running.
  const status = synthesizeRunStatus(entry, row.status);
  const stopReason = stopReasonOf(entry, row);
  const live = entry !== undefined && entry.terminal === undefined;
  return {
    runId: row.runId,
    ...(row.toolCallId === undefined ? {} : { toolCallId: row.toolCallId }),
    label,
    updatedAt: row.timeUpdated,
    status,
    ...(stopReason === undefined ? {} : { stopReason }),
    ...lineageFields(row.resumedFrom, supersededByOf(entry, row)),
    ...(live || row.failure?.code === undefined ? {} : { failureCode: row.failure.code }),
    ...(live || row.failure?.message === undefined ? {} : { failureMessage: row.failure.message }),
    resumable: isResumableSettlement(status, stopReason),
  };
}
