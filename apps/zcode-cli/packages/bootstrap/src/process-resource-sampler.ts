import { randomBytes } from "node:crypto";
import { availableParallelism, totalmem } from "node:os";
import {
  ZCODE_CLI_RESOURCE_SAMPLE_INTERVAL_MS,
  type ZCodeProcessResourceSample,
} from "@zcode/shared";

/** The sampling period and the app-side aggregation share the same shared constant to prevent the beats on both sides from drifting. */
const ZCODE_PROCESS_RESOURCE_SAMPLE_INTERVAL_MS = ZCODE_CLI_RESOURCE_SAMPLE_INTERVAL_MS;

let processInstanceToken: string | undefined;

/**
* The instance ID of this CLI process: generated once when sampling for the first time, and does not change during the entire process life cycle thereafter (it does not change when the sampler is rebuilt).
 *
* Only for main statistics on the app side: "How many CLI processes are alive at the same time" and "Maximum single process RSS".
 *
* Does not contain pid and does not exit the local machine; using a random token instead of pid is a privacy red line requirement.
 */
function resolveProcessInstanceToken(): string {
  processInstanceToken ??= randomBytes(8).toString("hex");
  return processInstanceToken;
}

interface CpuUsageSnapshot {
  user: number;
  system: number;
}

/** Identical to Node `process.memoryUsage()`; local memory diagnostic logs require heap subdivision, and protocol samples only take rss. */
interface ProcessMemoryUsageSnapshot {
  rss: number;
  heapTotal: number;
  heapUsed: number;
  external: number;
  arrayBuffers: number;
}

interface ResourceSamplerTimerHandle {
  unref?(): void;
}

interface ResourceSamplerTimer {
  setInterval(callback: () => void, intervalMs: number): ResourceSamplerTimerHandle;
  clearInterval(handle: ResourceSamplerTimerHandle): void;
}

interface CreateZCodeProcessResourceSamplerOptions {
  /**
* The second parameter is the complete memory snapshot of this cycle, which is used for local diagnostic logs in the process;
* The protocol sample itself only has two memory fields: rss and heapUsed.
   */
  onSample(sample: ZCodeProcessResourceSample, memoryUsage: ProcessMemoryUsageSnapshot): void;
  platform?: ZCodeProcessResourceSample["platform"];
  arch?: ZCodeProcessResourceSample["arch"];
  logicalCpuCount?: number;
  readCpuUsage?: () => CpuUsageSnapshot;
  readMonotonicTimeNs?: () => bigint;
  readMemoryUsage?: () => ProcessMemoryUsageSnapshot;
/** The physical memory of the running machine, read once during construction (will not change within the same process). */
  readTotalMemoryBytes?: () => number;
/** The running time of this process; the platform side divides the buckets according to the running time and finds that the memory increases over time. */
  readUptimeSeconds?: () => number;
/** Only for single test to inject predictable instance identifier; production process-level random token. */
  instanceToken?: string;
  timer?: ResourceSamplerTimer;
}

export interface ZCodeProcessResourceSampler {
  start(): void;
  stop(): void;
}

interface ResourceSamplerBaseline {
  cpu: CpuUsageSnapshot;
  monotonicTimeNs: bigint;
}

const defaultTimer: ResourceSamplerTimer = {
  setInterval(callback, intervalMs) {
    return setInterval(callback, intervalMs);
  },
  clearInterval(handle) {
    clearInterval(handle as ReturnType<typeof setInterval>);
  },
};

function roundResourceMetric(value: number): number {
  return Math.round(value * 10_000) / 10_000;
}

/**
* When the reading is unavailable, this field is directly absent (both items in the protocol are optional), and no 0 is used.
* The reading itself is thrown by the outer sampling try/catch pocket (red line 6 "throw away on failure").
 */
function toRoundedUnit(value: number, divisor: number): number | undefined {
  return Number.isFinite(value) && value >= 0 ? Math.round(value / divisor) : undefined;
}

