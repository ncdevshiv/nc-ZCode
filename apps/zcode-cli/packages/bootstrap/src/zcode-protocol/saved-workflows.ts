// GUI hub for saved workflows: workspace-level, sessionless approach.
//
// The same precedent as skills/referenceCatalog: without sessionId, scan the directory every time it is called - the snapshot will be missed when mounting
// Manually modified by the user / The file that the model has just been saved to disk by SaveWorkflow.
// The parser and serializer are only taken from @zcode/core: frontmatter is not parsed here, nor YAML is spelled.
//
// Global scope: The params of the five methods can be collected with optional `scope` (default
// `project`). When `global` is used, the operation is based on the local global root (`~/.zcode/workflows/`). `workspace` is just a **carrier**——
// The processor does not read the path of the global file. `workflows/move` moves global files back to the `workspace` project (only this time).
import { unlink, writeFile } from "node:fs/promises";
import { SavedWorkflowMetaSchema, isValidSavedWorkflowName } from "@zcode/contracts";
import {
  listSavedWorkflows,
  moveSavedWorkflow,
  resolveSavedWorkflow,
  savedWorkflowPath,
  savedWorkflowRoot,
  serializeSavedWorkflow,
  type SavedWorkflowResolveFailure,
} from "@zcode/core";
import {
  ZCODE_WORKFLOWS_RUNS_MAX_LIMIT,
  zcodeWorkflowsDeleteParamsSchema,
  zcodeWorkflowsGetParamsSchema,
  zcodeWorkflowsListParamsSchema,
  zcodeWorkflowsMoveParamsSchema,
  zcodeWorkflowsRunsParamsSchema,
  zcodeWorkflowsUpdateMetaParamsSchema,
  type ZCodeSavedWorkflowRun,
  type ZCodeSavedWorkflowScope,
  type ZCodeWorkflowsDeleteResult,
  type ZCodeWorkflowsGetResult,
  type ZCodeWorkflowsListResult,
  type ZCodeWorkflowsMoveResult,
  type ZCodeWorkflowsRunsResult,
  type ZCodeWorkflowsUpdateMetaResult,
} from "@zcode/shared";
import type { JournalStorePort } from "@zcode/dynamic-workflow";
import { artifactsOf } from "../app/dynamic-workflow-run-observation.js";
import {
  resolveDynamicWorkflowJournalStore,
  supportsRunIntrospection,
} from "../app/dynamic-workflow-run-service.js";
import { parseParams, type ZCodeProtocolAgentServerContext } from "./server-types.js";

// The default is `project`: the old GUI and project file calls without scope will go to the root of the project verbatim, and the shape will not change (version skew).
function scopeOf(params: { scope?: ZCodeSavedWorkflowScope }): ZCodeSavedWorkflowScope {
  return params.scope ?? "project";
}

