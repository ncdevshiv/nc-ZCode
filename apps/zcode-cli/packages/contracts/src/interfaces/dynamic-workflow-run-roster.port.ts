// ============================================================
// **Situational Cross-section** for Dynamic Workflow Run: Stage / Subagent / Health
// ============================================================
// Detached from dynamic-workflow-run.port.ts for the same reason as dwf-journal-introspection.ts: that contract
// The max-lines limit of oxlint has been reached. The public side remains unchanged - the main port file is exported in place and every name here is exported.
// The import path of `@zcode/contracts` is unchanged verbatim.
//
//Three sets of fields are hung on `DynamicWorkflowRunDetail`, derived when the reader reads in `getRunDetail`;
// See the comments for each type below for contracts and invariants.

// A discipline that runs through the three groups: **There is only one source of time**, that is `dwf_event.time_created` (the event falls into the journal
// moment). It is wrong for readers to retrieve `Date.now()` now - that will mark the entire history of a week ago in a cold reading as "just".
//The whole reason for the existence of these three sets of fields is to make "how long ago" credible. Old journals that do not have this column will always use the time field
// **Absent**, do not give 0, do not give NaN.

/**
* The situation of a stage in the situation cross section.
 *
* - `ahead`: The script declares it, the control flow has not yet arrived - **only** `rounds: 0` state;
* - `current`: run is still alive and it is the last stage entered;
* - `unfinished`: run has finalized, and there is still an unfinished ask in this stage (the process died under it);
* - `done`: the rest.
 */
export type DynamicWorkflowRunPhaseState = "done" | "current" | "ahead" | "unfinished";

/** A phase in the situation section (`phase("…")` tag). */
export interface DynamicWorkflowRunPhaseView {
/** The author's original words, in the same vocabulary as `run-launched.phaseNames` / `phase-entered.name`. */
  name: string;
  state: DynamicWorkflowRunPhaseState;
/** Has been entered several times (reentry with the same name is +1). `ahead` is always 0, and the rest are always ≥ 1. */
  rounds: number;
/** The number of settled/unsettled items in nodes born at this stage. */
  nodesSettled: number;
  nodesRunning: number;
  /**
* **The most recent** entry moment, and that exit moment (the next synonymous `phase-entered` moment).
* Take the most recent one instead of the first time: a stage that has been circled three times. What the reader has to ask is "how long has it been since this circle came in?"
* There is no departure time in the current stage; when the event lacks a timestamp, both are absent.
   */
  enteredAt?: number;
  exitedAt?: number;
}

/**
* The current situation of a subagent. **The order of reading is the order of writing**, the previous hit is finalized:
 *
* run unfinished state: `parked` (has its own questions parked there waiting for answers) → `waiting` (the last life of the current ask
* The periodic event is `node-waiting`: waiting for a slot or backing off) → `executing` (the ask line is still running) → `failed`
* (The last settled ask failed) → `idle`.
 *
* run is finalized: all three living words have exited - `unfinished` (there is also an ask line marked running, that is, the process dies in it
* below) → `failed` → `done`.
 */
export type DynamicWorkflowRunSubagentState =
  | "idle"
  | "executing"
  | "waiting"
  | "parked"
  | "done"
  | "failed"
  | "unfinished";

/**
* The most recently observed tool call in an ask. `target` is the **clue** for people to see (file path, command header),
* Not the input parameter itself - every section of this link from the engine to here is cut along the same boundary, and the full text of the input parameter will never come in.
 */
export interface DynamicWorkflowRunSubagentLastTool {
  name: string;
  target?: string;
/** The drop-out moment of the `node-progress` that observed it; absent when the event has no timestamp. */
  at?: number;
}

/** The ask that the subagent is running at the moment (there is an ask line or `running` is present). */
export interface DynamicWorkflowRunSubagentAsk {
  siteId: string;
  ordinal: number;
/** This is the subagent's ask (`actorSeq` of journal) several times; the old row that does not fall into this column is absent. */
  actorSeq?: number;
  /**
* The first 240 words of the author's instructions for this ask. According to the reading, it answers "What is this sub-agent sent to do?" - Phase only
* You can say "running". It is absent from old journals without `instructionsHead`, and **does not read the full text of instructions**.
   */
  instructionsHead?: string;
/** The `node-dispatched` moment of this ask; absent when the event has no timestamp. */
  startedAt?: number;
  /**
* Which round has been parsed and how many times have the tools been adjusted (arrived with `node-progress`).
* Old journals without `node-progress` are always absent - absence is read as "don't know", `0` is read as
* "Not a single tool has been adjusted." These are two different facts.
   */
  turn?: number;
  toolCalls?: number;
  lastTool?: DynamicWorkflowRunSubagentLastTool;
}

