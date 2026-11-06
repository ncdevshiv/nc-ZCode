import { renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const DEFAULT_RETRY_DURATION_MS = 1_500;
const RETRY_DELAYS_MS = [25, 50, 100] as const;
const RETRY_ATTEMPTS_FIELD = "zcodeOfficialPluginCacheAttempts";
const TRANSIENT_ERROR_CODES = new Set(["EACCES", "EBUSY", "EEXIST", "ENOTEMPTY", "EPERM"]);

export interface OfficialPluginCacheRetryBudget {
  deadlineAt: number;
  now: () => number;
  sleep: (delayMs: number) => void;
}

export function createOfficialPluginCacheRetryBudget(input?: {
  durationMs?: number;
  now?: () => number;
  sleep?: (delayMs: number) => void;
}): OfficialPluginCacheRetryBudget {
  const now = input?.now ?? Date.now;
  return {
    deadlineAt: now() + (input?.durationMs ?? DEFAULT_RETRY_DURATION_MS),
    now,
    sleep: input?.sleep ?? sleepSync,
  };
}

export function isTransientOfficialPluginCacheFsError(
  error: unknown,
): error is NodeJS.ErrnoException {
  return (
    typeof error === "object" &&
    error !== null &&
    TRANSIENT_ERROR_CODES.has(String((error as NodeJS.ErrnoException).code))
  );
}

export function getOfficialPluginCacheRetryAttempts(error: unknown): number {
  if (typeof error !== "object" || error === null) return 1;
  const attempts = (error as Record<string, unknown>)[RETRY_ATTEMPTS_FIELD];
  return typeof attempts === "number" && Number.isFinite(attempts) ? attempts : 1;
}

function retryOfficialPluginCacheFs<T>(
  operation: () => T,
  options?: {
    budget?: OfficialPluginCacheRetryBudget;
  },
): T {
  const budget = options?.budget ?? createOfficialPluginCacheRetryBudget();
  let attempts = 0;
  while (true) {
    attempts += 1;
    try {
      return operation();
    } catch (error) {
      setRetryAttempts(error, attempts);
      const delayMs = RETRY_DELAYS_MS[attempts - 1];
      if (
        !isTransientOfficialPluginCacheFsError(error) ||
        delayMs === undefined ||
        budget.now() + delayMs > budget.deadlineAt
      ) {
        throw error;
      }
      budget.sleep(delayMs);
    }
  }
}

export function removeOfficialPluginCacheDirectory(
  path: string,
  budget = createOfficialPluginCacheRetryBudget(),
): void {
  retryOfficialPluginCacheFs(
    () => {
      rmSync(path, {
        force: true,
        maxRetries: 3,
        recursive: true,
        retryDelay: 25,
      });
    },
    { budget },
  );
}

export function renameOfficialPluginCachePath(
  fromPath: string,
  toPath: string,
  budget: OfficialPluginCacheRetryBudget,
): void {
  retryOfficialPluginCacheFs(() => renameSync(fromPath, toPath), { budget });
}

export function writeTextFileAtomicallyWithRetry(
  filePath: string,
  contents: string,
  budget: OfficialPluginCacheRetryBudget,
): void {
  const temporaryPath = join(
    dirname(filePath),
    `.tmp-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`,
  );
  try {
    retryOfficialPluginCacheFs(() => writeFileSync(temporaryPath, contents), { budget });
    renameOfficialPluginCachePath(temporaryPath, filePath, budget);
  } catch (error) {
    try {
      rmSync(temporaryPath, { force: true });
    } catch {
// Failure to clean up the temporary file cannot overwrite the real write error; a new unique temporary file will be used at the next startup.
    }
    throw error;
  }
}

function setRetryAttempts(error: unknown, attempts: number): void {
  if (typeof error !== "object" || error === null) return;
  try {
    Object.defineProperty(error, RETRY_ATTEMPTS_FIELD, {
      configurable: true,
      value: attempts,
      writable: true,
    });
  } catch {
// Some third-party error objects may be frozen; in this case, the log returns attempts=1 and the original exception is not changed.
  }
}

function sleepSync(delayMs: number): void {
// The official plug-in discovery link is currently a synchronous API. Windows Antivirus/Indexer may temporarily occupy the cache,
// Absorb transient EPERMs with short waits that are limited by the total budget to avoid asynchronous expansion of the entire startup link for hot repairs.
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, delayMs);
}
