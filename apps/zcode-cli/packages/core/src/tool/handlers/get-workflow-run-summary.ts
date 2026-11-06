// ============================================================
// `<summary>` of GetWorkflowRun: a sentence of situation
// ============================================================
//
// This sentence is spelled out **deterministically** from the structured field, without any model involvement: the same snapshot will always spell out the same sentence,
// So it can be pinned verbatim and won't change itself between reads. It answers the three things readers really ask—
// Where is this run, is it moving, is there anything waiting for me to do - and the following blocks are an expansion of these three answers.
//
//A rule runs throughout the text: **If you don’t know, don’t tell**. If there is no timestamp, the age will not be given. If there is no stage, the stage position will not be mentioned.
// If the parking table cannot be found, just say "don't know". Never use 0 or "unknown" to pretend to be a fact.

import type { GetWorkflowRunOutput } from "@zcode/contracts";
import { GET_WORKFLOW_RUN_SUMMARY_MAX_CHARS } from "@zcode/contracts";
import {
  formatRelativeAge,
  formatWorkflowRunCount,
  formatWorkflowRunDuration,
} from "./workflow-run-introspection.js";

/** The fact that the summary should be read = the entire output minus the summary itself (the handler casts the output first, and then uses it to spell this sentence). */
type WorkflowRunSummaryFacts = Omit<GetWorkflowRunOutput, "summary">;

/** Three final state words: run has no next action (same as the final state determination of the port). */
const TERMINAL_STATUSES: ReadonlySet<string> = new Set(["completed", "errored", "stopped"]);

/** When the character budget is exceeded, the entire sentence will be discarded. If it cannot be discarded, it will be cut off - leaving one character for the ellipsis. */
const SUMMARY_ELLIPSIS = "…";

export function buildWorkflowRunSummary(run: WorkflowRunSummaryFacts): string {
  const now = run.generatedAt;
  const terminal = TERMINAL_STATUSES.has(run.status);

// The first two sentences are the skeleton (where is this run + what step is it taken to), which can not be lost under any budget.
  const required = [
    `${statusClause(run, now, terminal)}${phaseClause(run)}.`,
    `${stepsClause(run, terminal)}.`,
  ];
  const optional = [
    failureClause(run),
    questionsClause(run),
    progressClause(run, now, terminal),
    deliverableClause(run),
    ownershipClause(run),
  ].filter((clause): clause is string => clause !== undefined);

  const sentences = [...required, ...optional];
  while (
    sentences.length > required.length &&
    sentences.join(" ").length > GET_WORKFLOW_RUN_SUMMARY_MAX_CHARS
  ) {
    sentences.pop();
  }
  const text = sentences.join(" ");
  if (text.length <= GET_WORKFLOW_RUN_SUMMARY_MAX_CHARS) return text;
  return `${text.slice(0, GET_WORKFLOW_RUN_SUMMARY_MAX_CHARS - SUMMARY_ELLIPSIS.length)}${SUMMARY_ELLIPSIS}`;
}

/** Status + how long it has been running/how long it ended. */
function statusClause(run: WorkflowRunSummaryFacts, now: number, terminal: boolean): string {
  const elapsed = formatWorkflowRunDuration(run.updatedAt - run.createdAt);
  if (!terminal) {
    return run.status === "pending"
      ? "Pending, not dispatched yet"
      : `Running for ${formatWorkflowRunDuration(now - run.createdAt)}`;
  }
  if (run.status === "completed") return `Completed in ${elapsed}`;
  if (run.status === "errored") return `Errored after ${elapsed}`;
// stopped: First tell how long ago it stopped (what the reader has to judge is "Is this still new?"), and then how long it has been running.
  const reason = run.stopReason === undefined ? "stopped" : run.stopReason;
  const endedAge = formatRelativeAge(now, run.updatedAt);
  const ended = endedAge === undefined ? "" : ` ${endedAge}`;
  return `Stopped (${reason})${ended} after ${elapsed}`;
}

/**
* Stage location. The ones who are alive (or those who are in the final state but still have no mouth) run say "what number/total number", and those who have finished all the runs only say the total number——
* For a run that has been completed, "in the 4th stage" is a sentence without information. The entire sentence does not appear when the script has no stages.
 */
function phaseClause(run: WorkflowRunSummaryFacts): string {
  const phases = run.phases;
  if (phases === undefined || phases.length === 0) return "";
  const index = phases.findIndex(
    (phase) => phase.state === "current" || phase.state === "unfinished",
  );
  if (index < 0) return `, across ${phases.length} phase${phases.length === 1 ? "" : "s"}`;
  return `, in phase ${index + 1} of ${phases.length} (${phases[index]!.name})`;
}

