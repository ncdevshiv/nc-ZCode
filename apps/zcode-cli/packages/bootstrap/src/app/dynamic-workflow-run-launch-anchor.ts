// ============================================================
//Initiation anchor point of dwf run
// ============================================================
// The agent_step of the subagent should be classified under "the round that initiated this run". The anchor is an inputId,
// Live in the journal event `run-launched` with zero SQL (only once in the run generation):
// - Parsed from this file when submitting (active wheel / value of direct start casting / anchor point of front-wheel drive run / value of pocket casting)
// Hand it over to the engine for storage;
// - resume and progress event derived fields are read back from the journal, and the anchor points of the same run are therefore unique across the life cycle.
//
// The same event also carries three pieces of the same host metadata: the phase table (`phaseNames`) declared by the script, the run subagent's
// Which file does the selection (`subagentModel`) and script come from (`scriptPath`). All four are only written once in the life of the build run, the engine does not read at all, and they all have zero SQL - `dwf_run`.
// There is no corresponding column.

import type { TraceContext } from "@zcode/contracts";
import type { JournalStorePort } from "@zcode/dynamic-workflow";
import { uuidv7 } from "@zcode/shared";

export interface RunLaunchAnchor {
/** The inputId of the round that initiated the run (the hub directly starts as the cast UUID v7). */
  inputId: string;
}

/**
* Give the engine all the contents recorded in `run-launched`: anchor point + stage table declared by the script + selection of this run subagent (`subagentModel`, standard picker string
 * `providerId/modelId[$reasoningLevel]`）。
 *
* Neither of the latter two are part of the anchor point: the revised continuation uses the inputId of the predecessor, but uses the stage table of the new script, which will never be used.
* The model of the predecessor ("Omit it and use the predecessor" is the three states of the tool surface, normalized by AmendWorkflow's resolveInput)——
* So they are merged side by side with the anchor in submit, rather than being stuffed into {@link resolveLaunchAnchor}.
 */
export interface RunLaunch extends RunLaunchAnchor {
  phaseNames?: string[];
  subagentModel?: string;
  /**
* The absolute path of this run script file. and stage table,
* Sub-agent selection does not belong to the anchor point as usual: the revision records which file the script of **this revision** comes from, and the predecessor will never be used.
   */
  scriptPath?: string;
/** The "running at the same time" table aligned with `phaseNames` by position (the subscript points to the same table); also only the journal of the run phase is being built. */
  phaseAlongside?: number[][];
}

/**
* Read back the anchor point of run from journal: the first `run-launched`. The run initiated before the upgrade does not have this event → `undefined`,
* The caller does not derive `launchInputId` based on this (the actual layer does not send `workflow.lifecycle`, the sub-agent does not report it, and it does not rebuild it).
 *
* The anchor point immediately follows the first `run-started` (the engine's recording order), so only the first few entries in the journal are read, and the entire journal is not read.
* Read into memory; `RUN_LAUNCH_ANCHOR_SCAN_LIMIT` leaves a margin in case a run event is inserted before it in the future.
 */
const RUN_LAUNCH_ANCHOR_SCAN_LIMIT = 8;

export function readRunLaunchAnchor(
  journal: JournalStorePort,
  runId: string,
): RunLaunchAnchor | undefined {
  for (const stored of journal.listEvents(runId, { limit: RUN_LAUNCH_ANCHOR_SCAN_LIMIT })) {
    if (stored.event.type === "run-launched") return { inputId: stored.event.inputId };
  }
  return undefined;
}

/**
* Read back the selection of this run subagent from the journal: `subagentModel` on the same `run-launched`. Resume, two reading surfaces and cold playback all restore the same specification string based on this——
* Zero SQL, this column is not available on `dwf_run`. Absent means that the subagent runs on the session model (most runs, initiated before the upgrade
* run as well).
 *
* The same scan and upper limit as the anchor point, but **deliberately not hung on** {@link RunLaunchAnchor}:
* {@link resolveLaunchAnchor} will cause revision continuations to use the predecessor's anchor point, and models must not be inherited in this way - that is
* The three states of the tool surface (omitted = inherit the predecessor, null = return to the session model, string = set), normalization occurs in
* In resolveInput of `AmendWorkflow`, it is the same argument as `max_concurrency`.
 */
export function readRunSubagentModel(journal: JournalStorePort, runId: string): string | undefined {
  for (const stored of journal.listEvents(runId, { limit: RUN_LAUNCH_ANCHOR_SCAN_LIMIT })) {
    if (stored.event.type === "run-launched") return stored.event.subagentModel;
  }
  return undefined;
}

/**
* Read back the script file of this run from the journal: `scriptPath` on the same `run-launched`. The two cold paths for reading the surface are based on this
* Restore the same absolute path - zero SQL, no such column on `dwf_run`. Absence means that this run does not have an editable script file.
* (Projects that cannot be written in draft, runs initiated before this feature).
 *
* Same scan, same upper limit as {@link readRunSubagentModel}, also **deliberately not hung**
* {@link RunLaunchAnchor}: The anchor point will be used by the revised continuation, and the predecessor script path points to the old script.
* Keep using it to let the model edit a file that is no longer running next time.
 */
export function readRunScriptPath(journal: JournalStorePort, runId: string): string | undefined {
  for (const stored of journal.listEvents(runId, { limit: RUN_LAUNCH_ANCHOR_SCAN_LIMIT })) {
    if (stored.event.type === "run-launched") return stored.event.scriptPath;
  }
  return undefined;
}

/**
* Resolve anchors on submission. Priority:
* 1. The revised continuation (`resume_from`) follows the anchor point of the **predecessor** - all steps of the same job hang the same message;
* 2. `launchInputId` explicitly given by the caller (direct launch of the hub: sharing the same UUID v7 with the controlOnly launch wheel);
* 3. The inputId of the parent runtime activity wheel (chat CreateWorkflow: the tool is executed in that round);
* 4. Create a UUID v7 (CLI, host without movable wheels, revision of front drive without anchor point).
 */
export function resolveLaunchAnchor(input: {
  requested?: string;
  trace: TraceContext;
  resolveLaunchInputId?: (trace: TraceContext) => string | undefined;
  predecessor?: RunLaunchAnchor;
  mint?: () => string;
}): RunLaunchAnchor {
  if (input.predecessor !== undefined) return input.predecessor;
  const inputId =
    input.requested ?? input.resolveLaunchInputId?.(input.trace) ?? (input.mint ?? uuidv7)();
  return { inputId };
}
