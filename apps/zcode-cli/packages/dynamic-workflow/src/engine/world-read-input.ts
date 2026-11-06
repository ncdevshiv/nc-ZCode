// ============================================================
// Bounded input for world-read / world-run nodes
// ============================================================
// `inputHash` only answers "Is it the same input?"; the workspace transcript needs to answer "What is the input?". Put here
// `{op, args}` pushes 4 KB: most inputs (a path, a pattern, an argv) are far smaller than the upper limit,
// Drop the library as it is; only `world.run("node", ["-e", <a large section of code>])` will be intercepted. Truncation is **termwise
// String preview** instead of discarding it entirely - "ran node -e ... (intercepted)" is much more useful in auditing than a NULL.

import { WORLD_READ_INPUT_MAX_BYTES, type WorldReadInput } from "./types.js";

/** Maximum number of characters retained for a single argument in truncation mode: 4 KB divided equally among up to 8 arguments, before serialization overhead. */
const TRUNCATED_ARG_MAX_CHARS = 400;
const MAX_ARGS_KEPT = 8;

/** Number of UTF-8 bytes. Use `TextEncoder` instead of `Buffer`: This package maintains zero node built-in dependencies (same discipline as engine.ts). */
function utf8Length(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** A string preview of an actual parameter: the string as it is, the rest as JSON; super long tail truncated and ellipsis added. */
function previewArg(value: unknown): string {
  let text: string;
  if (typeof value === "string") text = value;
  else {
    try {
      text = JSON.stringify(value) ?? String(value);
    } catch {
      text = String(value);
    }
  }
  return text.length > TRUNCATED_ARG_MAX_CHARS
    ? `${text.slice(0, TRUNCATED_ARG_MAX_CHARS)}…`
    : text;
}

/**
* Push `{op, args}` into {@link WORLD_READ_INPUT_MAX_BYTES}.
 *
* Fast path: after serialization, it will be returned as it is without exceeding the limit (the actual parameters retain the original type - the opts object of `world.run`, `git.log`
* numbers are still themselves). Slow path: each argument is replaced by a string preview, up to 8, concatenated with `truncated`.
* Non-serializable arguments (with loops, BigInt) take the same slow path - they are not originally represented in the journal.
 */
export function boundWorldReadInput(op: string, args: readonly unknown[]): WorldReadInput {
  const plain: WorldReadInput = { op, args: [...args] };
  try {
    const text = JSON.stringify(plain);
    if (text !== undefined && utf8Length(text) <= WORLD_READ_INPUT_MAX_BYTES) return plain;
  } catch {
// Drop the preview path below.
  }
  return {
    op,
    args: args.slice(0, MAX_ARGS_KEPT).map(previewArg),
    truncated: true,
  };
}
