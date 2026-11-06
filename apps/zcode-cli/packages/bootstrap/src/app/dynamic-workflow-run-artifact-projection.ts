// ============================================================
// Dynamic Workflow Run: merged projection of user plane artifacts (journal line → artifacts of port)
// ============================================================
// dynamic-workflow-run-observation.ts reaches the upper limit of oxlint max-lines (400 lines), and the entire paragraph
// Merge artifacts ({@link artifactsOf} and its private decoder) into this file. Export the observation surface as it is, four
// The call point (snapshot/getRunDetail/listArtifacts/saved-workflows hub) so not a single line needs to be changed.
//
// ⚠ Terminology: The artifact here is the output of the script published to the user via `artifact.*` **(journal
// `kind = "artifact"` line), not the engine's internal `RunSettlement.artifact` (the script's top-level return value, on the port
// Called `output` / `result`). Two meanings of the same word.

import type { DwfRunIntrospectionQueries } from "@zcode/adapters/storage";
import type {
  DynamicWorkflowRunArtifact,
  DynamicWorkflowRunArtifactKind,
  DynamicWorkflowRunArtifactVersion,
} from "@zcode/contracts";
import type { JournalStorePort, NodeRecord } from "@zcode/dynamic-workflow";

/**
* Final state snapshot and `artifacts` on `getRunDetail`: **User interface artifacts**.
 *
* ⚠ Terminology: The artifact here is the output of the script published to the user via `artifact.*` (journal `kind = "artifact"`
* line), **not** the `RunSettlement.artifact` inside the engine (the top-level return value of the script, called `output` on the port) /
* `result`). Two meanings of the same word.
 *
* The access is the same as the `reportsOf` of the observation surface - journal is the **persistent home** of the version history (memory-only)
* `workflowRuns.artifacts` projection only carries the latest version of metadata and is empty after cold recovery), while failed / canceled run
* Also hand over the product it has released (a run that dies in step 12 still delivers the previous picture).
 *
* `listArtifactRows` is not on the engine's {@link JournalStorePort} (the engine never enumerates or aggregates), so press
* House Planning **Capability Detection** continued: Absence means that the entire field is absent, instead of throwing an error - the injected test store and the one without introspection query
* Implementations must continue to be available.
 *
*Merge rules:
* - Only accept `completed` rows. The failed release does not account for the id or version number, and is included in the version.
* History will cause "version 3" to point to a byte in the UI that never existed.
* - Rows with the same id are sorted in ascending order by `version`; `title` / `contentType` / `sourcePath` / `spec` etc. are mentioned
* The top level is the value of the **latest version**, so that readers who only care about "what is now" do not have to look up versions themselves.
* - `itemCount` = the number of `report` rows tagged with this id (the amount of data in the preset dashboard, which is also the refresh signal of the UI).
* Content product is always 0.
 *
* `nodes` is the list of nodes already in hand by the caller (the path of the snapshot is the same `listNodes` for reports and this function to share);
* Pick up once by yourself in case of absence. The part time field is absent - an empty array reads like "ran but no output", while absence means "this run
* There is no concept of product."
 */
