// ============================================================
// AmendWorkflow: confirm everything before (resolveInput fails with its structuring)
// ============================================================
//
// Take it out from amend-workflow.ts: there are handler and tool declarations, here is the only read port in the whole process - put the model
// The input parameters are normalized to "the execution fact that will occur". Each omitted field follows the same rule (if omitted, the predecessor will be used). All three are here.
//Settled: script (three sources, see amend-workflow-source.ts), concurrency upper bound, sub-agent model. Afterwards hook, permission,
// The confirmation window and handler face only "a script, a number or nothing, a specification form or nothing".

import {
  AmendWorkflowInputSchema,
  type AmendWorkflowInput,
  type AmendWorkflowPredecessor,
  type DynamicWorkflowRunSnapshot,
  type ModelCatalogPort,
} from "@zcode/contracts";
import type {
  ToolHandlerFailure,
  ToolInputResolutionContext,
  ToolInputResolutionResult,
} from "../types.js";
import {
  SUBAGENT_MODEL_UNAVAILABLE,
  clampWorkflowMaxConcurrency,
} from "./create-workflow-source.js";
import {
  AMEND_WORKFLOW_ERROR_CODE,
  refuseUnchangedScript,
  resolveAmendScript,
} from "./amend-workflow-source.js";
import { resolveModelReference } from "./model-reference.js";
import { workflowRunNotFoundFailure } from "./workflow-run-introspection.js";

export {
  AMEND_WORKFLOW_ERROR_CODE,
  scriptUnavailableFailure,
  validateAmendWorkflowSource,
} from "./amend-workflow-source.js";

/** The predecessor does not exist: reuse the `run_not_found` of the introspection tool, and only add a line to name `run_id`. */
export function predecessorNotFoundFailure(runId: string): ToolHandlerFailure {
  const base = workflowRunNotFoundFailure(runId);
  return {
    ...base,
    message: `${base.message} Nothing was stopped or created: \`run_id\` pointed at a run that does not exist — pass an existing run's ID (see ListWorkflowRuns), or start a fresh run with CreateWorkflow.`,
  };
}

/**
* The only read port in the whole process: parse `run_id` into `predecessor` and backfill the entry parameter in the fact block.
 *
* Permission determination (confirmation-free run of this session, permission/service.ts) and confirmation window ("still running, will be stopped") are both
* Read it, and both places are before the handler and must be synchronized, so it can only be calculated here. **Unconditional coverage**Any given by the model
* `predecessor`: Forging it has no effect. If the precursor does not exist, stop here - not popping up a confirmation window that is destined to fail.
 *
* The order cannot be changed: predecessor search (if it does not exist, it means `run_not_found`, which has nothing to do with the script) → settle the script (read the file or read the predecessor
* Archive; cannot read the reason for roll call) → than bytes (`script_unchanged`). If you do it the other way around, one points to the non-existent run
* The call will first report an error because the file cannot be read, and the model will fix something that is not a problem at all.
 *
* When the port is absent (unwired host), if the script (inline or file) is given, it will be released as it is: the handler will go "only typecheck,
* "Do not execute" path is the same as CreateWorkflow; if the permission side cannot read `predecessor`, just ask as usual. Neither source gave
* can't be used and fails on the spot - it cannot be reduced to "just typecheck" because there is nothing compilable.
 */
