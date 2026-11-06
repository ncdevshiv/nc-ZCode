/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
// ============================================================
// Read Tool Handler
// ============================================================

import { basename, dirname, extname } from "node:path";
import type {
  ReadFileStateEntry,
  ReadFileStateMap,
  ToolExecutionContext,
  ToolHandler,
  ToolEntry,
  ToolInputValidationResult,
} from "../types.js";
import {
  CoreErrorType,
  READ_DEFAULT_MAX_LINES,
  READ_MAX_FILE_SIZE_BYTES,
  ReadInputJsonSchema,
  ReadInputSchema,
  ReadOutputJsonSchema,
  ReadOutputSchema,
  createCoreError,
  getReadPdfPagesValidationFailure,
  isFileSystemPortError,
  type ReadInput,
  type ReadImageOutput,
  type ReadVideoOutput,
  type ModelMessageContent,
  type ReadOutput,
  type ReadTextOutput,
  type FileSystemStatResult,
  type TraceContext,
} from "@zcode/contracts";
import { resolveWorkspacePath } from "../path-policy.js";
import { createReadFileStateKey, normalizeReadFileStateMtimeMs } from "../read-file-state.js";
import { createReadFileStateMetadata } from "../read-file-state-metadata.js";
import { inferImageMimeFromPath, readImageFile } from "./read-image.js";
import { inferVideoMimeFromPath } from "../../runtime/helpers/attachment-video.js";
import { readVideoFile } from "./read-video.js";
import { formatReadTextOutput, readTextFileForModel } from "./read-text.js";
import {
  formatReadPdfOutput,
  formatReadPdfPagesOutput,
  isPdfPath,
  READ_PDF_TOOL_TIMEOUT_MS,
  readPdfFile,
  resolveReadInputSchema,
  resolveReadProviderDescription,
  resolveReadTimeoutBudgetMs,
  supportsPdfForExecution,
} from "./read-pdf.js";

export { addReadLineNumbers } from "./read-text.js";

const FILE_UNCHANGED_STUB =
  "Wasted call — file unchanged since your last Read. Refer to that earlier tool_result instead.";
const READ_PROVIDER_DESCRIPTION = [
  "Reads a file from the local filesystem.",
  "",
  "- `file_path` must be an absolute path.",
  `- Reads up to ${READ_DEFAULT_MAX_LINES} lines by default.`,
  "- You can optionally specify a line offset and limit (especially handy for long files), but it's recommended to read the whole file by not providing these parameters",
  "- Results are returned using cat -n format, with line numbers starting at 1",
  "- Reads images (PNG, JPG, …) and presents them visually.",
  "- Reads videos (MP4, MOV, WEBM, …) and presents them as video input (subject to ZCode's video input limit).",
  "- Reading a directory, a missing file, or an empty file returns an error or system reminder rather than content.",
  "- Do NOT re-read a file you just edited to verify — Edit/Write would have errored if the change failed, and the harness tracks file state for you.",
].join("\n");

const fallbackReadFileStates = new WeakMap<ToolExecutionContext, ReadFileStateMap>();

function formatReadModelContent(output: unknown): ModelMessageContent {
  const parsed = ReadOutputSchema.safeParse(output);
  if (!parsed.success) {
    return stringifyReadOutputFallback(output);
  }

  return formatReadOutput(parsed.data);
}

function formatReadOutput(output: ReadOutput): ModelMessageContent {
  switch (output.type) {
    case "text":
      return formatReadTextOutput(output);
    case "file_unchanged":
      return FILE_UNCHANGED_STUB;
    case "image":
      return formatReadImageOutput(output);
    case "video":
      return formatReadVideoOutput(output);
    case "pdf":
      return formatReadPdfOutput(output);
    case "parts":
      return formatReadPdfPagesOutput(output);
    case "notebook":
      return stringifyReadOutputFallback(output);
  }
}

function formatReadImageOutput(output: ReadImageOutput): ModelMessageContent {
  const imageBlock = {
    type: "image" as const,
    mediaType: output.mimeType,
    dataUrl: `data:${output.mimeType};base64,${output.base64}`,
    source: {
      id: "read-image",
      kind: "inline" as const,
      mimeType: output.mimeType,
      placeholder: "Read image",
      sizeBytes: output.originalSize,
    },
  };
// Spelling the size prompt into the tool result will cause the provider-visible content to change depending on whether it is zoomed or not;
// The image result only retains the media block, and dimensions remain in the structured output for UI and debugging use.
  return [imageBlock];
}

