import { z } from "zod";
import { OFFICIAL_MCP_TOOL_ERROR_CODES } from "@zcode/shared";

import {
  CREATE_WORKFLOW_DISPLAY_MAX_DIAGNOSTICS,
  CreateWorkflowCausalityGraphSchema,
  createWorkflowToolResultDisplayDiagnosticSchema,
} from "./create-workflow.js";
import {
  evalWorkflowSnippetToolResultDisplayPayloadSchema,
  getWorkflowRunToolResultDisplayPayloadSchema,
  listModelsToolResultDisplayPayloadSchema,
  listWorkflowRunsToolResultDisplayPayloadSchema,
  resumeWorkflowRunToolResultDisplayPayloadSchema,
  savedWorkflowListToolResultDisplayPayloadSchema,
} from "./workflow-observation-display.js";

export const COMPLETED_TOOL_PART_METADATA_SCHEMA_VERSION = 1;
export const TASK_OUTPUT_DISPLAY_MAX_STATUS_CHARS = 64;
export const TASK_OUTPUT_DISPLAY_MAX_OUTPUT_CHARS = 2_000;
export const MCP_TOOL_DISPLAY_MAX_NAME_CHARS = 256;
export const MCP_TOOL_DISPLAY_MAX_DESCRIPTION_CHARS = 4 * 1024;
export const CUA_TARGET_APP_DISPLAY_META_KEY = "zcode.cua/target-app-display-v1" as const;

export const applicationIconLocatorSchema = z.discriminatedUnion("kind", [
  z
    .object({ kind: z.literal("darwin-bundle-id"), value: z.string().trim().min(1).max(512) })
    .strict(),
  z
    .object({
      kind: z.literal("windows-executable-path"),
      value: z.string().trim().min(1).max(32_768),
    })
    .strict(),
  z.object({ kind: z.literal("windows-aumid"), value: z.string().trim().min(1).max(512) }).strict(),
]);

export const cuaTargetAppDisplaySchema = z
  .object({
    schemaVersion: z.literal(1),
    displayName: z.string().trim().min(1).max(512).optional(),
    iconLocators: z.array(applicationIconLocatorSchema).max(3),
  })
  .strict();

export const cuaRequestAccessStatusDisplaySchema = z
  .object({
    schemaVersion: z.literal(1),
    platform: z.literal("darwin"),
    grantOwner: z.string().trim().min(1).max(512),
    accessibility: z.enum(["granted", "stale", "denied"]),
    screenRecording: z.enum(["granted", "denied", "unknown"]),
  })
  .strict();

// The limited length constants and entries of CreateWorkflow display diagnostic schema have been moved to create-workflow.ts
// (Reused by two display payloads, create_workflow and eval_workflow_snippet, it will form a loop if placed here).

export const toolResultDisplayDiffHunkSchema = z
  .object({
    oldStart: z.number().int(),
    oldLines: z.number().int(),
    newStart: z.number().int(),
    newLines: z.number().int(),
    lines: z.array(z.string()),
  })
  .strict();

export const fileDiffToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("file_diff"),
    filePath: z.string().min(1),
    additions: z.number().int().nonnegative(),
    deletions: z.number().int().nonnegative(),
    structuredPatch: z.array(toolResultDisplayDiffHunkSchema),
    truncated: z.boolean().optional(),
  })
  .strict();

export const localAgentMessageToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("local_agent_message"),
    status: z.enum(["success", "failed"]),
    error: z.string().optional(),
    message: z.string().optional(),
  })
  .strict();

export const taskStopToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("task_stop"),
    taskId: z.string().min(1),
    taskType: z.string().min(1),
    command: z.string().min(1).optional(),
    message: z.string().min(1),
    truncated: z.boolean().optional(),
  })
  .strict();

export const taskOutputToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("task_output"),
    retrievalStatus: z.enum(["success", "not_ready", "timeout"]),
    taskStatus: z.string().min(1).max(TASK_OUTPUT_DISPLAY_MAX_STATUS_CHARS).optional(),
    output: z.string().min(1).max(TASK_OUTPUT_DISPLAY_MAX_OUTPUT_CHARS).optional(),
    truncated: z.literal(true).optional(),
  })
  .strict();

