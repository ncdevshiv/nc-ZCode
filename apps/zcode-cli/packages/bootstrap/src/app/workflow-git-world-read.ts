// ============================================================
// git.* world reads: read-only **by construction**
// ============================================================
// This module is the **pure** half of git world-read: argument verification, fixed argv construction, and porcelain-v2 /
// log / parsing of path list output. Side effects (really spawning out a git) remain in workflow-world-read.ts.
//
// Why split it into a pure module: Each item here is a property that can be pinned separately - "The argv of this op happens to be these
// element", "branch is absent when detached HEAD", "`-foo` is rejected as base", "filenames with newlines are passed through unchanged".
// If they are placed on the execution side, they can only be asserted indirectly through a fake port; if they are placed here, they can be asserted directly.
//
// ————————————————————————————————————————————————————————————————
// Read-only is **constructed**, not checked.
// ————————————————————————————————————————————————————————————————
// This module can only construct the argv of five allowed subcommands. There is no path that spells out the shell string, nor is there a
// Use the string given by the script directly as a subcommand or option: base ref passes a strict character set (the first character `-` is prohibited, and the `..` interval is prohibited),
// path must be relative to the workspace and not out of bounds, and **always** comes after `--`. So "can't write it in" is what this code can express
// The nature of the thing, not a permission check that may someday be forgotten.
//
// ————————————————————————————————————————————————————————————————
// Two output contracts that run through the entire module (both are measured in practice, not inferred from documents)
// ————————————————————————————————————————————————————————————————
// **1. `-z`: NUL separated, the path is byte-by-byte. ** When `-z` is not added, `core.quotePath` defaults to true, and git will
// Non-ASCII path C references plus octal escaping - `document.md` becomes a string of `"\346\226\207..."`. This user in this repository
// The body is the norm rather than the corners. `-z` solves two things at once: paths are no longer quote-escaped, and filenames with newlines are no longer
// Destroy splitting by row (cutting by row will split it into two non-existent paths, and no error will be reported - "rare and silent error" is exactly
// Failure patterns not accepted by this code). Our own `git log` has also used `%x00` to separate fields, so the whole family is consistent.
//
// **2. The path is always handed over as "relative to the workspace", and the conversion is done once on our side. ** Actual measurement: `-z` will make
// `status --porcelain=v2` outputs the **repository root relative** path, and no flag or config can change it back to cwd relative
// (`status.relativePaths` is invalid for porcelain -z); `diff --name-only` defaults to relative to the warehouse root; and
// `ls-files --others` defaults to **cwd relative**. Three commands and three benchmarks.
//
// So here is a unified rule: **Online, always take the warehouse root relative** (`ls-files` complement `--full-name` for alignment),
// Use `rev-parse --show-prefix` again to get the prefix of the workspace relative to the repository root, and strip it off during parsing. so
// There is only one "baseline" in one place, rather than three commands each relying on a different git default - that kind of mashup is exactly what
// This will cause `changedFiles()` to spit out the bug of two base paths at the same time (the three are exactly the same when the workspace is the warehouse root.
// So it will remain undiscovered until a workspace is a subdirectory).
//
// The only exception is the **patch text** of `git.diff`: it is not a list of paths we parse, but a paragraph with `a/… b/…`
// The overall output of the header cannot strip the prefix afterwards. That one therefore uses git's own `--relative` to align the bases.

import { WorkflowError } from "@zcode/dynamic-workflow";

/**
* Once the git world reads one command to run. `argv` does not contain `"git"` itself - the executable name is provided by the execution side,
* In this way, the module does not touch the process at all.
 */
interface GitCommandPlan {
  readonly argv: readonly string[];
}

/** The return shape of `git.status()`. The authoritative declaration is `declare interface GitStatus` in FACADE_DTS. */
export interface GitStatusResult {
  branch?: string;
  clean: boolean;
  staged: string[];
  unstaged: string[];
  untracked: string[];
}

/** A record of `git.log()`. The authoritative declaration is `declare interface GitCommit` in FACADE_DTS. */
export interface GitCommitResult {
  hash: string;
  subject: string;
  author: string;
  date: string;
}

