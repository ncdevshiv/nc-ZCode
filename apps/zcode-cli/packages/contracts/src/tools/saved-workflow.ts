// ============================================================
// Saved workflows - a shared vocabulary of saved dwf definitions
// ============================================================
//
// This module **only** declares "what a saved workflow is": the legal shape of the name, file placement, parameter declaration and
// Metadata. Three tools (SaveWorkflow for writing, ListSavedWorkflows for reading, CreateWorkflow with `saved`
// source run) all use it as a contract, and the store on the core side also takes the schema from here - once the metadata shape is on the write side and the read side
//Evolve independently, the symptom is "the workflow just saved cannot be listed", and that is the most difficult type of fork to be caught by one-sided testing.

import { z } from "zod";

/**
* The extension of the saved file. `.dwf.ts` instead of `.ts`: editor highlights by TypeScript (frontmatter is block comment,
* syntactically legal), and the `.dwf` section allows scanning to distinguish it from the project source code without opening the file.
 */
export const SAVED_WORKFLOW_FILE_EXTENSION = ".dwf.ts";

/** The storage directory of the project scope (relative to the session working directory). */
export const SAVED_WORKFLOW_PROJECT_DIR = ".zcode/workflows";

/**
* Draft directory (relative to the session working directory). The script file for in-place editing of the model between commits falls here and is
* The sibling directory of `.zcode/workflows/` (user-saved definition), which is native to the machine and comes with `.gitignore: *`.
 */
export const WORKFLOW_DRAFTS_DIR = ".zcode/workflow-drafts";

/**
* The storage directory of the global scope (relative to the home directory of the agent process). Drop point `~/.zcode/workflows/<name>.dwf.ts`
* - Same as the user root of the legacy Workflow tool, visible to all projects.
 */
export const SAVED_WORKFLOW_GLOBAL_DIR = ".zcode/workflows";

/**
* The legal shape of the name. Same pattern as the parser for the old `Workflow` tool (script-workflow-tool-port.ts) -
* The name must be used as a file name at the same time, so slashes, `..`, and blank spaces are not allowed: this regular rule is the defense line for path crossing.
*Not a style preference.
 */
export const SAVED_WORKFLOW_NAME_PATTERN = /^[A-Za-z0-9_.-]+$/u;

/** Maximum name length. The filename needs to be consistent across platforms, 64 well within any PATH_MAX and sufficiently descriptive. */
export const SAVED_WORKFLOW_MAX_NAME_CHARS = 64;

/**
* Scope. Two files: `project` falls in `.zcode/workflows/` of the project and is only visible in that project; `global`
*Located in `~/.zcode/workflows/` (the home directory of the agent process), visible to all projects. The scope of a file is inferred from the directory in which it is located, and frontmatter does not exist.
 */
export const SAVED_WORKFLOW_SCOPES = ["project", "global"] as const;

export const SavedWorkflowScopeSchema = z.enum(SAVED_WORKFLOW_SCOPES);

export type SavedWorkflowScope = z.infer<typeof SavedWorkflowScopeSchema>;

/**
* Obscuring fact: Another file already has a definition with the same name. It is calculated when saving and displayed in the confirmation window.
* `hides_global`: This time the project file is saved, it will hide the global file with the same name in this project;
* `hidden_by_project`: This time the global file is saved. If there is a project file with the same name in this project, it will be obscured.
 */
export const SAVED_WORKFLOW_SHADOWING = ["hides_global", "hidden_by_project"] as const;

export const SavedWorkflowShadowingSchema = z.enum(SAVED_WORKFLOW_SHADOWING);

export type SavedWorkflowShadowing = z.infer<typeof SavedWorkflowShadowingSchema>;

/**
* Type vocabulary for parameters. Three primitives plus one `json` for the bottom line: the primitives can be verified as "wrong transmission", `json` clearly indicates
* "Everything is accepted here", so "no verification" and "no verification" are two different things in the statement, not the same hole.
 */
export const SAVED_WORKFLOW_ARG_TYPES = ["string", "number", "boolean", "json"] as const;

export const SavedWorkflowArgTypeSchema = z.enum(SAVED_WORKFLOW_ARG_TYPES);

export type SavedWorkflowArgType = z.infer<typeof SavedWorkflowArgTypeSchema>;

export const SavedWorkflowArgDeclarationSchema = z
  .object({
    type: SavedWorkflowArgTypeSchema.describe(
      'Value type. "json" accepts any JSON value without further checking.',
    ),
    description: z
      .string()
      .optional()
      .describe("What this argument means, for whoever calls the workflow later."),
    required: z
      .boolean()
      .optional()
      .describe("When true the workflow cannot run without this argument."),
// `default` is deliberately unknown rather than a union by `type`: the type correctness of the default value is determined by
// validateWorkflowArgs goes through the same verification as the incoming value after **applying the default value**, one rule instead of two.
    default: z.unknown().optional().describe("Value used when the caller omits this argument."),
  })
  .strict();

export type SavedWorkflowArgDeclaration = z.infer<typeof SavedWorkflowArgDeclarationSchema>;

export const SavedWorkflowArgsDeclarationSchema = z.record(SavedWorkflowArgDeclarationSchema);

export type SavedWorkflowArgsDeclaration = z.infer<typeof SavedWorkflowArgsDeclarationSchema>;

/**
* Metadata body in frontmatter. `.strict()` makes "misspelling a key" a visible invalid line instead of
* A field that is silently discarded - the saved file will be manually modified by the user, and typos must be pointed out.
 */
export const SavedWorkflowMetaSchema = z
  .object({
    description: z.string().min(1),
    whenToUse: z.string().min(1).optional(),
    args: SavedWorkflowArgsDeclarationSchema.optional(),
  })
  .strict();

export type SavedWorkflowMeta = z.infer<typeof SavedWorkflowMetaSchema>;

/** A row in the list: metadata + drop point, **excluding script text** (enumeration is not reading). */
export const SavedWorkflowEntrySchema = z
  .object({
    name: z.string().min(1),
    description: z.string(),
    whenToUse: z.string().optional(),
    args: SavedWorkflowArgsDeclarationSchema.optional(),
    scope: SavedWorkflowScopeSchema,
    path: z.string().min(1),
  })
  .strict();

export type SavedWorkflowEntry = z.infer<typeof SavedWorkflowEntrySchema>;

/**
* A file that exists but cannot be read. Listing **does not fail due to a bad file**: the user manually modified a frontmatter
*, the rest of the workflow must be available as usual, and the bad file must be named, otherwise it just disappears.
 */
export const SavedWorkflowInvalidEntrySchema = z
  .object({
    path: z.string().min(1),
    reason: z.string().min(1),
  })
  .strict();

export type SavedWorkflowInvalidEntry = z.infer<typeof SavedWorkflowInvalidEntrySchema>;

/** Whether the name can be used as a file name (i.e. whether it may point to a saved workflow). */
export function isValidSavedWorkflowName(name: string): boolean {
  if (name.length === 0 || name.length > SAVED_WORKFLOW_MAX_NAME_CHARS) return false;
  if (!SAVED_WORKFLOW_NAME_PATTERN.test(name)) return false;
// `.` and `..` passed the above character set check but are directory entries, not names.
  return name.replaceAll(".", "").length > 0;
}
