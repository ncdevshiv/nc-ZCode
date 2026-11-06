// ============================================================
//Execution side of world reading (executeWorldRead of Boundary B)
// ============================================================
// Half split from workflow-driver.ts. On the driver side are actor sessions, turn orchestration and submit bridging;
// This is "turning an (op, args) into a real read-only observation" - the only contact between the two is
// {@link executeWorldRead}, and this half does not touch the session, model and journal at all.
//
//Three things are here, and only here:
// 1. **Check the element number and actual parameters of each op**. Boundary A's `worldRead(siteId, op, args)` only promises
// "The actual parameters are served as they are by position" (lowering does not look at the op), so "what does a pattern / a base ref look like"
// Return to this side.
// 2. **Execution of upper limit**. The constant is in the pure package (WORLD_READ_CAPS of `@zcode/dynamic-workflow`), implemented here,
// Because only this side can "not produce" - letting ripgrep stop at 2000 items is better than materializing a million items and then going back.
// 3. **fixed argv** for git. Constructed in workflow-git-world-read.ts (pure), spawned here.

import {
  isAbsolute as isAbsolutePath,
  relative as relativePath,
  resolve as resolvePath,
  sep,
} from "node:path";
import type { ExecutionPort, FileSystemPort } from "@zcode/contracts";
import { WORLD_READ_CAPS, WorkflowError, type WorldReadOp } from "@zcode/dynamic-workflow";
import {
  GIT_SHOW_PREFIX_ARGV,
  GIT_STATUS_ARGV,
  gitChangedFilesPlan,
  gitDiffArgv,
  gitLogArgv,
  parseGitLog,
  parseGitPathList,
  parseGitShowPrefix,
  parseGitStatusPorcelainV2,
  type GitCommitResult,
  type GitStatusResult,
} from "./workflow-git-world-read.js";

/** Ports and base directories required for world reading (a subset of driver deps). */
export interface WorldReadDeps {
/** The file system port where files.glob / files.read / files.grep falls. */
  readonly fileSystemPort: FileSystemPort;
/** The child process execution port where git.* falls (cwd = workspace root). */
  readonly executionPort: ExecutionPort;
/** The base directory (workspace root) for path resolution and relativization. */
  readonly cwd: string;
  /**
* The approved command set of world.run (the one collected from the literal cmd during compilation and displayed in the confirmation window). **Absent means rejecting everything world.run** (fail-closed):
* Empty sets on the authorization side must be as safe as "forgot to wire", and revalidation here is just defense in depth - real authorization
* Occurs at compile time literal + commit confirmation.
   */
  readonly declaredRunCommands?: ReadonlySet<string>;
}

/**
* Press op to get the parameters from the **position actual parameter group** and perform a world read. Boundary A only promises that "the actual parameters will be delivered as is in their position"
* (lowering does not look at op), so the arity and type check of each op are here - this side only knows
* The "what does a pattern / a base ref look like" side.
* If the shape does not match, always use structured `DriverError` to reject the node and never silently force it: one is `String(undefined)`
* A path that quietly becomes `"undefined"` will read an incomprehensible failure instead of an error that can be corrected.
 *
* The declaration as `async` is intentional: the error thrown by the argument validation must become **the node's promise rejection**. Synchronous errors will be thrown from
* `engine.worldRead` goes out (the engine `.then(...)` after entering the journal), leaving a permanent
* running node.
 */
export async function executeWorldRead(
  deps: WorldReadDeps,
  op: WorldReadOp,
  args: unknown[],
): Promise<unknown> {
  switch (op) {
    case "glob":
      return await worldGlob(deps, worldReadStringArgs(op, args, ["pattern"])[0]!);
    case "read":
      return await worldRead(deps, worldReadStringArgs(op, args, ["path"])[0]!);
    case "grep": {
      const [pattern, glob] = worldReadOptionalStringArgs(op, args, ["pattern"], ["glob"]);
      return await worldGrep(deps, pattern!, glob);
    }
    case "git-changed-files": {
      const [base] = worldReadOptionalStringArgs(op, args, [], ["base"]);
      return await gitChangedFiles(deps, base);
    }
    case "git-diff": {
      const [base, path] = worldReadOptionalStringArgs(op, args, [], ["base", "path"]);
      return await gitDiff(deps, base, path);
    }
    case "git-status": {
// No parameter op: `names` is empty, which means "exactly 0 actual parameters". Passing one more will still fail loudly.
      worldReadStringArgs(op, args, []);
      return await gitStatus(deps);
    }
    case "git-log":
      return await gitLog(deps, worldReadOptionalCount(op, args, "count"));
    case "run":
      return await worldRun(deps, args);
    default: {
// The op vocabulary is derived from the world-read registry: if a new line is added and it is not connected here, it will fail loudly here.
      const unknownOp: never = op;
      throw new WorkflowError("DriverError", `Unsupported world-read op "${String(unknownOp)}".`);
    }
  }
}

