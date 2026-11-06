// ============================================================
// Workflow draft path - pure judgment of "Does this writing target fall in the draft directory?"
// ============================================================
//
// There are two reasons for forming a separate module: PermissionService only needs a Boolean value and does not need to know how to compare paths;
// This judgment is the **only** gate that does not require confirmation, and must be able to be tested case by case without service (including Windows semantics).
//Do not touch the file system in the whole process: no realpath, no stat. Permission determination must be a pure function - the same input on any machine
// Give the same answer, and will not misjudge "do not release" as "release" because of a disk read failure.

import nodePath from "node:path";

import { WORKFLOW_DRAFTS_DIR } from "@zcode/contracts";

/**
* Determine the subset of `node:path` required. The parameters are made to inject `path.win32` / `path.posix` in the test:
* Windows semantics such as drive letters, backslashes, and uppercase and lowercase letters can only be verified by running them with win32.
 */
interface WorkflowDraftPathModule {
  isAbsolute: (path: string) => boolean;
  relative: (from: string, to: string) => string;
  resolve: (...paths: string[]) => string;
}

interface WorkflowDraftPathInput {
/** The writing target in the tool input, the relative path is resolved according to workingDirectory, and the absolute path is retained as is. */
  filePath: string;
/** Session working directory; if absent (empty string), it is not valid, see below. */
  workingDirectory: string;
/** Platform path implementation, defaults to the `node:path` of the current platform. */
  pathModule?: WorkflowDraftPathModule;
}

/**
* Built-in file writing tool that requires no confirmation when hitting a draft. These two share the `edit` permission name, and both point to a single `file_path`
* Target - It is meaningful to determine whether "the target is in the draft directory". ApplyPatch, also `edit`, is not among them: its input is
* `patch_text` is the patch text. Multiple files can be modified at one time. There is no single path to determine.
 */
const WORKFLOW_DRAFT_PREAPPROVED_TOOL_NAMES = new Set(["Edit", "Write"]);

interface WorkflowDraftWriteInput {
  toolName: string;
/** Tool input (not yet parsed according to the schema of the specific tool), only `file_path` is taken from it. */
  input: unknown;
/** Session working directory; if the caller cannot get it, it can be absent. If it is absent, it will not be confirmed. */
  workingDirectory?: string;
  pathModule?: WorkflowDraftPathModule;
}

/**
* Whether this tool call is "writing to the draft directory", that is, whether the confirmation window can be avoided.
 *
* Why it is safe: That directory is owned by the machine - the files in it are written by the tool itself, and the directory comes with `*` .gitignore
* Without entering the repository, users will not put things that need to be protected, and changing a draft will not affect anything in the project. and
* There is another gate to "let the script run": CreateWorkflow declares alwaysAsk, and the confirmation window looks at the submitted script.
* No matter what the file has been changed to. So releasing the write here does not mean releasing any execution.
 *
* If there is no workingDirectory (the caller did not pass it), it will not be established: the prerequisite for avoiding confirmation is that it can calculate where the target falls.
* The window that should pop up when the calculation cannot be calculated.
 */
export function isPreapprovedWorkflowDraftWrite(input: WorkflowDraftWriteInput): boolean {
  if (!WORKFLOW_DRAFT_PREAPPROVED_TOOL_NAMES.has(input.toolName)) return false;
  if (typeof input.workingDirectory !== "string") return false;
  if (!input.input || typeof input.input !== "object") return false;
  const filePath = (input.input as Record<string, unknown>).file_path;
  if (typeof filePath !== "string") return false;
  return isWorkflowDraftPath({
    filePath,
    pathModule: input.pathModule,
    workingDirectory: input.workingDirectory,
  });
}

/** Whether `filePath` falls within `<workingDirectory>/.zcode/workflow-drafts/`. */
function isWorkflowDraftPath(input: WorkflowDraftPathInput): boolean {
  const path = input.pathModule ?? nodePath;
// Without a working directory, there is no "draft directory for which project" to speak of. It is better not to release it: the premise of avoiding confirmation is that the target can be located.
  if (input.workingDirectory.length === 0 || input.filePath.length === 0) return false;

  const draftsDir = path.resolve(input.workingDirectory, WORKFLOW_DRAFTS_DIR);
  const resolved = path.resolve(input.workingDirectory, input.filePath);
  const relativePath = path.relative(draftsDir, resolved);

// Three conditions are indispensable: non-empty excludes "the target is the directory itself", non-absolute excludes cross-drive letters (on Windows
// `relative("C:\\a", "D:\\b")` returns the absolute path instead of `..`), not starting with `..` excludes traversal.
// Only look at the path string, not the file system: `.zcode/workflow-drafts-other/x.ts` so it will not be affected by the prefix
// The same is misjudged as being in the directory - `relative` gives `../workflow-drafts-other/x.ts`.
  return (
    relativePath.length > 0 && !path.isAbsolute(relativePath) && !relativePath.startsWith("..")
  );
}
