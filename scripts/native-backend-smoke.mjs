#!/usr/bin/env node
// Native backend smoke test: boot the real `zcode-server` binary, then prove the shipped
// HTTP + WebSocket surface answers a real client (no test harness, no fixtures).
//
// Usage (repo root, after `cargo build -p nc-code-server`):
//   node scripts/native-backend-smoke.mjs [--release]
//
// Exit code 0 = server booted, `/healthz`, `/api/server-info` and the `/ws` upgrade all
// answered. Any mismatch prints the observed payload and exits 1 (fail closed).

import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const release = process.argv.includes("--release");
const binary = join(
  repoRoot,
  "native",
  "target",
  release ? "release" : "debug",
  process.platform === "win32" ? "zcode-server.exe" : "zcode-server",
);

function fail(message, detail) {
  console.error(`[native-smoke] FAILED: ${message}`);
  if (detail !== undefined) console.error(detail);
  process.exit(1);
}

function log(message) {
  console.log(`[native-smoke] ${message}`);
}

/** Read `LISTENING <addr>` from stdout with a bounded wait. */
function waitForListening(child) {
  return new Promise((resolvePromise, reject) => {
    let buffer = "";
    const timer = setTimeout(
      () => reject(new Error(`server did not print LISTENING within 60s\n${buffer}`)),
      60_000,
    );
    const onData = (chunk) => {
      buffer += chunk.toString();
      const match = /LISTENING (\S+)/u.exec(buffer);
      if (match) {
        clearTimeout(timer);
        child.stdout.off("data", onData);
        resolvePromise(match[1]);
      }
    };
    child.stdout.on("data", onData);
    child.once("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`server exited early with code ${code}\n${buffer}`));
    });
  });
}

/** Open `/ws` and wait for the server's first frame (the protocol Initialize). */
function expectWebSocketUpgrade(address) {
  return new Promise((resolvePromise, reject) => {
    const url = `ws://${address}/ws`;
    const socket = new WebSocket(url);
    const timer = setTimeout(() => {
      socket.close();
      reject(new Error(`no frame received on ${url} within 15s`));
    }, 15_000);
    socket.binaryType = "arraybuffer";
    socket.onmessage = (event) => {
      clearTimeout(timer);
      const byteLength = event.data instanceof ArrayBuffer ? event.data.byteLength : 0;
      socket.close();
      if (byteLength < 13) {
        reject(new Error(`initialize frame too short (${byteLength} bytes)`));
        return;
      }
      resolvePromise(byteLength);
    };
    socket.onerror = () => {
      clearTimeout(timer);
      reject(new Error(`websocket handshake failed for ${url}`));
    };
  });
}

const workspace = await mkdtemp(join(tmpdir(), "nc-backend-smoke-ws-"));
const dataDir = await mkdtemp(join(tmpdir(), "nc-backend-smoke-data-"));
const child = spawn(binary, ["--workspace", workspace, "--data-dir", dataDir, "--port", "0"], {
  cwd: repoRoot,
  stdio: ["ignore", "pipe", "pipe"],
});
let stderr = "";
child.stderr.on("data", (chunk) => {
  stderr += chunk.toString();
});

let exitCode = 0;
try {
  const address = await waitForListening(child);
  log(`listening on ${address}`);

  const health = await fetch(`http://${address}/healthz`);
  const healthBody = await health.json();
  if (!health.ok || healthBody.status !== "ok") {
    fail(`/healthz unexpected`, { status: health.status, body: healthBody });
  }
  log(`/healthz -> ${health.status} ${JSON.stringify(healthBody)}`);

  const info = await fetch(`http://${address}/api/server-info`);
  const infoBody = await info.json();
  const workspaces = Array.isArray(infoBody.workspaces) ? infoBody.workspaces : [];
  if (!info.ok || workspaces.length !== 1) {
    fail(`/api/server-info unexpected`, { status: info.status, body: infoBody });
  }
  const advertised = workspaces[0];
  if (advertised.path !== workspace && advertised.workspacePath !== workspace) {
    fail(`/api/server-info advertised the wrong workspace`, advertised);
  }
  log(`/api/server-info -> ${info.status} workspace=${workspace}`);

  const unknown = await fetch(`http://${address}/api/does-not-exist`);
  const unknownBody = await unknown.json();
  if (unknown.status !== 404 || unknownBody.error !== "not_found") {
    fail(`unknown /api must be JSON 404`, { status: unknown.status, body: unknownBody });
  }
  log(`/api/does-not-exist -> 404 JSON`);

  const frameBytes = await expectWebSocketUpgrade(address);
  log(`/ws -> initialize frame ${frameBytes} bytes`);

  log("PASS");
} catch (error) {
  fail(error instanceof Error ? error.message : String(error), stderr.trim() || undefined);
  exitCode = 1;
} finally {
  child.kill();
  await new Promise((resolvePromise) => child.once("exit", resolvePromise));
  await rm(workspace, { recursive: true, force: true });
  await rm(dataDir, { recursive: true, force: true });
}
process.exit(exitCode);