// Isomorphic to the picture: tool result only retains the media block; the OpenAI system provider consists of
// tool-result-media-projection is split into post-user part (AI SDK tool result has no video part variant).
function formatReadVideoOutput(output: ReadVideoOutput): ModelMessageContent {
  const videoBlock = {
    type: "video" as const,
    mediaType: output.mimeType,
    dataUrl: `data:${output.mimeType};base64,${output.base64}`,
    source: {
      id: "read-video",
      kind: "inline" as const,
      mimeType: output.mimeType,
      placeholder: "Read video",
      sizeBytes: output.originalSize,
    },
  };
  return [videoBlock];
}

function stringifyReadOutputFallback(output: unknown): string {
  if (typeof output === "string") return output;
  return JSON.stringify(output) ?? "";
}

const readHandler: ToolHandler = async (input, context) => {
  const { file_path, offset, limit, pages } = parseReadInput(input);
  const fileSystemPort = context.fileSystemPort;

  if (!fileSystemPort) {
    throw createCoreError(
      CoreErrorType.ConfigurationError,
      "FileSystemPort is not configured for Read tool",
      {
        context: {
          toolCallId: context.toolCallId,
          toolName: "Read",
        },
        recoverable: false,
      },
    );
  }

  const filePath = resolveWorkspacePath({
    inputPath: file_path,
    operation: "read",
    workingDirectory: context.workingDirectory,
    workspaceRoot: context.workspaceRoot,
  });

  try {
    const imageMime = inferImageMimeFromPath(filePath);
    if (imageMime) {
      return await readImageFile(filePath, imageMime, context);
    }

    const videoMime = inferVideoMimeFromPath(filePath);
    if (videoMime) {
      return await readVideoFile(filePath, videoMime, context);
    }

    if (isPdfPath(filePath) && supportsPdfForExecution(context)) {
      return await readPdfFile({ filePath, pages }, context);
    }

    const trace = createReadTrace(context);
    const stat = await fileSystemPort.stat(
      { path: filePath, trace },
      { signal: context.abortSignal },
    );
    const readFileState = getReadFileState(context);
    const cacheOffset = normalizeCacheOffset(offset);
    const cacheKey = createReadFileStateKey(filePath, cacheOffset, limit);
    const cached = readFileState.get(cacheKey);
    if (cached && isCachedReadFresh(cached, stat)) {
      const output = { type: "file_unchanged", filePath } satisfies ReadOutput;
      recordReadFileStateMetadata(context, {
        output,
        readFileState,
        toolInput: input,
      });
      return output;
    }

    let rangeReadRevision: FileSystemStatResult["revision"] | undefined;
    const output = await readTextFileForModel({
      abortSignal: context.abortSignal,
      filePath,
      fileSystemPort,
      limit,
      onRead: (read) => {
        rangeReadRevision = read.revision;
      },
      offset,
      trace,
    });
    updateReadFileState(readFileState, cacheKey, {
      output,
      path: filePath,
      stat,
      rangeReadRevision,
      offset,
      limit,
    });
    recordReadFileStateMetadata(context, {
      output,
      readFileState,
      toolInput: input,
    });
    return output;
  } catch (error) {
    if (isFileSystemPortError(error) && error.code === "not_found") {
      const message = await createMissingReadFileMessage(filePath, context);
      throw createCoreError(CoreErrorType.ToolExecutionFailed, message, {
        cause: error,
        context: {
          code: "read_file_not_found",
          filePath,
        },
        recoverable: true,
      });
    }
    if (isFileSystemPortError(error) && error.code === "too_large") {
      throw createCoreError(CoreErrorType.ToolExecutionFailed, error.message, {
        cause: error,
        context: {
          code: "read_file_too_large",
          filePath,
          maxBytes: READ_MAX_FILE_SIZE_BYTES,
        },
        recoverable: true,
      });
    }
    throw error;
  }
};

function parseReadInput(input: unknown): ReadInput {
  const parsed = ReadInputSchema.safeParse(input);
  if (parsed.success) return parsed.data as ReadInput;

  const toolUseErrorMessage = getReadInputToolUseErrorMessage(parsed.error);
  if (!toolUseErrorMessage) {
    throw parsed.error;
  }

// Read input preflight failures should be entered into the provider with <tool_use_error> text;
// Exposing ZodError JSON directly will cause binary/device preflight to deviate from capture.
  throw createCoreError(
    CoreErrorType.ToolExecutionFailed,
    `<tool_use_error>${toolUseErrorMessage}</tool_use_error>`,
    {
      cause: parsed.error,
      context: {
        code: "read_input_preflight_failed",
      },
      recoverable: true,
    },
  );
}