/**
* `files.glob(pattern)`: normalized to the shape promised by facade via FileSystemPort's searchFiles.
 *
* The three-step unification all comes from the gap between port semantics and facade commitment - the port results cannot be handed over as they are.
* Ports designed for the UI's Glob tool: absolute path, mtime descending, default 100 truncation.
 *
* - **cap+1 Deny** (same idiom as worldGrep): Default truncation of ports takes effect silently, 5000 files at a time
* The glob is only handed over 2%, and the error view will be entered into the journal and resumed for replay. cap belongs to WORLD_READ_CAPS
* All, exactly cap bars cannot distinguish between "exactly" and "truncated", so one more is required.
* - **Workspace relativization**: Absolute paths allow scripts to be written with a workspace relative prefix (`startsWith("apps/…")`)
* All failed, the 6-way fan-out silently collapsed into 1-way; grep/git has long been relativized, and glob is the one that slipped through the net.
* - **lexicographic rearrangement**: mtime drifts with each write to the file, and the journaled value must be determined - the same script twice
* Commits should not be fanned out in a different order. Rejection semantics guarantees that the hand must be fully matched at this point, and the rearrangement is complete.
 */
async function worldGlob(deps: WorldReadDeps, pattern: string): Promise<string[]> {
  const cap = WORLD_READ_CAPS.globMaxFiles;
  const result = await deps.fileSystemPort.searchFiles({
    path: deps.cwd,
    pattern,
    maxResults: cap + 1,
  });
  if (result.files.length > cap || result.truncated) {
    throw capExceeded(`files.glob: over ${cap} files match (the cap). Narrow the pattern.`);
  }
  return result.files.map((path) => toWorkspaceRelative(deps.cwd, path)).sort();
}

/**
* `files.read(path)`: After parsing the absolute path, **confirm that it is still within the workspace**, and then hand it to the port.
 *
* Why is this check added now: before `git.*` was implemented, the paths that could be entered here were literals written by the script itself.
* A boundary crossing is an explicit boundary crossing. `git.*` is the first primitive that can produce ** paths - `changedFiles() → files.read(p)`
* is the most obvious two-line combination, and git's native output will be prefixed with `../` when the workspace is a subdirectory of the warehouse. git side
* We have used pathspec to close the scope in the work area. This check is the landing point of the same invariant at the other end: a primitive
* Obviously the combo shouldn't be a trap.
 */
async function worldRead(deps: WorldReadDeps, arg: string): Promise<string> {
  const path = assertWithinWorkspace("read", deps.cwd, arg);
  const result = await deps.fileSystemPort.readTextFile({ path });
  return result.content;
}

/**
* `files.grep(pattern, glob?)`: via FileSystemPort's searchText (ripgrep semantics).
 *
* **headLimit takes cap + 1**, which is the only unclear line of this method. The upper limit is "reject if the hit exceeds the cap",
* And the port will only be truncated according to headLimit - so if you just take cap, you will get an ambiguous result: if the cap bar hits, it may be either
* Exactly the cap bar (should be allowed), or maybe the next million bars have been cut off (should be rejected). Take one more and take this one
* The ambiguity is eliminated: if you get the cap+1 record, you will know the overflow, but the cost is still only one record, and there is no need to materialize the entire result set.
* `truncated` is the second line of the same judgment (if the port is truncated for other reasons, we will not have the complete result).
 */
