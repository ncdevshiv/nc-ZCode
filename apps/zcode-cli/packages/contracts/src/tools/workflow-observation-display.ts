import { z } from "zod";

import {
  CREATE_WORKFLOW_DISPLAY_MAX_DIAGNOSTICS,
  createWorkflowToolResultDisplayDiagnosticSchema,
} from "./create-workflow.js";
import {
  GET_WORKFLOW_RUN_ROSTER_LIMITS,
  GET_WORKFLOW_RUN_SUMMARY_MAX_CHARS,
  GetWorkflowRunActorSchema,
  GetWorkflowRunHealthSchema,
  GetWorkflowRunPhaseSchema,
  GetWorkflowRunSubagentLastToolSchema,
  GetWorkflowRunUsageSchema,
} from "./get-workflow-run.js";
import {
  ListWorkflowRunsRunSchema,
  WORKFLOW_RUN_LIFECYCLE_STATUSES,
  WORKFLOW_RUN_STOP_REASONS,
} from "./list-workflow-runs.js";

/**
* Observation workflow tools (GetWorkflowRun/ListWorkflowRuns/EvalWorkflowSnippet/
* ListSavedWorkflows / ListModels) result card display payload.
 *
* The limited length constant corresponds to the construction side (core's workflow-observation-display.ts) one-to-one: display does not pass
* result budget, text fields and array lengths must be limited independently before entering real-time events and persistence metadata,
* Overruns are marked truncated by the construction side.
 */
export const WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_ENTRIES = 40;
export const WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_CHARS = 1_024;
export const WORKFLOW_OBSERVATION_DISPLAY_MAX_ACTORS = 32;
export const WORKFLOW_OBSERVATION_DISPLAY_MAX_RESULT_CHARS = 4_000;
export const WORKFLOW_OBSERVATION_DISPLAY_MAX_META_CHARS = 2_048;
export const WORKFLOW_OBSERVATION_DISPLAY_MAX_RUNS = 50;
export const WORKFLOW_OBSERVATION_DISPLAY_MAX_ARGS = 32;
/** The directory card can draw a maximum of several rows of models at a time (beyond the constructed side slice and truncated). */
export const WORKFLOW_OBSERVATION_DISPLAY_MAX_MODELS = 100;
/** The limit of the form section on the card is the same as the east side ({@link GET_WORKFLOW_RUN_ROSTER_LIMITS}). */
export const WORKFLOW_OBSERVATION_DISPLAY_MAX_PHASES = GET_WORKFLOW_RUN_ROSTER_LIMITS.maxPhases;
export const WORKFLOW_OBSERVATION_DISPLAY_MAX_SUBAGENTS =
  GET_WORKFLOW_RUN_ROSTER_LIMITS.maxSubagents;

/**
* A subagent on the card. Deliberately **flat row** rather than the nested shape of the tool output: the card draws a row,
* The few things in `currentAsk` (what it was sent to do, which round it was, and which tool was touched in the end) are the second half of this line.
* One more layer of nesting will only cause the rendering side to deconstruct and then put it back together.
 *
* The enumeration set is literally the same as the tool surface and is closed: the consuming bundle will reject unknown enumeration values ​​(this once caused an entire
*Subscription expires), so these two places will always be changed together.
 */
export const getWorkflowRunToolResultDisplaySubagentSchema = z
  .object({
    siteId: z.string().min(1),
    ordinal: z.number().int().nonnegative(),
    name: z.string().max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxActorNameLength).optional(),
    state: z.enum(["idle", "executing", "waiting", "parked", "done", "failed", "unfinished"]),
    phaseName: z.string().max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxPhaseNameLength).optional(),
/** Task summary and progress readout for the current ask (absent = unknown, never 0). */
    instructionsHead: z
      .string()
      .max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxInstructionsHeadLength)
      .optional(),
    startedAt: z.number().optional(),
    turn: z.number().int().nonnegative().optional(),
    toolCalls: z.number().int().nonnegative().optional(),
    lastTool: GetWorkflowRunSubagentLastToolSchema.optional(),
/** What are you waiting for (the reason text is not on the card: the card only needs to be "waiting for a slot" or "retreating" and how long you have to wait). */
    waitCause: z.enum(["slot", "backoff"]).optional(),
    retryAfterMs: z.number().nonnegative().optional(),
    waitSince: z.number().optional(),
    parkedOn: z.string().optional(),
    stepsSettled: z.number().int().nonnegative(),
    stepsFailed: z.number().int().nonnegative(),
    tokens: z.number().int().nonnegative(),
    lastProgressAt: z.number().optional(),
  })
  .strict();

