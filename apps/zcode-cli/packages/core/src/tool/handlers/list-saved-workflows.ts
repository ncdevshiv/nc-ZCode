// ============================================================
// ListSavedWorkflows Tool Handler
// ============================================================
// Enumerate the dwf definitions saved by this project (= session working directory).
//
// and ListWorkflowRuns are **two things**, and both descriptions must make this clear: the one listed is the run run
// (history, stateful, with runId), this list lists the definitions that can be run (list, stateless, with name). The model is the most
//An easy mistake is to ask "What workflows are available" instead of "What workflows have run", and then answer the user "You don't have any
//Workflow" - and there are actually five of them in the project.

import {
  LIST_SAVED_WORKFLOWS_TOOL_NAME,
  ListSavedWorkflowsInputJsonSchema,
  ListSavedWorkflowsInputSchema,
  ListSavedWorkflowsOutputJsonSchema,
  ListSavedWorkflowsOutputSchema,
  SAVED_WORKFLOW_PROJECT_DIR,
  type ListSavedWorkflowsOutput,
  type ModelMessageContent,
} from "@zcode/contracts";
import type { ToolEntry, ToolHandler } from "../types.js";
import { listSavedWorkflows } from "./saved-workflows/index.js";

const LIST_SAVED_WORKFLOWS_TIMEOUT_MS = 10_000;
/** According to ListWorkflowRuns: The list is deliberately light (one directory scan can answer), 24k is enough for dozens of items and still leaves a margin. */
const LIST_SAVED_WORKFLOWS_MODEL_BYTES = 24_000;

const LIST_SAVED_WORKFLOWS_DESCRIPTION = [
  `Lists the dynamic workflows saved in this project (\`${SAVED_WORKFLOW_PROJECT_DIR}/\`, keyed on the session's working directory) and the global archive (\`~/.zcode/workflows\`, available from every project). These are workflow DEFINITIONS you can run, not past runs — for the run history use ListWorkflowRuns instead.`,
  "",
  "- Each row gives the name, what the workflow does, when to reach for it, and the arguments it takes.",
  "- Run one by passing its name to CreateWorkflow as `saved: { name, args }`. The user still confirms the run.",
  "- Check here before writing a workflow from scratch: if the project already saved one that fits, running it beats rebuilding it.",
  "- `invalid` lists saved files that could not be read (usually a hand-edited metadata block). They are named so they can be fixed, not silently skipped.",
].join("\n");

const listSavedWorkflowsHandler: ToolHandler = async (input, context) => {
  ListSavedWorkflowsInputSchema.parse(input);

// cwd always retrieves the working directory of this session: the model does not have permission to scan across projects, which is also the prerequisite for `sideEffectScope: "none"`.
  const { entries, invalid } = listSavedWorkflows({ cwd: context.workingDirectory ?? "." });

  return {
    workflows: entries,
// Absent when empty: An empty array will add a noise field to each call.
    ...(invalid.length > 0 ? { invalid } : {}),
  } satisfies ListSavedWorkflowsOutput;
};

/**
* Model side: an XML-ish container + a workflow piece.
 *
* Deliberately different from the **single-line** attribute format of ListWorkflowRuns: run is a high-cardinality entity with low information density (50 lines long
* The same thing, the attributes fit into one row), and a saved workflow comes with a description, usage time and parameter list - these are models
* Used to **select** the basis for the workflow. Squeezing it into a row will suppress the information required for selection. The number of items is also much lower (a few to a few in one project)
* Dozens), supporting several lines per line.
 */
