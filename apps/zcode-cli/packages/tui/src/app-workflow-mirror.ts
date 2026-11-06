// ============================================================
// workflowRuns image on TUI side
// ============================================================
// Single clock: The running state is **only** maintained by the shared reducer event-by-event reduction (@zcode/shared's
// workflow-runs-reducer, same implementation as v4 projection), plus one-time reseeding during cold start/recovery.
// No polling, no setInterval - the legacy workflow panel's full re-pulling every second is a negative example.
//
// The image has two more things than the protocol status key, both of which are intentional:
// 1. `logTailByRunId`: log events are not in workflowRuns schema (they only enter the journal event log),
// But the expanded details need to display the tail of the log, so TUI keeps the most recent ones (bounded).
// 2. `seedByRunId`: **Display meta information** (label / updatedAt) brought back from listDynamicWorkflowRuns by cold reseeding.
// Running status (status, steps, usage, resumable) **not** from the summary: cold start / journal events during `/resume`
// After `replayWorkflowRuns` is played back into the same reducer (cold playback), the run in the image is the same as before restarting byte by byte; `resumable` is the status bit, which is controlled by the CLI
// Run-settled payload decision - the existing decision "render server boolean" of dynamic-workflow-run.port.ts remains the same.

import {
  reduceWorkflowRunsState,
  type WorkflowRunActor,
  type WorkflowRunUsage,
  type WorkflowRunProgressEnvelope,
  type WorkflowRunState,
  type WorkflowRunsState,
} from "@zcode/shared/zcode-protocol-v4";

/** The limit for the end of the log: both the number of entries and the length of a single entry are limited to prevent a talkative run from eating the image into an unbounded state. */
const TUI_WORKFLOW_LOG_TAIL_LIMITS = {
  maxEntries: 10,
  maxEntryLength: 200,
} as const;

/**
* The **display** field that TUI will render in the cold replanting summary (the running state will always be played back, see the file header).
 *
* `label` / `updatedAt` are additive optional (see spec's `/dwf` boundary line "Implementation revision"):
* It is undefined when the server has not brought it. The rendering side will always return runId and never create a false label here.
 */
export type TuiWorkflowRunSeed = {
  runId: string;
  label?: string;
  updatedAt?: number;
};

export type TuiWorkflowMirror = {
/** Share the authoritative running state maintained by the reducer. */
  state: WorkflowRunsState;
  logTailByRunId: Readonly<Record<string, readonly string[]>>;
  seedByRunId: Readonly<Record<string, TuiWorkflowRunSeed>>;
};

export const EMPTY_TUI_WORKFLOW_MIRROR: TuiWorkflowMirror = {
  state: { revision: 0, runs: [] },
  logTailByRunId: {},
  seedByRunId: {},
};

/**
* A dwf progress event → new image.
 *
* **Return the same reference passed in when there is no change**, so React's setState will directly skip re-rendering:
* The "null = no semantic change" contract of the shared reducer is implemented here as "no redrawing", and no additional equality judgment is required.
 */
export function applyWorkflowProgressToMirror(
  mirror: TuiWorkflowMirror,
  envelope: WorkflowRunProgressEnvelope,
): TuiWorkflowMirror {
  const state = reduceWorkflowRunsState(mirror.state, envelope);
  const logTailByRunId = appendWorkflowLogTail(mirror.logTailByRunId, envelope);
  if (state === null && logTailByRunId === mirror.logTailByRunId) return mirror;
  return {
    ...mirror,
    ...(state === null ? {} : { state }),
    logTailByRunId,
  };
}

/**
* Cold reseeding: Merge the **display name** in the session-level summary of `listDynamicWorkflowRuns` into the image.
 *
* Only fill in meta information, **do not** forge running state entries - running state is controlled by cold replay (`replayWorkflowRuns` → shared reducer)
* Given, it is consistent byte by byte before restarting; nothing that is not included in the summary (except label) is created here.
 */
export function seedWorkflowMirror(
  mirror: TuiWorkflowMirror,
  seeds: readonly TuiWorkflowRunSeed[],
): TuiWorkflowMirror {
  if (seeds.length === 0) return mirror;
  const seedByRunId = { ...mirror.seedByRunId };
  let changed = false;
  for (const seed of seeds) {
    if (!seed.runId) continue;
    const existing = seedByRunId[seed.runId];
    if (existing && sameSeed(existing, seed)) continue;
    seedByRunId[seed.runId] = seed;
    changed = true;
  }
  return changed ? { ...mirror, seedByRunId } : mirror;
}

