const EMPTY_TOOL_NAME_PLACEHOLDER = "empty_tool_name";

/**
* Empty tool name restoration only serves model continuations and should not be materialized into user-visible tool rows.
* The fixed placeholder string may also be the legal tool name of registry / MCP / alias, and cannot be used as a basis for hiding alone.
* live only recognizes the original empty name; cold/legacy also requires that the placeholder name and the persistent original name metadata exist at the same time.
 */
export function shouldHideInvalidToolCallFromProduct(
  toolName: unknown,
  metadata?: Record<string, unknown>,
): boolean {
  if (typeof toolName !== "string") return false;
  if (toolName.trim().length === 0) return true;

  const providerToolName = metadata?.providerToolName;
  return (
    toolName === EMPTY_TOOL_NAME_PLACEHOLDER &&
    providerToolName !== undefined &&
    typeof providerToolName === "string" &&
    providerToolName.trim().length === 0
  );
}
