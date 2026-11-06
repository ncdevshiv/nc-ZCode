// ============================================================
// CreateWorkflow Tool Handler
// ============================================================
// Typechecks a dynamic-workflow script against the facade, and — once the user has
// confirmed at the gate — starts the run in the background through
// `DynamicWorkflowRunPort`.
//
//Two paths, determined by whether the port is injected:
// 1. Port present (production wiring): clean compile + Allow → port.submit → output
// {status: "backgrounded", backgroundTaskId: runId}, the result is returned to the main agent through the background notification pipeline.
// 2. The port is absent (unwired host, single test): Keep the placeholder behavior before wiring - only return the diagnosis and cause-and-effect diagram, and execute nothing.
//
// The bad script path has nothing to do with the port and is completely unchanged: `prepareApproval` cuts off the pop-up window, and the handler returns directly to diagnosis.
// Do not start or build a run (asking the user to approve a piece of code that cannot be compiled will only interrupt the agent's own decision with an ineffective decision
// Correct the error and retry the loop).

import {
  CreateWorkflowInputJsonSchema,
  CreateWorkflowInputSchema,
  CreateWorkflowOutputJsonSchema,
  CreateWorkflowOutputSchema,
  type CreateWorkflowInput,
  type CreateWorkflowOutput,
  type ModelMessageContent,
  type TraceContext,
  createWorkflowPhaseAlongside,
  createWorkflowPhaseNames,
} from "@zcode/contracts";
import type { ToolApprovalGate, ToolEntry, ToolExecutionContext, ToolHandler } from "../types.js";
import { CREATE_WORKFLOW_TOOL_DESCRIPTION } from "./create-workflow-description.js";
import {
  resolveCreateWorkflowInput,
  validateCreateWorkflowSource,
} from "./create-workflow-source.js";
import { describeWorkflowSubagentModel, parseWorkflowSubagentModel } from "./model-reference.js";
import { boundGraphOfAnalysis, displayOfAnalysis } from "./workflow-analysis-display.js";
import { resolveWorkflowDraftName, writeWorkflowDraft } from "./workflow-drafts.js";
import {
  formatWorkflowDiagnosticLines,
  workflowLaunchedScriptSentence,
  workflowSavedDraftNote,
  workflowScriptFileNote,
  type WorkflowScriptLocation,
} from "./workflow-script-notes.js";
import { analyzeScript } from "./workflow-script-analysis.js";
import { describeWorkflowScriptPath } from "./workflow-script-path.js";

const CREATE_WORKFLOW_TOOL_NAME = "CreateWorkflow";
const CREATE_WORKFLOW_TIMEOUT_MS = 15_000;
const CREATE_WORKFLOW_MODEL_BYTES = 24_000;

// The "not executed" NOTE is divided into two sentences according to the path, and each one only says that the path is true. Combined into an old way of writing a constant
// (PLACEHOLDER_HINT) After wiring the engine, both assertions turned out to be lies: it said that the execution model was still under development, and that the tool only
// Type checking, while clean script + port presence already started a background run. The copywriter is the only reader of the model, and the cost of lying is
// It accordingly abandons the commit or resubmits it.
export const DIAGNOSTICS_NOT_EXECUTED_NOTE =
  "NOTE: The workflow was NOT executed — fix the errors above and resubmit.";
export const EXECUTION_UNAVAILABLE_NOTE =
  "NOTE: The workflow was NOT executed — workflow execution is not available in this session, so the script was only typechecked.";