/**
* Step progress: **Settled / Scheduled** (settled / observed).
 *
* Dynamic workflow does not have a static total, so the denominator is the number of scheduled nodes, which never pretends to be a full percentage. The source of the rules is
* GUI's `packages/ui/src/v4/workflowRunCardJoin.ts` (the only desktop implementation of the same rule);
* TUI cannot import packages/ui (that is the React layer of Electron renderer), so it is rewritten according to the same rules here.
 */
export function workflowRunStepCounts(run: WorkflowRunState): {
  nodesSettled: number;
  nodesTotal: number;
} {
  let nodesSettled = 0;
  for (const node of run.nodes) {
    if (node.phase === "settled") nodesSettled += 1;
  }
  return { nodesSettled, nodesTotal: run.nodes.length };
}

/** All the facts needed for card rendering - make the view a pure function of props (TUI tests call components functionally). */
export type TuiWorkflowCard = {
  runId: string;
  status: WorkflowRunState["status"];
/** Reason for `stopped`; reducer moved from run-settled load. */
  stopReason?: WorkflowRunState["stopReason"];
  nodesSettled: number;
  nodesTotal: number;
  label?: string;
  resumable?: boolean;
  usage?: WorkflowRunUsage;
  actors: readonly WorkflowRunActor[];
  logTail: readonly string[];
  error?: string;
  resultPreview?: string;
  truncated?: boolean;
};

/**
* For the connection of tool card → workflow run, press `toolCallId` (in the schema comment, it is the "association key of tool card → details page").
* Same as GUI `buildWorkflowRunByToolCallId`. There is only one source of running state - mirror state (live events and
* Cold replay via the same reducer), reseeding only gives the display name.
 */
export function buildTuiWorkflowCardIndex(
  mirror: TuiWorkflowMirror,
): ReadonlyMap<string, TuiWorkflowCard> {
  const byToolCallId = new Map<string, TuiWorkflowCard>();
  for (const run of mirror.state.runs) {
// A run without toolCallId has no connectable card and will not be entered into the table.
    if (!run.toolCallId) continue;
    byToolCallId.set(run.toolCallId, cardFromRun(run, mirror));
  }
  return byToolCallId;
}

function cardFromRun(run: WorkflowRunState, mirror: TuiWorkflowMirror): TuiWorkflowCard {
  const { nodesSettled, nodesTotal } = workflowRunStepCounts(run);
  const seed = mirror.seedByRunId[run.runId];
  return {
    runId: run.runId,
    status: run.status,
    ...(run.stopReason === undefined ? {} : { stopReason: run.stopReason }),
    nodesSettled,
    nodesTotal,
// Label is only known by the server; projection entries do not carry it, and reseeding is used when reseeding is hit.
    ...(seed?.label === undefined ? {} : { label: seed.label }),
// resumable is a status bit (CLI arbitrates on run-settled loads, reducer handles), TUI does not re-derive.
    ...(run.resumable === undefined ? {} : { resumable: run.resumable }),
    usage: run.usage,
    actors: run.actors,
    logTail: mirror.logTailByRunId[run.runId] ?? [],
    ...(run.error === undefined ? {} : { error: run.error }),
    ...(run.resultPreview === undefined ? {} : { resultPreview: run.resultPreview }),
    ...(run.truncated === undefined ? {} : { truncated: run.truncated }),
  };
}

function appendWorkflowLogTail(
  current: Readonly<Record<string, readonly string[]>>,
  envelope: WorkflowRunProgressEnvelope,
): Readonly<Record<string, readonly string[]>> {
  if (envelope.eventType !== "log" || !envelope.runId) return current;
  const message = logMessage(envelope.payload);
  if (message === undefined) return current;
  const previous = current[envelope.runId] ?? [];
  const appended = [...previous, message].slice(-TUI_WORKFLOW_LOG_TAIL_LIMITS.maxEntries);
  return { ...current, [envelope.runId]: appended };
}

function logMessage(payload: Record<string, unknown> | undefined): string | undefined {
  if (!payload) return undefined;
  const raw = payload.message;
  if (typeof raw !== "string") return undefined;
  const collapsed = raw.replace(/\s+/gu, " ").trim();
  if (collapsed.length === 0) return undefined;
  return collapsed.slice(0, TUI_WORKFLOW_LOG_TAIL_LIMITS.maxEntryLength);
}

function sameSeed(left: TuiWorkflowRunSeed, right: TuiWorkflowRunSeed): boolean {
  return left.label === right.label && left.updatedAt === right.updatedAt;
}
