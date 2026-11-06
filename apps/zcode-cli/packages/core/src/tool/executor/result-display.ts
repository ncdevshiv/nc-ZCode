import {
  RESPOND_TO_COORDINATOR_TOOL_NAME,
  RespondToCoordinatorOutputSchema,
  MCP_TOOL_DISPLAY_MAX_DESCRIPTION_CHARS,
  MCP_TOOL_DISPLAY_MAX_NAME_CHARS,
  CUA_TARGET_APP_DISPLAY_META_KEY,
  cuaTargetAppDisplaySchema,
  nodeReplCuaAppDisplaySchema,
  ZCODE_MCP_NODE_REPL_CUA_APP_META_KEY,
  SEND_MESSAGE_TOOL_NAME,
  SendMessageOutputSchema,
  TASK_OUTPUT_DISPLAY_MAX_OUTPUT_CHARS,
  TASK_OUTPUT_DISPLAY_MAX_STATUS_CHARS,
  TASK_OUTPUT_TOOL_NAME,
  TaskOutputResultSchema,
  TASK_STOP_TOOL_NAME,
  TaskStopOutputSchema,
  type DiffHunk,
  type NodeReplCuaAppDisplay,
  type ToolResultDisplayPayload,
} from "@zcode/contracts";
import { createBashResultDisplay } from "./bash-result-display.js";
import { countPatchLines } from "../diff.js";
import { boundDisplayText } from "./display-text.js";
import { createCreateWorkflowDisplay } from "./create-workflow-display.js";
import { createWorkflowObservationDisplay } from "./workflow-observation-display.js";

// After splitting into create-workflow-display.ts, keep the existing export interface (handlers/create-workflow.ts is still imported from here).
export { createCreateWorkflowDisplay } from "./create-workflow-display.js";
import { isRecord } from "./utils.js";
import { parseOfficialMcpToolError, type OfficialMcpToolErrorCode } from "@zcode/shared";
import {
  CUA_REQUEST_ACCESS_STATUS_META_KEY,
  cuaRequestAccessStatusSchema,
} from "@zcode/zcode-cua/request-access-contract";

const MAX_DISPLAY_DIFF_HUNKS = 8;
const MAX_DISPLAY_DIFF_LINES = 160;
const MAX_SEND_MESSAGE_DISPLAY_FIELD_BYTES = 4 * 1024;
const MAX_TASK_STOP_DISPLAY_FIELD_BYTES = 16 * 1024;
export const MAX_NODE_REPL_DISPLAY_IMAGE_BASE64_BYTES = 200 * 1024;
const MAX_NODE_REPL_DISPLAY_IMAGES = 2;

export function createMcpToolDisplay(
  metadata:
    | {
        serverName: string;
        toolName: string;
        description?: string;
        official?: boolean;
      }
    | undefined,
  output?: unknown,
): ToolResultDisplayPayload | undefined {
  if (!metadata) return undefined;
  const serverName = boundMcpDisplayText(metadata.serverName, MCP_TOOL_DISPLAY_MAX_NAME_CHARS);
  const toolName = boundMcpDisplayText(metadata.toolName, MCP_TOOL_DISPLAY_MAX_NAME_CHARS);
  if (!serverName || !toolName) return undefined;
  const description = metadata.description
    ? boundMcpDisplayText(metadata.description, MCP_TOOL_DISPLAY_MAX_DESCRIPTION_CHARS)
    : undefined;
  const unavailable = metadata.official ? readOfficialMcpUnavailable(output) : undefined;
  return {
    kind: "mcp_tool",
    serverName,
    toolName,
    ...(description ? { description } : {}),
    ...(unavailable ? { unavailable } : {}),
  };
}

