// ============================================================
// `path` source: read a script file on disk into execution facts
// ============================================================
//
// Four tools share this section (`CreateWorkflow.path`, `AmendWorkflow.path`, `SaveWorkflow.script_path`,
// `EvalWorkflowSnippet.path`). The reason for sharing is not to save a few lines: they must give the same answer to "what is this file?"
// Answer, otherwise the same draft in Create is "save definition with metadata block", but in Save it becomes "The first line of the text is
// a comment" script - and that fork is only discovered once the user saves the draft as a definition.
//
// Read once, read only once: the read bytes are then brought all the way to hook, confirmation window and handler (contract of `resolveInput`),
// Changing the file after approval will not change what will be executed.

import { readFile } from "node:fs/promises";
import path from "node:path";
import type { SavedWorkflowMeta } from "@zcode/contracts";
import { SAVED_WORKFLOW_SENTINEL, parseSavedWorkflow } from "./saved-workflows/index.js";
import { describeWorkflowScriptPath } from "./workflow-script-path.js";

/** Read successfully script file. */
interface WorkflowScriptFile {
/** Absolute path (journal and model face each take what they need: the former stores it, the latter displays {@link described}). */
  path: string;
/** The writing method that should be seen on the model surface: give a relative path under the work area, otherwise an absolute path. */
  described: string;
/** Original text of the file, byte by byte. */
  source: string;
/** The section to be executed / to be saved: if there is a metadata block, it is the text, if there is not, it is the entire file. */
  script: string;
/** Statement parsed from the metadata block; absent when the file has no blocks. */
  meta?: SavedWorkflowMeta;
/** The number of lines before the text (0 if there is no block); add it when the diagnosis is reported by file line. */
  bodyLineOffset: number;
}

type WorkflowScriptFileResult =
  | { ok: true; file: WorkflowScriptFile }
  | { ok: false; message: string };

/**
* Relative paths are parsed according to the session working directory, and absolute paths are normalized as they are - the same rule as `Read` to accept the next path.
* (`resolveWorkspacePath` in core/src/tool/path-policy.ts).
 *
* The helper is not removed here: it requires a `workspaceRoot`, and the context of `resolveInput`
* (`ToolInputResolutionContext`) is intentionally narrow to only `workingDirectory`. Both in `operation: "read"`
The behavior on * is the same - that function currently does not block paths outside the workspace, but only does the same resolve/normalize.
 */
function resolveWorkflowScriptFilePath(cwd: string, inputPath: string): string {
  return path.isAbsolute(inputPath) ? path.normalize(inputPath) : path.resolve(cwd, inputPath);
}

/**
* Read a script file. If it cannot be read, or the metadata block is corrupted, the structured failure text of **named file** will be returned - if no name is specified,
* The model will only think that it has just written something wrong, and then try again as it is.
 *
* `parseFrontmatter: false` for `EvalWorkflowSnippet`: the snippet does not save the set of semantics defined, the entire file
* It's code, and a fragment that happens to start with `/* zcode-workflow` should not be swallowed as a declaration block.
 */
export async function readWorkflowScriptFile(options: {
  cwd: string;
  inputPath: string;
  parseFrontmatter?: boolean;
}): Promise<WorkflowScriptFileResult> {
  const absolute = resolveWorkflowScriptFilePath(options.cwd, options.inputPath);
  const described = describeWorkflowScriptPath(absolute, options.cwd);

  let source: string;
  try {
    source = await readFile(absolute, "utf8");
  } catch (error) {
    return {
      ok: false,
      message: `The workflow script file ${described} could not be read: ${describeError(error)}. Pass \`path\` for a file that exists, or submit the script inline.`,
    };
  }

  const base = { path: absolute, described, source };
  if (options.parseFrontmatter === false || !startsWithSentinel(source)) {
    return { ok: true, file: { ...base, script: source, bodyLineOffset: 0 } };
  }

  const parsed = parseSavedWorkflow(source);
  if (!parsed.ok) {
    return {
      ok: false,
      message: `The workflow script file ${described} starts with a \`${SAVED_WORKFLOW_SENTINEL}\` metadata block that could not be read (${parsed.reason}): ${parsed.detail}. Fix the block in that file, or remove it and pass the script alone.`,
    };
  }
  return {
    ok: true,
    file: {
      ...base,
      script: parsed.script,
      meta: parsed.meta,
      bodyLineOffset: parsed.bodyLineOffset,
    },
  };
}

/**
* Is the first non-blank line a start marker. The criteria are word-for-word consistent with {@link parseSavedWorkflow}'s own entry - two places
* If you give different answers to "Is this a saved definition", there will be a dead end of "the tool says there is no block, the parser says the block is broken".
 */
function startsWithSentinel(source: string): boolean {
  for (const line of source.split("\n")) {
    if (line.trim() === "") continue;
    return line.trim() === SAVED_WORKFLOW_SENTINEL;
  }
  return false;
}

function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
