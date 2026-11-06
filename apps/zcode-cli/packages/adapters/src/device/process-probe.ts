import { readdir, readFile } from "node:fs/promises";
import { readDarwinProcessGroup, readDarwinProcessTable } from "./process-probe-darwin.js";
import {
  sampleLinuxProcessGroup,
  sampleLinuxProcessTrees,
  type LinuxProcReaders,
} from "./process-probe-linux.js";
import {
  defaultProbeExecFile,
  groupProcessTrees,
  isSamplablePid,
  toSample,
  PROCESS_PROBE_SAMPLE_TIMEOUT_MS,
  type ProcessProbeExecFile,
  type ProcessProbeSample,
} from "./process-probe-shared.js";
import { readWindowsProcessMemory } from "./process-probe-windows.js";

export {
  PROCESS_PROBE_SAMPLE_TIMEOUT_MS,
  type ProcessProbeCommandResult,
  type ProcessProbeExecFile,
  type ProcessProbeSample,
} from "./process-probe-shared.js";

/** The actual caliber of process tree sampling: Windows tasklist does not have ppid, and can only get directly connected processes. */
export type ProcessTreeScope = "direct_process" | "process_tree";

/**
* Universal process probe: given a batch of pids or a process group id, the RSS and accumulated CPU time of each process will be returned.
* MCP 5-minute sampling and Bash slow command sampling share this module.
 *
* Performance redline constraints for this module:
* - The external process whitelist is only macOS's `ps` and Windows' `tasklist`, and each sampling can be called at most once;
* - Linux always reads `/proc` and does not start any processes;
* - Each sampling timeout is 1 second. Timeout or failure will be regarded as "no sample this time", and there will be no retrying or queuing;
* - After 3 consecutive failures, this instance is deactivated until the caller explicitly `reset()` in the next reporting window.
 *
* The failure budget belongs to the probe instance: each sampling scenario (MCP, Bash command) in a CLI process holds a long-life cycle instance.
* The scenes do not affect each other, and the caller `reset()` is used when switching windows.
 */
export interface ProcessProbe {
  /**
* Collect the entire process tree (including the root itself) according to the root pid, and return root pid → samples of each process in the tree.
* Roots that no longer exist at the time of sampling will not appear in the results; returning `undefined` means there is no sample this time.
   */
  sampleProcessTrees(
    rootPids: readonly number[],
  ): Promise<ReadonlyMap<number, readonly ProcessProbeSample[]> | undefined>;
/** Sampling by process group id; Windows has no process group semantics, so there is always no sample and no process is started. */
  sampleProcessGroup(processGroupId: number): Promise<readonly ProcessProbeSample[] | undefined>;
/** Clear the continuous failure count when the reporting window is switched, so that the deactivated probes can be used again in the new window. */
  reset(): void;
/** The caliber of `sampleProcessTrees` of this platform. The caller marks the sample range accordingly and does not need to judge the platform by himself. */
  readonly treeScope: ProcessTreeScope;
}

interface CreateProcessProbeOptions {
  execFile?: ProcessProbeExecFile;
/** List entries under `/proc`, Linux only */
  listProcDirectory?: () => Promise<readonly string[]>;
/** The reason for each sampling failure (including timeout); the caller receives its own debug log, and the probe itself does not perform I/O */
  onSampleFailed?: (reason: string) => void;
  platform?: NodeJS.Platform;
/** Read `/proc/<pid>/stat` and `/proc/<pid>/status`, Linux only */
  readProcFile?: (path: string) => Promise<string>;
}

const PROCESS_PROBE_MAX_CONSECUTIVE_FAILURES = 3;

const PROBE_TIMED_OUT = Symbol("process-probe-timed-out");

export function createProcessProbe(options: CreateProcessProbeOptions = {}): ProcessProbe {
  const platform = options.platform ?? process.platform;
  const execFile = options.execFile ?? defaultProbeExecFile;
  const listProcDirectory = options.listProcDirectory ?? (() => readdir("/proc"));
  const readProcFile = options.readProcFile ?? ((path: string) => readFile(path, "utf8"));
  let consecutiveFailures = 0;

  const reportFailure = (reason: string): undefined => {
    consecutiveFailures += 1;
    try {
      options.onSampleFailed?.(reason);
    } catch {
// Observation callback throwing errors cannot in turn affect sampling and business.
    }
    return undefined;
  };

/** Unified closure of one sampling: timeouts, exceptions, and command failures will only be displayed as "no sample", and the failure budget will be accumulated. */
  const sampleWithinBudget = async <T>(
    collect: (readers: LinuxProcReaders) => Promise<T>,
  ): Promise<T | undefined> => {
    if (consecutiveFailures >= PROCESS_PROBE_MAX_CONSECUTIVE_FAILURES) return undefined;
    const deadline = Date.now() + PROCESS_PROBE_SAMPLE_TIMEOUT_MS;
    let outcome: T | typeof PROBE_TIMED_OUT;
    try {
      outcome = await raceProbeTimeout(
        collect({
          isExpired: () => Date.now() >= deadline,
          listProcDirectory,
          readProcFile,
        }),
      );
    } catch (error) {
      return reportFailure(error instanceof Error ? error.message : String(error));
    }
    if (outcome === PROBE_TIMED_OUT) {
      return reportFailure(`Sample exceeded ${PROCESS_PROBE_SAMPLE_TIMEOUT_MS} milliseconds`);
    }
    consecutiveFailures = 0;
    return outcome;
  };

  return {
    treeScope: platform === "win32" ? "direct_process" : "process_tree",
    async sampleProcessTrees(rootPids) {
      const roots = [...new Set(rootPids.filter(isSamplablePid))];
      if (roots.length === 0) return new Map();
      if (platform === "win32") {
        const samples = await sampleWithinBudget(() => readWindowsProcessMemory(execFile, roots));
        if (!samples) return undefined;
// There is no ppid in tasklist, and the "process tree" of Windows can only degenerate into the root process itself, see treeScope.
        return new Map(samples.map((sample) => [sample.pid, [sample]]));
      }
      return await sampleWithinBudget(async (readers) =>
        platform === "linux"
          ? await sampleLinuxProcessTrees(readers, roots)
          : groupProcessTrees(await readDarwinProcessTable(execFile), roots),
      );
    },
    async sampleProcessGroup(processGroupId) {
      if (!isSamplablePid(processGroupId)) return undefined;
// Windows does not have process group semantics, nor does it have a zero-cost process tree data source, so this sampling is simply abandoned.
      if (platform === "win32") return undefined;
      return await sampleWithinBudget(async (readers) =>
        platform === "linux"
          ? await sampleLinuxProcessGroup(readers, processGroupId)
          : (await readDarwinProcessGroup(execFile, processGroupId)).map(toSample),
      );
    },
    reset() {
      consecutiveFailures = 0;
    },
  };
}

/**
* 1 second hard timeout: `ps` / `tasklist` is used to kill the process by the timeout of execFile;
* `/proc` reading cannot be canceled and is terminated at the batch boundary by readers.isExpired. Here it is only responsible for the negative return.
 */
async function raceProbeTimeout<T>(work: Promise<T>): Promise<T | typeof PROBE_TIMED_OUT> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      work,
      new Promise<typeof PROBE_TIMED_OUT>((resolve) => {
        timer = setTimeout(() => resolve(PROBE_TIMED_OUT), PROCESS_PROBE_SAMPLE_TIMEOUT_MS);
        timer.unref?.();
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}