/**
* Strict character set for base ref: first character must be alphanumeric, followed by `[A-Za-z0-9._/@^~-]` allowed.
 *
* The first character must not be `-` is the only **security-related** part of this rule: `-foo` will be treated as an option by git instead of a ref,
* And "which options exist" is not decided by us. The first character cannot be `.`, just block `.` / `..` which are neither legal ref,
* It looks like the input of interval.
 *
* Deliberately excluded: whitespace (a legitimate reason for not having whitespace in a ref), `{`/`}` (`@{u}` type of reflog syntax, v1
* not supported), `:` (`ref:path` syntax), and any `..` (range - v1 only accepts a **single** ref). None of these are "dangerous", just beyond the promised semantics of v1; accepting them amounts to
* Make the script rely on a contract that no one wrote down.
 */
const GIT_REF_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._/@^~-]*$/;

/** Field format of `git log`: NUL separated hash / subject / author name / author date (ISO-8601). */
const GIT_LOG_PRETTY = "--pretty=format:%H%x00%s%x00%an%x00%aI";

/**
* The prefix of the workspace relative to the repository root (`"sub/nested/"`, which is an empty string at the repository root). This is the relative path to the warehouse root online
* The only basis for converting to a relative path to the workspace, see output contract 2 at the top of this module.
 */
export const GIT_SHOW_PREFIX_ARGV: readonly string[] = ["rev-parse", "--show-prefix"];

/**
* Fixed argv for `git status`. `--branch` is the source of GitStatus.branch; `--.` closes the observation scope in
* Within the workspace (see {@link WORKSPACE_PATHSPEC}).
 */
export const GIT_STATUS_ARGV: readonly string[] = [
  "status",
  "--porcelain=v2",
  "-z",
  "--branch",
  "--",
  ".",
];

/**
* Fixed argv for untracked files. `--exclude-standard` makes it respect .gitignore; `--full-name` makes it respect .gitignore; `--full-name` makes it respect .gitignore;
* cwd is pulled relative to the repository root, aligned with status / diff (output contract 2 at the top of this module).
 */
const GIT_UNTRACKED_ARGV: readonly string[] = [
  "ls-files",
  "--others",
  "--exclude-standard",
  "-z",
  "--full-name",
  "--",
  ".",
];

/**
* workspace pathspec. git.* is world-reading, and the "world" of this workflow is full of workspaces:
* files.glob/read/grep is workspace-wide. The actor sub-session uses the workspace as cwd, and the permissions are also workspace-wide.
* So git observations are also collected in the same world - otherwise `changedFiles()` would hand over paths outside the workspace, and
* `changedFiles() → files.read(p)` This most obvious two-line combination becomes a trap.
 *
* Price (v1 intends to accept): When the workspace is a subdirectory of the warehouse, changes outside the subdirectory cannot be seen.
 */
const WORKSPACE_PATHSPEC = ".";

/**
* Verify a base ref and return the original string. Shape mismatch is a structured `DriverError` (node ​​level, script can `catch`).
* The error message explains **why** it was rejected, because the side that wrote the script needs to adapt it accordingly, rather than guessing.
 */
function validateGitRef(op: string, value: string): string {
  if (value.length === 0) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: base must not be empty. Pass a ref such as "main" or omit it.`,
    );
  }
  if (value.startsWith("-")) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: base '${value}' starts with '-', which git would read as an option, ` +
        `not a ref. Pass a plain ref.`,
    );
  }
  if (value.includes("..")) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: base '${value}' is a '..' range; only a single ref is accepted. Pass ` +
        `one ref.`,
    );
  }
  if (!GIT_REF_PATTERN.test(value)) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: base '${value}' is not a valid ref. A ref starts with a letter or ` +
        `digit, followed by letters, digits and . _ / @ ^ ~ - (no whitespace or syntax ` +
        `characters such as {} :).`,
    );
  }
  return value;
}

