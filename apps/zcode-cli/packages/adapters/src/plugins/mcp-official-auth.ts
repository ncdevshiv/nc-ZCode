/*
* Strict parsing and provenance generation of `auth: { type: "zcode_official" }` in Plugin `.mcp.json`.
 *
* Separately into a file instead of leaving it in mcp.ts: the official authentication parsing rules cover both http and stdio transmission methods.
* And it is not coupled with template variable parsing and transport layer field parsing - putting them together will only make mcp.ts continue to expand (it has reached the max-lines upper limit).
 */
import type { McpOfficialProvenance, ZCodeOfficialMcpAuthConfig } from "@zcode/contracts";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
* Strictly parse `auth`. If it is not declared, undefined will be returned (take the normal MCP path); if it is declared but the shape is illegal, an error will be thrown.
* Do not tolerate downgrades - downgrades will cause configurations that "appear to be equipped with official authentication but are actually anonymous requests" to go online silently.
 */
export function parseZCodeOfficialAuth(
  value: unknown,
  mcpKey: string,
): ZCodeOfficialMcpAuthConfig | undefined {
  if (value === undefined) return undefined;
  if (!isRecord(value)) {
    throw new Error(`MCP server ${mcpKey}: auth must be an object`);
  }
// Exact value matching, case-sensitive; aliases such as zcode-official, zcode_official_auth, etc. are rejected.
  if (value.type !== "zcode_official") {
    throw new Error(`MCP server ${mcpKey}: unsupported auth type: ${String(value.type)}`);
  }
  if (value.provider !== "jwt_token") {
    throw new Error(`MCP server ${mcpKey}: unsupported auth provider: ${String(value.provider)}`);
  }
  return { type: "zcode_official", provider: "jwt_token" };
}

/**
* Host-generated runtime provenance. The `official` field written in `.mcp.json` will also be overwritten by it——
*Official status cannot be declared by the party under review itself.
 */
export function buildOfficialProvenance(identity: {
  mcpKey: string;
  pluginId: string;
}): McpOfficialProvenance {
  return { mcpKey: identity.mcpKey, pluginId: identity.pluginId, source: "plugin" };
}