const createWorkflowHandler: ToolHandler = async (input, context) => {
// The input up to this point has been normalized by resolveInput: `script` must be present, and the two sources are completely isomorphic here.
// `saved` is just the context (the run tag reads it and the arguments are persisted), the execution does not read a single byte of it - so
// If a hook rewrites `saved` after normalization, it is **intentionally invalid** and cannot change what will be run.
  const parsed = CreateWorkflowInputSchema.parse(input) as CreateWorkflowInput;
  const script = parsed.script;
  if (script === undefined) {
// Unreachable: validateInput has blocked "neither", resolveInput will fill saved into script.
// If it really happens, it means someone has bypassed the executor life cycle. It is better to tell it than to silently run an empty script.
    throw new Error("CreateWorkflow handler received input without a resolved script");
  }
  const saved = parsed.saved;
  const cwd = context.workingDirectory;

  const analysis = analyzeScript(script);
  const { diagnostics, ok } = analysis;
// The confirmation window and the persistent output read the same static analysis: there are no model calls in the middle to rewrite the name.
  const causalityGraph = boundGraphOfAnalysis(analysis);

// A working copy of the inline script is placed **regardless of the compilation result**:
// A script that cannot be edited cannot reach the confirmation window. The handler is the only place it must pass. Not writing it is equivalent to "the only one that needs to be edited."
//The script has no file." The analysis is ranked first just for naming: when there is no `name`, the file name takes the first stage name, and the stage name
// On the graph. The copy of the saved source has been written in resolveInput, but the `path` source has not been written.
  const inlineDraft =
    saved === undefined && parsed.path === undefined
      ? await writeWorkflowDraft({
          cwd,
          name: resolveWorkflowDraftName(parsed.name, causalityGraph),
          source: script,
        })
      : undefined;
  const location = describeScriptLocation(parsed, inlineDraft?.path, cwd);

  if (!ok) {
    return {
      diagnostics,
      ok,
      response: [
// The saved definition is just a problem with it, not an input problem with this call; if you don’t name the file, the model
// You will think that you wrote something wrong just now, and then try again as is.
        saved === undefined
          ? "The workflow script has errors:"
          : `The saved workflow '${saved.name}'${saved.path === undefined ? "" : ` (${saved.path})`} has errors:`,
// If there is a file, press **file line** to report (the line number must be directly pasted into `Edit`); the `diagnostics` array in the output
// Keep the text lines unchanged - the transcription side shows the text.
        ...formatWorkflowDiagnosticLines(diagnostics, location),
        "",
        diagnosticsNote(parsed, location, cwd),
      ].join("\n"),
      ...(causalityGraph === undefined ? {} : { causalityGraph }),
    } satisfies CreateWorkflowOutput;
  }

  const port = context.dynamicWorkflowRunPort;
  if (port === undefined) {
// Unwired host: maintain placeholder semantics. Deliberately not downgrade to "pretend to be started" - the model will then wait for a notification that never comes.
    return {
      diagnostics,
      ok,
      response: `The workflow script compiled cleanly.\n\n${EXECUTION_UNAVAILABLE_NOTE}`,
      ...(causalityGraph === undefined ? {} : { causalityGraph }),
    } satisfies CreateWorkflowOutput;
  }

// What run remembers is always the **absolute path**: it is part of the run identity, and the session's working directory will change. How to write the model surface
// Responsible for `location.described` (another way of writing the same absolute path, not another source).
  const scriptPath = resolveSubmittedScriptPath(parsed, inlineDraft?.path);

// Submission failure (compile product is damaged, journal is unavailable) bubbles up into tool call failure: never swallowed into a band
// The successful output of backgroundTaskId will cause the background tracker to poll a run that does not exist.
  const submitted = await port.submit(
    {
      scriptText: script,
      cwd: context.workingDirectory,
// The optional display name goes all the way down to dwf_run.name (submit → EngineConfig → createRun), not just left in
// The link between the tool line and the task title - otherwise the run enumerated across sessions can only be a string of bare runIds.
      ...(parsed.name === undefined ? {} : { name: parsed.name }),
//The actual parameters follow the same metadata path as name / scriptText / cwd: submit → EngineConfig →
// createRun writes dwf_run.args_json → sandbox injection. Inline run has no arguments and fields are completely absent.
// The sandbox side interprets absence as `{}` (Invariant 7: `args` is always defined).
// Each of the two sources has its own landing point: `saved` is in `saved.args`, `path` is in the top-level `args` (the model surface is
// written like this), both cannot be present at the same time (`validateInput` ties `args` and `path`).
      ...(() => {
        const args = parsed.args ?? saved?.args;
        return args === undefined ? {} : { args };
      })(),
      parentSessionId: context.sessionId,
      toolCallId: context.toolCallId,
// The declaration stage table goes into run-launched with the submission, and the sidebar mini-track draws the site accordingly. "Running at the same time" means with it
// Same source and same peer: the subscripts point to the same table, and they will be misaligned if calculated separately.
      ...(() => {
        const phaseNames = createWorkflowPhaseNames(causalityGraph);
        if (phaseNames === undefined) return {};
        const phaseAlongside = createWorkflowPhaseAlongside(causalityGraph);
        return { phaseNames, ...(phaseAlongside === undefined ? {} : { phaseAlongside }) };
      })(),
// The concurrency upper bound has been clamped into `[1, ceiling]` in resolveInput (the confirmation window displays the value that will take effect);
// Absence is the ceiling, so no empty shell keys are created.
      ...(parsed.max_concurrency === undefined ? {} : { maxConcurrency: parsed.max_concurrency }),
// The subagent model has also been parsed into the canonical form in resolveInput (calls that cannot be resolved will not go here at all),
// So here we just split that string back into structured selection. The session model is inherited when absent, and no empty shell keys are created - port keys
// "Field present = model explicitly selected in this run" reads it.
      ...(() => {
        const subagentModel = parseWorkflowSubagentModel(parsed.subagent_model);
        return subagentModel === undefined ? {} : { subagentModel };
      })(),
// The home of the script goes into `run-launched` with the submission, and the final state notification and `GetWorkflowRun` are read back from there. When the draft cannot be written, the entire field is absent:
// The port reads it as "field present = this run has an editable file", an undefined will make that sentence a lie.
      ...(scriptPath === undefined ? {} : { scriptPath }),
      trace: resolveTraceContext(context),
    },
    { signal: context.abortSignal },
  );

  const { runId } = submitted;

  return {
    diagnostics,
    ok,
// Copywriting photo backgrounded Bash (bash-model-content.ts): Give the id and indicate that it is still running.
//Explicit results are returned in the form of notifications. The placeholder must disappear on this path.
// After the model gets the backgrounded output, it immediately uses TaskOutput to block and wait.
// Turn asynchronous run into synchronous wait - copywriting must explicitly discourage default polling (when the user explicitly requests to wait
// TaskOutput is still available, only the default boot is changed here, and the tool semantics are not changed).
    response: `The workflow script compiled cleanly and the run started in the background with ID: ${runId}. It is still running — you will be notified with the final output when it completes. Do not wait for it or poll it with TaskOutput; continue with other work unless the user asked you to wait.${describeWorkflowConcurrencyLimit(parsed.max_concurrency, port.concurrencyCeiling?.())}${describeWorkflowSubagentModel(parsed.subagent_model)}${location === undefined ? "" : workflowLaunchedScriptSentence(location)}`,
    status: "backgrounded",
    backgroundTaskId: runId,
    ...(causalityGraph === undefined ? {} : { causalityGraph }),
  } satisfies CreateWorkflowOutput;
};

