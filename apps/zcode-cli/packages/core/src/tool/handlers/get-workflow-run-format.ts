// ============================================================
// Model side of GetWorkflowRun: TaskOutput-style XML-ish block
// ============================================================
// Detach from handler (the file has reached line 400
// upper limit), the three format pieces are split from here to get-workflow-run-format-roster.ts.
//
// This is a **pure function** `(output) => text`: all clock readings have been put into `generatedAt` by the handler,
// The formatter itself does not touch `Date.now()`. One output therefore only has one ruler - two "how long ago" in the same output
// Always comparable, the test can nail the entire text verbatim.
//
//The block sequence is a contract (fixed sequence): first explain the situation in one sentence, then the identity and life cycle, and then
// **What is waiting for the model to do at this moment** (the problem of parking), and then how is the run, narrative, ending and routing.

import {
  GetWorkflowRunOutputSchema,
  type GetWorkflowRunOutput,
  type ModelMessageContent,
} from "@zcode/contracts";
import { formatWorkflowProviderStopError } from "../../runtime-task/notification.js";
import { formatPublishedArtifactLine } from "../executor/workflow-published-artifacts.js";
import {
  formatWorkflowRunHealthBlock,
  formatWorkflowRunLogTailBlock,
  formatWorkflowRunPhasesBlock,
  formatWorkflowRunSubagentsBlock,
} from "./get-workflow-run-format-roster.js";
import {
  escapeWorkflowRunText,
  formatRelativeAge,
  formatWorkflowRunInstant,
  formatWorkflowRunTimestamp,
  workflowRunAttribute,
} from "./workflow-run-introspection.js";

/** Three words in the final state: run There is no next action (same as the summary side). */
const TERMINAL_STATUSES: ReadonlySet<string> = new Set(["completed", "errored", "stopped"]);

/**
* The question of parking ** cannot find the sentence when **. This is one of the only two ways this tool can "tell what you don't know":
* Silence can be read as "no one is waiting", and that is the most dangerous misreading - the actor is parked there, and there is no timeout to take care of it.
 */
const PENDING_QUESTIONS_UNKNOWN =
  "Unknown: pending questions are tracked only by the process that owns the run, and this session does not. Resuming the run will re-ask any question its subagent still needs answered.";

const PENDING_QUESTIONS_INSTRUCTION =
  "Each of these subagents is parked waiting for an answer and nothing times out on its behalf. Answer one with ResolveWorkflowQuestion using the ID in brackets. The rest of the run keeps running meanwhile.";

export function formatGetWorkflowRunModelContent(output: unknown): ModelMessageContent {
  const parsed = GetWorkflowRunOutputSchema.safeParse(output);
  if (!parsed.success) return "GetWorkflowRun returned an invalid result.";
  const run = parsed.data;
  const terminal = TERMINAL_STATUSES.has(run.status);

  const blocks = [
// A sentence about situation is listed first: each subsequent piece is its unfolding, and readers must first know what they are looking at.
    `<summary>${escapeWorkflowRunText(run.summary)}</summary>`,
    ...identityBlocks(run),
    ...pendingQuestionBlocks(run),
    formatWorkflowRunHealthBlock(run, terminal),
    ...optionalBlock(formatWorkflowRunPhasesBlock(run, terminal)),
    formatWorkflowRunSubagentsBlock(run),
    formatWorkflowRunLogTailBlock(run),
    usageBlock(run),
    ...outcomeBlocks(run),
    ...routingBlocks(run),
  ];
  return blocks.join("\n\n");
}

function optionalBlock(block: string | undefined): string[] {
  return block === undefined ? [] : [block];
}

/** Identity and life cycle: One fact has one label, and no empty label will be left for absent facts. */
function identityBlocks(run: GetWorkflowRunOutput): string[] {
  const blocks = [
    `<run_id>${escapeWorkflowRunText(run.runId)}</run_id>`,
    `<label ${workflowRunAttribute("source", run.labelSource)}>${escapeWorkflowRunText(run.label)}</label>`,
    `<status>${escapeWorkflowRunText(run.status)}</status>`,
    ...(run.stopReason === undefined
      ? []
      : [`<stop_reason>${escapeWorkflowRunText(run.stopReason)}</stop_reason>`]),
    ...(run.resumedFrom === undefined
      ? []
      : [`<resumed_from>${escapeWorkflowRunText(run.resumedFrom)}</resumed_from>`]),
// Only present when this run itself suppresses concurrency (the port's "absent"). Once AmendWorkflow is omitted
// `max_concurrency` is inherited from this number - this is how the model knows what revisions will inherit.
    ...(run.maxConcurrency === undefined
      ? []
      : [`<max_concurrency>${run.maxConcurrency}</max_concurrency>`]),
// Run-level settings in the same family as the previous line: only present if this run itself has selected a subagent model.
    ...(run.subagentModel === undefined
      ? []
      : [`<subagent_model>${escapeWorkflowRunText(run.subagentModel)}</subagent_model>`]),
    ...(run.supersededBy === undefined
      ? []
      : [`<superseded_by>${escapeWorkflowRunText(run.supersededBy)}</superseded_by>`]),
    `<owned_by_this_session>${run.ownedByThisSession}</owned_by_this_session>`,
  ];
  if (run.possiblyInterrupted) {
    blocks.push(
      "<possibly_interrupted>true — this session cannot confirm the run is still alive</possibly_interrupted>",
    );
  }
// ISO + age: The former is a verifiable fact, the latter is the quantity the reader really wants.
  blocks.push(
    `<created_at>${formatWorkflowRunInstant(run.generatedAt, run.createdAt)}</created_at>`,
  );
  blocks.push(
    `<updated_at>${formatWorkflowRunInstant(run.generatedAt, run.updatedAt)}</updated_at>`,
  );
  return blocks;
}

