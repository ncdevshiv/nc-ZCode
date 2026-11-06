// ============================================================
// Dynamic Workflow Run Service: run introspection reading surface (ListWorkflowRuns/GetWorkflowRun)
// ============================================================
// dynamic-workflow-run-service.ts reaches the upper limit of oxlint max-lines (400 lines), put `listRuns` /
// `getRunDetail` Two optional port members and their pendingQuestions slices are split into this file; the public side is still from
// dynamic-workflow-run-service.ts export. Both members only load the service object when the journal is queried with introspection -
// In absence, the entire non-implemented assertion remains in the service (see the `introspection` field comment over there).

import type {
  DynamicWorkflowRunDetail,
  DynamicWorkflowRunLifecycleStatus,
  DynamicWorkflowRunListItem,
  DynamicWorkflowRunListQuery,
  DynamicWorkflowRunListResult,
  DynamicWorkflowRunPendingQuestion,
  DynamicWorkflowRunPort,
} from "@zcode/contracts";
import type { JournalStorePort } from "@zcode/dynamic-workflow";
import { reduceWorkflowRunsState, type WorkflowRunsState } from "@zcode/shared/zcode-protocol-v4";
import type { DynamicWorkflowIntrospectableJournal } from "./dynamic-workflow-run-journal.js";
import { readRunScriptPath, readRunSubagentModel } from "./dynamic-workflow-run-launch-anchor.js";
import {
  artifactsOf,
  journalRunSummary,
  registryRunSummary,
  runConcurrencyField,
  runScriptPathField,
  runSubagentModelField,
  terminalErrorField,
  terminalResultField,
  toLogTailEntry,
  TERMINAL_RUN_STATUSES,
  type RunRegistryEntry,
} from "./dynamic-workflow-run-observation.js";
import { replayRunProgressFromEvents } from "./dynamic-workflow-run-replay.js";
import { buildWorkflowRunRoster } from "./dynamic-workflow-run-roster.js";
import type { WorkflowEscalationRegistry } from "./workflow-escalation-registry.js";

/**
* `getRunDetail` takes the number of `log()` tails by default. Tail is a sampling of narrative progression rather than a complete log - the complete event
* The log has sub-pages of {@link DynamicWorkflowRunPort.listEvents}.
 */
const DEFAULT_LOG_TAIL_LIMIT = 20;

/** The internal state of the service to be read by the introspection reading surface: the registry and the docking table are both references to the same instance, not copies. */
interface DynamicWorkflowRunIntrospectionContext {
/** Narrowed journal (the same object as `journal`, just of a wider type) has been probed for capabilities. */
  introspection: DynamicWorkflowIntrospectableJournal;
  journal: JournalStorePort;
  parentSessionId: string;
/** The memory registry of the service (on the fly + bounded final state). */
  runs: ReadonlyMap<string, RunRegistryEntry>;
/** Upgrade the docking table of Q&A, the only projection source for pendingQuestions slices. */
  escalations: WorkflowEscalationRegistry;
  /**
* The concurrency ceiling of this process (the implementation of service). The details of judging the upper bound value of a run based on it are not worth mentioning.
* ({@link runConcurrencyField}). is a function rather than a number: the introspection member is created once during service construction, and the ceiling
* is the fact every time it is read.
   */
  concurrencyCeiling: () => number;
}

