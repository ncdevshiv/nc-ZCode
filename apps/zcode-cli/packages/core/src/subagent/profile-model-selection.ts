import { parseSubagentMarkdownSelection, type ModelSelection } from "@zcode/shared";

/** Host/Agent share the official Markdown codec; Provider migration must be completed at the user storage boundary first. */
export function resolveProfileModelSelection(
  frontmatter: Record<string, unknown>,
): ModelSelection | undefined {
  return parseSubagentMarkdownSelection(frontmatter);
}
