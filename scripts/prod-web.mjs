#!/usr/bin/env node
// Production entry for the Rust-native ZCode app: a release zcode-server that also
// serves the built frontend bundle. One process, no Node server, no Vite: /api, /ws,
// the channel RPC registry and the SPA static assets all come from zcode-server.
import { join } from "node:path";
import { stat } from "node:fs/promises";
import { spawn } from "node:child_process";
import {
  backendBinaryPath,
  buildBackend,
  fail,
  isNativeStale,
  log,
  parseFlags,
  repoRoot,
  startBackend,
  run,
  viteEntry,
  waitForHttp,
  webPackageRoot,
} from "./rust-backend.mjs";

const configuration = "release";

await main();

async function main() {
  const flags = parseFlags(process.argv.slice(2));
  const port = readPort(flags.port, 3030, "--port");
  const requestedHost = typeof flags.host === "string" ? flags.host : "127.0.0.1";
  // "localhost" can resolve to ::1 while the probes below target the IPv4 loopback.
  const host = requestedHost === "localhost" ? "127.0.0.1" : requestedHost;
  const workspace = await resolveWorkspace(flags.workspace);
  const staticDir =
    typeof flags["static-dir"] === "string" ? flags["static-dir"] : join(webPackageRoot, "dist");

  if (!process.env.ZCODE_ENV) process.env.ZCODE_ENV = "production";
  if (typeof flags["data-dir"] === "string") process.env.ZCODE_DATA_BASE_DIR = flags["data-dir"];
  // The backend reads the token from the environment: a command-line secret is
  // readable by any process through the Windows process list.
  const token = (
    typeof flags.token === "string" ? flags.token : (process.env.ZCODE_SERVER_AUTH_TOKEN ?? "")
  ).trim();
  if (token) process.env.ZCODE_SERVER_AUTH_TOKEN = token;
  const query = token ? `?token=${encodeURIComponent(token)}` : "";
  if (!token && !isLoopback(host)) {
    throw new Error(`binding ${host} exposes the app to the network; pass --token`);
  }

  log(`production mode: release Rust backend serving ${staticDir}, ZCODE_ENV=${process.env.ZCODE_ENV}`);
  if (!flags["no-build"]) {
    if (flags.rebuild || (await isNativeStale(configuration))) await buildBackend(configuration);
    else log(`reusing ${backendBinaryPath(configuration)}`);
  }
  await assertBinary();

  if (!flags["no-build"] || !(await stat(join(staticDir, "index.html")).catch(() => null))) {
    const exitCode = await run(process.execPath, [viteEntry, "build"], webPackageRoot);
    if (exitCode !== 0) throw new Error(`vite build failed with exit code ${exitCode}`);
  }
  const index = await stat(join(staticDir, "index.html")).catch(() => null);
  if (!index?.isFile()) throw new Error(`web bundle missing: ${join(staticDir, "index.html")}`);

  const backendArgs = [
    "--host",
    host,
    "--port",
    String(port),
    "--workspace",
    workspace,
    "--static-dir",
    staticDir,
    "--spa-fallback",
    "true",
  ];
  const backend = startBackend(configuration, backendArgs);
  let stopping = false;
  function shutdown(reason) {
    if (stopping) return;
    stopping = true;
    if (reason) {
      fail(reason);
      process.exitCode = 1;
    }
    backend.kill("SIGTERM");
  }
  process.on("SIGINT", () => shutdown());
  process.on("SIGTERM", () => shutdown());
  backend.on("error", (error) => shutdown(`zcode-server could not start: ${error.message}`));
  backend.on("exit", (code, signal) => {
    if (!stopping) shutdown(`zcode-server exited (${signal ?? `code ${code}`})`);
  });

  const probeHost = isLoopback(host) ? "127.0.0.1" : host;
  const base = `http://${probeHost}:${port}/`;
  try {
    await waitForHttp(`${base}healthz${query}`, {
      timeoutMs: 60_000,
      validate: (body) => {
        const info = JSON.parse(body);
        if (info.status !== "ok") return `healthz reported status "${info.status}"`;
        if (!(info.channels > 0)) return "healthz reported zero registered service channels";
        return undefined;
      },
    });
    log("healthz ok (release backend, real channel registry)");
    await waitForHttp(`${base}api/server-info${query}`, {
      validate: (body) =>
        JSON.parse(body).protocolVersion === 1
          ? undefined
          : "server-info is not the v1 remote contract",
    });
    const document = await waitForHttp(`${base}${query}`, {
      validate: (body) => {
        if (!body.includes("<html")) return "the backend did not serve index.html";
        if (!body.includes("/assets/")) return "index.html has no hashed asset graph - not a production bundle";
        return undefined;
      },
    });
    const entryAsset = /"\/(assets\/[^"]+\.js)"/.exec(document)?.[1];
    if (!entryAsset) throw new Error("index.html does not reference a hashed JS entry asset");
    await waitForHttp(`${base}${entryAsset}${query}`, { timeoutMs: 30_000 });
    log(`serving the built bundle (${entryAsset})`);
  } catch (error) {
    shutdown(error.message);
    return;
  }

  const openUrl = `${base}${query}`;
  log(`app  ${openUrl}`);
  log(`backend pid ${backend.pid}; Ctrl+C stops it`);
  if (flags.open) openBrowser(openUrl);

  while (!stopping) await new Promise((sleep) => setTimeout(sleep, 500));
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

function isLoopback(host) {
  return host === "127.0.0.1" || host === "::1";
}

function openBrowser(url) {
  const command =
    process.platform === "darwin" ? "open" : process.platform === "win32" ? "cmd" : "xdg-open";
  const args = process.platform === "win32" ? ["/c", "start", "", url] : [url];
  spawn(command, args, { detached: true, stdio: "ignore" }).unref();
}

function readPort(value, fallback, name) {
  if (value === undefined) return fallback;
  const port = Number(value);
  if (!Number.isInteger(port) || port < 1 || port > 65_535) {
    throw new Error(`${name} must be an integer between 1 and 65535`);
  }
  return port;
}
