import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { createDefaultFileWorkspaceHookTrustStore } from "@zcode/adapters/storage";
import { getDefaultConfigPath } from "@zcode/adapters/config";
import {
  DefaultRuntimeConfig,
  createWorkspaceHookBundleSnapshot,
  type WorkspaceHookBundleSnapshot,
  type WorkspaceHookTrustRecord,
} from "@zcode/contracts";
import {
  buildWorkspaceHookBundleSnapshot,
  readWorkspaceHookProjectSources,
  resolveWorkspaceHookRuntimeRoot,
  workspaceHooksConfigSchema,
  type WorkspaceHooksConfig,
} from "@zcode/shared/workspace-hook-discovery";

export interface WorkspaceHookTrustCliTarget {
  workspacePath: string;
  workspaceIdentity?: string;
  userConfigPath?: string;
}

export interface WorkspaceHookTrustCliItem {
  reviewItemId: string;
  event: string;
  matcher: string | null;
  displayCommand: string;
  sourcePath: string;
  configuredEnabled: boolean;
  hookDeclarationDigest: string;
  trustState: "trusted_persistent" | "pending_trust";
}

export interface WorkspaceHookTrustCliStatus {
  workspacePath: string;
  workspaceIdentity: string;
  bundleDigest: string | null;
  reasonCode:
    | "workspace_hooks_not_applicable"
    | "workspace_hooks_no_enabled_hooks"
    | "workspace_hooks_pending_trust"
    | "workspace_hooks_trusted_persistent"
    | "workspace_hooks_feature_disabled"
    | "workspace_hooks_require_trust_capable_host"
// status must be consistent with the failure semantics of grant/revoke - damaging the store if disguised as
//pending_trust, if the user presses the prompt to grant, it will only run into trust_store_corrupt, and the recovery guidance is inconsistent.
    | "workspace_hooks_trust_store_corrupt";
  items: WorkspaceHookTrustCliItem[];
}

export async function inspectWorkspaceHookTrust(
  input: WorkspaceHookTrustCliTarget,
): Promise<WorkspaceHookTrustCliStatus> {
  const target = normalizeTarget(input);
  const snapshot = await discoverSnapshot(target);
  const store = await createDefaultFileWorkspaceHookTrustStore({
    userConfigPath: target.userConfigPath,
  });
  const loaded = await store.load();
// Corrupting the store takes precedence over any trust calculations - all records are untrusted (adapters load has
// fail-closed returns empty records), explicitly reports corrupt to allow users to repair/remove damaged files first,
// No more "grant is enough" error guidance. Recovery: adapters load has renamed the damaged file
// *.corrupt-<ts>, confirm that it does not need to be retained and then delete the file or restore it from backup to rebuild.
  if (loaded.status === "corrupt") {
    const corruptItems = (snapshot?.hooks ?? []).map((entry) => ({
      reviewItemId: entry.reviewItemId,
      event: entry.event,
      matcher: entry.matcher,
      displayCommand: formatCommand(entry),
      sourcePath: entry.sourceRelativePath,
      configuredEnabled: entry.configuredEnabled,
      hookDeclarationDigest: entry.hookDeclarationDigest,
      trustState: "pending_trust" as const,
    }));
    return {
      workspacePath: target.workspacePath,
      workspaceIdentity: target.workspaceIdentity,
      bundleDigest: snapshot?.bundleDigest ?? null,
      reasonCode: "workspace_hooks_trust_store_corrupt",
      items: corruptItems,
    };
  }
  const trusted = new Set(
    loaded.records
      .filter((record) => record.workspaceIdentity === target.workspaceIdentity)
      .map((record) => record.hookDeclarationDigest),
  );
  if (!snapshot) {
    return {
      workspacePath: target.workspacePath,
      workspaceIdentity: target.workspaceIdentity,
      bundleDigest: null,
      reasonCode: "workspace_hooks_not_applicable",
      items: [],
    };
  }
  const items = snapshot.hooks.map((entry) => ({
    reviewItemId: entry.reviewItemId,
    event: entry.event,
    matcher: entry.matcher,
    displayCommand: formatCommand(entry),
    sourcePath: entry.sourceRelativePath,
    configuredEnabled: entry.configuredEnabled,
    hookDeclarationDigest: entry.hookDeclarationDigest,
    trustState: trusted.has(entry.hookDeclarationDigest)
      ? ("trusted_persistent" as const)
      : ("pending_trust" as const),
  }));
  const enabled = items.filter((item) => item.configuredEnabled);
  const reasonCode =
    enabled.length === 0
      ? "workspace_hooks_no_enabled_hooks"
      : enabled.every((item) => item.trustState === "trusted_persistent")
        ? "workspace_hooks_trusted_persistent"
        : "workspace_hooks_pending_trust";
  return {
    workspacePath: target.workspacePath,
    workspaceIdentity: target.workspaceIdentity,
    bundleDigest: snapshot.bundleDigest,
    reasonCode,
    items,
  };
}