export async function resolveAmendWorkflowInput(
  input: unknown,
  context: ToolInputResolutionContext,
): Promise<ToolInputResolutionResult> {
  const parsed = AmendWorkflowInputSchema.safeParse(input);
  if (!parsed.success) return { result: true, input };
  const cwd = context.workingDirectory ?? ".";
  const port = context.dynamicWorkflowRunPort;
  if (port === undefined) {
    const {
      predecessor: _forged,
      max_concurrency: requested,
      subagent_model: _model,
      script_line_offset: _offset,
      ...rest
    } = parsed.data;
    void _forged;
    void _model;
    void _offset;
// Without a port, there is neither a predecessor nor a ceiling: `null` (removal) and "inherit" both collapse into absence, and the data passes as it is.
// The normalized input parameter will always have the shape of "one number or none".
    const subagentModel = resolveAmendSubagentModel(
      parsed.data.subagent_model,
      undefined,
      context.modelCatalogPort,
    );
    if (!subagentModel.result) return subagentModel;
    const script = await resolveAmendScript({
      model: parsed.data,
      cwd,
      port,
      predecessorScriptPath: undefined,
    });
    if (!script.result) return script;
    return {
      result: true,
      input: {
        ...rest,
        ...script.fields,
        ...(typeof requested === "number" ? { max_concurrency: requested } : {}),
        ...subagentModel.field,
      },
    };
  }
  const snapshot = await port.getTask(parsed.data.run_id);
  if (snapshot === undefined) return predecessorNotFoundFailure(parsed.data.run_id);
  const subagentModel = resolveAmendSubagentModel(
    parsed.data.subagent_model,
    snapshot.subagentModel,
    context.modelCatalogPort,
  );
  if (!subagentModel.result) return subagentModel;
  const script = await resolveAmendScript({
    model: parsed.data,
    cwd,
    port,
    predecessorScriptPath: snapshot.scriptPath,
  });
  if (!script.result) return script;
  const unchanged = await refuseUnchangedScript({
    port,
    model: parsed.data,
    resolvedScript: script.fields.script,
    described: script.described,
  });
  if (unchanged !== undefined) return unchanged;
  const {
    predecessor: _forged,
    max_concurrency: _tristate,
    subagent_model: _model,
// The row offset is the same as `predecessor`: parsing results and those given by the model will be invalid (recalculated according to the file below).
    script_line_offset: _offset,
    ...rest
  } = parsed.data;
  void _forged;
  void _model;
  void _offset;
  const resolved: AmendWorkflowInput = {
    ...rest,
    ...script.fields,
    ...resolveAmendMaxConcurrency(
      parsed.data.max_concurrency,
      snapshot.maxConcurrency,
      port.concurrencyCeiling?.(),
    ),
    ...subagentModel.field,
    predecessor: {
      ...describePredecessor(snapshot, context.sessionId),
      ...(script.inherited ? { script_inherited: true as const } : {}),
    },
  };
  return { result: true, input: resolved };
}

/**
* Three-state normalization of sub-agent model, and concurrency upper bound
* The same shape convention - the three states only live up to this point. After the confirmation window and handler, they only face "a canonical shape or not":
 *
* - String → Parse; the entire call fails if it cannot be parsed (nothing is stopped, nothing is built, and the window is not opened).
* - `null` → Dismiss, the key disappears entirely (new run runs back to the session model).
* - Omitted → Use the one from the predecessor snapshot and **re-parse** it. The precursor may have started a few days ago, and that model has since
* may be deleted or disabled; without refactoring the failure will not occur until the subagent first opens, at which point it will look like a runtime failure.
 *
* When the directory is absent, the two sources are processed separately: the string photo `CreateWorkflow` given by the model itself is rejected (something that the host cannot understand)
* does not pass silently), and ** carries over the one that uses ** as it is - it has been parsed in the predecessor and is a call this time
* A field not mentioned at all that makes the entire revision fail is to pin a host wiring gap on the user's head.
 */
function resolveAmendSubagentModel(
  requested: string | null | undefined,
  inherited: string | undefined,
  catalog: ModelCatalogPort | undefined,
): { result: true; field: { subagent_model?: string } } | ToolHandlerFailure {
  const choice = resolveAmendSubagentModelChoice(requested, inherited, catalog);
  if (choice.ok) {
    return {
      result: true,
      field: choice.canonical === undefined ? {} : { subagent_model: choice.canonical },
    };
  }
// When the inherited one fails, it must be clearly stated that it is **inherited**: the model is not called at all. The model name is simply parsed.
// Throw diagnosis to it, it will think that it has passed the wrong parameters, and then try again as is.
  return subagentModelFailure(
    choice.inherited
      ? `This amend inherited the predecessor run's subagent model (\`${choice.text}\`), which is no longer usable. ${choice.message}\n\nPass \`subagent_model: null\` to run the revision on the session model instead.`
      : choice.message,
  );
}