/**
* Verify a workspace relative path and return a normalized (separator is unified into `/`) string.
 *
* The three items are all **lexical level closure** (do not follow symbolic links, see v1 instructions): no absolute paths, no starting with `-`,
* The `..` paragraph is not allowed. The third item is the out-of-bounds check on this side: in the normalized result of `relative`, `..` can only appear in
*, so "without a `..` segment and not an absolute path" is equivalent to "still in the workspace after parsing".
 *
* Starting with `-` is already safe even after `--`, but it is still rejected: this is defense in depth - once `--` is missed one day,
* A `-foo` path is an option.
 */
function validateGitPath(op: string, value: string): string {
  if (value.length === 0) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: path must not be empty. Pass a workspace-relative path or omit it.`,
    );
  }
  if (value.startsWith("-")) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: path '${value}' starts with '-', which git could read as an option. ` +
        `Pass a path that does not start with '-'.`,
    );
  }
  if (looksAbsolute(value)) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: path '${value}' is absolute; only workspace-relative paths are ` +
        `accepted. Pass the path relative to the workspace root.`,
    );
  }
// The delimiters are unified into `/`: git's pathspec eats `/` on all three platforms, and the model is written in windows style
// Relative paths are commonplace. Normalization must be done before `..` is checked, otherwise `..\x` will slip through the segmentation.
  const normalized = value.replace(/\\/g, "/");
  if (normalized.split("/").includes("..")) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: path '${value}' contains a '..' segment and would leave the ` +
        `workspace. Pass a path inside the workspace.`,
    );
  }
  return normalized;
}

/**
* Absolute path determination, ** does not depend on the current platform **: the posix version of `node:path` does not recognize `C:\x`, and this verification must be in
* Give the same answer on three platforms (cross-platform principle: not implemented according to the system behavior of the development machine).
 */
function looksAbsolute(value: string): boolean {
  return value.startsWith("/") || value.startsWith("\\") || /^[A-Za-z]:[\\/]?/.test(value);
}

/**
* `git.changedFiles(base?)` The command to run.
 *
* Without base, it is the union of **two** commands: `diff --name-only HEAD` plus `ls-files --others
* --exclude-standard`. Untracked files appear to the reader as changes, and missing them would cause this primitive to fail in a newly opened
* Useless on the feature branch (almost all new files were not tracked yet).
 *
* When base is given, there is only one: `diff --name-only <base>`. "What has been changed relative to a certain ref" is a question about
* **Tracked history** problem - a file that has never been entered into git cannot be compared with any ref.
 */
export function gitChangedFilesPlan(base: string | undefined): GitCommandPlan[] {
  const ref = base === undefined ? "HEAD" : validateGitRef("git-changed-files", base);
  const tracked: GitCommandPlan = {
    argv: ["diff", "--name-only", "-z", ref, "--", WORKSPACE_PATHSPEC],
  };
  if (base !== undefined) return [tracked];
  return [tracked, { argv: [...GIT_UNTRACKED_ARGV] }];
}

/**
* argv of `git.diff(base?, path?)`. base defaults to `HEAD`; path defaults to workspace pathspec `.`,
* and **always** come after `--`, so that a path with the same name as a branch will not be interpreted as a ref by git (nor vice versa).
 *
* `--relative` is required in this article for a different reason than elsewhere: the `a/… b/…` header in the patch text is written by git itself,
* We do not parse and cannot strip the prefix afterwards, so we can only let git output based on cwd (= workspace). Without it,
* `changedFiles()` says "only a.md changed" and `diff()` hands over a repository-level patch with `../x` - two primitives
* Will give different answers to "what counts as a change".
 *
* `--relative` itself will also exclude changes outside of cwd, so `-- .` is redundant when there is no path. **Redundancy is intentional**:
* Range closing should not quietly rely on the side effects of another flag. It must be written so that it can be nailed by the argv test.
 */
export function gitDiffArgv(base: string | undefined, path: string | undefined): string[] {
  const ref = base === undefined ? "HEAD" : validateGitRef("git-diff", base);
  const pathspec = path === undefined ? WORKSPACE_PATHSPEC : validateGitPath("git-diff", path);
  return ["diff", "--relative", ref, "--", pathspec];
}

