import { ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME } from "@zcode/contracts";
import { filterDisallowedToolNames } from "../tool/tool-visibility.js";

const SUBAGENT_CHILD_FORCED_DISALLOWED_TOOLS = [
  ENTER_PLAN_MODE_TOOL_NAME,
  EXIT_PLAN_MODE_TOOL_NAME,
] as const;

export function buildSubagentChildDisallowRules(
  disallowedTools: readonly string[] | undefined,
): readonly string[] {
  return [...SUBAGENT_CHILD_FORCED_DISALLOWED_TOOLS, ...(disallowedTools ?? [])];
}

export function filterSubagentChildToolNames(
  toolNames: readonly string[],
  disallowedTools: readonly string[] | undefined,
): readonly string[] {
// The sub-agent does not have an independent plan approval recovery interface. Exposing plan tools will cause
// ExitPlanMode waits for user confirmation and blocks the parent turn, so all child agent tool surfaces are eliminated uniformly.
  return filterDisallowedToolNames(toolNames, buildSubagentChildDisallowRules(disallowedTools));
}
