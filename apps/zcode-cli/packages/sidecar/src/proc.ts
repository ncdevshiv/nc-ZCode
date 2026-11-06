//! proc facade: process tree enumeration/forced killing (replaces TS `taskkill /T /F` and capture process list).
//!
//! Semantic alignment TS `adapters/src/mcp/process-tree.ts`: when the root PID is dead/does not exist
//! `kill_tree` is a no-operation success (no error is reported for an empty result) - this is the sidecar contract (see
//! native/crates/zcode-proc/README). flag off = TS original implementation; flag on = sidecar;
//! Sidecar is unavailable → Warn once and fall back to TS implementation (does not change best-effort recycling semantics).
//!
//! Note: This facade only covers **trees with existing PIDs**; `spawn_in_job` requires Windows Job
//! Object life cycle management, TS side `windows-job-object.ts` still owns this side (README difference table).

import { sidecarEnabled } from "./config.js";
import { sidecarPool } from "./pool.js";
import { SidecarUnavailableError } from "./types.js";

const PROC_SIDECAR = "zcode-proc";
const KILL_TREE_TIMEOUT_MS = 2_000;
const REQUEST_TIMEOUT_MS = 8_000;

const warned = new Set<string>();

function warnOnce(key: string, message: string): void {
  if (warned.has(key)) return;
  warned.add(key);
  process.stderr.write(`[sidecar:${PROC_SIDECAR}] ${message}\n`);
}

export interface ProcessInfo {
  pid: number;
  ppid: number;
  name: string;
}

export interface KillTreeFailure {
  pid: number;
  error: string;
}

export interface KillTreeResult {
  killed: number[];
  failed: KillTreeFailure[];
}

interface KillTreeWire {
  killed?: unknown;
  failed?: unknown;
}

function asPidArray(value: unknown): number[] {
  return Array.isArray(value) ? value.filter((item): item is number => typeof item === "number") : [];
}

function asFailureArray(value: unknown): KillTreeFailure[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((item) => {
    if (item && typeof item === "object" && "pid" in item && "error" in item) {
      const record = item as { pid: unknown; error: unknown };
      if (typeof record.pid === "number") {
        return [{ pid: record.pid, error: String(record.error) }];
      }
    }
    return [];
  });
}

export async function nativeKillTree(
  pid: number,
  timeoutMs: number = KILL_TREE_TIMEOUT_MS,
): Promise<KillTreeResult> {
  const process = await sidecarPool().acquire(PROC_SIDECAR);
  try {
    const raw = (await process.request(
      "kill_tree",
      { pid, timeoutMs },
      REQUEST_TIMEOUT_MS,
    )) as KillTreeWire;
    return { killed: asPidArray(raw?.killed), failed: asFailureArray(raw?.failed) };
  } finally {
    sidecarPool().release(process);
  }
}

export async function nativeListTree(pid: number): Promise<ProcessInfo[]> {
  const process = await sidecarPool().acquire(PROC_SIDECAR);
  try {
    const raw = (await process.request("list_tree", { pid }, REQUEST_TIMEOUT_MS)) as {
      processes?: unknown;
    };
    if (!Array.isArray(raw?.processes)) return [];
    return raw.processes.flatMap((item) => {
      if (item && typeof item === "object" && "pid" in item) {
        const record = item as { pid: unknown; ppid: unknown; name: unknown };
        if (typeof record.pid === "number") {
          return [
            {
              pid: record.pid,
              ppid: typeof record.ppid === "number" ? record.ppid : 0,
              name: typeof record.name === "string" ? record.name : "",
            },
          ];
        }
      }
      return [];
    });
  } finally {
    sidecarPool().release(process);
  }
}

/** true = tree killing has been completed by sidecar; false = the caller should use TS implementation. */
export async function tryNativeTerminateProcessTree(pid: number): Promise<boolean> {
  if (!sidecarEnabled("proc")) return false;
  try {
    await nativeKillTree(pid);
    return true;
  } catch (error) {
    if (error instanceof SidecarUnavailableError) {
      warnOnce("kill_tree", `${error.message}; falling back to the TS terminator`);
      return false;
    }
    throw error;
  }
}

export { PROC_SIDECAR };
