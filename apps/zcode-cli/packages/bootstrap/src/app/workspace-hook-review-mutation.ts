import { basename, resolve } from "node:path";
import {
  createWorkspaceHookBundleSnapshot,
  type WorkspaceHookBundleSnapshot,
} from "@zcode/contracts";
import { digestSummary, workspaceIdentitySummary } from "@zcode/core";
import {
  buildWorkspaceHookBundleSnapshot,
  readWorkspaceHookProjectSources,
  type WorkspaceHookRuntimeRoot,
} from "@zcode/shared/workspace-hook-discovery";
import {
  WorkspaceHookMutationError,
  writeWorkspaceHookConfiguredToggle,
} from "@zcode/shared/workspace-hook-mutation";
import type { WorkspaceHookReviewMutationPort } from "./workspace-hook-review-controller.js";

interface WorkspaceHookReviewMutationPortOptions {
  workingDirectory: string;
  workspaceIdentity: string;
  projectConfigPath?: string;
  runtimeRoot: WorkspaceHookRuntimeRoot;
}

const workspaceMutationTails = new Map<string, Promise<void>>();

export function createWorkspaceHookReviewMutationPort(
  options: WorkspaceHookReviewMutationPortOptions,
): WorkspaceHookReviewMutationPort {
  const workingDirectory = resolve(options.workingDirectory);
  const editableConfigPath = resolve(workingDirectory, ".zcode", "config.json");
  const lockKey = editableConfigPath;

  return {
    toggle(input, onWriteCommitted) {
      return withWorkspaceMutationLock(lockKey, async () => {
        const current = await rebuildSnapshot(options);
// These three types of failures must be distinguished: shared workspace_hooks_snapshot_mismatch
// This will make it difficult for users to distinguish between "the workspace has been changed", "the configuration has really changed" and "the configuration cannot be read".
        if (current.workspaceIdentity !== input.snapshot.workspaceIdentity) {
          throw new WorkspaceHookMutationError(
            "workspace_hooks_snapshot_mismatch",
// The message will enter telemetry.errorMessage through the controller, so it is desensitized here:
// The identity itself is an absolute path, and reporting of the complete workspace path is prohibited.
            `Workspace Hook identity changed after review (expected ${workspaceIdentitySummary(
              input.snapshot.workspaceIdentity,
            )}, got ${workspaceIdentitySummary(current.workspaceIdentity)})`,
          );
        }
        if (current.bundleDigest !== input.snapshot.bundleDigest) {
          throw new WorkspaceHookMutationError(
            "workspace_hooks_bundle_changed",
            `Workspace Hook bundle changed after review (expected ${digestSummary(
              input.snapshot.bundleDigest,
            )}, got ${digestSummary(current.bundleDigest)})`,
          );
        }

        await writeWorkspaceHookConfiguredToggle({
          configPath: editableConfigPath,
          snapshot: current,
          reviewItemId: input.reviewItemId,
          enabled: input.enabled,
        });
        await onWriteCommitted();
        return rebuildSnapshot(options);
      });
    },
  };
}

async function rebuildSnapshot(
  options: WorkspaceHookReviewMutationPortOptions,
): Promise<WorkspaceHookBundleSnapshot> {
  const discovery = await readWorkspaceHookProjectSources({
    workingDirectory: options.workingDirectory,
    ...(options.projectConfigPath ? { explicitProjectConfigPath: options.projectConfigPath } : {}),
  });
  if (discovery.errors.length > 0) {
//Only report the file name, not the absolute path ("do not record source path");
// The complete path and original error are kept in cause, and the debug channel of logger is used instead of telemetry.
    const failed = discovery.errors[0]?.path;
    throw new WorkspaceHookMutationError(
      "workspace_hooks_config_unreadable",
      `Workspace Hook config could not be read: ${failed ? basename(failed) : "unknown"}`,
      { cause: discovery.errors[0]?.error },
    );
  }
  const snapshot = buildWorkspaceHookBundleSnapshot({
    workspaceIdentity: options.workspaceIdentity,
    workspacePath: options.workingDirectory,
    sources: discovery.sources,
    runtimeRoot: options.runtimeRoot,
  });
  if (!snapshot) {
    throw new WorkspaceHookMutationError(
      "workspace_hooks_snapshot_mismatch",
      "Workspace Hook bundle no longer exists",
    );
  }
  return createWorkspaceHookBundleSnapshot(snapshot);
}

async function withWorkspaceMutationLock<T>(key: string, operation: () => Promise<T>): Promise<T> {
  const previous = workspaceMutationTails.get(key) ?? Promise.resolve();
  let release!: () => void;
  const gate = new Promise<void>((resolveGate) => {
    release = resolveGate;
  });
  const tail = previous.catch(() => undefined).then(() => gate);
  workspaceMutationTails.set(key, tail);

  await previous.catch(() => undefined);
  try {
    return await operation();
  } finally {
    release();
    if (workspaceMutationTails.get(key) === tail) workspaceMutationTails.delete(key);
  }
}
