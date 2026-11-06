// ============================================================
// ResumeWorkflowRun Tool Handler
// ============================================================
//Resume a stopped dwf run (model side entry; recoverable set = `stopped`, regardless of reason). See port contract
// `DynamicWorkflowRunPort.resume`（contracts/src/interfaces/dynamic-workflow-run.port.ts）。
//
// The handler is deliberately **very thin**: gate (recoverable set determination), registry replacement, compileOnce/scriptHash revalidation are all in
// port.resume The server only does three things here: get the port, transparently transmit the run_id, and project the result into a contract shape.
//The output backgrounded shape allows the executor to follow the same automatic tracking path of CreateWorkflow
// (call-runner → trackBackgroundTask), snapshot/wait/cancel/notify zero new code.
//
// resume is scriptHash
// Nail it on the same script that has been approved when submitting, in the same risk profile as the UI Resume button, and the model call is executed directly;
// PreToolUse deny and project deny rules can still intercept. There is no prepareApproval: there is no synchronization means to verify the runId.
// The bad id will still return run_not_found failure after approval, and the pop-up window cannot delete anything.

import {
  RESUME_WORKFLOW_RUN_TOOL_NAME,
  ResumeWorkflowRunInputJsonSchema,
  ResumeWorkflowRunInputSchema,
  ResumeWorkflowRunOutputJsonSchema,
  ResumeWorkflowRunOutputSchema,
  type ModelMessageContent,
  type ResumeWorkflowRunInput,
  type ResumeWorkflowRunOutput,
} from "@zcode/contracts";
import type {
  ToolEntry,
  ToolExecutionContext,
  ToolHandler,
  ToolHandlerFailure,
} from "../types.js";
import { workflowRunNotFoundFailure } from "./workflow-run-introspection.js";

const RESUME_WORKFLOW_RUN_TIMEOUT_MS = 15_000;
/** According to CreateWorkflow: the output is only one piece of guidance copy, 24k is more than enough. */
const RESUME_WORKFLOW_RUN_MODEL_BYTES = 24_000;

/**
* Local failure code table. The value itself does not enter the model (executor projects errorCode into `code: "N"` string),
* The only authority of the discriminant key is the message prefix - the ownership of the two numerical spaces is clearly stated in the comments:
 *
* - `not_found` **Reuse introspection table** (workflow-run-introspection.ts) RUN_NOT_FOUND code and its
* message (same key, same code), so it is not in this table;
* - The remaining four codes of this table are independent of the numerical space of the introspection table, and cross-table comparisons are prohibited** - deliberately compiled from 11 onwards to avoid conflicts with
* The introspection table (1/2) visually crashes, but what really prevents confusion is that "the values ​​are just log bits, and the discrimination key is in the message prefix."
* Choose annotation ownership instead of merging into the shared list: turning the introspection module into a dwf failure code master list is equivalent to making a
* The read-only introspection toolset piggybacks on execution-side failure semantics.
 */
const RESUME_WORKFLOW_RUN_ERROR_CODE = {
  RESUME_UNAVAILABLE: 11,
  NOT_RESUMABLE: 12,
  ALREADY_RUNNING: 13,
  SCRIPT_MISSING: 14,
  SCRIPT_MISMATCH: 15,
  SUPERSEDED: 16,
  COMPILE_FAILED: 17,
} as const;

const RESUME_WORKFLOW_RUN_DESCRIPTION = [
  "Resumes a dynamic-workflow run whose status is `stopped` — the user cancelled it, you stopped it with TaskStop, a provider-side error stopped it (expired sign-in, model not in the plan, quota cap), or the process that owned it exited (`interrupted`). The run continues under the same run ID: finished steps are replayed from the journal without spending tokens, unfinished steps are dispatched again. The one stopped run that is NOT resumable is a `superseded` one: an AmendWorkflow replaced it, and its successor is the live run.",
  "",
  "- Takes run_id — from CreateWorkflow's or AmendWorkflow's result, from a completion notification, or from GetWorkflowRun / ListWorkflowRuns.",
  "- The resumed run is backgrounded: you will be notified with the final output when it completes. Do not wait for it or poll it with TaskOutput; continue with other work unless the user asked you to wait.",
  "- An `errored` run (the script itself failed) is NOT resumable — replaying it would fail the same way. Fix the script and submit it with AmendWorkflow instead. A completed run is not resumable either; a `superseded` run is refused with the successor's ID.",
  "- Stop reason `user` means the user stopped it on purpose: resume it only when the user asks you to; never resume a run the user just cancelled on your own initiative. Reason `model` is your own TaskStop. Reason `provider` means a provider-side error stopped it: resolve the cause with the user first (the stop notification names it), then resume. Reason `interrupted` (the process died) is different: continuing it is usually what the user wants.",
].join("\n");

