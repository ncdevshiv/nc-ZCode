//! diff facade：jsdiff 兼容面，背后是 Rust sidecar（`zcode-diff`，byte-parity：
//! 87 个真实 jsdiff@9 fixture）。flag 关 = jsdiff 原路径（零行为变化）；flag 开 =
//! sidecar，**不可用时降级 jsdiff 并告警一次**（可用性事故不影响用户轮次），协议/
//! handler 错误一律抛错（配置与版本偏移是构建事故，不静默）。
//!
//! 超时语义对齐 jsdiff：`timeout` 到期 → sidecar 返回 null → 本层映射为
//! `undefined`（jsdiff 的超时返回值）。调用方已有的 `patch?.hunks ?? []` 继续成立。

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
  // sidecar 的 diff.unified 镜像 jsdiff `createTwoFilesPatch`（两文件补丁：
  // label 只进 ---/+++ 头）；fallback 必须同语义，否则 flag 两侧行为不一致。
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
    // sidecar 协议形状：{"patch": string | null}（null = 超时）。
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
