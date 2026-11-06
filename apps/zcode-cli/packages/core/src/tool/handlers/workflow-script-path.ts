// ============================================================
// How to write the model side of the workflow script file
// ============================================================
// The absolute path stored in the journal is always **absolute path**: it is part of the run identity, and the working directory of the session will change (`cd`,
// Another session reads the same run), and the relative path will point to other files elsewhere if saved.
//
// But the model should see the writing in the workspace: the next thing it needs to do is `Edit` the file, and the workspace relative path
// Exactly the one it uses when reading and writing files elsewhere. The only difference between the two is this pure function - cast once and shared by three model surfaces.
// (final state notification, `GetWorkflowRun`, tool response), so it is impossible to branch between the three places.

import path from "node:path";

/**
* Write the absolute path of the script file as it should be seen in the model: give the relative path to the workspace under the session working directory,
* Otherwise, give the absolute path as is.
 *
* The criterion for "under the working directory" is that the result of `path.relative` neither starts with `..` nor is it an absolute path——
* This article blocks two situations that cannot be relativized at the same time: sibling paths outside the directory (`../other/x.dwf.ts`, relative writing
* reads like there is such a thing in the workspace), and spanning drive letters on Windows (`C:` → `D:`, `path.relative`
* Directly return the absolute path). An empty result (the path is exactly equal to the working directory) also returns an absolute path: the empty string is not a valid
* The file name of `Edit`.
 *
* The absence of `cwd` means that the host has no working directory concept (partial port stub / no session context), and there is no way to relativize it at this time.
 */
export function describeWorkflowScriptPath(absolutePath: string, cwd: string | undefined): string {
  if (cwd === undefined || cwd === "") return absolutePath;
  const relative = path.relative(cwd, absolutePath);
  if (relative === "" || relative.startsWith("..") || path.isAbsolute(relative)) {
    return absolutePath;
  }
  return relative;
}