/** Create two members, `listRuns` / `getRunDetail`, which are expanded into the returned port object by service. */
export function createRunIntrospectionMethods(
  ctx: DynamicWorkflowRunIntrospectionContext,
): Required<Pick<DynamicWorkflowRunPort, "listRuns" | "getRunDetail">> {
  const { introspection, journal, parentSessionId, runs, escalations, concurrencyCeiling } = ctx;
  return {
    /**
* Enumeration by project run:journal line ∪ This session registry. **Read only** - `possiblyInterrupted` is
* Read the mark in the page and never rewrite the journal (collect the corpse of the running run for the brother's conversation = mark the living run as dead;
* The execution rights of orphan convergence only belong to the construction time of the owning session, see file header invariant 4).
     */
    async listRuns(query: DynamicWorkflowRunListQuery): Promise<DynamicWorkflowRunListResult> {
      const limit = Math.max(0, query.limit);
// Take one more ** just to determine truncated**, it will not enter the results page. The criterion must not be `length === limit`:
// When the number of items is exactly equal to the limit, it will be a false positive, and a false positive will cause the model to chase a page of history that does not exist. The same convention is in
// Already used once on v4 gateway's event paging (hasMore).
      const rows = introspection.listRuns({
        cwd: query.cwd,
        limit: limit + 1,
        ...(query.statuses === undefined ? {} : { statuses: query.statuses }),
      });
      const journalItems = rows.map((row) => ({
        ...journalRunSummary(row, runs.get(row.runId), parentSessionId),
        spentTokens: row.spentTokens,
      }));

// Registry complement: among the microtasks that submit has returned and the engine has not yet createdRun, in the journal
// No trace. Without this branch, the newly started run of the model will completely disappear in its own project list.
      const listed = new Set(rows.map((row) => row.runId));
      const gapItems: DynamicWorkflowRunListItem[] = [];
      for (const [runId, entry] of runs) {
        if (listed.has(runId) || entry.cwd !== query.cwd) continue;
// The row already exists but is not selected by this query, indicating that it is excluded by limit / statuses - the complement will never
// Get it back (that's equivalent to silently invalidating an explicitly passed filter).
        if (journal.getRun(runId) !== undefined) continue;
        const summary = registryRunSummary(runId, entry);
        if (query.statuses !== undefined && !query.statuses.includes(summary.status)) continue;
// Usage: The line has not yet fallen, but the starting point of the revised run is determined at the moment of submit (see RunRegistryEntry.inheritedTokens).
// A new run has no starting point ⇒ 0, the same as before.
        gapItems.push({ ...summary, spentTokens: entry.inheritedTokens ?? 0 });
      }

// The run in the gap must be the latest submission (there are only a few microtasks before the journal line falls), so press
//The submission time is sorted before the journal line of time_updated desc in reverse order; limit is the limit of the entire page.
      gapItems.sort((left, right) => right.createdAt - left.createdAt);
      const merged = [...gapItems, ...journalItems];
      const truncated = merged.length > limit;
      return {
        runs: truncated ? merged.slice(0, limit) : merged,
// Only present when true: `false` is a noise field that must be included on every page.
        ...(truncated ? { truncated: true } : {}),
      };
    },

/** Single run details: dwf_run row + node count + actors + log tail + memory final state product. */
    async getRunDetail(runId: string): Promise<DynamicWorkflowRunDetail | undefined> {
      const row = introspection.getRunRow(runId);
      const entry = runs.get(runId);
      if (row === undefined) {
// Neither → really unknown (tool layer normalized to run_not_found). Only the registry has → exactly
// The gap above: run does exist, reporting not_found would be a lie.
        if (entry === undefined) return undefined;
        const gapSummary = registryRunSummary(runId, entry);
// The gap shape of the situation section: there is no event or node row yet, so the stage table is absent and the roster is empty.
// Health is all zero. The only one with content is `pendingQuestionsKnown` - the entry is in this session,
// Of course the parking table can be found (and it is **most likely** that there are parked questions at this moment, see the pendingQuestions comment below).
        const gapRoster = buildWorkflowRunRoster({
          run: undefined,
          events: [],
          nodes: [],
          actors: [],
          status: gapSummary.status,
          pendingQuestions: escalations.pendingFor(runId),
          now: Date.now(),
        });
        return {
          ...gapSummary,
// Gap copy of the concurrency upper bound (see RunRegistryEntry.maxConcurrency): the row has not yet fallen, but this value is
// submit has been determined at that moment, and there is no reason to pretend not to know the details in these microtasks.
          ...runConcurrencyField(entry.maxConcurrency, concurrencyCeiling()),
// Gap copy of subagent model, same argument (see RunRegistryEntry.subagentModel).
          ...runSubagentModelField(entry.subagentModel),
// Gap copy of script file, same argument (see RunRegistryEntry.scriptPath).
          ...runScriptPathField(entry.scriptPath),
// The count does not have any authoritative source at the moment and is honestly 0; the starting point of the usage is known (revised run inherits from the predecessor
// Cumulative value, see RunRegistryEntry.inheritedTokens), reporting 0 will say "This lineage cost nothing."
          usage: {
            spentTokens: entry.inheritedTokens ?? 0,
            nodesObserved: 0,
            nodesRunning: 0,
            nodesCompleted: 0,
            nodesFailed: 0,
          },
          actors: [],
          logTail: [],
          ...gapRoster,
          ...terminalResultField(gapSummary.status, entry),
          ...terminalErrorField(gapSummary.status, entry),
// This gap branch is the moment when "run is flying and the journal line has not yet fallen", which is the **most likely** problem
// Dwell moment - miss it and the blocked actor in the initial run will not be visible on the model side.
          ...pendingQuestionsField(escalations, runId),
// Product: The dwf_run line in this gap branch has not yet fallen.
// But the **node row may have been dropped** - the engine can putNode an artifact immediately after createRun
// OK. So here we still take it once, instead of taking it for granted: a run that was checked for details just after declaring the kanban board.
// Shouldn't appear to produce nothing on the model side. Parts of artifactsOf make the entire field absent.
          ...artifactsOf(runId, journal),
        };
      }

      const summary = journalRunSummary(row, entry, parentSessionId);
      const counts = introspection.countNodesByStatus(runId);
//Event **read only once**: the same sequence
// First reduce the casting chain to the same state in the run panel through cold playback, and then feed the situation section together with the node row/actor row.
// Reading once on both sides means paying twice for the same data, and this query is the most expensive part of the long run.
      const stored = journal.listEvents(runId, {});
      const pendingQuestions = pendingQuestionsOf(escalations, runId, summary.status, entry);
      return {
        ...summary,
// The upper bound of the drop library (`dwf_run.caps_max_concurrency`), only present when it is lower than the ceiling. deliberately not to advance
// journalRunSummary - that's the common cross-section of list rows, a rarely set field should not widen every row.
        ...runConcurrencyField(row.caps.maxConcurrency, concurrencyCeiling()),
// The subagent model of this run is only present when set. It is not on the dwf_run column (deliberately not migrated) -
// The authority is the `run-launched` event. The same rule as the snapshot: there is an entry to read the entry (the three paths to create an entry are all failed),
//Only Lengxing scans the event header. JournalRunSummary is deliberately not entered in line with the concurrency upper bound: a rare setting
// Fields should not widen each row of the list.
        ...runSubagentModelField(
          entry === undefined ? readRunSubagentModel(journal, runId) : entry.subagentModel,
        ),
// The script file of this run is only present when it is recorded. Same as subagent model: authority is `run-launched`
//Event (dwf_run does not have this column), if there is an entry, read the entry, and only scan the event header once when running coldly; also deliberately
// Do not enter journalRunSummary - list rows should not be widened for a field that only AmendWorkflow uses.
        ...runScriptPathField(
          entry === undefined ? readRunScriptPath(journal, runId) : entry.scriptPath,
        ),
        usage: {
// Direct reading dwf_run.spent_tokens: the only authority for run-level token usage.
          spentTokens: row.spentTokens,
// The sum of the number of rows of dropped nodes is **not** the "total number of steps": dynamic workflow does not have a static total number.
// And queued only exists in the event phase and does not fall into the library.
          nodesObserved: counts.running + counts.completed + counts.failed,
          nodesRunning: counts.running,
          nodesCompleted: counts.completed,
          nodesFailed: counts.failed,
        },
        actors: journal.listActors(runId).map((actor) => ({
          siteId: actor.siteId,
          ordinal: actor.ordinal,
// persona is not intentional: the entire system prompt is a naturally unbounded field on the port.
          ...(actor.name === undefined ? {} : { name: actor.name }),
        })),
        logTail: introspection
          .listRecentLogEvents(runId, DEFAULT_LOG_TAIL_LIMIT)
          .map(toLogTailEntry),
        ...buildWorkflowRunRoster({
          run: reduceRunState(row, stored, concurrencyCeiling()),
          events: stored,
          nodes: journal.listNodes(runId),
          actors: journal.listActors(runId),
          status: summary.status,
          ...(pendingQuestions === undefined ? {} : { pendingQuestions }),
          now: Date.now(),
        }),
        ...terminalResultField(summary.status, entry, row),
        ...terminalErrorField(summary.status, entry, row),
        ...(pendingQuestions === undefined || pendingQuestions.length === 0
          ? {}
          : { pendingQuestions }),
// Product cross-section: attached to any status, including failed / canceled
// - A run that dies at step 12 still delivers the image it produced earlier. with `listArtifacts` and final state
// The snapshot takes the **same** artifactsOf, and the same list is given in the three places (each of the three places merges one copy, sooner or later it will be in
// "Does the failed line count as one version?" This is a bifurcation). Part when the entire field is absent.
        ...artifactsOf(runId, journal),
      };
    },
  };
}

