/**
* dwf_* Journal table row ↔ record mapping.
 *
* Record types are only imported from @zcode/dynamic-workflow as `import type`: ports live in domain packages,
* SQLite store is just an adapter of it, and it must not have any dependencies on domain packages during runtime.
 *
* Decoding rules: When the nullable column is NULL, the key is not written out (instead of undefined or null).
* The contract test does toEqual on the entire record, and optional fields that are missing in the input must be returned as they are.
 */

import type {
  ActorRecord,
  AskStats,
  Caps,
  NodeKind,
  NodeRecord,
  NodeRecordStatus,
  PersonaSpec,
  RunEvent,
  RunRecord,
  RunSettlementRecord,
  RunStatus,
  StoredEvent,
  WorkflowErrorJson,
  WorldReadInput,
} from "@zcode/dynamic-workflow";
import { decodeJson, encodeJson } from "../json.js";

/**
* **Physical** vocabulary for `dwf_run.status` (CHECK set for migration 0019, not migrated). Logical vocabulary is the engine's
* `RunStatus` (`errored` / `stopped`), the mapping between the two only lives in this file:
 *
* | logical | physical status | failure_json |
 * | ----------------------- | ----------- | --------------------------------------------------- |
* | stopped{reason, error?} | canceled | `{"stopReason": …, "error"?: WorkflowErrorJson}` envelope |
* | errored{error} | failed | WorkflowErrorJson as is |
* | completed/pending/running | same name | unchanged |
 *
* Decoding (no need to backfill historical rows): `cancelled` → stopped, reason = stopReason of envelope, absent (old row) ⇒ `user`;
* `failed` + code `Interrupted` → stopped(interrupted) (old orphan convergence line); the rest `failed` → errored.
* Accepted imprecision: canceled with no reason in history will always be interpreted as user (including those stopped by TaskStop).
 */
export type DwfRunPhysicalStatus = "pending" | "running" | "completed" | "failed" | "cancelled";

type RunStopReason = NonNullable<RunRecord["stopReason"]>;

/**
* Envelope in stopped line failure_json (distinguished from WorkflowErrorJson by `stopReason` key).
* `supersededBy` is the same write as `stopReason: "superseded"` - the entire envelope is rewritten, so it has zero migration
* Live here.
 */
interface DwfStoppedEnvelope {
  stopReason: RunStopReason;
  supersededBy?: string;
  error?: WorkflowErrorJson;
}

/** Triplet of logical final states: state + stop reason (+ successor) + structured failure. */
interface DwfRunSettlementFields {
  status: RunStatus;
  stopReason?: RunStopReason;
  supersededBy?: string;
  failure?: WorkflowErrorJson;
}

const INTERRUPTED_CODE = "Interrupted";
// Whitelist for envelope sniffing: One value is missing. For this reason, the entire envelope cannot be decoded and the line degenerates into stopped(user).
const STOP_REASONS: ReadonlySet<string> = new Set([
  "user",
  "model",
  "provider",
  "interrupted",
  "superseded",
]);

/** Logical state (+ settlement bag) → physical column value. The only entry on the writing side (shared by createRun/updateRunStatus). */
export function encodeRunSettlement(
  status: RunStatus,
  settlement?: Pick<RunSettlementRecord, "stopReason" | "supersededBy" | "failure">,
): { status: DwfRunPhysicalStatus; failureJson: string | null } {
  switch (status) {
    case "stopped": {
      const envelope: DwfStoppedEnvelope = { stopReason: settlement?.stopReason ?? "user" };
      if (settlement?.supersededBy !== undefined) envelope.supersededBy = settlement.supersededBy;
      if (settlement?.failure !== undefined) envelope.error = settlement.failure;
      return { status: "cancelled", failureJson: JSON.stringify(envelope) };
    }
    case "errored":
      return { status: "failed", failureJson: encodeJson(settlement?.failure) };
    case "completed":
      return { status: "completed", failureJson: encodeJson(settlement?.failure) };
    case "pending":
    case "running":
      return { status, failureJson: null };
  }
}

