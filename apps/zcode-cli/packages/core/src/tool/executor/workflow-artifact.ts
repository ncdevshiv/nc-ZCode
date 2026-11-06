/**
* The serialization rules of workflow run products are shared by two places in core: `<result>` (background-tasks.ts) of completion notification
* With `resultText` on the runtime task entry (background-task-registry.ts, the only source of TaskOutput).
 *
* Implementation** moved to `@zcode/contracts`** (`interfaces/dynamic-workflow-run.port.ts`, with
* `boundDynamicWorkflowRunEventPayload` the same seam), because it has a third consumer: bootstrap v4
* Projection should be calculated using the same rule `workflowRuns.reports[].preview` (Results area of ​​the details page). bootstrap cannot
* Import core's internal module, and copying a serialization rule means "the same value looks different in the notification and on the panel"
* Source. Keep one here and export it so that the two import sites on the core side do not move.
 */
import { serializeWorkflowArtifact } from "@zcode/contracts";

export { serializeWorkflowArtifact };

/**
* The budget for the `<reports>` section in the completion notice: the maximum number of characters in the entire section.
 *
* The notification end originally had a total truncation of 120k, but that was the **last** gate: it cut the fields at the back first. gradual product
* There can be 256, and spreading it out will crowd out notifications for `<result>` and `<error>` - those are the two the model looks at first.
*So bring your own budget for this section.
 */
const WORKFLOW_REPORTS_PREVIEW_MAX_CHARS = 8_000;
/** Single boundary. A 32KB product should not eat up the entire section budget, making it impossible for the next ten products to enter. */
const WORKFLOW_REPORT_ITEM_MAX_CHARS = 2_000;

interface WorkflowReportsNotificationSection {
/** **Real total number of items** (not the number of items in the preview). */
  count: number;
/** The actual number of items in the preview; less than count means the preview is partial. */
  shown: number;
  preview: string;
}

/**
* The report entry in the journal → the `<reports>` section of the completion notification (common to completed / failed / canceled).
 *
* Truncation and counting **Both are important**: Counting is what lets the main agent know that "the preview is partial and the full amount can be retrieved through run id"
* That signal. Giving only a preview will make the model think it has seen everything; giving only a count means returning no product at all.
 *
* Returns `undefined` when there are zero records, and the caller accordingly leaves the entire section absent - does not send an empty `<reports>`.
 */
export function buildWorkflowReportsNotificationSection(
  items: readonly unknown[] | undefined,
): WorkflowReportsNotificationSection | undefined {
  if (items === undefined || items.length === 0) return undefined;

  const lines: string[] = [];
  let used = 0;
  for (const [index, item] of items.entries()) {
    const text = clip(serializeWorkflowArtifact(item) ?? "", WORKFLOW_REPORT_ITEM_MAX_CHARS);
    const line = `[${index + 1}] ${text}`;
// Give at least one: A super long product is more useful than "reported 11 but can't see any of them".
    if (lines.length > 0 && used + line.length + 1 > WORKFLOW_REPORTS_PREVIEW_MAX_CHARS) break;
    lines.push(line);
    used += line.length + 1;
  }

  return { count: items.length, shown: lines.length, preview: lines.join("\n") };
}

/**
* The `<reports>` section of the manifest payload (`WorkflowNotificationMeta.reports`). with the one above
* **Notification text** uses the same source as item-by-item serialization (`serializeWorkflowArtifact`), but outputs **string[]**
* Instead of a piece of spliced ​​text - the manifest is rendered into a numbered preview one by one by the GUI, and splicing will cause it to be split again.
 *
* The bounds are deliberately different from the notification text section: each item here is ≤500, with a maximum of 8 items (synchronized with the shared `workflowNotificationMetaSchema` - exceeding the bounds will cause zod to reject the turnHeader row when it is dropped).
* `count` is always the **real total number**: `count ≠ shown` is the signal that "the preview is partial and the full amount is available via run id".
 *
* Returns `undefined` when there are zero fields, so the caller leaves the whole field absent.
 */
export function buildWorkflowReportsManifestSection(
  items: readonly unknown[] | undefined,
): { count: number; shown: number; preview: string[] } | undefined {
  if (items === undefined || items.length === 0) return undefined;

  const preview: string[] = [];
  for (const item of items) {
    if (preview.length >= WORKFLOW_NOTIFICATION_REPORTS_MAX_ITEMS) break;
    preview.push(clip(serializeWorkflowArtifact(item) ?? "", WORKFLOW_NOTIFICATION_REPORT_ITEM_MAX_CHARS));
  }
  return { count: items.length, shown: preview.length, preview };
}

/** The boundaries of product previews in the manifest payload (shared schema: ≤500 characters). */
const WORKFLOW_NOTIFICATION_REPORT_ITEM_MAX_CHARS = 500;
/** The maximum number of preview items in the manifest payload (shared schema: ≤8 items). */
const WORKFLOW_NOTIFICATION_REPORTS_MAX_ITEMS = 8;

function clip(text: string, max: number): string {
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}