export const getWorkflowRunToolResultDisplayLogEntrySchema = z
  .object({
    sequence: z.number(),
    message: z.string().max(WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_CHARS),
// The time when the event falls into the journal (epoch ms) has the same origin as the `logTail[].at` output by the tool; the age on the card is consistent
// `generatedAt` counts. Optional: Loads before the section are not available without it, synchronization with the shared side is optional.
    at: z.number().optional(),
  })
  .strict();

/**
* The structuring on the tool card failed: **Only code and message**, deliberately not reusing the tool output
 * `GetWorkflowRunErrorSchema`。
 *
* The readers of the two channels are different: the output is the model reading, it needs `providerStop` to know what to fix; display is
* The rendering end is reading, and the rendering end uses the packages/shared mirror schema to strictly verify each frame. Reuse equals to letting
* Display follows the output of long fields. If the mirror does not follow, the entire row will be rejected and the session recovery will fail-closed.
* This definition is the opposite side of that mirror, and both sides are synchronized field by field.
 */
export const getWorkflowRunToolResultDisplayErrorSchema = z
  .object({
    code: z.string(),
    message: z.string(),
  })
  .strict();

/**
* Scorecard payload for GetWorkflowRun. Budget / actor direct reuse tool output schema (pure decimal value or bounded
* Small structure); the mistake is to use the display-specific schema above; logTail and result have independent length limits on the display side——
* The 2048 characters/bar on the output side and the upper limit of artifact serialization belong to the model channel, and the two boundaries do not replace each other.
 */
export const getWorkflowRunToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("get_workflow_run"),
    runId: z.string().min(1),
    label: z.string(),
    status: z.enum(WORKFLOW_RUN_LIFECYCLE_STATUSES),
/** Only present if `status === "stopped"` is present. */
    stopReason: z.enum(WORKFLOW_RUN_STOP_REASONS).optional(),
    possiblyInterrupted: z.boolean().optional(),
    /**
* Situation cross-section: that one-sentence summary, snapshot time, phase table, roster, health.
     *
* The five fields are **all optional**, although the construction side is filled in every time. The reason is **already lying in Curry transcript**:
* The `get_workflow_run` payload that was persisted before the schema section came online does not have these keys, while the display schema is
* Strict - setting them as required is equivalent to failing the entire verification of this card in every historical session opened after the upgrade.
* Degenerate into plain text. The text dump only applies to old lines that have no load at all, not those that have a load.
     *
* `generatedAt` is the baseline for all "how long ago" on the card: it must be calculated for the snapshot time, not for the rendering time
* `Date.now()`——When a transcript three days ago is reopened, the age on the card will still be the age at that time.
     */
    summary: z.string().max(GET_WORKFLOW_RUN_SUMMARY_MAX_CHARS).optional(),
    generatedAt: z.number().optional(),
    usage: GetWorkflowRunUsageSchema,
    phases: z
      .array(GetWorkflowRunPhaseSchema)
      .max(WORKFLOW_OBSERVATION_DISPLAY_MAX_PHASES)
      .optional(),
    subagents: z
      .array(getWorkflowRunToolResultDisplaySubagentSchema)
      .max(WORKFLOW_OBSERVATION_DISPLAY_MAX_SUBAGENTS)
      .optional(),
    health: GetWorkflowRunHealthSchema.optional(),
    actors: z.array(GetWorkflowRunActorSchema).max(WORKFLOW_OBSERVATION_DISPLAY_MAX_ACTORS),
    logTail: z
      .array(getWorkflowRunToolResultDisplayLogEntrySchema)
      .max(WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_ENTRIES),
    result: z.string().max(WORKFLOW_OBSERVATION_DISPLAY_MAX_RESULT_CHARS).optional(),
    error: getWorkflowRunToolResultDisplayErrorSchema.optional(),
    truncated: z.boolean().optional(),
  })
  .strict();

/** The result card load of ListWorkflowRuns: the run line directly reuses the output schema (limit is capped at 50). */
export const listWorkflowRunsToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("list_workflow_runs"),
    runs: z.array(ListWorkflowRunsRunSchema).max(WORKFLOW_OBSERVATION_DISPLAY_MAX_RUNS),
    truncated: z.boolean().optional(),
  })
  .strict();

/**
* Scorecard payload for EvalWorkflowSnippet. Diagnosis reuse the display diagnostic schema of create_workflow
* (Same TS diagnosis shape, same limit length); logs takes the tail, response has independent limit length.
 */
