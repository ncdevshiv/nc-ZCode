// ============================================================
// GetWorkflowRun's **schema cross-section** schema: Stage/Subagent/Health
// ============================================================
// Detach from get-workflow-run.ts for the same reason as dynamic-workflow-run-roster.port.ts on the port side:
// That document is close to the max-lines limit of oxlint, and these three sets of schemas are self-contained. The public side remains unchanged——
// get-workflow-run.ts exports each name here in place, and the import path of `@zcode/contracts` remains unchanged.
//
//Here is a **field-by-field mirror** of the port type (DynamicWorkflowRunPhaseView/…SubagentView/…Health),
// Just more zod boundaries. Both sides must be synchronized: the port is the fact of the read side, here is the contract of the model side, missing a field means
// A fact that has been found cannot reach the model.

import { z } from "zod";

/**
* The boundary of the situation section. Each one has the same value as an existing one, no new set is required:
* The number of phases and phase names depend on the reducer's `WORKFLOW_RUNS_LIMITS.maxPhases` / `maxPhaseNameLength`,
* The command header and tool name/target follow the engine-side `INSTRUCTIONS_HEAD_MAX_CHARS` / `LAST_TOOL_TARGET_MAX_CHARS`.
 */
export const GET_WORKFLOW_RUN_ROSTER_LIMITS = {
/** Upper bound on the number of rows in the stage table (same value as reducer's maxPhases). */
  maxPhases: 32,
  maxPhaseNameLength: 128,
  /**
* Upper bound on the number of lines in the roster. Deliberately above the stage: a 50-way fan-out is commonplace, and this is exactly what readers are asking
* "Who is doing what?" When this boundary is exceeded, the entire block is cut, and `subagentsTruncated` indicates that it has been cut——
* A roster that silently loses 18 lines reads like "only 64 subagents".
   */
  maxSubagents: 64,
  maxActorNameLength: 128,
/** Task summary for an ask (`node-queued`’s `instructionsHead`). */
  maxInstructionsHeadLength: 240,
  maxLastToolNameLength: 64,
  maxLastToolTargetLength: 120,
/** Free text reason for `node-waiting` (wrong provider sentence in backoff). */
  maxWaitReasonLength: 240,
} as const;

/** The position of a phase in the status section (mirror of port `DynamicWorkflowRunPhaseView`). */
export const GetWorkflowRunPhaseSchema = z
  .object({
    name: z.string().min(1).max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxPhaseNameLength),
/** `ahead` = The script declares it but the control flow has not yet arrived, and the only state is `rounds: 0`. */
    state: z.enum(["done", "current", "ahead", "unfinished"]),
    rounds: z.number().int().nonnegative(),
    nodesSettled: z.number().int().nonnegative(),
    nodesRunning: z.number().int().nonnegative(),
/** The latest entry/exit time (epoch ms). Absent when the event has no timestamp, never give 0. */
    enteredAt: z.number().optional(),
    exitedAt: z.number().optional(),
  })
  .strict();

/** The most recently observed tool call in an ask. `target` is a clue (path/command header), not the full text of the input parameter. */
export const GetWorkflowRunSubagentLastToolSchema = z
  .object({
    name: z.string().min(1).max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxLastToolNameLength),
    target: z.string().max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxLastToolTargetLength).optional(),
    at: z.number().optional(),
  })
  .strict();

/**
* The ask that the subagent is running at the moment. `turn` / `toolCalls` **Absent is read as "don't know"**, `0` is read as
* "Not a single tool has been adjusted" - the old journal does not have `node-progress`, the two must be distinguishable.
 */
export const GetWorkflowRunSubagentAskSchema = z
  .object({
    siteId: z.string().min(1),
    ordinal: z.number().int().nonnegative(),
    actorSeq: z.number().int().nonnegative().optional(),
    instructionsHead: z
      .string()
      .max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxInstructionsHeadLength)
      .optional(),
    startedAt: z.number().optional(),
    turn: z.number().int().nonnegative().optional(),
    toolCalls: z.number().int().nonnegative().optional(),
    lastTool: GetWorkflowRunSubagentLastToolSchema.optional(),
  })
  .strict();

/** What ask is currently waiting for. `slot` = wait for the process-level admission gate; `backoff` = runner will retry after backing off. */
export const GetWorkflowRunSubagentWaitSchema = z
  .object({
    cause: z.enum(["slot", "backoff"]),
    reason: z.string().max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxWaitReasonLength).optional(),
    retryAfterMs: z.number().nonnegative().optional(),
    since: z.number().optional(),
  })
  .strict();

/**
* A subagent in the roster (mirror of port `DynamicWorkflowRunSubagentView`).
 *
* The seven words of `state` are closed sets, and the order of reading is the order of writing (the port annotation has a complete decision chain): when run is alive
* `parked` → `waiting` → `executing` → `failed` → `idle`, only the final state of run
 * `unfinished` → `failed` → `done`。
 */
export const GetWorkflowRunSubagentSchema = z
  .object({
    siteId: z.string().min(1),
    ordinal: z.number().int().nonnegative(),
    name: z.string().max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxActorNameLength).optional(),
    state: z.enum(["idle", "executing", "waiting", "parked", "done", "failed", "unfinished"]),
    phaseName: z.string().max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxPhaseNameLength).optional(),
    currentAsk: GetWorkflowRunSubagentAskSchema.optional(),
    wait: GetWorkflowRunSubagentWaitSchema.optional(),
/** The question it stops at; the read where `health.pendingQuestionsKnown` is false is never present. */
    parkedOn: z.string().optional(),
    stepsSettled: z.number().int().nonnegative(),
    stepsFailed: z.number().int().nonnegative(),
    tokens: z.number().int().nonnegative(),
    lastProgressAt: z.number().optional(),
  })
  .strict();

/**
* Run level and find the status. **The entire object is only present when it is pressed below its own boundary**: There is no run that fills its own boundary.
* can be said, and its presence means "being limited."
 */
export const GetWorkflowRunConcurrencyHealthSchema = z
  .object({
    effective: z.number().int().nonnegative(),
    cap: z.number().int().positive(),
    reason: z.string().max(GET_WORKFLOW_RUN_ROSTER_LIMITS.maxWaitReasonLength).optional(),
    since: z.number().optional(),
  })
  .strict();

/** The run is still running as a whole (mirror of port `DynamicWorkflowRunHealth`). */
export const GetWorkflowRunHealthSchema = z
  .object({
    lastProgressAt: z.number().optional(),
    stalledSince: z.number().optional(),
    concurrency: GetWorkflowRunConcurrencyHealthSchema.optional(),
    consecutiveFailures: z.number().int().nonnegative(),
    cachedSteps: z.number().int().nonnegative(),
/** Final state run only: Number of node lines also marked `running` (processes die under them). Absent when 0. */
    leftoverRunning: z.number().int().positive().optional(),
    /**
* Can you answer "Are there any questions waiting for answers" during this reading? When false, the entire `pendingQuestions` field is absent,
* And no sub-agent will be reported as `parked` - "no one is waiting" and "don't know" are two different facts.
     */
    pendingQuestionsKnown: z.boolean(),
  })
  .strict();

export type GetWorkflowRunPhase = z.infer<typeof GetWorkflowRunPhaseSchema>;
export type GetWorkflowRunSubagent = z.infer<typeof GetWorkflowRunSubagentSchema>;
export type GetWorkflowRunSubagentAsk = z.infer<typeof GetWorkflowRunSubagentAskSchema>;
export type GetWorkflowRunHealth = z.infer<typeof GetWorkflowRunHealthSchema>;
