import type { McpServerStatus } from "@zcode/contracts";
import { matchesModelVisibleMcpServerName } from "../mcp/name.js";

export function matchesRequiredMcpServer(
  requiredName: string,
  statuses: Record<string, McpServerStatus>,
): boolean {
  if (requiredName === "*") {
    return Object.values(statuses).some((status) => status.status === "connected");
  }
  const expected = requiredName.toLowerCase();
  return Object.entries(statuses).some(
    ([serverName, status]) =>
      status.status === "connected" &&
// MCP tool name will be plugin:android-emulator:android-emulator
// Normalize to plugin_android-emulator_android-emulator; required-server check
// The same set of model visible naming rules must be used, otherwise the connected plugin MCP will be misjudged as missing.
      matchesModelVisibleMcpServerName(expected, serverName),
  );
}

export function extractRequiredMcpServerNames(allowedTools: readonly string[]): string[] {
  const names = new Set<string>();
  for (const tool of allowedTools) {
    const requiredName = extractRequiredMcpServerName(tool);
    if (requiredName) names.add(requiredName);
  }
  return [...names];
}

function extractRequiredMcpServerName(tool: string): string | undefined {
  const trimmed = tool.trim();
  if (trimmed === "mcp__*" || trimmed === "mcp") return "*";
  if (!trimmed.startsWith("mcp__")) return undefined;
  const [, serverName] = trimmed.split("__");
  return serverName && serverName.length > 0 ? serverName : undefined;
}
