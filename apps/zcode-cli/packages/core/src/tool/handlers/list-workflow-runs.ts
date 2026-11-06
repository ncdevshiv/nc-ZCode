// ============================================================
// ListWorkflowRuns Tool Handler
// ============================================================
// Enumerate workflow runs by project (= session's working directory), with cross-session history.
//
// The handler is deliberately **very thin**: labels, attribution annotations, status synthesis, and timestamps are all cooked and handed over by the run service
// (The port's `DynamicWorkflowRunListItem` comment explains why the raw material does not pass the boundary). There are only three things to do here:
// Get the port, pin cwd to the session's working directory, and project the result into a contract shape.

import {
  LIST_WORKFLOW_RUNS_TOOL_NAME,
  ListWorkflowRunsInputJsonSchema,
  ListWorkflowRunsInputSchema,
  ListWorkflowRunsOutputJsonSchema,
  ListWorkflowRunsOutputSchema,
  type ListWorkflowRunsInput,
  type ListWorkflowRunsOutput,
  type ModelMessageContent,
} from "@zcode/contracts";
import type { ToolEntry, ToolHandler } from "../types.js";
import {
  WORKFLOW_RUN_INTROSPECTION_STEERING,
  formatWorkflowRunTimestamp,
  workflowIntrospectionUnavailableFailure,
  workflowRunAttribute,
} from "./workflow-run-introspection.js";

const LIST_WORKFLOW_RUNS_TIMEOUT_MS = 10_000;
/** According to CreateWorkflow: the list is deliberately light (a single line of SQL can be answered), 24k is enough for 50 lines and there is still room left. */
const LIST_WORKFLOW_RUNS_MODEL_BYTES = 24_000;

const LIST_WORKFLOW_RUNS_DESCRIPTION = [
  "Lists this project's dynamic-workflow runs (the session's working directory is the project key), most recently updated first. Includes runs started by other sessions — the run journal is per-project, not per-session.",
  "",
  WORKFLOW_RUN_INTROSPECTION_STEERING,
  "",
  "- Each row gives the run ID, its label, lifecycle status, whether this session started it, tokens spent, and timestamps.",
  '- `possibly_interrupted="true"` means this session cannot confirm the run is still alive: it may be a leftover from a process that exited, or a sibling session\'s run still in flight. It is an annotation, not a verdict — do not report it as a failure.',
  "- Pass a run ID to GetWorkflowRun for progress detail, the log tail, the final result, or the failure.",
  "- Three terminal states: `completed`; `errored` (the script itself failed); `stopped` with `stop_reason` — `user` (cancelled on purpose: resume only when the user asks), `model` (your own TaskStop), `provider` (a provider-side error stopped it: read GetWorkflowRun for the cause, resolve it with the user, then resume), `interrupted` (the owning process exited: continuing it is usually what the user wants), `superseded` (an AmendWorkflow replaced it; `superseded_by` names the live successor — never resume a superseded run).",
  "- Any stopped run other than a superseded one can be continued with ResumeWorkflowRun — no rebuild needed, same run ID, same script. An errored run cannot.",
  "- ANY run here — completed, stopped, errored, or still running — can instead be revised with AmendWorkflow: pass its run ID and the corrected script; the new run imports the old one's finished work as a warm cache (and stops it first if it is still running). A run whose script errored is the case to reach for it — fix the script instead of rewriting the workflow from scratch. `resumed_from` on a row names the run it was amended from.",
].join("\n");

const listWorkflowRunsHandler: ToolHandler = async (input, context) => {
  const parsed = ListWorkflowRunsInputSchema.parse(input) as ListWorkflowRunsInput;

  const port = context.dynamicWorkflowRunPort;
// "Port absent" and "port present but method absent" give the same business failure: it's the same thing for the model. Optional member press
// typeof detection (precedent set by `cancel` in the port contract).
  if (port === undefined || typeof port.listRuns !== "function") {
    return workflowIntrospectionUnavailableFailure();
  }

  const result = await port.listRuns({
// cwd always retrieves the working directory of this session: the model does not have the right to scan libraries across projects, which is also the prerequisite for `sideEffectScope: "none"`.
// Literal equivalent matching, no path normalization - the writing side (submit) is dropped as is, and the reading side is checked as it is. Normalization will only create a one-sided mismatch.
    cwd: context.workingDirectory,
// The bounds are clamped by the input schema to [1, 50] (preprocess), so the port never sees an unbounded enumeration.
    limit: parsed.limit,
    ...(parsed.statuses === undefined ? {} : { statuses: parsed.statuses }),
  });

  return {
    runs: result.runs.map((item) => ({
      runId: item.runId,
      label: item.label,
      labelSource: item.labelSource,
      status: item.status,
      ...(item.stopReason === undefined ? {} : { stopReason: item.stopReason }),
      ...(item.resumedFrom === undefined ? {} : { resumedFrom: item.resumedFrom }),
      ...(item.supersededBy === undefined ? {} : { supersededBy: item.supersededBy }),
      ownedByThisSession: item.ownedByThisSession,
// Only present if true: `false` will hang a noise field on each line.
      ...(item.possiblyInterrupted ? { possiblyInterrupted: true } : {}),
      createdAt: item.createdAt,
      updatedAt: item.updatedAt,
      spentTokens: item.spentTokens,
    })),
    ...(result.truncated ? { truncated: true } : {}),
  } satisfies ListWorkflowRunsOutput;
};

