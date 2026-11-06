// ============================================================
// GetWorkflowRun Tool Handler
// ============================================================
// Adaptive details of a single workflow run: running → progress summary + log tail; final state → product / failure.
//
// handler only does four things that ports can't do:
// 1. **Product serialization**. What the port hands over is the **original value** of the script return value; the model-oriented text projection has a unique value in core
// Implement (`serializeWorkflowArtifact`, completion notifications share it with TaskOutput). Do it again on the port side
// It will appear that "the product of the same run looks different in the notification and in this tool."
// 2. **Normalization of unknown runId**. The port returns `undefined`, the model expects a structured failure.
// 3. **Read the clock once**. `generatedAt` is taken once here, and all "how long ago" in the model are calculated for it——
// The formatter is therefore a pure function, and the two ages in one output are always comparable.
// 4. **Spell summary**. `summary` is spelled out deterministically from structured fields (get-workflow-run-summary.ts),
// No model involved.
//
// Deliberately **not doing** wait/block semantics: waiting is TaskOutput's job, here is an instant snapshot.

import {
  GET_WORKFLOW_RUN_TOOL_NAME,
  GetWorkflowRunInputJsonSchema,
  GetWorkflowRunInputSchema,
  GetWorkflowRunOutputJsonSchema,
  GetWorkflowRunOutputSchema,
  type GetWorkflowRunInput,
  type GetWorkflowRunOutput,
} from "@zcode/contracts";
import { serializeWorkflowArtifact } from "../executor/workflow-artifact.js";
// ⚠ Two artifacts: the one serialized above is the **top-level return value** of the script (into `<result>`), and the one below is described
// The output of the script is published to the user via `artifact.*` (enter `<artifacts>`). The same word has two meanings, and both appear in this document.
import { WORKFLOW_ARTIFACTS_INTROSPECTION_MAX_LINES } from "../executor/workflow-published-artifacts.js";
import type { ToolEntry, ToolHandler } from "../types.js";
import { formatGetWorkflowRunModelContent } from "./get-workflow-run-format.js";
import {
  toGetWorkflowRunHealth,
  toGetWorkflowRunPhases,
  toGetWorkflowRunSubagents,
} from "./get-workflow-run-roster-output.js";
import { buildWorkflowRunSummary } from "./get-workflow-run-summary.js";
import { describeWorkflowScriptPath } from "./workflow-script-path.js";
import {
  WORKFLOW_RUN_INTROSPECTION_STEERING,
  workflowIntrospectionUnavailableFailure,
  workflowRunNotFoundFailure,
} from "./workflow-run-introspection.js";

const GET_WORKFLOW_RUN_TIMEOUT_MS = 10_000;
/** According to TaskOutput: a product is a payload that may be large enough to require an artifact. */
const GET_WORKFLOW_RUN_RESULT_BUDGET_BYTES = 400_000;
const GET_WORKFLOW_RUN_PERSIST_THRESHOLD_CHARS = 100_000;

const GET_WORKFLOW_RUN_DESCRIPTION = [
  "Returns the current state of one dynamic-workflow run: progress, token usage and the tail of its log() narration while it runs; the final result once it completed; the structured failure if it errored or was stopped.",
  "",
  WORKFLOW_RUN_INTROSPECTION_STEERING,
  "",
  "- Takes run_id — from CreateWorkflow's or AmendWorkflow's result, from a completion notification, or from ListWorkflowRuns.",
  "- This is an instant snapshot and never waits. To block until a run THIS session started finishes, use TaskOutput instead: that is the waiting tool. GetWorkflowRun is the right tool when you must not wait, or when the run belongs to another session (TaskOutput cannot see those).",
  "- The `artifacts` section lists what the run published for the user — files, documents and live dashboards that are ALREADY shown to them as cards. Refer to one by its title; do not paste its contents back. The one marked `primary` is the deliverable: point the user to it first.",
  "- Three terminal states: `completed`; `errored` (the script itself failed — not resumable, amend it); `stopped` with a stop reason — `user` (cancelled on purpose: resume only when the user asks), `model` (your own TaskStop), `provider` (a provider-side error such as an expired sign-in, a model missing from the plan or a quota cap — the `<error>` block names the cause and the fix; resolve it with the user, then resume), `interrupted` (the process that owned the run exited — continuing it is usually what the user wants), `superseded` (an AmendWorkflow replaced it; `<superseded_by>` names the successor — read that run instead, never resume this one).",
  "- A stopped run (other than a superseded one) can be continued with ResumeWorkflowRun — no rebuild needed, same run ID, same script.",
  "- ANY run — completed, stopped, errored, or still running — can instead be revised with AmendWorkflow: pass its run ID and the corrected script, and the finished work is imported as cache. When the script itself errored, that is the move — fix the script and keep the work that already succeeded, rather than rewriting from scratch.",
].join("\n");