/** Physical column value → logical final state triplet. The only entry on the read side (shared with full record and enumeration). */
function decodeRunSettlement(
  status: DwfRunPhysicalStatus,
  failureJson: string | null,
): DwfRunSettlementFields {
  const raw = decodeJson<Record<string, unknown>>(failureJson);
  switch (status) {
    case "cancelled": {
      const fields: DwfRunSettlementFields = { status: "stopped", stopReason: "user" };
      if (raw !== undefined && isStoppedEnvelope(raw)) {
        fields.stopReason = raw.stopReason;
        if (typeof raw.supersededBy === "string" && raw.supersededBy.length > 0) {
          fields.supersededBy = raw.supersededBy;
        }
        if (raw.error !== undefined) fields.failure = raw.error;
      }
      return fields;
    }
    case "failed": {
      const failure = raw as WorkflowErrorJson | undefined;
      if (failure?.code === INTERRUPTED_CODE) {
        return { status: "stopped", stopReason: "interrupted", failure };
      }
      return failure === undefined ? { status: "errored" } : { status: "errored", failure };
    }
    case "completed": {
      const failure = raw as WorkflowErrorJson | undefined;
      return failure === undefined ? { status: "completed" } : { status: "completed", failure };
    }
    case "pending":
    case "running":
      return { status };
  }
}

function isStoppedEnvelope(
  value: Record<string, unknown>,
): value is DwfStoppedEnvelope & Record<string, unknown> {
  return typeof value.stopReason === "string" && STOP_REASONS.has(value.stopReason);
}

/**
* Logical status filtering → SQL predicates (states pushdown of `listRuns`). `stopped` / `errored` are shared at the physical layer
* The `failed` column value is determined by the code of `failure_json` - use SQLite's json_extract to distinguish it in SQL.
* Instead of taking a page and filtering (the latter will distort limit and truncation detection).
 */
export function encodeRunStatusPredicate(statuses: readonly RunStatus[]): {
  sql: string;
  params: string[];
} {
  const clauses: string[] = [];
  const params: string[] = [];
  for (const status of statuses) {
    switch (status) {
      case "stopped":
        clauses.push(
          "(status = 'cancelled' or (status = 'failed' and json_extract(failure_json, '$.code') = ?))",
        );
        params.push(INTERRUPTED_CODE);
        break;
      case "errored":
        clauses.push(
          "(status = 'failed' and coalesce(json_extract(failure_json, '$.code'), '') <> ?)",
        );
        params.push(INTERRUPTED_CODE);
        break;
      default:
        clauses.push("status = ?");
        params.push(status);
    }
  }
  return { sql: clauses.length === 0 ? "0" : `(${clauses.join(" or ")})`, params };
}

export interface DwfRunRow {
  args_json: string | null;
  caps_max_concurrency: number;
  cwd: string | null;
  failure_json: string | null;
  id: string;
  name: string | null;
  parent_session_id: string | null;
  result_json: string | null;
/** Lineage pointer to amend-resume (added by newer migrations). Narrow projections also select it: see {@link DwfRunMetadataRow}. */
  resumed_from: string | null;
  script_hash: string | null;
  script_text: string | null;
/** Run-level token usage (formerly called budget_spent; the same number, just no longer called budget). */
  spent_tokens: number;
/** Physical vocabulary (see the mapping table in the file header); the logical state is decoded by {@link decodeRunSettlement} together with failure_json. */
  status: DwfRunPhysicalStatus;
  time_created: number;
  time_updated: number;
  tool_call_id: string | null;
}