export async function grantWorkspaceHookTrust(
  input: WorkspaceHookTrustCliTarget & {
    hookDeclarationDigests?: readonly string[];
    allCurrent?: boolean;
    bundleDigest?: string;
    appVersion?: string;
  },
): Promise<WorkspaceHookTrustCliStatus> {
  const target = normalizeTarget(input);
  const snapshot = await requireSnapshot(target);
  const selected = selectGrantEntries(snapshot, input);
  const grantedAt = new Date().toISOString();
  const records: WorkspaceHookTrustRecord[] = selected.map((entry) => ({
    workspaceIdentity: snapshot.workspaceIdentity,
    hookDeclarationDigest: entry.hookDeclarationDigest,
    digestAlgorithm: "sha256",
    decision: "trusted",
    grantedAt,
    bundleDigestAtGrant: snapshot.bundleDigest,
    eventAtGrant: entry.event,
    displayCommandAtGrant: formatCommand(entry),
    sourcePathAtGrant: entry.sourceRelativePath,
    sourceDiscoveryOrderAtGrant: snapshot.sourceFiles[entry.sourceFileIndex]?.discoveryOrder,
    matcherAtGrant: entry.matcher,
    matcherIndexAtGrant: entry.matcherIndex,
    hookIndexAtGrant: entry.hookIndex,
    ...(input.appVersion ? { appVersionAtGrant: input.appVersion } : {}),
  }));
  const store = await createDefaultFileWorkspaceHookTrustStore({
    userConfigPath: target.userConfigPath,
  });
// The workspace-level Settings pre-trust does not have a session controller to help intercept damaged storage;
// If granted directly, the store's recovery logic will record corrupt as empty and overwrite it immediately. Explicit authorization for the first time is required
// fail closed, let the user see the damage status first, and cannot disguise the recovery side effect as a successful authorization.
  const loaded = await store.load();
  if (loaded.status === "corrupt") {
    throw new Error("workspace_hooks_trust_store_corrupt");
  }
  await store.grant(records);
  return inspectWorkspaceHookTrust(target);
}

export async function revokeWorkspaceHookTrustCli(
  input: WorkspaceHookTrustCliTarget & {
    hookDeclarationDigests?: readonly string[];
    all?: boolean;
  },
): Promise<WorkspaceHookTrustCliStatus> {
  const target = normalizeTarget(input);
  const digests = unique(input.hookDeclarationDigests ?? []);
  if ((input.all === true) === digests.length > 0) {
    throw new Error("Specify exactly one of --all or --hook-digest.");
  }
  const store = await createDefaultFileWorkspaceHookTrustStore({
    userConfigPath: target.userConfigPath,
  });
  await store.revoke({
    workspaceIdentity: target.workspaceIdentity,
    ...(input.all ? {} : { hookDeclarationDigests: digests }),
  });
  return inspectWorkspaceHookTrust(target);
}