/**
* argv of `git.log(count)`. count is checked by the caller and sandwiched within [1, cap].
 *
* ** Deliberately without pathspec, it is asymmetrical with other ops in this family. ** commit is a warehouse-level object; add one to log
* pathspec will change its semantics to "those commits that have touched the workspace", that is another problem, no one has clicked it. log reads
* Historical metadata (hash/subject/author/date) is not a path in the workspace, so the workspace closure has no effect on it
*Applicable objects.
 */
export function gitLogArgv(count: number): string[] {
  return ["log", `-n${count}`, GIT_LOG_PRETTY];
}

/** Output of `rev-parse --show-prefix` → prefix string (empty string when repository root). */
export function parseGitShowPrefix(stdout: string): string {
  return stdout.replace(/\r?\n$/, "");
}

/**
* Cut `-z` output into segments: cut by NUL, discarding empty segments (the NUL terminator at the end leaves an empty segment).
 *
* Dropping empty segments is safe and has no impact on the dual-segment consumption of the rename item: the path is never an empty string, so empty segments can only come from
* The terminator itself.
 */
function nulSegments(stdout: string): string[] {
  return stdout.split("\u0000").filter((segment) => segment.length > 0);
}

/**
* Strip off the workspace prefix and convert repository root relative paths to **workspace relative** paths (output contract 2 at the top of this module).
 *
* Paths outside the prefix **fail loud** instead of being handed over as-is. `-- .` has ensured that each path is within the work area, so
* Coming here means that a certain assumption has collapsed; at this time, handing a path with `../` to the script is exactly what this set of wrap-ups should prevent.
* and the script will take it to `files.read`. Rather let the node fail.
 */
function stripWorkspacePrefix(prefix: string, path: string): string {
  if (prefix.length === 0) return path;
  if (path.startsWith(prefix)) return path.slice(prefix.length);
  throw new WorkflowError(
    "DriverError",
    `git reported path '${path}' outside the workspace prefix '${prefix}'. The pathspec should ` +
      `have scoped the read to the workspace, so the scoping is broken; report this as a bug.`,
  );
}

/**
* Parse `git status --porcelain=v2 -z --branch -- .`.
 *
* The first character of the segment is the record type: `#` header information, `1` ordinary changes, `2` rename/copy, `u` not merged, `?` not tracked,
* `!` is ignored (only appears under `--ignored`, not here). The two `XY` of `1`/`2` are **temporary storage area** and
* **Workspace** status, `.` means unchanged - so a file can be staged and unstaged (changed, temporarily saved,
* and continued to change).
 *
* Two unique shapes of `-z` (both measured):
* - Header lines are also NUL terminated, not newline.
* - **rename/copy item occupies two paragraphs**: `2 ... R100 <new path>\0<original path>` (the two are separated by TAB without `-z`).
* Therefore, the parsing must explicitly consume one more section, otherwise the original path will be regarded as the next record - and it does not start with a type character,
* will be silently discarded. The original path itself is not handed over: the facade reports "which paths have changed now".
 *
* The path is the **tail segment** of the record rather than a fixed-length field: the path can contain spaces (or even line breaks, and is retained as is under `-z`), so press
* "Skip the first N space-delimited fields and take the remaining text" to cut, never split the entire paragraph.
 */
