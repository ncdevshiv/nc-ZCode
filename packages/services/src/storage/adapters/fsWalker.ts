/**
 * 数据根遍历器：异步、有界并发、可取消、不跟随符号链接。
 * 产出 (relativePath, bytes, mtimeMs)，分类与聚合由 domain 在调用方完成。
 * 每处理 yieldEvery 个条目让出一次事件循环，避免在 Worker/host 内长时间独占。
 */
import { lstat, opendir } from "node:fs/promises";
import { join, relative, sep } from "node:path";
import { nativeScanStorageRoot, scanEnabled } from "@zcode/sidecar";
import type { NativeScanResult } from "@zcode/sidecar";
import type { StorageScanEntry } from "../domain/usageAggregate.js";
import type { StoragePathError } from "@zcode/shared";

interface WalkStorageRootOptions {
  rootPath: string;
  onEntry: (entry: StorageScanEntry) => void;
  onError?: (error: StoragePathError) => void;
  signal?: AbortSignal;
  /** 同时打开的目录数，默认 4。 */
  concurrency?: number;
  /** 每处理多少条目让出一次事件循环，默认 256。 */
  yieldEvery?: number;
}

interface WalkStorageRootResult {
  directoriesScanned: number;
  filesScanned: number;
  /** 根目录本身不存在时为 true（视为空根，不算错误）。 */
  missingRoot: boolean;
}

function createStorageAbortError(): Error {
  return new DOMException("storage scan aborted", "AbortError");
}

function errorCode(error: unknown): string {
  return error && typeof error === "object" && "code" in error && typeof error.code === "string"
    ? error.code
    : "UNKNOWN";
}

function toRelative(rootPath: string, absolutePath: string): string {
  return relative(rootPath, absolutePath).split(sep).join("/");
}

/**
 * Rust sidecar 扫描：成功返回结果；sidecar 不可用返回 null（调用方继续走 TS
 * 遍历）。条目/错误按事件到达序回流，语义与 onEntry/onError 回调一致。
 */
async function tryNativeScan(
  options: WalkStorageRootOptions,
): Promise<WalkStorageRootResult | null> {
  let result: NativeScanResult;
  try {
    result = await nativeScanStorageRoot(options.rootPath, {
      recursive: true,
      followSymlinks: false,
    });
  } catch {
    // SidecarUnavailableError 时 facade 已告警；其余错误同样不得让扫描失败。
    return null;
  }
  for (const entry of result.entries as StorageScanEntry[]) {
    options.onEntry(entry);
  }
  for (const error of result.errors as StoragePathError[]) {
    options.onError?.(error);
  }
  return {
    directoriesScanned: 0,
    filesScanned: result.filesScanned,
    missingRoot: result.missingRoot,
  };
}

export async function walkStorageRoot(
  options: WalkStorageRootOptions,
): Promise<WalkStorageRootResult> {
  // Rust sidecar（zcode-scan）路径：flag 开且 sidecar 可用时由 Rust 遍历，
  // 本文件只做条目/错误回流。差异（顺序 = 串行、目录计数不提供）记录在
  // native/crates/zcode-scan/README.md 与 RUST-PORT-LOG。
  if (!options.signal) {
    if (scanEnabled()) {
      const native = await tryNativeScan(options);
      if (native) return native;
    }
  }
  const { rootPath, onEntry, onError, signal } = options;
  const concurrency = Math.max(1, options.concurrency ?? 4);
  const yieldEvery = Math.max(1, options.yieldEvery ?? 256);
  const pending: string[] = [rootPath];
  const result: WalkStorageRootResult = {
    directoriesScanned: 0,
    filesScanned: 0,
    missingRoot: false,
  };
  let processedSinceYield = 0;
  let active = 0;

  const throwIfAborted = () => {
    if (signal?.aborted) throw createStorageAbortError();
  };
  const maybeYield = async () => {
    processedSinceYield += 1;
    if (processedSinceYield >= yieldEvery) {
      processedSinceYield = 0;
      await new Promise<void>((resolve) => setImmediate(resolve));
      throwIfAborted();
    }
  };

  const scanDirectory = async (directoryPath: string): Promise<void> => {
    throwIfAborted();
    let directory;
    try {
      directory = await opendir(directoryPath);
    } catch (error) {
      if (directoryPath === rootPath && errorCode(error) === "ENOENT") {
        result.missingRoot = true;
        return;
      }
      onError?.({ path: toRelative(rootPath, directoryPath), code: errorCode(error) });
      return;
    }
    result.directoriesScanned += 1;
    try {
      for await (const dirent of directory) {
        throwIfAborted();
        const entryPath = join(directoryPath, dirent.name);
        if (dirent.isSymbolicLink()) continue;
        if (dirent.isDirectory()) {
          pending.push(entryPath);
          continue;
        }
        if (!dirent.isFile()) continue;
        try {
          const stats = await lstat(entryPath);
          if (!stats.isFile()) continue;
          result.filesScanned += 1;
          onEntry({
            relativePath: toRelative(rootPath, entryPath),
            bytes: stats.size,
            mtimeMs: stats.mtimeMs,
          });
        } catch (error) {
          // 扫描期间文件可能被日志轮转或 Agent 删除；ENOENT 属于正常竞态，不计为错误。
          if (errorCode(error) !== "ENOENT") {
            onError?.({ path: toRelative(rootPath, entryPath), code: errorCode(error) });
          }
        }
        await maybeYield();
      }
    } finally {
      await directory.close().catch(() => {});
    }
  };

  // 有界并发：最多 concurrency 个目录同时打开；队列为空且无活动任务时结束。
  await new Promise<void>((resolve, reject) => {
    let failed = false;
    const pump = () => {
      if (failed) return;
      if (pending.length === 0 && active === 0) {
        resolve();
        return;
      }
      while (active < concurrency && pending.length > 0) {
        const next = pending.pop()!;
        active += 1;
        scanDirectory(next)
          .then(() => {
            active -= 1;
            pump();
          })
          .catch((error: unknown) => {
            failed = true;
            reject(error);
          });
      }
    };
    pump();
  });
  return result;
}
