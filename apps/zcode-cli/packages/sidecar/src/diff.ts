//! diff facade: jsdiff compatibility surface, behind which is Rust sidecar (`zcode-diff`, byte-parity:
//! 87 real jsdiff@9 fixtures). flag off = jsdiff original path (zero behavior change); flag on =
//! sidecar, **downgrade jsdiff when unavailable and alert once** (availability incidents do not affect user rounds), protocol/
//! Handler errors will always be thrown (configuration and version offsets are build accidents and will not be silent).
//!
//! Timeout semantic alignment jsdiff: `timeout` expires → sidecar returns null → this layer is mapped to
//! `undefined` (timeout return value of jsdiff). The caller's existing `patch?.hunks ?? []` continues to be established.

import {
  applyPatch as jsdiffApplyPatch,
  createTwoFilesPatch as jsdiffCreateTwoFilesPatch,
  diffLines as jsdiffDiffLines,
  structuredPatch as jsdiffStructuredPatch,
} from "diff";
import type { StructuredPatch } from "diff";

import { sidecarEnabled } from "./config.js";
import { sidecarPool } from "./pool.js";
import { SidecarUnavailableError } from "./types.js";

const DIFF_SIDECAR = "zcode-diff";
const DIFF_TIMEOUT_MS = 5_000;
const REQUEST_TIMEOUT_MS = DIFF_TIMEOUT_MS + 5_000;

const warned = new Set<string>();

function warnOnce(key: string, message: string): void {
  if (warned.has(key)) return;
  warned.add(key);
  process.stderr.write(`[sidecar:${DIFF_SIDECAR}] ${message}\n`);
}

async function callDiffMethod(method: string, params: Record<string, unknown>): Promise<unknown> {
  const process = await sidecarPool().acquire(DIFF_SIDECAR);
  try {
    return await process.request(method, params, REQUEST_TIMEOUT_MS);
  } finally {
    sidecarPool().release(process);
  }
}

function degraded(key: string, error: SidecarUnavailableError): void {
  warnOnce(key, `${error.message}; falling back to jsdiff for this session`);
}

export interface StructuredPatchOptions {
  context?: number;
  timeout?: number;
}

export async function nativeStructuredPatch(
  oldFileName: string,
  newFileName: string,
  oldStr: string,
  newStr: string,
  oldHeader?: string,
  newHeader?: string,
  options?: StructuredPatchOptions,
): Promise<StructuredPatch | undefined> {
  if (!sidecarEnabled("diff")) {
    return jsdiffStructuredPatch(oldFileName, newFileName, oldStr, newStr, oldHeader, newHeader, options);
  }
  try {
    const result = (await callDiffMethod("structuredPatch", {
      old: oldStr,
      new: newStr,
      oldLabel: oldFileName,
      newLabel: newFileName,
      oldHeader,
      newHeader,
      context: options?.context,
      timeoutMs: options?.timeout,
    })) as StructuredPatch | null;
    return result ?? undefined;
  } catch (error) {
    if (error instanceof SidecarUnavailableError) {
      degraded("structuredPatch", error);
      return jsdiffStructuredPatch(oldFileName, newFileName, oldStr, newStr, oldHeader, newHeader, options);
    }
    throw error;
  }
}

export interface ApplyPatchOptions {
  fuzzFactor?: number;
  autoConvertLineEndings?: boolean;
}

export async function nativeUnifiedPatch(
  oldFileName: string,
  newFileName: string,
  oldStr: string,
  newStr: string,
  oldHeader?: string,
  newHeader?: string,
  options?: StructuredPatchOptions,
): Promise<string | undefined> {
// sidecar's diff.unified image jsdiff `createTwoFilesPatch` (two file patch:
// label only goes into ---/+++ header); fallback must have the same semantics, otherwise the behavior of both sides of flag will be inconsistent.
  if (!sidecarEnabled("diff")) {
    return jsdiffCreateTwoFilesPatch(
      oldFileName,
      newFileName,
      oldStr,
      newStr,
      oldHeader,
      newHeader,
      options,
    );
  }
  try {
// sidecar protocol shape: {"patch": string | null} (null = timeout).
    const result = (await callDiffMethod("diff.unified", {
      old: oldStr,
      new: newStr,
      oldLabel: oldFileName,
      newLabel: newFileName,
      oldHeader,
      newHeader,
      context: options?.context,
      timeoutMs: options?.timeout,
    })) as { patch: string | null } | null;
    return result?.patch ?? undefined;
  } catch (error) {
    if (error instanceof SidecarUnavailableError) {
      degraded("unifiedPatch", error);
      return jsdiffCreateTwoFilesPatch(
        oldFileName,
        newFileName,
        oldStr,
        newStr,
        oldHeader,
        newHeader,
        options,
      );
    }
    throw error;
  }
}

export interface DiffLinesChunk {
  count: number;
  added: boolean;
  removed: boolean;
  value: string;
}

export async function nativeDiffLines(
  oldStr: string,
  newStr: string,
): Promise<DiffLinesChunk[]> {
  if (!sidecarEnabled("diff")) {
    return jsdiffDiffLines(oldStr, newStr) as unknown as DiffLinesChunk[];
  }
  try {
    const result = (await callDiffMethod("diff.lines", { old: oldStr, new: newStr })) as {
      chunks: DiffLinesChunk[];
    };
    return result.chunks;
  } catch (error) {
    if (error instanceof SidecarUnavailableError) {
      degraded("diffLines", error);
      return jsdiffDiffLines(oldStr, newStr) as unknown as DiffLinesChunk[];
    }
    throw error;
  }
}

export async function nativeApplyPatch(
  source: string,
  patch: StructuredPatch | [StructuredPatch],
  options?: ApplyPatchOptions,
): Promise<string | false> {
  if (!sidecarEnabled("diff")) {
    return jsdiffApplyPatch(source, patch, options);
  }
  const fuzzFactor = options?.fuzzFactor ?? 0;
  try {
    const result = (await callDiffMethod("diff.applyPatch", {
      source,
      hunks: patch,
      fuzzFactor,
    })) as { applied: boolean; result?: string };
    return result.applied ? (result.result ?? "") : false;
  } catch (error) {
    if (error instanceof SidecarUnavailableError) {
      degraded("applyPatch", error);
      return jsdiffApplyPatch(source, patch, options);
    }
    throw error;
  }
}

export { DIFF_SIDECAR, DIFF_TIMEOUT_MS };
