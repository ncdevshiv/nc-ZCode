/**
* The **user interface product** of workflow run is projected in a row on the model surface and is shared by three places:
* The `<artifacts>` section of the completion notification (background-tasks.ts), the `artifacts[]` of the manifest payload (same as above),
* and the `<artifacts>` section of the `GetWorkflowRun` model text (handlers/get-workflow-run.ts).
 *
* ⚠ Terminology: artifacts in this file are scripts published via `artifact.*`
* Output for the user** - a file, a markdown, or a graph / table / indicator / fed by the tag `report`
* Kanban. It is **two different things** from `serializeWorkflowArtifact` in `workflow-artifact.ts` next door:
* That artifact is the top-level return value of the script (`RunSettlement.artifact` inside the engine), which is shown to the model.
* Both appear at the same time in the completion notification (`<result>` and `<artifacts>`), so the files are deliberately separated.
 *
* Why the format of a line is only written once: the readers of the three places are the same model, and the format bifurcation will make them think that the product in the notice is the same as the one in the notice.
* The products in `GetWorkflowRun` are two sets of things.
 */

/** All the facts required for a product on the model side. The input shapes of the three places are different, and this structure is obtained after taking the intersection. */
interface PublishedArtifactSummary {
  id: string;
  kind: string;
  version: number;
  title?: string;
  contentType?: string;
/** The number of bytes in the latest version of the content product (file/markdown). The preset Kanban board has no bytes. */
  bytes?: number;
/** The number of tags `report` received by the preset kanban (amount of data). The content product is always 0. */
  itemCount?: number;
/** run deliverables: list starts with it, line superscript `primary`. */
  primary?: true;
/** Only this sentence of the deliverable goes into the manifest: the completion card draws it as a line with text. */
  description?: string;
}

/** The bounds of the deliverable `description` in the manifest (`ARTIFACT_CAPS.maxDescriptionLength`, the same value as the shared zod). */
const WORKFLOW_ARTIFACT_DESCRIPTION_MAX_CHARS = 500;

/**
* Deliverables take the lead, and the rest remain in the original order (stable sorting). The three lists all go through this step before cutting, so the upper boundary cannot cut the deliverables.
* The `artifactsOf` of the port has already been sorted; here it is sorted again for snapshots and notification paths (they are read in projected order).
 */
function primaryFirst<T extends { primary?: true }>(artifacts: readonly T[]): T[] {
  return [...artifacts].sort(
    (left, right) => Number(right.primary === true) - Number(left.primary === true),
  );
}

/** The maximum number of rows `<artifacts>` in the completion notification can be listed. The upper bound on the load (≤ 8) is intentionally the same as the number of lines of text. */
export const WORKFLOW_ARTIFACTS_NOTIFICATION_MAX_LINES = 8;
/** `<artifacts>` section upper bound for `GetWorkflowRun` (bounded 32 = per-run upper limit of `ARTIFACT_CAPS`). */
export const WORKFLOW_ARTIFACTS_INTROSPECTION_MAX_LINES = 32;
/** The limit of the title in the manifest payload (`ARTIFACT_CAPS.maxTitleLength`, the same value as the shared zod). */
const WORKFLOW_ARTIFACT_TITLE_MAX_CHARS = 120;
/** The bounds of id (`ARTIFACT_CAPS.maxIdLength`, the same value as shared's zod). */
const WORKFLOW_ARTIFACT_ID_MAX_CHARS = 64;
/** The bounds of contentType (same value as shared's zod; MIME itself does not have such a long legal form). */
const WORKFLOW_ARTIFACT_CONTENT_TYPE_MAX_CHARS = 255;

/** Four members of the preset Kanban board: they have no bytes, and the amount of data is the number of items in the label report. */
const PRESET_ARTIFACT_KINDS: ReadonlySet<string> = new Set(["chart", "table", "metrics", "board"]);

/**
* A product → a line: `- {id} ({kind}, v{version}, {contentType}, {bytes} bytes): {title}`.
 *
* Absent parts are omitted entirely instead of leaving a gap - `(file, v2, , : )` reads like the data is corrupted. Preset kanban bytes
* Replaced with `{n} items`: the "size" of an image is how many points it has, and the number of bytes has no meaning to it (it has no bytes at all).
* If `title` is absent, the colon is omitted (the default title of the facade is the id, and there is no information if repeated).
 *
* Deliberately not giving `uri`**: the model cannot read the tool-artifact store. If you want content just go to `GetWorkflowRun`, or let
* The subagent reads the original path in the workspace.
 */
