import { takeCoverage } from "node:v8";

const DEFAULT_SHUTDOWN_CLEANUP_TIMEOUT_MS = 2_000;
export const DEFAULT_CLI_CLEANUP_TIMEOUT_MS = 6_000;
const DEFAULT_CLI_EXIT_WATCHDOG_TIMEOUT_MS = 1_000;
const SIGNAL_EXIT_CODES: Partial<Record<NodeJS.Signals, number>> = {
  SIGHUP: 129,
  SIGINT: 130,
  SIGTERM: 143,
};

type SignalListener = () => void;

export interface CliShutdownProcess {
  platform: NodeJS.Platform;
  off(signal: NodeJS.Signals, listener: SignalListener): unknown;
  once(signal: NodeJS.Signals, listener: SignalListener): unknown;
}

interface CliShutdownOptions {
  abort?: (signal: NodeJS.Signals) => void;
  cleanup: () => Promise<void> | void;
  cleanupTimeoutMs?: number;
  exitProcess?: (code: number) => void;
  process?: CliShutdownProcess;
}

interface CliExitWatchdogOptions {
  exitCode: number;
  exitProcess?: (code: number) => void;
  timeoutMs?: number;
}

export function registerCliShutdownHandlers(options: CliShutdownOptions): () => void {
  const signalTarget = options.process ?? process;
  const exitProcess = options.exitProcess ?? ((code: number) => process.exit(code));
  const cleanupTimeoutMs = Math.max(
    0,
    Math.trunc(options.cleanupTimeoutMs ?? DEFAULT_SHUTDOWN_CLEANUP_TIMEOUT_MS),
  );
  const signals = shutdownSignals(signalTarget.platform);
  const listeners: Array<[NodeJS.Signals, SignalListener]> = [];
  let disposed = false;
  let shuttingDown = false;

  const unregister = () => {
    if (disposed) return;
    disposed = true;
    for (const [signal, listener] of listeners) {
      signalTarget.off(signal, listener);
    }
  };

  const shutdown = async (signal: NodeJS.Signals) => {
    if (shuttingDown) return;
    shuttingDown = true;
    // detached shell processes form their own process group, so parent
    // process shutdown must explicitly close the app before Node exits.
    options.abort?.(signal);
    await runCliCleanupWithTimeout(options.cleanup, cleanupTimeoutMs);
    await flushE2ECoverage();
    unregister();
    exitProcess(exitCodeForSignal(signal));
  };

  for (const signal of signals) {
    const listener = () => {
      void shutdown(signal);
    };
    signalTarget.once(signal, listener);
    listeners.push([signal, listener]);
  }

  return unregister;
}

export async function flushE2ECoverage(): Promise<void> {
  if (process.env.ZCODE_E2E_COVERAGE !== "1" || !process.env.NODE_V8_COVERAGE?.trim()) {
    return;
  }
  try {
// Agent shutdown eventually calls process.exit, and the SIGTERM path will not be triggered reliably.
// Automatic placement of NODE_V8_COVERAGE; only explicitly refreshed in E2E coverage mode.
    takeCoverage();
// takeCoverage will hand over the disk writing to the V8 background task; immediately process.exit will occasionally leave only readiness
// marker. Coverage mode leaves a short disk download window, and ordinary CLI shutdown does not increase the delay.
    await new Promise<void>((resolve) => setTimeout(resolve, 500));
  } catch {
// Coverage is a diagnostic product, and disk writing failure cannot block the existing exit process of the CLI.
  }
}

function shutdownSignals(platform: NodeJS.Platform): NodeJS.Signals[] {
  return platform === "win32" ? ["SIGINT", "SIGTERM"] : ["SIGINT", "SIGTERM", "SIGHUP"];
}

function exitCodeForSignal(signal: NodeJS.Signals): number {
  return SIGNAL_EXIT_CODES[signal] ?? 1;
}

export async function runCliCleanupWithTimeout(
  cleanup: () => Promise<void> | void,
  timeoutMs: number,
): Promise<void> {
  let timeout: NodeJS.Timeout | undefined;
  try {
    const cleanupPromise = Promise.resolve().then(cleanup);
    if (timeoutMs === 0) {
      await cleanupPromise;
      return;
    }

    await Promise.race([
      cleanupPromise,
      new Promise<void>((resolve) => {
        timeout = setTimeout(resolve, timeoutMs);
        timeout.unref?.();
      }),
    ]);
  } catch {
    // Shutdown cleanup is best-effort; exiting should not be blocked by a
    // failing adapter close path.
  } finally {
    if (timeout) clearTimeout(timeout);
  }
}

export function scheduleCliExitWatchdog(options: CliExitWatchdogOptions): () => void {
  const exitProcess = options.exitProcess ?? ((code: number) => process.exit(code));
  const timeoutMs = Math.max(
    1,
    Math.trunc(options.timeoutMs ?? DEFAULT_CLI_EXIT_WATCHDOG_TIMEOUT_MS),
  );
  const timer = setTimeout(() => {
// Unknown pipe/socket handles may still remain after normal run() has completed. watchdog
// Only executed when the event loop has not been exhausted by the deadline, so the natural exit path will not be delayed.
    void flushE2ECoverage().finally(() => exitProcess(options.exitCode));
  }, timeoutMs);
  timer.unref?.();
  return () => clearTimeout(timer);
}