/**
* Model side: an XML-ish container + **one run per line**.
 *
* Why not split each field into independent elements as in TaskOutput: that's the shape of a single object detail, 50 rows × 8 elements will
* The list reads hundreds of lines at a time, approaching a 24k budget without changing the information density. Attributed single lines retain the same set of XML-ish tag conventions
* (which the model parses reliably), while allowing 50 rows to still be 50 rows.
 */
function formatListWorkflowRunsModelContent(output: unknown): ModelMessageContent {
  const parsed = ListWorkflowRunsOutputSchema.safeParse(output);
  if (!parsed.success) return "ListWorkflowRuns returned an invalid result.";

  const { runs, truncated } = parsed.data;
  const header = [
    workflowRunAttribute("count", runs.length),
    ...(truncated ? [workflowRunAttribute("truncated", true)] : []),
  ].join(" ");

  if (runs.length === 0) {
// "This project has not run through the workflow" must be said in one sentence: an empty container is easily read as "the tool has not been answered".
    return `<workflow_runs ${header}>\nNo workflow runs recorded for this project.\n</workflow_runs>`;
  }

  const rows = runs.map((run) =>
    [
      "<run",
      workflowRunAttribute("id", run.runId),
      workflowRunAttribute("status", run.status),
      ...(run.stopReason === undefined
        ? []
        : [workflowRunAttribute("stop_reason", run.stopReason)]),
      ...(run.resumedFrom === undefined
        ? []
        : [workflowRunAttribute("resumed_from", run.resumedFrom)]),
      ...(run.supersededBy === undefined
        ? []
        : [workflowRunAttribute("superseded_by", run.supersededBy)]),
      workflowRunAttribute("label", run.label),
      workflowRunAttribute("label_source", run.labelSource),
      workflowRunAttribute("owned_by_this_session", run.ownedByThisSession),
      ...(run.possiblyInterrupted ? [workflowRunAttribute("possibly_interrupted", true)] : []),
      workflowRunAttribute("spent_tokens", run.spentTokens),
      workflowRunAttribute("created_at", formatWorkflowRunTimestamp(run.createdAt)),
      workflowRunAttribute("updated_at", formatWorkflowRunTimestamp(run.updatedAt)),
      "/>",
    ].join(" "),
  );

  return [`<workflow_runs ${header}>`, ...rows, "</workflow_runs>"].join("\n");
}

export const listWorkflowRunsToolEntry: ToolEntry = {
  capability: "List this project's dynamic-workflow runs, including runs from other sessions",
  metadata: {
    name: LIST_WORKFLOW_RUNS_TOOL_NAME,
    description: LIST_WORKFLOW_RUNS_DESCRIPTION,
    readOnly: true,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: LIST_WORKFLOW_RUNS_TIMEOUT_MS,
    maxOutputBytes: LIST_WORKFLOW_RUNS_MODEL_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
    needsApproval: false,
  },
  handler: listWorkflowRunsHandler,
  inputSchema: ListWorkflowRunsInputJsonSchema,
  outputSchema: ListWorkflowRunsOutputJsonSchema,
  runtimeInputSchema: ListWorkflowRunsInputSchema,
  runtimeOutputSchema: ListWorkflowRunsOutputSchema,
  formatModelContent: formatListWorkflowRunsModelContent,
  permission: {
    permission: "listWorkflowRuns",
    reason: "ListWorkflowRuns reads the run journal for the current project",
    riskLevel: "low",
    sideEffectScope: "none",
    needsApproval: false,
// There is no path body in the input (cwd comes from the session context), so the pattern only matches by tool name.
    patternSources: ["toolName"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
// Deliberately **not** inherit CreateWorkflow's alwaysAsk: the reason for that gate is to "execute the entire code",
// Read status does not belong to it.
  },
  resultBudget: {
    maxInlineBytes: LIST_WORKFLOW_RUNS_MODEL_BYTES,
    maxModelBytes: LIST_WORKFLOW_RUNS_MODEL_BYTES,
    strategy: "truncate",
    preview: {
      maxBytes: LIST_WORKFLOW_RUNS_MODEL_BYTES,
// head: The most recently updated runs are first, and the oldest ones are truncated.
      direction: "head",
    },
  },
  timeout: {
    kind: "timed",
    defaultMs: LIST_WORKFLOW_RUNS_TIMEOUT_MS,
    maxMs: LIST_WORKFLOW_RUNS_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: false,
    cleanup: "none",
    userVisibleMessage: "ListWorkflowRuns reads the run journal synchronously and cannot be cancelled",
  },
  trace: {
    required: true,
    propagateToAdapters: false,
    recordInput: "summary",
    recordOutput: "summary",
  },
};
