import { describe, expect, it, vi } from "bun:test";
import { createTuiProcessRuntimeState, prepareTuiAppRuntime } from "./tui-prompt-handler-runtime.js";

describe("createTuiProcessRuntimeState", () => {
  it("creates empty state", () => {
    const state = createTuiProcessRuntimeState();
    expect(state.providerRegistryRuntimePromise).toBeUndefined();
    expect(state.shutdownTelemetry).toBeUndefined();
  });
});

describe("prepareTuiAppRuntime", () => {
  it("prepares app runtime with dependencies", async () => {
    const state = createTuiProcessRuntimeState();
    const deps = {
      env: { NODE_ENV: "test" },
      cwd: () => "/test",
      loadDotenv: vi.fn(() => ({ error: undefined, path: ".env" })),
      createZCodeApp: vi.fn(),
      startProcessProviderRegistryRuntime: vi.fn(async () => ({
        runtime: { registryService: {} },
        modelSelectionConfigRepository: {
          read: vi.fn(async () => ({ model: "test" })),
        },
      })),
    } as any;

    const result = await prepareTuiAppRuntime(deps, "1.0.0", { continueSession: false }, state);
    expect(result.sessionId).toBeUndefined();
    expect(result.workingDirectory).toBe("/test");
    expect(state.providerRegistryRuntimePromise).toBeDefined();
  });
});
