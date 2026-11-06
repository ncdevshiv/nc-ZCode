// ============================================================
//Cold replay of Dynamic Workflow Run: journal → the same progress load as live
// ============================================================
// `workflowRuns` projection has only one source of memory.
// It is empty after restarting; and dwf_event contains every event sent by the engine - the same bounded serialization that is fed to the projection during live.
// This module recasts them in journal order into DynamicWorkflowRunProgressPayload. Cold materialization treats this batch of payloads as
// Memory events are fed to the same reducer, so the projections before and after restart are consistent byte by byte.
//
// Separate module instead of stuffed into observation.ts: the casting chain (toProgressPayload) is in launch.ts, and launch.ts
// Already imported observation.ts (predicate), the reverse import is a ring.

import type { DwfRunSessionListItem } from "@zcode/adapters/storage";
import type { DynamicWorkflowRunProgressPayload } from "@zcode/contracts";
import type { JournalStorePort, RunEvent, StoredEvent } from "@zcode/dynamic-workflow";
import { toProgressPayload } from "./dynamic-workflow-run-launch.js";
import { TERMINAL_RUN_STATUSES } from "./dynamic-workflow-run-observation.js";

/**
* All progress loads of a run, in ascending order of journal sequence.
 *
* **Settlement is based on the line**: When the line is in the final state (completed/errored/stopped), the last line played back `run-settled`
* Always cast by line - `run-settled` (if any) that exists at the end is replaced by it, and there is no `run-settled` at the end (process death, orphan convergence only overwrites
* OK, deliberately not to synthesize events - the journal's contract is "what the engine has sent") is added. Same casting chain, same resumable
* Predicate (internal toProgressPayload); the `run-settled` branch of the reducer has been fully normalized, and there is no need for the second one here
* Normalizer. The cast payload only exists in the return value of this playback and is never written to dwf_event.
 *
* Final state vocabulary reconstruction (completed/errored/stopped) only in
The mapping is done in the codec of the * line, and the payload of dwf_event is the original JSON - the `run-settled` written before contains the old words.
* `cancelled` / `failed`. When the original implementation sees that there is already `run-settled` at the end, it will no longer synthesize it and feed the old words to the reducer as they are.
* The reducer doesn't recognize the word, so run is left running: the card is lit, Cancel can be clicked but there is nothing to cancel in the backend. row is status
* is the only authority (codec has translated the old lines into new words), so the settlement is always derived from the lines; no matter how the vocabulary of events changes, cold playback will not
* Will lie about old events again. The two paths to the new vocabulary's run give byte-for-byte identical payloads (the engine's `run-settled` happens to only take
* The four fields on the line), the contract between live and cold playback remains unchanged.
 *
* `concurrencyCeiling` is **required** rather than optional: it is a host-derived field on the `run-started` payload, live
* Side Heng is present (counted at the moment of launch). If it is omitted here, the first load of cold playback will be one less key than live - and
* "Both sides are equal byte by byte" is the only contract of this module. The caller gives the same `resolveWorkflowConcurrencyCeiling`.
 */
export function replayRunProgress(
  row: DwfRunSessionListItem,
  journal: Pick<JournalStorePort, "listEvents">,
  concurrencyCeiling: number,
): DynamicWorkflowRunProgressPayload[] {
  return replayRunProgressFromEvents(row, journal.listEvents(row.runId, {}), concurrencyCeiling);
}

/**
* The same casting chain, but the event is passed in by the caller **has been read**.
 *
* There is only one reason for existence: `getRunDetail` is to reduce the run state and calculate the situation according to the event time in the same call.
* Section. Let each side `listEvents` once,
* It means paying twice for the same data - and this reading is the most expensive section on a long run.
 *
* What is passed in must be **all events of the run, in ascending sequence** (that is, the return of `listEvents(runId, {})`),
* The replacement logic of tail settlement relies on "the last one is the last one".
 */
export function replayRunProgressFromEvents(
  row: DwfRunSessionListItem,
  stored: readonly StoredEvent[],
  concurrencyCeiling: number,
): DynamicWorkflowRunProgressPayload[] {
  const toolCallId = row.toolCallId === undefined ? {} : { toolCallId: row.toolCallId };
// Initiate anchor point: the same casting chain as live, and the derived fields must also be consistent——
// It is a contract of this module (test pinning) that the cold replay payload is byte-for-byte equal to the live payload. The anchor point is in this batch of events (the first
// run-launched), there is no need to check the journal again; the run before the upgrade does not have it, and the field is absent.
  const launched = stored.find((entry) => entry.event.type === "run-launched")?.event;
  const launchInputId =
    launched?.type === "run-launched" ? { launchInputId: launched.inputId } : {};
// The lineage pointer has the same origin as live (the launch side reads from the input parameter or journal line, here it is the line directly).
  const resumedFrom = row.resumedFrom === undefined ? {} : { resumedFrom: row.resumedFrom };
// Subagent model has the same origin as the anchor: the same `run-launched` event (zero SQL, no column on dwf_run).
// The live side reads back the same specification string from the launch input parameters or the same event, so the loads on both sides are still equal byte by byte. Be present only after setting it up.
  const subagentModel =
    launched?.type === "run-launched" && launched.subagentModel !== undefined
      ? { subagentModel: launched.subagentModel }
      : {};
  const terminal = TERMINAL_RUN_STATUSES.has(row.status);
  const last = stored.at(-1);
// Only replace the **tail** settlement: the `run-settled` of the previous life is still lying in the middle of the run after resume. That is real history.
// The following `run-started` will turn it back to running (the existing semantics of reducer), leaving it unchanged.
  const trailingSettle = terminal && last?.event.type === "run-settled" ? last : undefined;
  const replayable = trailingSettle === undefined ? stored : stored.slice(0, -1);
  const payloads = replayable.map((entry) =>
    toProgressPayload({
      event: entry.event,
      runId: row.runId,
      sequence: entry.sequence,
      ...toolCallId,
      ...launchInputId,
      ...resumedFrom,
      ...subagentModel,
      concurrencyCeiling,
    }),
  );
  if (!terminal) return payloads;
  const settled: RunEvent = {
    type: "run-settled",
    status: row.status,
// The stopped row must have reason (when the old row is absent from decoding, the warehouse has been converted to user); the other states do not.
    ...(row.status === "stopped" ? { stopReason: row.stopReason ?? "user" } : {}),
    ...(row.status === "stopped" && row.supersededBy !== undefined
      ? { supersededBy: row.supersededBy }
      : {}),
    ...(row.failure === undefined ? {} : { error: row.failure }),
  };
  payloads.push(
    toProgressPayload({
      event: settled,
      runId: row.runId,
// When replacing, the sequence of the replaced item will be used (the water level is consistent with live); when appending, it will follow the last one.
      sequence: trailingSettle?.sequence ?? (last?.sequence ?? 0) + 1,
      ...toolCallId,
      ...launchInputId,
      ...resumedFrom,
      ...subagentModel,
      concurrencyCeiling,
    }),
  );
  return payloads;
}
