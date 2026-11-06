// ============================================================
// Writing of Workflow draft directory
// ============================================================
// Every script accepted by the tool must have a home on the disk. That file is the **handle** between the two submissions of the model: diagnosis to
// is the file line number. You only need `path` for the next submission. You don’t have to reflow the script worth 20,000 tokens to change one line. inline text therefore
// Just a door - `CreateWorkflow` / `AmendWorkflow` / The hub starts directly as soon as a script is received that does not come from a file.
//Write one here.
//
// Drop point `<cwd>/.zcode/workflow-drafts/`, and `.zcode/workflows/` (user-saved definition),
// `.zcode/workflow-runs/` (compilation entry for each run) is flat. The directory comes with a copy of `.gitignore: *`, written in the same way
// dynamic-workflow-runtime/src/child-entry-file.ts verbatim isomorphism (comment there notes the verdict): only in absence
//Write it once and the user will not touch it again after making changes. The project's own `.gitignore` will not touch a word.
//
// **Best effort**: unable to write (read-only checkout, `.zcode` is an ordinary file, the disk is full) does not allow the call to fail and returns
// `undefined`, when the model reads it, the old saying "change the script and then submit it inline" is returned. Deliberately **not** fall back to the temporary directory - a
// Drafts that the user can't find in the project don't deserve a path.

import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import {
  SAVED_WORKFLOW_FILE_EXTENSION,
  WORKFLOW_DRAFTS_DIR,
  createWorkflowPhaseNames,
  type CreateWorkflowCausalityGraph,
} from "@zcode/contracts";

/**
* Draft filenames are **not** reserved characters: any Unicode letters/digits and everything except `_ . -`.
 *
* Deliberately different from the name of the saved definition (pure ASCII): the saved name should be used as a CLI parameter and URL fragment, and the draft name should only be used in
* It exists on the file systems of the three platforms, and people and models can recognize which workflow this is - and the run name and stage name in the real disk
* Almost all are in Chinese (actual test: the ASCII version compresses every draft into `workflow-N`, two completely different
* Workflow cannot be distinguished in the directory). What is lost outside the alphabet are path separators and Windows reserved characters.
* (`/ \ : * ? " < > |`), whitespace and control characters, path traversal is therefore still not possible.
 */
const WORKFLOW_DRAFT_SLUG_DROP_PATTERN = /[^\p{L}\p{N}_.-]/gu;

/** The blanks are connected into a `-`: `PR review #12` → `PR-review-12`, which is more recognizable than the crowded `PRreview12`. */
const WORKFLOW_DRAFT_WHITESPACE_PATTERN = /\s+/gu;

/** The upper limit of file name length is the same number as `SAVED_WORKFLOW_MAX_NAME_CHARS`, and the reason is the same (PATH_MAX for each platform). */
const WORKFLOW_DRAFT_MAX_SLUG_CHARS = 64;

/**
* The answer when there are no usable characters left in the name (Chinese names are the most common).
 */
const WORKFLOW_DRAFT_FALLBACK_SLUG = "workflow";

/**
* Upper bound for suffixes with the same name. If it hits so many times, it can only be because someone is using the same name to submit, and at this time give up writing the draft (return
* `undefined`) is better than infinite loops - drafting is a convenience, not a link to correctness.
 */
const WORKFLOW_DRAFT_MAX_ATTEMPTS = 1_000;

interface WriteWorkflowDraftInput {
  /**
* The session working directory; drafts fall under it in `.zcode/workflow-drafts/`. Absence means that the host has no concept of working directory
* (port stub / no session context), there is nowhere to write at this time, synonymous with write failure.
   */
  cwd: string | undefined;
/** The display name of run, used to cast the file name. */
  name: string;
/** The bytes to be written, verbatim (saved source with metadata block). */
  source: string;
}

