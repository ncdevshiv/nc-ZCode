/**
* dwf-journal.ts reaches the upper limit of oxlint max-lines (400 lines), and puts the host side **run introspection reading**
* The family (`DwfRunIntrospectionQueries` and its SQL) is split into this file; the public side is still exported from dwf-journal.ts,
* `SqliteDwfJournalStore` only does delegation.
 *
* Same split as dwf-journal-artifacts.ts: these queries only require one db handle, same as run/actor/node/event
*'s writing-reading has no shared state, and each comes with a long argument of "why the data source is this table, and why the sorting is this".
* The engine's `JournalStorePort` writing surface is not here - there are no writers here.
 */

import type { DatabaseSync } from "node:sqlite";
import type { NodeRecord, NodeRecordStatus, RunStatus, StoredEvent } from "@zcode/dynamic-workflow";
import type { DwfArtifactItem, DwfArtifactItemsQuery } from "./dwf-journal-artifacts.js";
import {
  decodeEvent,
  decodeNode,
  decodeRunDetailRow,
  decodeRunListItem,
  decodeRunSessionListItem,
  type DwfEventRow,
  type DwfNodeRow,
  type DwfWorldNodeRow,
  type DwfRunDetailRow,
  type DwfRunListItem,
  type DwfRunMetadataRow,
  type DwfRunRow,
  type DwfRunSessionListItem,
  type DwfRunSessionRow,
  encodeRunStatusPredicate,
} from "./dwf-journal-codecs.js";

/** Query bag for {@link DwfRunIntrospectionQueries.listRuns}. */
export interface DwfListRunsQuery {
  /**
* Project key. Literal equivalent matches `dwf_run.cwd` (the writing side is dropped as is, the reading side is checked as is).
   *
* **Optional**: By default, no cwd predicate is added, and it is enumerated across all projects. The running history of a global workflow spans every time it has been initiated
* Project (`scope: "global"` variant of `workflows/runs`); the project file variant still passes cwd, and the behavior is verbatim.
   */
  cwd?: string;
  /**
* Return the upper limit of rows, **required**. The clamping policy belongs to the caller (the tool face clamps to [1, 50]); the storage layer does not guess one for it
* Default - an unbounded enum query is the only shape that shouldn't be there.
   *
* But **don't add your own ceiling** here (like `Math.min(50, limit)`). The caller can legally pass "clamp_limit + 1":
* Run service takes one more item to determine `truncated` (the extra one will not enter the page). A 50 hard top will silence the detection line
* eaten, so `truncated` is permanently absent when **exactly** limit = 50 - exactly what the user needs to know "there's more"
* that page size, and no tests will find it on other limits.
   */
  limit: number;
/** Optional subset of states. The default is no filtering; an empty array means "does not match any status" (returns to an empty page). */
  statuses?: readonly RunStatus[];
  /**
* Optional run name (literal equivalent of `dwf_run.name`). The GUI center belongs to the running history according to "workflow name = run name", and the filter is pushed down to SQL instead of taking a page and filtering again - otherwise a high-frequency
* The workflow will push other workflows out of the page, and the "last run" on the card will be wrong.
   */
  name?: string;
}

/** Three-state count of dwf_node (all values ​​of `NodeRecordStatus`, three keys are always present). */
export interface DwfNodeStatusCounts {
  completed: number;
  failed: number;
  running: number;
}

/**
* Run introspection query surface on the host side (`ListWorkflowRuns` / `GetWorkflowRun` two read-only tool access bases).
 *
* Intentionally **not widening** the engine's `JournalStorePort`, with {@link SqliteDwfJournalStore.listNonTerminalRuns}
* The same argument verbatim: the engine only reads and writes its own row according to runId, never enumerates runs, and does no aggregate counting - add these to
* Domain port is equivalent to requiring each journal implementation (including the memory implementation that comes with the engine) to be responsible for something that the engine does not do.
 *
* The consumer determines tool availability based on capability detection (`typeof journal.listRuns === "function"`), so this interface is
* The **only** signature source between the host and the adapter: if the signature is written in two places, it will drift, and the consequence of the drift is that the tool becomes silent
* Downgraded to "This session does not have this ability".
 */
export interface DwfRunIntrospectionQueries {
  countNodesByStatus(runId: string): DwfNodeStatusCounts;
  getRunRow(runId: string): DwfRunDetailRow | undefined;
  listArtifactItems(
    runId: string,
    artifactId: string,
    query: DwfArtifactItemsQuery,
  ): DwfArtifactItem[];
  listArtifactRows(runId: string): NodeRecord[];
  listRecentLogEvents(runId: string, limit: number): StoredEvent[];
  listRuns(query: DwfListRunsQuery): DwfRunListItem[];
  /**
* The world-read / world-run lines of this run are in the order of logging (`order by id`), with journal timestamps. **Not taken
* `result_json`**: This reading page is a list (op / args / status / time), and the text is also (siteId, ordinal)
* Reading surface - 256 nodes on a page to solve each 256 KB stdout together, which is equivalent to reading the entire journal into memory.
   */
  listWorldNodes(runId: string): DwfWorldNodeRow[];
}

