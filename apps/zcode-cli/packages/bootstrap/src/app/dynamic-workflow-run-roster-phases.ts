// ============================================================
// **Stage table** of situation section: declared site + visited site
// ============================================================
//The name and phase read the same reduction in the run panel
// Status (`run.phaseNames` / `run.phases` / `run.currentPhase` / `run.nodes`), what is read at all times is
//Event index - the division of labor on both sides is the same as what is written in the -roster-events.ts file header.
//
// Why does the node count take the **reduced state** instead of the node row of the journal: the phase coordinate (`phaseName`) only lives in the reduced state
// (The engine only stamps the birth event, there is no such column on the row), taking the row to join will only combine the nodes that cannot be accommodated in the reduction state
// Discard; and the reduction state still contains queued nodes that have not yet been executed, which is what counts in "how many are running in the current stage".

import type { DynamicWorkflowRunPhaseView } from "@zcode/contracts";
import type { WorkflowRunNode, WorkflowRunState } from "@zcode/shared/zcode-protocol-v4";
import type { RosterEventIndex } from "./dynamic-workflow-run-roster-events.js";

/** Nodes other than world-read / world-run are all asked. Note: `kind` is only carried in birth events, and is unknown if it is absent. */
const WORLD_NODE_KIND = "world-read";

/**
* Stage table: The declared stages in declaration order, followed by those "entered but not declared" (in the order of first entry).
 *
* **When the script is not declared and no one is entered, the entire script returns undefined**: In that case, there is no such thing as a stage in run.
* Posting an empty array reads like "the stage table is empty", which is another sentence.
 */
export function buildPhaseViews(input: {
  run: WorkflowRunState | undefined;
  index: RosterEventIndex;
  terminal: boolean;
}): DynamicWorkflowRunPhaseView[] | undefined {
  const { run, index, terminal } = input;
  const declared = run?.phaseNames ?? [];
  const entered = run?.phases ?? [];
  if (declared.length === 0 && entered.length === 0) return undefined;

  const names = [...declared];
  const seen = new Set(declared);
  for (const phase of entered) {
    if (seen.has(phase.name)) continue;
    seen.add(phase.name);
    names.push(phase.name);
  }

  const nodes = run?.nodes ?? [];
  return names.map((name) => {
    const rounds = entered.find((phase) => phase.name === name)?.rounds ?? 0;
    const owned = nodes.filter((node) => node.phaseName === name);
    const settled = owned.filter((node) => node.phase === "settled").length;
    const running = owned.length - settled;
    const trace = index.phases.get(name);
    return {
      name,
      state: phaseStateOf({ rounds, terminal, owned, isCurrent: run?.currentPhase === name }),
      rounds,
      nodesSettled: settled,
      nodesRunning: running,
      ...(trace?.enteredAt === undefined ? {} : { enteredAt: trace.enteredAt }),
      ...(trace?.exitedAt === undefined ? {} : { exitedAt: trace.exitedAt }),
    };
  });
}

/**
* The situation of a phase (see the four words of `DynamicWorkflowRunPhaseState`).
 *
* `unfinished` **Only true for the final state run**: When run is still alive, a flying ask in a non-current stage is a parallel branch
* is the norm, not unfinished - there are a few of it flying, `nodesRunning` has said it truthfully.
 */
function phaseStateOf(input: {
  rounds: number;
  terminal: boolean;
  owned: readonly WorkflowRunNode[];
  isCurrent: boolean;
}): DynamicWorkflowRunPhaseView["state"] {
  const { rounds, terminal, owned, isCurrent } = input;
  if (rounds === 0) return "ahead";
  if (terminal) {
    const leftover = owned.some(
      (node) => node.phase !== "settled" && node.kind !== WORLD_NODE_KIND,
    );
    return leftover ? "unfinished" : "done";
  }
  return isCurrent ? "current" : "done";
}