async function worldGrep(
  deps: WorldReadDeps,
  pattern: string,
  glob: string | undefined,
): Promise<GrepMatch[]> {
  const cap = WORLD_READ_CAPS.grepMaxMatches;
  const result = await deps.fileSystemPort.searchText({
    path: deps.cwd,
    pattern,
    ...(glob === undefined ? {} : { glob }),
    outputMode: "content",
    showLineNumbers: true,
    headLimit: cap + 1,
  });
  if (result.entries.length > cap || result.truncated) {
    throw capExceeded(
      `files.grep: over ${cap} matches (the cap). Narrow the pattern or add a glob.`,
    );
  }
  const matches: GrepMatch[] = [];
  for (const entry of result.entries) {
// In content mode, each hit has a line number and line text; an entry missing either is not a content hit (for example, the port is in
// Count items produced under other outputMode), skip instead of filling in 0 / "" creates a false hit.
    if (entry.lineNumber === undefined || entry.text === undefined) continue;
    matches.push({
      path: toWorkspaceRelative(deps.cwd, entry.path),
      line: entry.lineNumber,
      text: entry.text,
    });
  }
  const serializedBytes = Buffer.byteLength(JSON.stringify(matches), "utf8");
  if (serializedBytes > WORLD_READ_CAPS.grepMaxSerializedBytes) {
    throw capExceeded(
      `files.grep: result is ${serializedBytes} bytes, over the ` +
        `${WORLD_READ_CAPS.grepMaxSerializedBytes}-byte cap. Narrow the pattern or add a glob.`,
    );
  }
  return matches;
}

// ——————————————————————————————— Internal: git.* world-read ——————————————————————————————

/**
* Run a git command and return stdout. argv **constructed** by workflow-git-world-read.ts (fixed array,
* Never shell string), this method is only responsible for handing over to the port and normalizing failures to node-level `DriverError`.
 *
* `maxInlineBytes` is given by the caller: git.diff to use it for upper limit detection (same as grep's cap+1 method),
* Use a large enough default value for the remaining ops.
 */
async function runGit(
  deps: WorldReadDeps,
  op: WorldReadOp,
  argv: readonly string[],
  maxInlineBytes: number,
): Promise<{ text: string; bytes: number; truncated: boolean }> {
  const result = await deps.executionPort.run({
    command: { mode: "argv", file: "git", args: [...argv] },
    cwd: deps.cwd,
    outputLimit: { maxInlineBytes },
  });
  if (result.status !== "completed" || (result.exitCode ?? 0) !== 0) {
// Missing git, non-warehouse, and bad refs all go here. Normalized into a **catchable** DriverError instead of run level
// Fail, allowing the script to use files.glob via try/catch instead.
    const detail = firstLine(result.stderr.text) || firstLine(result.stdout.text) || result.status;
    throw new WorkflowError(
      "DriverError",
      `git ${argv.join(" ")} failed (${result.status}, exit=${result.exitCode ?? "n/a"}): ${detail}`,
    );
  }
  return {
    text: result.stdout.text,
    bytes: result.stdout.bytes,
    truncated: result.stdout.truncated,
  };
}

/**
* The prefix of the workspace relative to the warehouse root (empty string for the warehouse root). The paths on the line are always relative to the warehouse root. Strip off this prefix.
* is the relative path to the workspace promised by the facade (see output contract 2 at the top of workflow-git-world-read.ts).
 *
* Ask once for each read without caching: a `rev-parse` is milliseconds, while world reads are site×ordinal
* Only happens once and drops the journal, not on any hot path. The result is that this side is completely stateless.
 */
async function gitWorkspacePrefix(deps: WorldReadDeps, op: WorldReadOp): Promise<string> {
  const out = await runGit(deps, op, GIT_SHOW_PREFIX_ARGV, GIT_TEXT_OUTPUT_BYTES);
  return parseGitShowPrefix(out.text);
}