/**
* The **determination ontology** of the three states of the subagent model, the tool is shared with the GUI "configuration" (amendWorkflowRunSettings of runtime)
* (parsing code shared, not copied). The two only failed in how to **say**
* Different - the tool talks to the model (suggestion with `subagent_model: null`), the GUI talks to the human (diagnostic ACK message).
 *
* Result: The absence of `canonical` is the "session model"; failure with `inherited` (the one that failed is the inherited one) and analytical diagnosis.
 */
export function resolveAmendSubagentModelChoice(
  requested: string | null | undefined,
  inherited: string | undefined,
  catalog: ModelCatalogPort | undefined,
):
  | { ok: true; canonical?: string }
  | { ok: false; inherited: boolean; text: string; message: string } {
  if (requested === null) return { ok: true };
  const text = requested ?? inherited;
  if (text === undefined) return { ok: true };
  if (catalog === undefined) {
    return requested === undefined
      ? { ok: true, canonical: text }
      : { ok: false, inherited: false, text, message: SUBAGENT_MODEL_UNAVAILABLE };
  }
  const resolution = resolveModelReference(text, catalog.listModels());
  if (!resolution.ok) {
    return { ok: false, inherited: requested === undefined, text, message: resolution.message };
  }
  return { ok: true, canonical: resolution.canonical };
}

/** Parsing failed → Structured business failed. The identification key is in the message prefix (same as the other codes in this document). */
function subagentModelFailure(message: string): ToolHandlerFailure {
  return {
    result: false,
    errorCode: AMEND_WORKFLOW_ERROR_CODE.SUBAGENT_MODEL,
    message: `workflow_subagent_model_unresolved: ${message} Nothing was stopped or created.`,
  };
}

/**
* Three-state normalization of concurrency upper bound:
 *
* - count → clamp to `[1, ceiling]`;
* - `null` → release, the key disappears (new run runs on the ceiling);
* - Omitted → Use the upper bound of the predecessor. The snapshot only has `maxConcurrency` when it is below the ceiling, so "the predecessor has not been set"
* Same thing as "front wheel running on the ceiling" here: also the keys disappear. The inherited value is also clamped again - the precursor may
* It was raised on another machine (another ceiling).
 *
* The three states only live here: after the confirmation window and handler, they only face "a number or nothing".
 */
export function resolveAmendMaxConcurrency(
  requested: number | null | undefined,
  inherited: number | undefined,
  ceiling: number | undefined,
): { max_concurrency?: number } {
  if (requested === null) return {};
  const resolved = requested ?? inherited;
  if (resolved === undefined) return {};
  return { max_concurrency: clampWorkflowMaxConcurrency(resolved, ceiling) };
}

function describePredecessor(
  snapshot: DynamicWorkflowRunSnapshot,
  sessionId: string | undefined,
): AmendWorkflowPredecessor {
// The `status` of the snapshot is the common word for the tracker (stopped is folded into canceled...); the real word is in `runStatus`, only in
// The final state is present - non-final states are always read as running (pending here is no different from running: they will both be stopped by amend).
  const status = snapshot.runStatus ?? "running";
  return {
    ...(snapshot.name === undefined ? {} : { name: snapshot.name }),
    status,
    ...(snapshot.stopReason === undefined ? {} : { stop_reason: snapshot.stopReason }),
    owned_by_this_session:
      sessionId !== undefined &&
      snapshot.parentSessionId !== undefined &&
      snapshot.parentSessionId === sessionId,
  };
}