export async function listSavedWorkflowsOp(
  _context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodeWorkflowsListResult> {
  const params = parseParams(zcodeWorkflowsListParamsSchema, rawParams);
  const cwd = params.workspace.workspacePath;
  const scope = scopeOf(params);
// The central PROJECT group only lists the project's share, and the global group only lists the global share - **always** passes the directional scope and never leaves.
// Undirected variant (two first-wins will mix global into the project group and hide the obscured global from the global group).
  const listed = listSavedWorkflows({ cwd, scope });
//Scanned directory (returned even if it does not exist): GUI's file monitoring relies on this watch.
  return {
    workflows: listed.entries,
    invalid: listed.invalid,
    dir: savedWorkflowRoot(cwd, scope).dir,
  };
}

export async function getSavedWorkflowOp(
  _context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodeWorkflowsGetResult> {
  const params = parseParams(zcodeWorkflowsGetParamsSchema, rawParams);
// Directed scope: `global` only checks the global root and does not block it - the central global group must see the copy blocked by the project file.
  const resolved = resolveSavedWorkflow({
    cwd: params.workspace.workspacePath,
    name: params.name,
    scope: scopeOf(params),
  });
  if (!resolved.ok) return toFailure(resolved);
  return {
    ok: true,
    name: resolved.name,
    path: resolved.path,
    scope: resolved.scope,
    meta: resolved.meta,
    script: resolved.script,
  };
}

/**
* Only rewrite the metadata at the top of the file: read back the current script text, and then overwrite the entire file as `serialize(newMeta, script)`.
* Reading-modification-writing are completed in the same call; no three-party merge is performed (the file is small, stand-alone, and the user is modifying it himself).
 */
export async function updateSavedWorkflowMetaOp(
  _context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodeWorkflowsUpdateMetaResult> {
  const params = parseParams(zcodeWorkflowsUpdateMetaParamsSchema, rawParams);
// shared is aligned verbatim with the meta schema of contracts, but the serializer recognizes the type of contracts; go through it again
// Let "drift on both sides" explode to -32602 here instead of writing a file that you can't read back.
  const meta = SavedWorkflowMetaSchema.parse(params.meta);
  const resolved = resolveSavedWorkflow({
    cwd: params.workspace.workspacePath,
    name: params.name,
    scope: scopeOf(params),
  });
  if (!resolved.ok) return toFailure(resolved);
  await writeFile(resolved.path, serializeSavedWorkflow(meta, resolved.script), "utf8");
  return { ok: true, path: resolved.path };
}

/**
* Delete by name only: `isValidSavedWorkflowName` precedes the path - this order is the line of defense itself that the path crosses
* (the same argument for store.ts), so the protocol does not accept paths nor `..`. Select the root by scope (no longer hard-coding roots[0]):
* `global` deletes the global root copy of this machine. The legacy `.workflow.js` with the same name is a file for another set of parsers and is not visible.
 */
export async function deleteSavedWorkflowOp(
  _context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodeWorkflowsDeleteResult> {
  const params = parseParams(zcodeWorkflowsDeleteParamsSchema, rawParams);
  if (!isValidSavedWorkflowName(params.name)) {
    return { ok: false, reason: "invalid_name" };
  }
  const root = savedWorkflowRoot(params.workspace.workspacePath, scopeOf(params));
  const path = savedWorkflowPath(root, params.name);
  try {
    await unlink(path);
  } catch (error) {
    const code = (error as NodeJS.ErrnoException | undefined)?.code;
    if (code === "ENOENT" || code === "ENOTDIR") return { ok: false, reason: "not_found" };
    return { ok: false, reason: "read_error", detail: describeError(error) };
  }
  return { ok: true, path };
}

/**
* run history, attributed to workflow by `dwf_run.name`. Read-only journal: The hub is outside any session and cannot be seen
* The registry gap of "submit has been returned, but the line has not yet been completed" - those few microtasks are filled by the next refresh, and it is not worth tying the hub to the session for them.
* The absence of journal (injected test store, implementation without introspection query) returns an empty page instead of throwing an error: the hub accordingly displays "not yet running".
 *
* `project` (default): only check `dwf_run.cwd === workspacePath`. `global`: **not** filter by cwd, across all projects
* Get the running history of this name (the global workflow runs in any project, so the history spans cwd); each line returns `cwd` for the GUI to target the project.
 */
export async function listSavedWorkflowRunsOp(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodeWorkflowsRunsResult> {
  const params = parseParams(zcodeWorkflowsRunsParamsSchema, rawParams);
  const journal = resolveDynamicWorkflowJournalStore(context.deps.sessionStore);
  if (journal === undefined || !supportsRunIntrospection(journal)) return { runs: [] };
  const limit = Math.min(ZCODE_WORKFLOWS_RUNS_MAX_LIMIT, params.limit);
  const global = scopeOf(params) === "global";
// Take one more ** just to determine truncated** (the same convention of run service and v4 event paging).
// The global variant omits the cwd predicate (journal's cwd is optional = across all projects); the project variant passes the cwd, unchanged verbatim.
  const rows = journal.listRuns({
    ...(global ? {} : { cwd: params.workspace.workspacePath }),
    limit: limit + 1,
    ...(params.name === undefined ? {} : { name: params.name }),
  });
  const truncated = rows.length > limit;
  const page = truncated ? rows.slice(0, limit) : rows;
// Product summary: once per row `listArtifactRows` (journal's persistent home), only done when the ability is present - if it is absent
// If the entire field is absent, the central chips will not be drawn. **Only count the rows** of this page, so the cost is of the same order as the page size (≤ 50).
  const artifactsByRun = savedWorkflowRunArtifacts(journal, page);
  const runs: ZCodeSavedWorkflowRun[] = page.map((row) => ({
    runId: row.runId,
    ...(row.name === undefined ? {} : { name: row.name }),
    status: row.status,
    ...(row.stopReason === undefined ? {} : { stopReason: row.stopReason }),
    createdAt: row.timeCreated,
    updatedAt: row.timeUpdated,
    spentTokens: row.spentTokens,
    ...(row.parentSessionId === undefined ? {} : { parentSessionId: row.parentSessionId }),
    ...(row.toolCallId === undefined ? {} : { toolCallId: row.toolCallId }),
    ...(row.args === undefined ? {} : { args: row.args }),
//The actual running project directory: the global variant uses it to mark each row's project; in the project variant, it is always equal to workspacePath, which is harmless.
    ...(row.cwd === undefined ? {} : { cwd: row.cwd }),
    ...(artifactsByRun.get(row.runId) === undefined
      ? {}
      : { artifacts: artifactsByRun.get(row.runId)! }),
  }));
  return { runs, ...(truncated ? { truncated: true } : {}) };
}

/** Upper bound of central chips: the number of kind icons that can be drawn in one line (the same value as `.max(8)` of the protocol schema). */
const SAVED_WORKFLOW_RUN_ARTIFACTS_LIMIT = 8;

/**
* One page of run lines → **User interface product** summary for each line.
 *
* ⚠ Terminology: The artifact here is the output of the script published to the user through `artifact.*`, not the top-level return value of the script
* (engine-internal `RunSettlement.artifact`).
 *
* Get the number {@link artifactsOf}: **Same merge rule as snapshot/`GetWorkflowRun`** (only completed lines,
* Sort by version in ascending order with the same ID, and take the latest version in the top-level field). If the hub writes it again, the products of the same run will be displayed in the hub and side panels.
* Looks different. `listArtifactRows` is not on the engine port, so capability detection is done inside `artifactsOf`:
* Absent ⇒ Not getting entries per row ⇒ Fields entirely absent (old CLI/injected test store unchanged verbatim).
 *
* The part's run does not enter the table - the caller accordingly leaves the entire field absent instead of sending an empty array.
 */
function savedWorkflowRunArtifacts(
  journal: JournalStorePort,
  page: readonly { runId: string }[],
): Map<string, ZCodeSavedWorkflowRun["artifacts"]> {
  const byRun = new Map<string, ZCodeSavedWorkflowRun["artifacts"]>();
  for (const row of page) {
    const { artifacts } = artifactsOf(row.runId, journal);
    if (artifacts === undefined || artifacts.length === 0) continue;
    byRun.set(
      row.runId,
      artifacts.slice(0, SAVED_WORKFLOW_RUN_ARTIFACTS_LIMIT).map((artifact) => ({
        id: artifact.id,
        kind: artifact.kind,
        ...(artifact.title === undefined ? {} : { title: artifact.title }),
        version: artifact.version,
        ...(artifact.contentType === undefined ? {} : { contentType: artifact.contentType }),
      })),
    );
  }
  return byRun;
}

/**
* Move the file with the same name of the local global root to the `workspace` project root (this is the only way: project → global is a summary of the model "promoted to global", not a file move). Move byte by byte
* (frontmatter does not have scope), no coverage (rejection if the target already exists, invariant 7). The name is a priori and then the path is spelled.
* `workspace` is both a carrier and a target project: core calculates two roots based on its cwd and the local home.
 */
export async function moveSavedWorkflowOp(
  _context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodeWorkflowsMoveResult> {
  const params = parseParams(zcodeWorkflowsMoveParamsSchema, rawParams);
  const result = moveSavedWorkflow({ cwd: params.workspace.workspacePath, name: params.name });
  if (result.ok) {
    return { ok: true, from: result.from, to: result.to };
  }
// The failure branch of core is aligned verbatim with the protocol result; path / detail is brought out unchanged when present (the key is omitted when absent).
  switch (result.reason) {
    case "invalid_name":
      return { ok: false, reason: "invalid_name", detail: result.detail };
    case "not_found":
      return { ok: false, reason: "not_found" };
    case "target_exists":
      return { ok: false, reason: "target_exists", path: result.path };
    case "read_error":
    case "write_error":
      return { ok: false, reason: result.reason, path: result.path, detail: result.detail };
  }
}

function toFailure(failure: SavedWorkflowResolveFailure) {
  switch (failure.reason) {
    case "invalid_name":
      return { ok: false as const, reason: failure.reason, detail: failure.detail };
    case "not_found":
      return { ok: false as const, reason: failure.reason };
    case "parse_error":
    case "read_error":
      return { ok: false as const, reason: failure.reason, detail: failure.detail };
  }
}

function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