export const evalWorkflowSnippetToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("eval_workflow_snippet"),
    ok: z.boolean(),
    diagnostics: z
      .array(createWorkflowToolResultDisplayDiagnosticSchema)
      .max(CREATE_WORKFLOW_DISPLAY_MAX_DIAGNOSTICS),
    logs: z
      .array(z.string().max(WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_CHARS))
      .max(WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_ENTRIES),
    response: z.string().max(WORKFLOW_OBSERVATION_DISPLAY_MAX_RESULT_CHARS),
    durationMs: z.number().int().nonnegative(),
    truncated: z.boolean().optional(),
  })
  .strict();

/**
* Scorecard payload for ListSavedWorkflows. Root cause: output.text on v4 wire is formatModelContent
* XML-style projection, UI side JSON probe never hits (falls into raw JSON trap) - structured list must go
* This display channel. description / whenToUse has independent length limit; args only retains the name (declaration details belong to the confirmation window).
 */
export const savedWorkflowListToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("saved_workflow_list"),
    workflows: z
      .array(
        z
          .object({
            name: z.string().min(1),
            description: z.string().max(WORKFLOW_OBSERVATION_DISPLAY_MAX_META_CHARS).optional(),
            whenToUse: z.string().max(WORKFLOW_OBSERVATION_DISPLAY_MAX_META_CHARS).optional(),
            scope: z.string(),
            path: z.string().min(1),
            argNames: z.array(z.string()).max(WORKFLOW_OBSERVATION_DISPLAY_MAX_ARGS),
          })
          .strict(),
      )
      .max(WORKFLOW_OBSERVATION_DISPLAY_MAX_RUNS),
    invalid: z
      .array(
        z
          .object({
            path: z.string().min(1),
            reason: z.string().max(WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_CHARS).optional(),
          })
          .strict(),
      )
      .optional(),
    truncated: z.boolean().optional(),
  })
  .strict();

/**
* Scorecard payload for ListModels. The root cause is the same as saved_workflow_list: output.text on v4 wire is
* For the `<models>` projection of formatModelContent, the UI side JSON probe never hits - structured directories can only go through this channel.
 *
* The field is the tool output itself (the three things to be drawn on the reading side "what are they, where are they from, and which one is current" are all in it),
* One more truncated. providerLabel / disabledReason is free text from the registry, display but result
* budget, the length must be limited here independently; the rest are short ids and decimal values, which are transparently transmitted as they are.
 */
export const listModelsToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("list_models"),
/** The model at the moment of the session (canonical form); absent when none of the entries in the directory match. */
    current: z.string().optional(),
    models: z
      .array(
        z
          .object({
            id: z.string().min(1),
            providerId: z.string().min(1),
            modelId: z.string().min(1),
            providerLabel: z.string().max(WORKFLOW_OBSERVATION_DISPLAY_MAX_META_CHARS).optional(),
            reasoningLevels: z.array(z.string()),
            defaultReasoningLevel: z.string().optional(),
            contextWindow: z.number().optional(),
            disabledReason: z.string().max(WORKFLOW_OBSERVATION_DISPLAY_MAX_META_CHARS).optional(),
          })
          .strict(),
      )
      .max(WORKFLOW_OBSERVATION_DISPLAY_MAX_MODELS),
    truncated: z.boolean().optional(),
  })
  .strict();

export type GetWorkflowRunToolResultDisplayPayload = z.infer<
  typeof getWorkflowRunToolResultDisplayPayloadSchema
>;
export type ListWorkflowRunsToolResultDisplayPayload = z.infer<
  typeof listWorkflowRunsToolResultDisplayPayloadSchema
>;
export type EvalWorkflowSnippetToolResultDisplayPayload = z.infer<
  typeof evalWorkflowSnippetToolResultDisplayPayloadSchema
>;
export type SavedWorkflowListToolResultDisplayPayload = z.infer<
  typeof savedWorkflowListToolResultDisplayPayloadSchema
>;
export type ListModelsToolResultDisplayPayload = z.infer<
  typeof listModelsToolResultDisplayPayloadSchema
>;

/**
* Results card payload for ResumeWorkflowRun.
 *
* Deliberately minimal `{runId}`: The information to be conveyed by the recovery card is "which run continues in the background" - the response guide copy is
* In terms of model channel, the UI has its own localized vocabulary, and multi-band fields only add wire bytes without consumers. structural side
* If safeParse fails to output schema, it will return undefined (take the text out of the box), which has the same skeleton as the above four constructors.
 */
export const resumeWorkflowRunToolResultDisplayPayloadSchema = z
  .object({
    kind: z.literal("resume_workflow_run"),
    runId: z.string().min(1),
  })
  .strict();
export type ResumeWorkflowRunToolResultDisplayPayload = z.infer<
  typeof resumeWorkflowRunToolResultDisplayPayloadSchema
>;