const getWorkflowRunHandler: ToolHandler = async (input, context) => {
  const parsed = GetWorkflowRunInputSchema.parse(input) as GetWorkflowRunInput;

  const port = context.dynamicWorkflowRunPort;
  if (port === undefined || typeof port.getRunDetail !== "function") {
    return workflowIntrospectionUnavailableFailure();
  }

  const detail = await port.getRunDetail(parsed.run_id);
// An empty object will make the model think that this run exists but has no content; an unknown runId is a first-class failure.
  if (detail === undefined) return workflowRunNotFoundFailure(parsed.run_id);

// `undefined` product → the entire field is absent (same as completion notification). `null` is a legal product and serializes to "null".
  const result = serializeWorkflowArtifact(detail.result);
// Call one ruler at a time: all "how long ago" are calculated for this one reading, so two ages are always comparable.
  const generatedAt = Date.now();
  const roster = toGetWorkflowRunSubagents(detail.subagents);
  const phases = toGetWorkflowRunPhases(detail.phases);

  const base = {
    runId: detail.runId,
    label: detail.label,
    labelSource: detail.labelSource,
    status: detail.status,
    ...(detail.stopReason === undefined ? {} : { stopReason: detail.stopReason }),
    ...(detail.resumedFrom === undefined ? {} : { resumedFrom: detail.resumedFrom }),
// The port only gives this field when it is below the ceiling (there is nothing to say about run running on the ceiling), so forward it as it is here.
// Already "nothing means absence".
    ...(detail.maxConcurrency === undefined ? {} : { maxConcurrency: detail.maxConcurrency }),
//Same as "nothing means absence": there is nothing to say about run on the session model. Omit `subagent_model` once
// AmendWorkflow inherits this string, so it must be readable on the model surface.
    ...(detail.subagentModel === undefined ? {} : { subagentModel: detail.subagentModel }),
// Same as "nothing means absence": there is nothing to say about run without script file. The port is given as an absolute path (part of the run identity),
// The model side is notated relative to the workspace - it will then edit the file, and that's the same path it uses elsewhere.
    ...(detail.scriptPath === undefined
      ? {}
      : { scriptPath: describeWorkflowScriptPath(detail.scriptPath, context.workingDirectory) }),
    ...(detail.supersededBy === undefined ? {} : { supersededBy: detail.supersededBy }),
    ownedByThisSession: detail.ownedByThisSession,
    ...(detail.possiblyInterrupted ? { possiblyInterrupted: true } : {}),
    createdAt: detail.createdAt,
    updatedAt: detail.updatedAt,
    generatedAt,
    usage: {
      spentTokens: detail.usage.spentTokens,
      nodesObserved: detail.usage.nodesObserved,
      nodesRunning: detail.usage.nodesRunning,
      nodesCompleted: detail.usage.nodesCompleted,
      nodesFailed: detail.usage.nodesFailed,
    },
    actors: detail.actors.map((actor) => ({
      siteId: actor.siteId,
      ordinal: actor.ordinal,
      ...(actor.name === undefined ? {} : { name: actor.name }),
    })),
    logTail: detail.logTail.map((entry) => ({
      sequence: entry.sequence,
      message: entry.message,
// The time when the event was dropped into the database; if there is no old journal with this column, such rows will not have the age prefix.
      ...(entry.at === undefined ? {} : { at: entry.at }),
    })),
// Situation Section (Phase/Roster/Health): Turn those counts above into a "where is this run, who is doing what,
// Is it still moving?" report. Move get-workflow-run-roster-output.ts field by field.
    ...(phases === undefined ? {} : { phases }),
    subagents: roster.subagents,
    ...(roster.truncated ? { subagentsTruncated: true as const } : {}),
    health: toGetWorkflowRunHealth(detail.health),
    ...(result === undefined ? {} : { result }),
    ...(detail.error === undefined
      ? {}
      : {
          error: {
            code: detail.error.code,
            message: detail.error.message,
            ...(detail.error.providerStop === undefined
              ? {}
              : { providerStop: detail.error.providerStop }),
          },
        }),
// The zero integer field is absent (the port itself does not send an empty array, confirm again here instead of `?? []`): an empty
// The pending area reads like "asked, answered", while absence reads like "no one is waiting".
    ...(detail.pendingQuestions === undefined || detail.pendingQuestions.length === 0
      ? {}
      : {
          pendingQuestions: detail.pendingQuestions.map((pending) => ({
            qid: pending.qid,
            actor: pending.actor,
            ...(pending.actorName === undefined ? {} : { actorName: pending.actorName }),
            question: pending.question,
            ...(pending.context === undefined ? {} : { context: pending.context }),
            askedAt: pending.askedAt,
          })),
        }),
//User surface product. Part time integral field is absent; upper bound 32 and
// `ARTIFACT_CAPS.maxArtifactsPerRun` Same value - the port itself will not give more, here it is just written in
//On the model surface. `bytes` is only hung in the version item on the port, taking the latest version (the list describes the latest version).
    ...(detail.artifacts === undefined || detail.artifacts.length === 0
      ? {}
      : {
          artifacts: detail.artifacts
            .slice(0, WORKFLOW_ARTIFACTS_INTROSPECTION_MAX_LINES)
            .map((artifact) => {
              const latest = artifact.versions[artifact.versions.length - 1];
              return {
                id: artifact.id,
                kind: artifact.kind,
                ...(artifact.title === undefined ? {} : { title: artifact.title }),
                version: artifact.version,
                ...(artifact.contentType === undefined
                  ? {}
                  : { contentType: artifact.contentType }),
                ...(latest?.bytes === undefined ? {} : { bytes: latest.bytes }),
                ...(artifact.sourcePath === undefined
                  ? {}
                  : { sourcePath: artifact.sourcePath }),
                itemCount: artifact.itemCount,
                ...(artifact.primary === true ? { primary: true as const } : {}),
              };
            }),
        }),
  } satisfies Omit<GetWorkflowRunOutput, "summary">;

//Final spelling of the abstract: It reads the above fields, so there are facts first, then the sentence.
  return { ...base, summary: buildWorkflowRunSummary(base) } satisfies GetWorkflowRunOutput;
};

