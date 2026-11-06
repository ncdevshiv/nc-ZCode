import type { McpServerConfig, PluginLoadOutcome } from "@zcode/contracts";
import { createBundledMcpRuntimeConfig } from "./official-plugin-runtime.js";
import {
  OFFICIAL_BROWSER_USE_PLUGIN_ID,
  OFFICIAL_CUA_PLUGIN_ID,
  OFFICIAL_NODE_REPL_HOST_PLUGIN_ID,
} from "./official-plugin-definitions.js";

const BUILT_IN_NODE_REPL_SERVER_NAME = "node_repl";

/**
* node_repl is a host tool shared by official capabilities, not a plugin MCP declared in any plugin manifest. Products are produced by independent
* `node-repl-host` seed unit is carried (no listing, not exposed to users); Browser Use and Computer Use
* Each contributes only its own skills, docs and native dependencies root.
 */
export function resolveBuiltInNodeReplMcpServers(input: {
  pluginOutcome: Pick<PluginLoadOutcome, "plugins">;
  workingDirectory: string;
}): Record<string, McpServerConfig> {
  const browserUsePackage = input.pluginOutcome.plugins.find(
    (plugin) => plugin.id === OFFICIAL_BROWSER_USE_PLUGIN_ID && plugin.enabled,
  );
  const cuaPackage = input.pluginOutcome.plugins.find(
    (plugin) => plugin.id === OFFICIAL_CUA_PLUGIN_ID && plugin.enabled,
  );
  if (!browserUsePackage && !cuaPackage) return {};
// The host itself does not participate in the activation judgment: it has no skill and is not exposed to the user. Its absence means that there is no host to run.
// It must be safe not to register, rather than falling back to an old product in a plug-in package.
  const hostPackage = input.pluginOutcome.plugins.find(
    (plugin) => plugin.id === OFFICIAL_NODE_REPL_HOST_PLUGIN_ID,
  );
  if (!hostPackage) return {};

  const nodeRepl = createBundledMcpRuntimeConfig({
    cwd: input.workingDirectory,
    env: {
// Domain roots are injected individually, and only when the corresponding capability is enabled: the host decides which half of the document is available based on this.
      ...(browserUsePackage ? { ZCODE_PLUGIN_ROOT: browserUsePackage.rootPath } : {}),
      ...(cuaPackage ? { ZCODE_CUA_PLUGIN_ROOT: cuaPackage.rootPath } : {}),
    },
    rootPath: hostPackage.rootPath,
    timeoutMs: 600_000,
  });
  if (!nodeRepl) return {};
  return {
    [BUILT_IN_NODE_REPL_SERVER_NAME]: {
      ...nodeRepl,
      isolation: "workspace",
      protocolVersion: "2026-07-28",
    },
  };
}