/**
* Slice of pendingQuestions for `getRunDetail`.
 *
* The same projection source as the `getTask` snapshot** (memory upgrade docking table), deliberately not going to the journal replay: there is in the journal
* raised/resolved are two types of events, but "who still owes an answer now" is a living fact in the process - the unmatched replayed
* raised will only lie after the process dies (the parked deferred has already disappeared with the process).
 *
* When there are zero entries, the entire field is absent (empty arrays are not sent, the same as snapshots): empty arrays read like "asked, answered", and absences read like "asked, answered"
* That is "no one is waiting".
 */
function pendingQuestionsField(
  escalations: WorkflowEscalationRegistry,
  runId: string,
): Pick<DynamicWorkflowRunDetail, "pendingQuestions"> {
  const pendingQuestions = escalations.pendingFor(runId);
  return pendingQuestions.length === 0 ? {} : { pendingQuestions };
}

/**
* In this reading, can you** answer "Are there any questions waiting for answers?" and the answers themselves
 * （`health.pendingQuestionsKnown`）。
 *
* `undefined` = cannot be found, not "none". The docking table is a table in the memory of this process (see {@link pendingQuestionsField}
* argument), so there are only two situations where this reading can tell the truth: run is in the registry of this session, or run has finalized
* ——The final state of run is by definition no one still listening. Other situations (brothers' sessions are running in the air, relics of dead processes)
* The empty table and "don't know" look exactly the same, and the two are two different next steps for the model: one can continue to wait,
* The other one has to ask another way.
 */
