//! watch facade：目录监视（替代 TS `services/src/fileWatcher/fileWatcherService.ts`
//! 的 `node:fs.watch` 路径）。
//!
//! 语义：sidecar 发 `fs_event {watchId, path, kind}`（kind ∈ create/change/
//! delete/rename/other），本 facade 按 `debounceMs` 窗口聚合成 TS 的
//! `FileWatchEvent {dirPath, changedPath?}`——「窗口内只有一条明确路径才透传
//! changedPath，否则只报 dirPath」与 TS 实现逐字一致（TS 注释里记的正是这个
//! 语义，README 差异表同步）。
//!
//! 进程模型：watcher 是**有状态长驻** sidecar（watch/unwatch/list），不能走
//! 请求-归还的池（归还会关进程）。本模块自持一个专用进程；sidecar 不可用时
//! `createNativeFileWatcher` 返回 null，调用方（TS service）继续用 `node:fs.watch`。

import { sidecarEnabled } from "./config.js";
import { SidecarProcess } from "./process.js";
import { resolveSidecarBin } from "./config.js";
import { SidecarUnavailableError, type SidecarEventListener } from "./types.js";

const WATCH_SIDECAR = "zcode-watch";
const DEBOUNCE_MS = 150;

export type FileWatchEvent = { dirPath: string; changedPath?: string };

export interface NativeWatchHandle {
  /** TS service 的 watch id（调用方生成的稳定 id）。 */
  id: string;
  path: string;
  unwatch(): Promise<void>;
  /** 事件订阅（每 watch 一个）。 */
  onEvent(listener: (event: FileWatchEvent) => void): void;
  dispose(): void;
}

let watchProcess: SidecarProcess | null = null;
let starting: Promise<SidecarProcess | null> | null = null;
const registries = new Map<string, { handle: NativeWatchHandle; sidecarId: string }>();
let eventPump: SidecarEventListener | null = null;

async function ensureProcess(): Promise<SidecarProcess | null> {
  if (watchProcess?.isAlive()) return watchProcess;
  if (starting) return starting;
  starting = (async () => {
    try {
      const bin = resolveSidecarBin(WATCH_SIDECAR);
      if (!bin) throw new SidecarUnavailableError(`${WATCH_SIDECAR} binary not found`);
      const process = await SidecarProcess.start(bin, WATCH_SIDECAR);
      watchProcess = process;
      if (!eventPump) {
        eventPump = (event, data) => {
          if (event !== "fs_event") return;
          if (!data || typeof data !== "object") return;
          const record = data as { watchId?: unknown; path?: unknown };
          if (typeof record.watchId !== "string" || typeof record.path !== "string") return;
          deliver(record.watchId, record.path);
        };
        process.onEvent(eventPump);
      }
      return process;
    } catch (error) {
      if (!(error instanceof SidecarUnavailableError)) throw error;
      process.stderr.write(`[sidecar:${WATCH_SIDECAR}] ${error.message}\n`);
      return null;
    } finally {
      starting = null;
    }
  })();
  return starting;
}

interface DebounceState {
  timer: ReturnType<typeof setTimeout> | null;
  pending: Set<string>;
  hasUnknown: boolean;
}

const debounces = new Map<string, DebounceState>();
const listenersByWatchId = new Map<string, Set<(event: FileWatchEvent) => void>>();

function deliver(sidecarWatchId: string, changedPath: string): void {
  const entry = Array.from(registries.entries()).find(
    ([, value]) => value.sidecarId === sidecarWatchId,
  );
  if (!entry) return;
  const [watchId, registry] = entry;
  const handle = registry.handle;
  const state = debounces.get(watchId) ?? { timer: null, pending: new Set(), hasUnknown: false };
  debounces.set(watchId, state);
  if (changedPath) state.pending.add(changedPath);
  else state.hasUnknown = true;
  if (state.timer) clearTimeout(state.timer);
  state.timer = setTimeout(() => {
    state.timer = null;
    const onlyChangedPath =
      !state.hasUnknown && state.pending.size === 1 ? state.pending.values().next().value : undefined;
    state.pending.clear();
    state.hasUnknown = false;
    const event: FileWatchEvent = {
      dirPath: handle.path,
      ...(onlyChangedPath ? { changedPath: onlyChangedPath } : {}),
    };
    for (const listener of listenersByWatchId.get(watchId) ?? []) listener(event);
  }, DEBOUNCE_MS);
}

/**
 * 创建原生 watcher；sidecar 不可用时返回 null（调用方用 TS 实现）。
 * `id` 由调用方提供（TS service 的 watch id），便于事件回接既有订阅链。
 */
export async function createNativeFileWatcher(
  id: string,
  path: string,
  recursive = false,
): Promise<NativeWatchHandle | null> {
  if (!sidecarEnabled("watch")) return null;
  const sidecar = await ensureProcess();
  if (!sidecar) return null;
  const result = (await sidecar.request("watch", { paths: [path], recursive }, 10_000)) as {
    watchId?: string;
  };
  if (!result?.watchId) return null;
  const sidecarId = result.watchId;
  const handle: NativeWatchHandle = {
    id,
    path,
    onEvent(listener) {
      const set = listenersByWatchId.get(id) ?? new Set();
      set.add(listener);
      listenersByWatchId.set(id, set);
    },
    async unwatch() {
      await sidecar.request("unwatch", { watchId: sidecarId }, 5_000).catch(() => undefined);
      registries.delete(id);
      const state = debounces.get(id);
      if (state?.timer) clearTimeout(state.timer);
      debounces.delete(id);
      listenersByWatchId.delete(id);
    },
    dispose() {
      void handle.unwatch();
    },
  };
  registries.set(id, { handle, sidecarId });
  return handle;
}

/** 宿主退出前收尾（关闭长驻进程）。 */
export async function shutdownNativeWatchers(): Promise<void> {
  for (const [, value] of registries) {
    await value.handle.unwatch();
  }
  registries.clear();
  if (watchProcess?.isAlive()) await watchProcess.shutdown();
  watchProcess = null;
}

export function watchEnabled(): boolean {
  return sidecarEnabled("watch");
}

export { WATCH_SIDECAR };
