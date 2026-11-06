import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";

const JsInputBaseShape = {
// The same model schema serves both persistent core REPL and fresh-kernel Browser Use MCP;
// Field copy cannot promise cross-call status for any execution boundary, and the life cycle is explained by the respective tool description.
  code: z.string().describe("JavaScript code to execute in the Node REPL session"),
// Writing only optional does not allow the model to determine when to overwrite the default value, and long waits can easily time out after the side effects are completed.
  timeout_ms: z
    .number()
    .int()
    .positive()
    .max(120_000)
    .optional()
    .describe(
      "Per-call timeout in milliseconds. You MUST provide this when the code is expected to run longer than 30000 ms, including all awaited operations. Set it to at least the estimated total runtime plus 15000 ms. If that exceeds the 120000 ms maximum, split the work into multiple calls.",
    ),
};
const JsUserTitleSchema = z
  .string()
  .min(1)
  .max(120)
  .describe(
    "Required short user-facing title in the user's language that describes the intended action without implementation terms such as js, JavaScript, or node_repl",
  );

/** js: Execute a piece of JS code in the persistent REPL. */
export const JsInputSchema = z
  .object({
    ...JsInputBaseShape,
    title: JsUserTitleSchema,
  })
  .strict();
export type JsInput = z.infer<typeof JsInputSchema>;

// New calls must provide a user-readable header, but historical calls from old sessions and third-party providers may not have this field.
export const JsRuntimeInputSchema = z
  .object({
    ...JsInputBaseShape,
    title: JsUserTitleSchema.optional(),
  })
  .strict();
export type JsRuntimeInput = z.infer<typeof JsRuntimeInputSchema>;

export const JsOutputSchema = z
  .object({
    result: z.string().optional(),
    logs: z.string(),
    error: z
      .object({
        name: z.string(),
        message: z.string(),
        stack: z.string().optional(),
      })
      .optional(),
// Images collected by nodeRepl.emitImage (such as screenshots of tab.screenshot); formatModelContent will be converted into image content blocks for the model.
    images: z.array(z.object({ base64: z.string(), mimeType: z.string() }).strict()).optional(),
// Model explicit tab.screenshot() Absolute path to the session artifact of the original PNG.
    browserScreenshotPaths: z.array(z.string()).optional(),
    responseMeta: z.record(z.string(), z.unknown()).optional(),
  })
  .strict();
export type JsOutput = z.infer<typeof JsOutputSchema>;

export const JsInputJsonSchema = toToolJsonSchema(JsInputSchema);
export const JsOutputJsonSchema = toToolJsonSchema(JsOutputSchema);