/**
* On-the-fly upgrade issues, before health/roster/narrative: the rest are all about "how was this run?"
* And this piece is **one thing waiting for the model to do at this moment** - buried behind twenty lines of narrative, it is equivalent to hiding the only way to unblock it.
 */
function pendingQuestionBlocks(run: GetWorkflowRunOutput): string[] {
  if (!run.health.pendingQuestionsKnown) {
    return [`<pending_questions>${PENDING_QUESTIONS_UNKNOWN}</pending_questions>`];
  }
  const questions = run.pendingQuestions ?? [];
  if (questions.length === 0) return [];
  const rendered = questions.map((question) => {
    const who = question.actorName ?? question.actor;
    const age = formatRelativeAge(run.generatedAt, question.askedAt);
    const asked =
      age === undefined
        ? `asked at ${formatWorkflowRunTimestamp(question.askedAt)}`
        : `asked ${age}`;
    const lines = [
      `[${escapeWorkflowRunText(question.qid)}] ${escapeWorkflowRunText(who)} ${asked}`,
      escapeWorkflowRunText(question.question),
    ];
    if (question.context !== undefined)
      lines.push(`context: ${escapeWorkflowRunText(question.context)}`);
    return lines.join("\n");
  });
  return [
    `<pending_questions>\n${rendered.join("\n\n")}\n\n${PENDING_QUESTIONS_INSTRUCTION}\n</pending_questions>`,
  ];
}

/** nodes_observed is the number of rows of dropped nodes, and never pretends to be the "total number of steps": there is no static total in dynamic workflows. */
function usageBlock(run: GetWorkflowRunOutput): string {
  const usage = [
    `spent_tokens=${run.usage.spentTokens}`,
    `nodes_observed=${run.usage.nodesObserved}`,
    `nodes_running=${run.usage.nodesRunning}`,
    `nodes_completed=${run.usage.nodesCompleted}`,
    `nodes_failed=${run.usage.nodesFailed}`,
  ].join(" ");
  return `<usage>${usage}</usage>`;
}

/** The end of run: product → failure → user interface product list (same order as completion notification). */
function outcomeBlocks(run: GetWorkflowRunOutput): string[] {
  const blocks: string[] = [];
  if (run.result !== undefined) {
// Enter the product as it is (without escaping): it may be a whole piece of JSON or code, and escaping will make what the model reads different from the real product.
    blocks.push(`<result>\n${run.result}\n</result>`);
  }
  if (run.error !== undefined) {
// provider stops: `<error>` is a whole block of copy cast by the same function as the final state notification (reason → action → fact → original text),
// The model reads the same paragraph on both reading pages; enter the block as it is according to the precedent of `<result>` - there is `run_id="…"` in it
// In this way, if you want the fragment to be copied by the model, escaping it as " will cause it to be copied incorrectly. Otherwise, it will still be a message.
    const body =
      run.error.providerStop === undefined
        ? escapeWorkflowRunText(run.error.message)
        : `\n${formatWorkflowProviderStopError(run.error, run.runId)}\n`;
    blocks.push(`<error ${workflowRunAttribute("code", run.error.code)}>${body}</error>`);
  }
//User-side products are ranked after result/error **(same as the order of completion notifications): the end of run is the first thing to be read by the model.
// The list of deliverables is an index. The format of the line shares the same formatter as the completion notification verbatim.
  if (run.artifacts !== undefined && run.artifacts.length > 0) {
    const lines = run.artifacts.map((artifact) =>
      escapeWorkflowRunText(formatPublishedArtifactLine(artifact)),
    );
    blocks.push(`<artifacts count="${run.artifacts.length}">\n${lines.join("\n")}\n</artifacts>`);
  }
  return blocks;
}

