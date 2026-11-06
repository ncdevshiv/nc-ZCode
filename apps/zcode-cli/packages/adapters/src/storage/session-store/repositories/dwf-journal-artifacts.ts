/**
* Two host readings for user interface **product**.
 *
* ⚠ The artifact here is the deliverable (file/markdown/preset board) that the script **publishes to users**, not
* `RunSettlement.artifact` (the top-level return value of the script). Both meanings coexist.
 *
* Why lives outside `JournalStorePort`: verbatim same argument as `listRuns` - the engine only presses
* `listNodes` follows its own ordinal chain, never asking "what products are there in this run" or "what entries have been received by a certain board"
* Responsible. Adding them to the domain port is equivalent to requiring each journal implementation (including the memory implementation that comes with the engine) to implement one
* What the engine doesn't do. Consumer detection by capability (`typeof journal.listArtifactRows === "function"`)
* Determining read surface availability, the single source of truth for signatures is `DwfRunIntrospectionQueries`.
 *
* Why it lives outside dwf-journal.ts: only one db handle is needed for both queries (similar to run/actor/node/event
* (write-read no shared state), and each comes with a long argument of "why the data source is this table, and why the sorting is this".
* The same division method as dwf-journal-codecs.ts.
 */

import type { DatabaseSync } from "node:sqlite";
import type { NodeRecord } from "@zcode/dynamic-workflow";
import { decodeNode, type DwfEventRow, type DwfNodeRow } from "./dwf-journal-codecs.js";

/** Paging bag of {@link listArtifactItems} (cursor = journal sequence). */
export interface DwfArtifactItemsQuery {
  /**
* Return only entries in sequence **strictly greater than** this value. The cursor is "the last sequence read" rather than the offset.
* Literally the same semantics as `listEvents` - the kanban hook uses the exact cursor it is already using.
   */
  afterSequence?: number;
  /**
*Maximum number of entries on a single page, **required**. The storage layer doesn't guess default values ​​for the caller: an unbounded access query is the only thing that shouldn't be there.
   *
* But **don't add your own ceiling** here (like `Math.min(500, limit)`). The caller legally passes "clamp limit + 1"
* to determine `hasMore` (the one taken more will not enter the page) - a hard cap will quietly eat the detection row, so `hasMore` is in
* Exactly limit = upper limit of permanent absence. Same argument as `DwfListRunsQuery.limit`'s truncation probe line.
   */
  limit: number;
}

/**
* A `report` entry fed to a certain preset product, **located according to journal sequence**.
 *
* Why the key is sequence instead of (siteId, ordinal): The incremental cursor of the UI is the journal one
* sequence (`afterSequence`, the same shape as the run event query), which is not available on dwf_node. The site coordinates are still returned with the trip——
* Revealing animation requires a React key that is stable across retrievals, and both sequence and coordinates are satisfied.
 */
export interface DwfArtifactItem {
/** The original value of the item being reported (arbitrary JSON; `REPORT_CAPS` is guaranteed to be bounded on the write side). */
  item: unknown;
  ordinal: number;
  sequence: number;
  siteId: string;
}

/**
* The **product line** (`kind = 'artifact'`) of this run is placed in the order of storage (`order by id`).
 *
* One row = one version (re-release with the same id is a new row, history is retained), so the caller is grouped by `artifactId`,
* Get the version from each row's `result` (`ArtifactVersionRecord`). The sorting is **insertion order** instead of
* `order by artifact_id, ordinal`: The order of versions is the order of dropping into the library, and rows with the same id are naturally continuous.
* Just a coincidence - relying on the presentation order, a staggered release script would make the releases look out of order.
 *
* Failed releases are also in the results (`status: "failed"` + `error`): the reader should be able to say "This release failed"
*Failure", filtering it out is equivalent to making a user-visible failure non-existent on every surface.
 */
export function listArtifactRows(db: DatabaseSync, runId: string): NodeRecord[] {
  const rows = db
    .prepare("select * from dwf_node where run_id = ? and kind = 'artifact' order by id")
    .all(runId) as unknown as DwfNodeRow[];
  return rows.map(decodeNode);
}

/**
* Feed the `report` entries of a certain preset product, and page them in ascending order according to the journal sequence (the access side of the Kanban board).
 *
* The number source is **dwf_event instead of dwf_node**, although both tables record the same batch of label reports. The reason is the cursor:
* The UI uses the `sequence` set of event logs to incrementally pull, but there is no sequence on dwf_node, only one
* The composite coordinates of (siteId, ordinal) - forging a total order on it is equivalent to creating a second set of cursor semantics for the same batch of data.
* So the filter is pushed down to `json_extract(payload_json, '$.artifactId') = ?`, `dwf_event_artifact_idx`
* (expression index of 0029) is exactly this shape.
 *
* `type = 'report'` also enters the condition: the `type` column is redundant for pushdown (there is also a copy in the payload). Without it,
* Another event that happens to have `artifactId` (today there is only `artifact-published`, which is nested
* `artifact.id` instead of the top-level `artifactId` (but not necessarily tomorrow) will be mixed into the Kanban data stream.
 *
* Untagged reports are naturally excluded: there is no `artifactId` key in their payload, `json_extract`
* gives NULL, and SQL's `= ?` does not match NULL.
 */
export function listArtifactItems(
  db: DatabaseSync,
  runId: string,
  artifactId: string,
  query: DwfArtifactItemsQuery,
): DwfArtifactItem[] {
// limit ≤ 0 is an empty page (`listEvents`'s `limit -1` full idiom does not apply to this query - Kanban's
// The number surface is always bounded). Floor here, ceiling not: see {@link DwfArtifactItemsQuery}.limit.
  if (query.limit <= 0) return [];
  const after = query.afterSequence;
  const cursor = after === undefined ? "" : " and sequence > ?";
  const rows = db
    .prepare(
      `
      select sequence, payload_json from dwf_event
      where run_id = ?
        and type = 'report'
        and json_extract(payload_json, '$.artifactId') = ?${cursor}
      order by sequence
      limit ?
      `,
    )
    .all(
      runId,
      artifactId,
      ...(after === undefined ? [] : [after]),
      query.limit,
    ) as unknown as Pick<DwfEventRow, "payload_json" | "sequence">[];
  return rows.map((row) => {
// payload is the `RunEvent` appendEvent stringify as is, so the narrow shape here is the same as
// `{ type: "report"; instance: InstanceRef; item: unknown; artifactId?: string }` Same origin.
    const payload = JSON.parse(row.payload_json) as {
      instance: { ordinal: number; siteId: string };
      item: unknown;
    };
    return {
      sequence: row.sequence,
      siteId: payload.instance.siteId,
      ordinal: payload.instance.ordinal,
      item: payload.item,
    };
  });
}