export const getWorkflowRunToolEntry: ToolEntry = {
  capability: "Read one dynamic-workflow run's progress, final result, or failure",
  maxModelChars: GET_WORKFLOW_RUN_PERSIST_THRESHOLD_CHARS,
  metadata: {
    name: GET_WORKFLOW_RUN_TOOL_NAME,
    description: GET_WORKFLOW_RUN_DESCRIPTION,
    readOnly: true,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: GET_WORKFLOW_RUN_TIMEOUT_MS,
    maxOutputBytes: GET_WORKFLOW_RUN_RESULT_BUDGET_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
    needsApproval: false,
  },
  handler: getWorkflowRunHandler,
  inputSchema: GetWorkflowRunInputJsonSchema,
  outputSchema: GetWorkflowRunOutputJsonSchema,
  runtimeInputSchema: GetWorkflowRunInputSchema,
  runtimeOutputSchema: GetWorkflowRunOutputSchema,
  formatModelContent: formatGetWorkflowRunModelContent,
  permission: {
    permission: "getWorkflowRun",
    reason: "GetWorkflowRun reads one run's record from the project's run journal",
    riskLevel: "low",
    sideEffectScope: "none",
    needsApproval: false,
// run_id enters the pattern matching surface (according to task_id of TaskOutput), so that project rules can be constrained to specific runs.
    patternSources: ["toolName", "input"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
  },
  resultBudget: {
    maxInlineBytes: GET_WORKFLOW_RUN_RESULT_BUDGET_BYTES,
    maxModelBytes: GET_WORKFLOW_RUN_RESULT_BUDGET_BYTES,
    strategy: "artifact",
    preview: {
      maxBytes: GET_WORKFLOW_RUN_RESULT_BUDGET_BYTES,
      direction: "head",
    },
    artifact: {
      enabled: true,
      retention: "session",
    },
  },
  resultArtifactContentType: "text/plain",
  timeout: {
    kind: "timed",
    defaultMs: GET_WORKFLOW_RUN_TIMEOUT_MS,
    maxMs: GET_WORKFLOW_RUN_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: false,
    cleanup: "none",
    userVisibleMessage: "GetWorkflowRun reads the run journal synchronously and cannot be cancelled",
  },
  trace: {
    required: true,
    propagateToAdapters: false,
    recordInput: "summary",
    recordOutput: "summary",
  },
};
