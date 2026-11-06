import {
  isSamplablePid,
  runProbeCommand,
  type ProcessProbeExecFile,
  type ProcessRow,
} from "./process-probe-shared.js";

/** macOS only allows `ps`; the output format and column name are declared in pairs to avoid silent misalignment caused by writing one copy in each place. */
const PS_COMMAND = "ps";
const PS_PROCESS_TABLE_FORMAT = {
  columns: ["pid", "parentPid", "rssKb", "cpuTime"],
  spec: "pid=,ppid=,rss=,cputime=",
} as const;
const PS_PROCESS_GROUP_FORMAT = {
  columns: ["pid", "rssKb", "cpuTime"],
  spec: "pid=,rss=,cputime=",
} as const;
const PS_ALL_PROCESSES_FLAG = "-eo";
const PS_FORMAT_FLAG = "-o";
const PS_PROCESS_GROUP_FLAG = "-g";
const SECONDS_PER_MINUTE = 60;
const SECONDS_PER_DAY = 86_400;
const MS_PER_SECOND = 1_000;
/** BSD `cputime` is at most `dd-hh:mm:ss`, that is, the colon segment does not exceed 3 segments */
const MAX_BSD_CLOCK_SEGMENTS = 3;

/** macOS: The only external process allowed is `ps`, with a maximum of one call per sample. */
export async function readDarwinProcessTable(
  execFile: ProcessProbeExecFile,
): Promise<readonly ProcessRow[]> {
// The process tree requires ppid, and `-p <pid list>` will only return the processes in the list, but not the descendants, so the entire table will be fetched once.
  const stdout = await runProbeCommand(execFile, PS_COMMAND, [
    PS_ALL_PROCESSES_FLAG,
    PS_PROCESS_TABLE_FORMAT.spec,
  ]);
  return parseDarwinRows(stdout, PS_PROCESS_TABLE_FORMAT.columns);
}

export async function readDarwinProcessGroup(
  execFile: ProcessProbeExecFile,
  processGroupId: number,
): Promise<readonly ProcessRow[]> {
  const stdout = await runProbeCommand(execFile, PS_COMMAND, [
    PS_FORMAT_FLAG,
    PS_PROCESS_GROUP_FORMAT.spec,
    PS_PROCESS_GROUP_FLAG,
    String(processGroupId),
  ]);
  return parseDarwinRows(stdout, PS_PROCESS_GROUP_FORMAT.columns);
}

type DarwinColumn = "pid" | "parentPid" | "rssKb" | "cpuTime";

function parseDarwinRows(stdout: string, columns: readonly DarwinColumn[]): readonly ProcessRow[] {
  const rows: ProcessRow[] = [];
  for (const line of stdout.split(/\r?\n/)) {
    const fields = line.trim().split(/\s+/);
    if (fields.length !== columns.length) continue;
    const values = new Map(columns.map((column, index) => [column, fields[index] ?? ""]));
    const pid = Number(values.get("pid"));
    const rssKb = Number(values.get("rssKb"));
    const parentPidText = values.get("parentPid");
    const parentPid = parentPidText === undefined ? undefined : Number(parentPidText);
    const cpuTimeMs = parseBsdCpuTimeMs(values.get("cpuTime") ?? "");
    if (!isSamplablePid(pid) || !Number.isFinite(rssKb) || rssKb < 0) continue;
    if (parentPid !== undefined && (!Number.isInteger(parentPid) || parentPid < 0)) continue;
    rows.push({
      pid,
      rssKb,
      ...(parentPid === undefined ? {} : { parentPid }),
      ...(cpuTimeMs === undefined ? {} : { cpuTimeMs }),
    });
  }
  return rows;
}

/** BSD `cputime` has the form `[dd-][hh:]mm:ss[.ff]`. */
function parseBsdCpuTimeMs(raw: string): number | undefined {
  const trimmed = raw.trim();
  if (!trimmed) return undefined;
  const dashIndex = trimmed.indexOf("-");
  const days = dashIndex === -1 ? 0 : Number(trimmed.slice(0, dashIndex));
  const clockParts = trimmed.slice(dashIndex + 1).split(":");
  if (!Number.isFinite(days) || days < 0 || clockParts.length > MAX_BSD_CLOCK_SEGMENTS) {
    return undefined;
  }
  let seconds = days * SECONDS_PER_DAY;
  for (const [index, part] of [...clockParts].reverse().entries()) {
    const value = Number(part);
    if (!Number.isFinite(value) || value < 0) return undefined;
    seconds += value * SECONDS_PER_MINUTE ** index;
  }
  return Math.round(seconds * MS_PER_SECOND);
}