export const respondToCoordinatorToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("respond_to_coordinator"),
    status: z.enum(["success", "failed"]),
  })
  .strict();

export const cuaToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("cua"),
    schemaVersion: z.literal(1),
    toolName: z.string().min(1),
    status: z.enum(["success", "failed"]),
// The old v1 history record once carried ToolCallRow.input repeatedly; it will only continue to be accepted for playback compatibility, and the new producer will no longer write it.
    input: z.string().optional(),
    structuredContent: z.string().optional(),
    text: z.string().optional(),
    errorCode: z.string().optional(),
    suggestedAction: z.string().optional(),
    targetApp: cuaTargetAppDisplaySchema.optional(),
    permissionStatus: cuaRequestAccessStatusDisplaySchema.optional(),
    media: z
      .array(
        z
          .object({
            mimeType: z.string().min(1),
// 256 KiB Maximum base64 length of the original image after encoding; total budget enforced by the projector.
            data: z.string().min(1).max(349_528).optional(),
            artifactUri: z.string().min(1).optional(),
          })
          .strict(),
      )
      .max(4)
      .optional(),
    truncated: z.boolean().optional(),
  })
  .strict();

/**
* The target application identity (Computer Use) of node_repl cell. `appKey` is the form of producer:
 * `darwin:<bundleId>` / `windows-aumid:<aumid>` / `windows-exe:<path>` / `linux-exe:<path>`；
* The UI derives `ApplicationIconLocator` according to the prefix and then passes it to the platform service for parsing. The protocol does not carry icon bytes.
 */
export const nodeReplCuaAppDisplaySchema = z
  .object({
    appKey: z.string().trim().min(1).max(2_048),
    displayName: z.string().trim().min(1).max(512).optional(),
  })
  .strict();

export const nodeReplImageToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("node_repl_images"),
// images optional instead of min(1): CUA's pure action cells (click, input) do not take screenshots, but still project the app identity.
// kind name remains as node_repl_images - changing the name will cause the entire persisted row to be stripped in strict union.
    images: z
      .array(
        z
          .object({
            base64: z
              .string()
              .min(1)
              .max(200 * 1024),
            mimeType: z.string().regex(/^image\/[a-z0-9.+-]+$/iu),
          })
          .strict(),
      )
      .min(1)
      .max(2)
      .optional(),
    app: nodeReplCuaAppDisplaySchema.optional(),
    truncated: z.boolean().optional(),
    source: z.literal("browser_turn_end").optional(),
  })
  .strict();

export const mcpToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("mcp_tool"),
    serverName: z.string().min(1).max(MCP_TOOL_DISPLAY_MAX_NAME_CHARS),
    toolName: z.string().min(1).max(MCP_TOOL_DISPLAY_MAX_NAME_CHARS),
    description: z.string().min(1).max(MCP_TOOL_DISPLAY_MAX_DESCRIPTION_CHARS).optional(),
    /**
* The structured identification issued by the official Server MCP when it determines that this call is unavailable (limit exhausted/no Coding Plan).
* Only appears when tool result is isError and the MCP is an official source, and the UI prompts above the input box accordingly.
* Same origin as code: OFFICIAL_MCP_TOOL_ERROR_CODES of `@zcode/shared`.
     */
    unavailable: z
      .object({ code: z.enum(OFFICIAL_MCP_TOOL_ERROR_CODES) })
      .strict()
      .optional(),
  })
  .strict();

/**
* ⚠ This field collection is **frozen**. The extra key on the existing kind is not "one less field read by the old client", but the entire display
* Failure to verify: two real strict parsing points (create-workflow renderer safeParse of packages/ui,
* With legacy v3 (kind lookup table), the entire tool result will be discarded. Gate-specific facts will be entered through the **tool**
* channel (that side has no schema for all versions), the saved source of the reusable workflow does this.
 */
