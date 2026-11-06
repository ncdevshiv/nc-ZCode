import { inspect } from "node:util";

export const PROCESS_MODULE_IDS = new Set(["process", "node:process"]);

export function createRestrictedProcessFacade(): Readonly<Record<string, unknown>> {
  return Object.freeze({
    arch: process.arch,
    argv: Object.freeze([...process.argv]),
    cwd: () => process.cwd(),
    env: Object.freeze({ ...process.env }),
    execArgv: Object.freeze([...process.execArgv]),
    execPath: process.execPath,
    hrtime: process.hrtime.bind(process),
    memoryUsage: process.memoryUsage.bind(process),
    nextTick: process.nextTick.bind(process),
    pid: process.pid,
    platform: process.platform,
    release: Object.freeze({ ...process.release }),
    resourceUsage: process.resourceUsage.bind(process),
    uptime: process.uptime.bind(process),
    version: process.version,
    versions: Object.freeze({ ...process.versions }),
  });
}

export function createReplRequire(
  baseRequire: NodeJS.Require,
  restrictedProcess: Readonly<Record<string, unknown>> | undefined,
): NodeJS.Require {
  if (!restrictedProcess) return baseRequire;
  const facade = ((specifier: string) =>
    PROCESS_MODULE_IDS.has(specifier)
      ? restrictedProcess
      : baseRequire(specifier)) as NodeJS.Require;
  facade.cache = baseRequire.cache;
  facade.extensions = baseRequire.extensions;
  facade.main = baseRequire.main;
  facade.resolve = baseRequire.resolve;
  return facade;
}

export function stringifyReplResult(value: unknown): string | undefined {
  if (value === undefined) return undefined;
  if (typeof value === "string") return value;
  if (ArrayBuffer.isView(value) && !(value instanceof DataView)) {
// screenshot() returns Uint8Array; JSON.stringify will expand each byte into a numeric key,
// Therefore use bounded inspect to avoid the binary result from swelling into a large block of numeric text.
    return inspect(value, { maxArrayLength: 100 });
  }
  try {
    const json = JSON.stringify(value, null, 2);
    return json === undefined ? String(value) : json;
  } catch {
    return String(value);
  }
}

/** The Errors of the vm context belong to different realms and are extracted according to the error-like field to avoid misjudgment by instanceof. */
export function normalizeReplError(error: unknown): {
  name: string;
  message: string;
  stack?: string;
} {
  if (error && typeof error === "object") {
    const value = error as { name?: unknown; message?: unknown; stack?: unknown };
    const name = typeof value.name === "string" ? value.name : "Error";
    const message = typeof value.message === "string" ? value.message : String(error);
    const stack = typeof value.stack === "string" ? value.stack : undefined;
    return { name, message, stack };
  }
  return { name: "Error", message: String(error) };
}
