// ============================================================
// Read File State Helpers
// ============================================================

import { platform as currentPlatform } from "node:process";
import { normalizeToolPathForComparison } from "./path-normalization.js";
import type { ReadFileStateEntry, ReadFileStateMap } from "./types.js";

type ReadFileStatePlatform = NodeJS.Platform;

function createReadFileStatePathKey(
  filePath: string,
  platform: ReadFileStatePlatform = currentPlatform,
): string {
  return normalizeToolPathForComparison(filePath, platform);
}

export function createReadFileStateKey(
  filePath: string,
  offset: number | undefined,
  limit: number | undefined,
  platform: ReadFileStatePlatform = currentPlatform,
): string {
  return [
    createReadFileStatePathKey(filePath, platform),
    String(offset ?? 1),
    limit === undefined ? "" : String(limit),
  ].join("\0");
}

export function findEditableReadFileState(
  readFileState: ReadFileStateMap | undefined,
  filePath: string,
  platform: ReadFileStatePlatform = currentPlatform,
): ReadFileStateEntry | undefined {
  return findLatestReadFileState(readFileState, filePath, platform);
}

export function findLatestReadFileState(
  readFileState: ReadFileStateMap | undefined,
  filePath: string,
  platform: ReadFileStatePlatform = currentPlatform,
): ReadFileStateEntry | undefined {
  if (!readFileState) return undefined;

// Returning to full Read first will allow Bash/formatter to restart the model after the file is modified.
// Range Read, Edit/Write still use the old full Read for mtime verification and continue to falsely report stale. Click here
// Single-file latest read-state semantic selection benchmark; true partial view rejected by consumer alone.
  return findLatestReadFileStateByPath(readFileState, filePath, platform, () => true);
}

export function normalizeReadFileStateMtimeMs(mtimeMs: number | undefined): number | undefined {
  if (mtimeMs === undefined) return undefined;
  return Math.floor(mtimeMs);
}

function findLatestReadFileStateByPath(
  readFileState: ReadFileStateMap,
  filePath: string,
  platform: ReadFileStatePlatform,
  accepts: (entry: ReadFileStateEntry) => boolean,
): ReadFileStateEntry | undefined {
  const pathKey = createReadFileStatePathKey(filePath, platform);
  let latest: ReadFileStateEntry | undefined;
  let latestReadAt = Number.NEGATIVE_INFINITY;
  for (const entry of readFileState.values()) {
    if (createReadFileStatePathKey(entry.path, platform) !== pathKey) continue;
    if (!accepts(entry)) continue;
    const readAt = entry.readAt.getTime();
    if (readAt < latestReadAt) continue;
    latest = entry;
    latestReadAt = readAt;
  }
  return latest;
}