/**
* Which script file is the run this time (absolute path, enter the journal with submission): `path` source is that file,
* `saved` source is a freshly written copy, inline is a freshly written draft. All three may be absent (the draft cannot be written).
 */
function resolveSubmittedScriptPath(
  parsed: CreateWorkflowInput,
  inlineDraft: string | undefined,
): string | undefined {
  if (parsed.path !== undefined) return parsed.path;
  if (parsed.saved !== undefined) return parsed.saved.draft;
  return inlineDraft;
}

/** The identity of the script file in the model; when there is no file (the draft cannot be written), the copywriting will be returned to the old one. */
function describeScriptLocation(
  parsed: CreateWorkflowInput,
  inlineDraft: string | undefined,
  cwd: string,
): WorkflowScriptLocation | undefined {
  const absolute = resolveSubmittedScriptPath(parsed, inlineDraft);
  if (absolute === undefined) return undefined;
  return {
// The `path` source file is given by the model itself, not just written by the tool - the verb is therefore different.
    kind: parsed.path === undefined ? "draft" : "path",
    described: describeWorkflowScriptPath(absolute, cwd),
    lineOffset: parsed.script_line_offset ?? 0,
  };
}

/**
* A timeless NOTE. Three situations: the saved source names which definition it is copied from, and the other files that have files name that file.
* There is no file to retain the old saying before the change (the model on that path can indeed only be submitted inline once).
 */
