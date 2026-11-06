// ============================================================
// ResumeWorkflowRun Tool - Resume a canceled / Interrupted workflow run
// ============================================================
// see port
// Contract for `DynamicWorkflowRunPort.resume` (interfaces/dynamic-workflow-run.port.ts).
//
// This is the third entry point for workflow run recovery (after the UI details page button and CLI /dwf resume). The execution base zero
// Change: The same `port.resume(runId)`, the same runId to continue running in place (the script is nailed by scriptHash, and the actual participation
// caps follows journal records, completed nodes are purely replayed, and unfinished nodes are redistributed).
//
// The output is only in success form: if it fails, go to ToolHandlerFailure (core side) and do not enter this schema - so all fields are required.
// None optional. `status: "backgrounded"` tells the executor's automatic tracking to claim it according to the output shape, go
//CreateWorkflow same trackBackgroundTask.

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";

export const RESUME_WORKFLOW_RUN_TOOL_NAME = "ResumeWorkflowRun";

export const ResumeWorkflowRunInputSchema = z
  .object({
// snake_case depends on the run_id of GetWorkflowRun (which in turn depends on the task_id of TaskOutput): In the eyes of the model, this
// The three keys are run/task identifiers of the same family. The recoverable set (cancelled ∪ failed+Interrupted) is written in
// In describe, the authority is determined on the port.resume server - here it is just routing guidance.
    run_id: z
      .string()
      .min(1)
      .describe(
        "The workflow run ID to resume — a cancelled run or one that failed with code `Interrupted`, as seen with GetWorkflowRun or ListWorkflowRuns",
      ),
  })
  .strict();

export type ResumeWorkflowRunInput = z.infer<typeof ResumeWorkflowRunInputSchema>;

export const ResumeWorkflowRunInputJsonSchema = toToolJsonSchema(ResumeWorkflowRunInputSchema);

/**
* Successful output: run has resumed and is flying in the background.
 *
* - `backgroundTaskId ≡ runId` (the same identity as the backgrounded output of CreateWorkflow), cancel,
* TaskOutput query and final notification paths share this key.
* - `status` only accepts `"backgrounded"` literal: the executor's background tracking is triggered according to this shape, with one more value
* Just one more life cycle branch to explain.
* - `response` is the guidance copy for the model (do not poll, wait for notification), constructed by the core handler - it is prose, not
* Contract fields, schema are only guaranteed to be present.
 */
export const ResumeWorkflowRunOutputSchema = z
  .object({
    ok: z.literal(true),
    runId: z.string().min(1),
    response: z.string(),
    status: z.literal("backgrounded"),
    backgroundTaskId: z.string().min(1),
  })
  .strict();

export type ResumeWorkflowRunOutput = z.infer<typeof ResumeWorkflowRunOutputSchema>;

export const ResumeWorkflowRunOutputJsonSchema = toToolJsonSchema(ResumeWorkflowRunOutputSchema);