/** Routing: replaced → only points to the successor; otherwise, the recoverable and revisable routes each occupy one section, and each has its own discussion. */
function routingBlocks(run: GetWorkflowRunOutput): string[] {
  const blocks: string[] = [];
  if (run.stopReason === "superseded") {
// The replaced run cannot be resumed (the living one is the successor), nor should it be revised again - the successor is the one that needs to be revised.
    const successor =
      run.supersededBy === undefined
        ? "its successor"
        : `run ${escapeWorkflowRunText(run.supersededBy)}`;
    blocks.push(
      `<superseded>This run was stopped by an AmendWorkflow and superseded by ${successor}, which owns its unfinished work. Do not resume it (ResumeWorkflowRun will refuse) and do not amend it again; read or amend ${successor} instead.</superseded>`,
    );
    return blocks;
  }
  if (isWorkflowRunResumableOutput(run)) blocks.push(formatResumableHint(run));
  blocks.push(formatAmendableHint(run));
  return blocks;
}

/**
* The four stop reasons are both recoverable, but the next steps are different: user was stopped intentionally (recovered only when requested by the user); provider must first resolve the reason;
* model / interrupted Directly continued. The prompt block includes this sentence, otherwise the model will continue the run just canceled by the user as an accident.
 */
function formatResumableHint(run: GetWorkflowRunOutput): string {
  const reasonSentence =
    run.stopReason === "user"
      ? " This run was stopped on purpose by the user: resume it only when the user asks."
      : run.stopReason === "model"
        ? " You stopped this run yourself with TaskStop: resume it unchanged only if that is what the user wants. If you stopped it to fix the script, do not wait — amend it now, see <amendable>."
        : run.stopReason === "provider"
          ? " A provider-side error stopped it: resolve the cause named in <error> with the user before resuming, or it will stop again the same way."
          : "";
  return `<resumable>This run can be continued with ResumeWorkflowRun — it will resume under the same run ID, replaying finished steps and re-dispatching the unfinished ones.${reasonSentence} The script must be byte-for-byte the one this run was started with; to change it, see <amendable>.</resumable>`;
}

/**
* Revise the routing prompt for amend-resume. **parallel with `<resumable>` rather than replace**: the predicates of the two are different,
* The actions are also different, so I deliberately occupy one section for each and explain the differences (same run, same script vs new run, new script).
 *
* The last sentence is forked according to status: the script's true failure is the highest value scenario for revision (fixing bugs and saving cache), and the default reflection of the model is
* Rewrite from scratch - that will invalidate all the work for which tokens have been paid.
 *
* **Only one sentence for run that is running healthily**: At that time, there was no decision for the model to make now, and the entire routing argument was just a squeeze.
* It is the attention of reading the roster; it has stalled, or has not yet taken off, or has reached its final state, before the complete argument is presented.
 */
function formatAmendableHint(run: GetWorkflowRunOutput): string {
// The sentence in the script file includes both: the one that is running healthy
// Only one sentence is given, but that sentence is exactly "how to revise" - missing the file means asking the model to inline and repost the entire script.
  const scriptSentence =
    run.scriptPath === undefined
      ? ""
      : ` Its script is at ${escapeWorkflowRunText(run.scriptPath)}: edit that file in place and pass \`path: "${escapeWorkflowRunText(run.scriptPath)}"\` to AmendWorkflow instead of a script.`;
  if (run.status === "running" && run.health.stalledSince === undefined) {
    return `<amendable>AmendWorkflow with run_id "${escapeWorkflowRunText(run.runId)}" supersedes this run with a revised script and imports its finished work as cache.${scriptSentence}</amendable>`;
  }
  const tail =
    run.status === "errored"
      ? "This is the highest-value case for it: the script itself failed, so fix the script and re-run — every step that already succeeded is imported instead of being paid for a second time. Do NOT rewrite from scratch."
      : run.status === "completed"
        ? "Use it to extend or refine a finished workflow — added steps run live, unchanged ones cost nothing."
        : run.status === "stopped"
          ? "Use it when the script or a setting needs to change; use ResumeWorkflowRun to continue it unchanged. A run stopped because its script was wrong is amended now, not after the user asks: the cache holds everything that settled before the stop, and waiting buys nothing."
          : "It is still running: if the script is visibly wrong, amend it now — AmendWorkflow stops this run, imports everything that settled so far, and starts the revision in one call. Do not TaskStop it first and do not wait for it to finish.";
  const body = [
    `This run can be superseded by a revised script: call AmendWorkflow with \`run_id: "${escapeWorkflowRunText(run.runId)}"\` and your new script.`,
    "That mints a NEW run and imports this one's finished work as a warm cache — matched per named subagent along its conversation prefix — so steps you did not change settle from cache at zero tokens and only the revised part runs live.",
// If omitted, use it: don’t say it,
// The model will copy the entire script again in order to change a number.
    "To change only its settings (max_concurrency, subagent_model, name), omit both `script` and `path`: the new run keeps this run's script.",
    tail,
  ].join(" ");
  return `<amendable>${body}${scriptSentence}</amendable>`;
}

/**
* The predicate for restoring the final state has the same semantics as the gate of `port.resume`:
* `stopped`, regardless of reason. **Never** relax to `errored`: a run replay where the script really fails will reproduce the failure verbatim.
* This is just a route preview, not a release promise, the door is still on the port.resume server.
 */
function isWorkflowRunResumableOutput(run: GetWorkflowRunOutput): boolean {
  return run.status === "stopped";
}
