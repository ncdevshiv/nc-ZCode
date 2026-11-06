// Shared plumbing for the Rust-native app entries (dev-web.mjs / prod-web.mjs):
// locate the zcode-server binary, decide whether cargo must rebuild it, spawn
// processes with inherited stdio and wait until the backend really answers.
// One implementation so the Windows wrappers (startdev.ps1 / startprod.ps1) and
// `pnpm dev:web` drive the same backend.
import { spawn } from "node:child_process";
import { readdir, stat } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const webPackageRoot = join(repoRoot, "packages", "web");
export const viteEntry = join(repoRoot, "node_modules", "vite", "bin", "vite.js");

const BINARY_NAME = process.platform === "win32" ? "zcode-server.exe" : "zcode-server";

export function backendBinaryPath(configuration) {
  return join(repoRoot, "native", "target", configuration, BINARY_NAME);
}

async function newestSourceTimeUnder(directory) {
  let newest = 0;
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "target" || entry.name === "node_modules") continue;
      newest = Math.max(newest, await newestSourceTimeUnder(path));
      continue;
    }
    if (entry.name.endsWith(".rs") || entry.name.startsWith("Cargo.")) {
      newest = Math.max(newest, (await stat(path)).mtimeMs);
    }
  }
  return newest;
}

/** mtime of the newest native source - the clock the watchers compare against. */
export function nativeSourceStamp() {
  return newestSourceTimeUnder(join(repoRoot, "native"));
}

/** True when native sources are newer than the built binary, or it is missing. */
export async function isNativeStale(configuration) {
  const binaryTime = await stat(backendBinaryPath(configuration))
    .then((info) => info.mtimeMs)
    .catch(() => 0);
  if (!binaryTime) return true;
  return (await nativeSourceStamp()) > binaryTime;
}

export async function buildBackend(configuration) {
  const args = ["build", "--manifest-path", join(repoRoot, "native", "Cargo.toml"), "-p", "zcode-server"];
  if (configuration === "release") args.push("--release");
  const exitCode = await run("cargo", args, repoRoot);
  if (exitCode !== 0) throw new Error(`cargo build (${configuration}) failed with exit code ${exitCode}`);
}

/** Spawn with inherited stdio so the caller's terminal sees backend/Vite output. */
export function start(file, args, cwd = repoRoot) {
  return spawn(file, args, { cwd, stdio: "inherit", env: process.env });
}

export function startBackend(configuration, args) {
  return start(backendBinaryPath(configuration), args);
}

export function run(file, args, cwd = repoRoot) {
  return new Promise((done, fail) => {
    const child = start(file, args, cwd);
    child.on("error", fail);
    child.on("exit", (code, signal) => {
      if (signal) fail(new Error(`${file} was terminated by ${signal}`));
      else done(code ?? 0);
    });
  });
}

export async function waitForHttp(url, { timeoutMs = 60_000, validate } = {}) {
  const deadline = Date.now() + timeoutMs;
  let lastError = "not attempted";
  while (Date.now() < deadline) {
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(3_000) });
      if (!response.ok) lastError = `HTTP ${response.status}`;
      else {
        const body = await response.text();
        const failure = validate?.(body);
        if (!failure) return body;
        lastError = failure;
      }
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
    }
    await new Promise((sleep) => setTimeout(sleep, 200));
  }
  throw new Error(`${url} never became healthy (${lastError})`);
}

export function parseFlags(argv) {
  const flags = {};
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (!argument.startsWith("--")) throw new Error(`unexpected argument "${argument}"`);
    const name = argument.slice(2);
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) {
      flags[name] = true;
      continue;
    }
    flags[name] = value;
    index += 1;
  }
  return flags;
}

export function log(message) {
  console.log(`[zcode] ${message}`);
}

export function fail(message) {
  console.error(`[zcode] ${message}`);
  process.exitCode = 1;
}
