// ============================================================
// EvalWorkflowSnippet Tool - synchronously compile and run a dynamic workflow snippet (scratch facade)
// ============================================================
// Experimental channel for workflow creation:
// The same compilation / lowering / sandbox / world-read execution surface, memory journal, completely transient.

import { z } from "zod";
import { CreateWorkflowDiagnosticSchema } from "./create-workflow.js";
import { toToolJsonSchema } from "./json-schema.js";

export const EVAL_WORKFLOW_SNIPPET_TOOL_NAME = "EvalWorkflowSnippet";

/** The snippet wall clock defaults to 60s; the upper limit is 600s (margin is required to test the real build class check). */
export const EVAL_WORKFLOW_SNIPPET_DEFAULT_TIMEOUT_MS = 60_000;
export const EVAL_WORKFLOW_SNIPPET_MAX_TIMEOUT_MS = 600_000;
export const EVAL_WORKFLOW_SNIPPET_MIN_TIMEOUT_MS = 1_000;

/** Violation description for "given exactly one clip" (same tone as the other three tools). */
export const EVAL_WORKFLOW_SNIPPET_SOURCE_ERROR =
  "Provide exactly one snippet source: `code` for the snippet inline, or `path` for a file holding it. Passing both, or neither, is ambiguous.";

export const EvalWorkflowSnippetInputSchema = z
  .object({
    code: z
      .string()
      .min(1)
      .optional()
      .describe(
        "TypeScript snippet written against the snippet facade (files.*, git.*, log, " +
          "plain interface declarations, top-level await and return). No agent()/report(). " +
          "Provide this OR `path`, never both.",
      ),
    /**
*Second source of snippet. **The entire file is code**:
* Fragments do not preserve the set of semantics defined, and a file that happens to begin with `/* zcode-workflow` should not be stripped off as a declaration block.
     */
    path: z
      .string()
      .min(1)
      .optional()
      .describe(
        "A file holding the snippet, relative to the working directory or absolute. Provide this OR `code`, never both. The whole file is the snippet; it is read as-is, with no metadata block handling.",
      ),
    timeoutMs: z
      .number()
      .int()
      .min(EVAL_WORKFLOW_SNIPPET_MIN_TIMEOUT_MS)
      .max(EVAL_WORKFLOW_SNIPPET_MAX_TIMEOUT_MS)
      .optional()
      .describe("Wall-clock timeout for the whole snippet in milliseconds. Default 60000."),
  })
  .strict();

export type EvalWorkflowSnippetInput = z.infer<typeof EvalWorkflowSnippetInputSchema>;

export const EvalWorkflowSnippetInputJsonSchema = toToolJsonSchema(EvalWorkflowSnippetInputSchema);

// The limit of logs (all payloads on the protocol boundary are bounded): number of entries × length of a single entry, and any excess logs will be truncated and marked on the service side.
export const EVAL_WORKFLOW_SNIPPET_MAX_LOGS = 100;
export const EVAL_WORKFLOW_SNIPPET_MAX_LOG_CHARS = 2_048;
/** The upper limit of serialization of the top-level return value (harness does not measure the artifact volume - this boundary belongs to the service/tool ​​layer). */
export const EVAL_WORKFLOW_SNIPPET_MAX_ARTIFACT_BYTES = 256 * 1024;

export const EvalWorkflowSnippetOutputSchema = z
  .object({
/** Compilation is clean and the script is normal return is true; diagnostics present or run failed (timeout/throw error/cap) is false. */
    ok: z.boolean(),
    diagnostics: z.array(CreateWorkflowDiagnosticSchema),
/** Engine `log()` events are captured in order of arrival (bounded; when truncated, the last entry is the annotation). */
    logs: z.array(z.string().max(EVAL_WORKFLOW_SNIPPET_MAX_LOG_CHARS)),
/** Model-oriented text: product serialization / diagnostic list / structured failure (error code + message). */
    response: z.string(),
    durationMs: z.number().int().nonnegative(),
  })
  .strict();

export type EvalWorkflowSnippetOutput = z.infer<typeof EvalWorkflowSnippetOutputSchema>;

export const EvalWorkflowSnippetOutputJsonSchema = toToolJsonSchema(
  EvalWorkflowSnippetOutputSchema,
);