/** What ask is currently waiting for (the last observation of `node-waiting`). */
export interface DynamicWorkflowRunSubagentWait {
/** `slot` = Waiting for the process-level admission gate; `backoff` = runner will retry after backing off. */
  cause: "slot" | "backoff";
  reason?: string;
  retryAfterMs?: number;
  /**
* **The moment when this waiting** is entered (i.e. the first one after the latest non-waiting life cycle event)
* `node-waiting`), not the last moment: the escape ladder will be sent several times in a row, and the reader is asking "how long has it been stuck?"
* Absent when event has no timestamp.
   */
  since?: number;
}

/** A subagent (= an actor instance) in the situation section. */
export interface DynamicWorkflowRunSubagentView {
  siteId: string;
  ordinal: number;
/** Valid name for `agent("poet")`; in the absence of anonymous actors, no hidden labels are synthesized here (same as pendingQuestions). */
  name?: string;
  state: DynamicWorkflowRunSubagentState;
/** Which stage it was born in during the current (or last) ask; when it cannot read out, it retreats to its own birth stage. */
  phaseName?: string;
  currentAsk?: DynamicWorkflowRunSubagentAsk;
  wait?: DynamicWorkflowRunSubagentWait;
  /**
* Which question it stops at (`pendingQuestions[].qid`). **You can only be present if you can actually find the parking table**:
* When `health.pendingQuestionsKnown` is false, the read does not know if anyone is waiting, see that field.
   */
  parkedOn?: string;
/** It has resolved / the number of failed asks, and the sum of the tokens of these asks (`stats` of the node row). */
  stepsSettled: number;
  stepsFailed: number;
  tokens: number;
/** The last time it was observed moving (the last progress event of any node under its name). */
  lastProgressAt?: number;
}

/**
* Run-level concurrency status: how much it is pressed at this moment, how much it should be, and why and when.
 *
* **The entire object is only present when `effective < cap`** (same absence rule as `maxConcurrency` in details):
* There is nothing to say about a run that fills its own boundary, and being present here means "it is being limited."
 */
export interface DynamicWorkflowRunConcurrencyHealth {
/** The number actually released by the manager at this time = min (shared gate, {@link cap}). */
  effective: number;
  /**
* **This run's own ** upper bound: if the user has set it, it is that number. If it has not been set, it is the ceiling of the machine.
   *
* Deliberately not reporting the ceiling: a run starting with `max_concurrency: 3` will always be displayed as "3/6" on a six-core machine.
* It sounds like it is being throttled, but it is running on the boundary set by the user - that boundary is not a fault, but a setting.
   */
  cap: number;
/** The reason for the last `concurrency-changed` (`rate_limited` / `recovered` ...). */
  reason?: string;
/** The drop-in time of that event; absent if there is no timestamp. */
  since?: number;
}

/** run Is the whole thing still moving? */
export interface DynamicWorkflowRunHealth {
/** Full run The moment of the last progress event. */
  lastProgressAt?: number;
  /**
* The moment when the last `run-stalled` was executed, and there is no progress event after it.
* "Waiting for a slot" does not count as progress - that period of waiting is the stagnation itself, and if you include it, you will never see stagnation.
   */
  stalledSince?: number;
  concurrency?: DynamicWorkflowRunConcurrencyHealth;
/** In order of settlement **The number of consecutive ** failed ask at the end: 3 consecutive times and 3 scattered times are two different situations. */
  consecutiveFailures: number;
/** The number of settlements that hit the completion cache (`node-settled { cached: true }`). */
  cachedSteps: number;
/** **Final state run only**: Number of node lines also marked with `running`, that is, those under which the process died. Absent when 0. */
  leftoverRunning?: number;
  /**
* Can you answer the question "Are there any questions waiting for answers" during this reading?
   *
* Parked questions only live in the memory of the process asking the question (see {@link DynamicWorkflowRunDetail.pendingQuestions}),
* So when reading a running run under the name of another process, "no questions to answer" and "don't know" look exactly the same - and these two
* For models are two completely different next steps. True if: this session holds a registry entry for this run, or run has been
* Final state (final state run is by definition no one listening). When false the entire `pendingQuestions` field is absent and there is no
* Subagents will be reported as `parked`.
   */
  pendingQuestionsKnown: boolean;
}
