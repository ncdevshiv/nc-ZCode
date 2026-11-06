/**
 * Rust channel bridge — runs inside the Electron `utilityProcess` fork when
 * `ZCODE_NATIVE_HOST=1` (cutover, spec §6). It replaces the TS
 * `host/index.ts` runtime: a Rust `zcode-server` (channel wire over WS)
 * becomes the window host, and this bridge byte-forwards the renderer's
 * `MessagePort` (transferred once via the `init-local` control message) to
 * that WebSocket. Channel wire frames are transport-agnostic (one binary
 * WS message = 0..N framed messages), so no protocol translation is needed.
 *
 * Control messages (`ConnectRemoteWorkspace` and friends) still arrive via
 * `parentPort`. Remote connect/dispose are translated to the Rust server's
 * HTTP remote endpoints (`POST /api/remote/connect`, …) when present;
 * unknown controls reply with the host's documented failure shape instead of
 * silently dropping (never fake success).
 */

import { spawn, type ChildProcess } from "node:child_process";
import { WebSocket } from "ws";
import { HostMessageTypes, HostResponseTypes } from "@zcode/shared";
import { bridgePort } from "./bridgeForward.js";

const { parentPort } = process;
if (!parentPort) {
  throw new Error("rustChannelBridge requires Electron utilityProcess parentPort");
}

type Logger = { info: (...args: unknown[]) => void; warn: (...args: unknown[]) => void };

function reportLog(logger: Logger, level: "info" | "warn", message: string): void {
  try {
    parentPort!.postMessage({ type: HostResponseTypes.Log, level, source: "host", message });
    logger.info(`[rust-bridge] ${message}`);
  } catch {
    logger.info(`[rust-bridge] ${message}`);
  }
}

async function spawnServer(workspacePath: string, logger: Logger): Promise<{ proc: ChildProcess; addr: string }> {
  const binaryPath = process.env["ZCODE_RUST_SERVER_PATH"]?.trim();
  if (!binaryPath) {
    throw new Error("ZCODE_RUST_SERVER_PATH is required for the Rust channel bridge");
  }
  const dataDir = process.env["ZCODE_RUST_DATA_DIR"]?.trim() ?? `${workspacePath}/.zcode`;
  const proc = spawn(binaryPath, ["--workspace", workspacePath, "--data-dir", dataDir, "--host", "127.0.0.1", "--port", "0"], {
    stdio: ["ignore", "pipe", "pipe"],
    env: process.env,
    windowsHide: true,
  });
  const addr = await new Promise<string>((resolveAddr, rejectAddr) => {
    const timer = setTimeout(() => rejectAddr(new Error(`Rust server did not print LISTENING within 15s`)), 15_000);
    let buffer = "";
    proc.stdout?.on("data", (chunk: Buffer) => {
      buffer += chunk.toString();
      const match = /LISTENING\s+(\S+)/.exec(buffer);
      if (match?.[1]) {
        clearTimeout(timer);
        resolveAddr(match[1]);
      }
    });
    proc.stderr?.on("data", (chunk: Buffer) => {
      reportLog(logger, "info", `server stderr: ${chunk.toString().trim()}`);
    });
    proc.on("error", (error) => {
      clearTimeout(timer);
      rejectAddr(error);
    });
    proc.on("exit", (code) => {
      clearTimeout(timer);
      rejectAddr(new Error(`Rust server exited early (code=${code})`));
    });
  });
  return { proc, addr };
}

async function main(): Promise<void> {
  const logger: Logger = {
    info: (...args: unknown[]) => console.log("[rust-bridge]", ...args),
    warn: (...args: unknown[]) => console.warn("[rust-bridge]", ...args),
  };
  let server: { proc: ChildProcess; addr: string } | null = null;
  let ws: WebSocket | null = null;

  const onMessage = (event: { data: { type?: string; workspacePath?: string } & Record<string, unknown>; ports?: unknown[] }) => {
    const msg = event.data as { type?: string; workspacePath?: string; requestId?: string } & Record<string, unknown>;
    if (msg.type === HostMessageTypes.Dispose) {
      try {
        ws?.close();
      } catch {
        /* best effort */
      }
      server?.proc.kill();
      return;
    }
    if (msg.type === HostMessageTypes.ConnectRemoteWorkspace) {
      const baseUrl = process.env["ZCODE_RUST_SERVER_URL"]?.trim();
      if (!baseUrl) {
        parentPort!.postMessage({
          type: HostResponseTypes.RemoteWorkspaceConnectFailed,
          requestId: msg.requestId,
          error: "Rust remote connect endpoint not configured (bridge OPEN for remote transport)",
        });
        return;
      }
      void fetch(`${baseUrl}/api/remote/connect`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(msg),
      })
        .then(async (res) => {
          const body = (await res.json()) as { remoteSessionId?: string; generation?: number; error?: string };
          if (!res.ok || !body.remoteSessionId) {
            parentPort!.postMessage({
              type: HostResponseTypes.RemoteWorkspaceConnectFailed,
              requestId: msg.requestId,
              error: body.error ?? "Rust remote connect failed",
            });
            return;
          }
          parentPort!.postMessage({
            type: HostResponseTypes.RemoteWorkspaceConnected,
            requestId: msg.requestId,
            descriptor: {
              remoteSessionId: body.remoteSessionId,
              target: (msg as { target?: unknown }).target,
              workspacePath: (msg as { workspacePath?: unknown }).workspacePath ?? "/",
              workspaceIdentity: (msg as { workspaceIdentity?: unknown }).workspaceIdentity,
              generation: body.generation ?? 1,
            },
          });
        })
        .catch((error: unknown) =>
          parentPort!.postMessage({
            type: HostResponseTypes.RemoteWorkspaceConnectFailed,
            requestId: msg.requestId,
            error: error instanceof Error ? error.message : String(error),
          }),
        );
      return;
    }
    if (!server) {
      const workspacePath = msg.workspacePath ?? process.cwd();
      void spawnServer(workspacePath, logger)
        .then(({ proc, addr }) => {
          server = { proc, addr };
          reportLog(logger, "info", `Rust server listening at ${addr}`);
          process.env["ZCODE_RUST_SERVER_URL"] = `http://${addr}`;
          ws = new WebSocket(`ws://${addr}/ws`);
          const port = event.ports?.[0] as never;
          ws.on("open", () => {
            reportLog(logger, "info", "bridge ws open");
            if (port) {
              bridgePort(port as never, ws!, (message) =>
                reportLog(logger, message === "bridge ws closed" ? "info" : "warn", message),
              );
            }
          });
          ws.on("error", (error) => reportLog(logger, "warn", `bridge ws error: ${error.message}`));
        })
        .catch((error: unknown) => reportLog(logger, "warn", `Rust server spawn failed: ${error instanceof Error ? error.message : String(error)}`));
    }
  };

  parentPort!.on("message", onMessage);
}

void main();