function diagnosticsNote(
  parsed: CreateWorkflowInput,
  location: WorkflowScriptLocation | undefined,
  cwd: string,
): string {
  const saved = parsed.saved;
  if (location === undefined) {
    return saved === undefined
      ? DIAGNOSTICS_NOT_EXECUTED_NOTE
      : "NOTE: The workflow was NOT executed — the saved file needs fixing (edit it, or save a corrected version).";
  }
  if (saved !== undefined) {
    return workflowSavedDraftNote({
      savedName: saved.name,
// The definition itself is also written as in the model surface: if it wants to change the definition next, it uses `SaveWorkflow`, but reads a
// The absolute path and the rest of the paths are relative to the workspace will make people think it is something on another machine.
      savedPath:
        saved.path === undefined ? location.described : describeWorkflowScriptPath(saved.path, cwd),
      draft: location.described,
    });
  }
  return workflowScriptFileNote(location);
}

/**
* The effective concurrency upper bound is a sentence in the result copy (common to `AmendWorkflow`). **Only appears when an upper bound is set**: Run on
* There is nothing to say about the run on the ceiling. Adding "at most N" will only make the model think that it has set something.
 *
* When it is exactly equal to the ceiling, it indicates that it is the upper limit of the machine - it means that the number required by the model is lowered. Otherwise, it will be "up to 32".
* Treat it as valid and repeat a false number when the user asks.
 */
export function describeWorkflowConcurrencyLimit(
  limit: number | undefined,
  ceiling: number | undefined,
): string {
  if (limit === undefined) return "";
  const subject = limit === 1 ? "1 subagent runs" : `${limit} subagents run`;
  return ` At most ${subject} at once${limit === ceiling ? " (this machine's maximum)" : ""}.`;
}

/**
* The port contract requires `trace` to be non-optional, and `context.traceContext` is optional, so it is synthesized as a discrete field.
 */
export function resolveTraceContext(context: ToolExecutionContext): TraceContext {
  return (
    context.traceContext ??
    ({
      traceId: context.traceId,
      spanId: context.spanId,
      parentSpanId: context.parentSpanId,
      sessionId: context.sessionId,
      turnId: context.turnId,
    } as TraceContext)
  );
}

/**
* Determine whether "running this script" is worth interrupting the user, and construct a preview to render in the confirmation window.
 *
* **The same code for both sources**: The input reached here has been normalized by `resolveInput`, `script` must be present,
* So here we neither know nor need to know whether the script is written inline or read from disk. The confirmation window for saved run is therefore the same as the inline
* Byte-by-byte isomorphism - This is exactly what the test of "the display produced by the same script through two paths is exactly the same" is nailed.
 *
* Two failure modes are directly released to the handler without a pop-up window: the input does not match the schema and cannot be compiled. Let the user approve a paragraph
* Code that cannot be edited will only interrupt the agent's own error correction and retry loop with a decision that has no effect. (parsing failed
* I closed it earlier in `resolveInput` and couldn't get here at all. )
 */
function prepareCreateWorkflowApproval(input: unknown): ToolApprovalGate {
  const parsed = CreateWorkflowInputSchema.safeParse(input);
  if (!parsed.success || parsed.data.script === undefined) return { gate: "proceed" };

  const analysis = analyzeScript(parsed.data.script);
  if (!analysis.ok) return { gate: "proceed" };

// The pop-up window has its own title and uses the picture as the main body; display has the same constructor as the directly launched startup wheel metadata.
  const display = displayOfAnalysis(analysis);
  return { gate: "ask", ...(display ? { display } : {}) };
}

