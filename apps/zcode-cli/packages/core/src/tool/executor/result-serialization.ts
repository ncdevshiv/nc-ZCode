import {
  CoreErrorType,
  createCoreError,
  modelMessageContentToText,
  traceContextToLogContext,
  type ModelMessageContent,
  type ModelMessageContentBlock,
  type ToolResultBudget,
  type TraceContext,
} from "@zcode/contracts";
import {
  OFFICIAL_CUA_FRAME_MODEL_CONTENT_PROTECTION,
  containsOfficialCuaImageRefCredentialText,
} from "@zcode/zcode-cua/frame-contract";
import type { ToolEntry, ToolResultSerialization } from "../types.js";
import { formatHookAdditionalContexts } from "./hook-flow.js";
import {
  formatGenericPersistedOutputContent,
  isPersistedOutputContent,
} from "../result-persistence-format.js";
import {
  appendHookToPersistedArtifactPreview,
  appendHookToStringContent,
  appendHookWithoutReorderingStructuredContent,
  fitContentWithSuffix,
  OfficialCuaFrameContractError,
  projectHookAugmentedModelContent,
  projectOfficialCuaStructuredContent,
} from "./result-content-projection.js";
import type { ToolExecutorDeps } from "./types.js";
import { isRecord } from "./utils.js";

const DEFAULT_RESULT_BUDGET: ToolResultBudget = {
  maxInlineBytes: 100_000,
  maxModelBytes: 100_000,
  strategy: "truncate",
  preview: {
    direction: "head",
  },
};

const OFFICIAL_CUA_INVALID_RASTER_RECOVERY_TEXT =
  "This CUA raster is invalid and cannot be used in this request. " +
  "Do not send a coordinate target; capture a new raster first.";

