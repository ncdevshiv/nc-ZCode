// ============================================================
// ResolveWorkflowQuestion Tool - The main agent answers blocking questions caused by actor upgrades
// ============================================================
// See port `DynamicWorkflowRunPort.resolveQuestion`
// （interfaces/dynamic-workflow-run.port.ts）。
//
// Only accept qid but not `(run_id, question_id)` Right: qid is globally unique (across runs), and the model is used when multiple runs are concurrent
// Pairing yourself is a breeding ground for mismatches. In case of failure, ToolHandlerFailure (core side) is used, so this output is only successful.

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";

export const RESOLVE_WORKFLOW_QUESTION_TOOL_NAME = "ResolveWorkflowQuestion";

export const ResolveWorkflowQuestionInputSchema = z
  .object({
// snake_case is based on the run_id of GetWorkflowRun / the run_id of ResumeWorkflowRun: in the eyes of the model, this is
// Opaque identification keys of the same family.
    question_id: z
      .string()
      .min(1)
      .describe(
        "The question ID from the escalation notification, or from GetWorkflowRun's pending questions (looks like `dwfq-...`)",
      ),
    answer: z
      .string()
      .min(1)
      .describe("The answer text, delivered verbatim to the subagent that asked"),
  })
  .strict();

export type ResolveWorkflowQuestionInput = z.infer<typeof ResolveWorkflowQuestionInputSchema>;

export const ResolveWorkflowQuestionInputJsonSchema = toToolJsonSchema(
  ResolveWorkflowQuestionInputSchema,
);

export const ResolveWorkflowQuestionOutputSchema = z
  .object({
    ok: z.literal(true),
    qid: z.string().min(1),
/** Confirmation copy for the model (prose is not a contract field, schema only guarantees presence). */
    response: z.string(),
  })
  .strict();

export type ResolveWorkflowQuestionOutput = z.infer<typeof ResolveWorkflowQuestionOutputSchema>;

export const ResolveWorkflowQuestionOutputJsonSchema = toToolJsonSchema(
  ResolveWorkflowQuestionOutputSchema,
);
