/**
* Drop the sandbox entry file to disk.
 *
* Why this file exists: The command line limit of Windows is 32,767 characters, and the payload cannot exceed argv. harness handle
* {@link import("./child-source.js").renderChildEntry} The rendered ESM writes
* `<cwd>/.zcode/workflow-runs/<runId>.mjs`, this is the only path left in the command line when spawning.
 *
* Ruling:
* - The location `.zcode/workflow-runs/` is not mixed with the `.zcode/workflows/` of the project-level saved workflow;
* - The file is **retained** and not deleted (the same runId is overwritten in place), and the directory also serves as an archive of the actual execution body of each run;
* - This module writes a `.gitignore` (`*`) in the directory, only writes it once in absence, and never touches the project's own `.gitignore`;
* - The project directory cannot be written (read-only checkout, cwd does not exist, `.zcode` is an ordinary file...) → fall back to
* `os.tmpdir()/zcode-workflow-runs/` and after `onWarning` is reported, run starts as usual. If the pullback fails, throw it away.
* Settled by harness normalized to failed.
 */

import { mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** The non-fatal condition reported by harness to the caller; bootstrap completes the warn log. */
export interface HarnessWarning {
  kind: "entry_file_fallback";
/** The project directory you want to write to (`<cwd>/.zcode/workflow-runs`). */
  projectDir: string;
/** The actual directory to fall back to. */
  fallbackDir: string;
/** Error text for failure to write to the project directory. */
  error: string;
}

export interface WriteChildEntryFileInput {
  cwd: string;
  runId: string;
/** Rendered entry file source code. */
  source: string;
  onWarning?: (warning: HarnessWarning) => void;
}

export interface ChildEntryFile {
  path: string;
  location: "project" | "tmpdir";
}

/** Entry file directory within the project. */
export function workflowRunsDir(cwd: string): string {
  return join(cwd, ".zcode", "workflow-runs");
}

/** Fallback directory (under the OS temporary directory, shared across projects). */
export function fallbackWorkflowRunsDir(): string {
  return join(tmpdir(), "zcode-workflow-runs");
}

/** RunId shall be replaced with `_` outside the safe character set to prevent path separators and the like from being mixed into the file name. */
export function childEntryFileName(runId: string): string {
  return `${runId.replace(/[^A-Za-z0-9._-]/g, "_")}.mjs`;
}

export function writeChildEntryFile(input: WriteChildEntryFileInput): ChildEntryFile {
  const projectDir = workflowRunsDir(input.cwd);
  const fileName = childEntryFileName(input.runId);
  try {
    return { path: writeInto(projectDir, fileName, input.source), location: "project" };
  } catch (error) {
    const fallbackDir = fallbackWorkflowRunsDir();
    input.onWarning?.({
      kind: "entry_file_fallback",
      projectDir,
      fallbackDir,
      error: error instanceof Error ? error.message : String(error),
    });
    return { path: writeInto(fallbackDir, fileName, input.source), location: "tmpdir" };
  }
}

function writeInto(dir: string, fileName: string, source: string): string {
  mkdirSync(dir, { recursive: true });
  try {
// `wx`: Only created in absence. If the user changes this .gitignore, it will not be touched again.
    writeFileSync(join(dir, ".gitignore"), "*\n", { encoding: "utf8", flag: "wx" });
  } catch (error) {
    if ((error as { code?: string }).code !== "EEXIST") throw error;
  }
  const path = join(dir, fileName);
  writeFileSync(path, source, "utf8");
  return path;
}
