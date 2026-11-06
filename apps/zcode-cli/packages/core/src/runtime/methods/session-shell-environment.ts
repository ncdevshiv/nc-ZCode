import { countContextPrefixMessages } from "../deps.js";
import type { EnvInfo, ExecutionShellSelection, TraceContext } from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";
import type { AgentRuntimeConfig } from "../types.js";
import {
  persistBashShellSelectionSnapshot,
  readPersistedBashShellSelectionSnapshot,
  resolveBashShellSnapshotForResume,
  type BashShellSnapshotRestore,
} from "./bash-shell-snapshot.js";
import { rebuildContextPrefix } from "./context-refresh.js";
import {
  buildShellEnvironmentResumeNotice,
  getShellEnvironmentResumeNoticeKind,
} from "./shell-environment.js";
import { refreshBranchAwareBuiltInTools } from "./embedded-search-branch.js";

type SessionShellConfig = Pick<AgentRuntimeConfig, "bashShellSelection">;

export type SessionShellEnvironmentCandidate =
  | ExecutionShellSelection
  | (() => ExecutionShellSelection);

interface SessionShellEnvironment {
  selection: ExecutionShellSelection;
  promptShell: string;
}

export function getSessionShellSelectionFromConfig(
  config: SessionShellConfig,
): ExecutionShellSelection | undefined {
  return config.bashShellSelection;
}

export function getSessionShellEnvironment(
  runtime: AgentRuntimeInternal,
): SessionShellEnvironment | undefined {
  const selection = getSessionShellSelectionFromConfig(runtime.config);
  if (!selection) return undefined;
  return {
    promptShell: selection.display.name,
    selection,
  };
}

export function getSessionShellSelection(
  runtime: AgentRuntimeInternal,
): ExecutionShellSelection | undefined {
  return getSessionShellEnvironment(runtime)?.selection;
}

export function getContextSourceShellDisplayName(
  runtime: AgentRuntimeInternal,
): string | undefined {
  return getSessionShellEnvironment(runtime)?.promptShell;
}

export function initializeSessionShellEnvironmentIfNeeded(
  runtime: AgentRuntimeInternal,
  candidate: SessionShellEnvironmentCandidate,
): boolean {
  if (getSessionShellEnvironment(runtime)) {
    return false;
  }

// Bash shell is a session-start snapshot. All entries only express the "current candidate value",
// The runtime is uniformly responsible for initializing before the first real user execution. candidate can be a lazy resolver,
// In this way, sessions that already have snapshots will not repeatedly detect the shell in the outer layer.
  applySessionShellEnvironment(runtime, resolveSessionShellCandidate(candidate), {
    refreshPreConversationContext: true,
  });
  return true;
}

function applySessionShellEnvironment(
  runtime: AgentRuntimeInternal,
  selection: ExecutionShellSelection | undefined,
  options: { refreshPreConversationContext?: boolean } = {},
): void {
  runtime.config.bashShellSelection = selection;
  refreshBranchAwareBuiltInTools(runtime);

  if (options.refreshPreConversationContext !== false) {
    refreshPreConversationShellContext(runtime, selection);
  }
}

function resolveSessionShellCandidate(
  candidate: SessionShellEnvironmentCandidate,
): ExecutionShellSelection {
  return typeof candidate === "function" ? candidate() : candidate;
}

function applySessionShellToEnvInfo<T extends { shell?: string }>(
  envInfo: T,
  selection: ExecutionShellSelection | undefined,
): T;
function applySessionShellToEnvInfo<T extends { shell?: string }>(
  envInfo: T | undefined,
  selection: ExecutionShellSelection | undefined,
): T | undefined;
function applySessionShellToEnvInfo<T extends { shell?: string }>(
  envInfo: T | undefined,
  selection: ExecutionShellSelection | undefined,
): T | undefined {
  if (!envInfo || !selection?.display.name) {
    return envInfo;
  }
  return {
    ...envInfo,
    shell: selection.display.name,
  };
}