/**
* **Metadata columns** for dwf_run (without the potentially large result_json). The enum query selects only these columns:
* Decoding result_json together in 50 lines on one page is equivalent to reading the entire library's products into memory, while the list surface does not display it at all.
 *
* Physics `failed` may be either errored or
* It may be stopped(interrupted), the logical status can only be solved together with failure_json; it is a small structured
* Object (code + message + envelope), which can be solved line by line without memory risk. The enumeration is still done without the `failure` field.
 *
* Actual parameters are declared and verified
* Small JSON bag, the running history row in the GUI hub should show it - and failure / result are the unbounded product columns.
 *
* `resumed_from` belongs to this layer (not the omitted large column): it is a short string, and "absent = not a revision"
* The decoding rule requires that the column is actually selected - leave it out and `row.resumed_from` in narrow queries will be undefined.
* takes a different branch than NULL, so the enumeration line brings a key with a value of undefined.
 */
export type DwfRunMetadataRow = Omit<DwfRunRow, "result_json">;

/** Journal side row timestamp. `RunRecord` deliberately does not include time (the engine does not care), but the reading page must report created/updated. */
export interface DwfRunTimestamps {
  timeCreated: number;
  timeUpdated: number;
}

/**
* One line of enumeration query: run metadata + timestamp, **excluding** failure / result.
* The detail line is a superset of it, so the two reading surfaces of list and get can share the same set of labels and attribution derivation.
 */
export type DwfRunListItem = Omit<RunRecord, "failure" | "result"> & DwfRunTimestamps;

/** One line of detailed query: complete `RunRecord` (including failure / result) + timestamp. */
export type DwfRunDetailRow = RunRecord & DwfRunTimestamps;

/**
* A row of session enumeration query: {@link DwfRunListItem} + `failure`, **still does not contain result**.
 *
* Why not use {@link DwfRunListItem} directly: session enumeration surface (`listRunsForSession` → `/dwf list`)
* To report failureCode/failureMessage, and the predicate of `resumable` is "failed and code is
* Interrupted" - omitting failure_json will cause each interrupted run to be counted as unrecoverable.
* That's a silent wrong answer, not one less column to display.
 *
* Why result_json is still not taken: that column is truly unbounded (the top-level return value of the script), and the list surface is never displayed
* Product. failure_json is a structured small object (code + message), which can be parsed line by line without memory risk.
 */
export type DwfRunSessionRow = DwfRunMetadataRow;

/** The record form of {@link DwfRunSessionRow}. */
export type DwfRunSessionListItem = DwfRunListItem & Pick<RunRecord, "failure">;

export interface DwfActorRow {
  id: number;
  name: string | null;
  ordinal: number;
  persona_json: string | null;
  resolved_model: string | null;
  run_id: string;
  session_id: string | null;
  site_id: string;
  time_created: number;
  time_updated: number;
}

export interface DwfNodeRow {
  actor_ordinal: number | null;
  actor_seq: number | null;
  actor_site_id: string | null;
  /**
* The id of the user interface product (added in newer migrations). A line with `kind = 'artifact'` takes the artifact it publishes/declares;
* The tagged `kind = 'report'` line carries the preset it feeds. The remaining rows are NULL.
* ⚠ Has nothing to do with `RunSettlement.artifact` (the script's top-level return value).
   */
  artifact_id: string | null;
  error_json: string | null;
  id: number;
  input_hash: string;
/** Bounded `{op, args}` for world-read / world-run lines (added by newer migrations); NULL for remaining lines and older lines. */
  input_json: string | null;
  kind: NodeKind;
/** ask The length of the actor session message log after settlement (count offset, added by newer migrations). */
  message_boundary: number | null;
  ordinal: number;
  result_json: string | null;
  run_id: string;
  site_id: string;
  stats_json: string | null;
  status: NodeRecordStatus;
  time_created: number;
  time_updated: number;
}

/**
* A line in the workspace reading surface: world-read / world-run
* `NodeRecord` (**excluding `result`** - the text also has a reading surface according to (siteId, ordinal)) + journal timestamp +
* The number of bytes of `result_json` (the "big" in the list can be reported without decoding the text).
 */
