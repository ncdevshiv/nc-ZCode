import { createContext, runInContext, type Context } from "node:vm";
import { createRequire } from "node:module";
import { homedir, tmpdir } from "node:os";
import { IifeContextExecutor, type ReplExecutor } from "./executors.js";
import {
  PROCESS_MODULE_IDS,
  createReplRequire,
  createRestrictedProcessFacade,
  normalizeReplError,
  stringifyReplResult,
} from "./node-repl-runtime-helpers.js";

/** Images collected by REPL (nodeRepl.emitImage). */
export interface NodeReplImage {
  base64: string;
  mimeType: string;
}

/** SDK/bridge result block; keep MCP's structured fields, but don't let core depend on MCP SDK types. */
export interface NodeReplStructuredResult {
  content: Array<{ type: string; [key: string]: unknown }>;
  isError?: boolean;
  structuredContent?: Record<string, unknown>;
  _meta?: Record<string, unknown>;
}

/**
* The target application identity (Computer Use) of this cell operation.
 *
* Only recorded by the host's CUA bridge when it receives a broker response - it is not on the sandbox globals and the model cannot be changed.
* `nodeRepl.setResponseMeta` / `nodeRepl.emitStructuredResult` are writable channels of the model, and the objects reached through them
* The producer application metadata is not trusted and must be discarded in toMcpRunResult.
 */
export interface NodeReplCuaAppIdentity {
  appKey: string;
  displayName?: string;
}

/** Output sink: nodeRepl.write/console in REPL is gathered here; emitImage collects images. */
export interface NodeReplWriteSink {
  write(text: string): void;
  images: NodeReplImage[];
  browserScreenshots: NodeReplImage[];
  structuredResults: NodeReplStructuredResult[];
  responseMeta: Record<string, unknown>;
  cuaApps: NodeReplCuaAppIdentity[];
}

export interface NodeReplRunResult {
/** Stringification of the last value/return value (may be undefined). */
  result?: string;
/** Output collected by nodeRepl.write + console during this run. */
  logs: string;
/** Structural error when throwing an error (without crashing the process). */
  error?: { name: string; message: string; stack?: string };
/** Images collected by nodeRepl.emitImage during this run (such as screenshots). */
  images?: NodeReplImage[];
/** Confirm the index from this explicit tab.screenshot() in images. */
  browserScreenshotImageIndices?: number[];
/** Structured results written by the SDK over a dedicated channel; takes precedence over console/REPL echo. */
  structuredResults?: NodeReplStructuredResult[];
/** Metadata set by nodeRepl.setResponseMeta during this run. */
  responseMeta?: Record<string, unknown>;
/** The application operated by the last CUA call to establish the identity of this cell; it is recorded by the host bridge and the model is not writable. */
  cuaApp?: NodeReplCuaAppIdentity;
}

export type NodeReplRequestMeta = Record<string, unknown>;

export interface NodeReplSessionOptions {
/** Additional globals injected into the sandbox (e.g. browser execute bridge, agent object). */
  injectedGlobals?: Record<PropertyKey, unknown> | (() => Record<PropertyKey, unknown>);
/** MCP stdio runtime uses restricted process facade to prevent cell from shutting down the process or writing bad protocol stdout. */
  restrictProcess?: boolean;
}

function browserScreenshotIndexResult(
  images: readonly NodeReplImage[],
  browserScreenshots: readonly NodeReplImage[],
): Pick<NodeReplRunResult, "browserScreenshotImageIndices"> {
  if (images.length === 0 || browserScreenshots.length === 0) return {};

  const remainingByPayload = new Map<string, number>();
  for (const screenshot of browserScreenshots) {
    const key = `${screenshot.mimeType}\u0000${screenshot.base64}`;
    remainingByPayload.set(key, (remainingByPayload.get(key) ?? 0) + 1);
  }

  const browserScreenshotImageIndices: number[] = [];
  images.forEach((image, index) => {
    const key = `${image.mimeType}\u0000${image.base64}`;
    const remaining = remainingByPayload.get(key) ?? 0;
    if (remaining <= 0) return;
    browserScreenshotImageIndices.push(index);
    remainingByPayload.set(key, remaining - 1);
  });
  return browserScreenshotImageIndices.length > 0 ? { browserScreenshotImageIndices } : {};
}

/**
* When there are multiple CUA calls in a cell, the last one with established identity is used, which is consistent with the existing last-write-wins of `_meta`.
* Calls that do not generate primary (items mode of list_apps, none of request_access/stop) will not
* Enter this array, so the identity of the previous action will not be overwritten.
 */
function latestCuaAppResult(
  cuaApps: readonly NodeReplCuaAppIdentity[],
): Pick<NodeReplRunResult, "cuaApp"> {
  const cuaApp = cuaApps.at(-1);
  return cuaApp ? { cuaApp } : {};
}

