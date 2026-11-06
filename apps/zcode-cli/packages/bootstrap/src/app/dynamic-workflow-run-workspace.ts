// ============================================================
// The reading surface of the workspace transcript (the implementation of the two workspace methods of DynamicWorkflowRunPort)
// ============================================================
//
// A run's `files.*` / `git.*` / `world.run` call in journal is `kind ∈ {world-read,
// `dwf_node` line of world-run}`: `input_json` (migration 0030) is the op with arguments, `result_json` is the body.
// The UI plays them back into tool cards - the list (without text) comes first, and is taken when the text is expanded.
//
// Remove from dynamic-workflow-run-service.ts (this file is already in the existing debt of max-lines), and match it with the product
// artifact-read.ts same reason.
//
// **Authorization chain** (same discipline as readArtifact, both queries are used):
//
// runId given by the caller
// ├─① Does the dwf_run line exist?                              No ⇒ undefined
// ├─② Is the parent_session_id of this row == the parent session of this service?   No ⇒ undefined
// └─③ Only touch the node row
//
// The manifest also authorizes not just the text: the text of `files.read` is the workspace file content, and the args on the manifest are already the path and
// Command line - neither should allow "not your run". The three types of rejections are normalized to undefined (the gateway is normalized to an empty list /
// not found), without telling an overreaching caller which half it guessed correctly.

import type { DwfRunIntrospectionQueries, DwfWorldNodeRow } from "@zcode/adapters/storage";
import type {
  DynamicWorkflowRunError,
  DynamicWorkflowRunWorkspaceNode,
  DynamicWorkflowRunWorkspaceNodeResult,
  DynamicWorkflowRunWorkspaceNodeResultQuery,
  DynamicWorkflowRunWorkspaceNodeSummary,
} from "@zcode/contracts";
import type { JournalStorePort, NodeRecord } from "@zcode/dynamic-workflow";

/** Display length of failure message (= protocol side `WORKFLOW_WORKSPACE_LIMITS.maxErrorMessageLength`). */
const ERROR_MESSAGE_MAX_CHARS = 2000;

/**
* Journal with workspace reading surface. The only source of signatures is adapters' {@link DwfRunIntrospectionQueries}
* (`import type`, zero dependency at runtime) - the same argument as the product reading: `listWorldNodes` is not in the engine
* {@link JournalStorePort} (the engine never enumerates nodes by kind), and can only be connected through capability detection.
 */
interface WorkspaceReadableJournal
  extends JournalStorePort, Pick<DwfRunIntrospectionQueries, "listWorldNodes"> {}

/** Whether the journal has a workspace for reading. Deliberately a sibling of `supportsArtifactReads` rather than extending it (same argument). */
function supportsWorkspaceReads(journal: JournalStorePort): journal is WorkspaceReadableJournal {
  return typeof (journal as Partial<DwfRunIntrospectionQueries>).listWorldNodes === "function";
}

interface WorkflowWorkspaceReadDeps {
  journal: JournalStorePort;
/** The parent session of this service (= the session of this app). The comparison object in step ② of the authorization chain. */
  parentSessionId: string;
}

/** Authorization chain ①②. `getRun` is on the engine port and does not require introspection capability detection. */
function authorizeRun(deps: WorkflowWorkspaceReadDeps, runId: string): boolean {
  const run = deps.journal.getRun(runId);
  if (run === undefined) return false;
//Old rows whose parent_session_id is NULL are **not released**: "Unable to determine" does not mean "Judgment passed".
  return run.parentSessionId !== undefined && run.parentSessionId === deps.parentSessionId;
}

/** List of transcripts in the workspace: world lines are in the order they are dropped into the library, without text. */
export async function listWorkspaceNodesFrom(
  deps: WorkflowWorkspaceReadDeps,
  runId: string,
): Promise<readonly DynamicWorkflowRunWorkspaceNode[] | undefined> {
  if (!supportsWorkspaceReads(deps.journal)) return undefined;
  if (!authorizeRun(deps, runId)) return undefined;
  return deps.journal.listWorldNodes(runId).map(toWorkspaceNode);
}

function toWorkspaceNode(row: DwfWorldNodeRow): DynamicWorkflowRunWorkspaceNode {
  const summary = summaryOf(row);
  return {
    siteId: row.siteId,
    ordinal: row.ordinal,
    kind: row.kind === "world-run" ? "world-run" : "world-read",
    ...(row.input === undefined
      ? {}
      : {
          op: row.input.op,
          args: row.input.args,
          ...(row.input.truncated === true ? { inputTruncated: true as const } : {}),
        }),
    status: row.status,
    ...(row.error === undefined ? {} : { error: toRunError(row.error) }),
    ...(summary === undefined ? {} : { summary }),
    createdAt: row.timeCreated,
    updatedAt: row.timeUpdated,
  };
}

/** The summary of the storage layer calculated in the library → port shape; only rows with successful settlement (with text) are available. */
function summaryOf(row: DwfWorldNodeRow): DynamicWorkflowRunWorkspaceNodeSummary | undefined {
  if (row.status !== "completed" || row.resultBytes === undefined) return undefined;
  return {
    resultBytes: row.resultBytes,
    ...(row.resultCount === undefined ? {} : { resultCount: row.resultCount }),
    ...(row.exitCode === undefined ? {} : { exitCode: row.exitCode }),
    ...(row.stdoutBytes === undefined ? {} : { stdoutBytes: row.stdoutBytes }),
    ...(row.stderrBytes === undefined ? {} : { stderrBytes: row.stderrBytes }),
  };
}

