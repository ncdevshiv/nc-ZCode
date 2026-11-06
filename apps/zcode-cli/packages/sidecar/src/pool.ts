//! Pool: reuse the process according to the sidecar name, recycle it when idle, and rebuild it on demand after a crash.
//!
//! Life cycle rules:
//! - `acquire()` returns the available process (spawn if it does not exist); the caller is finished `release()`;
//! - Crash (exit) is immediately evicted from the pool, and the next acquire will be rebuilt naturally - the storm will not be automatically restarted;
//! - Processes that are idle for more than IDLE_REAP_MS are recycled (unref timer, do not drag event loop);
//! - `shutdownAll()` is used to finish the process before exiting.

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
// First atomically evict from the free list and then shut down the process: it is impossible for the ongoing acquire to get the process being recycled
// (Otherwise closure competes with write and the caller sees write EPIPE).
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

/// Process-level shared pool: facade (diff.ts) and host exit end share the same instance.
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