function formatListSavedWorkflowsModelContent(output: unknown): ModelMessageContent {
  const parsed = ListSavedWorkflowsOutputSchema.safeParse(output);
  if (!parsed.success) return "ListSavedWorkflows returned an invalid result.";

  const { workflows, invalid } = parsed.data;

  if (workflows.length === 0 && invalid === undefined) {
// "This project has no saved workflow" must be said in one sentence: an empty container is easily read as "the tool is not available".
    return [
      '<saved_workflows count="0">',
      `No workflows are saved in this project yet. Saved definitions live in ${SAVED_WORKFLOW_PROJECT_DIR}/.`,
      "</saved_workflows>",
    ].join("\n");
  }

  const blocks = workflows.map((workflow) => {
    const lines = [
      `<workflow name="${escapeAttribute(workflow.name)}" scope="${workflow.scope}">`,
      `  ${workflow.description}`,
    ];
    if (workflow.whenToUse !== undefined) lines.push(`  When to use: ${workflow.whenToUse}`);
    for (const [key, spec] of Object.entries(workflow.args ?? {})) {
      const notes = [
        spec.type,
        spec.required === true ? "required" : undefined,
        spec.default === undefined ? undefined : `default ${JSON.stringify(spec.default)}`,
      ].filter((note) => note !== undefined);
      const description = spec.description === undefined ? "" : ` — ${spec.description}`;
      lines.push(`  arg ${key} (${notes.join(", ")})${description}`);
    }
    lines.push("</workflow>");
    return lines.join("\n");
  });

  const invalidLines = (invalid ?? []).map(
    (entry) => `<invalid path="${escapeAttribute(entry.path)}">${entry.reason}</invalid>`,
  );

  return [
    `<saved_workflows count="${workflows.length}">`,
    ...blocks,
    ...invalidLines,
    "</saved_workflows>",
  ].join("\n");
}

/** Name and path attributes: Both may be quoted (especially paths), and not escaping will create malformed tags. */
function escapeAttribute(value: string): string {
  return value.replaceAll("&", "&amp;").replaceAll('"', "&quot;").replaceAll("<", "&lt;");
}

export const listSavedWorkflowsToolEntry: ToolEntry = {
  capability: "List the reusable dynamic-workflow definitions saved in this project",
  metadata: {
    name: LIST_SAVED_WORKFLOWS_TOOL_NAME,
    description: LIST_SAVED_WORKFLOWS_DESCRIPTION,
    readOnly: true,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: LIST_SAVED_WORKFLOWS_TIMEOUT_MS,
    maxOutputBytes: LIST_SAVED_WORKFLOWS_MODEL_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
    needsApproval: false,
  },
  handler: listSavedWorkflowsHandler,
  inputSchema: ListSavedWorkflowsInputJsonSchema,
  outputSchema: ListSavedWorkflowsOutputJsonSchema,
  runtimeInputSchema: ListSavedWorkflowsInputSchema,
  runtimeOutputSchema: ListSavedWorkflowsOutputSchema,
  formatModelContent: formatListSavedWorkflowsModelContent,
  permission: {
    permission: "listSavedWorkflows",
    reason: "ListSavedWorkflows reads the saved workflow definitions of the current project",
    riskLevel: "low",
    sideEffectScope: "none",
    needsApproval: false,
// There is no path body in the input (cwd comes from the session context), so the pattern only matches by tool name.
    patternSources: ["toolName"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
// Deliberately **not** inherit the alwaysAsk of SaveWorkflow / CreateWorkflow: the reasons for the two gates are
// "Write the user's warehouse" and "Execute the entire code", the read list does not belong to either one.
  },
  resultBudget: {
    maxInlineBytes: LIST_SAVED_WORKFLOWS_MODEL_BYTES,
    maxModelBytes: LIST_SAVED_WORKFLOWS_MODEL_BYTES,
    strategy: "truncate",
    preview: {
      maxBytes: LIST_SAVED_WORKFLOWS_MODEL_BYTES,
      direction: "head",
    },
  },
  timeout: {
    kind: "timed",
    defaultMs: LIST_SAVED_WORKFLOWS_TIMEOUT_MS,
    maxMs: LIST_SAVED_WORKFLOWS_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: false,
    cleanup: "none",
    userVisibleMessage:
      "ListSavedWorkflows scans the project's workflow directory synchronously and cannot be cancelled",
  },
  trace: {
    required: true,
    propagateToAdapters: false,
    recordInput: "summary",
    recordOutput: "summary",
  },
};
