// ============================================================
// **Situational Cross-section** for Dynamic Workflow Run: Stage / Subagent / Health
// ============================================================
// `getRunDetail` originally only gave the count and a cut
// The log tail cannot answer the three things that the model really wants to ask: where has run gone, what is each sub-agent doing at the moment, and is it still moving.
// This module is a derivation of those three things, and **just a derivation** - a pure function, without touching the journal, registry, or clock:
// Events, node rows, actor rows, reduction states, parking issues and `now` are all passed in by the caller.
//
//The reason why it should be pure: the rules of these three groups of fields (the order of determination of status words, the pronunciation of absence, the only source of time) are high in density,
// Error-prone, and their access points (SQLite journal + engine registry) are ridiculously expensive in testing. Separate the rules from the numbers,
// The rules can be nailed down with a handful of events and rows, and only one integrated use case is needed to obtain the data to prove that the wiring is connected.
//
// Split: event scan in -roster-events.ts, phase table in -roster-phases.ts, roster in
// -roster-subagents.ts (each file header writes its own numbering rules); this file leaves open contracts, health aspects and layout.

import type {
  DynamicWorkflowRunHealth,
  DynamicWorkflowRunLifecycleStatus,
  DynamicWorkflowRunPendingQuestion,
  DynamicWorkflowRunPhaseView,
  DynamicWorkflowRunSubagentView,
} from "@zcode/contracts";
import type { ActorRecord, NodeRecord, StoredEvent } from "@zcode/dynamic-workflow";
import type { WorkflowRunState } from "@zcode/shared/zcode-protocol-v4";
import { TERMINAL_RUN_STATUSES } from "./dynamic-workflow-run-observation.js";
import {
  indexRosterEvents,
  instanceKey,
  type RosterEventIndex,
} from "./dynamic-workflow-run-roster-events.js";
import { buildPhaseViews } from "./dynamic-workflow-run-roster-phases.js";
import { buildSubagentViews, NODE_ROW_RUNNING } from "./dynamic-workflow-run-roster-subagents.js";

/** The `kind` of the ask node line. The settlement of the losing streak is only a few ask: the world-read failure is not "subagent hanging in a row". */
const ASK_NODE_KIND = "ask";

/** The input parameter of the form section. */
interface WorkflowRunRosterInput {
  /**
* The reduction status of this run (the result of journal events replayed into the same reducer in the run panel).
* A run without an event (registry gap) passes `undefined`: the stage table is absent and the roster is empty at that time.
   */
  run: WorkflowRunState | undefined;
/** All journal events of this run, sequence ascending order. The **only** source of time. */
  events: readonly StoredEvent[];
/** `journal.listNodes(runId)`. Only contribute status with `stats`, without any time column on the row. */
  nodes: readonly NodeRecord[];
/** `journal.listActors(runId)`. The roster itself. */
  actors: readonly ActorRecord[];
/** The life cycle state of run has the same value as the one given in the details page (half of the state words are rewritten in the final state or not). */
  status: DynamicWorkflowRunLifecycleStatus;
  /**
* The problem of parking on this run at the moment; **`undefined` means that the parking table cannot be found during this reading** (the run is in another process
* name), at this time `health.pendingQuestionsKnown` is false and no sub-agent will be reported as `parked`.
* An empty array, on the contrary, is a definite "no one is waiting".
   */
  pendingQuestions?: readonly DynamicWorkflowRunPendingQuestion[];
/** The time of this read is only used to clamp the event time (see `timeOf` of -roster-events.ts). */
  now: number;
}

/** Situation section: Expand these three keys directly in the details section. */
interface WorkflowRunRoster {
  phases?: DynamicWorkflowRunPhaseView[];
  subagents: DynamicWorkflowRunSubagentView[];
  health: DynamicWorkflowRunHealth;
}