/**
* Enumerate runs by project (cwd), with the most recently updated first. Same argument as {@link SqliteDwfJournalStore.listNonTerminalRuns}: Engine
* Never enumerate run, this is the reading requirement of the host (`ListWorkflowRuns` tool), so it does not enter the domain port.
 *
* cwd / statuses / sort / limit **Push down all SQL**: `dwf_run_cwd_idx` (0021) is exactly this shape;
* If you take the whole amount in JS and then filter it, the index and limit will be wasted. cwd is a **literal** equivalent match - the write side falls as is
* `context.workingDirectory`, check the read side as it is, any unilateral path normalization will only create a mismatch.
 *
* The row is a narrow projection ({@link DwfRunListItem}, without failure / result): the list surface does not display the product, but the product
* Can be very large.
 */
export function listRuns(db: DatabaseSync, query: DwfListRunsQuery): DwfRunListItem[] {
// The semantics of the empty state set is "not matching any state" rather than "no filtering": treating it as the latter is equivalent to making an explicit
//The passed filter is silently invalidated. In the same way, limit ≤ 0 is an empty page (the `limit -1` of listEvents is the full idiom
// Not applicable for this query - enum faces are always bounded).
  //
// Floor here, ceiling **not**: the caller legally passes "Tool surface upper limit + 1" when truncating the probe line and adding one
// `Math.min(50, …)` will make truncated permanently absent at exactly limit = 50. See {@link DwfListRunsQuery.limit}.
  if (query.statuses !== undefined && query.statuses.length === 0) return [];
  if (query.limit <= 0) return [];

// cwd defaults to no predicate: the history of the global workflow spans all projects it runs (global variants are not filtered by cwd).
// If cwd is given, literal equivalent matching will be performed. Use dwf_run_cwd_idx, and the behavior will remain unchanged.
  const cwdFilter = query.cwd === undefined ? "" : " and cwd = ?";
// Logical state → physical predicate (stopped / errored shared physical failed, relying on the code of failure_json in SQL
// distinguish; see encodeRunStatusPredicate of dwf-journal-codecs.ts).
  const statusPredicate =
    query.statuses === undefined ? undefined : encodeRunStatusPredicate(query.statuses);
  const statusFilter = statusPredicate === undefined ? "" : ` and ${statusPredicate.sql}`;
  const nameFilter = query.name === undefined ? "" : " and name = ?";
  const rows = db
    .prepare(
      `
      select
        id, parent_session_id, cwd, name, script_text, script_hash, tool_call_id,
        args_json, resumed_from, caps_max_concurrency,
        spent_tokens, status, failure_json, time_created, time_updated
      from dwf_run
      where 1 = 1${cwdFilter}${statusFilter}${nameFilter}
      order by time_updated desc
      limit ?
      `,
    )
    .all(
      ...(query.cwd === undefined ? [] : [query.cwd]),
      ...(statusPredicate?.params ?? []),
      ...(query.name === undefined ? [] : [query.name]),
      query.limit,
    ) as unknown as DwfRunMetadataRow[];
  return rows.map(decodeRunListItem);
}

/**
* Complete line of a single run + journal timestamp. The `RunRecord` returned by `getRun` does not contain time (the engine does not care),
* The details must be reported createdAt / updatedAt, and **must read the journal directly** - the memory snapshot is given after the entry evaporates.
* False starting time.
 */
export function getRunRow(db: DatabaseSync, runId: string): DwfRunDetailRow | undefined {
  const row = db.prepare("select * from dwf_run where id = ?").get(runId) as DwfRunRow | undefined;
  return row ? decodeRunDetailRow(row) : undefined;
}

/**
* The nodes of this run are aggregated and counted by status. Counting is done in SQL: the details page only needs three numbers, and the entire dwf_node row
* Reading it out and counting is an expensive version of the same answer (and the number of nodes is not guaranteed beyond the upper bound).
 *
* The three keys are always present (the missing node is 0): the downstream needs to add them directly to calculate nodesObserved. The missing key will be "no node yet"
* becomes NaN. The vocabulary is the three values ​​of `NodeRecordStatus` - `queued` only exists in the event phase and does not fall into the queue.
 */
export function countNodesByStatus(db: DatabaseSync, runId: string): DwfNodeStatusCounts {
  const rows = db
    .prepare("select status, count(*) as total from dwf_node where run_id = ? group by status")
    .all(runId) as unknown as { status: NodeRecordStatus; total: number }[];
  const counts: DwfNodeStatusCounts = { running: 0, completed: 0, failed: 0 };
  for (const row of rows) counts[row.status] = Number(row.total);
  return counts;
}

