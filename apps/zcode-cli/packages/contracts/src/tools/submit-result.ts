// ============================================================
// submit_result Tool - actor terminal structured-result submission
// ============================================================
// The workflow actor (sub-AgentRuntime) uses it to submit the structured results of this ask. The declared input is universal
// Arbitrary JSON (single `result` attribute) - the specific schema for each ask is not declared in the tool, but is appended to the ask
// The epilogue of the command message is issued to maintain the frozen-per-actor tool cache invariant.

import { z } from "zod";
import { TOOL_JSON_SCHEMA_VERSION, toToolJsonSchema } from "./json-schema.js";

export const SUBMIT_RESULT_TOOL_NAME = "submit_result";

export const SubmitResultInputSchema = z
  .object({
// Intentionally generic: no constraints on the shape of result. The specific per-ask schema is in the ask command epilogue,
// Verified by the engine on the WorkflowSubmitPort side; this way the tool statement can cross ask freezes and hit the prompt cache.
    result: z
      .unknown()
      .describe(
        "The structured result for this ask, matching the JSON schema given in the ask instructions.",
      ),
  })
  .strict();

export type SubmitResultInput = z.infer<typeof SubmitResultInputSchema>;

export const SubmitResultInputJsonSchema = toToolJsonSchema(SubmitResultInputSchema);

/**
* The typed tool for mono subagent declares: The subschema of `result` is the only ask result schema for this actor. The zod verification at runtime is still the same as above
* Generic `SubmitResultInputSchema` - per-ask shapes are verified by the engine on the WorkflowSubmitPort side, tool declarations just let
* The provider sees (and natively constrains if supported) it. The outer object has the same shape as the general declaration: a single required `result`, no redundant keys.
 */
export function typedSubmitResultInputSchema(
  resultSchema: Record<string, unknown>,
): Record<string, unknown> {
  return {
    type: "object",
    properties: {
      result: {
        description: "The structured result for this ask.",
        ...resultSchema,
      },
    },
    required: ["result"],
    additionalProperties: false,
    $schema: TOOL_JSON_SCHEMA_VERSION,
  };
}

export const SubmitResultOutputSchema = z
  .object({
    status: z.literal("accepted"),
  })
  .strict();

export type SubmitResultOutput = z.infer<typeof SubmitResultOutputSchema>;

export const SubmitResultOutputJsonSchema = toToolJsonSchema(SubmitResultOutputSchema);