export async function serializeOutput(
  deps: ToolExecutorDeps,
  output: unknown,
  entry: ToolEntry,
  traceContext: TraceContext,
  toolCallId: string,
  signal: AbortSignal,
): Promise<ToolResultSerialization> {
  const effectiveBudget: ToolResultBudget = entry.resultBudget ?? DEFAULT_RESULT_BUDGET;
  const modelContent = stringifyOutputForModel(output, entry);
  const content = stringifyModelContentForSerialization(modelContent);
  if (isEmptyModelContent(modelContent)) {
// Bash empty output also requires a universal placeholder to prevent the model from misinterpreting silent success as a missing tool result.
    const emptyContent = `(${entry.metadata.name} completed with no output)`;
    return {
      content: emptyContent,
      modelContent: emptyContent,
      originalBytes: Buffer.byteLength(content, "utf8"),
      returnedBytes: Buffer.byteLength(emptyContent, "utf8"),
      truncated: false,
      budgetStrategy: effectiveBudget.strategy,
    };
  }

  const contentType =
    entry.resultArtifactContentType ??
    (typeof output === "string" ? "text/plain" : "application/json");
  const originalBytes = Buffer.byteLength(content, "utf8");
// Some provider contracts are counted by JS characters; preserve UTF-8 for existing tools
// byte budget, only let tools that explicitly declare character thresholds increase this persistence criterion.
  const exceedsCharacterBudget =
    entry.maxModelChars !== undefined && content.length > entry.maxModelChars;
  const maxModelBytes = Math.max(
    0,
    Math.min(effectiveBudget.maxModelBytes, effectiveBudget.maxInlineBytes),
  );
  const artifactPath = findArtifactPath(output);
  const shouldPersistArtifact =
    (originalBytes > maxModelBytes || exceedsCharacterBudget) &&
    effectiveBudget.artifact?.enabled === true &&
    effectiveBudget.strategy === "artifact";
  const artifact = shouldPersistArtifact
    ? await tryWriteToolArtifact(
        deps,
        content,
        contentType,
        entry,
        effectiveBudget,
        traceContext,
        toolCallId,
        signal,
      )
    : undefined;
  const resolvedArtifactPath = artifact?.path ?? artifact?.uri ?? artifactPath;

  if (
    entry.modelContentProtection === OFFICIAL_CUA_FRAME_MODEL_CONTENT_PROTECTION &&
    hasImageBlock(modelContent)
  ) {
    let protectedProjection;
    try {
      protectedProjection = projectOfficialCuaStructuredContent(
        modelContent,
        maxModelBytes,
        effectiveBudget.preview?.direction ?? "head",
      );
    } catch (error) {
      if (!(error instanceof OfficialCuaFrameContractError)) throw error;
// Reason: non-canonical frames must continue to fail closed, but low-level layout invariants cannot be used as
// The tool result is exposed to the model; only stable diagnosis is recorded here, and then handed over to the executor to generate recoverable error results.
      deps.logger?.warn("Official CUA frame contract rejected during result serialization", {
        ...traceContextToLogContext(traceContext),
        code: error.code,
        event: "tool.result.cua_frame_contract_rejected",
        module: "core.tool.executor",
        status: "failed",
        toolCallId,
        toolName: entry.metadata.name,
      });
      throw createCoreError(
        CoreErrorType.ToolExecutionFailed,
        OFFICIAL_CUA_INVALID_RASTER_RECOVERY_TEXT,
        {
          context: { code: error.code, source: "tool" },
          recoverable: true,
        },
      );
    }
    if (protectedProjection) {
      const projectedModelContent = protectedProjection.content;
      const projectedContent = stringifyModelContentForSerialization(projectedModelContent);
      const projectedBytes = Buffer.byteLength(projectedContent, "utf8");
// image blocks in serialized text are just short placeholders; real base64 rasters (bridge capped at 200 KiB)
// Will enter the model request as is. The semantics of returnedBytes are "bytes sent to the model" and must be accounted for
// Image load, otherwise setOutputBytes / turn-tool-usage / usage-observability
// Each CUA frame systematically counts one less raster. Keep only aggregate, avoid text/media components
// Form a second state with returnedBytes that needs to be maintained synchronously.
      const structuredPayloadBytes = structuredMediaBytes(projectedModelContent);

// The official CUA's image/image_ref atomic pair is always retained as is; the image bytes are independent of the bridge's
// Upper limit protection, ordinary text still goes through the resultBudget, and you cannot borrow a legal raster to bypass the context budget.
// Return early and force pairing with official CUA protection authority; pairing verification fails
// The universal budget must not be bypassed when protectedProjection is undefined.
      return {
        content: projectedContent,
        modelContent: projectedModelContent,
        originalBytes,
        returnedBytes: projectedBytes + structuredPayloadBytes,
        truncated: protectedProjection.truncated,
        budgetStrategy: effectiveBudget.strategy,
        artifactPath: resolvedArtifactPath,
      };
    }
  }

  if (
    (originalBytes <= maxModelBytes && !exceedsCharacterBudget) ||
// Provider text with character threshold must retain the original text when persistence fails;
// Can no longer fall into the generic resultBudget truncation and inject another set of hints.
    (exceedsCharacterBudget && shouldPersistArtifact && artifact === undefined)
  ) {
    return {
      content,
      modelContent,
      originalBytes,
      returnedBytes: originalBytes,
      truncated: false,
      budgetStrategy: effectiveBudget.strategy,
      artifactPath: resolvedArtifactPath,
    };
  }

  if (effectiveBudget.strategy === "artifact" && effectiveBudget.artifact?.enabled === true) {
    if (artifact && resolvedArtifactPath) {
      const persistedOutputContent = formatPersistedOutputContent({
        content,
        entry,
        originalBytes,
        output,
        persistedPath: resolvedArtifactPath,
      });
      const persistedContent = stringifyModelContentForSerialization(persistedOutputContent);
      return {
        content: persistedContent,
        modelContent: persistedOutputContent,
        originalBytes,
        returnedBytes: Buffer.byteLength(persistedContent, "utf8"),
        truncated: true,
        budgetStrategy: effectiveBudget.strategy,
        artifactPath: resolvedArtifactPath,
      };
    }
  }

  const artifactHint = resolvedArtifactPath ? `artifactPath=${resolvedArtifactPath}, ` : "";
  const suffix = `\n\n[Tool output truncated by resultBudget: ${artifactHint}originalBytes=${originalBytes}, maxModelBytes=${maxModelBytes}, strategy=${effectiveBudget.strategy}]`;
  const truncatedContent = fitContentWithSuffix(
    content,
    maxModelBytes,
    suffix,
    effectiveBudget.preview?.direction ?? "head",
  );

  return {
    content: truncatedContent,
    modelContent: truncatedContent,
    originalBytes,
    returnedBytes: Buffer.byteLength(truncatedContent, "utf8"),
    truncated: true,
    budgetStrategy: effectiveBudget.strategy,
    artifactPath: resolvedArtifactPath,
  };
}

export function appendHookAdditionalContexts(
  serialization: ToolResultSerialization,
  additionalContexts: string[],
  entry: ToolEntry,
): ToolResultSerialization {
  if (additionalContexts.length === 0) return serialization;
  const filteredAdditionalContexts =
    entry.modelContentProtection === OFFICIAL_CUA_FRAME_MODEL_CONTENT_PROTECTION
      ? additionalContexts.filter((context) => !containsOfficialCuaImageRefCredentialText(context))
      : additionalContexts;
  const omittedFrameCredential = filteredAdditionalContexts.length !== additionalContexts.length;
  if (filteredAdditionalContexts.length === 0) {
    return omittedFrameCredential ? { ...serialization, truncated: true } : serialization;
  }
  const hookContext = formatHookAdditionalContexts(filteredAdditionalContexts);
  const suffix = `\n\n${hookContext}`;
  const maxModelBytes = resolveMaxModelBytes(entry);
  if (
    entry.modelContentProtection === OFFICIAL_CUA_FRAME_MODEL_CONTENT_PROTECTION &&
    hasImageBlock(serialization.modelContent ?? serialization.content)
  ) {
    const projected = appendHookWithoutReorderingStructuredContent(
      serialization,
      hookContext,
      suffix,
      maxModelBytes,
    );
    return omittedFrameCredential ? { ...projected, truncated: true } : projected;
  }
  const previewDirection = entry.resultBudget?.preview?.direction ?? "head";
  const artifactPreview = isPersistedArtifactPreview(serialization);
  const contentProjection = artifactPreview
    ? appendHookToPersistedArtifactPreview(serialization.content, suffix, maxModelBytes)
    : appendHookToStringContent(serialization.content, suffix, maxModelBytes, previewDirection);

  const projected = {
    ...serialization,
    content: contentProjection.content,
    modelContent: projectHookAugmentedModelContent({
      artifactPreview,
      contentProjection,
      hookContext,
      maxModelBytes,
      modelContent: serialization.modelContent ?? serialization.content,
      previewDirection,
      suffix,
    }),
    returnedBytes: Buffer.byteLength(contentProjection.content, "utf8"),
    truncated: serialization.truncated || contentProjection.truncated,
  };
  return omittedFrameCredential ? { ...projected, truncated: true } : projected;
}

