const processGuardInstalled = new WeakSet<object>();
const OUTPUT_CLOSED_ERROR_CODES = new Set([
  "EPIPE",
  "EIO",
  "ENXIO",
  "EBADF",
  "ERR_STREAM_DESTROYED",
]);

/**
* REPL cell's synchronization errors are handled by NodeReplSession, but fire-and-forget's asynchronous errors
* (If the tab.* call without await is rejected when the turn is interrupted), the entire server will be penetrated according to Node's default strategy.
* Process. Once the child process dies, Browser Use within the session is no longer available. Here the asynchronous errors are downgraded to stderr logs:
* Runtime state is preserved and protocol stdout is not affected.
 *
* After the parent process exits, stderr will report EPIPE. The old handler writes back the same EPIPE stack
* stderr, forming an infinite loop of EPIPE -> uncaughtException -> stderr.write -> EPIPE.
* Closing the output pipe means that the MCP client is unreachable and must enter shutdown directly, and cannot continue to write diagnostics.
 */
export function installNodeReplProcessGuards(input: {
  onOutputClosed: (error: Error) => void;
  process: Pick<NodeJS.Process, "on">;
  writeStderr: (text: string) => void;
}): void {
  if (processGuardInstalled.has(input.process)) return;
  processGuardInstalled.add(input.process);

  let outputClosed = false;
  const describe = (reason: unknown): string =>
    reason instanceof Error ? (reason.stack ?? reason.message) : String(reason);
  const report = (kind: "uncaughtException" | "unhandledRejection", reason: unknown): void => {
    if (outputClosed) return;
    if (isOutputClosedError(reason)) {
      outputClosed = true;
      input.onOutputClosed(reason);
      return;
    }

    try {
      input.writeStderr(`node_repl ${kind} (process kept alive): ${describe(reason)}\n`);
    } catch (error) {
      if (!isOutputClosedError(error)) throw error;
      outputClosed = true;
      input.onOutputClosed(error);
    }
  };
  input.process.on("unhandledRejection", (reason) => {
    report("unhandledRejection", reason);
  });
  input.process.on("uncaughtException", (error) => {
    report("uncaughtException", error);
  });
}

function isOutputClosedError(error: unknown): error is Error {
  if (!(error instanceof Error)) return false;
  const code = (error as NodeJS.ErrnoException).code;
  return typeof code === "string" && OUTPUT_CLOSED_ERROR_CODES.has(code);
}

export function installNodeReplShutdownTriggers(input: {
  process: Pick<NodeJS.Process, "once">;
  shutdown: () => void;
  stdin: Pick<NodeJS.ReadStream, "once">;
}): void {
  let shutdownStarted = false;
  const shutdownOnce = () => {
    if (shutdownStarted) return;
    shutdownStarted = true;
    input.shutdown();
  };

// MCP SDK's stdio transport does not listen to stdin end/close. runtime when the parent process exits abnormally
// Therefore, it does not receive the end of the life cycle and becomes an orphan process.
  input.stdin.once("end", shutdownOnce);
  input.stdin.once("close", shutdownOnce);
  input.process.once("SIGINT", shutdownOnce);
  input.process.once("SIGTERM", shutdownOnce);
}

export async function isDirectMcpEntrypoint(
  importMetaUrl: string,
  argvPath: string | undefined,
): Promise<boolean> {
  if (!argvPath) return false;
  try {
// macOS's /tmp, /var and other paths will be resolved to /private/...; direct comparison of file URL will make
// The stdio child process was misjudged as "imported", main was not started and exited without error. Asynchronous realpath is also compatible with
// symlink installation directory, and avoid introducing synchronous file IO in the module evaluation path.
    const [modulePath, executablePath] = await Promise.all([
      realpath(fileURLToPath(importMetaUrl)),
      realpath(argvPath),
    ]);
    return modulePath === executablePath;
  } catch {
    return false;
  }
}
import { realpath } from "node:fs/promises";
import { fileURLToPath } from "node:url";