export function createZCodeProcessResourceSampler(
  options: CreateZCodeProcessResourceSamplerOptions,
): ZCodeProcessResourceSampler {
  const platform = options.platform ?? (process.platform as ZCodeProcessResourceSample["platform"]);
  const arch = options.arch ?? (process.arch as ZCodeProcessResourceSample["arch"]);
  const logicalCpuCount = Math.max(
    1,
    Math.min(4_096, Math.trunc(options.logicalCpuCount ?? availableParallelism())),
  );
  const readCpuUsage = options.readCpuUsage ?? (() => process.cpuUsage());
  const readMonotonicTimeNs = options.readMonotonicTimeNs ?? (() => process.hrtime.bigint());
// One memoryUsage() gets rss and heap breakdown at the same time; separate memoryUsage.rss() on Linux
// Also read /proc, merged into one call without increasing cost.
  const readMemoryUsage = options.readMemoryUsage ?? (() => process.memoryUsage());
  const readUptimeSeconds = options.readUptimeSeconds ?? (() => process.uptime());
  const instanceToken = options.instanceToken ?? resolveProcessInstanceToken();
// Read once when running the physical memory structure: the remote CLI sample needs to use it to overwrite the desktop's total_memory_gb.
  const totalMemoryGb = toRoundedUnit(
    (options.readTotalMemoryBytes ?? (() => totalmem()))(),
    1024 ** 3,
  );
  const timer = options.timer ?? defaultTimer;
  let baseline: ResourceSamplerBaseline | undefined;
  let timerHandle: ResourceSamplerTimerHandle | undefined;

  const readBaseline = (): ResourceSamplerBaseline => ({
    cpu: readCpuUsage(),
    monotonicTimeNs: readMonotonicTimeNs(),
  });

  const sample = (): void => {
    try {
      const nextBaseline = readBaseline();
      if (!baseline) {
        baseline = nextBaseline;
        return;
      }
      const elapsedNs = nextBaseline.monotonicTimeNs - baseline.monotonicTimeNs;
      const cpuDeltaUs =
        nextBaseline.cpu.user - baseline.cpu.user + (nextBaseline.cpu.system - baseline.cpu.system);
      if (elapsedNs <= 0n || cpuDeltaUs < 0) {
        baseline = nextBaseline;
        return;
      }
      const intervalMs = Math.round(Number(elapsedNs) / 1_000_000);
      if (intervalMs <= 0) {
        baseline = nextBaseline;
        return;
      }
      const cpuCores = cpuDeltaUs / (Number(elapsedNs) / 1_000);
      const memoryUsage = readMemoryUsage();
      const rssKb = memoryUsage.rss / 1_024;
      const uptimeMinutes = toRoundedUnit(readUptimeSeconds(), 60);
      baseline = nextBaseline;
      const resourceSample: ZCodeProcessResourceSample = {
        platform,
        arch,
        logicalCpuCount,
        intervalMs,
        cpuCores: roundResourceMetric(cpuCores),
        cpuPercent: roundResourceMetric((cpuCores / logicalCpuCount) * 100),
        rssKb: roundResourceMetric(rssKb),
        heapUsedKb: roundResourceMetric(memoryUsage.heapUsed / 1_024),
        instanceToken,
        ...(uptimeMinutes === undefined ? {} : { uptimeMinutes }),
        ...(totalMemoryGb === undefined ? {} : { totalMemoryGb }),
      };
      try {
        options.onSample(resourceSample, memoryUsage);
      } catch {
// When the reporting end is closed or under back pressure, only the current sample is lost, and the exception cannot be brought back to the Agent main loop.
      }
    } catch {
// Process indicator API exceptions only skip the current cycle and retain the latest successful baseline for subsequent recovery.
    }
  };

  return {
    start() {
      if (timerHandle) {
        return;
      }
      try {
        baseline = readBaseline();
      } catch {
        baseline = undefined;
      }
      try {
        timerHandle = timer.setInterval(sample, ZCODE_PROCESS_RESOURCE_SAMPLE_INTERVAL_MS);
      } catch {
        timerHandle = undefined;
        return;
      }
      try {
        timerHandle.unref?.();
      } catch {
// Retain the timer owner when unref is unavailable, ensuring that stop can recycle the timer.
      }
    },
    stop() {
      if (!timerHandle) {
        return;
      }
      const handle = timerHandle;
      timerHandle = undefined;
      baseline = undefined;
      try {
        timer.clearInterval(handle);
      } catch {
// Sampler cleanup failure cannot block the CLI's existing exit process.
      }
    },
  };
}
