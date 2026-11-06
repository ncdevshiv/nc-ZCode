// ============================================================
// SaveWorkflow Tool - Save a dwf script along with metadata as a reusable definition in the project
// ============================================================

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";
import { CreateWorkflowDiagnosticSchema } from "./create-workflow.js";
import {
  SAVED_WORKFLOW_MAX_NAME_CHARS,
  SavedWorkflowArgsDeclarationSchema,
  SavedWorkflowScopeSchema,
  SavedWorkflowShadowingSchema,
} from "./saved-workflow.js";

export const SAVE_WORKFLOW_TOOL_NAME = "SaveWorkflow";

/**
* "Give exactly one text source" violation description. The field is not called `path` because `path` has already been parsed and backfilled by this tool**
* Drop point ** (confirmation window and UI read it), two fields with the same name, one is input and the other is output, will only make the model regard the save target as
* The source file is passed in.
 */
export const SAVE_WORKFLOW_SOURCE_ERROR =
  "Provide exactly one script source: `script` for the body inline, or `script_path` for the file holding it (a draft, usually). Passing both, or neither, is ambiguous.";

/** Business failure description when the model passes in the script with its own frontmatter. */
export const SAVE_WORKFLOW_SENTINEL_IN_SCRIPT_ERROR =
  "The `script` must be the workflow body only — it already starts with a `/* zcode-workflow` metadata block. Pass the metadata through the `description` / `whenToUse` / `args` fields instead; the block is written for you.";

export const SaveWorkflowInputSchema = z
  .object({
    name: z
      .string()
      .min(1)
      .max(SAVED_WORKFLOW_MAX_NAME_CHARS)
      .describe(
        "File-safe identifier the workflow will be recalled by. Letters, digits, dot, dash and underscore only.",
      ),
    description: z
      .string()
      .min(1)
      .describe("One line saying what the workflow does. Shown wherever the workflow is listed."),
    whenToUse: z
      .string()
      .min(1)
      .optional()
      .describe("Optional guidance on the situations this workflow is the right answer to."),
    args: SavedWorkflowArgsDeclarationSchema.optional().describe(
      "Optional argument declarations. The script reads validated values off the `args` global.",
    ),
    script: z
      .string()
      .optional()
      .describe(
        "Full TypeScript workflow script written against the dynamic-workflow facade, exactly as CreateWorkflow takes it. Body only — do not include a metadata block. Provide this OR `script_path`, never both.",
      ),
    /**
* The second source of the text: a file already on the disk,
* Usually the draft you just ran. When the file has a metadata block, the block is discarded - the metadata is determined by the field called this time.
* The user approves those fields, not the one in the document.
     */
    script_path: z
      .string()
      .min(1)
      .optional()
      .describe(
        "The file holding the script body, relative to the working directory or absolute — usually a draft a CreateWorkflow/AmendWorkflow result named. Provide this OR `script`, never both; it saves a working draft without re-emitting it. A `/* zcode-workflow` block in that file is dropped: the metadata comes from this call's `description` / `whenToUse` / `args`.",
      ),
// The scope is stated by the **model**, not guessed by the system - there is no default value, it must be judged every time. Criterion Neutral: Whether the script
//Reference something from this warehouse? Yes → project, no → global.
    scope: SavedWorkflowScopeSchema.describe(
      'Where the workflow is saved. "project" when the script references this repository\'s files, commands, conventions or directory layout; "global" when it depends on nothing in this project and should be available from every project (saved under ~/.zcode/workflows). Decide every time; there is no default.',
    ),
// ——The following three fields are parsed and backfilled by `resolveInput`, and are not filled in by the model——
// They are the facts to be displayed in the confirmation window: which file is saved this time, whether it is overwritten once, and whether it blocks another file.
// Go to parameter instead of display because the parameter channel is transparent transmission without schema for each client version. The old desktop and
// legacy v3 so the full content can also be seen.
/** Parsing backfill: file placement point. */
    path: z.string().min(1).optional(),
/** Parsing backfill: The target already exists, and what is approved this time is an **override**. */
    overwrite: z.boolean().optional(),
/** Parsing backfill: Another file already has a definition with the same name (covering the fact for display in the confirmation window). Absent = no namesake. */
    shadowing: SavedWorkflowShadowingSchema.optional(),
  })
  .strict();

export type SaveWorkflowInput = z.infer<typeof SaveWorkflowInputSchema>;

export const SaveWorkflowInputJsonSchema = toToolJsonSchema(SaveWorkflowInputSchema);

/**
* The output shares a schema with CreateWorkflow's diagnostic shape: both tools run the same type checker,
* Having the model see different shaped diagnoses in two places will only teach it to write two sets of parses.
 *
* `overwritten` only appears when the file is actually written - the result of diagnostic-only is not "overwritten", send one
* `false` will make the model think that the placement is successful.
 */
export const SaveWorkflowOutputSchema = z
  .object({
    diagnostics: z.array(CreateWorkflowDiagnosticSchema),
    ok: z.boolean(),
    response: z.string(),
    name: z.string().min(1),
    scope: SavedWorkflowScopeSchema,
/** File placement point (when not written, it is the location where it should have been written, so that the model can explain the error clearly). */
    path: z.string().min(1),
    overwritten: z.boolean().optional(),
  })
  .strict();

export type SaveWorkflowOutput = z.infer<typeof SaveWorkflowOutputSchema>;

export const SaveWorkflowOutputJsonSchema = toToolJsonSchema(SaveWorkflowOutputSchema);
