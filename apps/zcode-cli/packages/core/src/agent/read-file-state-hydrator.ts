import type {
  MessageId,
  MessagePart,
  MessageWithParts,
  ToolPart,
} from "@zcode/contracts";
import {
  parseReadFileStateMetadata,
  type PersistedReadFileStateTool,
} from "../tool/read-file-state-metadata.js";
import { createReadFileStateKey, normalizeReadFileStateMtimeMs } from "../tool/read-file-state.js";
import type { ReadFileStateMap } from "../tool/types.js";
import { activeSessionMessages } from "./session-history-hydrator.js";

export interface ReadFileStateHydrationResult {
  restoredCount: number;
  skippedRangeReadCount: number;
  skippedUnreadableEditCount: number;
}

type CompletedToolPart = ToolPart & {
  state: ToolPart["state"] & {
    output: unknown;
    status: "completed";
  };
};

export async function hydrateReadFileStateFromSession(input: {
  branchCutAfterMessageId?: MessageId;
  messages: MessageWithParts[];
  readFileState: ReadFileStateMap;
  rewindCreatedMessageId?: MessageId;
  rewindKeptMessageIds?: readonly MessageId[];
  rewindTargetMessageId?: MessageId;
  workingDirectory: string;
  workspaceRoot: string;
}): Promise<ReadFileStateHydrationResult> {
  input.readFileState.clear();
  const activeMessages = activeSessionMessages(input.messages, {
    branchCutAfterMessageId: input.branchCutAfterMessageId,
    includeCompactPreservedSegment: false,
    rewindCreatedMessageId: input.rewindCreatedMessageId,
    rewindKeptMessageIds: input.rewindKeptMessageIds,
    rewindTargetMessageId: input.rewindTargetMessageId,
  });

  const result: ReadFileStateHydrationResult = {
    restoredCount: 0,
    skippedRangeReadCount: 0,
    skippedUnreadableEditCount: 0,
  };

  for (const message of activeMessages) {
    if (message.info.role !== "assistant") continue;

    for (const part of dedupeParts(message.parts)) {
      if (!isCompletedToolPart(part)) continue;

      if (part.tool === "Read") {
        const restored = restoreReadToolState(input, part, result);
        if (restored) result.restoredCount++;
        continue;
      }

      if (part.tool === "Write") {
        const restored = restoreMetadataToolState(input.readFileState, part, "Write");
        if (restored) result.restoredCount++;
        continue;
      }

      if (part.tool === "Edit") {
        const restored = restoreMetadataToolState(input.readFileState, part, "Edit");
        if (restored) result.restoredCount++;
      }
    }
  }

  return result;
}

function restoreReadToolState(
  input: {
    readFileState: ReadFileStateMap;
  },
  part: CompletedToolPart,
  result: ReadFileStateHydrationResult,
): boolean {
  const toolInput = asRecord(part.state.input);
  if (!toolInput) return false;
  if (!isHistoricalFullReadWindow(toolInput as HistoricalReadWindow)) {

// The real range read is only used as the latest water level within the same runtime and will not be restored across resumes.
    result.skippedRangeReadCount++;
    return false;
  }

  const metadata = parseReadFileStateMetadata(part.state.metadata);
  if (!metadata) return false;
  if (metadata.tool !== "Read") return false;
  if (!isHistoricalFullReadWindow(metadata)) {
    return false;
  }
  setFullReadState(input.readFileState, metadata.path, metadata.content, {
// resume no longer resumes Read state from provider-visible cat-n text; only with
// The structured metadata of mtimeMs/revisionId/sizeBytes can support subsequent stale guard.
    isPartialView: metadata.isPartialView,
    mtimeMs: normalizeReadFileStateMtimeMs(metadata.mtimeMs),
    readAt: new Date(metadata.readAtMs),
    revisionId: metadata.revisionId,
    sizeBytes: metadata.sizeBytes,
    sourceTool: metadata.tool,
  });
  return true;
}

function restoreMetadataToolState(
  readFileState: ReadFileStateMap,
  part: CompletedToolPart,
  expectedTool: PersistedReadFileStateTool,
): boolean {
  const metadata = parseReadFileStateMetadata(part.state.metadata);
  if (!metadata || metadata.tool !== expectedTool) return false;
  if (!isHistoricalFullReadWindow(metadata)) return false;

// Write/Edit history tool part cannot read the current disk to "complete" the status during resume;
// External manual saving will be mistakenly authenticated as agent read. Here only the complete snapshot persisted upon success is restored.
  setFullReadState(readFileState, metadata.path, metadata.content, {
    isPartialView: metadata.isPartialView,
    mtimeMs: normalizeReadFileStateMtimeMs(metadata.mtimeMs),
    readAt: new Date(metadata.readAtMs),
    revisionId: metadata.revisionId,
    sizeBytes: metadata.sizeBytes,
    sourceTool: metadata.tool,
  });
  return true;
}

function setFullReadState(
  readFileState: ReadFileStateMap,
  filePath: string,
  content: string,
  metadata: {
    isPartialView?: boolean;
    mtimeMs?: number;
    readAt: Date;
    revisionId?: string;
    sizeBytes?: number;
    sourceTool?: PersistedReadFileStateTool;
  },
): void {
  readFileState.set(createReadFileStateKey(filePath, 1, undefined), {
    path: filePath,
    content,
    offset: undefined,
    limit: undefined,
    isPartialView: metadata.isPartialView ?? false,
    readAt: metadata.readAt,
    sourceTool: metadata.sourceTool,
    revisionId: metadata.revisionId,
    mtimeMs: metadata.mtimeMs,
    sizeBytes: metadata.sizeBytes ?? Buffer.byteLength(content, "utf8"),
  });
}

interface HistoricalReadWindow {
  limit?: number;
  offset?: number;
}

function isHistoricalFullReadWindow({ offset, limit }: HistoricalReadWindow): boolean {
  return (offset ?? 1) <= 1 && limit === undefined;
}

function dedupeParts(parts: MessagePart[]): MessagePart[] {
  const byId = new Map<string, MessagePart>();
  for (const part of parts) {
    byId.set(part.id, part);
  }
  return [...byId.values()];
}

function isCompletedToolPart(part: MessagePart): part is CompletedToolPart {
  return part.type === "tool" && part.state.status === "completed" && "output" in part.state;
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
  return value as Record<string, unknown>;
}
