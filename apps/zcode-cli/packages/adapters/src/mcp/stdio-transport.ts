import {
  StdioClientTransport,
  type StdioServerParameters,
} from "@modelcontextprotocol/client/stdio";
import type { JSONRPCMessage } from "@modelcontextprotocol/client";
import type { ChildProcess } from "node:child_process";
import { terminateMcpStdioProcessTree } from "./process-tree.js";
import {
  attachProcessToWindowsJobObject,
  type WindowsJobObjectController,
} from "./windows-job-object.js";

type SdkStdioDispose = (this: StdioClientTransport) => Promise<void>;

const sdkDispose = Object.getOwnPropertyDescriptor(StdioClientTransport.prototype, "_dispose")
  ?.value as SdkStdioDispose | undefined;

type StdioRequestMetaProvider = () => Promise<Record<string, unknown> | undefined>;

interface StdioProcessExitInfo {
  exitCode: number | null;
  exitedAt: number;
  signal: NodeJS.Signals | null;
  startedAt: number;
}

type ProcessTreeStdioServerParameters = StdioServerParameters & {
  /**
* Official per-message identity payload provider for stdio MCP. It is reserved as a non-executable field of the spawn parameter, ensuring that the SDK
* When cloning the sibling transport for modern probe, `server/discover` uses the same injection boundary as the formal session.
   */
  requestMetaProvider?: StdioRequestMetaProvider;
};

interface ProcessTreeStdioClientTransportOptions {
  windowsJobObjectFactory?: (pid: number) => Promise<WindowsJobObjectController | undefined>;
}

/**
* SDK 2.0 creates one-time sibling processes for stdio version detection, but its private recycling hook only kills direct child processes.
* Descendants derived from launcher/watchdog will become orphans; first recycle the entire process tree in the same hook, and then let
* SDK cleans pipe and read buffer. The prototype must have `_dispose` directly before the SDK will use the transport
* Probe transport identified as safe to clone.
 */
export class ProcessTreeStdioClientTransport extends StdioClientTransport {
  private childProcess?: ChildProcess;
  private lastProcessExit?: StdioProcessExitInfo;
  private readonly requestMetaProvider?: StdioRequestMetaProvider;
  private readonly windowsJobObjectFactory: (
    pid: number,
  ) => Promise<WindowsJobObjectController | undefined>;
  private windowsJobObject?: WindowsJobObjectController;

  constructor(
    server: ProcessTreeStdioServerParameters,
    options: ProcessTreeStdioClientTransportOptions = {},
  ) {
    super(server);
    this.requestMetaProvider = server.requestMetaProvider;
    this.windowsJobObjectFactory =
      options.windowsJobObjectFactory ?? attachProcessToWindowsJobObject;
  }

  async terminateWindowsJobObject(): Promise<void> {
    const windowsJobObject = this.windowsJobObject;
    this.windowsJobObject = undefined;
    if (!windowsJobObject) return;
    try {
      windowsJobObject.terminate();
    } catch {
// Continue to execute close and taskkill rollback.
    } finally {
      try {
        windowsJobObject.close();
      } catch {
// Failure to close the handle cannot block SDK pipe cleanup.
      }
    }
  }

  override async send(message: JSONRPCMessage): Promise<void> {
    const requestMeta = await this.requestMetaProvider?.();
    await super.send(mergeRequestMeta(message, requestMeta));
  }

  override async start(): Promise<void> {
    const startedAt = Date.now();
    await super.start();
    const child = (this as unknown as { _process?: ChildProcess })._process;
    if (!child) return;
    this.childProcess = child;
    const recordExit = (exitCode: number | null, signal: NodeJS.Signals | null): void => {
      this.lastProcessExit ??= {
        exitCode,
        exitedAt: Date.now(),
        signal,
        startedAt,
      };
    };
    if (child.exitCode !== null || child.signalCode !== null) {
      recordExit(child.exitCode, child.signalCode);
      return;
    }
    child.once("exit", recordExit);

// staging once combined the Job Object takeover and exit observations into two start()s with the same name, resulting in CLI
// Build and report directly to Duplicate function implementation; here merge the two responsibilities and retain Windows
// Process tree hosting and cross-platform exit logging.
    if (process.platform !== "win32" || this.pid == null) return;
    try {
      this.windowsJobObject = await this.windowsJobObjectFactory(this.pid);
    } catch {
// Keep the taskkill fallback when native hosting is unavailable, and do not allow MCP connection establishment to fail due to optional capabilities.
      this.windowsJobObject = undefined;
    }
  }

  get processExit(): StdioProcessExitInfo | undefined {
    return this.lastProcessExit;
  }

  get processAlive(): boolean {
    return Boolean(
      this.childProcess &&
      this.childProcess.exitCode === null &&
      this.childProcess.signalCode === null,
    );
  }
}

function mergeRequestMeta(
  message: JSONRPCMessage,
  requestMeta: Record<string, unknown> | undefined,
): JSONRPCMessage {
// JSON-RPC response does not have a method, and forged params must not be given to the response. Only the requests and notifications sent by the client are modified.
  if (!("method" in message) || !requestMeta || Object.keys(requestMeta).length === 0) {
    return message;
  }
  const params = isRecord(message.params) ? message.params : {};
  const existingMeta = isRecord(params._meta) ? params._meta : {};
  return {
    ...message,
    params: {
      ...params,
// The official identity payload just parsed by the host must overwrite the caller's residual value to prevent the old credentials from continuing to survive.
      _meta: { ...existingMeta, ...requestMeta },
    },
  } as JSONRPCMessage;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

Object.defineProperty(ProcessTreeStdioClientTransport.prototype, "_dispose", {
  configurable: true,
  async value(this: ProcessTreeStdioClientTransport): Promise<void> {
    const pid = this.pid;
    await this.terminateWindowsJobObject();
    if (typeof pid === "number" && Number.isInteger(pid) && pid > 0) {
      try {
        await terminateMcpStdioProcessTree(pid);
      } catch {
// The SDK's detection and recycling is inherently best effort; it is still necessary to continue to release the pipe to avoid the negotiation being permanently stuck.
      }
    }

    if (sdkDispose) {
      await sdkDispose.call(this);
      return;
    }
    await this.close();
  },
});
