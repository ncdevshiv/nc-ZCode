#!/usr/bin/env node
// Development entry for the Rust-native ZCode app: the zcode-server debug binary
// (agent runtime, services, session store and the automation scheduler in-process)
// plus the Vite dev server for frontend HMR. /api and /ws are proxied by Vite to the
// backend port (packages/web/vite.config.ts), so the browser needs no extra config.
//
// Backend hot restart: native sources are polled; when one is newer than the last
// build, cargo rebuilds and only the backend is restarted - Vite keeps its HMR state.
import { join } from "node:path";
import { stat } from "node:fs/promises";
import {
  backendBinaryPath,
  buildBackend,
  fail,
  isNativeStale,
  log,
  nativeSourceStamp,
  parseFlags,
  repoRoot,
  start,
  startBackend,
  viteEntry,
  waitForHttp,
  webPackageRoot,
} from "./rust-backend.mjs";

const configuration = "debug";

await main();

async function main() {
  const flags = parseFlags(process.argv.slice(2));
  const apiPort = readPort(flags["api-port"], 3030, "--api-port");
  const webPort = readPort(flags["web-port"], 5173, "--web-port");
  const workspace = await resolveWorkspace(flags.workspace);
  const noWatch = Boolean(flags["no-watch"]);

  if (!process.env.ZCODE_ENV) process.env.ZCODE_ENV = "test";
  if (typeof flags["data-dir"] === "string") process.env.ZCODE_DATA_BASE_DIR = flags["data-dir"];
  // The backend reads the token from the environment: a command-line secret is
  // readable by any process through the Windows process list.
  const token = (typeof flags.token === "string" ? flags.token : (process.env.ZCODE_SERVER_AUTH_TOKEN ?? "")).trim();
  if (token) process.env.ZCODE_SERVER_AUTH_TOKEN = token;

  const backendArgs = ["--host", "127.0.0.1", "--port", String(apiPort), "--workspace", workspace];
  const authTokenQuery = token ? `?token=${encodeURIComponent(token)}` : "";
  const healthUrl = `http://127.0.0.1:${apiPort}/healthz${authTokenQuery}`;

  log(`development mode: Rust backend (debug) + Vite HMR, ZCODE_ENV=${process.env.ZCODE_ENV}`);
  if (!flags["no-build"]) {
    if (flags.rebuild || (await isNativeStale(configuration))) await buildBackend(configuration);
    else log(`reusing ${backendBinaryPath(configuration)}`);
  }
  await assertBinary();

  let stopping = false;
  const children = new Set();
  function shutdown(reason) {
    if (stopping) return;
    stopping = true;
    if (reason) {
      fail(reason);
      process.exitCode = 1;
    }
    for (const child of children) child.kill("SIGTERM");
  }
  process.on("SIGINT", () => shutdown());
  process.on("SIGTERM", () => shutdown());

  function track(child, label) {
    children.add(child);
    child.on("error", (error) => shutdown(`${label} could not start: ${error.message}`));
    child.on("exit", () => {
      children.delete(child);
      if (!stopping) shutdown(`${label} exited (${child.signalCode ?? `code ${child.exitCode}`})`);
    });
    return child;
  }

  let backend = track(startBackend(configuration, backendArgs), "zcode-server");
  try {
    await waitForHttp(healthUrl, {
      timeoutMs: 60_000,
      validate: (body) => {
        const info = JSON.parse(body);
        if (info.status !== "ok") return `healthz reported status "${info.status}"`;
        if (!(info.channels > 0)) return "healthz reported zero registered service channels";
        return undefined;
      },
    });
  } catch (error) {
    shutdown(error.message);
    return;
  }
  log(`backend healthy on http://127.0.0.1:${apiPort}/ (${configuration})`);

  track(
    start(
      process.execPath,
      [viteEntry, "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"],
      webPackageRoot,
    ),
    "vite",
  );
  try {
    await waitForHttp(`http://127.0.0.1:${webPort}/`, {
      timeoutMs: 120_000,
      validate: (body) => (body.includes("<html") ? undefined : "Vite served a non-HTML document"),
    });
    await waitForHttp(`http://127.0.0.1:${webPort}/api/server-info${authTokenQuery}`, {
      validate: (body) =>
        JSON.parse(body).protocolVersion === 1
          ? undefined
          : "the Vite proxy did not reach the zcode-server server-info contract",
    });
  } catch (error) {
    shutdown(error.message);
    return;
  }
  log("Vite proxies /api and /ws to the Rust backend");

  const openUrl = `http://127.0.0.1:${webPort}/${authTokenQuery}`;
  log(`app     ${openUrl}`);
  log(`backend http://127.0.0.1:${apiPort}/ (healthz, /api/server-info, /ws)`);
  log(
    noWatch
      ? "backend hot restart disabled (--no-watch); frontend HMR stays on"
      : `backend hot restart watching ${join("native", "crates")}`,
  );

  let lastBuildStamp = Date.now();
  while (!stopping) {
    await new Promise((sleep) => setTimeout(sleep, 500));
    if (stopping || noWatch) continue;
    const newest = await nativeSourceStamp();
    if (newest <= lastBuildStamp) continue;
    await new Promise((sleep) => setTimeout(sleep, 800));
    if (stopping || (await nativeSourceStamp()) !== newest) continue;
    log("native source changed, rebuilding zcode-server");
    try {
      await buildBackend(configuration);
    } catch (error) {
      log(`build failed, the running backend is left untouched: ${error.message}`);
      lastBuildStamp = Date.now();
      continue;
    }
    const previous = backend;
    backend = track(startBackend(configuration, backendArgs), "zcode-server");
    previous.kill("SIGTERM");
    try {
      await waitForHttp(healthUrl, { timeoutMs: 120_000 });
    } catch (error) {
      shutdown(`backend did not come back after the restart: ${error.message}`);
      break;
    }
    lastBuildStamp = Date.now();
    log("backend restarted");
  }

  shutdown();
  await new Promise((sleep) => setTimeout(sleep, 300));
}

async function assertBinary() {
  const path = backendBinaryPath(configuration);
  if (!(await stat(path).catch(() => null))) {
    throw new Error(`backend binary missing: ${path} (rerun without --no-build)`);
  }
}

async function resolveWorkspace(value) {
  const path = value ?? repoRoot;
  const info = await stat(path).catch(() => null);
  if (!info?.isDirectory()) throw new Error(`workspace does not exist: ${path}`);
  return path;
}

function readPort(value, fallback, name) {
  if (value === undefined) return fallback;
  const port = Number(value);
  if (!Number.isInteger(port) || port < 1 || port > 65_535) {
    throw new Error(`${name} must be an integer between 1 and 65535`);
  }
  return port;
}
