import { join } from "node:path";
import type { AgentRuntimeConfig } from "@zcode/core";
import type { PluginLoadOutcome } from "@zcode/contracts";
import {
  OFFICIAL_BROWSER_USE_PLUGIN_ID,
  OFFICIAL_CUA_PLUGIN_ID,
} from "./official-plugin-definitions.js";

type RuntimeFeaturesConfig = NonNullable<AgentRuntimeConfig["runtimeFeatures"]>;

export function resolvePluginRuntimeFeatures(
  pluginOutcome: Pick<PluginLoadOutcome, "plugins">,
): RuntimeFeaturesConfig {
  const browserUsePlugin = pluginOutcome.plugins.find(
    (plugin) => plugin.id === OFFICIAL_BROWSER_USE_PLUGIN_ID && plugin.enabled,
  );
  const cuaPlugin = pluginOutcome.plugins.find(
    (plugin) => plugin.id === OFFICIAL_CUA_PLUGIN_ID && plugin.enabled,
  );
  if (!browserUsePlugin && !cuaPlugin) {
    return {};
  }
  return {
// Node REPL has been moved to the real MCP server; only BrowserControlPort injection is enabled here, and core bare js* is no longer registered.
    ...(browserUsePlugin ? { browserUse: true } : {}),
    ...(browserUsePlugin
      ? { browserDocumentationRoot: join(browserUsePlugin.rootPath, "docs") }
      : {}),
// CUA and Browser Use share the same stateless node_repl MCP server; only CUA is recorded here
// Whether the SDK needs to be injected into the broker by bootstrap. Do not set nodeRepl: that field will be re-registered
// The core bare js tool before migration has resulted in duplicate projection of MCP and core.
    ...(cuaPlugin ? { computerUse: true } : {}),
  };
}
