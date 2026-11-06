import {
  CoreErrorType,
  createCoreError,
  isCoreError,
  type RepairRemoteSessionPathsInput,
  type SessionInfo,
  type SessionStorePort,
} from "@zcode/contracts";
import { parseRemoteWorkspaceIdentity } from "@zcode/shared";

const REMOTE_SESSION_PATH_CORRUPTION_REASON = "remote_session_workspace_path_corrupted";

type PathFieldResolution = "clean" | "repair" | "unrelated" | "unsafe";

type RemoteSessionPathRepairStore = Pick<SessionStorePort, "getSession"> & {
  repairRemoteSessionPaths?: (input: RepairRemoteSessionPathsInput) => Promise<boolean>;
};

function buildKnownPollutedPath(workspacePath: string, workspaceIdentity: string): string {
  return workspacePath === "/" ? `/${workspaceIdentity}` : `${workspacePath}/${workspaceIdentity}`;
}

function resolvePathField(
  value: string,
  workspacePath: string,
  workspaceIdentity: string,
): PathFieldResolution {
  if (value === workspacePath) return "clean";
  if (
    value === workspaceIdentity ||
    value === buildKnownPollutedPath(workspacePath, workspaceIdentity)
  ) {
    return "repair";
  }
  return value.includes(workspaceIdentity) ? "unsafe" : "unrelated";
}

/**
* The old version treats workspaceIdentity as cwd, or appends it to the real workspacePath.
* Only two known and provable forms of pollution are fixed here; guessing is rejected for data that contains identity but does not match the prefix.
 */
export async function repairPersistedRemoteSessionPaths(
  sessionStore: RemoteSessionPathRepairStore,
  session: SessionInfo,
  options?: { onPersistenceFailure?: (error: unknown) => void },
): Promise<SessionInfo> {
  const workspaceIdentity = session.workspaceID?.trim();
  if (!workspaceIdentity) return session;
  const remoteWorkspace = parseRemoteWorkspaceIdentity(workspaceIdentity);
  if (!remoteWorkspace) {
    if (!workspaceIdentity.startsWith("remote:")) return session;
    throw createCoreError(
      CoreErrorType.SessionCorrupted,
      "Persisted remote session workspace identity is invalid",
      {
        context: {
          directory: session.directory,
          path: session.path,
          reason: REMOTE_SESSION_PATH_CORRUPTION_REASON,
          sessionId: session.id,
          workspaceIdentity,
        },
        recoverable: true,
      },
    );
  }

  const directoryResolution = resolvePathField(
    session.directory,
    remoteWorkspace.workspacePath,
    workspaceIdentity,
  );
  const pathResolution =
    session.path === undefined
      ? "clean"
      : resolvePathField(session.path, remoteWorkspace.workspacePath, workspaceIdentity);

  if (directoryResolution === "unsafe" || pathResolution === "unsafe") {
    throw createCoreError(
      CoreErrorType.SessionCorrupted,
      "Remote session workspace path is corrupted and cannot be repaired safely",
      {
        context: {
          directory: session.directory,
          path: session.path,
          reason: REMOTE_SESSION_PATH_CORRUPTION_REASON,
          sessionId: session.id,
          workspaceIdentity,
        },
        recoverable: true,
      },
    );
  }
  if (directoryResolution !== "repair" && pathResolution !== "repair") return session;

  const repairedSession: SessionInfo = {
    ...session,
    ...(directoryResolution === "repair" ? { directory: remoteWorkspace.workspacePath } : {}),
    ...(pathResolution === "repair" ? { path: remoteWorkspace.workspacePath } : {}),
  };
  const repairInput: RepairRemoteSessionPathsInput = {
    sessionID: session.id,
    workspaceID: workspaceIdentity as RepairRemoteSessionPathsInput["workspaceID"],
    expectedDirectory: session.directory,
    expectedPath: session.path ?? null,
    directory: repairedSession.directory,
    path: repairedSession.path ?? null,
    timeUpdated: session.time.updated,
  };
  if (!sessionStore.repairRemoteSessionPaths) {
    const error = new Error("Session store does not support narrow remote path repair");
    options?.onPersistenceFailure?.(error);
    return repairedSession;
  }
  try {
    const persisted = await sessionStore.repairRemoteSessionPaths(repairInput);
    const refreshed = await sessionStore.getSession(session.id);
    if (persisted) return refreshed ?? repairedSession;
    if (!refreshed) {
      throw createCoreError(
        CoreErrorType.SessionCorrupted,
        "Remote session disappeared during path repair",
        {
          context: {
            reason: REMOTE_SESSION_PATH_CORRUPTION_REASON,
            sessionId: session.id,
            workspaceIdentity,
          },
          recoverable: true,
        },
      );
    }

// A CAS miss indicates that the path or identity has been written concurrently; it must be re-adjudicated based on new facts, and the old snapshot cannot be returned.
    const refreshedIdentity = refreshed.workspaceID?.trim();
    if (refreshedIdentity !== workspaceIdentity) {
      throw createCoreError(
        CoreErrorType.SessionCorrupted,
        "Remote session workspace identity changed during path repair",
        {
          context: {
            directory: refreshed.directory,
            path: refreshed.path,
            reason: REMOTE_SESSION_PATH_CORRUPTION_REASON,
            sessionId: refreshed.id,
            workspaceIdentity: refreshedIdentity,
          },
          recoverable: true,
        },
      );
    }
    const refreshedDirectory = resolvePathField(
      refreshed.directory,
      remoteWorkspace.workspacePath,
      workspaceIdentity,
    );
    const refreshedPath =
      refreshed.path === undefined
        ? "clean"
        : resolvePathField(refreshed.path, remoteWorkspace.workspacePath, workspaceIdentity);
    if (refreshedDirectory === "unsafe" || refreshedPath === "unsafe") {
      throw createCoreError(
        CoreErrorType.SessionCorrupted,
        "Remote session workspace path changed to an unsafe value during repair",
        {
          context: {
            directory: refreshed.directory,
            path: refreshed.path,
            reason: REMOTE_SESSION_PATH_CORRUPTION_REASON,
            sessionId: refreshed.id,
            workspaceIdentity,
          },
          recoverable: true,
        },
      );
    }
    if (refreshedDirectory !== "repair" && refreshedPath !== "repair") return refreshed;

// When the adapter rejects CAS and the persistent value is still the same known taint form, only use the one constructed based on the latest metadata
// Deterministic memory repair; cold read will be retried next time without overwriting any concurrent session facts.
    options?.onPersistenceFailure?.(new Error("Remote session path repair CAS did not match"));
    return {
      ...refreshed,
      ...(refreshedDirectory === "repair" ? { directory: remoteWorkspace.workspacePath } : {}),
      ...(refreshedPath === "repair" ? { path: remoteWorkspace.workspacePath } : {}),
    };
  } catch (error) {
    if (isCoreError(error) && error.type === CoreErrorType.SessionCorrupted) {
      throw error;
    }
// When the path can be determined, temporary disk writing failure should not continue to block this recovery; the next read will still be retried.
    options?.onPersistenceFailure?.(error);
    return repairedSession;
  }
}