/**
* "This session does not have the ability to resume". Port absence (journal is not available → run service is not constructed at all) and methods
* Absent (stub without resume) returns the same failure: this is the same thing for the model (as in listRuns/getRunDetail
* typeof detection precedent). Never degrade silently - the model will therefore wait for a notification that never arrives.
 */
function workflowResumeUnavailableFailure(): ToolHandlerFailure {
  return {
    result: false,
    errorCode: RESUME_WORKFLOW_RUN_ERROR_CODE.RESUME_UNAVAILABLE,
    message:
      "workflow_resume_unavailable: this session cannot resume workflow runs — workflow execution is not available here. This is a capability gap, not a status problem.",
  };
}

/** The reason of the port → structured failure with each operable text (the judgment key is in the message prefix). */
function resumeFailureFor(reason: string, runId: string, detail?: string): ToolHandlerFailure {
  switch (reason) {
// The saved script may no longer adapt to the current facade; when compilation fails, the script needs to be revised first and cannot be replayed directly.
    case "compile_failed":
      return {
        result: false,
        errorCode: RESUME_WORKFLOW_RUN_ERROR_CODE.COMPILE_FAILED,
        message: `workflow_run_compile_failed: the stored script of run ${runId} no longer compiles against the current workflow facade, so it cannot be replayed as-is. Rewrite it for the current facade and submit it with AmendWorkflow, which keeps the finished work of this run.${detail === undefined ? "" : `\n${detail}`}`,
      };
    case "not_resumable":
      return {
        result: false,
        errorCode: RESUME_WORKFLOW_RUN_ERROR_CODE.NOT_RESUMABLE,
        message:
          "workflow_run_not_resumable: this run is not in the resumable set — only a `stopped` run can be resumed (any stop reason except `superseded`). An `errored` run needs a corrected script submitted with AmendWorkflow; a completed run has nothing to resume. Check the status with GetWorkflowRun.",
      };
    case "superseded":
      return {
        result: false,
        errorCode: RESUME_WORKFLOW_RUN_ERROR_CODE.SUPERSEDED,
        message: `workflow_run_superseded: run ${runId} was stopped by an AmendWorkflow and superseded; its unfinished work belongs to the successor run (see GetWorkflowRun's <superseded_by>). Read or amend the successor instead of resuming this run.`,
      };
// The gate scope of already_running is the current run service instance (per-app/per-session).
// Registry: cross-session concurrent resume same journal run will not be blocked - it has open semantics. The text here only describes the semantics of this instance and does not make any commitment to cross-instance behavior.
    case "already_running":
      return {
        result: false,
        errorCode: RESUME_WORKFLOW_RUN_ERROR_CODE.ALREADY_RUNNING,
        message:
          "workflow_run_already_running: this run is already in flight in this session's workflow runtime. Wait for its completion notification instead of resuming it again.",
      };
    case "script_missing":
      return {
        result: false,
        errorCode: RESUME_WORKFLOW_RUN_ERROR_CODE.SCRIPT_MISSING,
        message:
          "workflow_run_script_missing: this run's journal record has no stored script text (it predates script persistence), so there is nothing to re-run. Start a fresh run with CreateWorkflow instead.",
      };
    case "script_mismatch":
      return {
        result: false,
        errorCode: RESUME_WORKFLOW_RUN_ERROR_CODE.SCRIPT_MISMATCH,
        message: `workflow_run_script_mismatch: the stored script hash for run ${runId} no longer matches the stored script text — the journal record was modified by an outside force. Start a fresh run with CreateWorkflow instead.`,
      };
    default:
//Reason outside the port contract: still returns structured failure (no throw - that is the channel of wiring failure), the judgment key is used
// Keep the prefix and include the original words in the copy for log checking.
      return {
        result: false,
        errorCode: RESUME_WORKFLOW_RUN_ERROR_CODE.NOT_RESUMABLE,
        message: `workflow_run_not_resumable: the workflow runtime refused to resume run ${runId} (reason: ${reason}).`,
      };
  }
}