export const createWorkflowToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("create_workflow"),
    ok: z.boolean(),
    errorCount: z.number().int().nonnegative(),
    diagnostics: z
      .array(createWorkflowToolResultDisplayDiagnosticSchema)
      .max(CREATE_WORKFLOW_DISPLAY_MAX_DIAGNOSTICS),
// The tool output boundary has been limited in length (see the schema of create-workflow.ts), and display directly reuses the same contract.
    causalityGraph: CreateWorkflowCausalityGraphSchema.optional(),
    truncated: z.boolean().optional(),
  })
  .strict();

// contracts use zod v3, App uses v4; maintain the same strict contract as shared/bash-output-display.ts.
const bashOutputDisplaySchema = z
  .object({
    kind: z.literal("bash_output"),
    output: z.string().max(150_000),
    truncated: z.boolean(),
    outputPath: z.string().min(1).max(32_768).optional(),
  })
  .strict();

export const toolResultDisplayPayloadSchema = z.discriminatedUnion("kind", [
  bashOutputDisplaySchema,
  fileDiffToolResultDisplayPayloadSchema,
  localAgentMessageToolResultDisplayPayloadSchema,
  taskStopToolResultDisplayPayloadSchema,
  taskOutputToolResultDisplayPayloadSchema,
  respondToCoordinatorToolResultDisplayPayloadSchema,
  cuaToolResultDisplayPayloadSchema,
  nodeReplImageToolResultDisplayPayloadSchema,
  mcpToolResultDisplayPayloadSchema,
  createWorkflowToolResultDisplayPayloadSchema,
  getWorkflowRunToolResultDisplayPayloadSchema,
  listWorkflowRunsToolResultDisplayPayloadSchema,
  evalWorkflowSnippetToolResultDisplayPayloadSchema,
  savedWorkflowListToolResultDisplayPayloadSchema,
  listModelsToolResultDisplayPayloadSchema,
  resumeWorkflowRunToolResultDisplayPayloadSchema,
]);

export type FileDiffToolResultDisplayPayload = z.infer<
  typeof fileDiffToolResultDisplayPayloadSchema
>;
export type LocalAgentMessageToolResultDisplayPayload = z.infer<
  typeof localAgentMessageToolResultDisplayPayloadSchema
>;
export type TaskStopToolResultDisplayPayload = z.infer<
  typeof taskStopToolResultDisplayPayloadSchema
>;
export type TaskOutputToolResultDisplayPayload = z.infer<
  typeof taskOutputToolResultDisplayPayloadSchema
>;
export type RespondToCoordinatorToolResultDisplayPayload = z.infer<
  typeof respondToCoordinatorToolResultDisplayPayloadSchema
>;
export type CuaToolResultDisplayPayload = z.infer<typeof cuaToolResultDisplayPayloadSchema>;
export type ApplicationIconLocator = z.infer<typeof applicationIconLocatorSchema>;
export type CuaTargetAppDisplay = z.infer<typeof cuaTargetAppDisplaySchema>;
export type CuaRequestAccessStatusDisplay = z.infer<typeof cuaRequestAccessStatusDisplaySchema>;
export type NodeReplImageToolResultDisplayPayload = z.infer<
  typeof nodeReplImageToolResultDisplayPayloadSchema
>;
export type McpToolResultDisplayPayload = z.infer<typeof mcpToolResultDisplayPayloadSchema>;
export type CreateWorkflowToolResultDisplayPayload = z.infer<
  typeof createWorkflowToolResultDisplayPayloadSchema
>;
export type NodeReplCuaAppDisplay = z.infer<typeof nodeReplCuaAppDisplaySchema>;

export type ToolResultDisplayPayload = z.infer<typeof toolResultDisplayPayloadSchema>;

export const toolResultSerializationMetadataSchema = z
  .object({
    truncated: z.boolean(),
    originalBytes: z.number().int().nonnegative(),
    returnedBytes: z.number().int().nonnegative(),
    budgetStrategy: z.enum(["inline", "truncate", "artifact"]),
    artifactPath: z.string().min(1).optional(),
  })
  .strict();

export type ToolResultSerializationMetadata = z.infer<typeof toolResultSerializationMetadataSchema>;

