import { nativeStructuredPatch } from "@zcode/sidecar";
import type { DiffHunk } from "@zcode/contracts";

const CONTEXT_LINES = 3;
const DIFF_TIMEOUT_MS = 5_000;

export const createStructuredPatch = async ({
  filePath,
  newContent,
  oldContent,
}: {
  filePath: string;
  newContent: string;
  oldContent: string;
}): Promise<DiffHunk[]> => {
// flag off = jsdiff (sidecar facade internal downgrade path); flag on = Rust zcode-diff
// sidecar (byte-parity: 87 real jsdiff fixtures). When the facade is unavailable, the facade will alert and downgrade.
  const patch = await nativeStructuredPatch(
    filePath,
    filePath,
    oldContent,
    newContent,
    undefined,
    undefined,
    {
      context: CONTEXT_LINES,
      timeout: DIFF_TIMEOUT_MS,
    },
  );

  return patch?.hunks ?? [];
};

export const countPatchLines = (
  structuredPatch: DiffHunk[],
): { additions: number; deletions: number } => {
  let additions = 0;
  let deletions = 0;

  for (const hunk of structuredPatch) {
    for (const line of hunk.lines) {
      if (line.startsWith("+")) additions += 1;
      if (line.startsWith("-")) deletions += 1;
    }
  }

  return { additions, deletions };
};