export function artifactsOf(
  runId: string,
  journal: JournalStorePort,
  nodes?: readonly NodeRecord[],
): { artifacts?: readonly DynamicWorkflowRunArtifact[] } {
  const candidate = journal as Partial<DwfRunIntrospectionQueries>;
  if (typeof candidate.listArtifactRows !== "function") return {};
  const rows = candidate.listArtifactRows(runId);
  if (rows.length === 0) return {};

// id → version accumulator. Insertion order = first occurrence order, which is the order of `artifacts` on the port contract.
  const byId = new Map<
    string,
    { kind: DynamicWorkflowRunArtifactKind; versions: DynamicWorkflowRunArtifactVersion[] }
  >();
  for (const row of rows) {
    if (row.status !== "completed") continue;
    const record = row.result;
    if (record === null || typeof record !== "object") continue;
    const version = artifactVersionOf(record as Record<string, unknown>);
    if (version === undefined) continue;
    const id =
      typeof row.artifactId === "string" && row.artifactId.length > 0
        ? row.artifactId
        : stringField(record as Record<string, unknown>, "id");
    const kind = artifactKindOf((record as Record<string, unknown>).kind);
    if (id === undefined || kind === undefined) continue;
    const bucket = byId.get(id);
    if (bucket === undefined) byId.set(id, { kind, versions: [version] });
    else bucket.versions.push(version);
  }
  if (byId.size === 0) return {};

// The tag count only scans the node table when there is a preset kanban: the `itemCount` of the content product is always 0, and `listNodes`
// It is a full table decoding. There are 50 lines on the central page, and this function is called once for each line. This short circuit is the only blocker on that path.
  const needsTally = [...byId.values()].some((bucket) => PRESET_ARTIFACT_KINDS.has(bucket.kind));
  const itemCounts = needsTally ? tagItemCounts(nodes ?? journal.listNodes(runId)) : undefined;
  const artifacts = [...byId].map(([id, bucket]) => {
    const versions = [...bucket.versions].sort((left, right) => left.version - right.version);
//Latest version = the one with the largest version number. Empty bucket is not possible (at least one during construction).
    const latest = versions[versions.length - 1]!;
// The flag is attached by id (the engine guarantees that it will be included in subsequent versions), even if it is included in any version - the row dropped by the old CLI does not have this key.
    const primary = versions.some((version) => version.primary === true);
    return {
      id,
      kind: bucket.kind,
      ...(latest.title === undefined ? {} : { title: latest.title }),
      ...(latest.description === undefined ? {} : { description: latest.description }),
      ...(latest.contentType === undefined ? {} : { contentType: latest.contentType }),
      ...(latest.sourcePath === undefined ? {} : { sourcePath: latest.sourcePath }),
      ...(latest.spec === undefined ? {} : { spec: latest.spec }),
      version: latest.version,
      versions,
      itemCount: itemCounts?.get(id) ?? 0,
      ...(primary ? { primary: true as const } : {}),
    } satisfies DynamicWorkflowRunArtifact;
  });
// Deliverables take the lead, and the rest maintain the order of first release:
// The order is determined here once. Snapshot/GetWorkflowRun/hub/v4 queries are all read from here, and any upper bound cannot cut it.
// `Array.prototype.sort` is stable since ES2019, so the relative order between non-primary objects does not change.
  artifacts.sort((left, right) => Number(right.primary === true) - Number(left.primary === true));
  return { artifacts };
}

/** Count by id of tag report rows (`kind = "report" ∧ artifact_id = ?` - the amount of data in the Kanban board). */
function tagItemCounts(nodes: readonly NodeRecord[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const node of nodes) {
    if (node.kind !== "report" || node.artifactId === undefined) continue;
    counts.set(node.artifactId, (counts.get(node.artifactId) ?? 0) + 1);
  }
  return counts;
}

const ARTIFACT_KINDS: ReadonlySet<string> = new Set<DynamicWorkflowRunArtifactKind>([
  "file",
  "markdown",
  "chart",
  "table",
  "metrics",
  "board",
]);

/** Four members of the preset Kanban: only they will be fed data by the label `report`, and the `itemCount` of the content product is always 0. */
const PRESET_ARTIFACT_KINDS: ReadonlySet<string> = new Set<DynamicWorkflowRunArtifactKind>([
  "chart",
  "table",
  "metrics",
  "board",
]);

function artifactKindOf(value: unknown): DynamicWorkflowRunArtifactKind | undefined {
  return typeof value === "string" && ARTIFACT_KINDS.has(value)
    ? (value as DynamicWorkflowRunArtifactKind)
    : undefined;
}

/**
* `ArtifactVersionRecord` on `dwf_node.result_json` → Version entry for the port. The shape of the rows comes from the engine with
* driver, but it goes through a JSON round trip and may come from an older CLI, so each field is defensively narrowed: `version`
* If it is not a positive integer, the entire line is discarded (a version without a version number cannot be located on the UI, and the bytes cannot be read).
 */
function artifactVersionOf(
  record: Record<string, unknown>,
): DynamicWorkflowRunArtifactVersion | undefined {
  const version = record.version;
  if (typeof version !== "number" || !Number.isInteger(version) || version < 1) return undefined;
  const bytes = record.bytes;
// The driver always writes `publishedAt`, which is optional based on the engine type. Give 0 in absence instead of dropping rows:
// A product without a timestamp is still viewable, and discarding it will make the version number appear empty in the UI.
  const publishedAt = typeof record.publishedAt === "number" ? record.publishedAt : 0;
  const title = stringField(record, "title");
  const description = stringField(record, "description");
  const contentType = stringField(record, "contentType");
  const uri = stringField(record, "uri");
  const sourcePath = stringField(record, "sourcePath");
  return {
    version,
    ...(title === undefined ? {} : { title }),
    ...(description === undefined ? {} : { description }),
    ...(contentType === undefined ? {} : { contentType }),
    ...(typeof bytes === "number" && Number.isFinite(bytes) ? { bytes } : {}),
    ...(uri === undefined ? {} : { uri }),
    ...(sourcePath === undefined ? {} : { sourcePath }),
    ...(record.spec === undefined ? {} : { spec: record.spec }),
    publishedAt,
    ...(record.primary === true ? { primary: true as const } : {}),
  };
}

function stringField(record: Record<string, unknown>, key: string): string | undefined {
  const value = record[key];
  return typeof value === "string" ? value : undefined;
}
