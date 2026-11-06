import { randomUUID } from "node:crypto";
import { cpus, totalmem } from "node:os";
import { ZCODE_MCP_RESOURCE_SAMPLE_INTERVAL_MS, type ZCodeMcpResourceSample } from "@zcode/shared";
import {
  createProcessProbe,
  type ProcessProbe,
  type ProcessProbeSample,
  type ProcessTreeScope,
} from "../device/process-probe.js";

interface TimerHandle {
  unref?(): void;
}
export interface McpResourceTimer {
  clearInterval(handle: TimerHandle): void;
  setInterval(callback: () => void, intervalMs: number): TimerHandle;
}

export interface McpResourceProcess {
  instanceId: string;
  mcpId: string;
  pid: number;
  startedAt: number;
/** Old instances that are restarted or logged out during probe await cannot contribute resources or modify the tracker. */
  isCurrent(): boolean;
  observed(
    samples: readonly ProcessProbeSample[] | undefined,
    sampledAt: number,
    scope: ProcessTreeScope,
  ): void;
}

export interface McpResourceTelemetryOptions {
  arch: ZCodeMcpResourceSample["arch"];
  platform: ZCodeMcpResourceSample["platform"];
  now(): number;
  getProcesses(): McpResourceProcess[];
  onResourceSamples?(samples: ZCodeMcpResourceSample[]): void;
  processProbe?: ProcessProbe;
  logicalCpuCount?: number;
  totalMemoryGb?: number;
  timer?: McpResourceTimer;
}

/** A tracker, a timer and a probe; the CPU baseline only lives until the next sampling and does not save the historical sequence. */
export function createMcpResourceTelemetry(options: McpResourceTelemetryOptions) {
  const probe = options.processProbe ?? createProcessProbe({ platform: options.platform });
  const logicalCpuCount = options.logicalCpuCount ?? Math.max(1, cpus().length);
  const totalMemoryGb = options.totalMemoryGb ?? Math.round(totalmem() / 1024 ** 3);
  const instanceToken = randomUUID();
  const timer = options.timer ?? {
    setInterval: (callback: () => void, intervalMs: number) => setInterval(callback, intervalMs),
    clearInterval: (handle: TimerHandle) => clearInterval(handle as ReturnType<typeof setInterval>),
  };
  let handle: TimerHandle | undefined;
  let generation = 0;
  let inFlight = false;
  let previousAt: number | undefined;
  let previous = new Map<string, Map<number, number>>();

  const sampleNow = async (): Promise<void> => {
    if (inFlight) return;
    const processes = options.getProcesses();
    if (processes.length === 0) {
      previous.clear();
      previousAt = undefined;
      return;
    }
    inFlight = true;
    const sampleGeneration = generation;
    const sampledAt = options.now();
    try {
      probe.reset();
      const trees = await probe.sampleProcessTrees(processes.map((entry) => entry.pid));
      if (sampleGeneration !== generation) return;
      if (!trees) {
        previous.clear();
        previousAt = undefined;
        return;
      }
      const intervalMs =
        previousAt === undefined
          ? ZCODE_MCP_RESOURCE_SAMPLE_INTERVAL_MS
          : Math.max(1, sampledAt - previousAt);
      const next = new Map<string, Map<number, number>>();
      const groups = new Map<string, ZCodeMcpResourceSample>();
      const seenPids = new Set<number>();
      for (const entry of processes) {
        if (!entry.isCurrent()) continue;
        const tree = trees.get(entry.pid);
        entry.observed(tree, sampledAt, probe.treeScope);
        if (!tree?.length) continue;
        const group = groups.get(entry.mcpId) ?? {
          mcpId: entry.mcpId,
          instanceToken,
          sampledAt,
          intervalMs,
          processCount: 0,
          rssKbTotal: 0,
          rssKbMaxProcess: 0,
          cpuTimeMsDelta: 0,
          uptimeMinutes: 0,
          platform: options.platform,
          arch: options.arch,
          logicalCpuCount,
          totalMemoryGb,
        };
        const baseline = new Map<number, number>();
        for (const sample of tree) {
// Shared roots or nested roots are only counted once to prevent the same OS process from polluting the application total.
          if (seenPids.has(sample.pid)) continue;
          seenPids.add(sample.pid);
          group.processCount += 1;
          group.rssKbTotal += sample.rssKb;
          group.rssKbMaxProcess = Math.max(group.rssKbMaxProcess, sample.rssKb);
          if (sample.cpuTimeMs !== undefined) {
            baseline.set(sample.pid, sample.cpuTimeMs);
            const old = previous.get(entry.instanceId)?.get(sample.pid);
            if (old !== undefined) group.cpuTimeMsDelta += Math.max(0, sample.cpuTimeMs - old);
          }
        }
        next.set(entry.instanceId, baseline);
        group.uptimeMinutes = Math.max(
          group.uptimeMinutes,
          Math.floor(Math.max(0, sampledAt - entry.startedAt) / 60_000),
        );
        if (group.processCount > 0) groups.set(entry.mcpId, group);
      }
      previous = next;
      previousAt = sampledAt;
      if (groups.size > 0) options.onResourceSamples?.([...groups.values()]);
    } catch {
// Sampling/notification failures cannot affect the MCP lifecycle, nor can CPU time across the failure window be calculated as a five-minute average.
      previous.clear();
      previousAt = undefined;
    } finally {
      inFlight = false;
    }
  };
  return {
    sampleNow,
    start() {
      if (handle) return;
      try {
        handle = timer.setInterval(() => {
          void sampleNow();
        }, ZCODE_MCP_RESOURCE_SAMPLE_INTERVAL_MS);
        handle.unref?.();
      } catch {
// Maintain the bypass semantics of the original tracker: the MCP connection cannot be blocked if the timer is unavailable.
      }
    },
    stop() {
      generation += 1;
      previous.clear();
      previousAt = undefined;
      const activeHandle = handle;
      handle = undefined;
      try {
        if (activeHandle) timer.clearInterval(activeHandle);
      } catch {
// Cleanup failure does not prevent the Agent from exiting; the above generation has invalidated the in-transit results.
      }
    },
  };
}
