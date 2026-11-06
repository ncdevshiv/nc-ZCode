// ============================================================
// AmendWorkflow Tool - revise a run: stop it if it is still going, import its finished work, start the revision
// ============================================================
//
// The reason for splitting into two tools with CreateWorkflow: Revision is **another action, another card** - the model is not initiating anything,
// Instead, we are changing what the user is looking at, and the precursor may still be running. Both share the same output shape, so executor's
// There is only one copy of background tracking, `backgrounded` contract and display payload.

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";
import {
  WORKFLOW_RUN_LIFECYCLE_STATUSES,
  WORKFLOW_RUN_STOP_REASONS,
} from "./list-workflow-runs.js";

export const AMEND_WORKFLOW_TOOL_NAME = "AmendWorkflow";

/**
* "At most one revised script" violation description. Same position and tone as `CREATE_WORKFLOW_SOURCE_ERROR`;
* The difference is that neither of them is **legal** here - that is "following the predecessor's script".
 */
export const AMEND_WORKFLOW_SOURCE_ERROR =
  "Provide at most one revised script: `path` for the script file you edited (the usual form), or `script` for the whole revised script inline. Passing both is ambiguous; omit both to keep the predecessor's script unchanged.";

/**
* Parameters are entered on the model surface. Only `run_id` is required, and every other field follows the same rule: **If omitted, the predecessor will be used**. `predecessor` is not here - that is
* resolveInput backfills the fact (below) that the model's JSON schema does not list it.
 */
const AmendWorkflowModelInputSchema = z.object({
  run_id: z
    .string()
    .min(1)
    .describe(
      "ID of the run to amend — from CreateWorkflow's or AmendWorkflow's result, a notification, GetWorkflowRun or ListWorkflowRuns. Any run of this project qualifies, settled or still running; a running run is stopped and superseded by the new one.",
    ),
  /**
* Both `script` and `path` are omitted = the script of the predecessor archive is used:
* There is no need to copy thousands of token scripts again when only changing settings. Same life cycle as the two settings - `resolveInput` read predecessor
* The script is backfilled in and stamped with `predecessor.script_inherited`. After that, the hook, confirmation window and handler all face the same copy.
* Script. The runtime schema must still accept the absence: after the hook is rewritten, the call-runner will verify it twice according to the original shape of the model.
   */
  script: z
    .string()
    .min(1)
    .optional()
    .describe(
      "The WHOLE revised workflow script, inline, written against the same facade as CreateWorkflow. Provide this OR `path`, never both — `path` is the usual form. OMIT both to keep the predecessor's script unchanged and change only the settings (max_concurrency, subagent_model, name). Named subagents whose asks you left byte-identical settle from the predecessor's recorded results at zero token cost; the first changed or new ask runs live, and from that point everything runs live.",
    ),
  /**
* The normal source of revision: the precursor script file changes one line in place,
* Hand the same path back again. Inlining still closes, but it requires streaming the entire script over again - and that's what this path eliminates.
   */
  path: z
    .string()
    .min(1)
    .optional()
    .describe(
      "The revised script's file, relative to the working directory or absolute — usually the predecessor's own script file, which the errored notification and GetWorkflowRun name. Provide this OR `script`, never both; omit both when the script is not changing. Edit that file in place and pass the same path back: a revision then costs one Edit instead of a second copy of the whole script. Submitting a file whose bytes are unchanged is refused (`script_unchanged`) unless the call also changes `max_concurrency` or `subagent_model`.",
    ),
  name: z
    .string()
    .min(1)
    .optional()
    .describe("Optional display label for the new run. Defaults to the predecessor's name."),
  /**
* Three states: omitted = use the upper bound of the predecessor,
* `null` = release (return to the ceiling), number = set (clamp under the ceiling). Tristate only lives up to `resolveInput`: there put it
* Normalized to a number or absent, the value read by the confirmation window and handler is the value that will take effect. Runtime schema remains `nullable`
* This is because after the hook is rewritten, the call-runner will verify the original shape of the model twice.
   */
  max_concurrency: z
    .number()
    .int()
    .positive()
    .nullable()
    .optional()
    .describe(
      "Upper bound on how many subagents work at the same time in the new run. Three states: OMIT the field to keep the predecessor's limit; pass null to remove that limit (run at this machine's default); pass a number to set one. Set a number ONLY when the user asks to limit parallelism — never on your own initiative and never in response to provider rate limits or errors, which the runtime already adapts to. A value above what this machine allows is lowered to that maximum.",
    ),
  /**
* Three-state, literally the same as `max_concurrency`:
* Omitted = Inherit the selection of the predecessor (`resolveInput` **re-parse** after backfilling from the predecessor snapshot, so that one has been
* Deleted models now fail instead of being dragged to the first opening of the subagent), `null` = clear (return to session model),
* string = setting. Tristate only lives up to `resolveInput`: where it is normalized to a canonical string or absent.
   */
  subagent_model: z
    .string()
    .trim()
    .min(1)
    .nullable()
    .optional()
    .describe(
      "Model for the workflow's subagents in the new run, as `providerId/modelId` or a bare model id (optionally `$reasoningLevel`). Set it ONLY when the user asks for the subagents to run on a specific model; pass the name the user used, and if the tool answers that it cannot resolve it, pick from the listed ids or call ListModels. The main agent (you) keeps the session model regardless. Three states: OMIT to keep the predecessor's choice; null to return to the session model; a string to set one.",
    ),
});