/**
* Official Server MCP renders structured identifiers into tool error content when quota is exhausted/no Coding Plan
* JSON text (server-side `ToolError.Error()`). Only the identifier is read here and ordinary error text is not parsed.
 *
* Only reached if `metadata.official` is true, and this flag is only set for the **http** official MCP - that kind of
* Response from ZCode backend with verified origin. Stdio official MCP and third-party MCP will ignore the same payload:
* Their results are produced by the plug-in process itself, and a Coding Plan prompt can be forged to mislead users into purchasing.
 */
function readOfficialMcpUnavailable(
  output: unknown,
): { code: OfficialMcpToolErrorCode } | undefined {
  if (!isRecord(output) || output.isError !== true || !Array.isArray(output.content)) {
    return undefined;
  }
  for (const block of output.content) {
    if (!isRecord(block) || block.type !== "text" || typeof block.text !== "string") continue;
    const parsed = parseOfficialMcpToolError(block.text);
    if (parsed) return { code: parsed.code };
  }
  return undefined;
}

export function createToolResultDisplay(
  toolName: string,
  output: unknown,
  options?: {
    officialCua?: boolean;
    mcp?: {
      serverName: string;
      toolName: string;
      description?: string;
      official?: boolean;
    };
  },
): ToolResultDisplayPayload | undefined {
  if (toolName === "Bash") return createBashResultDisplay(output);

  const cuaToolName = readCuaToolName(toolName);
  if (cuaToolName) {
    return createCuaToolResultDisplay(cuaToolName, output, options?.officialCua === true);
  }

  const nodeReplDisplay = createNodeReplDisplay(toolName, output);
  if (nodeReplDisplay) return nodeReplDisplay;

  const createWorkflow = createCreateWorkflowDisplay(toolName, output);
  if (createWorkflow) return createWorkflow;

  const workflowObservation = createWorkflowObservationDisplay(toolName, output);
  if (workflowObservation) return workflowObservation;

  if (options?.mcp) {
// Result-level structure: The unavailable flag of the official MCP can only be read from this result, so the output is passed in together.
    return createMcpToolDisplay(options.mcp, output);
  }

  if (toolName === SEND_MESSAGE_TOOL_NAME) {
    const parsed = SendMessageOutputSchema.safeParse(output);
    if (!parsed.success) return undefined;
// The display does not go through the tool result budget, and must be limited separately before entering real-time events and persistence metadata.
    const error =
      parsed.data.error === undefined
        ? undefined
        : boundDisplayText(parsed.data.error, MAX_SEND_MESSAGE_DISPLAY_FIELD_BYTES).value;
    const message =
      parsed.data.message === undefined
        ? undefined
        : boundDisplayText(parsed.data.message, MAX_SEND_MESSAGE_DISPLAY_FIELD_BYTES).value;
    return {
      kind: "local_agent_message",
      status: parsed.data.status,
      ...(error !== undefined ? { error } : {}),
      ...(message !== undefined ? { message } : {}),
    };
  }

  if (toolName === TASK_STOP_TOOL_NAME) {
    const parsed = TaskStopOutputSchema.safeParse(output);
    if (!parsed.success) return undefined;
    const command =
      parsed.data.command === undefined
        ? undefined
        : boundDisplayText(parsed.data.command, MAX_TASK_STOP_DISPLAY_FIELD_BYTES);
    const message = boundDisplayText(
      compactTaskStopDisplayMessage(parsed.data),
      MAX_TASK_STOP_DISPLAY_FIELD_BYTES,
    );
    const truncated = command?.truncated === true || message.truncated;
    return {
      kind: "task_stop",
      taskId: parsed.data.task_id,
      taskType: parsed.data.task_type,
      ...(command !== undefined ? { command: command.value } : {}),
      message: message.value,
      ...(truncated ? { truncated: true } : {}),
    };
  }

  if (toolName === TASK_OUTPUT_TOOL_NAME) {
    const parsed = TaskOutputResultSchema.safeParse(output);
    if (!parsed.success) return undefined;
    const taskStatus = parsed.data.task?.status
      .trim()
      .slice(0, TASK_OUTPUT_DISPLAY_MAX_STATUS_CHARS);
    const fullOutput = parsed.data.task?.output.trimEnd();
    const hasOutput = fullOutput !== undefined && fullOutput.trim().length > 0;
    const truncated = hasOutput && fullOutput.length > TASK_OUTPUT_DISPLAY_MAX_OUTPUT_CHARS;

// UI display is a bounded projection independent of provider content; it is forbidden to put the complete TaskOutput XML
// Or the result object is stuffed with real-time events and persistent metadata.
    return {
      kind: "task_output",
      retrievalStatus: parsed.data.retrieval_status,
      ...(taskStatus ? { taskStatus } : {}),
      ...(hasOutput ? { output: fullOutput.slice(0, TASK_OUTPUT_DISPLAY_MAX_OUTPUT_CHARS) } : {}),
      ...(truncated ? { truncated: true } : {}),
    };
  }

  if (toolName === RESPOND_TO_COORDINATOR_TOOL_NAME) {
    const parsed = RespondToCoordinatorOutputSchema.safeParse(output);
    if (!parsed.success) return undefined;
    return {
      kind: "respond_to_coordinator",
      status: parsed.data.status,
    };
  }

  if (!isRecord(output)) return undefined;
  const filePath = output.filePath;
  const structuredPatch = output.structuredPatch;
  if (typeof filePath !== "string" || !Array.isArray(structuredPatch)) {
    return undefined;
  }

  const hunks = structuredPatch.filter(isDiffHunk);
  if (hunks.length === 0) return undefined;

  const { additions, deletions } = countPatchLines(hunks);
  const { patch: boundedPatch, truncated } = boundDiffHunks(hunks);
  return {
    kind: "file_diff",
    filePath,
    additions,
    deletions,
    structuredPatch: boundedPatch,
    truncated,
  };
}