/**
* NodeReplSession: A persistent JavaScript execution engine within the caller process.
 *
* One instance per session; the sandbox is the globalThis seen by the model and maintains state between multiple runs().
* Top-level await is executed by async IIFE, dynamic loading is completed through injected importModule(), and the results are returned in a unified structure.
 *
* Security note: VM is not a security sandbox, and isolation is still borne by the upper-level permissions and approval gate. The restricted process facade is only responsible for
* Limit the direct impact of cell on process control and MCP stdio; third-party modules are still executed in the host Node realm.
 */
export class NodeReplSession {
  private context: Context;
  private readonly createInjectedGlobals: () => Record<PropertyKey, unknown>;
  private readonly restrictedProcess: Readonly<Record<string, unknown>> | undefined;
  private currentSink: NodeReplWriteSink | null = null;
  private nodeReplApi: {
    requestMeta: NodeReplRequestMeta;
    emitStructuredResult: (result: unknown) => void;
  } | null = null;
  private disposed = false;
  private cleanupContextResources: () => void = () => undefined;
/** Execution + persistence strategy (default route B); switching executor means switching routes, and the run body remains unchanged. */
  private readonly executor: ReplExecutor;

  constructor(options: NodeReplSessionOptions = {}) {
    const injectedGlobals = options.injectedGlobals;
    this.createInjectedGlobals =
      typeof injectedGlobals === "function" ? injectedGlobals : () => injectedGlobals ?? {};
    this.restrictedProcess = options.restrictProcess ? createRestrictedProcessFacade() : undefined;
    this.executor = new IifeContextExecutor();
    this.context = this.buildContext();
  }

  private buildContext(): Context {
    const timeouts = new Set<ReturnType<typeof setTimeout>>();
    const intervals = new Set<ReturnType<typeof setInterval>>();
    const scopedSetTimeout = (
      callback: (...args: unknown[]) => void,
      delay?: number,
      ...args: unknown[]
    ) => {
      const handle = setTimeout(() => {
        timeouts.delete(handle);
        callback(...args);
      }, delay);
      timeouts.add(handle);
      return handle;
    };
    const scopedClearTimeout = (handle: ReturnType<typeof setTimeout>) => {
      timeouts.delete(handle);
      clearTimeout(handle);
    };
    const scopedSetInterval = (
      callback: (...args: unknown[]) => void,
      delay?: number,
      ...args: unknown[]
    ) => {
      const handle = setInterval(callback, delay, ...args);
      intervals.add(handle);
      return handle;
    };
    const scopedClearInterval = (handle: ReturnType<typeof setInterval>) => {
      intervals.delete(handle);
      clearInterval(handle);
    };
    this.cleanupContextResources = () => {
      for (const handle of timeouts) clearTimeout(handle);
      for (const handle of intervals) clearInterval(handle);
      timeouts.clear();
      intervals.clear();
    };
// console tee to the current sink (set before each run), while retaining the original console for debugging.
    const teeConsole = {
      log: (...args: unknown[]) => this.emit(args),
      info: (...args: unknown[]) => this.emit(args),
      warn: (...args: unknown[]) => this.emit(args),
      error: (...args: unknown[]) => this.emit(args),
      debug: (...args: unknown[]) => this.emit(args),
    };
// Use the current working directory as the require base.
    const req = createReplRequire(createRequire(`${process.cwd()}/`), this.restrictedProcess);
    const nodeReplApi = {
      cwd: process.cwd(),
      homeDir: homedir(),
      tmpDir: tmpdir(),
      requestMeta: {} as NodeReplRequestMeta,
      write: (text: string) => this.emit([text]),
// Return the image (such as the tab.screenshot result) to the model as an image content block; accepts bytes/base64/dataUrl.
      emitImage: (image: unknown) => this.emitImage(image),
// SDK results must go through the structured channel; otherwise the model's console.log will split image/image_ref into ordinary text.
// The official CUA's accurate frame check cannot confirm that the two blocks are still adjacent and have not been overwritten.
      emitStructuredResult: (result: unknown) => this.emitStructuredResult(result),
      setResponseMeta: (meta: unknown) => this.setResponseMeta(meta),
    };
    this.nodeReplApi = nodeReplApi;
    const sandbox: Record<PropertyKey, unknown> = {
      console: teeConsole,
      process: this.restrictedProcess ?? process,
      Buffer,
      URL,
      URLSearchParams,
      TextEncoder,
      TextDecoder,
      setTimeout: scopedSetTimeout,
      clearTimeout: scopedClearTimeout,
      setInterval: scopedSetInterval,
      clearInterval: scopedClearInterval,
      queueMicrotask,
      structuredClone,
      require: req,
// Dynamically load modules. Use injected functions instead of bare import(): import() in vm requires --experimental-vm-modules
// Start the flag (the agent is a packaged binary, so it is inconvenient to add a flag); the importModule goes directly to the host import(), no flag is required.
// Models and browser skills use standard await import("..."); the executor will be safely rewritten to the loader according to the AST.
      importModule: (specifier: string) => this.importModule(specifier),
      nodeRepl: nodeReplApi,
// Rebuild the browser-client facade every time kernel reset; browser/tab binding in the old context
// Cannot drift to new generation across reset, keep new session bootstrap boundaries consistent.
      ...this.createInjectedGlobals(),
    };
    const context = createContext(sandbox);
// globalThis is self-referential, making the model available globalThis.x = ... for explicit persistence.
    runInContext("globalThis.globalThis = globalThis;", context);
    return context;
  }