/**
* The last N `log` events of this run are returned in time sequence (sequence ascending order).
 *
* The method is `order by sequence desc limit ?` and then reverse it in JS: a long run dwf_event is its largest
* For a table, reading the entire journal into memory for the sake of a few tail entries is exactly the reason why paging exists and should be eliminated. Type filtering is the same
* Pushdown - The `type` column is redundant for this purpose (there is also a copy in payload_json).
 */
export function listRecentLogEvents(db: DatabaseSync, runId: string, limit: number): StoredEvent[] {
  if (limit <= 0) return [];
  const rows = db
    .prepare(
      "select * from dwf_event where run_id = ? and type = 'log' order by sequence desc limit ?",
    )
    .all(runId, limit) as unknown as DwfEventRow[];
  return rows.reverse().map(decodeEvent);
}

/**
* Run under the name of a certain parent session (most recently updated first, up to limit items). The enumeration side of the service host
* (`DynamicWorkflowRunPort.listRunsForSession` → Discovery query after UI restart): `workflowRuns` projection does not survive across processes, tool card join
* Availability with the Resume button can only be restored from this table.
 *
* Same family as {@link SqliteDwfJournalStore.listNonTerminalRuns}: hosts narrow queries and deliberately does not enter the engine's JournalStorePort.
* No index: The number of rows in dwf_run is in the order of "number of workflows per session" (single digits to double digits), and full scanning is acceptable;
* If the magnitude changes in the future, the index shape should be (parent_session_id, time_updated).
 *
* Row is narrow projection ({@link DwfRunSessionListItem}): **Not select `result_json`** - that column is scripted
* The top-level return value is truly unbounded, and the list surface never displays the product. `failure_json` must be taken instead: the session enumeration page must be reported
* failureCode, and the predicate of `resumable` is "failed and code is Interrupted", omitting it will make every
* Interrupted runs are silently counted as unrecoverable. The timestamp is returned along with the string (`RunRecord` is intentionally not accompanied by a time).
 */
export function listRunsByParentSession(
  db: DatabaseSync,
  parentSessionId: string,
  limit: number,
): DwfRunSessionListItem[] {
  const rows = db
    .prepare(
      `
      select
        id, parent_session_id, cwd, name, script_text, script_hash, tool_call_id,
        args_json, resumed_from, caps_max_concurrency,
        spent_tokens, status, failure_json, time_created, time_updated
      from dwf_run
      where parent_session_id = ?
      order by time_updated desc, id desc
      limit ?
      `,
    )
    .all(parentSessionId, Math.max(0, limit)) as unknown as DwfRunSessionRow[];
  return rows.map(decodeRunSessionListItem);
}

export function listWorldNodes(db: DatabaseSync, runId: string): DwfWorldNodeRow[] {
  const rows = db
    .prepare(
      `
      select
        run_id, site_id, ordinal, kind, actor_site_id, actor_ordinal, actor_seq, input_hash,
        status, error_json, stats_json, message_boundary, artifact_id, input_json,
        length(cast(result_json as blob)) as result_bytes,
        case when json_type(result_json) = 'array' then json_array_length(result_json) end
          as result_count,
        case when json_type(result_json) = 'object' then json_extract(result_json, '$.exitCode') end
          as exit_code,
        case when json_type(result_json) = 'object'
          then length(cast(json_extract(result_json, '$.stdout') as blob)) end as stdout_bytes,
        case when json_type(result_json) = 'object'
          then length(cast(json_extract(result_json, '$.stderr') as blob)) end as stderr_bytes,
        time_created, time_updated
      from dwf_node
      where run_id = ? and kind in ('world-read', 'world-run')
      order by id
      `,
    )
    .all(runId) as unknown as (Omit<DwfNodeRow, "id" | "result_json"> & {
    result_bytes: number | null;
    result_count: number | null;
    exit_code: number | null;
    stdout_bytes: number | null;
    stderr_bytes: number | null;
  })[];
  return rows.map((row) => {
    const record = decodeNode({ ...row, id: 0, result_json: null });
    return {
      ...record,
      timeCreated: row.time_created,
      timeUpdated: row.time_updated,
      ...(row.result_bytes === null ? {} : { resultBytes: row.result_bytes }),
      ...(row.result_count === null ? {} : { resultCount: row.result_count }),
      ...(typeof row.exit_code === "number" ? { exitCode: row.exit_code } : {}),
      ...(row.stdout_bytes === null ? {} : { stdoutBytes: row.stdout_bytes }),
      ...(row.stderr_bytes === null ? {} : { stderrBytes: row.stderr_bytes }),
    };
  });
}
