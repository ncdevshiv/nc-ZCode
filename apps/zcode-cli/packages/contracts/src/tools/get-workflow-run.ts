// ============================================================
// GetWorkflowRun Tool - Adaptation details for a single workflow run (Progress Summary/Product/Failure)
// ============================================================

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";
import {
  GET_WORKFLOW_RUN_ROSTER_LIMITS,
  GetWorkflowRunHealthSchema,
  GetWorkflowRunPhaseSchema,
  GetWorkflowRunSubagentSchema,
} from "./get-workflow-run-roster.js";
import { WorkflowRunSummarySchema } from "./list-workflow-runs.js";

//The schema of the situation section (stage/subagent/health) lives in get-workflow-run-roster.ts, and is exported here as it is.
// To keep the import path of `@zcode/contracts` unchanged.
export * from "./get-workflow-run-roster.js";

export const GET_WORKFLOW_RUN_TOOL_NAME = "GetWorkflowRun";

export const GetWorkflowRunInputSchema = z
  .object({
// snake_case follows TaskOutput's `task_id`: in the eyes of the model, these two keys are run/task identifiers of the same family.
// Forking the naming style will just make it guess between the two tools.
    run_id: z
      .string()
      .min(1)
      .describe("The workflow run ID to inspect (as returned by CreateWorkflow or ListWorkflowRuns)"),
  })
  .strict();

export type GetWorkflowRunInput = z.infer<typeof GetWorkflowRunInputSchema>;

export const GetWorkflowRunInputJsonSchema = toToolJsonSchema(GetWorkflowRunInputSchema);

/**
* The progress and usage of run (observation surface, without any upper limit). `nodesObserved` is the number of rows of dropped nodes (sum of three states),
* Never pretend to be "total number of steps": there is no static total number in dynamic workflow, and `queued` only exists in the event phase and does not fall into the library.
 */
export const GetWorkflowRunUsageSchema = z
  .object({
    spentTokens: z.number(),
    nodesObserved: z.number(),
    nodesRunning: z.number(),
    nodesCompleted: z.number(),
    nodesFailed: z.number(),
  })
  .strict();

/** An actor site instance. `persona` is not intentional: the entire system prompt is a natural unbounded field. */
export const GetWorkflowRunActorSchema = z
  .object({
    siteId: z.string(),
    ordinal: z.number(),
    name: z.string().optional(),
  })
  .strict();

/**
* A `log()` narrative. `at` is the time when this event was written into the journal (`dwf_event.time_created`), and the model is based on it
* Render "how long ago"; it is absent from old journals that do not have this column, and such rows do not have the age prefix - **never** use the read-time
* `Date.now()`, to be honest, that would mark the entire narrative from a week ago as "just now".
 */
export const GetWorkflowRunLogEntrySchema = z
  .object({
    sequence: z.number(),
    message: z.string(),
    at: z.number().optional(),
  })
  .strict();

/**
* An escalation question that is parked at this moment, waiting for an answer from the main agent.
 *
* This is the **query bottom line** after the upgrade notification is discarded, and it is also the only discovery surface on the model side: `ResolveWorkflowQuestion` only recognizes
* `qid`, while notifications have two known drop paths (stale branch generation/shutdown). `askedAt` lets the model
* You can tell "how long this problem has been waiting for" - no timeout will take care of it.
 */
export const GetWorkflowRunPendingQuestionSchema = z
  .object({
    qid: z.string(),
/** Questioning actor, `site@ordinal` form. */
    actor: z.string(),
/** The human-readable name of the actor; anonymous actors are absent (the reader decides how to render "unnamed"). */
    actorName: z.string().optional(),
    question: z.string(),
    context: z.string().optional(),
/** Question time (epoch ms, the same as createdAt / updatedAt). */
    askedAt: z.number(),
  })
  .strict();

/**
* A **user interface product**: the script is published to the user via `artifact.*`
* The output is now placed in front of the user as a card.
 *
* ⚠ Terminology: **two different things from the `result` (the top-level return value of the script, also called artifact inside the engine) on the output of this tool
* thing**. The usage on the model side is also different: `result` is the content to be reported, and the product is to be pressed
* The title **quotes** something.
 *
* Deliberately **without `uri`**: the model cannot read the tool-artifact store. The field takes the value of **latest version** (`version` is the version
*, historical version metadata is on the UI side panel).
 */
export const GetWorkflowRunArtifactSchema = z
  .object({
    id: z.string(),
    kind: z.enum(["file", "markdown", "chart", "table", "metrics", "board"]),
/** Card title; absent when the script is not given (the default title of the facade is id). */
    title: z.string().optional(),
/** The latest version number. */
    version: z.number(),
/** MIME of content product; preset billboard is absent. */
    contentType: z.string().optional(),
/** The number of bytes of the latest version of the content product; the preset billboard is absent. */
    bytes: z.number().optional(),
/** The workspace origin of the content product (the bytes have been copied into the store, this is just "where it was"). */
    sourcePath: z.string().optional(),
/** The number of tags `report` received by the preset billboard; the content product is always 0. */
    itemCount: z.number(),
/** Run's deliverable (at most one), ranked first in the list. */
    primary: z.literal(true).optional(),
  })
  .strict();

/** Structuring failed. `code` is a stable discriminant key - the model must be able to distinguish between "the process is dead" and "the script really failed". */
export const GetWorkflowRunErrorSchema = z
  .object({
    code: z.string(),
    message: z.string(),
/** Only present when `code === "ProviderStop"``. */
    providerStop: z
      .object({
        kind: z.enum([
          "auth",
          "not_configured",
          "model_unavailable",
          "invalid_request",
          "quota",
          "other",
        ]),
        reason: z.string(),
        providerId: z.string().optional(),
        providerLabel: z.string().optional(),
        modelId: z.string().optional(),
        providerCode: z.string().optional(),
        subagent: z.string().optional(),
        subagentName: z.string().optional(),
        phase: z.string().optional(),
        rawMessage: z.string().optional(),
        resetAt: z.number().optional(),
      })
      .strict()
      .optional(),
  })
  .strict();

