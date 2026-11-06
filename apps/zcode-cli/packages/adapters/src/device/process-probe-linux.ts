import {
  collectProcessTreePids,
  isSamplablePid,
  ProcessProbeFailure,
  toSample,
  type ProcessProbeSample,
  type ProcessRelation,
} from "./process-probe-shared.js";

/** Linux `USER_HZ`: utime/stime of `/proc/<pid>/stat` is in this unit, 100 on all supported platforms */
const LINUX_CLOCK_TICKS_PER_SECOND = 100;
const MS_PER_SECOND = 1_000;
/**
* `/proc` scan is read in batches: the remaining batches will not be initiated after a timeout.
* One-time `Promise.all` hundreds of readFiles cannot be canceled, and the slow disk will continue to consume IO after the samples have been discarded.
 */
const PROC_READ_BATCH_SIZE = 64;

export interface LinuxProcReaders {
  listProcDirectory: () => Promise<readonly string[]>;
  readProcFile: (path: string) => Promise<string>;
/** Whether this sampling has timed out; if true, the remaining `/proc` reading will be stopped and judged as no sample */
  isExpired?: () => boolean;
}

/**
* Linux walks `/proc` twice: read all `stat` first to get ppid, pgid and CPU time,
* Then read the `VmRSS` of `status` only for the pid in the target process tree. No process is started during the entire process.
 */
export async function sampleLinuxProcessTrees(
  readers: LinuxProcReaders,
  rootPids: readonly number[],
): Promise<ReadonlyMap<number, readonly ProcessProbeSample[]>> {
  const relations = await readLinuxProcessRelations(readers);
  const relationByPid = new Map(relations.map((relation) => [relation.pid, relation]));
  const treePidsByRoot = collectProcessTreePids(relations, rootPids);
  const rssByPid = await readLinuxRssKb(readers, [...treePidsByRoot.values()].flat());
  const trees = new Map<number, readonly ProcessProbeSample[]>();
  for (const [rootPid, treePids] of treePidsByRoot) {
    trees.set(rootPid, buildLinuxSamples(treePids, relationByPid, rssByPid));
  }
  return trees;
}

export async function sampleLinuxProcessGroup(
  readers: LinuxProcReaders,
  processGroupId: number,
): Promise<readonly ProcessProbeSample[]> {
  const relations = await readLinuxProcessRelations(readers);
  const members = relations.filter((relation) => relation.processGroupId === processGroupId);
  const rssByPid = await readLinuxRssKb(
    readers,
    members.map((relation) => relation.pid),
  );
  return buildLinuxSamples(
    members.map((relation) => relation.pid),
    new Map(members.map((relation) => [relation.pid, relation])),
    rssByPid,
  );
}

function buildLinuxSamples(
  pids: readonly number[],
  relationByPid: ReadonlyMap<number, ProcessRelation>,
  rssByPid: ReadonlyMap<number, number>,
): readonly ProcessProbeSample[] {
  return pids.flatMap((pid) => {
    const relation = relationByPid.get(pid);
    const rssKb = rssByPid.get(pid);
// When the process exits between reads, the status has disappeared, the pid is skipped, and the remaining samples are returned as usual.
    if (!relation || rssKb === undefined) return [];
    return [toSample({ ...relation, rssKb })];
  });
}

async function readLinuxProcessRelations(
  readers: LinuxProcReaders,
): Promise<readonly ProcessRelation[]> {
  let entries: readonly string[];
  try {
    entries = await readers.listProcDirectory();
  } catch (error) {
    throw new ProcessProbeFailure(`/proc is not readable: ${String(error)}`);
  }
  const pids = entries.map(Number).filter(isSamplablePid);
  const relations = await readProcInBatches(readers, pids, async (pid) => {
    let stat: string;
    try {
      stat = await readers.readProcFile(`/proc/${pid}/stat`);
    } catch {
// If a single pid fails to be read (mostly because the process has just exited), it will only be skipped and will not affect the overall sampling.
      return undefined;
    }
    const parsed = parseLinuxStat(stat);
    return parsed ? { pid, ...parsed } : undefined;
  });
  return relations;
}

async function readLinuxRssKb(
  readers: LinuxProcReaders,
  pids: readonly number[],
): Promise<ReadonlyMap<number, number>> {
  const entries = await readProcInBatches(readers, [...new Set(pids)], async (pid) => {
    try {
      const rssKb = parseLinuxVmRssKb(await readers.readProcFile(`/proc/${pid}/status`));
      return rssKb === undefined ? undefined : ([pid, rssKb] as const);
    } catch {
      return undefined;
    }
  });
  return new Map(entries);
}

/** Read `/proc` concurrently in batches, and check the timeout before each batch; if the timeout occurs, the scan will be aborted and it will be judged that there is no sample this time. */
async function readProcInBatches<T>(
  readers: LinuxProcReaders,
  pids: readonly number[],
  read: (pid: number) => Promise<T | undefined>,
): Promise<readonly T[]> {
  const collected: T[] = [];
  for (let offset = 0; offset < pids.length; offset += PROC_READ_BATCH_SIZE) {
    if (readers.isExpired?.()) throw new ProcessProbeFailure("/proc scan timeout");
    const batch = await Promise.all(pids.slice(offset, offset + PROC_READ_BATCH_SIZE).map(read));
    for (const item of batch) {
      if (item !== undefined) collected.push(item);
    }
  }
  return collected;
}

/** The comm field allows spaces and parentheses, and must be separated by the last `)`. */
function parseLinuxStat(
  stat: string,
): { cpuTimeMs: number; parentPid: number; processGroupId: number } | undefined {
  const commEnd = stat.lastIndexOf(")");
  if (commEnd === -1) return undefined;
  const fields = stat
    .slice(commEnd + 1)
    .trim()
    .split(/\s+/);
// After segmentation, fields[0] is state (the third field of stat), so ppid=1, pgrp=2, utime=11, stime=12.
  const parentPid = Number(fields[1]);
  const processGroupId = Number(fields[2]);
  const utimeTicks = Number(fields[11]);
  const stimeTicks = Number(fields[12]);
  if (!Number.isInteger(parentPid) || parentPid < 0 || !Number.isInteger(processGroupId)) {
    return undefined;
  }
  if (!Number.isFinite(utimeTicks) || !Number.isFinite(stimeTicks)) return undefined;
  return {
    cpuTimeMs: Math.round(
      ((utimeTicks + stimeTicks) / LINUX_CLOCK_TICKS_PER_SECOND) * MS_PER_SECOND,
    ),
    parentPid,
    processGroupId,
  };
}

function parseLinuxVmRssKb(status: string): number | undefined {
  const match = /^VmRSS:\s+(\d+)\s*kB$/mu.exec(status);
  if (!match) return undefined;
  const rssKb = Number(match[1]);
  return Number.isFinite(rssKb) ? rssKb : undefined;
}
