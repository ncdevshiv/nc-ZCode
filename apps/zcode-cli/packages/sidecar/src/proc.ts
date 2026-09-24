//! proc facade：进程树枚举/强杀（替代 TS `taskkill /T /F` 与抓取式进程列表）。
//!
//! 语义对齐 TS `adapters/src/mcp/process-tree.ts`：根 PID 已死/不存在时
//! `kill_tree` 是无操作成功（空结果不报错）——这是 sidecar 契约（见
//! native/crates/zcode-proc/README）。flag 关 = TS 原实现；flag 开 = sidecar；
//! sidecar 不可用 → 告警一次并退回 TS 实现（不改变 best-effort 回收语义）。
//!
//! 注意：本 facade 只覆盖**已有 PID 的树**；`spawn_in_job` 需要 Windows Job
//! 对象生命周期管理，TS 侧 `windows-job-object.ts` 仍拥有该面（README 差异表）。

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

/** true = 已由 sidecar 完成杀树；false = 调用方应走 TS 实现。 */
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
