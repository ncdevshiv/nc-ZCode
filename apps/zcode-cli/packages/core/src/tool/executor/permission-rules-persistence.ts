// Reading and persistence of project-level permission rules (detached from permission-flow.ts).
// Reason for splitting: permission-flow.ts introduced responder and exceeded the 400-line limit of a single file after racing;
// These three helpers are only cohesive with the project permission access of sessionStore and have nothing to do with the ask timing.
import {
  traceContextToLogContext,
  type PermissionRuleset,
  type PermissionUpdate,
  type ProjectId,
  type TraceContext,
} from "@zcode/contracts";
import { applyPermissionUpdates } from "./permission-rules.js";
import type { ToolExecutorDeps } from "./types.js";

export async function loadProjectPermissionRuleset(
  deps: ToolExecutorDeps,
): Promise<PermissionRuleset | null> {
  if (!deps.sessionStore) return null;
  const projectId = await resolveProjectId(deps);
  if (!projectId) return null;
  return deps.sessionStore.getProjectPermission(projectId);
}

export async function persistProjectPermissionUpdates(
  deps: ToolExecutorDeps,
  updates: PermissionUpdate[],
  traceContext: TraceContext,
): Promise<void> {
  if (updates.length === 0) return;

  if (!deps.sessionStore) {
    deps.logger?.warn("Project permission update skipped without session store", {
      ...traceContextToLogContext(traceContext),
      event: "tool.permission.project_update.skipped",
      module: "core.tool.executor",
      status: "completed",
    });
    return;
  }

  const projectID = await resolveProjectId(deps);
  if (!projectID) {
    deps.logger?.warn("Project permission update skipped without persisted session", {
      ...traceContextToLogContext(traceContext),
      event: "tool.permission.project_update.skipped",
      module: "core.tool.executor",
      status: "completed",
    });
    return;
  }

  const current = (await deps.sessionStore.getProjectPermission(projectID)) ?? { version: 1 };
  const next = applyPermissionUpdates(current, updates);
  await deps.sessionStore.saveProjectPermission({ projectID, permission: next });

  deps.logger?.info("Project permission updated", {
    ...traceContextToLogContext(traceContext),
    event: "tool.permission.project_update.saved",
    module: "core.tool.executor",
    status: "completed",
    updateCount: updates.length,
  });
}

async function resolveProjectId(deps: ToolExecutorDeps): Promise<ProjectId | undefined> {
  const session = await deps.sessionStore?.getSession(deps.sessionId);
  return session?.projectID;
}