function validateReadInput(input: unknown): ToolInputValidationResult {
  if (typeof input !== "object" || input === null || Array.isArray(input)) {
    return { result: true };
  }

  const candidate = input as { file_path?: unknown; pages?: unknown };
  if (typeof candidate.file_path !== "string" || typeof candidate.pages !== "string") {
    return { result: true };
  }

// When the semantic constraints of PDF pages only exist in the runtime schema, JSON Schema will accept any
// string, causing the error call to go through the Hook and permissions before throwing a bare ZodError in the handler.
  const failure = getReadPdfPagesValidationFailure(candidate.file_path, candidate.pages);
  return failure ? { result: false, ...failure } : { result: true };
}

function getReadInputToolUseErrorMessage(error: unknown): string | undefined {
  const issues = (error as { issues?: unknown }).issues;
  if (!Array.isArray(issues)) return undefined;

  for (const issue of issues) {
    if (!isReadInputToolUseIssue(issue)) continue;
    return issue.message;
  }
  return undefined;
}

function isReadInputToolUseIssue(issue: unknown): issue is { message: string } {
  if (!issue || typeof issue !== "object") return false;
  const record = issue as { code?: unknown; message?: unknown; path?: unknown };
  if (record.code !== "custom" || typeof record.message !== "string") return false;
  if (!Array.isArray(record.path)) return false;
  return record.path.length === 1 && record.path[0] === "file_path";
}

function createReadTrace(context: ToolExecutionContext): TraceContext {
  return {
    traceId: context.traceId,
    spanId: context.spanId,
    parentSpanId: context.parentSpanId,
    sessionId: context.sessionId,
    turnId: context.turnId,
  } as unknown as TraceContext;
}

function getReadFileState(context: ToolExecutionContext): ReadFileStateMap {
  if (context.readFileState) return context.readFileState;
  const existing = fallbackReadFileStates.get(context);
  if (existing) return existing;
  const state: ReadFileStateMap = new Map();
  fallbackReadFileStates.set(context, state);
  return state;
}

function normalizeCacheOffset(offset: number | undefined): number {
  return offset === undefined ? 1 : offset;
}

function isCachedReadFresh(entry: ReadFileStateEntry, stat: FileSystemStatResult): boolean {
  if (entry.isPartialView) return false;

  const mtimeMs = stat.revision?.mtimeMs ?? stat.mtimeMs;
  if (entry.mtimeMs !== undefined && mtimeMs !== undefined) {
// Keep the same strategy as the freshness check before writing, mtime only compares integer milliseconds.
    return (
      normalizeReadFileStateMtimeMs(entry.mtimeMs) === normalizeReadFileStateMtimeMs(mtimeMs) &&
      entry.sizeBytes === stat.sizeBytes
    );
  }

  const revisionId = stat.revision?.id;
  if (entry.revisionId && revisionId) return entry.revisionId === revisionId;

  return entry.sizeBytes !== undefined && entry.sizeBytes === stat.sizeBytes;
}

function updateReadFileState(
  state: ReadFileStateMap,
  key: string,
  input: {
    output: ReadTextOutput;
    path: string;
    stat: FileSystemStatResult;
    rangeReadRevision?: FileSystemStatResult["revision"];
    offset?: number;
    limit?: number;
  },
): void {
  const revision = input.stat.revision ?? input.rangeReadRevision;
  state.set(key, {
    path: input.path,
    content: input.output.content,
    offset: input.offset,
    limit: input.limit,
// offset/limit is range view, not equivalent to partial view.
// partial view only means that the content seen by the model has been truncated by the tool, and Write/Edit must reject this incomplete view.
    isPartialView: input.output.truncatedByTokenCap === true,
    readAt: new Date(),
    sourceTool: "Read",
    revisionId: revision?.id,
    mtimeMs: normalizeReadFileStateMtimeMs(revision?.mtimeMs ?? input.stat.mtimeMs),
    sizeBytes: input.stat.sizeBytes,
  });
}

