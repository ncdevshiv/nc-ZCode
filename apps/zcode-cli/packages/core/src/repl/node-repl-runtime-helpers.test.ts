import { describe, expect, it } from "bun:test";
import {
  createRestrictedProcessFacade,
  createReplRequire,
  normalizeReplError,
  stringifyReplResult,
  PROCESS_MODULE_IDS,
} from "./node-repl-runtime-helpers.js";

describe("node-repl-runtime-helpers", () => {
  it("creates restricted process facade", () => {
    const facade = createRestrictedProcessFacade();
    expect(facade.pid).toBe(process.pid);
    expect(facade.platform).toBe(process.platform);
    expect(typeof facade.cwd).toBe("function");
    expect((facade.cwd as any)()).toBe(process.cwd());
  });

  it("createReplRequire returns base require when not restricted", () => {
    const req = createReplRequire(require, undefined);
    expect(req).toBe(require);
  });

  it("createReplRequire intercepts process module when restricted", () => {
    const facade = createRestrictedProcessFacade();
    const req = createReplRequire(require, facade);
    expect(req("process")).toBe(facade);
    expect(req("node:process")).toBe(facade);
  });

  it("stringifyReplResult handles undefined", () => {
    expect(stringifyReplResult(undefined)).toBeUndefined();
  });

  it("stringifyReplResult handles strings", () => {
    expect(stringifyReplResult("hello")).toBe("hello");
  });

  it("stringifyReplResult handles objects as JSON", () => {
    expect(stringifyReplResult({ a: 1 })).toBe('{\n  "a": 1\n}');
  });

  it("stringifyReplResult handles Uint8Array with inspect", () => {
    const arr = new Uint8Array([1, 2, 3]);
    const result = stringifyReplResult(arr);
    expect(result).toContain("Uint8Array");
  });

  it("normalizeReplError handles Error objects", () => {
    const err = new Error("test message");
    err.stack = "stack trace";
    const normalized = normalizeReplError(err);
    expect(normalized.name).toBe("Error");
    expect(normalized.message).toBe("test message");
    expect(normalized.stack).toBe("stack trace");
  });

  it("normalizeReplError handles non-objects", () => {
    const normalized = normalizeReplError("string error");
    expect(normalized.name).toBe("Error");
    expect(normalized.message).toBe("string error");
  });

  it("PROCESS_MODULE_IDS contains process and node:process", () => {
    expect(PROCESS_MODULE_IDS.has("process")).toBe(true);
    expect(PROCESS_MODULE_IDS.has("node:process")).toBe(true);
  });
});