function hasImageBlock(content: ModelMessageContent): content is ModelMessageContentBlock[] {
  return Array.isArray(content) && content.some((block) => block.type === "image");
}

/** The actual payload bytes of all image/file chunks in structured content (dataUrl is counted as is). */
function structuredMediaBytes(content: ModelMessageContent): number {
  if (!Array.isArray(content)) return 0;
  return content.reduce((total, block) => {
    if (block.type === "image") return total + Buffer.byteLength(block.dataUrl, "utf8");
// The visible content of the file-with-text model is text (dataUrl does not enter the request) and is not counted in the media bytes.
    if (block.type === "file" && block.dataUrl !== undefined && block.text === undefined)
      return total + Buffer.byteLength(block.dataUrl, "utf8");
    return total;
  }, 0);
}

function resolveMaxModelBytes(entry: ToolEntry): number {
  const budget = entry.resultBudget ?? DEFAULT_RESULT_BUDGET;
  return Math.max(0, Math.min(budget.maxModelBytes, budget.maxInlineBytes));
}

async function tryWriteToolArtifact(
  deps: ToolExecutorDeps,
  content: string,
  contentType: string,
  entry: ToolEntry,
  budget: ToolResultBudget,
  traceContext: TraceContext,
  toolCallId: string,
  signal: AbortSignal,
): Promise<{ path?: string; uri: string } | undefined> {
  if (!deps.artifactStore) {
    return undefined;
  }

  try {
    return await deps.artifactStore.writeToolResultArtifact(
      {
        sessionId: deps.sessionId,
        turnId: traceContext.turnId ?? deps.turnId,
        toolCallId,
        toolName: entry.metadata.name,
        content,
        contentType,
        retention: budget.artifact?.retention ?? "session",
        trace: traceContext,
      },
      { signal },
    );
  } catch {
// When interacting with the main model, artifact writing failure should not turn a successful tool call into a failure.
// Fallback here to the unified resultBudget truncation path, keeping the tool results visible and avoiding the original large output from entering the model.
    return undefined;
  }
}

function formatPersistedOutputContent(input: {
  content: string;
  entry: ToolEntry;
  originalBytes: number;
  output: unknown;
  persistedPath: string;
}): ModelMessageContent {
  const projected = input.entry.formatPersistedModelContent?.({
    content: input.content,
    originalBytes: input.originalBytes,
    output: input.output,
    persistedPath: input.persistedPath,
  });
  if (projected !== undefined) return projected;

  return formatGenericPersistedOutputContent({
    content: input.content,
    originalBytes: input.originalBytes,
    persistedPath: input.persistedPath,
  });
}

function isEmptyModelContent(content: ModelMessageContent): boolean {
  if (typeof content === "string") return content.trim() === "";
  if (content.length === 0) return true;
  return content.every((block) => {
    if (block.type !== "text") return false;
    return typeof block.text !== "string" || block.text.trim() === "";
  });
}

function stringifyOutputForModel(output: unknown, entry: ToolEntry): ModelMessageContent {
  if (entry.formatModelContent) return entry.formatModelContent(output);
  if (typeof output === "string") return output;
  if (output === undefined) return "";

  try {
    return JSON.stringify(output) ?? "";
  } catch {
    return String(output);
  }
}

function stringifyModelContentForSerialization(content: ModelMessageContent): string {
  return typeof content === "string" ? content : modelMessageContentToText(content);
}

function isPersistedArtifactPreview(serialization: ToolResultSerialization): boolean {
  return (
    serialization.budgetStrategy === "artifact" &&
    serialization.truncated &&
    typeof serialization.artifactPath === "string" &&
    serialization.artifactPath.length > 0 &&
    isPersistedOutputContent(serialization.content)
  );
}

function findArtifactPath(output: unknown): string | undefined {
  if (!isRecord(output)) return undefined;
  for (const key of ["persistedOutputPath", "rawOutputPath", "artifactPath", "outputPath"]) {
    const value = output[key];
    if (typeof value === "string" && value.length > 0) {
      return value;
    }
  }
  return undefined;
}