async function gitChangedFiles(deps: WorldReadDeps, base: string | undefined): Promise<string[]> {
// Construct argv first: if base is illegal, it should be rejected before running any git.
  const plans = gitChangedFilesPlan(base);
  const prefix = await gitWorkspacePrefix(deps, "git-changed-files");
  const paths: string[] = [];
  for (const plan of plans) {
    const out = await runGit(deps, "git-changed-files", plan.argv, GIT_TEXT_OUTPUT_BYTES);
    paths.push(...parseGitPathList(out.text, prefix));
  }
// Union deduplication and sorting: two commands can report the same path, and the journal stores this value, so it must only
// Whichever of the two commands returns first, depending on the content or not.
  return [...new Set(paths)].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

async function gitDiff(
  deps: WorldReadDeps,
  base: string | undefined,
  path: string | undefined,
): Promise<string> {
  const cap = WORLD_READ_CAPS.gitDiffMaxBytes;
// cap + 1: Same reason as grep - "exactly cap bytes" and "truncated" must be distinguishable.
  const out = await runGit(deps, "git-diff", gitDiffArgv(base, path), cap + 1);
  if (out.bytes > cap || out.truncated) {
    throw capExceeded(
      `git.diff: output is ${out.bytes} bytes, over the ${cap}-byte cap. Pass a path to narrow.`,
    );
  }
  return out.text;
}

async function gitStatus(deps: WorldReadDeps): Promise<GitStatusResult> {
  const prefix = await gitWorkspacePrefix(deps, "git-status");
  const out = await runGit(deps, "git-status", GIT_STATUS_ARGV, GIT_TEXT_OUTPUT_BYTES);
  return parseGitStatusPorcelainV2(out.text, prefix);
}

async function gitLog(deps: WorldReadDeps, count: number): Promise<GitCommitResult[]> {
  const out = await runGit(deps, "git-log", gitLogArgv(count), GIT_TEXT_OUTPUT_BYTES);
  return parseGitLog(out.text);
}

// ————————————————————————————————— Internal: world.run ——————————————————————————————

/** The return value of `world.run`. The authoritative declaration is `declare interface WorldRunResult` in FACADE_DTS. */
interface WorldRunResult {
  exitCode: number;
  stdout: string;
  stderr: string;
}

/**
* `world.run(cmd, args?, opts?)`: journalized command execution.
 *
* Three deliberate differences from `git.*`, each a contract rather than an oversight:
* 1. **Non-zero exit is a value**. The execution adapter maps non-zero exits to `status:"failed"` and `error` is absent
* (statusFromExit/statusFailure of node-execution-adapter-results.ts), accordingly with
* Reliable distinction between spawn failure/timeout. The normal path of a gated loop should not take abnormal control flow.
* 2. **cmd retest**. Authorization occurs at compile time literal + commit confirmation; here for declaredRunCommands
* The comparison only prevents wiring errors (the lowering / line protocol sends other strings in), and is fail-closed——
* Collection absence is rejected in the same way as the command not being in the collection.
* 3. **No upper limit for timeout**. The default is 300s, and the script can be increased arbitrarily (designed for real long-distance running tests);
* cancel is still the last control.
 *
* stdout/stderr are 256KB each, cap+1 detection (`maxInlineBytes` is based on the larger of the two caps +1, and then judged by stream),
* Over-limit rejection without truncation - message gives actionable next step, same house policy as grep/diff.
 */
async function worldRun(deps: WorldReadDeps, args: unknown[]): Promise<WorldRunResult> {
  const { argv, cmd, timeoutMs } = worldRunArgs(args);

  const declared = deps.declaredRunCommands;
  if (declared === undefined || !declared.has(cmd)) {
// Fail-closed wiring defense: The cmd that can reach here should be collected and confirmed by compile-time literals.
    throw new WorkflowError(
      "DriverError",
      `world.run: command '${cmd}' is not in the declared set of commands (a wiring error).`,
    );
  }

  const stdoutCap = WORLD_READ_CAPS.runStdoutMaxBytes;
  const stderrCap = WORLD_READ_CAPS.runStderrMaxBytes;
  const result = await deps.executionPort.run({
    command: { mode: "argv", file: cmd, args: argv },
    cwd: deps.cwd,
    timeoutMs,
    outputLimit: { maxInlineBytes: Math.max(stdoutCap, stderrCap) + 1 },
  });

  if (result.status === "timed_out") {
    throw new WorkflowError(
      "DriverError",
      `world.run '${cmd}' timed out after ${timeoutMs}ms. Raise opts.timeoutMs or narrow the work.`,
    );
  }
// Completed (exit 0) and failed-none-error (non-zero exit) are both **finished observations** and hand over the values;
// The rest (spawn_error / canceled / failed with error, such as output_limit) is that the observation itself is not established.
  const ranToExit =
    result.status === "completed" ||
    (result.status === "failed" &&
      result.error === undefined &&
      typeof result.exitCode === "number");
  if (!ranToExit) {
    const detail = result.error?.message ?? (firstLine(result.stderr.text) || result.status);
    throw new WorkflowError(
      "DriverError",
      `world.run '${cmd}' did not run to completion (${result.status}): ${detail}`,
    );
  }

  if (result.stdout.bytes > stdoutCap || result.stdout.truncated) {
    throw capExceeded(
      `world.run '${cmd}': stdout is ${result.stdout.bytes} bytes, over the ${stdoutCap}-byte cap. ` +
        `Quiet the output (e.g. --quiet), or write a file and files.read a summary.`,
    );
  }
  if (result.stderr.bytes > stderrCap || result.stderr.truncated) {
    throw capExceeded(
      `world.run '${cmd}': stderr is ${result.stderr.bytes} bytes, over the ${stderrCap}-byte cap. ` +
        `Reduce the diagnostics, or write a file and files.read a summary.`,
    );
  }

  return {
    exitCode: result.exitCode ?? 0,
    stdout: result.stdout.text,
    stderr: result.stderr.text,
  };
}

/**
* Argument shape of `world.run`: `[cmd: string, args?: string[], opts?: { timeoutMs?: number }]`.
* Same discipline as {@link worldReadStringArgs} (fail loudly, never force), but shape (array + option bag)
* Beyond the expression surface of the string sequence helper, so single column.
 */
function worldRunArgs(args: unknown[]): { cmd: string; argv: string[]; timeoutMs: number } {
  if (args.length < 1 || args.length > 3) {
    throw new WorkflowError(
      "DriverError",
      `world.run takes 1 to 3 arguments (cmd, args?, opts?), got ${args.length}.`,
    );
  }
  const cmd = requireStringArg("run", args[0], "cmd", 0);

  const rawArgv = args[1];
  let argv: string[] = [];
  if (rawArgv !== undefined) {
    if (!Array.isArray(rawArgv)) {
      throw new WorkflowError(
        "DriverError",
        `world.run: argument 2 (args) must be an array of strings, got ${describeArg(rawArgv)}.`,
      );
    }
    argv = rawArgv.map((item, index) => {
      if (typeof item !== "string") {
        throw new WorkflowError(
          "DriverError",
          `world.run: args[${index}] must be a string, got ${describeArg(item)}. Stringify values.`,
        );
      }
      return item;
    });
  }

  const rawOpts = args[2];
  let timeoutMs: number = WORLD_READ_CAPS.runDefaultTimeoutMs;
  if (rawOpts !== undefined) {
    if (typeof rawOpts !== "object" || rawOpts === null || Array.isArray(rawOpts)) {
      throw new WorkflowError(
        "DriverError",
        `world.run: arg 3 (opts) must be an options object or omitted, got ${describeArg(rawOpts)}.`,
      );
    }
    const rawTimeout = (rawOpts as { timeoutMs?: unknown }).timeoutMs;
    if (rawTimeout !== undefined) {
      if (typeof rawTimeout !== "number" || !Number.isInteger(rawTimeout) || rawTimeout < 1) {
        throw new WorkflowError(
          "DriverError",
          `world.run: opts.timeoutMs must be an integer >= 1, got ${describeArg(rawTimeout)}.`,
        );
      }
// Deliberately unlimited clamping: designed for real long-distance running testing; cancel is the final control.
      timeoutMs = rawTimeout;
    }
  }

  return { argv, cmd, timeoutMs };
}

// ——————————————————————————————— Pure support ————————————————————————————

/**
* Take out the **required string actual parameters** described by `names` from the positional actual parameter group of world-read. If the shape does not match, the structure will be thrown.
* `DriverError` (node ​​level failure, script can `catch`). The arity is accurately verified according to `names.length`: multiple passed actual parameters are only possible
* From a wiring error - the facade's type signature blocks redundant arguments at compile time, so it appears as lowering / at runtime
* There is something wrong with the line protocol, and silently ignoring it is equivalent to hiding a localizable bug as a read with ambiguous semantics.
 *
* Why the verification is not forced: Boundary A's `args: unknown[]` is the actual script parameter of **passthrough transmission** as it is, and this side is the first one
* is also the only place where the arity of each op is known.
* `String(args[0])` will turn `undefined` into the path `"undefined"`, and the reported ENOENT will not point to the real error.
 *
* Multi-parameter ops with optional arguments (`files.grep(pattern, glob?)`, `git.diff(base?, path?)`) allow tail
* The default variant {@link worldReadOptionalStringArgs}; the seam is in these two functions, not at the call point.
 */
function worldReadStringArgs(op: WorldReadOp, args: unknown[], names: readonly string[]): string[] {
  if (args.length !== names.length) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op} takes ${names.length} arguments (${names.join(", ")}), got ${args.length}.`,
    );
  }
  return names.map((name, index) => requireStringArg(op, args[index], name, index));
}

/**
* **Tail optional** variant of {@link worldReadStringArgs}: `required` is the required prefix and `optional` is the optional trailer.
* Returns an array of fixed length `required.length + optional.length`, the missing optional bit is `undefined`.
 *
* Three rules, all on the "fail loudly" side:
* 1. The actual parameters are less than the required number or more than the total number → structured `DriverError`. Multipass still rejected, reasons and exact variations
* Same: the signature of the facade blocks redundant arguments during compilation, and the occurrence during runtime can only be caused by lowering/line protocol.
* Wiring errors, silent ignoring is equivalent to hiding a localizable bug as a read with ambiguous semantics.
* 2. The required bit must be string.
* 3. **Optional bit allows explicit `undefined`** because it is writable in scripts: `git.diff(undefined, "a.ts")`
* It is legal under `base?: string`, lowering is packed by position as it is, so the driver will really receive a
* `undefined` prefix. Reading it as "absent" is the only way that makes sense.
 *
* Note that lowering packs the **tail** by default into a **shorter array** (not an undefined hole), and
* `inputHash({op, args})` is the journal key - so `["TODO"]` and `["TODO", "*.ts"]` are naturally two keys.
* This function only normalizes the two arrival shapes into the same calling shape without rewriting args.
 */
function worldReadOptionalStringArgs(
  op: WorldReadOp,
  args: unknown[],
  required: readonly string[],
  optional: readonly string[],
): (string | undefined)[] {
  const max = required.length + optional.length;
  if (args.length < required.length || args.length > max) {
    const range = required.length === max ? `${max}` : `${required.length}~${max}`;
    throw new WorkflowError(
      "DriverError",
      `world-read ${op} takes ${range} arguments ` +
        `(${[...required, ...optional.map((n) => `${n}?`)].join(", ")}), got ${args.length}.`,
    );
  }
  const out: (string | undefined)[] = [];
  required.forEach((name, index) => out.push(requireStringArg(op, args[index], name, index)));
  optional.forEach((name, offset) => {
    const index = required.length + offset;
    if (index >= args.length) {
      out.push(undefined);
      return;
    }
    const value = args[index];
    if (value === undefined) {
      out.push(undefined);
      return;
    }
    out.push(requireStringArg(op, value, name, index));
  });
  return out;
}

/**
* Actual parameters of `git.log(count?)`: 0 or 1, must be a positive integer and not exceed the upper limit. Pick up in absentia
 * {@link WORLD_READ_CAPS.gitLogDefaultCount}。
 *
* Exceed the upper limit **Reject** instead of silently clamping to the upper limit: if the script requests 500 items but gets 100 items, it will become in its own logic
* An incomprehensible question of "Why is history so short?" A named upper bound error allows the script to be corrected directly.
 */
function worldReadOptionalCount(op: WorldReadOp, args: unknown[], name: string): number {
  const max = WORLD_READ_CAPS.gitLogMaxCount;
  if (args.length > 1) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op} takes at most 1 argument (${name}?), got ${args.length}.`,
    );
  }
  const raw = args[0];
  if (raw === undefined) return WORLD_READ_CAPS.gitLogDefaultCount;
  if (typeof raw !== "number" || !Number.isInteger(raw) || raw < 1) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: argument '${name}' must be an integer >= 1, got ${describeArg(raw)}.`,
    );
  }
  if (raw > max) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: argument '${name}'=${raw} is over the cap of ${max}; pass at most ${max}.`,
    );
  }
  return raw;
}