export interface DwfWorldNodeRow extends Omit<NodeRecord, "result">, DwfRunTimestamps {
/** Number of UTF-8 bytes of `result_json`; absent if the row has not yet settled or failed to settle. */
  resultBytes?: number;
  /**
* The number of elements when the body is a JSON array (number of files in `glob`, number of hits in `grep`, number of paths in `git.changedFiles`).
* It is calculated by SQLite's JSON function in the query, and the text itself is not exported from the database.
   */
  resultCount?: number;
/** `exitCode` on the body after settlement of `world.run`; other ops and unsettled lines are absent. */
  exitCode?: number;
/** Number of UTF-8 bytes of `stdout` / `stderr` after settlement of `world.run`. */
  stdoutBytes?: number;
  stderrBytes?: number;
}

export interface DwfEventRow {
  id: number;
  payload_json: string;
  run_id: string;
  sequence: number;
  time_created: number;
  type: string;
}

/**
* `result` column-specific encoding. `null` is a legal ask result (`ask<T | null>` will return it),
* The shared encodeJson compresses undefined and null together into SQL NULL - so `result: null`
* After being dropped into the library and read out again, it will become "No result". Here only undefined is mapped to SQL NULL.
 */
export function encodeResultJson(value: unknown): string | null {
  if (value === undefined) return null;
  const text = JSON.stringify(value);
  return text === undefined ? null : text;
}

/**
* Public decoding of run metadata. Both the complete record and the enumeration process start from here, and the two readings are therefore not possible in "Which optional columns
* Counted as absent" fork upward.
 */
function decodeRunMetadata(row: DwfRunMetadataRow): Omit<RunRecord, "failure" | "result"> {
  const caps: Caps = { maxConcurrency: row.caps_max_concurrency };

  const settlement = decodeRunSettlement(row.status, row.failure_json);
  const record: Omit<RunRecord, "failure" | "result"> = {
    runId: row.id,
    caps,
    spentTokens: row.spent_tokens,
    status: settlement.status,
  };
  if (settlement.stopReason !== undefined) record.stopReason = settlement.stopReason;
  if (settlement.supersededBy !== undefined) record.supersededBy = settlement.supersededBy;
  if (row.parent_session_id !== null) record.parentSessionId = row.parent_session_id;
  if (row.cwd !== null) record.cwd = row.cwd;
//The name of the row before the column is introduced is NULL (new column, no backfill): it is resolved into **absent key**, and the reader uses the first line of the script accordingly to find out.
  if (row.name !== null) record.name = row.name;
// The rows dropped into the database early do not have the value of this column (NULL means absent), so they are resolved into absent keys, and the historical run still reads them back.
  if (row.tool_call_id !== null) record.toolCallId = row.tool_call_id;
  if (row.script_text !== null) record.scriptText = row.script_text;
  if (row.script_hash !== null) record.scriptHash = row.script_hash;
// This column is NULL for early dropped rows and every non-revised run (mostly): interpreted as **absent key**.
// The reading side determines "whether this run needs to rebuild the import cache" based on this.
  if (row.resumed_from !== null) record.resumedFrom = row.resumed_from;
// The rows dropped early do not have the value of this column (NULL means absent): interpret it as **absent key** instead of `{}`, so that "no actual parameters"
// Remain distinguishable from "the actual parameter is an empty bag" at the record level; the sandbox side uniformly reads absence as `{}` (invariant 7).
  if (row.args_json !== null) record.args = JSON.parse(row.args_json) as Record<string, unknown>;
  return record;
}

export function decodeRun(row: DwfRunRow): RunRecord {
  const record: RunRecord = decodeRunMetadata(row);
  const { failure } = decodeRunSettlement(row.status, row.failure_json);
  if (failure !== undefined) record.failure = failure;
// The rows dropped early have no value (the column is newly added and the value is NULL): it must be resolved into **absent key**, not undefined
// Value or throw an error, otherwise all historical runs will not be read back after the upgrade. `result: null` follows the same JSON.parse
// Path, so legal null products still appear in the record (same convention as node's result_json).
  if (row.result_json !== null) record.result = JSON.parse(row.result_json);
  return record;
}