/** The character upper bound of `summary`. A one-sentence budget: anything beyond it is not an abstract, but another report. */
export const GET_WORKFLOW_RUN_SUMMARY_MAX_CHARS = 400;

export const GetWorkflowRunOutputSchema = WorkflowRunSummarySchema.extend({
  /**
* A word about the situation: status + how long it has been run/finished + stage position + number of steps and on-the-fly breakdown + questions to be answered + final progress.
* Deterministically spelled out by the handler from the following structured fields (no model involvement), so the same snapshot is always spelled out
* The same sentence; the upper bound {@link GET_WORKFLOW_RUN_SUMMARY_MAX_CHARS}, when exceeded, the entire sentence will be discarded.
   */
  summary: z.string().max(GET_WORKFLOW_RUN_SUMMARY_MAX_CHARS),
  /**
* The time when this snapshot is read out (epoch ms). Every "how long ago" on the model surface is calculated for it——
* Output a ruler at a time, and two ages can be compared. `formatModelContent` only accepts one parameter (core's ToolEntry
* contract), so the read clock can only occur in the handler, and the result is brought with the output; the formatter is therefore
* Pure function for `(output) => text`, which can be pinned verbatim.
   */
  generatedAt: z.number(),
  usage: GetWorkflowRunUsageSchema,
  /**
* This run's own concurrency upper bound (the number of subagents flying at the same time), **only present when it is lower than the current ceiling**: running on the ceiling
* run has nothing to say (same as "nothing means absence" in pendingQuestions). AmendWorkflow omits `max_concurrency`
* This is the value that is inherited when * - this is how the model knows what a revision will inherit.
   */
  maxConcurrency: z.number().int().positive().optional(),
  /**
* On which model the sub-agent of this run runs, the standard form is `providerId/modelId[$reasoningLevel]`, ** only when setting
*Presence**: There is nothing to say about the run that follows the session model (same as the "no or absent" of `maxConcurrency`).
* AmendWorkflow inherits this value when `subagent_model` is omitted.
   */
  subagentModel: z.string().optional(),
  /**
* The script file of this run has been written as it should be seen on the model surface** (under the session working directory is the relative path of the workspace,
* Otherwise absolute path). **Only recorded files in this run
* Always present** (same as "subagentModel"'s "absent"): projects that cannot be written in draft, projects initiated before this feature
* There is nothing to say about run. When present, it is the `path` that should be passed to `AmendWorkflow` next time.
   */
  scriptPath: z.string().optional(),
  actors: z.array(GetWorkflowRunActorSchema),
/** `log()` The tail of events, in chronological order (sequence ascending order). If there are no log events, the array will be empty. */
  logTail: z.array(GetWorkflowRunLogEntrySchema),
  /**
* Phase table: The declared phases in the declaration sequence, followed by those that have been "entered but not declared". **The script does not have a declaration phase, and there is only one
* The integer field is absent even if it has not been entered** - there is no such thing as a stage in such a run, and an empty array reads like "the stage table is empty".
   */
  phases: z.array(GetWorkflowRunPhaseSchema).max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxPhases).optional(),
  /**
* Sub-agent roster, in casting order. **Constantly present**, run without an actor is an empty array: same as `phases`
* Different, "How many subagents does this run have?" is always a question with an answer, and 0 is that answer.
   *
* Deliberately not merged with parallel `actors`: it is a constant identity table, where each item is a read-time snapshot.
   */
  subagents: z.array(GetWorkflowRunSubagentSchema).max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxSubagents),
/** The roster has been crossed by this boundary. If not, it is absent (same as `truncated` family: do not hit `false`). */
  subagentsTruncated: z.literal(true).optional(),
/** run Is the whole thing still moving? Always present. */
  health: GetWorkflowRunHealthSchema,
  /**
* The top-level return value of the script, **serialized to text** (string as is, rest pretty JSON). Only completed
* Run is only present; `undefined` means that the integer field is absent.
   *
* The reason for collecting string instead of original value: model-oriented serialization has the only implementation in core
* (`serializeWorkflowArtifact`, completion notification shares it with TaskOutput). Releasing the original value is equivalent to allowing "the same
* run products look different in notifications than in this tool."
   */
  result: z.string().optional(),
/** errored is always present; stopped is only present for provider / interrupted. The code is exposed as it is, not folded. */
  error: GetWorkflowRunErrorSchema.optional(),
  /**
* Escalation questions for which answers are not yet available at this time, in the order in which they are asked. **The integer field is absent for zero entries** (same as port projection, no empty array is sent):
* Based on this, the reading side makes the entire pending area disappear instead of rendering an empty section.
   */
  pendingQuestions: z.array(GetWorkflowRunPendingQuestionSchema).optional(),
  /**
* User interface products that have been released by this run, in the order of first release, **are attached to any status** (a run that is still running may also
*First image has been delivered). **The part time field is absent**, the same as pendingQuestions. Bounded 32
* (= `ARTIFACT_CAPS.maxArtifactsPerRun`, the maximum number of ids a run can have).
   */
  artifacts: z.array(GetWorkflowRunArtifactSchema).optional(),
}).strict();

export type GetWorkflowRunOutput = z.infer<typeof GetWorkflowRunOutputSchema>;

export const GetWorkflowRunOutputJsonSchema = toToolJsonSchema(GetWorkflowRunOutputSchema);