/** Derive the situation section from the facts that have been read in a getRunDetail call. */
export function buildWorkflowRunRoster(input: WorkflowRunRosterInput): WorkflowRunRoster {
  const { run, events, nodes, actors, status, pendingQuestions, now } = input;
  const terminal = TERMINAL_RUN_STATUSES.has(status);
  const index = indexRosterEvents(events, now);
  const phases = buildPhaseViews({ run, index, terminal });
  return {
    ...(phases === undefined ? {} : { phases }),
    subagents: buildSubagentViews({
      actors,
      nodes,
      run,
      index,
      terminal,
      ...(pendingQuestions === undefined ? {} : { pendingQuestions }),
    }),
    health: buildHealth({ run, nodes, index, terminal, pendingQuestions }),
  };
}

/** The run is still running as a whole (see `DynamicWorkflowRunHealth`). */
function buildHealth(input: {
  run: WorkflowRunState | undefined;
  nodes: readonly NodeRecord[];
  index: RosterEventIndex;
  terminal: boolean;
  pendingQuestions?: readonly DynamicWorkflowRunPendingQuestion[];
}): DynamicWorkflowRunHealth {
  const { run, nodes, index, terminal, pendingQuestions } = input;
// The final state of run is a legacy: when run is still alive, "there is a line marked running", which means it is working normally.
  const leftoverRunning = terminal
    ? nodes.filter((node) => node.status === NODE_ROW_RUNNING).length
    : 0;
  return {
    ...(index.lastProgressAt === undefined ? {} : { lastProgressAt: index.lastProgressAt }),
    ...(index.stalledSince === undefined ? {} : { stalledSince: index.stalledSince }),
    ...concurrencyField(run, index),
    consecutiveFailures: countTrailingFailures(nodes, index),
    cachedSteps: index.settlements.filter((settlement) => settlement.cached).length,
    ...(leftoverRunning === 0 ? {} : { leftoverRunning }),
    pendingQuestionsKnown: pendingQuestions !== undefined,
  };
}

/**
* and found the status.
 *
* `cap` is the upper bound of this run: if the user has set it, it will be that number; if it is not set, it will be the ceiling of the machine.
* `effective` is the number actually released by the manager at the moment, that is, it is taken again with the shared gate.
 *
* **Only present when `effective < cap`**, the same absence rule as `maxConcurrency` in the details page: one runs full of himself
* There is nothing to say about the run in that line. It is wrong to report ceiling as `cap` - a run starting with `max_concurrency: 3` is
* On a six-core machine, it will always show "3/6", which reads like it is being throttled, and it is running on the boundary defined by the user.
 */
function concurrencyField(
  run: WorkflowRunState | undefined,
  index: RosterEventIndex,
): Pick<DynamicWorkflowRunHealth, "concurrency"> {
  const concurrency = run?.concurrency;
  if (concurrency === undefined) return {};
  const cap = concurrency.limit ?? concurrency.ceiling;
  const effective = Math.min(concurrency.cap, cap);
  if (effective >= cap) return {};
  return {
    concurrency: {
      effective,
      cap,
      ...(index.concurrencyReason === undefined ? {} : { reason: index.concurrencyReason }),
      ...(index.concurrencySince === undefined ? {} : { since: index.concurrencySince }),
    },
  };
}

/**
* In order of settlement ** The number of consecutive ** failed ask at the end: 3 consecutive times and 3 scattered times are two different situations.
* The former means that the next one will probably fail. Just ask-world-read's failure is the fault of the script, not the failure of the subagent.
* `cancelled` breaks the losing streak the same as success: being canceled is not a failure.
 */
function countTrailingFailures(nodes: readonly NodeRecord[], index: RosterEventIndex): number {
  const askKeys = new Set(
    nodes
      .filter((node) => node.kind === ASK_NODE_KIND)
      .map((node) => instanceKey(node.siteId, node.ordinal)),
  );
  let failures = 0;
  for (let position = index.settlements.length - 1; position >= 0; position -= 1) {
    const settlement = index.settlements[position]!;
    if (!askKeys.has(settlement.key)) continue;
    if (settlement.outcome !== "failed") break;
    failures += 1;
  }
  return failures;
}
