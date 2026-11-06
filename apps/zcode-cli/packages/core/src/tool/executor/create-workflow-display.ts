/**
* The result card display construct of CreateWorkflow. Unpacked from result-display.ts (400 lines of discipline):
* Same family as workflow-observation-display.ts - dispatched by tool name, safeParse output schema,
* The display side has independent length limit.
 */

import {
  AMEND_WORKFLOW_TOOL_NAME,
  CREATE_WORKFLOW_DISPLAY_MAX_DIAGNOSTICS,
  CREATE_WORKFLOW_DISPLAY_MAX_MESSAGE_CHARS,
  CREATE_WORKFLOW_TOOL_NAME,
  CreateWorkflowOutputSchema,
  type ToolResultDisplayPayload,
} from "@zcode/contracts";

/**
* ⚠ This projected field collection is **frozen** (the contracts schema annotation explains why). gate exclusive
* Facts - such as the source of saved run and the parsed script - all go through the ** tool to enter the parameter ** channel, not the display.
 */
export function createCreateWorkflowDisplay(
  toolName: string,
  output: unknown,
): ToolResultDisplayPayload | undefined {
// The two startup tools share the same display kind: there is only one implementation of the picture, scratch pen and diagnostic card on the UI side.
  if (toolName !== CREATE_WORKFLOW_TOOL_NAME && toolName !== AMEND_WORKFLOW_TOOL_NAME) {
    return undefined;
  }
  const parsed = CreateWorkflowOutputSchema.safeParse(output);
  if (!parsed.success) return undefined;

  const { causalityGraph, diagnostics, ok } = parsed.data;
// display does not go through the tool result budget: both the number of diagnoses and the length of a single message must be entered into the real-time event and
// Independently limit the length before persisting metadata to avoid type errors and expand continuous/replayable messages into unbounded payloads.
//The causalityGraph has been bounded by the tool output boundary (handler's boundCausalityGraph + output schema),
// Direct transparent transmission.
  const errorCount = diagnostics.length;
  const bounded = diagnostics
    .slice(0, CREATE_WORKFLOW_DISPLAY_MAX_DIAGNOSTICS)
    .map((diagnostic) => ({
      line: diagnostic.line,
      column: diagnostic.column,
      code: diagnostic.code,
      message: diagnostic.message.slice(0, CREATE_WORKFLOW_DISPLAY_MAX_MESSAGE_CHARS),
    }));
  const truncated = errorCount > bounded.length;

  return {
    kind: "create_workflow",
    ok,
    errorCount,
    diagnostics: bounded,
    ...(causalityGraph === undefined ? {} : { causalityGraph }),
    ...(truncated ? { truncated: true } : {}),
  };
}
