import type { McpTrackedProcess } from "@zcode/adapters";
import type { ZCodeProcessChildProcessesResult } from "@zcode/shared";
import { resolveOfficialPluginNameByHostMcpServerName } from "../app/official-plugin-definitions.js";

/**
* `process/childProcesses`: Organize the child process mapping in the MCP process registry memory into protocol results.
* Only attribution completion (builtin host MCP → official plug-in name) is performed, without any I/O; CPU/memory sampling is completed by the desktop Host.
 */
export function listChildProcesses(
  tracked: readonly McpTrackedProcess[],
): ZCodeProcessChildProcessesResult {
  return {
    processes: tracked.map((process) => {
      const pluginName =
        process.pluginName ??
        (process.mcpSource === "builtin"
          ? resolveOfficialPluginNameByHostMcpServerName(process.serverName)
          : undefined);
      return {
        pid: process.pid,
        serverName: process.serverName,
        mcpSource: process.mcpSource,
        ...(pluginName ? { pluginName } : {}),
      };
    }),
  };
}
