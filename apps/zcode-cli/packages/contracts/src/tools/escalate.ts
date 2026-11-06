// ============================================================
// escalate Tool - actor escalates blocking issues to the main agent
// ============================================================
// Inject actor session completely isomorphically with `submit_result`
// (The port is registered when present, and is still filled in by core under `tools:"none"`), but the two settle different things:
// Submit settles **the result of this ask**, escalate settles **one question and answer**.
//
// The output shape is deliberately "flat object with discriminant bits" rather than union: both are normal tool results (not errors),
// The model only reads a piece of text, and the discriminant bit `status` only serves logs and result projections.

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";

export const ESCALATE_TOOL_NAME = "escalate";

export const EscalateInputSchema = z
  .object({
    question: z
      .string()
      .min(1)
      .describe(
        "The single focused question that unblocks you. One question per call, answerable in a sentence.",
      ),
    context: z
      .string()
      .optional()
      .describe(
        "What you already tried and where exactly you are stuck — the evidence the answerer needs.",
      ),
  })
  .strict();

export type EscalateInput = z.infer<typeof EscalateInputSchema>;

export const EscalateInputJsonSchema = toToolJsonSchema(EscalateInputSchema);

/**
* An upgraded ending.
 *
* - `answered`: `message` is the original answer given by the main agent, `qid` is the globally unique id of this question and answer.
* - `refused`: `message` is a copy stating the current situation and the next step (the budget is exhausted / nothing is flying ask), `reason` is
* Discrimination key. **This is not an error** - rendering "budget exhausted" as an error tool_result will make the model appear to be retryable
* Glitch, hitting the same wall repeatedly, and that's exactly the behavior this feature is meant to eliminate.
 */
export const EscalateOutputSchema = z
  .object({
    status: z.enum(["answered", "refused"]),
    message: z.string(),
/** Only present if `answered` is present. */
    qid: z.string().optional(),
/** Only present if `refused` is present. */
    reason: z.enum(["budget_exhausted", "no_active_ask"]).optional(),
  })
  .strict();

export type EscalateOutput = z.infer<typeof EscalateOutputSchema>;

export const EscalateOutputJsonSchema = toToolJsonSchema(EscalateOutputSchema);