export const completedToolPartMetadataSchema = z
  .object({
    schemaVersion: z.literal(COMPLETED_TOOL_PART_METADATA_SCHEMA_VERSION),
    display: toolResultDisplayPayloadSchema.optional(),
    serialization: toolResultSerializationMetadataSchema.optional(),
  })
  .passthrough();

export type CompletedToolPartMetadata = z.infer<typeof completedToolPartMetadataSchema>;

export function parseCompletedToolPartMetadata(
  input: unknown,
): CompletedToolPartMetadata | undefined {
  const scrubbed =
    isPlainRecord(input) && "display" in input
      ? { ...input, display: scrubPersistedDisplay(input.display) }
      : input;
  const result = completedToolPartMetadataSchema.safeParse(scrubbed);
  return result.success ? result.data : undefined;
}

export function parseToolResultDisplayPayload(
  input: unknown,
): ToolResultDisplayPayload | undefined {
  const result = toolResultDisplayPayloadSchema.safeParse(scrubPersistedDisplay(input));
  return result.success ? result.data : undefined;
}

/**
* Unified cleaning before parsing.
 *
* display is **inventory**: it is written into the `metadata.display` of the tool part, and must be read again every time thereafter.
* It is strictly parsed in one pass, and the rendering side also has a strict mirror schema for each frame. So the stripping must happen on the CLI side,
* Before parsing - the above two entrances are the only stuck points, v4 cold start hydration and session-transcript playback both pass through them.
 *
* Each stripper only recognizes its own kind and only changes the keys present. Other kinds will be parsed as they are.
 */
function scrubPersistedDisplay(display: unknown): unknown {
  return stripProviderStopFromGetWorkflowRunError(stripWithdrawnRefinedNames(display));
}

/**
* Strip `providerStop` on persistent get_workflow_run card.
 *
* In the early construction side, `providerStop`, which only belonged to the model channel, was written into the display, while the rendering side
* The mirror schema always only recognizes `{code, message}`, and those frames are always rejected. The structural side has been changed to only take
* code/message, but the historical persistence part will be re-read every cold start and can only be stripped here.
 *
* Only payloads whose kind is get_workflow_run, error is a normal object, and `providerStop` is present; without
* The error of the key and the missing error are returned as they are without adding an undefined out of thin air.
 */
function stripProviderStopFromGetWorkflowRunError(display: unknown): unknown {
  if (!isPlainRecord(display) || display.kind !== "get_workflow_run") return display;
  const error = display.error;
  if (!isPlainRecord(error) || !("providerStop" in error)) return display;
  const { providerStop: _dropped, ...rest } = error;
  return { ...display, error: rest };
}

/**
* Strip withdrawn model refinement fields.
 *
* In the previously persisted create_workflow display, lanes and stages may have `refinedName`,
* step may take `refinedLabel`. These three fields have been removed from the schema, and both entries are `.strict()`
* Analysis: Without stripping it off first, the entire display of the old session (along with the image) will be rejected here, not just a name.
 *
* Only create_workflow with the payload of causalityGraph is recognized; other kinds are parsed as they are. Only change those who are present
* key, do not add an undefined out of thin air to absent `phases` and the like.
 */
function stripWithdrawnRefinedNames(display: unknown): unknown {
  if (!isPlainRecord(display) || display.kind !== "create_workflow") return display;
  const graph = display.causalityGraph;
  if (!isPlainRecord(graph)) return display;
  return {
    ...display,
    causalityGraph: {
      ...graph,
      ...("lanes" in graph ? { lanes: withoutKey(graph.lanes, "refinedName") } : {}),
      ...("steps" in graph ? { steps: withoutKey(graph.steps, "refinedLabel") } : {}),
      ...("phases" in graph ? { phases: withoutKey(graph.phases, "refinedName") } : {}),
    },
  };
}

function withoutKey(items: unknown, key: string): unknown {
  if (!Array.isArray(items)) return items;
  return items.map((item) => {
    if (!isPlainRecord(item) || !(key in item)) return item;
    const { [key]: _dropped, ...rest } = item;
    return rest;
  });
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