  private emit(args: unknown[]): void {
    if (!this.currentSink) {
      return;
    }
    const text = args
      .map((a) => (typeof a === "string" ? a : (stringifyReplResult(a) ?? "undefined")))
      .join(" ");
    this.currentSink.write(text);
  }

  /**
* Collect an image to the output of this run (for formatModelContent to convert into an image content block for the model).
* Accepts Uint8Array/Buffer/number[], data URL, or { base64 | bytes, mimeType }.
* Throw TypeError when the input parameter is illegal, so that the model can directly see the usage error.
   */
  private emitImage(image: unknown): void {
    if (!this.currentSink) {
      return;
    }
    const directBytes =
      (ArrayBuffer.isView(image) && !(image instanceof DataView)) || Array.isArray(image)
        ? (image as Uint8Array | number[])
        : null;
    const rec =
      directBytes !== null
        ? { bytes: directBytes }
        : ((image ?? {}) as {
            base64?: unknown;
            bytes?: unknown;
            dataUrl?: unknown;
            mimeType?: unknown;
          });
    let base64: string | null = null;
    let dataUrlMimeType: string | undefined;
    if (typeof rec.dataUrl === "string") {
      const match = /^data:([^;,]+);base64,(.+)$/u.exec(rec.dataUrl);
      if (match) {
        dataUrlMimeType = match[1];
        base64 = match[2];
      }
    } else if (typeof rec.base64 === "string" && rec.base64.length > 0) {
      base64 = rec.base64;
    } else if (rec.bytes != null) {
      try {
        base64 = Buffer.from(rec.bytes as Uint8Array | readonly number[]).toString("base64");
      } catch {
        base64 = null;
      }
    }
    if (!base64) {
      throw new TypeError(
        "nodeRepl.emitImage requires bytes, dataUrl, { base64 }, or { bytes }; e.g. emitImage(await tab.screenshot())",
      );
    }
    const mimeType =
      typeof rec.mimeType === "string" && rec.mimeType
        ? rec.mimeType
        : (dataUrlMimeType ?? "image/png");
    this.currentSink.images.push({ base64, mimeType });
  }

  private setResponseMeta(meta: unknown): void {
    if (!this.currentSink) return;
    if (!meta || typeof meta !== "object" || Array.isArray(meta)) {
      throw new TypeError("nodeRepl.setResponseMeta requires a plain object");
    }
    Object.assign(this.currentSink.responseMeta, meta as Record<string, unknown>);
  }

  private emitStructuredResult(result: unknown): void {
    if (!this.currentSink) return;
    if (!result || typeof result !== "object" || Array.isArray(result)) {
      throw new TypeError("nodeRepl.emitStructuredResult requires a result object");
    }
    const candidate = result as { content?: unknown };
    if (!Array.isArray(candidate.content)) {
      throw new TypeError("nodeRepl.emitStructuredResult requires a content array");
    }
    for (const block of candidate.content) {
      if (!block || typeof block !== "object" || typeof (block as { type?: unknown }).type !== "string") {
        throw new TypeError("nodeRepl.emitStructuredResult content blocks require a type");
      }
    }
    this.currentSink.structuredResults.push(result as NodeReplStructuredResult);
  }

/** browser-client transport merges backend meta into tool results within the same js run. */
  mergeResponseMeta(meta: Record<string, unknown>): void {
    if (!this.currentSink) return;
    const currentSurface = this.currentSink.responseMeta["zcode/toolSurface"];
    const nextSurface = meta["zcode/toolSurface"];

// At the end of the cell, openTabIds/sessionEnded of the last successful side effect is appended; automatic preview and follow-up
// Title/url/domSnapshot reading cannot overwrite the previous action meta.
    if (
      currentSurface &&
      typeof currentSurface === "object" &&
      !Array.isArray(currentSurface) &&
      nextSurface &&
      typeof nextSurface === "object" &&
      !Array.isArray(nextSurface)
    ) {
      this.setResponseMeta({
        ...meta,
        "zcode/toolSurface": {
          ...(currentSurface as Record<string, unknown>),
          ...(nextSurface as Record<string, unknown>),
        },
      });
      return;
    }
    this.setResponseMeta(meta);
  }

/** The browser transport records the original image of the explicit screenshot of the model for source mapping of the emitImage result. */
  recordBrowserScreenshot(image: NodeReplImage): void {
    if (!this.currentSink) return;
    this.currentSink.browserScreenshots.push(image);
  }

