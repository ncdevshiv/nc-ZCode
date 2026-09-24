//! Pool：按 sidecar 名复用进程，空闲回收，崩溃后按需重建。
//!
//! 生命周期规则：
//!   - `acquire()` 返回可用进程（不存在则 spawn）；调用方用完 `release()`；
//!   - 崩溃（exit）即刻从池中逐出，下次 acquire 自然重建——不自动重启风暴；
//!   - 空闲超过 IDLE_REAP_MS 的进程被回收（unref 定时器，不拖事件循环）；
//!   - `shutdownAll()` 供进程退出前收尾。

import { SidecarProcess } from "./process.js";
import { resolveSidecarBin } from "./config.js";
import { SidecarUnavailableError } from "./types.js";

const IDLE_REAP_MS = 60_000;

class PooledProcess {
  constructor(
    readonly process: SidecarProcess,
    private idleTimer: NodeJS.Timeout,
    private readonly onReap: () => void,
  ) {}

  touch(): void {
    clearTimeout(this.idleTimer);
    this.idleTimer = setTimeout(() => {
      // 先从空闲表原子逐出再关进程：进行中的 acquire 不可能拿到正在回收的进程
      // （否则关闭与写出竞争，调用方看到 write EPIPE）。
      this.onReap();
      void this.process.shutdown();
    }, IDLE_REAP_MS);
    this.idleTimer.unref();
  }

  dispose(): void {
    clearTimeout(this.idleTimer);
  }
}

export class SidecarPool {
  private readonly entries = new Map<string, PooledProcess[]>();
  private readonly starting = new Map<string, Promise<SidecarProcess>>();

  async acquire(name: string): Promise<SidecarProcess> {
    const idle = this.entries.get(name);
    if (idle && idle.length > 0) {
      const entry = idle.pop();
      if (entry) {
        if (entry.process.isAlive()) {
          entry.touch();
          return entry.process;
        }
        entry.dispose();
      }
    }
    const inFlight = this.starting.get(name);
    if (inFlight) return inFlight;
    const starting = this.spawn(name).finally(() => {
      this.starting.delete(name);
    });
    this.starting.set(name, starting);
    return starting;
  }

  release(process: SidecarProcess): void {
    if (!process.isAlive()) return;
    const list = this.entries.get(process.name) ?? [];
    let entry: PooledProcess | null = null;
    const created = new PooledProcess(
      process,
      setTimeout(() => {}, IDLE_REAP_MS),
      () => {
        const index = list.indexOf(entry as PooledProcess);
        if (index >= 0) list.splice(index, 1);
      },
    );
    entry = created;
    list.push(created);
    this.entries.set(process.name, list);
    created.touch();
  }

  async shutdownAll(): Promise<void> {
    for (const list of this.entries.values()) {
      for (const entry of list) {
        entry.dispose();
        await entry.process.shutdown();
      }
    }
    this.entries.clear();
  }

  private async spawn(name: string): Promise<SidecarProcess> {
    const binPath = resolveSidecarBin(name);
    if (!binPath) {
      throw new SidecarUnavailableError(
        `sidecar binary not found for ${name} (set ${"ZCODE_SIDECAR_DIR"} or build native/target)`,
      );
    }
    return SidecarProcess.start(binPath, name);
  }
}

/// 进程级共享 pool：facade（diff.ts）与宿主退出收尾共用同一个实例。
let sharedPool: SidecarPool | null = null;

export function sidecarPool(): SidecarPool {
  if (!sharedPool) sharedPool = new SidecarPool();
  return sharedPool;
}

export async function shutdownSidecars(): Promise<void> {
  if (!sharedPool) return;
  await sharedPool.shutdownAll();
  sharedPool = null;
}