function pendingQuestionsOf(
  escalations: WorkflowEscalationRegistry,
  runId: string,
  status: DynamicWorkflowRunLifecycleStatus,
  entry: RunRegistryEntry | undefined,
): readonly DynamicWorkflowRunPendingQuestion[] | undefined {
  const known = entry !== undefined || TERMINAL_RUN_STATUSES.has(status);
  return known ? escalations.pendingFor(runId) : undefined;
}

/**
* journal event → run panel same reduction status. Task summary for `node-queued` and progress reading for `node-progress`
* Live only on events, and the rules for extracting them have been written once in the reducer - reuse that one here, not in the read surface
* Write another parser (the two parsers will diverge sooner or later on "whether to clear the round or not before requeuing").
 *
* Events are read and delivered by the caller; `replayRunProgressFromEvents` is the cold playback casting chain itself, so here
* The reduced state is the same byte-for-byte as what the UI sees after restarting.
 */
function reduceRunState(
  row: Parameters<typeof replayRunProgressFromEvents>[0],
  stored: Parameters<typeof replayRunProgressFromEvents>[1],
  concurrencyCeiling: number,
): WorkflowRunsState["runs"][number] | undefined {
  let state: WorkflowRunsState | undefined;
  for (const payload of replayRunProgressFromEvents(row, stored, concurrencyCeiling)) {
    state = reduceWorkflowRunsState(state, payload) ?? state;
  }
  return state?.runs.find((run) => run.runId === row.runId);
}
