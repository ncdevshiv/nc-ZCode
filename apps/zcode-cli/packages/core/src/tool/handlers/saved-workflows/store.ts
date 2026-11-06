// ============================================================
// Saved workflows - storage layer (scope root + parse/enumerate/write)
// ============================================================
//
// All are **synchronous** fs. The reason is not to save trouble: the `prepareApproval` contract of the confirmation window is synchronous (core's
// ToolEntry annotation: "it inspects the input the executor already holds"), and with the `saved` source
// The initiated run must read the script **before** the pop-up window - without the script, there will be no cause and effect diagram, and the user will be on an empty window.
//Approve execution. These files are local, single, and measured in KB, and the cost of synchronous reading is much less than opening a new asynchronous approval path for it.
// There is a precedent of the same form on the handler side of core (readFileSync of bash-git-runtime-safety.ts).

import {
  mkdirSync,
  readdirSync,
  readFileSync,
  renameSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import {
  SAVED_WORKFLOW_FILE_EXTENSION,
  SAVED_WORKFLOW_GLOBAL_DIR,
  SAVED_WORKFLOW_MAX_NAME_CHARS,
  SAVED_WORKFLOW_PROJECT_DIR,
  isValidSavedWorkflowName,
  type SavedWorkflowEntry,
  type SavedWorkflowInvalidEntry,
  type SavedWorkflowMeta,
  type SavedWorkflowScope,
  type SavedWorkflowShadowing,
} from "@zcode/contracts";
import { parseSavedWorkflow, serializeSavedWorkflow } from "./frontmatter.js";

/** A search root: scope label + absolute directory. */
export interface SavedWorkflowRoot {
  scope: SavedWorkflowScope;
  dir: string;
}

/**
* `savedWorkflowRoots` / Optional parameters for derived functions. `homeDir` is only injected for testing: production constant access
* `os.homedir()` (the home directory of the machine where the agent process is located) does not follow any `storage.dir` configuration.
 */
export interface SavedWorkflowRootsOptions {
  homeDir?: string;
}

/**
* The search root for this session, **ordered by priority**: `[project, global]`.
 *
* The project file is located in `.zcode/workflows/` in the session working directory, and the global file is located in `~/.zcode/workflows/` in the home directory.
* All searches are first-wins in order: the one in the project always wins the global one (same name masking).
 */
export function savedWorkflowRoots(
  cwd: string,
  options?: SavedWorkflowRootsOptions,
): SavedWorkflowRoot[] {
  return [
    { scope: "project", dir: join(cwd, SAVED_WORKFLOW_PROJECT_DIR) },
    { scope: "global", dir: join(options?.homeDir ?? homedir(), SAVED_WORKFLOW_GLOBAL_DIR) },
  ];
}

/** The search root for a single scope. The scope is a known enumeration and must be present in `savedWorkflowRoots`. */
export function savedWorkflowRoot(
  cwd: string,
  scope: SavedWorkflowScope,
  options?: SavedWorkflowRootsOptions,
): SavedWorkflowRoot {
  const root = savedWorkflowRoots(cwd, options).find((candidate) => candidate.scope === scope);
// scope is a member of the SavedWorkflowScope enumeration, roots covers all members, and find will not fail.
  return root!;
}

/** A saved definition that parses successfully. */
export interface ResolvedSavedWorkflow {
  name: string;
  path: string;
  scope: SavedWorkflowScope;
  meta: SavedWorkflowMeta;
  script: string;
  /**
* File original text (metadata block + text), **byte by byte**. That's what the draft copy takes: the copy must match the bytes just read
* Exactly the same, reserializing it will cause the default values ​​​​of `args`, comments and handwritten YAML formatting to drift in the copy, and
* That copy is what the model will return as `path` next.
   */
  source: string;
/** The number of lines before the text; added when converting diagnostics to file lines (see {@link parseSavedWorkflow}). */
  bodyLineOffset: number;
}

export type SavedWorkflowResolveFailure =
  | { ok: false; reason: "invalid_name"; detail: string }
  | { ok: false; reason: "not_found" }
  | { ok: false; reason: "parse_error"; path: string; detail: string }
  | { ok: false; reason: "read_error"; path: string; detail: string };

export type SavedWorkflowResolveResult =
  | ({ ok: true } & ResolvedSavedWorkflow)
  | SavedWorkflowResolveFailure;

export interface SavedWorkflowListResult {
  entries: SavedWorkflowEntry[];
  invalid: SavedWorkflowInvalidEntry[];
}

/** name → filename. The name has been passed {@link isValidSavedWorkflowName}, so I won’t go into detail here. */
export function savedWorkflowFileName(name: string): string {
  return `${name}${SAVED_WORKFLOW_FILE_EXTENSION}`;
}

/**
* The point at which a name falls under a given root. The write side and the read side share it - the two separate paths are exactly the "save successfully but cannot be read out"
* Classic origin.
 */
export function savedWorkflowPath(root: SavedWorkflowRoot, name: string): string {
  return join(root.dir, savedWorkflowFileName(name));
}

/**
* Parse a saved workflow by name.
 *
* The name first passes the legality check and then the path is spelled: this sequence is the defense line itself that the path traverses, not the input hygiene section——
* After `../../.ssh/id_rsa` is spelled into join, it will be an absolute path that can be read.
 *
* With `scope`: only search for that one (directed search between the center and `saved.scope`); without: two roots first-wins in order.
 */
export function resolveSavedWorkflow(options: {
  cwd: string;
  name: string;
  scope?: SavedWorkflowScope;
  homeDir?: string;
}): SavedWorkflowResolveResult {
  const { cwd, name, scope, homeDir } = options;
  if (!isValidSavedWorkflowName(name)) {
    return {
      ok: false,
      reason: "invalid_name",
      detail: `workflow names may only contain letters, digits, '.', '-' and '_', and must be 1-${SAVED_WORKFLOW_MAX_NAME_CHARS} characters`,
    };
  }

  const roots =
    scope === undefined
      ? savedWorkflowRoots(cwd, { homeDir })
      : [savedWorkflowRoot(cwd, scope, { homeDir })];

  for (const root of roots) {
    const path = savedWorkflowPath(root, name);
    let source: string;
    try {
      source = readFileSync(path, "utf8");
    } catch (error) {
// This one doesn't have it, look at the next one. The rest of the misreadings (permissions, directories) are problems with this file. Say it instead.
// Pretend not to be found - "not found" will send the user to check for a name that actually exists.
      if (isNotFound(error)) continue;
      return { ok: false, reason: "read_error", path, detail: describeError(error) };
    }

    const parsed = parseSavedWorkflow(source);
    if (!parsed.ok) {
      return { ok: false, reason: "parse_error", path, detail: parsed.detail };
    }
    return {
      ok: true,
      name,
      path,
      scope: root.scope,
      meta: parsed.meta,
      script: parsed.script,
      source,
      bodyLineOffset: parsed.bodyLineOffset,
    };
  }

  return { ok: false, reason: "not_found" };
}

/**
* Enum save definitions (tiled scans to depth 1, no recursion into subdirectories).
 *
* Bad files go into `invalid` instead of throwing an error: these files were manually modified by the user, and one typo shouldn't make the entire list disappear.
 *
* Do not give `scope`: two roots in order, the same name defines first-wins, the obscured one **does not** appear in the list - what the list wants to say
* is "what will be run when calling this name", not "how many copies are on the disk". Given `scope`: only scan that one, without any masking
* (The hub's global group needs to see the copy that is obscured by the project file).
 */
export function listSavedWorkflows(options: {
  cwd: string;
  scope?: SavedWorkflowScope;
  homeDir?: string;
}): SavedWorkflowListResult {
  const entries: SavedWorkflowEntry[] = [];
  const invalid: SavedWorkflowInvalidEntry[] = [];
  const claimed = new Set<string>();

  const roots =
    options.scope === undefined
      ? savedWorkflowRoots(options.cwd, { homeDir: options.homeDir })
      : [savedWorkflowRoot(options.cwd, options.scope, { homeDir: options.homeDir })];

  for (const root of roots) {
    let fileNames: string[];
    try {
      fileNames = readdirSync(root.dir);
    } catch (error) {
// It is normal that the directory does not exist (most projects have not saved the workflow), and it is not an error.
      if (isNotFound(error)) continue;
      invalid.push({ path: root.dir, reason: describeError(error) });
      continue;
    }

// The order of readdir is file system dependent; ordering makes the list consistent on both machines.
    for (const fileName of [...fileNames].sort()) {
      if (!fileName.endsWith(SAVED_WORKFLOW_FILE_EXTENSION)) continue;
      const name = fileName.slice(0, -SAVED_WORKFLOW_FILE_EXTENSION.length);
      const path = join(root.dir, fileName);

      if (!isValidSavedWorkflowName(name)) {
        invalid.push({ path, reason: "file name is not a usable workflow name" });
        continue;
      }
// Already claimed by a higher-priority scope: this one cannot be reached, so it will not be listed.
      if (claimed.has(name)) continue;

      let source: string;
      try {
        source = readFileSync(path, "utf8");
      } catch (error) {
// The directory entry exists but cannot be read (subdirectories, permissions) - it's not "no", it's "broken".
        invalid.push({ path, reason: describeError(error) });
        continue;
      }

      const parsed = parseSavedWorkflow(source);
      if (!parsed.ok) {
        invalid.push({ path, reason: `${parsed.reason}: ${parsed.detail}` });
        continue;
      }

      claimed.add(name);
      entries.push({
        name,
        description: parsed.meta.description,
        ...(parsed.meta.whenToUse === undefined ? {} : { whenToUse: parsed.meta.whenToUse }),
        ...(parsed.meta.args === undefined ? {} : { args: parsed.meta.args }),
        scope: root.scope,
        path,
      });
    }
  }

  return { entries, invalid };
}

/**
* Write a save definition, return the drop point and "whether it is overwritten this time".
 *
* `scope` determines which root it falls on (default `project`, maintaining existing semantics). Scope is a **write-side choice** - model
* This is required in SaveWorkflow.
 */
export function saveSavedWorkflow(options: {
  cwd: string;
  name: string;
  meta: SavedWorkflowMeta;
  script: string;
  scope?: SavedWorkflowScope;
  homeDir?: string;
}): { path: string; scope: SavedWorkflowScope; overwritten: boolean } {
  const root = savedWorkflowRoot(options.cwd, options.scope ?? "project", {
    homeDir: options.homeDir,
  });
  const path = savedWorkflowPath(root, options.name);
  mkdirSync(root.dir, { recursive: true });
  const overwritten = fileExists(path);
  writeFileSync(path, serializeSavedWorkflow(options.meta, options.script), "utf8");
  return { path, scope: root.scope, overwritten };
}

/** Whether the target already exists (the confirmation window should refer to "overwrite" and "new" as two different things). The project file is checked by default. */
export function savedWorkflowExists(options: {
  cwd: string;
  name: string;
  scope?: SavedWorkflowScope;
  homeDir?: string;
}): boolean {
  if (!isValidSavedWorkflowName(options.name)) return false;
  const root = savedWorkflowRoot(options.cwd, options.scope ?? "project", {
    homeDir: options.homeDir,
  });
  return fileExists(savedWorkflowPath(root, options.name));
}

/**
* When saving to `scope`, whether another file already has a definition with the same name (hiding the fact for display in the confirmation window).
 *
* Save the project file and the global file already has the same name → `hides_global` (the project file in this project wins); Save the global file and the project file already has the same name
* Same name → `hidden_by_project` (it cannot run in this project). Neither level has → `undefined`.
 */
export function findSavedWorkflowShadowing(options: {
  cwd: string;
  name: string;
  scope: SavedWorkflowScope;
  homeDir?: string;
}): SavedWorkflowShadowing | undefined {
  if (!isValidSavedWorkflowName(options.name)) return undefined;
  const otherScope: SavedWorkflowScope = options.scope === "project" ? "global" : "project";
  const otherRoot = savedWorkflowRoot(options.cwd, otherScope, { homeDir: options.homeDir });
  if (!fileExists(savedWorkflowPath(otherRoot, options.name))) return undefined;
  return options.scope === "project" ? "hides_global" : "hidden_by_project";
}

/** The result of moving the global file back to the project file. */
export type SavedWorkflowMoveResult =
  | { ok: true; from: string; to: string }
  | { ok: false; reason: "invalid_name"; detail: string }
  | { ok: false; reason: "not_found" }
  | { ok: false; reason: "target_exists"; path: string }
  | { ok: false; reason: "read_error" | "write_error"; path: string; detail: string };

/**
* Move the `name` of the global root to the project root of `cwd` (**only this way**).
 *
* Reverse (project→global) is not about moving files: most project files refer to the path/command/convention of this warehouse, and they are moved there byte by byte.
* A global definition that will inevitably break in other projects. That has always been a generalization of the model ("promoted to the big picture": the GUI was
* A summary prompt is sent to a new session, and the model is saved as a global file through SaveWorkflow). Global→Project is a specialization. If a global definition falls into a certain project, it will still run, so the files still need to be moved.
 *
* **Move byte by byte** (no parse, no reserialize): frontmatter does not store scope, so moving means moving files.
* `renameSync` takes precedence, cross-device (EXDEV) falls back to read → write → delete. Reject if the target already exists (not overwrite: Coverage is
* SaveWorkflow is an action only available through the confirmation window). The name first goes through a legality check - the line of defense itself that the path crosses.
 */
export function moveSavedWorkflow(options: {
  cwd: string;
  name: string;
  homeDir?: string;
}): SavedWorkflowMoveResult {
  const { cwd, name, homeDir } = options;
  if (!isValidSavedWorkflowName(name)) {
    return {
      ok: false,
      reason: "invalid_name",
      detail: `workflow names may only contain letters, digits, '.', '-' and '_', and must be 1-${SAVED_WORKFLOW_MAX_NAME_CHARS} characters`,
    };
  }

  const fromRoot = savedWorkflowRoot(cwd, "global", { homeDir });
  const toRoot = savedWorkflowRoot(cwd, "project", { homeDir });
  const fromPath = savedWorkflowPath(fromRoot, name);
  const toPath = savedWorkflowPath(toRoot, name);

  if (!fileExists(fromPath)) return { ok: false, reason: "not_found" };
  if (fileExists(toPath)) return { ok: false, reason: "target_exists", path: toPath };

  try {
    mkdirSync(toRoot.dir, { recursive: true });
  } catch (error) {
    return { ok: false, reason: "write_error", path: toPath, detail: describeError(error) };
  }

  try {
    renameSync(fromPath, toPath);
    return { ok: true, from: fromPath, to: toPath };
  } catch (error) {
// When cross-device (for example, the home directory and the project branch have different mount points), rename reports EXDEV: read→write→delete fallback transfer,
// The reading and writing are returned to their respective errors, so that the upper layer can say "the source cannot be read" and "the target cannot be written" as two different things.
    if ((error as NodeJS.ErrnoException | undefined)?.code !== "EXDEV") {
      return { ok: false, reason: "write_error", path: toPath, detail: describeError(error) };
    }
  }

  let bytes: Buffer;
  try {
    bytes = readFileSync(fromPath);
  } catch (error) {
    return { ok: false, reason: "read_error", path: fromPath, detail: describeError(error) };
  }
  try {
    writeFileSync(toPath, bytes);
    unlinkSync(fromPath);
  } catch (error) {
    return { ok: false, reason: "write_error", path: toPath, detail: describeError(error) };
  }
  return { ok: true, from: fromPath, to: toPath };
}

function fileExists(path: string): boolean {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}

function isNotFound(error: unknown): boolean {
  const code = (error as NodeJS.ErrnoException | undefined)?.code;
// ENOTDIR: There is a section in the middle of the path that is a file (`.zcode/workflows` was created into a file). For search purposes the same as
// "Directory does not exist" is the same thing.
  return code === "ENOENT" || code === "ENOTDIR";
}

function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
