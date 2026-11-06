import { PassThrough } from "node:stream";
import { flushE2ECoverage } from "./shutdown.js";

const PROTOCOL_SHUTDOWN_TIMEOUT_MS = 1_500;
const PROTOCOL_INPUT_DRAIN_MS = 100;
const COVERAGE_FLUSH_ALLOWANCE_MS = 500;
const SIGNAL_EXIT_CODES = { SIGINT: 130, SIGTERM: 143, SIGHUP: 129 } as const;

/** The only exit owner of the CLI entry; installed before loading the runtime, does not rely on run() return. */
export function createProtocolProcessLifecycle(
  options: {
    input?: NodeJS.ReadableStream;
    output?: NodeJS.WritableStream;
    stderr?: NodeJS.WritableStream;
    timeoutMs?: number;
    exitProcess?: (code: number) => void;
  } = {},
) {
  const source = options.input ?? process.stdin;
  const output = options.output ?? process.stdout;
  const stderr = options.stderr ?? process.stderr;
  const input = new PassThrough();
  const controller = new AbortController();
  const exit = options.exitProcess ?? ((code: number) => process.exit(code));
  let deadlineAt: number | undefined;
  let exitCode = 0;
  let deadlineTimer: NodeJS.Timeout | undefined;
  let drainTimer: NodeJS.Timeout | undefined;
  let completion: Promise<void> | undefined;

  const stop = (error?: Error, code = error ? 1 : 0) => {
    if (deadlineAt !== undefined) return;
    exitCode = code;
    const timeoutMs =
      options.timeoutMs ??
      PROTOCOL_SHUTDOWN_TIMEOUT_MS +
        (process.env.ZCODE_E2E_COVERAGE === "1" ? COVERAGE_FLUSH_ALLOWANCE_MS : 0);
    deadlineAt = Date.now() + timeoutMs;
//Keep ref: Even if the initialized Promise never settles and there is no IO, the final state must be delivered.
    deadlineTimer = setTimeout(() => exit(exitCode), timeoutMs);
    if (error) controller.abort(error);
    else
      drainTimer = setTimeout(
        () => controller.abort(new Error("Protocol input closed")),
        PROTOCOL_INPUT_DRAIN_MS,
      );
  };
  const onInputEnd = () => stop();
  const onIoError = (error: Error) => stop(error);
  const onOutputClose = () => stop(new Error("Protocol output closed"));
  const signals =
    process.platform === "win32"
      ? (["SIGINT", "SIGTERM"] as const)
      : (["SIGINT", "SIGTERM", "SIGHUP"] as const);
  for (const signal of signals) {
    const listener = () =>
      stop(new Error(`Protocol received ${signal}`), SIGNAL_EXIT_CODES[signal]);
// on instead of once: repeated signals do not bypass cleanup and do not reset the first deadline.
    process.on(signal, listener);
  }
  source.on("end", onInputEnd);
  source.on("close", onInputEnd);
  source.on("error", onIoError);
  output.on("error", onIoError);
  output.on("close", onOutputClose);
  input.on("error", onIoError);
// During the startup phase, consume the original stdin to observe the parent EOF; the backpressure buffer retains bytes that have not been consumed by the protocol.
  source.pipe(input);
  if ((source as NodeJS.ReadableStream & { readableEnded?: boolean }).readableEnded) onInputEnd();

  return {
    input,
    signal: controller.signal,
    get deadlineAt() {
      return deadlineAt;
    },
    requestShutdown: stop,
    complete(code: number): Promise<void> {
      completion ??= (async () => {
        stop(code === 0 ? undefined : new Error("Protocol command failed"), code);
        if (drainTimer) clearTimeout(drainTimer);
        controller.abort(new Error("Protocol command completed"));
        await Promise.all([flush(output), flush(stderr), flushE2ECoverage()]);
        if (deadlineTimer) clearTimeout(deadlineTimer);
        exit(exitCode);
      })();
      return completion;
    },
  };
}

function flush(stream: NodeJS.WritableStream): Promise<void> {
  return new Promise((resolve) => {
    if (!stream.writable) return resolve();
    try {
      stream.write("", () => resolve());
    } catch {
      resolve();
    }
  });
}
