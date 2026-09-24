//! scan facade：数据根遍历（替代 TS `storage/adapters/fsWalker.ts` 的热路径）。
//!
//! 语义镜像：`entry = {relativePath, bytes, mtimeMs}`（TS `StorageScanEntry`），
//! 进度走 `scan_entry` / `scan_error` 事件，`scan_done` 终态；根缺失 =
//! `scan_done.missingRoot=true` 且不产生错误（与 TS「根不存在 = 空根非错误」
//! 一致）。遍历顺序按 sidecar 的串行语义（与 TS concurrency=1 等价；并发只影响
//! 顺序，不影响集合——README 差异表）。目录计数 sidecar 不提供，恒为 0 并在此
//! 显式记录（TS 侧按需可从条目集合派生）。
//!
//! cancel：facade 在 AbortSignal 触发时发 `cancel`（按 scanId），事件消费随即收口。

import { sidecarEnabled } from "./config.js";
import { sidecarPool } from "./pool.js";
import { SidecarUnavailableError, type SidecarEventListener } from "./types.js";
import type { SidecarProcess } from "./process.js";

const SCAN_SIDECAR = "zcode-scan";
const REQUEST_TIMEOUT_MS = 15_000;
const CANCEL_TIMEOUT_MS = 2_000;

export interface NativeScanEntry {
  relativePath: string;
  bytes: number;
  mtimeMs: number;
}

export interface NativeScanError {
  path: string;
  code: string;
}

export interface NativeScanResult {
  /** sidecar 不报目录数（只有条目/错误），恒为 0；调用方按需从集合派生。 */
  directoriesScanned: number;
  filesScanned: number;
  missingRoot: boolean;
  entries: NativeScanEntry[];
  errors: NativeScanError[];
}

export interface NativeScanOptions {
  recursive?: boolean;
  maxDepth?: number;
  followSymlinks?: boolean;
  signal?: AbortSignal;
}

let scanCounter = 0;

export function scanEnabled(): boolean {
  return sidecarEnabled("scan");
}

/**
 * 扫描一个根目录并收集全部条目（事件驱动收口：`scan` 响应 + `scan_done` 事件）。
 */
export async function nativeScanStorageRoot(
  rootPath: string,
  options: NativeScanOptions = {},
): Promise<NativeScanResult> {
  const sidecar = await sidecarPool().acquire(SCAN_SIDECAR);
  scanCounter += 1;
  const scanId = `scan-${Date.now()}-${scanCounter}`;

  let resolveDone: (() => void) | null = null;
  const done = new Promise<void>((resolve) => {
    resolveDone = resolve;
  });
  const entries: NativeScanEntry[] = [];
  const errors: NativeScanError[] = [];
  let missingRoot = false;

  const listener: SidecarEventListener = (event, data) => {
    if (!data || typeof data !== "object") return;
    const record = data as Record<string, unknown>;
    if (record.scanId !== scanId) return;
    if (event === "scan_entry") {
      const entry = record.entry as NativeScanEntry | undefined;
      if (entry && typeof entry.relativePath === "string") entries.push(entry);
    } else if (event === "scan_error") {
      errors.push({
        path: typeof record.path === "string" ? record.path : "",
        code: typeof record.code === "string" ? record.code : "UNKNOWN",
      });
    } else if (event === "scan_done") {
      missingRoot = record.missingRoot === true;
      resolveDone?.();
    }
  };
  sidecar.onEvent(listener);

  const onAbort = (): void => {
    void sidecar.request("cancel", { scanId }, CANCEL_TIMEOUT_MS).catch(() => undefined);
  };
  if (options.signal) {
    if (options.signal.aborted) onAbort();
    else options.signal.addEventListener("abort", onAbort, { once: true });
  }

  try {
    await sidecar.request(
      "scan",
      {
        path: rootPath,
        scanId,
        recursive: options.recursive,
        maxDepth: options.maxDepth,
        followSymlinks: options.followSymlinks,
      },
      REQUEST_TIMEOUT_MS,
    );
    // 响应只是 ACK；条目与终态全部走事件，等 scan_done 才算完成。
    if (options.signal?.aborted) {
      throw new DOMException("storage scan aborted", "AbortError");
    }
    await done;
    return {
      directoriesScanned: 0,
      filesScanned: entries.length,
      missingRoot,
      entries,
      errors,
    };
  } catch (error) {
    if (error instanceof SidecarUnavailableError) {
      // sidecar 不可用：调用方必须退回 TS walker（本 facade 只保证 TS 可得时可用）。
      globalThis.process.stderr.write(
        `[sidecar:${SCAN_SIDECAR}] ${error.message}; caller must fall back to the TS walker\n`,
      );
    }
    throw error;
  } finally {
    sidecar.offEvent(listener);
    options.signal?.removeEventListener("abort", onAbort);
    if (sidecar.isAlive()) sidecarPool().release(sidecar);
  }
}

export { SCAN_SIDECAR };
export type { SidecarProcess };