function boundMcpDisplayText(value: string, maxChars: number): string | undefined {
  const trimmed = value.trim();
  if (!trimmed) return undefined;
  const bounded = trimmed.slice(0, maxChars);
// MCP discovery is external input, direct slice may be in UTF-16 surrogate pair
// Intermediate truncation, resulting in a string that cannot be stable across events, persistence and replayable snapshots.
// If the boundary falls after the high surrogate, discard this half character to ensure that display can always be safely serialized.
  const lastCodeUnit = bounded.charCodeAt(bounded.length - 1);
  return lastCodeUnit >= 0xd800 && lastCodeUnit <= 0xdbff ? bounded.slice(0, -1) : bounded;
}

const MAX_CUA_DISPLAY_FIELD_BYTES = 32 * 1024;
const MAX_CUA_INLINE_MEDIA_BYTES = 256 * 1024;
const MAX_CUA_INLINE_MEDIA_TOTAL_BYTES = 512 * 1024;

function readCuaToolName(toolName: string): string | undefined {
// Compatible with two CUA tool naming: direct `mcp__computer_use__<action>` and plugin
// Namespace form `mcp__plugin_zcode-cua_computer-use__<action>`.
// After normalization (lowercase + `-`→`_`): the name contains `computer_use`, and action is the substring after the last `__`.
  const normalized = toolName.trim().toLowerCase().replaceAll("-", "_");
  if (!normalized.includes("computer_use")) return undefined;
  const lastSep = normalized.lastIndexOf("__");
  if (lastSep === -1) return undefined;
  const action = normalized.slice(lastSep + "__".length);
  return action.length > 0 ? action : undefined;
}

