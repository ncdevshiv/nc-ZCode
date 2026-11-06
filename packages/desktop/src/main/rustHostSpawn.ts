import { existsSync } from "node:fs";
import { spawn, type ChildProcess } from "node:child_process";
import { dirname, join, resolve } from "node:path";

/** Cutover flag (spec §6): `ZCODE_NATIVE_HOST=1` selects the Rust stdio host. */
export function isRustHostEnabled(): boolean {
  return process.env["ZCODE_NATIVE_HOST"]?.trim() === "1";
}

/**
 * Resolve the Rust channel server binary (`zcode-server`, channel wire over
 * TCP — the MessagePort bridge target from `host/rustChannelBridge.ts`).
 *
 * Priority: `ZCODE_RUST_SERVER_PATH` → `ZCODE_RUST_HOST_PATH` (back-compat)
 * → dev checkout `native/target/debug` (or `release` under
 * `ZCODE_RUST_HOST_RELEASE=1`) → packaged resources next to the main binary.
 * Returns `undefined` when none exists (caller falls back to the TS host).
 */
export function resolveRustServerPath(): string | undefined {
  const candidates: string[] = [];
  const explicit =
    process.env["ZCODE_RUST_SERVER_PATH"]?.trim() ||
    process.env["ZCODE_RUST_HOST_PATH"]?.trim();
  if (explicit) {
    candidates.push(explicit);
  }
  const exe = process.platform === "win32" ? "zcode-server.exe" : "zcode-server";
  const release = process.env["ZCODE_RUST_HOST_RELEASE"]?.trim() === "1";
  // Dev checkout: main process runs from packages/desktop (built) or the repo
  // root under electron-forge/dev; walk up from cwd to find native/.
  let dir = process.cwd();
  for (let depth = 0; depth < 6; depth += 1) {
    candidates.push(join(dir, "native", "target", release ? "release" : "debug", exe));
    const parent = dirname(dir);
    if (parent === dir) {
      break;
    }
    dir = parent;
  }
  // Packaged: binary ships next to the main executable's resources.
  candidates.push(resolve(process.execPath, "..", "resources", exe), resolve(process.execPath, "..", "..", "resources", exe));
  return candidates.find((candidate) => existsSync(candidate));
}

/** Back-compat alias (stdio host) — prefer `resolveRustServerPath`. */
export function resolveRustHostPath(): string | undefined {
  return resolveRustServerPath();
}

export interface RustHostSpawn {
  proc: ChildProcess;
  /** Resolves once the child emitted its `ready` diagnostic on stderr. */
  ready: Promise<void>;
}

/**
 * Spawn the Rust stdio host (`--workspace/--data-dir/--full-access`).
 * Diagnostics go to stderr (never stdout, which is the NDJSON protocol pipe);
 * readiness is the host's `[zcode-host] ready` stderr line (same contract as
 * the TS host readiness for the log relay).
 */
export function spawnRustHost(options: {
  binaryPath: string;
  workspacePath: string;
  dataDir: string;
  fullAccess: boolean;
  label: string;
  logger: {
    info: (...args: unknown[]) => void;
    warn: (...args: unknown[]) => void;
  };
}): RustHostSpawn {
  const args = [
    "--workspace",
    options.workspacePath,
    "--data-dir",
    options.dataDir,
    ...(options.fullAccess ? ["--full-access"] : []),
  ];
  const proc = spawn(options.binaryPath, args, {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, ZCODE_PROCESS_LABEL: options.label },
    windowsHide: true,
  });
  const ready = new Promise<void>((resolveReady, rejectReady) => {
    let readySeen = false;
    const timer = setTimeout(() => {
      if (!readySeen) {
        rejectReady(
          new Error(
            `Rust host did not become ready within 15s (binary=${options.binaryPath})`,
          ),
        );
      }
    }, 15_000);
    proc.stderr?.on("data", (chunk: Buffer) => {
      const text = chunk.toString();
      options.logger.info(`[rust-host ${options.label}] stderr: ${text.trim()}`);
      if (!readySeen && text.includes("ready workspace=")) {
        readySeen = true;
        clearTimeout(timer);
        resolveReady();
      }
    });
    proc.stdout?.on("data", (chunk: Buffer) => {
      // NDJSON protocol pipe: only log at debug intent (never mixed into stdout).
      options.logger.info(
        `[rust-host ${options.label}] ndjson bytes=${chunk.length}`,
      );
    });
    proc.on("error", (error: Error) => {
      if (!readySeen) {
        readySeen = true;
        clearTimeout(timer);
        rejectReady(error);
      }
    });
  });
  return { proc, ready };
}
