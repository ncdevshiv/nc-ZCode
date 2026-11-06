import { basename, relative } from "node:path";
import {
  WORKSPACE_HOOK_REVIEW_TIMEOUT_MS,
  type WorkspaceHookBundleSnapshot,
} from "@zcode/contracts";
import type { WorkspaceHookReviewTarget, WorkspaceHookSnapshotEvaluation } from "@zcode/core";
import type { WorkspaceHookReviewRequestPayload } from "@zcode/shared/zcode-protocol-v4";
import type { WorkspaceHookReviewHostPort } from "./workspace-hook-review-types.js";

export function buildWorkspaceHookReviewRequest(input: {
  snapshot: WorkspaceHookBundleSnapshot;
  evaluation: WorkspaceHookSnapshotEvaluation;
  reviewFlowId: string;
  generation: number;
  sessionId: string;
  host: WorkspaceHookReviewHostPort;
  now: () => number;
  createId: () => string;
}): WorkspaceHookReviewRequestPayload {
  const createdAt = input.now();
  const stateByItem = new Map(
    input.evaluation.items.map((item) => [item.reviewItemId, item.trustState] as const),
  );
  const items = input.snapshot.hooks.map((entry) => ({
    reviewItemId: entry.reviewItemId,
    event: entry.event,
    ...(entry.matcher ? { matcher: entry.matcher } : {}),
    type: entry.type,
    displayName: entry.matcher ? `${entry.event} · ${entry.matcher}` : entry.event,
    displayCommand:
      entry.type === "process" ? [entry.command, ...(entry.args ?? [])].join(" ") : entry.command,
    sourcePath: entry.sourceRelativePath,
    resolvedTimeoutMs: entry.resolvedTimeoutMs,
    resolvedMaxOutputBytes: entry.resolvedMaxOutputBytes,
    executionMode:
      entry.type === "command" && entry.async ? ("background" as const) : ("foreground" as const),
    configuredEnabled: entry.configuredEnabled,
    editable: entry.editable,
    trustState: stateByItem.get(entry.reviewItemId) ?? "blocked_untrusted",
  }));
  return {
    kind: "workspaceHookReview",
    reviewFlowId: input.reviewFlowId,
    generation: input.generation,
    interactionId: `workspace-hook-interaction:${input.createId()}`,
    sessionId: input.sessionId,
    taskId: input.host.taskId,
    runId: input.host.runId,
    workspaceIdentity: input.snapshot.workspaceIdentity,
    workspaceLabel: input.host.workspaceLabel,
    ...(input.host.remoteSessionId ? { remoteSessionId: input.host.remoteSessionId } : {}),
    bundleDigest: input.snapshot.bundleDigest,
    createdAt,
    deadlineAt: createdAt + WORKSPACE_HOOK_REVIEW_TIMEOUT_MS,
    sourceFiles: input.snapshot.sourceFiles.map((source) => ({
      path: source.canonicalPath,
      displayPath: relative(source.baseDir, source.canonicalPath) || basename(source.canonicalPath),
      editable: source.editable,
    })),
    summary: {
      eventCount: new Set(items.map((item) => item.event)).size,
      hookCount: items.length,
// Note: pendingCount is a protocol contract field, shared
// workspaceHookReviewRequestPayloadSchema is based on "trustState ∈ {pending_trust,
// revoked, stale_digest}" Verify it, deliberately not looking at configuredEnabled - unilateral filtering will
// Direct breach of contract.
      //
// Unenabled Hooks can also be trusted along with the current immutable audit snapshot, so don't click here
// configuredEnabled filtering; switch only controls operation, trust only controls access.
      pendingCount: items.filter((item) =>
        ["pending_trust", "revoked", "stale_digest"].includes(item.trustState),
      ).length,
    },
    items,
    warningCode: "workspace_hooks_execute_code",
  };
}

export function resolveWorkspaceHookReviewDigests(
  snapshot: WorkspaceHookBundleSnapshot,
  reviewItemIds: readonly string[],
): string[] {
  const byId = new Map(snapshot.hooks.map((entry) => [entry.reviewItemId, entry] as const));
  return [...new Set(reviewItemIds)].map((reviewItemId) => {
    const entry = byId.get(reviewItemId);
    if (!entry) throw new Error(`Unknown Workspace Hook review item: ${reviewItemId}`);
    return entry.hookDeclarationDigest;
  });
}

export function toWorkspaceHookReviewTarget(
  request: WorkspaceHookReviewRequestPayload,
): WorkspaceHookReviewTarget {
  return {
    sessionId: request.sessionId,
    taskId: request.taskId,
    runId: request.runId,
    ...(request.remoteSessionId ? { remoteSessionId: request.remoteSessionId } : {}),
    workspaceIdentity: request.workspaceIdentity,
    bundleDigest: request.bundleDigest,
    reviewFlowId: request.reviewFlowId,
    generation: request.generation,
    interactionId: request.interactionId,
  };
}