function createCuaToolResultDisplay(
  toolName: string,
  output: unknown,
  officialCua: boolean,
): ToolResultDisplayPayload {
  const result = isRecord(output) ? output : {};
  const content = Array.isArray(result.content) ? result.content : [];
  const text = content
    .filter(isRecord)
    .filter((item) => item.type === "text" && typeof item.text === "string")
    .map((item) => item.text as string)
    .join("\n");
  const structuredContent = result.structuredContent;
  const structuredRecord = isRecord(structuredContent) ? structuredContent : undefined;
  const errorRecord = isRecord(structuredRecord?.error) ? structuredRecord.error : undefined;
  const structuredJson =
    structuredContent === undefined
      ? undefined
      : boundDisplayText(safeJson(structuredContent), MAX_CUA_DISPLAY_FIELD_BYTES);
  const boundedText = text ? boundDisplayText(text, MAX_CUA_DISPLAY_FIELD_BYTES) : undefined;
// The artifact URI is only readable locally by the Agent. Direct projection will cause the multi-end UI to receive media that cannot be rendered.
// Before the controlled read API is established, display only carries inline images that can be directly rendered.
  const media: Array<{ mimeType: string; data: string }> = [];
  let inlineMediaBytes = 0;
  let mediaTruncated = false;
  for (const item of content) {
    if (!isRecord(item)) continue;
    let projectedMedia: { mimeType: string; data: string } | undefined;
    let decodedBytes = 0;
    if (
      item.type === "image" &&
      typeof item.mimeType === "string" &&
      typeof item.data === "string"
    ) {
      decodedBytes = Buffer.byteLength(item.data, "base64");
      projectedMedia = { mimeType: item.mimeType, data: item.data };
    }
    if (!projectedMedia) continue;
// The media quota only restricts real media, and the preceding text block cannot occupy the screenshot position.
    if (media.length >= 4) {
      mediaTruncated = true;
      break;
    }
    if (
      decodedBytes > MAX_CUA_INLINE_MEDIA_BYTES ||
      inlineMediaBytes + decodedBytes > MAX_CUA_INLINE_MEDIA_TOTAL_BYTES
    ) {
      mediaTruncated = true;
      continue;
    }
    inlineMediaBytes += decodedBytes;
    media.push(projectedMedia);
  }
  const truncated =
    mediaTruncated || structuredJson?.truncated === true || boundedText?.truncated === true;
  const meta = isRecord(result._meta) ? result._meta : undefined;
  const targetApp = officialCua
    ? cuaTargetAppDisplaySchema.safeParse(meta?.[CUA_TARGET_APP_DISPLAY_META_KEY])
    : undefined;
  const permissionStatus =
    officialCua && toolName === "request_access"
      ? cuaRequestAccessStatusSchema.safeParse(meta?.[CUA_REQUEST_ACCESS_STATUS_META_KEY])
      : undefined;

// MCP modelContent will flatten structuredContent into text; generate independent, finite-length before flattening
// display allows real-time events and historical sessions to stably distinguish CUA errors and structured results.
  return {
    kind: "cua",
    schemaVersion: 1,
    toolName,
    status: result.isError === true ? "failed" : "success",
    ...(structuredJson ? { structuredContent: structuredJson.value } : {}),
    ...(boundedText ? { text: boundedText.value } : {}),
    ...(typeof errorRecord?.code === "string" ? { errorCode: errorRecord.code } : {}),
    ...(typeof errorRecord?.suggested_action === "string"
      ? { suggestedAction: errorRecord.suggested_action }
      : {}),
    ...(targetApp?.success ? { targetApp: targetApp.data } : {}),
    ...(permissionStatus?.success ? { permissionStatus: permissionStatus.data } : {}),
    ...(media.length > 0 ? { media } : {}),
    ...(truncated ? { truncated: true } : {}),
  };
}

function safeJson(value: unknown): string {
  try {
    return JSON.stringify(value ?? null);
  } catch {
    return "null";
  }
}

/**
* Read the CUA target application identity written by the host.
 *
* Only recognize `zcode/nodeReplCuaApp`: producer's own `zcode.cua/app-associations-v1` may also appear in
* in `_meta`, but that key is writable by the model `nodeRepl.setResponseMeta` /
* `nodeRepl.emitStructuredResult` can also be reached, but the host has deleted it in toMcpRunResult. Not done here
* The second thorough analysis avoids bringing sources that have been judged untrustworthy back to the display.
 */