async function discoverSnapshot(
  target: Required<Pick<WorkspaceHookTrustCliTarget, "workspacePath">> &
    WorkspaceHookTrustCliTarget & {
      workspaceIdentity: string;
    },
): Promise<WorkspaceHookBundleSnapshot | undefined> {
  const discovery = await readWorkspaceHookProjectSources({
    workingDirectory: target.workspacePath,
  });
  if (discovery.errors.length > 0) {
    throw new Error(`Unable to read Workspace Hook config: ${discovery.errors[0]?.path}`);
  }
  const userHooks = await readUserHooks(target.userConfigPath ?? getDefaultConfigPath());
  const runtimeRoot = resolveWorkspaceHookRuntimeRoot([
    DefaultRuntimeConfig.hooks,
    userHooks,
    ...discovery.sources.map((source) => source.hooks),
  ]);
  const data = buildWorkspaceHookBundleSnapshot({
    workspaceIdentity: target.workspaceIdentity,
    workspacePath: target.workspacePath,
    sources: discovery.sources,
    runtimeRoot,
  });
  return data ? createWorkspaceHookBundleSnapshot(data) : undefined;
}

async function requireSnapshot(
  target: ReturnType<typeof normalizeTarget>,
): Promise<WorkspaceHookBundleSnapshot> {
  const snapshot = await discoverSnapshot(target);
  if (!snapshot) throw new Error("No Workspace Hook declarations were discovered.");
  return snapshot;
}

function selectGrantEntries(
  snapshot: WorkspaceHookBundleSnapshot,
  input: {
    hookDeclarationDigests?: readonly string[];
    allCurrent?: boolean;
    bundleDigest?: string;
  },
) {
  const digests = unique(input.hookDeclarationDigests ?? []);
  if ((input.allCurrent === true) === digests.length > 0) {
    throw new Error("Specify exactly one of --all-current or --hook-digest.");
  }
  if (input.bundleDigest && input.bundleDigest !== snapshot.bundleDigest) {
    throw new Error("workspace_hooks_bundle_changed");
  }
  if (input.allCurrent) {
    if (!input.bundleDigest) {
      throw new Error("workspace_hooks_bundle_changed");
    }
    return snapshot.hooks.filter((entry) => entry.configuredEnabled);
  }
  if (digests.length === 0) throw new Error("At least one --hook-digest is required.");
  const selected = snapshot.hooks.filter((entry) => digests.includes(entry.hookDeclarationDigest));
  if (new Set(selected.map((entry) => entry.hookDeclarationDigest)).size !== digests.length) {
    throw new Error("workspace_hooks_snapshot_mismatch");
  }
  return selected;
}

async function readUserHooks(path: string): Promise<WorkspaceHooksConfig | undefined> {
  try {
    const value = JSON.parse(await readFile(path, "utf8")) as unknown;
    if (!isRecord(value) || value.hooks === undefined) return undefined;
    return workspaceHooksConfigSchema.parse(value.hooks);
  } catch (error) {
    if (isNodeError(error, "ENOENT")) return undefined;
    throw error;
  }
}

function normalizeTarget(input: WorkspaceHookTrustCliTarget) {
  const workspacePath = resolve(input.workspacePath);
  return {
    workspacePath,
    workspaceIdentity: input.workspaceIdentity?.trim() || workspacePath,
    ...(input.userConfigPath ? { userConfigPath: input.userConfigPath } : {}),
  };
}

function formatCommand(entry: WorkspaceHookBundleSnapshot["hooks"][number]): string {
  return entry.type === "process" && entry.args?.length
    ? [entry.command, ...entry.args].join(" ")
    : entry.command;
}

function unique(values: readonly string[]): string[] {
  return [...new Set(values)];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isNodeError(error: unknown, code: string): error is NodeJS.ErrnoException {
  return error instanceof Error && "code" in error && error.code === code;
}