function recordReadFileStateMetadata(
  context: ToolExecutionContext,
  input: {
    output: ReadOutput;
    readFileState: ReadFileStateMap;
    toolInput: unknown;
  },
): void {
  if (!context.recordReadFileStateMetadata) return;
  const metadata = createReadFileStateMetadata({
    completedAt: new Date(),
    output: input.output,
    readFileState: input.readFileState,
    toolInput: input.toolInput,
    toolName: "Read",
  });
  if (metadata) context.recordReadFileStateMetadata(metadata);
}

async function createMissingReadFileMessage(
  filePath: string,
  context: ToolExecutionContext,
): Promise<string> {
  const suggestion = await findSimilarFilename(filePath, context);
  return [
    `File does not exist. Note: your current working directory is ${context.workingDirectory}.`,
    suggestion ? ` Did you mean ${suggestion}?` : "",
  ].join("");
}

async function findSimilarFilename(
  filePath: string,
  context: ToolExecutionContext,
): Promise<string | undefined> {
  const fileSystemPort = context.fileSystemPort;
  if (!fileSystemPort) return undefined;

  try {
    const parent = dirname(filePath);
    const targetName = basename(filePath);
    const targetStem = basename(filePath, extname(filePath));
    const listed = await fileSystemPort.listDirectory(
      { path: parent, trace: createReadTrace(context) },
      { signal: context.abortSignal },
    );
    const entries = listed.entries
      .filter((entry) => entry.kind === "file" || entry.kind === "symlink")
      .map((entry) => entry.name)
      .filter((name) => name !== targetName)
      .sort();

    const sameStem = entries.find((name) => basename(name, extname(name)) === targetStem);
    if (sameStem) return sameStem;

    return entries.find((name) => levenshteinDistance(name, targetName) <= 3);
  } catch {
    return undefined;
  }
}

function levenshteinDistance(left: string, right: string): number {
  const previous = Array.from({ length: right.length + 1 }, (_, index) => index);
  const current = Array.from({ length: right.length + 1 }, () => 0);

  for (let leftIndex = 1; leftIndex <= left.length; leftIndex += 1) {
    current[0] = leftIndex;
    for (let rightIndex = 1; rightIndex <= right.length; rightIndex += 1) {
      const cost = left[leftIndex - 1] === right[rightIndex - 1] ? 0 : 1;
      current[rightIndex] = Math.min(
        current[rightIndex - 1]! + 1,
        previous[rightIndex]! + 1,
        previous[rightIndex - 1]! + cost,
      );
    }
    for (let index = 0; index < previous.length; index += 1) {
      previous[index] = current[index]!;
    }
  }

  return previous[right.length] ?? 0;
}

export const readToolEntry: ToolEntry = {
  capability:
    "Read text files and supported images from the file-system adapter without modifying files",
  metadata: {
    name: "Read",
    description: READ_PROVIDER_DESCRIPTION,
    readOnly: true,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: 30000,
    maxOutputBytes: READ_MAX_FILE_SIZE_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
    needsApproval: false,
  },
  handler: readHandler,
  validateInput: validateReadInput,
  resolveModelContract: (context) => ({
    description: resolveReadProviderDescription(READ_PROVIDER_DESCRIPTION, context),
    inputSchema: resolveReadInputSchema(context),
  }),
  resolveTimeoutBudgetMs: resolveReadTimeoutBudgetMs,
  formatModelContent: formatReadModelContent,
  inputSchema: ReadInputJsonSchema,
  outputSchema: ReadOutputJsonSchema,
  runtimeInputSchema: ReadInputSchema,
  runtimeOutputSchema: ReadOutputSchema,
  permission: {
    permission: "read",
    reason: "Read only inspects file content and has no external side effects",
    riskLevel: "low",
    sideEffectScope: "none",
    needsApproval: false,
    patternSources: ["path"],
    alwaysAllowPatternSources: ["path"],
    denyPriority: "beforeAsk",
  },
  resultBudget: {
    maxInlineBytes: READ_MAX_FILE_SIZE_BYTES,
    maxModelBytes: READ_MAX_FILE_SIZE_BYTES,
    strategy: "truncate",
    preview: {
      maxBytes: READ_MAX_FILE_SIZE_BYTES,
      direction: "head",
    },
  },
  timeout: {
    defaultMs: 30000,
    maxMs: READ_PDF_TOOL_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: true,
    cleanup: "required",
    userVisibleMessage: "Read was cancelled before file content was returned",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};