  /**
* CUA bridge records the target application identity of this call from the broker response.
   *
* Deliberately not put in sandbox globals (compare write/emitImage/emitStructuredResult/setResponseMeta):
* The tool card displays the App icon accordingly, and the model can claim to have operated other applications if it can be written.
   */
  recordCuaAppIdentity(app: NodeReplCuaAppIdentity): void {
    if (!this.currentSink) return;
    this.currentSink.cuaApps.push(app);
  }

/** Execute a piece of code; signal supports cancellation (timeout/stop). */
  async run(
    code: string,
    options: {
      signal?: AbortSignal;
      requestMeta?: NodeReplRequestMeta;
      syncTimeoutMs?: number;
    } = {},
  ): Promise<NodeReplRunResult> {
    if (this.disposed) {
      return {
        logs: "",
        error: { name: "DisposedError", message: "REPL session released" },
      };
    }
    let buffer = "";
    const images: NodeReplImage[] = [];
    const browserScreenshots: NodeReplImage[] = [];
    const structuredResults: NodeReplStructuredResult[] = [];
    const responseMeta: Record<string, unknown> = {};
    const cuaApps: NodeReplCuaAppIdentity[] = [];
    if (this.nodeReplApi) {
      this.nodeReplApi.requestMeta = { ...options.requestMeta };
    }
    this.currentSink = {
      write: (text: string) => {
        buffer += (buffer ? "\n" : "") + text;
      },
      images,
      browserScreenshots,
      structuredResults,
      responseMeta,
      cuaApps,
    };
    try {
//Delegate execution to executor (default route B: instrument top-level declaration → async-IIFE → runInContext),
// Make top-level const/let/var/function/class persistent across js calls (copied to globalThis).
      const value = await this.executor.run(
        code,
        this.context,
        options.signal,
        options.syncTimeoutMs,
      );
      return {
        result: stringifyReplResult(value),
        logs: buffer,
        ...(images.length > 0 ? { images } : {}),
        ...browserScreenshotIndexResult(images, browserScreenshots),
        ...(structuredResults.length > 0 ? { structuredResults } : {}),
        ...(Object.keys(responseMeta).length > 0 ? { responseMeta } : {}),
        ...latestCuaAppResult(cuaApps),
      };
    } catch (error) {
// Note: Errors thrown in the vm context belong to different realms, and the `instanceof Error` on the host side is false.
// Therefore, it is extracted according to the error-like structure (with name/message/stack fields) instead of packaging into a new Error
// (The wrapper will turn the message into "Error: boom" and lose the original message).
      const normalized = normalizeReplError(error);
      const mustResetKernel =
        normalized.name === "AbortError" ||
        normalized.name === "TimeoutError" ||
        normalized.message.includes("Script execution timed out");
      if (mustResetKernel && !this.disposed) {
// Promise.race can only stop waiting, the old async continuation/timer may still be on the same global
// Continue writing status. After cancellation or VM timeout, the entire context is discarded and the timers of the generation are cleared;
// Subsequent calls can only see the fresh kernel and cannot observe late mutations.
        this.cleanupContextResources();
        this.context = this.buildContext();
// Silently rebuilding the context will cause the model to continue calling cleared browser/tab variables.
// Expand a timeout into a continuous ReferenceError. The error itself must expose reset semantics and recovery actions.
        normalized.message = `${normalized.message}; kernel reset, all previous bindings were cleared; reinitialize browser/tab bindings before rerunning`;
      }
      return {
        logs: buffer,
        error: normalized,
        ...(images.length > 0 ? { images } : {}),
        ...browserScreenshotIndexResult(images, browserScreenshots),
        ...(structuredResults.length > 0 ? { structuredResults } : {}),
        ...(Object.keys(responseMeta).length > 0 ? { responseMeta } : {}),
        ...latestCuaAppResult(cuaApps),
      };
    } finally {
      this.currentSink = null;
      if (this.nodeReplApi) {
        this.nodeReplApi.requestMeta = {};
      }
    }
  }

  private async importModule(specifier: string): Promise<unknown> {
    if (this.restrictedProcess && PROCESS_MODULE_IDS.has(specifier)) {
      return { ...this.restrictedProcess, default: this.restrictedProcess };
    }
    return await import(specifier);
  }

/** Release resources. */
  dispose(): void {
    this.disposed = true;
    this.cleanupContextResources();
    this.currentSink = null;
  }
}
