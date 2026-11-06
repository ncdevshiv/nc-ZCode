import { nativeStructuredPatch } from "@zcode/sidecar";
import type { DiffHunk, TurnFileChangeSummary } from "../deps.js";
import type { RuntimeTurnFileChangeMap } from "../types.js";

const DIFF_TIMEOUT_MS = 5_000;

export async function recordTurnFileChange(
  changes: RuntimeTurnFileChangeMap,
  input: {
    afterContent?: string;
    beforeContent: string | null;
    path: string;
    structuredPatch: DiffHunk[];
    toolName: string;
  },
): Promise<void> {
  const fallback = countPatchLines(input.structuredPatch);
  const existing = changes.get(input.path);
  if (!existing) {
    changes.set(input.path, {
      afterContent: input.afterContent,
      beforeContent: input.beforeContent,
      fallbackAdditions: fallback.additions,
      fallbackDeletions: fallback.deletions,
      path: input.path,
      toolNames: new Set([input.toolName]),
      writeCount: 1,
    });
    return;
  }

  existing.afterContent = input.afterContent ?? existing.afterContent;
  existing.fallbackAdditions += fallback.additions;
  existing.fallbackDeletions += fallback.deletions;
  existing.toolNames.add(input.toolName);
  existing.writeCount += 1;
}

export async function buildTurnFileChangeSummary(
  changes: RuntimeTurnFileChangeMap,
): Promise<TurnFileChangeSummary | undefined> {
  if (changes.size === 0) return undefined;

  const mapped = await Promise.all(
    Array.from(changes.values()).map(async (entry) => {
      const stat =
        entry.afterContent === undefined
          ? {
              additions: entry.fallbackAdditions,
              deletions: entry.fallbackDeletions,
            }
          : await diffLineStat(entry.beforeContent ?? "", entry.afterContent, entry.path);
      return {
        additions: stat.additions,
        deletions: stat.deletions,
        path: entry.path,
        toolNames: Array.from(entry.toolNames).sort(),
        writeCount: entry.writeCount,
      };
    }),
  );
  const items = mapped.sort((left, right) => left.path.localeCompare(right.path));

  return {
    additions: items.reduce((total, item) => total + item.additions, 0),
    deletions: items.reduce((total, item) => total + item.deletions, 0),
    files: items.length,
    items,
  };
}

async function diffLineStat(
  beforeContent: string,
  afterContent: string,
  filePath: string,
): Promise<{ additions: number; deletions: number }> {
// flag off = jsdiff (internal to facade); flag on = Rust zcode-diff sidecar.
  const patch = await nativeStructuredPatch(
    filePath,
    filePath,
    beforeContent,
    afterContent,
    undefined,
    undefined,
    { timeout: DIFF_TIMEOUT_MS },
  );
  return countPatchLines(patch?.hunks ?? []);
}

function countPatchLines(hunks: DiffHunk[]): { additions: number; deletions: number } {
  let additions = 0;
  let deletions = 0;

  for (const hunk of hunks) {
    for (const line of hunk.lines) {
      if (line.startsWith("+")) additions += 1;
      if (line.startsWith("-")) deletions += 1;
    }
  }

  return { additions, deletions };
}