/** Enumerated rows: metadata + timestamp. */
export function decodeRunListItem(row: DwfRunMetadataRow): DwfRunListItem {
  return {
    ...decodeRunMetadata(row),
    timeCreated: row.time_created,
    timeUpdated: row.time_updated,
  };
}

/** Detail row: complete record + timestamp. */
export function decodeRunDetailRow(row: DwfRunRow): DwfRunDetailRow {
  return { ...decodeRun(row), timeCreated: row.time_created, timeUpdated: row.time_updated };
}

/** Session enumeration: metadata + timestamp + failure (I don’t understand result_json - that column was not selected). */
export function decodeRunSessionListItem(row: DwfRunSessionRow): DwfRunSessionListItem {
  const item: DwfRunSessionListItem = decodeRunListItem(row);
  const { failure } = decodeRunSettlement(row.status, row.failure_json);
  if (failure !== undefined) item.failure = failure;
  return item;
}

export function decodeActor(row: DwfActorRow): ActorRecord {
  const record: ActorRecord = {
    runId: row.run_id,
    siteId: row.site_id,
    ordinal: row.ordinal,
  };
  if (row.name !== null) record.name = row.name;
  const persona = decodeJson<PersonaSpec>(row.persona_json);
  if (persona !== undefined) record.persona = persona;
  if (row.session_id !== null) record.sessionId = row.session_id;
//The column in the row before the column is introduced is NULL: it is resolved into **absent key**, and the historical actor records can still be retrieved.
  if (row.resolved_model !== null) record.resolvedModel = row.resolved_model;
  return record;
}

export function decodeNode(row: DwfNodeRow): NodeRecord {
  const record: NodeRecord = {
    runId: row.run_id,
    siteId: row.site_id,
    ordinal: row.ordinal,
    kind: row.kind,
    inputHash: row.input_hash,
    status: row.status,
  };
  if (row.actor_site_id !== null) record.actorSiteId = row.actor_site_id;
  if (row.actor_ordinal !== null) record.actorOrdinal = row.actor_ordinal;
  if (row.actor_seq !== null) record.actorSeq = row.actor_seq;
  if (row.result_json !== null) record.result = JSON.parse(row.result_json);
  const error = decodeJson<WorkflowErrorJson>(row.error_json);
  if (error !== undefined) record.error = error;
  const stats = decodeJson<AskStats>(row.stats_json);
  if (stats !== undefined) record.stats = stats;
// Rows dropped early, non-ask nodes, and ask whose boundaries have not been filled by the driver are all NULL → absent keys.
// `0` is a **legal boundary** (zero messages are copied), so the test must be `!== null` rather than a truth test.
  if (row.message_boundary !== null) record.messageBoundary = row.message_boundary;
// Rows dropped early and every report / ask / world-* row that is neither a product row nor a label are NULL
// → resolves to **absent keys** (not undefined values). Kanban access is filtered by "artifact_id matching".
// A key with a value of undefined will cause the entire toEqual of the contract test to fail.
  if (row.artifact_id !== null) record.artifactId = row.artifact_id;
// Lines before 0030, and ask / report / artifact lines are NULL → absent keys (readers return static tags accordingly).
  const input = decodeJson<WorldReadInput>(row.input_json);
  if (input !== undefined) record.input = input;
  return record;
}

export function decodeEvent(row: DwfEventRow): StoredEvent {
  return {
    sequence: row.sequence,
    event: JSON.parse(row.payload_json) as RunEvent,
// The only clock in the event log: age of log line, subagent
//The time of the last action is calculated from it. The column has a constant value since 0019, so it is written unconditionally here.
    timeCreated: row.time_created,
  };
}
