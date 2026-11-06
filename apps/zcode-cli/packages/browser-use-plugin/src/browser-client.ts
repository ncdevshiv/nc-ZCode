// Importing from the @zcode/core root barrel will cause esbuild to track the Agent/tool ​​module with top-level side effects,
// Integrate the Bash registry, subagent and runtime into the official plug-in. Browser publications can only rely on narrow subpaths.
import { setupBrowserRuntime as setupCoreBrowserRuntime } from "@zcode/core/browser-client";
import { readNodeReplBrowserRuntimeBridge } from "@zcode/node-repl-host/runtime-bridge";

export async function setupBrowserRuntime(input: {
  globals: Record<PropertyKey, unknown>;
}): Promise<void> {
  const bridge = readNodeReplBrowserRuntimeBridge(input.globals);
  bridge.assertAvailable();
  setupCoreBrowserRuntime({
    globals: input.globals as Record<string, unknown>,
    transport: bridge,
    documentationRoot: bridge.documentationRoot,
    assertAvailable: bridge.assertAvailable,
  });
}