/**
* resolveInput backfills the predecessor fact (step 2).
 *
* Both the permission determination (no confirmation required for run of this session) and the confirmation window ("still running, will be stopped") read it, and both places are
* Before the handler, and must be synchronized - so resolveInput (asynchronous, the only read port in the whole process) is calculated and placed into the input parameters.
* Normalized **Unconditional Coverage**: Model fake it is invalid (same precedent for saved.path).
 */
export const AmendWorkflowPredecessorSchema = z
  .object({
    name: z.string().min(1).optional(),
    status: z.enum(WORKFLOW_RUN_LIFECYCLE_STATUSES),
    stop_reason: z.enum(WORKFLOW_RUN_STOP_REASONS).optional(),
    owned_by_this_session: z.boolean(),
    /**
* Neither `script` nor `path` is given in this call. The `script` in the input parameter is read from the predecessor archive by resolveInput. There is only one value: "presence is true":
* According to the confirmation window, it says "script unchanged" in the lineage line, and the handler changes it to the copy and diagnostic first sentence of "inherit". together with the whole piece
* is overridden unconditionally, and model forgery is invalid.
     */
    script_inherited: z.literal(true).optional(),
  })
  .strict();

export type AmendWorkflowPredecessor = z.infer<typeof AmendWorkflowPredecessorSchema>;

/**
* Runtime input parameters: those keys in the model + backfilled `predecessor` and `script_line_offset`. `.strict()`:
* Old spellings like `resume_from` are also visible errors here.
 */
export const AmendWorkflowInputSchema = AmendWorkflowModelInputSchema.extend({
  predecessor: AmendWorkflowPredecessorSchema.optional(),
  /**
* Text line → Offset of file line, non-zero only if `path` file has `/* zcode-workflow` block. with `predecessor`
* Same posture: The parsing result is not a fillable parameter, and the model's JSON schema does not list it.
   */
  script_line_offset: z.number().int().nonnegative().optional(),
}).strict();

export type AmendWorkflowInput = z.infer<typeof AmendWorkflowInputSchema>;

/** JSON schema handed to the model: does not contain `predecessor`, nor `script_line_offset`. */
export const AmendWorkflowInputJsonSchema = toToolJsonSchema(
  AmendWorkflowModelInputSchema.strict(),
);