/** Take a positional argument that must be a string, otherwise the structure will be rejected (never `String(...)` coercion). */
function requireStringArg(op: WorldReadOp, value: unknown, name: string, index: number): string {
  if (typeof value !== "string") {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: arg ${index + 1} (${name}) must be a string, got ${describeArg(value)}.`,
    );
  }
  return value;
}

/**
* Structured rejection of upper bound overflow. The message always has an actionable next step ("narrow the pattern or add a glob"):
* The upper limit is that a script can rewrite its own contract, and an error that just says "exceeded" wastes this.
 */
function capExceeded(message: string): WorkflowError {
  return new WorkflowError("WorldReadCapExceeded", message);
}

/**
* Parse the relative path given by a script into an absolute path, and **lexically** determine whether it is still within the workspace; return out of bounds
* `undefined` (wrong wording and structured code returns to the caller - World Reader `DriverError`, product release report
 * `ArtifactPathOutsideWorkspace`）。
 *
* The criterion is `relative(cwd, resolved)`: its output has been normalized, `..` can only appear at the beginning, so
* "Does not start with a `..` segment and is not an absolute path" is equivalent to "still in the workspace". The second condition is required on windows
* ——When spanning drive letters, `relative` will return an absolute path instead of a string of `..`.
 *
* **Only perform lexical checking, do not follow symbolic links**: A soft link pointing to the outside in the workspace still passes at this layer. world read
* Accept this known boundary (the bytes read are the values ​​seen by the script itself); the production release does not accept it because it converts the bytes
* **Copy it into a persistent store and give it to the user**, so that side does another realpath review on top of this function.
* (see workflow-artifact-publish.ts).
 *
* **Export is intentional**: The out-of-bounds definition must have only one implementation. Write it once in both places, and one day two places will be right about the same thing.
* The path gives different answers, and one of them is a safe boundary.
 */
export function resolveWithinWorkspace(cwd: string, arg: string): string | undefined {
  const resolved = resolvePath(cwd, arg);
  const rel = relativePath(cwd, resolved);
  const escapes = rel === ".." || rel.startsWith(`..${sep}`) || rel.startsWith("../");
  return escapes || isAbsolutePath(rel) ? undefined : resolved;
}

/** World read wrapper for {@link resolveWithinWorkspace}: out-of-bounds structured `DriverError` (scriptable `catch`). */
function assertWithinWorkspace(op: string, cwd: string, arg: string): string {
  const resolved = resolveWithinWorkspace(cwd, arg);
  if (resolved === undefined) {
    throw new WorkflowError(
      "DriverError",
      `world-read ${op}: path '${arg}' is outside the workspace; pass a path inside it.`,
    );
  }
  return resolved;
}

/**
* The path returned by the port is normalized to a **workspace relative** path (the shape of the facade promise). The port may be an absolute path or
* Relative paths have been given, both are accepted. The delimiters are unified into `/`: this value will be entered into the journal and will also be interpolated into the model prompt.
* So it should not change with the host platform (the same script should read the same path on windows and mac).
 *
* **Export is intentional** (same as {@link resolveWithinWorkspace}): the `sourcePath` in the product record is the same
* Value - workspace relative, forward slash - and the "same value" must be defined in only one place, otherwise the two places will diverge sooner or later on windows.
 */
export function toWorkspaceRelative(cwd: string, path: string): string {
  const rel = resolvePath(cwd, path) === path ? relativePath(cwd, path) : path;
  return rel.replace(/\\/g, "/");
}

/** The first line of stderr / stdout (used for error messages, the entire output is not echoed). */
function firstLine(text: string): string {
  return text.split("\n", 1)[0]?.trim() ?? "";
}

/**
* Inline cap on git text output (path list/status/log). There is no spec-level cap on these ops, but a
* Unbounded buffer is not an option - 100 commits and a working tree path list is orders of magnitude shy of 4MB.
* git.diff **does not** use this value: it has its own 512KB upper limit, and relies on cap+1 for overflow detection.
 */
const GIT_TEXT_OUTPUT_BYTES = 4 * 1024 * 1024;

/** A hit in `files.grep`. The authoritative declaration is `declare interface GrepMatch` in FACADE_DTS. */
interface GrepMatch {
  path: string;
  line: number;
  text: string;
}

/** A short description of the shape of the actual parameter (only used in error messages, the full content is not echoed). */
function describeArg(value: unknown): string {
  if (value === undefined) return "undefined";
  if (value === null) return "null";
  return Array.isArray(value) ? "array" : typeof value;
}
