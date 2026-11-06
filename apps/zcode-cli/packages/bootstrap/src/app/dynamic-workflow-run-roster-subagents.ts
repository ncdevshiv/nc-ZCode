// ============================================================
// **Sub-agent roster** for situational cross-section: what each sub-agent is doing at the moment
// ============================================================
// a subagent = an actor instance under which
// ask is run sequentially by `actorSeq`, so "what is it doing at the moment" = where is its ask chain.
//
// The three numbers each perform their own duties and do not pretend to be each other:
// - **actor row/node row** (journal): who exists, whether each ask was successful, and how many tokens were spent. Action is a lasting fact,
// There is no upper bound, and it will not be eaten by the elimination rule of projection.
// - **Reduction status**: Task summary and progress reading of this ask (`instructionsHead` / `turn` / `toolCalls` /
// `lastTool`), and the node's stage coordinates - they only live on events, there are no such columns on rows.
// - **Event Index**: All moments. See the header of -roster-events.ts.

import type {
  DynamicWorkflowRunPendingQuestion,
  DynamicWorkflowRunSubagentAsk,
  DynamicWorkflowRunSubagentState,
  DynamicWorkflowRunSubagentView,
} from "@zcode/contracts";
import type { ActorRecord, NodeRecord } from "@zcode/dynamic-workflow";
import type { WorkflowRunNode, WorkflowRunState } from "@zcode/shared/zcode-protocol-v4";
import {
  instanceKey,
  laterOf,
  type RosterEventIndex,
  type RosterNodeTrace,
} from "./dynamic-workflow-run-roster-events.js";

/** The "not yet settled" status of the node row is also the criterion of `unfinished` / `leftoverRunning`. */
export const NODE_ROW_RUNNING = "running";

/** The ask rows under the name of a subagent are sorted in ascending order by `actorSeq` (old rows missing this column are ranked first, and the order is stable). */
function asksOf(
  nodes: readonly NodeRecord[],
  actor: Pick<ActorRecord, "siteId" | "ordinal">,
): NodeRecord[] {
  return nodes
    .filter((node) => node.actorSiteId === actor.siteId && node.actorOrdinal === actor.ordinal)
    .sort((left, right) => (left.actorSeq ?? 0) - (right.actorSeq ?? 0));
}

/**
*Roster, in actor row order (=casting order). **Always returns an array**. If there is no actor, it is an empty array:
* "How many subagents does this run have?" is always a question with an answer, and 0 is that answer.
 *
* `pendingQuestions` is `undefined`, that is, "the parking table cannot be found in this read" (run is under the name of another process): at that time
* **No sub-agent will be reported as `parked`**, because at this moment it is impossible to distinguish between "no one is waiting" and "don't know".
 */
export function buildSubagentViews(input: {
  actors: readonly ActorRecord[];
  nodes: readonly NodeRecord[];
  run: WorkflowRunState | undefined;
  index: RosterEventIndex;
  terminal: boolean;
  pendingQuestions?: readonly DynamicWorkflowRunPendingQuestion[];
}): DynamicWorkflowRunSubagentView[] {
  const { actors, nodes, run, index, terminal, pendingQuestions } = input;
  const reduced = new Map<string, WorkflowRunNode>();
  for (const node of run?.nodes ?? []) reduced.set(instanceKey(node.siteId, node.ordinal), node);

  return actors.map((actor) => {
    const asks = asksOf(nodes, actor);
    const settled = asks.filter((ask) => ask.status !== NODE_ROW_RUNNING);
// The same subagent's ask is serial, so "the one running" can be at most one; if there are more than one, use actorSeq
// The biggest one - it was sent down recently.
    const runningAsk = asks.filter((ask) => ask.status === NODE_ROW_RUNNING).at(-1);
    const trace =
      runningAsk === undefined
        ? undefined
        : index.nodes.get(instanceKey(runningAsk.siteId, runningAsk.ordinal));
    const parkedOn = pendingQuestions?.find(
      (question) => question.actor === instanceKey(actor.siteId, actor.ordinal),
    )?.qid;
    const lastAskFailed = settled.at(-1)?.status === "failed";

    return {
      siteId: actor.siteId,
      ordinal: actor.ordinal,
      ...(actor.name === undefined ? {} : { name: actor.name }),
      state: subagentStateOf({
        terminal,
        running: runningAsk !== undefined,
        waiting: trace?.lastLifecycleType === "node-waiting",
        parked: parkedOn !== undefined,
        lastAskFailed,
      }),
      ...phaseNameField({ actor, runningAsk, settled, reduced, run }),
      ...(runningAsk === undefined ? {} : { currentAsk: currentAskOf(runningAsk, reduced, trace) }),
      ...(trace?.wait === undefined ? {} : { wait: trace.wait }),
      ...(parkedOn === undefined ? {} : { parkedOn }),
      stepsSettled: settled.length,
      stepsFailed: settled.filter((ask) => ask.status === "failed").length,
//Only accumulate the tokens of settled ask: the one in flight has not yet been accounted for (`stats` is backfilled by the driver during settlement).
      tokens: settled.reduce((total, ask) => total + (ask.stats?.tokens ?? 0), 0),
      ...lastProgressField(asks, index),
    };
  });
}

