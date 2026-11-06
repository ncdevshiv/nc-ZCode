import { randomUUID } from "node:crypto";
import {
  ZCODE_PROCESS_DIAGNOSTIC_PREFIX,
  ZCODE_PROCESS_DIAGNOSTIC_NAME_MAX_CHARS,
  ZCODE_PROCESS_DIAGNOSTIC_MESSAGE_MAX_CHARS,
  ZCODE_PROCESS_DIAGNOSTIC_STACK_MAX_CHARS,
  type ZCodeProcessDiagnostic,
} from "@zcode/shared/process-diagnostic";

interface CliProcessErrorBoundaryTarget {
  on(event: string, listener: (...args: unknown[]) => void): unknown;
  off(event: string, listener: (...args: unknown[]) => void): unknown;
}

interface CliProcessErrorBoundaryOptions {
  onFatal: (reason: unknown) => void;
  target?: CliProcessErrorBoundaryTarget;
  stderr?: {
    write(chunk: string): unknown;
  };
}

interface MonitoredException {
  error: unknown;
  origin: string;
}

/**
* Install a final process-level exception boundary for the protocol-based CLI.
 *
* First retain the diagnosis once, and then hand it to the entrance lifecycle for bounded closing. Unknown exception cannot continue to keep alive orders.
* It is also not possible to recursively report write failures to bad stderr; the caller must first install a stderr output boundary.
 */
export function installCliProcessErrorBoundary(
  options: CliProcessErrorBoundaryOptions,
): () => void {
  const target = options.target ?? (process as unknown as CliProcessErrorBoundaryTarget);
  const stderr = options.stderr ?? process.stderr;
  let monitoredException: MonitoredException | undefined;
  let fatal = false;
  const reportFatal = (
    kind: "uncaughtException" | "unhandledRejection",
    origin: string,
    reason: unknown,
  ) => {
    if (fatal) return;
    fatal = true;
    writeProcessErrorDiagnostic(stderr, kind, origin, reason);
    options.onFatal(reason);
  };

  const onUncaughtExceptionMonitor = (error: unknown, origin: unknown): void => {
    monitoredException = {
      error,
      origin: typeof origin === "string" ? origin : "uncaughtException",
    };
  };
  const onUncaughtException = (error: unknown): void => {
    const matchingMonitor = monitoredException?.error === error ? monitoredException : undefined;
    const origin = matchingMonitor ? matchingMonitor.origin : "uncaughtException";
    monitoredException = undefined;
// Node strict mode triggers uncaughtException first, and then triggers unhandledRejection after processing.
// Uniformly report by rejection listener to avoid generating two different errorIds for the same Promise error.
    if (origin === "unhandledRejection") return;
    reportFatal("uncaughtException", origin, error);
  };
  const onUnhandledRejection = (reason: unknown): void => {
    monitoredException = undefined;
    reportFatal("unhandledRejection", "unhandledRejection", reason);
  };

  target.on("uncaughtExceptionMonitor", onUncaughtExceptionMonitor);
  target.on("uncaughtException", onUncaughtException);
  target.on("unhandledRejection", onUnhandledRejection);

  return () => {
    target.off("uncaughtExceptionMonitor", onUncaughtExceptionMonitor);
    target.off("uncaughtException", onUncaughtException);
    target.off("unhandledRejection", onUnhandledRejection);
    monitoredException = undefined;
  };
}

function writeProcessErrorDiagnostic(
  stderr: { write(chunk: string): unknown },
  kind: "uncaughtException" | "unhandledRejection",
  origin: string,
  reason: unknown,
): void {
  try {
    const detail = formatProcessError(reason).slice(0, ZCODE_PROCESS_DIAGNOSTIC_STACK_MAX_CHARS);
    const diagnostic: ZCodeProcessDiagnostic = {
      version: 1,
      errorId: randomUUID(),
      kind,
      origin: origin === "unhandledRejection" ? origin : "uncaughtException",
      name: (reason instanceof Error ? reason.name || "Error" : "Error").slice(
        0,
        ZCODE_PROCESS_DIAGNOSTIC_NAME_MAX_CHARS,
      ),
      message: (reason instanceof Error ? reason.message : detail).slice(
        0,
        ZCODE_PROCESS_DIAGNOSTIC_MESSAGE_MAX_CHARS,
      ),
      ...(reason instanceof Error && reason.stack ? { stack: detail } : {}),
      occurredAt: Date.now(),
    };
// Root cause: When the process is alive, the old stderr only enters debug, and the Electron SDK cannot catch child process exceptions.
// Add a single-line structured event for Host to forward immediately, retaining readable text for compatibility with old Host and crash tail.
    stderr.write(
      `${ZCODE_PROCESS_DIAGNOSTIC_PREFIX}${JSON.stringify(diagnostic)}\n[zcode] process error kind=${kind} origin=${origin}\n${detail}\n`,
    );
  } catch {
// Diagnostic output cannot penetrate the process-level exception boundary again.
  }
}

function formatProcessError(reason: unknown): string {
  if (reason instanceof Error) {
    return reason.stack || `${reason.name}: ${reason.message}`;
  }
  if (typeof reason === "string") {
    return reason;
  }
  try {
    const serialized = JSON.stringify(reason);
    return serialized === undefined ? String(reason) : serialized;
  } catch {
    return String(reason);
  }
}