export function formatPublishedArtifactLine(artifact: PublishedArtifactSummary): string {
// `primary` follows the type: `- report (markdown, primary, v2, …)` - this is how the model knows which item to give to the user first.
  const parts = artifact.primary === true ? [artifact.kind, "primary"] : [artifact.kind];
  parts.push(`v${artifact.version}`);
  if (artifact.contentType !== undefined && artifact.contentType.length > 0) {
    parts.push(artifact.contentType);
  }
  if (PRESET_ARTIFACT_KINDS.has(artifact.kind)) {
    if (artifact.itemCount !== undefined) {
      parts.push(`${artifact.itemCount} item${artifact.itemCount === 1 ? "" : "s"}`);
    }
  } else if (artifact.bytes !== undefined) {
    parts.push(`${artifact.bytes} bytes`);
  }
  const head = `- ${artifact.id} (${parts.join(", ")})`;
  const title = artifact.title?.trim();
  return title === undefined || title.length === 0 ? head : `${head}: ${title}`;
}

/** `<artifacts count shown>` The three things required for a section, the same shape as the section `<reports>`. */
interface WorkflowArtifactsNotificationSection {
/** **Real total number of pieces** (not the number of listed rows). */
  count: number;
/** The actual number of rows listed; less than count means the list is partial. */
  shown: number;
  preview: string;
}

/**
* Artifact List → Completion Notifications / `<artifacts>` section of `GetWorkflowRun`.
 *
* Same as `<reports>`: `count` is always the real total number, `count ≠ shown` means "the list is partial and full through run id
* Desirable" signal. `undefined` is returned when the part is returned, and the caller accordingly leaves the entire section absent - does not send an empty `<artifacts>`.
 */
export function buildWorkflowArtifactsNotificationSection(
  artifacts: readonly PublishedArtifactSummary[] | undefined,
  maxLines: number,
): WorkflowArtifactsNotificationSection | undefined {
  if (artifacts === undefined || artifacts.length === 0) return undefined;
  const lines = primaryFirst(artifacts).slice(0, maxLines).map(formatPublishedArtifactLine);
  return { count: artifacts.length, shown: lines.length, preview: lines.join("\n") };
}

/** An entry in the manifest payload (`WorkflowNotificationMeta.artifacts`). Shared zods are bounded by the same group. */
interface WorkflowArtifactManifestEntry {
  id: string;
  kind: "file" | "markdown" | "chart" | "table" | "metrics" | "board";
  title?: string;
  version: number;
  contentType?: string;
  primary?: true;
/** Only on the `primary` entry (read it after completing the card's deliverable; no need to fit the rest of the chips). */
  description?: string;
}

/**
* `artifacts` and `artifactsTruncated` of the manifest payload.
 *
* **Same origin but different form** from the above section: the GUI renders each item as a chip (icon + truncated title), so the payload is
* Structured fields instead of assembled lines; `bytes` / `itemCount` deliberately do not enter the load - the chip cannot be placed on it, just click on the side panel
* See all. Truncation is completed before **construction**: the load follows the turnHeader row protocol, and if it exceeds the bounds, zod will reject it when the entire row is dropped into the database.
 *
* `kind` entries that are not in the six literals are discarded entirely (no guessing, no normalization): an unknown type of chip is not available in the GUI
* The icon is drawable, but allowing it will cause the entire payload to be rejected by zod, taking the remaining chips with it.
 *
* Returns `undefined` when a part is used, in which case the caller makes both fields absent together.
 */
