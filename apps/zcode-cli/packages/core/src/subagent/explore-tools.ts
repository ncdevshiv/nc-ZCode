// Align the tool surface of the file search subagent. direct branch will expose Glob/Grep;
//The embedded search branch takes over the search through Bash find/grep. NOTE: The whitelist intentionally does not contain any
// File writing tool (Write/Edit/ApplyPatch), so Bash is the only side effect entry, and the read-only semantics are restricted by the Explore prompt.
export const EXPLORE_AGENT_ALLOWED_TOOLS = [
  "Bash",
  "Glob",
  "Grep",
  "Read",
  "WebFetch",
  "WebSearch",
  "TodoWrite",
] as const;

export type ExploreAgentAllowedTool = (typeof EXPLORE_AGENT_ALLOWED_TOOLS)[number];

export const EXPLORE_AGENT_EMBEDDED_SEARCH_ALLOWED_TOOLS = [
  "Bash",
  "Read",
  "WebFetch",
  "WebSearch",
  "TodoWrite",
] as const;

const EXPLORE_AGENT_DESCRIPTION_TOOL_PRIORITY = [
  "Glob",
  "Grep",
  "Read",
  "Bash",
  "WebFetch",
  "WebSearch",
  "TodoWrite",
] as const satisfies readonly ExploreAgentAllowedTool[];
const EXPLORE_AGENT_DESCRIPTION_TOOL_PRIORITY_SET = new Set<ExploreAgentAllowedTool>(
  EXPLORE_AGENT_DESCRIPTION_TOOL_PRIORITY,
);

export function buildExploreAllowedTools(options: {
  embeddedSearchEnabled?: boolean;
} = {}): readonly ExploreAgentAllowedTool[] {
  return options.embeddedSearchEnabled
    ? EXPLORE_AGENT_EMBEDDED_SEARCH_ALLOWED_TOOLS
    : EXPLORE_AGENT_ALLOWED_TOOLS;
}

export function formatExploreAllowedToolsForAgentDescription(options: {
  embeddedSearchEnabled?: boolean;
} = {}): string {
  const allowedTools = buildExploreAllowedTools(options);
  const allowedToolSet = new Set(allowedTools);
  const prioritizedTools = EXPLORE_AGENT_DESCRIPTION_TOOL_PRIORITY.filter((tool) =>
    allowedToolSet.has(tool),
  );
  const unprioritizedTools = allowedTools.filter(
    (tool) => !EXPLORE_AGENT_DESCRIPTION_TOOL_PRIORITY_SET.has(tool),
  );
  return [...prioritizedTools, ...unprioritizedTools].join(", ");
}
