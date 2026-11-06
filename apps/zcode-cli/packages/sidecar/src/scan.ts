//! scan facade: data root traversal (replaces the hot path of TS `storage/adapters/fsWalker.ts`).
//!
//! Semantic mirror: `entry = {relativePath, bytes, mtimeMs}` (TS `StorageScanEntry`),
//! Progress `scan_entry` / `scan_error` event, `scan_done` final state; root missing =
//! `scan_done.missingRoot=true` and no error is generated (the same as TS "Root does not exist = empty root is not an error"
//! consistent). The traversal order follows the serial semantics of sidecar (equivalent to TS concurrency=1; concurrency only affects
//! Order, does not affect collections - README difference table). Directory count sidecar is not provided, always 0 and here
//! Explicit record (TS side can be derived from the entry collection on demand).
//!
//! cancel: The facade sends `cancel` (according to scanId) when AbortSignal is triggered, and the event consumption is stopped immediately.

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
/** Sidecar does not report the directory number (only entries/errors), which is always 0; the caller derives it from the collection on demand. */
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
* Scan a root directory and collect all entries (event-driven closure: `scan` response + `scan_done` event).
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
//The response is only ACK; all entries and final states are event-driven and will not be completed until scan_done.
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
// Sidecar is unavailable: the caller must return the TS walker (this facade only guarantees that it will be available when the TS is available).
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