export async function persistSessionShellEnvironmentSnapshot(
  runtime: AgentRuntimeInternal,
  traceContext: TraceContext,
): Promise<void> {
  await persistBashShellSelectionSnapshot({
    logger: runtime.logger,
    selection: getSessionShellSelection(runtime),
    sessionId: runtime.sessionId,
    sessionStore: runtime.sessionStore,
    traceContext,
  });
}

export async function restoreSessionShellEnvironmentSelectionForResume(
  runtime: AgentRuntimeInternal,
  options: {
    currentSelection: ExecutionShellSelection | undefined;
    traceContext: TraceContext;
  },
): Promise<BashShellSnapshotRestore> {
  const restore = resolveBashShellSnapshotForResume({
    currentSelection: options.currentSelection,
    logger: runtime.logger,
    restore: await readPersistedBashShellSelectionSnapshot({
      logger: runtime.logger,
      sessionId: runtime.sessionId,
      sessionStore: runtime.sessionStore,
      traceContext: options.traceContext,
    }),
    traceContext: options.traceContext,
  });

  if (restore.status === "restored" || restore.status === "fallback") {
    applySessionShellEnvironment(runtime, restore.selection, {
      refreshPreConversationContext: false,
    });
  }

  return restore;
}

export function announceSessionShellEnvironmentNoticeAfterResume(
  runtime: AgentRuntimeInternal,
  options: {
    persistedEnvInfo: EnvInfo | undefined;
    restore: BashShellSnapshotRestore;
  },
): void {
  const selection = getSessionShellSelection(runtime);
  const noticeKind = getShellEnvironmentResumeNoticeKind({
    persistedShell: options.persistedEnvInfo?.shell,
    restoreStatus: options.restore.status,
    selection,
  });
  if (!selection || !noticeKind) {
    return;
  }

  const notice = buildShellEnvironmentResumeNotice(noticeKind, selection);
  if (hasShellEnvironmentChangeAttachment(runtime, notice)) {
    return;
  }

// When the old Windows session does not have a shell snapshot available, it may be caused by auto Git Bash after the upgrade
// Take over Bash execution. Historical context may still cause the model to continue to use old shell habits, so it must be
// Add a provider-visible shell reminder after resume; you can use snapshot to restore without inserting it to avoid
// Break the contract that "shell setting changes only take effect in new sessions".
  runtime.messageHistory.addAttachment("shell_environment_change", notice);
}

function hasShellEnvironmentChangeAttachment(
  runtime: AgentRuntimeInternal,
  content: string,
): boolean {
  return runtime.messageHistory
    .borrowReadOnlyRuntimeEntries()
    .some(
      (entry) =>
        entry.kind === "attachment" &&
        entry.metadata?.source === "shell_environment_change" &&
        entry.content === content,
    );
}

function refreshPreConversationShellContext(
  runtime: AgentRuntimeInternal,
  selection: ExecutionShellSelection | undefined,
): void {
  if (
    !selection ||
    !runtime.contextBuilder ||
    !runtime.contextInitialized ||
    !runtime.contextSourceSnapshot ||
    runtime.sessionPersisted
  ) {
    return;
  }
  const activeEntries = runtime.messageHistory.borrowReadOnlyRuntimeEntries();
  if (activeEntries.length !== countContextPrefixMessages(activeEntries)) {
    return;
  }

// The deferred draft is in a hidden preheating state, and it is not yet real when the shell is refreshed before the first release.
// conversation message. At this time, session-start # Environment should be refreshed together.
// Avoid inconsistencies between the shell seen by the model and the actual execution shell of Bash.
  runtime.config.envInfo = applySessionShellToEnvInfo(runtime.config.envInfo, selection);
  runtime.contextSourceSnapshot = {
    ...runtime.contextSourceSnapshot,
    envInfo: applySessionShellToEnvInfo(runtime.contextSourceSnapshot.envInfo, selection),
  };
  rebuildContextPrefix(runtime);
}
