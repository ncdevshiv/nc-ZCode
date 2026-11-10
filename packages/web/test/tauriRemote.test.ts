/**
 * Tauri remote-workspace transport (Rust `/api/remote/*`).
 *
 * Run from `packages/web`:
 *   node --import tsx --test test/tauriRemote.test.ts
 *
 * The fake `fetch` here is a test double: product code contains no mock, and every
 * assertion checks the request the renderer really issues and how a real server reply
 * (including its error envelope) is interpreted.
 */
import assert from "node:assert/strict";
import test from "node:test";
import type { RemoteTarget } from "@zcode/shared";
import {
  buildRemoteConnectBody,
  createRemoteWorkspaceClient,
  parseRemoteConnectResponse,
} from "../src/tauriRemote.js";

interface Captured {
  url: string;
  init?: RequestInit;
}

function fakeFetch(reply: { status: number; body: string }) {
  const calls: Captured[] = [];
  const fetchImpl = async (url: string, init?: RequestInit) => {
    calls.push({ url, init });
    return {
      ok: reply.status >= 200 && reply.status < 300,
      status: reply.status,
      text: async () => reply.body,
    };
  };
  return { calls, fetchImpl };
}

const sshTarget: RemoteTarget = {
  kind: "ssh",
  host: "build.example.com",
  username: "dev",
  port: 2222,
};

const context = { workspacePath: "/srv/app", workspaceIdentity: "/srv/app" };

test("connect body carries the workspace target and identity", () => {
  const body = buildRemoteConnectBody(sshTarget, context);
  assert.deepEqual(body, {
    workspacePath: "/srv/app",
    workspaceIdentity: "/srv/app",
    target: sshTarget,
  });
  // No identity supplied: the key is omitted rather than sent as null.
  const noIdentity = buildRemoteConnectBody(sshTarget, { workspacePath: "/srv/app" });
  assert.equal("workspaceIdentity" in noIdentity, false);
});

test("a real registry reply becomes a session id", () => {
  const result = parseRemoteConnectResponse(
    200,
    JSON.stringify({
      remoteSessionId: "remote-session-1",
      generation: 1,
      workspacePath: "/srv/app",
      workspaceIdentity: "/srv/app",
    }),
  );
  assert.deepEqual(result, { success: true, sessionId: "remote-session-1" });
});

test("a 2xx without a session id is not a success", () => {
  const result = parseRemoteConnectResponse(200, JSON.stringify({ generation: 1 }));
  assert.deepEqual(result, {
    success: false,
    error: "Remote connect returned no remoteSessionId",
  });
});

test("server errors surface verbatim", () => {
  const unsupported = parseRemoteConnectResponse(
    400,
    JSON.stringify({
      error:
        "unsupported remote target kind 'wsl' (wsl/docker are host-side bind transports; register via /api/remote/bind)",
    }),
  );
  assert.equal(unsupported.success, false);
  assert.match(unsupported.error ?? "", /unsupported remote target kind/);

  const gateway = parseRemoteConnectResponse(
    502,
    JSON.stringify({ error: "ssh remote connect failed: exit 255" }),
  );
  assert.match(gateway.error ?? "", /ssh remote connect failed/);

  // A non-JSON failure is reported with the status, never silently accepted.
  const nonJson = parseRemoteConnectResponse(500, "<html>oops</html>");
  assert.equal(nonJson.success, false);
  assert.match(nonJson.error ?? "", /non-JSON \(HTTP 500\)/);
});

test("connectRemote POSTs to the Rust route and returns the server verdict", async () => {
  const { calls, fetchImpl } = fakeFetch({
    status: 200,
    body: JSON.stringify({ remoteSessionId: "remote-session-9" }),
  });
  const client = createRemoteWorkspaceClient(fetchImpl, "http://127.0.0.1:43117");
  const result = await client.connectRemote(sshTarget, "req-1", context);

  assert.deepEqual(result, { success: true, sessionId: "remote-session-9" });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, "http://127.0.0.1:43117/api/remote/connect");
  assert.equal(calls[0].init?.method, "POST");
  assert.deepEqual(JSON.parse(String(calls[0].init?.body)), {
    workspacePath: "/srv/app",
    workspaceIdentity: "/srv/app",
    target: sshTarget,
  });
});

test("connectRemote without a workspace path fails before any request", async () => {
  const { calls, fetchImpl } = fakeFetch({ status: 200, body: "{}" });
  const client = createRemoteWorkspaceClient(fetchImpl);
  const result = await client.connectRemote(sshTarget, undefined, { workspacePath: "" });
  assert.deepEqual(result, {
    success: false,
    error: "Remote connect requires a workspacePath",
  });
  assert.equal(calls.length, 0);
});

test("bind and dispose hit their routes and throw the server error", async () => {
  const okFetch = fakeFetch({ status: 200, body: JSON.stringify({ generation: 2 }) });
  const okClient = createRemoteWorkspaceClient(okFetch.fetchImpl);
  await okClient.bindRemoteWorkspaceSessionContext({
    remoteSessionId: "remote-session-1",
    workspacePath: "/srv/app",
    workspaceIdentity: "/srv/app",
  });
  assert.equal(okFetch.calls[0].url, "/api/remote/bind");
  assert.deepEqual(JSON.parse(String(okFetch.calls[0].init?.body)), {
    remoteSessionId: "remote-session-1",
    workspacePath: "/srv/app",
    workspaceIdentity: "/srv/app",
  });

  await okClient.disposeRemoteSession("remote-session-1");
  assert.equal(okFetch.calls[1].url, "/api/remote/dispose");
  assert.deepEqual(JSON.parse(String(okFetch.calls[1].init?.body)), {
    remoteSessionId: "remote-session-1",
  });

  const failing = fakeFetch({
    status: 404,
    body: JSON.stringify({ error: "unknown remote session" }),
  });
  const failingClient = createRemoteWorkspaceClient(failing.fetchImpl);
  await assert.rejects(
    () =>
      failingClient.bindRemoteWorkspaceSessionContext({
        remoteSessionId: "missing",
        workspacePath: "/srv/app",
      }),
    /unknown remote session/,
  );
});

test("cancelPendingRemoteConnection aborts the in-flight connect", async () => {
  let signal: AbortSignal | undefined;
  const fetchImpl = (async (_url: string, init?: RequestInit) => {
    signal = init?.signal ?? undefined;
    return await new Promise<never>((_resolve, reject) => {
      signal?.addEventListener("abort", () => {
        const error = new Error("aborted");
        error.name = "AbortError";
        reject(error);
      });
    });
  }) as Parameters<typeof createRemoteWorkspaceClient>[0];
  const client = createRemoteWorkspaceClient(fetchImpl);

  const pending = client.connectRemote(sshTarget, "req-cancel", context);
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(signal?.aborted, false);

  await client.cancelPendingRemoteConnection("req-cancel");
  assert.equal(signal?.aborted, true);

  const result = await pending;
  assert.equal(result.success, false);
  assert.match(result.error ?? "", /aborted/);
});
