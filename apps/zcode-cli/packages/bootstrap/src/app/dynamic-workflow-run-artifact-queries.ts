// ============================================================
// The journal reading surface of the user surface product (the access base shared by the three product methods of DynamicWorkflowRunPort)
// ============================================================
//
// ⚠ Terminology: The artifact here is the output released by the script to **users** through `artifact.*`, not internal to the engine
// `RunSettlement.artifact` (script top-level return value).

import type { DwfArtifactItem, DwfRunIntrospectionQueries } from "@zcode/adapters/storage";
import type { DynamicWorkflowRunArtifactItem } from "@zcode/contracts";
import type { JournalStorePort, NodeRecord } from "@zcode/dynamic-workflow";

/**
* Journal with product reading surface. The **only source** of signatures is adapters' {@link DwfRunIntrospectionQueries}
* (`import type`, zero runtime dependencies) - the same argument as `DynamicWorkflowIntrospectableJournal`:
* These two queries are not on the engine's {@link JournalStorePort} (the engine never enumerates products nor connects to the event table),
* Can only be connected by ability detection.
 */
interface ArtifactReadableJournal
  extends JournalStorePort, Pick<DwfRunIntrospectionQueries, "listArtifactItems" | "listArtifactRows"> {}

/**
* Whether the journal has a product reading page. **Deliberately a sibling of `supportsRunIntrospection`, rather than expanding it into six. **
 *
* Those four (listRuns/getRunRow/countNodesByStatus/listRecentLogEvents) are an overall capability:
* There needs to be one list and three more details. If one is missing, the whole thing should be downgraded. Product reading is the second ability that developed **later**.
* The two are independent of each other - a journal with only the first four entries (the dist of the old adapter, a test double that only implements introspection) should
* Continue to run `ListWorkflowRuns` / `GetWorkflowRun`, but do not provide products. merge them into one probe,
* will cause two tools that have already worked on this type of journal to disappear silently, and the symptoms are far from the cause.
 */
export function supportsArtifactReads(journal: JournalStorePort): journal is ArtifactReadableJournal {
  const candidate = journal as Partial<DwfRunIntrospectionQueries>;
  return (
    typeof candidate.listArtifactItems === "function" &&
    typeof candidate.listArtifactRows === "function"
  );
}

/**
* Feed the `report` entries of a certain preset product, in ascending order of journal sequence.
 *
* The storage layer **accurately** honors the limit and never clamps it by itself - so "take one more to determine hasMore"
* Completed by the caller (gateway) passing limit+1, here it is transparently transmitted as it is. Out of bounds cursor gets empty page instead of error: flip to tail
* This is a normal page-turning ending, not an anomaly.
 */
export function listArtifactItemsFrom(
  journal: JournalStorePort,
  runId: string,
  artifactId: string,
  page: { afterSequence?: number; limit: number },
): DynamicWorkflowRunArtifactItem[] {
  if (!supportsArtifactReads(journal)) return [];
  const rows = journal.listArtifactItems(runId, artifactId, {
    ...(page.afterSequence === undefined ? {} : { afterSequence: page.afterSequence }),
    limit: page.limit,
  });
  return rows.map(toArtifactItem);
}

/**
* A row for the storage layer → a row for the port. Fields correspond one to one, deliberately **not doing preview serialization**: the pure functions of Kanban must be based on fields
* The path (`ChartSpec.x.field` is in the shape of "timing.after") to get the number. If you get a piece of pretty JSON text, you can't get it out.
* . Items are already bounded by `REPORT_CAPS.maxItemSerializedBytes` (32KB) on the line and do not need to be stacked one more layer.
 */
function toArtifactItem(row: DwfArtifactItem): DynamicWorkflowRunArtifactItem {
  return {
    sequence: row.sequence,
    siteId: row.siteId,
    ordinal: row.ordinal,
    item: row.item,
  };
}

/**
* One line of product id claimed by the artifact node.
 *
* Read the `dwf_node.artifact_id` column first (the indexed column added by the newer migration); the `id` in the record is just a hint.
* Both are written by the engine in the same putNode and are always equal, but the old row whose column is NULL should not make the entire row unreadable.
 */
export function artifactRowId(row: NodeRecord): string | undefined {
  if (typeof row.artifactId === "string" && row.artifactId.length > 0) return row.artifactId;
  const record = row.result;
  if (record === null || typeof record !== "object" || Array.isArray(record)) return undefined;
  const id = (record as Record<string, unknown>).id;
  return typeof id === "string" && id.length > 0 ? id : undefined;
}