const resumeWorkflowRunHandler: ToolHandler = async (input, context: ToolExecutionContext) => {
  const parsed = ResumeWorkflowRunInputSchema.parse(input) as ResumeWorkflowRunInput;

  const port = context.dynamicWorkflowRunPort;
  if (port === undefined || typeof port.resume !== "function") {
    return workflowResumeUnavailableFailure();
  }

  const result = await port.resume(parsed.run_id);
  if (!result.ok) {
// not_found reuses the run_not_found key of the introspection tool (same key, same code, same message).
    return result.reason === "not_found"
      ? workflowRunNotFoundFailure(parsed.run_id)
      : resumeFailureFor(result.reason, parsed.run_id, result.message);
  }

  return {
    ok: true,
    runId: result.runId,
//Copywriting photo CreateWorkflow's backgrounded guide (create-workflow.ts): Give the id and the description is still there
//Run, clear the result and return it in the form of notification, explicitly dissuade the default polling (if the actual test fails, the model will use it immediately
// TaskOutput turns asynchronous run into synchronous wait).
    response: `The workflow run ${result.runId} has been resumed and is running in the background. It is still running — you will be notified with the final output when it completes. Do not wait for it or poll it with TaskOutput; continue with other work unless the user asked you to wait.`,
    status: "backgrounded",
// backgroundTaskId ≡ runId (same identity as CreateWorkflow's backgrounded output):
//Cancel, TaskOutput query, and final state notification share this key.
    backgroundTaskId: result.runId,
  } satisfies ResumeWorkflowRunOutput;
};

function formatResumeWorkflowRunModelContent(output: unknown): ModelMessageContent {
  const parsed = ResumeWorkflowRunOutputSchema.safeParse(output);
  if (!parsed.success) return "ResumeWorkflowRun returned an invalid result.";
  return parsed.data.response;
}

export const resumeWorkflowRunToolEntry: ToolEntry = {
  capability: "Resume a stopped dynamic-workflow run under the same run ID",
  metadata: {
    name: RESUME_WORKFLOW_RUN_TOOL_NAME,
    description: RESUME_WORKFLOW_RUN_DESCRIPTION,
// Recovery = Restart the execution child process and actor session (completed node replay, unfinished redistribution), and
// Same file as CreateWorkflow, read-only declaration is not valid.
    readOnly: false,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: RESUME_WORKFLOW_RUN_TIMEOUT_MS,
    maxOutputBytes: RESUME_WORKFLOW_RUN_MODEL_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
// Confirmation-free: nail the same script that has been approved, no pop-up window.
    needsApproval: false,
  },
  handler: resumeWorkflowRunHandler,
  inputSchema: ResumeWorkflowRunInputJsonSchema,
  outputSchema: ResumeWorkflowRunOutputJsonSchema,
  runtimeInputSchema: ResumeWorkflowRunInputSchema,
  runtimeOutputSchema: ResumeWorkflowRunOutputSchema,
  formatModelContent: formatResumeWorkflowRunModelContent,
  permission: {
    permission: "resumeWorkflowRun",
    reason: "resumeWorkflowRun.runConfirmation: resuming continues executing a stopped workflow run",
    riskLevel: "low",
    sideEffectScope: "none",
// resume is
// The scriptHash is nailed to the same script that has been approved when submitting. The completion node is pure replay and is the same as the UI button.
// Risk profile - model calls are executed directly. The interception surface is still there: PreToolUse deny (call-runner, precede execution) and
// The project deny rule (denyPriority:"beforeAsk") takes effect as usual.
    needsApproval: false,
// run_id enters the pattern matching surface (according to task_id of TaskOutput), so that project rules can be constrained to specific runs.
    patternSources: ["toolName", "input"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
  },
  resultBudget: {
    maxInlineBytes: RESUME_WORKFLOW_RUN_MODEL_BYTES,
    maxModelBytes: RESUME_WORKFLOW_RUN_MODEL_BYTES,
    strategy: "truncate",
    preview: {
      maxBytes: RESUME_WORKFLOW_RUN_MODEL_BYTES,
      direction: "head",
    },
  },
  timeout: {
    kind: "timed",
    defaultMs: RESUME_WORKFLOW_RUN_TIMEOUT_MS,
    maxMs: RESUME_WORKFLOW_RUN_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: false,
    cleanup: "none",
    userVisibleMessage: "ResumeWorkflowRun resumes the run synchronously and cannot be cancelled",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};