export function parseGitStatusPorcelainV2(stdout: string, prefix = ""): GitStatusResult {
  const staged: string[] = [];
  const unstaged: string[] = [];
  const untracked: string[] = [];
  let branch: string | undefined;

  const segments = nulSegments(stdout);
  for (let index = 0; index < segments.length; index += 1) {
    const segment = segments[index]!;
    if (segment.startsWith("# branch.head ")) {
      const head = segment.slice("# branch.head ".length);
// When detached HEAD, git writes the literal "(detached)". At this point branch is **absent** instead of carrying this
// Placeholder string - hand it over as the branch name, and the script will use it to do diff.
      if (head !== "(detached)") branch = head;
      continue;
    }
    if (segment.startsWith("#")) continue;

    const kind = segment[0];
    if (kind === "1" || kind === "2") {
      const xy = segment.split(" ")[1] ?? "..";
// The path of `1` starts from the 8th field; `2` has one more `<X><score>` field, so it starts from the 9th field, and its **original path**
// Occupies the entire paragraph immediately following it (the delimiter under `-z` is NUL) and must be explicitly skipped.
      const path = fieldTail(segment, kind === "1" ? 8 : 9);
      if (kind === "2") index += 1;
      if (path.length === 0) continue;
      const relative = stripWorkspacePrefix(prefix, path);
      if (xy[0] !== undefined && xy[0] !== ".") staged.push(relative);
      if (xy[1] !== undefined && xy[1] !== ".") unstaged.push(relative);
      continue;
    }
    if (kind === "u") {
// Unmerged (conflict): Count the workspace to be processed and fall into unstaged. It does "have something to do", and staged is misleading -
// A conflicting file is not "ready to commit".
      const path = fieldTail(segment, 10);
      if (path.length > 0) unstaged.push(stripWorkspacePrefix(prefix, path));
      continue;
    }
    if (kind === "?") {
      const path = fieldTail(segment, 1);
      if (path.length > 0) untracked.push(stripWorkspacePrefix(prefix, path));
    }
// `!` and unrecognized segments: skip.
  }

  const clean = staged.length === 0 && unstaged.length === 0 && untracked.length === 0;
  return {
    ...(branch === undefined ? {} : { branch }),
    clean,
    staged: sortedUnique(staged),
    unstaged: sortedUnique(unstaged),
    untracked: sortedUnique(untracked),
  };
}

/**
* Parse the output of {@link GIT_LOG_PRETTY}: one record per line, fields separated by NUL.
 *
* This record cut by **line break** is reliable, unlike the case of the path list: `%s` is the first line of the commit message,
* does not contain newlines by definition, and the remaining three fields (hash / author name / ISO date) do not contain newlines either. So here
* `-z` is not required.
 *
* If the number of fields is not equal to 4, throw away: both the format string and the parser are in this module. Inconsistency between the two may only be caused by local changes.
* And a half-filled commit will flow all the way into the journal and model context.
 */
export function parseGitLog(stdout: string): GitCommitResult[] {
  const commits: GitCommitResult[] = [];
  for (const raw of stdout.split("\n")) {
    const record = raw.endsWith("\r") ? raw.slice(0, -1) : raw;
    if (record.length === 0) continue;
    const fields = record.split("\u0000");
    if (fields.length !== 4) {
      throw new WorkflowError(
        "DriverError",
        `world-read git-log: cannot parse the output; a record should have 4 NUL-separated ` +
          `fields, got ${fields.length}. Retry, and report this as a bug if it persists.`,
      );
    }
    commits.push({
      hash: fields[0]!,
      subject: fields[1]!,
      author: fields[2]!,
      date: fields[3]!,
    });
  }
  return commits;
}

/**
* Split the output of `--name-only -z` / `ls-files -z` into a list of **workspace relative** paths.
* Both have been pulled by argv to be relative to the warehouse root (`--full-name`), so the prefix is ​​stripped once here.
 */
export function parseGitPathList(stdout: string, prefix = ""): string[] {
  return nulSegments(stdout).map((path) => stripWorkspacePrefix(prefix, path));
}

/**
* The entire rest of the text starting from the `index` space-delimited field. The fields of porcelain v2 are separated by single spaces and have a fixed length.
* Only the path is at the end and can contain spaces, so "skip N spaces and then take the remainder" is the only correct way to cut here.
 */
function fieldTail(line: string, index: number): string {
  let at = 0;
  for (let i = 0; i < index; i += 1) {
    const next = line.indexOf(" ", at);
    if (next === -1) return "";
    at = next + 1;
  }
  return line.slice(at);
}

/** Remove duplicates and sort. The journal stores this value, so it must only rely on the content and not on the output order of git. */
function sortedUnique(values: readonly string[]): string[] {
  return [...new Set(values)].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}