function readNodeReplCuaApp(output: Record<string, unknown>): NodeReplCuaAppDisplay | undefined {
  const meta = isRecord(output._meta) ? output._meta : undefined;
  const parsed = nodeReplCuaAppDisplaySchema.safeParse(
    meta?.[ZCODE_MCP_NODE_REPL_CUA_APP_META_KEY],
  );
  return parsed.success ? parsed.data : undefined;
}

function createNodeReplDisplay(
  toolName: string,
  output: unknown,
): ToolResultDisplayPayload | undefined {
  if (toolName !== "js" && toolName !== "mcp__node_repl__js") return undefined;
  if (!isRecord(output)) return undefined;

  const candidates = [
    ...(Array.isArray(output.images) ? output.images : []),
    ...(Array.isArray(output.content) ? output.content : []),
  ];
  const images: Array<{ base64: string; mimeType: string }> = [];
  let truncated = false;

  for (const candidate of candidates) {
    if (!isRecord(candidate)) continue;
    const mimeType = candidate.mimeType;
    const encoded = candidate.base64 ?? candidate.data;
    if (
      typeof mimeType !== "string" ||
      !/^image\/[a-z0-9.+-]+$/iu.test(mimeType) ||
      typeof encoded !== "string"
    ) {
      continue;
    }
    const base64 = encoded.startsWith("data:")
      ? encoded.slice(Math.max(0, encoded.indexOf(",") + 1))
      : encoded;
    if (
      base64.length === 0 ||
      Buffer.byteLength(base64, "utf8") > MAX_NODE_REPL_DISPLAY_IMAGE_BASE64_BYTES
    ) {
      truncated = true;
      continue;
    }
    if (images.length >= MAX_NODE_REPL_DISPLAY_IMAGES) {
      truncated = true;
      continue;
    }
    images.push({ base64, mimeType });
  }

// Pure action cells (click, input) do not take screenshots, but the App identity must still be projected to the leading icon of the tool card;
// Therefore, "having a picture" can no longer be used as the only condition for producing display.
  const app = readNodeReplCuaApp(output);
  if (images.length === 0 && !app) return undefined;
  return {
    kind: "node_repl_images",
    ...(images.length > 0 ? { images } : {}),
    ...(app ? { app } : {}),
    ...(truncated ? { truncated: true } : {}),
  };
}

function compactTaskStopDisplayMessage(output: {
  command?: string;
  message: string;
  task_id: string;
}): string {
  if (output.command === undefined) {
    return output.message;
  }

// TaskStop's standard success copy will spell command into brackets; display already exists
//Independent command field, the result line only retains the stop conclusion.
  const standardMessage = `Successfully stopped task: ${output.task_id} (${output.command})`;
  return output.message === standardMessage
    ? `Successfully stopped task: ${output.task_id}`
    : output.message;
}

function isDiffHunk(value: unknown): value is DiffHunk {
  if (!isRecord(value)) return false;
  return (
    typeof value.oldStart === "number" &&
    typeof value.oldLines === "number" &&
    typeof value.newStart === "number" &&
    typeof value.newLines === "number" &&
    Array.isArray(value.lines) &&
    value.lines.every((line) => typeof line === "string")
  );
}

function boundDiffHunks(hunks: DiffHunk[]): { patch: DiffHunk[]; truncated: boolean } {
  const bounded: DiffHunk[] = [];
  let remainingLines = MAX_DISPLAY_DIFF_LINES;
  let truncated = hunks.length > MAX_DISPLAY_DIFF_HUNKS;

  for (const hunk of hunks.slice(0, MAX_DISPLAY_DIFF_HUNKS)) {
    if (remainingLines <= 0) {
      truncated = true;
      break;
    }

    const lines = hunk.lines.slice(0, remainingLines);
    bounded.push({ ...hunk, lines });
    remainingLines -= lines.length;
    if (lines.length < hunk.lines.length) {
      truncated = true;
      break;
    }
  }

  return { patch: bounded, truncated };
}
