import { mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const DEFAULT_RETRY_DELAY_MS = 50;
const DEFAULT_STALE_LOCK_AGE_MS = 60_000;
const DEFAULT_TIMEOUT_MS = 15_000;
const SEED_LOCK_TIMEOUT_ERROR_CODE = "ZCODE_PLUGIN_SEED_LOCK_TIMEOUT";

interface OfficialPluginSeedLockOptions {
  retryDelayMs?: number;
  staleLockAgeMs?: number;
  timeoutMs?: number;
}

export function withOfficialPluginSeedLock<T>(
  targetRoot: string,
  action: () => T,
  options: OfficialPluginSeedLockOptions = {},
): T {
  const retryDelayMs = options.retryDelayMs ?? DEFAULT_RETRY_DELAY_MS;
  const staleLockAgeMs = options.staleLockAgeMs ?? DEFAULT_STALE_LOCK_AGE_MS;
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const lockRoot = `${targetRoot}.seed-lock`;
  const startedAt = Date.now();

  mkdirSync(dirname(lockRoot), { recursive: true });
  while (true) {
    try {
      mkdirSync(lockRoot);
      break;
    } catch (error) {
      if (!isAlreadyExistsError(error)) throw error;
      tryTakeOverStaleLock(lockRoot, staleLockAgeMs);
      if (Date.now() - startedAt >= timeoutMs) {
        throw Object.assign(
          new Error(`[official-plugin-seed-lock] timed out waiting for ${lockRoot}`),
          { code: SEED_LOCK_TIMEOUT_ERROR_CODE },
        );
      }
      sleepSync(retryDelayMs);
    }
  }

  try {
    writeFileSync(
      join(lockRoot, "owner.json"),
      JSON.stringify({ createdAt: new Date().toISOString(), pid: process.pid }),
    );
    return action();
  } finally {
    removeLockDirectory(lockRoot);
  }
}

/** Stable error code determination (project specifications: do not rely on error text diversion), for the caller to downgrade the waiting lock timeout. */
export function isOfficialPluginSeedLockTimeoutError(
  error: unknown,
): error is NodeJS.ErrnoException {
  return errorCode(error) === SEED_LOCK_TIMEOUT_ERROR_CODE;
}

function tryTakeOverStaleLock(lockRoot: string, staleLockAgeMs: number): void {
  let ageMs: number;
  try {
    ageMs = Date.now() - statSync(lockRoot).mtimeMs;
  } catch (error) {
    if (isNotFoundError(error)) return;
    throw error;
  }

  const ownerPid = readLockOwnerPid(lockRoot);
  if (ownerPid !== undefined && isProcessAlive(ownerPid)) return;
  if (ownerPid === undefined && ageMs < staleLockAgeMs) return;

  const staleRoot = `${lockRoot}.stale-${process.pid}-${Date.now()}`;
  try {
// Directly delete the stale lock. If TOCTOU exists, you may delete the new lock just created by another process by mistake.
// First atomically rename to obtain ownership of the old lock, and only clean up the directory that was successfully renamed.
    renameSync(lockRoot, staleRoot);
  } catch (error) {
    if (isTransientLockRace(error)) return;
    throw error;
  }
  removeLockDirectory(staleRoot);
}

function readLockOwnerPid(lockRoot: string): number | undefined {
  try {
    const owner = JSON.parse(readFileSync(join(lockRoot, "owner.json"), "utf8")) as {
      pid?: unknown;
    };
    return typeof owner.pid === "number" && Number.isSafeInteger(owner.pid) && owner.pid > 0
      ? owner.pid
      : undefined;
  } catch (error) {
    if (isNotFoundError(error) || error instanceof SyntaxError) return undefined;
    throw error;
  }
}

function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
// Windows/Unix may return EPERM due to permissions; this means that the process exists, but cannot be detected.
    return errorCode(error) === "EPERM";
  }
}

function removeLockDirectory(path: string): void {
  rmSync(path, {
    force: true,
    maxRetries: 5,
    recursive: true,
    retryDelay: DEFAULT_RETRY_DELAY_MS,
  });
}

function isAlreadyExistsError(error: unknown): boolean {
  return errorCode(error) === "EEXIST";
}

function isNotFoundError(error: unknown): boolean {
  return errorCode(error) === "ENOENT";
}

function isTransientLockRace(error: unknown): boolean {
  return ["EACCES", "EBUSY", "EEXIST", "ENOENT", "ENOTEMPTY", "EPERM"].includes(errorCode(error));
}

function errorCode(error: unknown): string {
  return typeof error === "object" && error !== null
    ? String((error as NodeJS.ErrnoException).code ?? "")
    : "";
}

function sleepSync(ms: number): void {
  if (ms <= 0) return;
// The filesystem seed of bootstrap itself is a synchronous startup boundary; here, the current Agent is temporarily blocked
// Process, in exchange for having only one writer for the same version cache across processes.
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}
