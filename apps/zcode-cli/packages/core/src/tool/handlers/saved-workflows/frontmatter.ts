// ============================================================
// Saved workflows - frontmatter codec
// ============================================================
//
//File shape:
//
//     /* zcode-workflow
//     description: ...
//     args:
//       pr: { type: string, required: true }
//     */
//     <plain dwf script>
//
// Why is it **block comment** instead of `---` fence like Markdown: The saved file extension is `.dwf.ts`,
// The user will open it in the editor or modify it manually. Block comments make the entire file still valid TypeScript, so highlighting,
// Bracket matching and formatting all work as usual; `---` will turn the first line of the file into a syntax error.
//
// The body uses YAML instead of JSON: `yaml` is already a direct dependency of @zcode/core (no new dependencies are added), and manual modification
// A block of YAML is much more error-tolerant than hand-modifying a block of JSON with quotes and commas - the readers of this file are humans.

import { parse as parseYaml, stringify as stringifyYaml } from "yaml";
import { SavedWorkflowMetaSchema, type SavedWorkflowMeta } from "@zcode/contracts";

/** The opening flag of frontmatter. Must be the first non-blank section of the file. */
export const SAVED_WORKFLOW_SENTINEL = "/* zcode-workflow";

/** The end tag of frontmatter: a line with only the end of the block comment. */
const SAVED_WORKFLOW_TERMINATOR = "*/";

export type SavedWorkflowParseErrorReason =
  | "missing_frontmatter"
  | "unterminated_frontmatter"
  | "invalid_yaml"
  | "invalid_metadata";

export type SavedWorkflowParseResult =
  | {
      ok: true;
      meta: SavedWorkflowMeta;
      script: string;
      /**
* How many lines are before the body (leading blank line + start tag + YAML + ending line). When the diagnosis is reported by **file line**
* Just add it: `fileLine = bodyLine + bodyLineOffset`. The compiler looks at the section after the termination line, so the two sets of line numbers must be different.
* This constant; let each call point count it by itself, is the standard origin of bugs such as "the diagnostic line number does not match the file".
       */
      bodyLineOffset: number;
    }
  | { ok: false; reason: SavedWorkflowParseErrorReason; detail: string };

/**
* Metadata + script → file body.
 *
* The script is placed **byte by byte** after the termination line: when saving and reading it back, you must get the copy written by the author, otherwise the run script hash
* Does not match what the user sees in the editor (the comparison benchmark of resume is the original text of the script).
 */
export function serializeSavedWorkflow(meta: SavedWorkflowMeta, script: string): string {
// The key order is fixed (description → whenToUse → args) rather than the insertion order of the object literal: save twice to get
// Files that are the same byte by byte, otherwise SaveWorkflow will create a meaningless diff in git every time.
  const body: Record<string, unknown> = { description: meta.description };
  if (meta.whenToUse !== undefined) body.whenToUse = meta.whenToUse;
  if (meta.args !== undefined) body.args = meta.args;

// stringify comes with a trailing newline, so the terminating line follows it directly.
  return `${SAVED_WORKFLOW_SENTINEL}\n${stringifyYaml(body)}${SAVED_WORKFLOW_TERMINATOR}\n${script}`;
}

/**
* File body → metadata + script.
 *
* Each of the four failures has its own name, because they give different advice to the user: no frontmatter is "This is not a save
* workflow", unclosed means "you deleted a line", bad YAML means "the indentation is wrong", and schema means "the field name is written wrong".
* A general "parse error" won't help in any of the three cases.
 */
export function parseSavedWorkflow(source: string): SavedWorkflowParseResult {
  const lines = source.split("\n");

  let start = 0;
  while (start < lines.length && lines[start]!.trim() === "") start += 1;
  if (start >= lines.length || lines[start]!.trim() !== SAVED_WORKFLOW_SENTINEL) {
    return {
      ok: false,
      reason: "missing_frontmatter",
      detail: `file does not start with the \`${SAVED_WORKFLOW_SENTINEL}\` metadata block`,
    };
  }

  let end = start + 1;
  while (end < lines.length && lines[end]!.trim() !== SAVED_WORKFLOW_TERMINATOR) end += 1;
  if (end >= lines.length) {
    return {
      ok: false,
      reason: "unterminated_frontmatter",
      detail: `metadata block is never closed with \`${SAVED_WORKFLOW_TERMINATOR}\``,
    };
  }

  const bodyText = lines.slice(start + 1, end).join("\n");
// Everything after the terminating line is script and left as is (the same holds true for files without a newline at the end: slice gives an empty array).
  const script = lines.slice(end + 1).join("\n");

  let body: unknown;
  try {
    body = parseYaml(bodyText);
  } catch (error) {
    return {
      ok: false,
      reason: "invalid_yaml",
      detail: error instanceof Error ? error.message : String(error),
    };
  }

  const parsed = SavedWorkflowMetaSchema.safeParse(body);
  if (!parsed.success) {
    return {
      ok: false,
      reason: "invalid_metadata",
      detail: parsed.error.issues
        .map((issue) => `${issue.path.join(".") || "(root)"}: ${issue.message}`)
        .join("; "),
    };
  }

// The text starts on the line next to the terminating line, so there are exactly `end + 1` lines before it.
  return { ok: true, meta: parsed.data, script, bodyLineOffset: end + 1 };
}