export function buildWorkflowArtifactsManifestSection(
  artifacts: readonly PublishedArtifactSummary[] | undefined,
): { artifacts: WorkflowArtifactManifestEntry[]; artifactsTruncated?: true } | undefined {
  if (artifacts === undefined || artifacts.length === 0) return undefined;
  const entries: WorkflowArtifactManifestEntry[] = [];
//Advanced list of deliverables: The upper bound of 8 will never cut it.
  for (const artifact of primaryFirst(artifacts)) {
    if (entries.length >= WORKFLOW_ARTIFACTS_NOTIFICATION_MAX_LINES) break;
    const kind = manifestArtifactKind(artifact.kind);
// id is the identity key of the chip. Truncation is to make up a non-existent product - the super boundary can only be lost in its entirety, which is the same as the unknown kind.
// Both are "a bad line should not take the rest of the chip with it": passing it will cause zod to reject the entire payload.
    if (kind === undefined || artifact.id.length === 0) continue;
    if (artifact.id.length > WORKFLOW_ARTIFACT_ID_MAX_CHARS) continue;
// The version number is one of the positioning keys of the chip. The shared zod requires a positive integer; non-positive integers are also discarded.
    if (!Number.isInteger(artifact.version) || artifact.version < 1) continue;
    const title = artifact.title?.trim();
// contentType is just a decoration on the chip. If you exceed the limit, you will lose this one key instead of the entire one.
    const contentType =
      artifact.contentType !== undefined &&
      artifact.contentType.length <= WORKFLOW_ARTIFACT_CONTENT_TYPE_MAX_CHARS
        ? artifact.contentType
        : undefined;
    const description = artifact.primary === true ? artifact.description?.trim() : undefined;
    entries.push({
      id: artifact.id,
      kind,
      ...(title === undefined || title.length === 0
        ? {}
        : { title: title.slice(0, WORKFLOW_ARTIFACT_TITLE_MAX_CHARS) }),
      version: artifact.version,
      ...(contentType === undefined ? {} : { contentType }),
      ...(artifact.primary === true ? { primary: true as const } : {}),
      ...(description === undefined || description.length === 0
        ? {}
        : { description: description.slice(0, WORKFLOW_ARTIFACT_DESCRIPTION_MAX_CHARS) }),
    });
  }
  if (entries.length === 0) return undefined;
// Truncation of honesty: what is cut off by the upper bound and what is filtered out by types are considered "there are more that have not been drawn" - the criterion is therefore
// "The number drawn is less than the total number", not "hit the upper bound".
  const truncated = entries.length < artifacts.length;
  return { artifacts: entries, ...(truncated ? { artifactsTruncated: true as const } : {}) };
}

const MANIFEST_ARTIFACT_KINDS: readonly WorkflowArtifactManifestEntry["kind"][] = [
  "file",
  "markdown",
  "chart",
  "table",
  "metrics",
  "board",
];

function manifestArtifactKind(kind: string): WorkflowArtifactManifestEntry["kind"] | undefined {
  return MANIFEST_ARTIFACT_KINDS.find((known) => known === kind);
}

/**
* Product on Snapshot/Port Details → Input form of this module. `bytes` is only on the version entry (the top level of the port intentionally does not carry it),
* So take the **latest version** - that's the one the listing is describing.
 *
* The form is still defensively checked: input comes across packages (port implementation, or a line returned from a cold recovery), and a bad entry should not destroy the entire section
* disappear. The entire line that does not fit the shape is skipped.
 */
export function toPublishedArtifactSummaries(
  value: unknown,
): PublishedArtifactSummary[] | undefined {
  if (!Array.isArray(value) || value.length === 0) return undefined;
  const summaries: PublishedArtifactSummary[] = [];
  for (const entry of value) {
    if (entry === null || typeof entry !== "object") continue;
    const record = entry as Record<string, unknown>;
    const id = typeof record.id === "string" ? record.id : undefined;
    const kind = typeof record.kind === "string" ? record.kind : undefined;
    const version = typeof record.version === "number" ? record.version : undefined;
    if (id === undefined || kind === undefined || version === undefined) continue;
    const versions = Array.isArray(record.versions) ? record.versions : undefined;
    const latest = versions?.[versions.length - 1];
    const bytes =
      latest !== null &&
      typeof latest === "object" &&
      typeof (latest as Record<string, unknown>).bytes === "number"
        ? ((latest as Record<string, unknown>).bytes as number)
        : undefined;
    summaries.push({
      id,
      kind,
      version,
      ...(typeof record.title === "string" ? { title: record.title } : {}),
      ...(typeof record.contentType === "string" ? { contentType: record.contentType } : {}),
      ...(bytes === undefined ? {} : { bytes }),
      ...(typeof record.itemCount === "number" ? { itemCount: record.itemCount } : {}),
      ...(record.primary === true ? { primary: true as const } : {}),
      ...(typeof record.description === "string" ? { description: record.description } : {}),
    });
  }
  return summaries.length === 0 ? undefined : summaries;
}