/** Number of steps. `nodesObserved` is the number of dropped node rows, and never pretends to be the "total number of steps" - there is no static total in dynamic workflows. */
function stepsClause(run: WorkflowRunSummaryFacts, terminal: boolean): string {
  const settled = run.usage.nodesCompleted + run.usage.nodesFailed;
  const leftover = run.health.leftoverRunning;
  if (terminal && leftover !== undefined && leftover > 0) {
// The process dies under these steps: they are not "running", but marks on a corpse - this is how the reader knows that recovery will resend them.
    return `${settled} of ${run.usage.nodesObserved} dispatched steps settled; ${leftover} ${
      leftover === 1 ? "was" : "were"
    } still running when the owning process exited and will be re-dispatched on resume`;
  }
  if (terminal) {
    const failed = run.usage.nodesFailed > 0 ? `, ${run.usage.nodesFailed} failed` : "";
    return `${settled} step${settled === 1 ? "" : "s"} settled${failed}, ${formatWorkflowRunCount(
      run.usage.spentTokens,
    )} tokens`;
  }
  const running =
    run.usage.nodesRunning > 0 ? `, ${run.usage.nodesRunning} running${runningBreakdown(run)}` : "";
  return `${settled} of ${run.usage.nodesObserved} dispatched steps settled${running}`;
}

/**
* What are you doing during the flying steps (phase counting of the roster). The entire bracket does not appear when the roster is not read.
 *
* **Invariant**: A subagent in the active phase (`executing` / `waiting` / `parked`) has exactly one item in its name also marked
* The ask line of `running` - `waiting` means that the ask is waiting for a slot or backing off, `parked` means that it stops on a question,
*Both lines have not been settled yet. So these three numbers are a **division** of `usage.nodesRunning` and must add up to equal it.
* The numbers in the brackets do not match the numbers outside the brackets. It can only be that the person who created the data made a journal that does not exist in reality.
 */
function runningBreakdown(run: WorkflowRunSummaryFacts): string {
  const counts = { executing: 0, waiting: 0, parked: 0 };
  for (const subagent of run.subagents) {
    if (subagent.state === "executing") counts.executing += 1;
    else if (subagent.state === "waiting") counts.waiting += 1;
    else if (subagent.state === "parked") counts.parked += 1;
  }
  const parts = (Object.keys(counts) as (keyof typeof counts)[])
    .filter((key) => counts[key] > 0)
    .map((key) => `${counts[key]} ${key}`);
  return parts.length === 0 ? "" : ` (${parts.join(", ")})`;
}

/**
* Questions to be answered. **"I don't know" is a word that must be said**: When reading run under the name of another process, "No one is waiting"
* looks exactly the same as "can't find", and the two are completely different next steps for the model.
 */
function questionsClause(run: WorkflowRunSummaryFacts): string | undefined {
  if (!run.health.pendingQuestionsKnown) return "Pending questions are unknown from this session.";
  const count = run.pendingQuestions?.length ?? 0;
  if (count === 0) return undefined;
  return `${count} question${count === 1 ? "" : "s"} awaiting your answer.`;
}

/** When was the last time you were observed moving; if you are stagnant, say the word. */
function progressClause(
  run: WorkflowRunSummaryFacts,
  now: number,
  terminal: boolean,
): string | undefined {
  if (terminal) return undefined;
  const age = formatRelativeAge(now, run.health.lastProgressAt);
  if (age === undefined) return undefined;
  return run.health.stalledSince === undefined
    ? `Last progress ${age}.`
    : `Stalled, last progress ${age}.`;
}

/** Failure code summary: The model uses it to distinguish "the process died" and "the script really failed", and this determines which route it will take next. */
function failureClause(run: WorkflowRunSummaryFacts): string | undefined {
  if (run.error === undefined) return undefined;
  if (run.status !== "errored" && run.stopReason !== "provider") return undefined;
  return `Failure: ${run.error.code}.`;
}

/**
* Deliverable (only for completed): The reader should be pointed to that one thing, not a list of products.
 *
* Title **unquoted**: This sentence will eventually be XML-ish escaped into `<summary>`, and the escape table replaces `"` with
* `"`——A pair of quotation marks added for readability will turn into two entities in front of the model. The kind in the brackets has enclosed the title.
 */
function deliverableClause(run: WorkflowRunSummaryFacts): string | undefined {
  if (run.status !== "completed") return undefined;
  const primary = run.artifacts?.find((artifact) => artifact.primary === true);
  if (primary === undefined) return undefined;
  return `Deliverable: ${primary.title ?? primary.id} (${primary.kind}, primary).`;
}

/** Run that is not owned by this session: the notification will not come, and TaskOutput cannot see it - the first sentence of summary must indicate the attribution. */
function ownershipClause(run: WorkflowRunSummaryFacts): string | undefined {
  return run.ownedByThisSession ? undefined : "Owned by another session.";
}