/**
* The situation of the subagent. **The decision order is the order of this code**, the previous hit is the final decision - the order itself is part of the contract:
* A subagent parked waiting for an answer also has "ask running". Reporting `executing` will hide its life-threatening incident.
 */
function subagentStateOf(input: {
  terminal: boolean;
  running: boolean;
  waiting: boolean;
  parked: boolean;
  lastAskFailed: boolean;
}): DynamicWorkflowRunSubagentState {
  const { terminal, running, waiting, parked, lastAskFailed } = input;
  if (terminal) {
// All three alive words on the final state run exit: the line still marked running only means that the process died under it.
    if (running) return "unfinished";
    return lastAskFailed ? "failed" : "done";
  }
  if (parked) return "parked";
  if (running) return waiting ? "waiting" : "executing";
  return lastAskFailed ? "failed" : "idle";
}

/** The cross-section of the current ask: the identity comes from the self, the task and progress come from the reduction state, and the time comes from the event index. */
function currentAskOf(
  ask: NodeRecord,
  reduced: ReadonlyMap<string, WorkflowRunNode>,
  trace: RosterNodeTrace | undefined,
): DynamicWorkflowRunSubagentAsk {
  const node = reduced.get(instanceKey(ask.siteId, ask.ordinal));
  const lastTool = node?.lastTool;
  return {
    siteId: ask.siteId,
    ordinal: ask.ordinal,
    ...(ask.actorSeq === undefined ? {} : { actorSeq: ask.actorSeq }),
    ...(node?.instructionsHead === undefined ? {} : { instructionsHead: node.instructionsHead }),
    ...(trace?.dispatchedAt === undefined ? {} : { startedAt: trace.dispatchedAt }),
    ...(node?.turn === undefined ? {} : { turn: node.turn }),
    ...(node?.toolCalls === undefined ? {} : { toolCalls: node.toolCalls }),
    ...(lastTool === undefined
      ? {}
      : {
          lastTool: {
            name: lastTool.name,
            ...(lastTool.target === undefined ? {} : { target: lastTool.target }),
            ...(trace?.lastToolAt === undefined ? {} : { at: trace.lastToolAt }),
          },
        }),
  };
}

/**
* The stage in which the current (or last) ask was born; when neither ask can be read, it returns to the actor's own birth stage.
* The stage coordinates are only in the reduction state, so what is checked here is the reduction node, not the row.
 */
function phaseNameField(input: {
  actor: Pick<ActorRecord, "siteId" | "ordinal">;
  runningAsk: NodeRecord | undefined;
  settled: readonly NodeRecord[];
  reduced: ReadonlyMap<string, WorkflowRunNode>;
  run: WorkflowRunState | undefined;
}): Pick<DynamicWorkflowRunSubagentView, "phaseName"> {
  const { actor, runningAsk, settled, reduced, run } = input;
  const candidates = [runningAsk, settled.at(-1)];
  for (const ask of candidates) {
    if (ask === undefined) continue;
    const phaseName = reduced.get(instanceKey(ask.siteId, ask.ordinal))?.phaseName;
    if (phaseName !== undefined) return { phaseName };
  }
  const born = run?.actors.find(
    (entry) => entry.siteId === actor.siteId && entry.ordinal === actor.ordinal,
  )?.phaseName;
  return born === undefined ? {} : { phaseName: born };
}

/** The last time it was observed moving: the last progress event of any ask under its name. */
function lastProgressField(
  asks: readonly NodeRecord[],
  index: RosterEventIndex,
): Pick<DynamicWorkflowRunSubagentView, "lastProgressAt"> {
  let latest: number | undefined;
  for (const ask of asks) {
    latest = laterOf(latest, index.nodes.get(instanceKey(ask.siteId, ask.ordinal))?.lastActivityAt);
  }
  return latest === undefined ? {} : { lastProgressAt: latest };
}