/** `WorkflowErrorJson` of journal → code + message of the port (the remaining fields do not exit the port; message is truncated). */
function toRunError(error: { code: string; message: string }): DynamicWorkflowRunError {
  const message =
    error.message.length > ERROR_MESSAGE_MAX_CHARS
      ? `${error.message.slice(0, ERROR_MESSAGE_MAX_CHARS - 1)}…`
      : error.message;
  return { code: error.code, message };
}

/** The body of a node, conformally bounded by `query.maxBytes`. */
export async function readWorkspaceNodeResultFrom(
  deps: WorkflowWorkspaceReadDeps,
  runId: string,
  siteId: string,
  ordinal: number,
  query: DynamicWorkflowRunWorkspaceNodeResultQuery,
): Promise<DynamicWorkflowRunWorkspaceNodeResult | undefined> {
  if (!authorizeRun(deps, runId)) return undefined;
  const node = deps.journal.getNode(runId, siteId, ordinal);
  if (node === undefined) return undefined;
  if (node.kind !== "world-read" && node.kind !== "world-run") return undefined;
  return toNodeResult(node, query.maxBytes);
}

function toNodeResult(node: NodeRecord, maxBytes: number): DynamicWorkflowRunWorkspaceNodeResult {
  if (node.status === "failed") {
    return {
      status: "failed",
      ...(node.error === undefined ? {} : { error: toRunError(node.error) }),
      truncated: false,
      totalBytes: 0,
    };
  }
  if (node.status !== "completed" || !("result" in node)) {
    return { status: node.status, truncated: false, totalBytes: 0 };
  }
  const bounded = boundWorkspaceResult(node.result, maxBytes);
  return {
    status: "completed",
    result: bounded.result,
    truncated: bounded.truncated,
    totalBytes: bounded.totalBytes,
  };
}

// ──Conformal and bounded ─────────────────────────────────────────────────────────
// The policy of `WORLD_READ_CAPS` on the engine side is "reject when overflowing, never truncate and add a flag bit" - that is the access side of the script,
//Half grep results will cause the script to make wrong decisions. Here is the audit interface: What a card wants is "what ran and what were the first few hundred lines".
// Truncate and **explain** Truncation (`truncated` + `totalBytes`) is much more useful than a "too big to read".

function utf8ByteLength(text: string): number {
  return Buffer.byteLength(text, "utf8");
}

/** Cut the string to ≤ maxBytes UTF-8 bytes, not in the middle of the proxy pair. */
function truncateUtf8(text: string, maxBytes: number): string {
  if (utf8ByteLength(text) <= maxBytes) return text;
  const bytes = Buffer.from(text, "utf8").subarray(0, Math.max(0, maxBytes));
// Remove the incomplete multi-byte sequence at the end: decode will replace it with U+FFFD, we would rather lose one character.
  let end = bytes.length;
  while (end > 0 && (bytes[end - 1]! & 0b1100_0000) === 0b1000_0000) end -= 1;
  if (end > 0 && (bytes[end - 1]! & 0b1100_0000) === 0b1100_0000) end -= 1;
  return bytes.subarray(0, end).toString("utf8");
}

/**
* Bounded according to the shape of the text:
* - String (read/diff/status): trim;
* - Array (glob / grep / changedFiles / log): accumulate items one by one, and remove the tail if it cannot fit;
* - `{exitCode, stdout, stderr}` of `world.run`: exitCode is always retained, and the two outputs are each divided into half of the budget;
* - Others: After serialization, it will not exceed the original limit. If it exceeds the limit, it will degenerate into truncated JSON text (the shape is no longer preserved).
 */
function boundWorkspaceResult(
  result: unknown,
  maxBytes: number,
): { result: unknown; truncated: boolean; totalBytes: number } {
  const serialized = JSON.stringify(result) ?? "null";
  const totalBytes = utf8ByteLength(serialized);
  if (totalBytes <= maxBytes) return { result, truncated: false, totalBytes };

  if (typeof result === "string") {
    return { result: truncateUtf8(result, maxBytes), truncated: true, totalBytes };
  }
  if (Array.isArray(result)) {
    const kept: unknown[] = [];
    let used = 2; // 方括号
    for (const item of result) {
      const itemBytes = utf8ByteLength(JSON.stringify(item) ?? "null") + 1;
      if (used + itemBytes > maxBytes) break;
      kept.push(item);
      used += itemBytes;
    }
    return { result: kept, truncated: true, totalBytes };
  }
  if (isRunResult(result)) {
    const overhead = utf8ByteLength(JSON.stringify({ ...result, stdout: "", stderr: "" }));
    const budget = Math.max(0, maxBytes - overhead);
    const stderrWant = utf8ByteLength(result.stderr);
    const stdoutWant = utf8ByteLength(result.stdout);
// Divide each half in half; give the unused balance to the other.
    const stderrBudget = Math.min(stderrWant, Math.max(budget >> 1, budget - stdoutWant));
    const stdoutBudget = Math.max(0, budget - stderrBudget);
    return {
      result: {
        ...result,
        stdout: truncateUtf8(result.stdout, stdoutBudget),
        stderr: truncateUtf8(result.stderr, stderrBudget),
      },
      truncated: true,
      totalBytes,
    };
  }
  return { result: truncateUtf8(serialized, maxBytes), truncated: true, totalBytes };
}

function isRunResult(
  value: unknown,
): value is { exitCode: number; stdout: string; stderr: string } & Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  const fields = value as Record<string, unknown>;
  return (
    typeof fields.exitCode === "number" &&
    typeof fields.stdout === "string" &&
    typeof fields.stderr === "string"
  );
}
