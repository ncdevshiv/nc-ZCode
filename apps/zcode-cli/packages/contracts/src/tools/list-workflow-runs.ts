// ============================================================
// ListWorkflowRuns Tool - enumerate workflow runs by project (cwd), including cross-session history
// ============================================================

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";

export const LIST_WORKFLOW_RUNS_TOOL_NAME = "ListWorkflowRuns";

/**
* Lifecycle vocabulary for run. Literally the same set as `DynamicWorkflowRunLifecycleStatus` (port side), but here
* Re-declared as zod enumeration: schema is the **runtime** verification surface, type-only union types are not useful here.
* Once the two places diverge, the symptom is that the legal state passed down by the model is rejected by the tool - so the comments on the port side also point back here.
 */
export const WORKFLOW_RUN_LIFECYCLE_STATUSES = [
  "completed",
  "errored",
  "pending",
  "running",
  "stopped",
] as const;

/** Reason for `stopped` (zod face of port `DynamicWorkflowRunStopReason`). */
export const WORKFLOW_RUN_STOP_REASONS = [
  "user",
  "model",
  "provider",
  "interrupted",
  "superseded",
] as const;

/** The limit and default value of limit. 50 is the upper limit that does not require cursor paging under "run is a low cardinality entity". */
export const LIST_WORKFLOW_RUNS_MIN_LIMIT = 1;
export const LIST_WORKFLOW_RUNS_MAX_LIMIT = 50;
export const LIST_WORKFLOW_RUNS_DEFAULT_LIMIT = 20;

export const ListWorkflowRunsInputSchema = z
  .object({
    limit: clampedLimit().describe(
      `Maximum number of runs to return (${LIST_WORKFLOW_RUNS_MIN_LIMIT}-${LIST_WORKFLOW_RUNS_MAX_LIMIT}, default ${LIST_WORKFLOW_RUNS_DEFAULT_LIMIT}). Most recently updated runs come first.`,
    ),
    statuses: z
      .array(z.enum(WORKFLOW_RUN_LIFECYCLE_STATUSES))
      .optional()
      .describe(
        "Optional status filter. Omit to see every run in this project. Pass [\"running\", \"pending\"] to see only what is still in flight.",
      ),
  })
// Deliberately no cwd input: the tool always checks the working directory of the current session (the model does not have permission to scan libraries across projects, which is also
// `sideEffectScope: "none"` is established). .strict() makes "passing one more cwd" a visible error.
  .strict();

export type ListWorkflowRunsInput = z.infer<typeof ListWorkflowRunsInputSchema>;

export const ListWorkflowRunsInputJsonSchema = toToolJsonSchema(ListWorkflowRunsInputSchema);

/**
* List and Details **Common Section** (schema facet of `DynamicWorkflowRunSummary` on the port side).
 *
* The reason why it is a shared field bag instead of two independently evolved field tables: the same run displays different fields in the list and details
* Name or attribution is the type of inconsistency that is most difficult to catch in testing and most directly damages trust. The field bag is placed on the **list** side,
* Because the list is the shallowest projection of this section; extend it for details.
 */
export const WorkflowRunSummarySchema = z.object({
  runId: z.string().min(1),
/** Cooked display tag (name → first line of script → runId derived from server side). */
  label: z.string(),
/** `"name"` = name given by the user; `"script"` = derived from the script when reading (including runId). */
  labelSource: z.enum(["name", "script"]),
  status: z.enum(WORKFLOW_RUN_LIFECYCLE_STATUSES),
/** Only present if `status === "stopped"` is present. */
  stopReason: z.enum(WORKFLOW_RUN_STOP_REASONS).optional(),
/** Which run this run was revised from (`dwf_run.resumed_from`); if it is not a revision, it will be absent. */
  resumedFrom: z.string().min(1).optional(),
/** Which revision this run was stopped and replaced (the settlement bag of `stopped(superseded)`); if it is not replaced, it is absent. */
  supersededBy: z.string().min(1).optional(),
  ownedByThisSession: z.boolean(),
/** The "This session cannot confirm that it is alive" annotation is present only when true. It's definitely not a status override. */
  possiblyInterrupted: z.boolean().optional(),
/** epoch ms (time_created / time_updated of journal). */
  createdAt: z.number(),
  updatedAt: z.number(),
});

export const ListWorkflowRunsRunSchema = WorkflowRunSummarySchema.extend({
  spentTokens: z.number(),
}).strict();

export type ListWorkflowRunsRun = z.infer<typeof ListWorkflowRunsRunSchema>;

export const ListWorkflowRunsOutputSchema = z
  .object({
    runs: z.array(ListWorkflowRunsRunSchema),
/** There are more runs that have not entered this page (press limit+1 to detect). Be present when it is true. */
    truncated: z.boolean().optional(),
  })
  .strict();

export type ListWorkflowRunsOutput = z.infer<typeof ListWorkflowRunsOutputSchema>;

export const ListWorkflowRunsOutputJsonSchema = toToolJsonSchema(ListWorkflowRunsOutputSchema);

/**
* limit is a clamp rather than a reject: there is no reason for a read-only enumeration to become a tool for the model to recover from because of an out-of-bounds number
* Bug; `limit` defaults to 20, and is clamped to [1, 50].
 *
* The clamp is placed in the preprocess instead of the handler. The picture shows that two things are true at the same time: JSON schema is still based on the inner layer
* schema projection (`type: integer` + minimum/maximum/default, the model can see the boundary), and the out-of-bounds value at runtime is
* Turn in bounds. `Math.min/max` written in the handler will be rejected first by the min/max of this schema and will never be executed.
* Dead code. It is an established convention to use preprocess for semantic coercion (task-output's semanticBoolean, bash's
 * timeout）。
 */
function clampedLimit(): z.ZodEffects<z.ZodDefault<z.ZodNumber>, number, unknown> {
  return z.preprocess((value) => {
    if (typeof value !== "number" || !Number.isFinite(value)) return value;
    return Math.min(
      LIST_WORKFLOW_RUNS_MAX_LIMIT,
      Math.max(LIST_WORKFLOW_RUNS_MIN_LIMIT, Math.trunc(value)),
    );
  }, z.number().int().min(LIST_WORKFLOW_RUNS_MIN_LIMIT).max(LIST_WORKFLOW_RUNS_MAX_LIMIT).default(LIST_WORKFLOW_RUNS_DEFAULT_LIMIT));
}
