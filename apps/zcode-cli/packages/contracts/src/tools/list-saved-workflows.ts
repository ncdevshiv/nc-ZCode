// ============================================================
// ListSavedWorkflows Tool - enumerate the dwf definitions saved by this project
// ============================================================
//
// There are **two things** with ListWorkflowRuns: the one listed is the run (history), and the one listed can be used for running.
// definition (list). The names are intentionally separated on "Runs" / "SavedWorkflows" because the most common mistake models make is to
// "What workflows are available" becomes "What workflows have been run".

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";
import { SavedWorkflowEntrySchema, SavedWorkflowInvalidEntrySchema } from "./saved-workflow.js";

export const LIST_SAVED_WORKFLOWS_TOOL_NAME = "ListSavedWorkflows";

export const ListSavedWorkflowsInputSchema = z
  .object({})
// The same constraint as ListWorkflowRuns: deliberately no cwd input, the tool always scans the working directory of the current session.
// The model does not have the right to scan across projects, which is also the prerequisite for `sideEffectScope: "none"` to be established.
  .strict();

export type ListSavedWorkflowsInput = z.infer<typeof ListSavedWorkflowsInputSchema>;

export const ListSavedWorkflowsInputJsonSchema = toToolJsonSchema(ListSavedWorkflowsInputSchema);

export const ListSavedWorkflowsOutputSchema = z
  .object({
    workflows: z.array(SavedWorkflowEntrySchema),
/** Unreadable file. Absent when empty - An empty array will cause each call to hang a noise field. */
    invalid: z.array(SavedWorkflowInvalidEntrySchema).optional(),
  })
  .strict();

export type ListSavedWorkflowsOutput = z.infer<typeof ListSavedWorkflowsOutputSchema>;

export const ListSavedWorkflowsOutputJsonSchema = toToolJsonSchema(ListSavedWorkflowsOutputSchema);
