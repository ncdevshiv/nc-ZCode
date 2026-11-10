/**
 * Tauri remote-workspace transport.
 *
 * The embedded Rust server (`nc-code-server`) owns the remote-session registry and exposes
 * it over `/api/remote/*` (`native/crates/nc-code-server/src/remote_api.rs`). The Tauri
 * renderer talks to those routes directly — same origin, no Electron bridge, no TS host.
 *
 * Every failure is surfaced with the server's own message; nothing here fabricates a
 * connected session.
 */
import type { IPlatformService } from "@zcode/shared";
import type { RemoteTarget } from "@zcode/shared";

type ConnectRemoteArgs = Parameters<IPlatformService["connectRemote"]>;
type ConnectRemoteContext = NonNullable<ConnectRemoteArgs[2]>;

export interface RemoteConnectResult {
  success: boolean;
  error?: string;
  sessionId?: string;
}

interface FetchLike {
  (input: string, init?: RequestInit): Promise<{
    ok: boolean;
    status: number;
    text(): Promise<string>;
  }>;
}

/** SSH targets may carry the remote `zcode-server` port used by the tunnel. */
export interface RemoteTargetWithServerPort extends Partial<{
  remotePort: number;
}> {}

function errorMessage(body: unknown, status: number): string {
  if (body && typeof body === "object" && "error" in body) {
    const value = (body as { error?: unknown }).error;
    if (typeof value === "string" && value.trim()) return value;
  }
  return `Remote connect failed (HTTP ${status})`;
}

/** Request body for `POST /api/remote/connect` (server contract, camelCase). */
export function buildRemoteConnectBody(
  target: RemoteTarget,
  context: ConnectRemoteContext,
): Record<string, unknown> {
  const body: Record<string, unknown> = {
    workspacePath: context.workspacePath,
    target: target as unknown as Record<string, unknown>,
  };
  if (context.workspaceIdentity) body.workspaceIdentity = context.workspaceIdentity;
  return body;
}

/**
 * Interpret the server's reply. A 2xx without `remoteSessionId` is not a success: the
 * registry is the single source of truth for the session id.
 */
export function parseRemoteConnectResponse(status: number, text: string): RemoteConnectResult {
  let body: unknown = null;
  if (text.trim()) {
    try {
      body = JSON.parse(text);
    } catch {
      return {
        success: false,
        error: `Remote connect returned non-JSON (HTTP ${status})`,
      };
    }
  }
  if (status < 200 || status >= 300) {
    return { success: false, error: errorMessage(body, status) };
  }
  const sessionId =
    body && typeof body === "object" && "remoteSessionId" in body
      ? (body as { remoteSessionId?: unknown }).remoteSessionId
      : undefined;
  if (typeof sessionId !== "string" || !sessionId) {
    return {
      success: false,
      error: "Remote connect returned no remoteSessionId",
    };
  }
  return { success: true, sessionId };
}

export interface RemoteWorkspaceClient {
  connectRemote(
    target: RemoteTarget,
    requestId: string | undefined,
    context: ConnectRemoteContext,
  ): Promise<RemoteConnectResult>;
  cancelPendingRemoteConnection(requestId?: string): Promise<void>;
  bindRemoteWorkspaceSessionContext(context: {
    remoteSessionId: string;
    workspacePath: string;
    workspaceIdentity?: string;
  }): Promise<void>;
  disposeRemoteSession(sessionId: string): Promise<void>;
}

/**
 * Fetch-backed client for the Rust remote endpoints. `requestId` scopes the abort
 * controller so cancelling one pending connect never touches another.
 */
export function createRemoteWorkspaceClient(
  fetchImpl: FetchLike,
  baseUrl = "",
): RemoteWorkspaceClient {
  const pending = new Map<string, AbortController>();

  const post = async (
    path: string,
    body: unknown,
    requestId?: string,
  ): Promise<{ status: number; text: string }> => {
    const controller = new AbortController();
    if (requestId) pending.set(requestId, controller);
    try {
      const response = await fetchImpl(`${baseUrl}${path}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
        signal: controller.signal,
      });
      return { status: response.status, text: await response.text() };
    } finally {
      if (requestId) pending.delete(requestId);
    }
  };

  return {
    async connectRemote(target, requestId, context) {
      if (!context?.workspacePath) {
        return { success: false, error: "Remote connect requires a workspacePath" };
      }
      try {
        const { status, text } = await post(
          "/api/remote/connect",
          buildRemoteConnectBody(target, context),
          requestId,
        );
        return parseRemoteConnectResponse(status, text);
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        return { success: false, error: `Remote connect failed: ${message}` };
      }
    },

    async cancelPendingRemoteConnection(requestId) {
      if (!requestId) return;
      const controller = pending.get(requestId);
      if (!controller) return;
      pending.delete(requestId);
      controller.abort();
    },

    async bindRemoteWorkspaceSessionContext(context) {
      const body: Record<string, unknown> = {
        remoteSessionId: context.remoteSessionId,
        workspacePath: context.workspacePath,
      };
      if (context.workspaceIdentity) body.workspaceIdentity = context.workspaceIdentity;
      const { status, text } = await post("/api/remote/bind", body);
      if (status < 200 || status >= 300) {
        let parsed: unknown = null;
        try {
          parsed = text ? JSON.parse(text) : null;
        } catch {
          parsed = null;
        }
        throw new Error(errorMessage(parsed, status));
      }
    },

    async disposeRemoteSession(sessionId) {
      const { status, text } = await post("/api/remote/dispose", {
        remoteSessionId: sessionId,
      });
      if (status < 200 || status >= 300) {
        let parsed: unknown = null;
        try {
          parsed = text ? JSON.parse(text) : null;
        } catch {
          parsed = null;
        }
        throw new Error(errorMessage(parsed, status));
      }
    },
  };
}