export const createWorkflowToolEntry: ToolEntry = {
  capability:
    "Typecheck a dynamic-workflow TypeScript script against the facade and, once confirmed, start the run in the background",
  metadata: {
    name: CREATE_WORKFLOW_TOOL_NAME,
    description: CREATE_WORKFLOW_TOOL_DESCRIPTION,
// After the engine is wired, this call will start an execution sub-process and multiple actor sessions; the read-only declaration is flipped with execution semantics
    readOnly: false,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: CREATE_WORKFLOW_TIMEOUT_MS,
    maxOutputBytes: CREATE_WORKFLOW_MODEL_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
    needsApproval: true,
  },
  handler: createWorkflowHandler,
//Choose one of the two sources for the model input parameters (after normalization, both are present at the same time, which is a legal execution state), so it lives here and
// Not on the schema - see comments on CreateWorkflowInputSchema.
  validateInput: (input) => validateCreateWorkflowSource(input),
// Read the disk only once in the whole process. From then on, hooks, permission rules, confirmation windows and handlers all see the same bytes.
// The ceiling is the same as here: clamping must occur before the confirmation window, otherwise the user approves a number that will not take effect.
// The model directory is the same as here: `subagent_model` must be parsed into a canonical form before the confirmation window, otherwise the user approves a
// Names that have not yet been recognized, and calls that cannot be resolved will fail only after approval.
  resolveInput: (input, context) =>
    resolveCreateWorkflowInput(
      input,
      context.workingDirectory ?? ".",
      context.dynamicWorkflowRunPort?.concurrencyCeiling?.(),
      context.modelCatalogPort,
    ),
  prepareApproval: prepareCreateWorkflowApproval,
  inputSchema: CreateWorkflowInputJsonSchema,
  outputSchema: CreateWorkflowOutputJsonSchema,
  runtimeInputSchema: CreateWorkflowInputSchema,
  runtimeOutputSchema: CreateWorkflowOutputSchema,
  formatModelContent: formatCreateWorkflowModelContent,
  permission: {
    permission: "createWorkflow",
// For diagnostic purposes, not for users: the confirmation window renders the localized title itself, and the UI will also filter reasons that read like internal information.
    reason: "createWorkflow.runConfirmation: user must confirm running the analyzed script",
    riskLevel: "low",
    sideEffectScope: "none",
    needsApproval: true,
    patternSources: ["toolName"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
// Workflow is a whole block of execution + model call. Any permission mode (including yolo / plan) must be asked first.
    alwaysAsk: true,
// "Every call is a different script" is no longer true for saved workflows: by name
// When called, the same name is the "same" workflow every time. But the "same" workflow is not enough to support confirmation-free——
// Still need to confirm every time, there are two more difficult reasons: run it once
// It costs money and has real side effects; and the saved file may be modified at any time after approval (manual modification, git pull, submission by others),
// So "this name was approved last time" cannot deduce "the code that will be run this time" cannot be inferred at all. **Durable** Confirmed Never Waives.
    //
// What is released is **session scope**: after the user has read and approved the first script of this session,
// You can select "Always allow in this session" to avoid confirmation for subsequent CreateWorkflows in this session. Authorization only lives in
// In the memory of the PermissionService instance, restart/cold recovery/`/new` starts from zero.
    askOptions: { allowAlways: "session" },
  },
  resultBudget: {
    maxInlineBytes: CREATE_WORKFLOW_MODEL_BYTES,
    maxModelBytes: CREATE_WORKFLOW_MODEL_BYTES,
    strategy: "truncate",
    preview: {
      maxBytes: CREATE_WORKFLOW_MODEL_BYTES,
      direction: "head",
    },
  },
  timeout: {
    defaultMs: CREATE_WORKFLOW_TIMEOUT_MS,
    maxMs: CREATE_WORKFLOW_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: false,
    cleanup: "none",
    userVisibleMessage: "CreateWorkflow typechecks synchronously and cannot be cancelled",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};

function formatCreateWorkflowModelContent(output: unknown): ModelMessageContent {
  const parsed = CreateWorkflowOutputSchema.safeParse(output);
  if (!parsed.success) return "CreateWorkflow returned an invalid result.";
  return parsed.data.response;
}