/**
* Write a draft and return its absolute path; if it cannot be written, it returns `undefined` (never thrown).
 *
* **Create a new file** for every inline commit, including a commit that should have gone `path`: drafts are never overwritten behind models,
* Otherwise, a repeated submission by mistake will erase the copy that the user is editing. Conflicts with the same name are postponed according to `-2`, `-3`... and used
* `wx` (exclusive creation) commit - two concurrent commits, so it is impossible to fall into the same file, the "stat first and then write" kind of writing method.
 */
export async function writeWorkflowDraft(
  input: WriteWorkflowDraftInput,
): Promise<{ path: string } | undefined> {
  if (input.cwd === undefined || input.cwd === "") return undefined;
  try {
    const dir = path.join(input.cwd, WORKFLOW_DRAFTS_DIR);
    await mkdir(dir, { recursive: true });
    await writeDraftGitignore(dir);
    const slug = workflowDraftSlug(input.name);
    for (let attempt = 1; attempt <= WORKFLOW_DRAFT_MAX_ATTEMPTS; attempt += 1) {
      const fileName = `${attempt === 1 ? slug : `${slug}-${attempt}`}${SAVED_WORKFLOW_FILE_EXTENSION}`;
      const filePath = path.join(dir, fileName);
      try {
// `wx`: Report EEXIST when the file already exists, so "name" and "name" are the same atomic action.
        await writeFile(filePath, input.source, { encoding: "utf8", flag: "wx" });
        return { path: filePath };
      } catch (error) {
        if (errorCode(error) !== "EEXIST") throw error;
      }
    }
    return undefined;
  } catch {
// Try your best: the directory cannot be written, `.zcode` is a file, the disk is full... all will be treated as "there is no draft this time". The caller accordingly
// Return the old copy and the tool call itself completes as usual.
    return undefined;
  }
}

/**
* What should the draft be called: the `name` given by the model takes precedence; if not, take the literal value of the first `phase("...")` in the script - the phase is each
* Scripts must be written and written in user language for users (Phases rules described by `CreateWorkflow`), so it is
* The thing that most resembles a name without a name; it falls into a vague form without even a stage.
 *
* Deliberately do not use run tags (the first line of the script): the first line in the real disk is mostly a comment banner such as `// ==== Result type ====`.
 */
export function resolveWorkflowDraftName(
  name: string | undefined,
  graph: Pick<CreateWorkflowCausalityGraph, "phases"> | undefined,
): string {
  if (name !== undefined && name.trim() !== "") return name;
  return createWorkflowPhaseNames(graph)?.[0] ?? WORKFLOW_DRAFT_FALLBACK_SLUG;
}

/**
* name → filename stem. Blanks are connected into `-`, and all characters outside the alphabet (Unicode letters / numbers / `_ . -`) are discarded.
* Cut to the upper limit (by code points, not by UTF-16 units, so as not to cut a word into half surrogate pairs); nothing or only points are left
* (`.` / `..` is a directory entry, not a file name).
 */
function workflowDraftSlug(name: string): string {
  const reduced = Array.from(
    name
      .trim()
      .replace(WORKFLOW_DRAFT_WHITESPACE_PATTERN, "-")
      .replace(WORKFLOW_DRAFT_SLUG_DROP_PATTERN, ""),
  )
    .slice(0, WORKFLOW_DRAFT_MAX_SLUG_CHARS)
    .join("")
    .replace(/^[.-]+|[.-]+$/gu, "");
  if (reduced === "" || /^\.+$/u.test(reduced)) return WORKFLOW_DRAFT_FALLBACK_SLUG;
  return reduced;
}

/** `.gitignore` in the draft directory is only written once in absence (the user will not touch it after making changes). */
async function writeDraftGitignore(dir: string): Promise<void> {
  try {
    await writeFile(path.join(dir, ".gitignore"), "*\n", { encoding: "utf8", flag: "wx" });
  } catch (error) {
    if (errorCode(error) !== "EEXIST") throw error;
  }
}

function errorCode(error: unknown): string | undefined {
  return (error as NodeJS.ErrnoException | undefined)?.code;
}
