/**
* The result card display structure of the workflow tool: five-piece set of observation classes (GetWorkflowRun / ListWorkflowRuns /
* EvalWorkflowSnippet / ListSavedWorkflows / ListModels) + ResumeWorkflowRun recovery card.
 *
* Separate files instead of stuffed into result-display.ts (500 lines already): these constructors share the same set
* The skeleton of "safeParse output schema → display side independent limit length → exceeding limit and truncated" is consistent with the existing
* CreateCreateWorkflowDisplay is of the same family but in its own right.
 */

import {
  CREATE_WORKFLOW_DISPLAY_MAX_DIAGNOSTICS,
  CREATE_WORKFLOW_DISPLAY_MAX_MESSAGE_CHARS,
  EVAL_WORKFLOW_SNIPPET_TOOL_NAME,
  EvalWorkflowSnippetOutputSchema,
  GET_WORKFLOW_RUN_TOOL_NAME,
  GetWorkflowRunOutputSchema,
  LIST_MODELS_TOOL_NAME,
  ListModelsOutputSchema,
  LIST_SAVED_WORKFLOWS_TOOL_NAME,
  ListSavedWorkflowsOutputSchema,
  LIST_WORKFLOW_RUNS_TOOL_NAME,
  ListWorkflowRunsOutputSchema,
  RESUME_WORKFLOW_RUN_TOOL_NAME,
  ResumeWorkflowRunOutputSchema,
  WORKFLOW_OBSERVATION_DISPLAY_MAX_ACTORS,
  WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_CHARS,
  WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_ENTRIES,
  WORKFLOW_OBSERVATION_DISPLAY_MAX_META_CHARS,
  WORKFLOW_OBSERVATION_DISPLAY_MAX_MODELS,
  WORKFLOW_OBSERVATION_DISPLAY_MAX_PHASES,
  WORKFLOW_OBSERVATION_DISPLAY_MAX_RESULT_CHARS,
  WORKFLOW_OBSERVATION_DISPLAY_MAX_SUBAGENTS,
  type GetWorkflowRunOutput,
  type GetWorkflowRunToolResultDisplayPayload,
  type ToolResultDisplayPayload,
} from "@zcode/contracts";

import { boundDisplayText } from "./display-text.js";

export function createWorkflowObservationDisplay(
  toolName: string,
  output: unknown,
): ToolResultDisplayPayload | undefined {
  return (
    createGetWorkflowRunDisplay(toolName, output) ??
    createListWorkflowRunsDisplay(toolName, output) ??
    createEvalWorkflowSnippetDisplay(toolName, output) ??
    createSavedWorkflowListDisplay(toolName, output) ??
    createListModelsDisplay(toolName, output) ??
    createResumeWorkflowRunDisplay(toolName, output)
  );
}

function createGetWorkflowRunDisplay(
  toolName: string,
  output: unknown,
): ToolResultDisplayPayload | undefined {
  if (toolName !== GET_WORKFLOW_RUN_TOOL_NAME) return undefined;
  const parsed = GetWorkflowRunOutputSchema.safeParse(output);
  if (!parsed.success) return undefined;

  const data = parsed.data;
  let truncated = false;

  const actors = data.actors.slice(0, WORKFLOW_OBSERVATION_DISPLAY_MAX_ACTORS);
  if (actors.length < data.actors.length) truncated = true;

// Get the tail: The value of logTail is "Latest Progress", the head is truncated but not the tail.
  const droppedLogEntries = Math.max(0, data.logTail.length - WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_ENTRIES);
  const logTail = data.logTail
    .slice(-WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_ENTRIES)
    .map((entry) => {
      const bounded = boundDisplayText(entry.message, WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_CHARS);
      if (bounded.truncated) truncated = true;
// `at` Bring the card as it is (if present, absent if not): the log age on the card is the same as the `<log_tail>` on the model surface.
      return {
        sequence: entry.sequence,
        message: bounded.value,
        ...(entry.at === undefined ? {} : { at: entry.at }),
      };
    });
  if (droppedLogEntries > 0) truncated = true;

  let result: string | undefined;
  if (data.result !== undefined) {
    const bounded = boundDisplayText(data.result, WORKFLOW_OBSERVATION_DISPLAY_MAX_RESULT_CHARS);
    if (bounded.truncated) truncated = true;
    result = bounded.value;
  }

// Errors on cards only have code / message.
// `providerStop` on the output side is the diagnostic details of the model channel, which is only left in the tool text and output; it is used on the rendering side
// The mirror schema of packages/shared strictly verifies each frame. One more key on the display means that the entire row will be rejected——
// Here data.error is passed through as it is, and a desktop session is stuck on fault.subscription.recoveryFailed;
// display will also fall into the tool part's metadata, so a wrong write will recur every time a cold start occurs.
  const error =
    data.error === undefined ? undefined : { code: data.error.code, message: data.error.message };

// Situation cross-section: The stage table and roster each have their own boundaries on the card (display but result budget),
// Truncate to the same truncated mark - there should only be rows on the card that it actually draws.
// These fields are optional in the schema (in order to allow the old payload that is persisted before the form goes online to still pass verification), but on the structure side
// **Fill in every time**: Optional is a door left for reading old data, not a gap left for new calls.
  const phases = data.phases?.slice(0, WORKFLOW_OBSERVATION_DISPLAY_MAX_PHASES);
  if (phases !== undefined && phases.length < data.phases!.length) truncated = true;
  const subagents = data.subagents.slice(0, WORKFLOW_OBSERVATION_DISPLAY_MAX_SUBAGENTS);
  if (subagents.length < data.subagents.length || data.subagentsTruncated === true) truncated = true;

  return {
    kind: "get_workflow_run",
    runId: data.runId,
    label: data.label,
    status: data.status,
    ...(data.stopReason === undefined ? {} : { stopReason: data.stopReason }),
    ...(data.possiblyInterrupted === true ? { possiblyInterrupted: true } : {}),
    summary: data.summary,
    generatedAt: data.generatedAt,
    usage: data.usage,
    ...(phases === undefined || phases.length === 0 ? {} : { phases }),
    subagents: subagents.map(toDisplaySubagent),
    health: data.health,
    actors,
    logTail,
    ...(result !== undefined ? { result } : {}),
    ...(error !== undefined ? { error } : {}),
    ...(truncated ? { truncated: true } : {}),
  };
}

/**
* Nested subagents for tool faces → flat rows for card faces. The few things in `currentAsk` are the second half of this line;
* The free text of the reason for waiting is not included in the card (the card only needs "Waiting for a slot/retreating" and how long to wait), and the remaining fields are
* **Absence is absence** - `0 tool calls` and "not knowing" are two different things.
 */
function toDisplaySubagent(
  subagent: GetWorkflowRunOutput["subagents"][number],
): NonNullable<GetWorkflowRunToolResultDisplayPayload["subagents"]>[number] {
  const ask = subagent.currentAsk;
  return {
    siteId: subagent.siteId,
    ordinal: subagent.ordinal,
    ...(subagent.name === undefined ? {} : { name: subagent.name }),
    state: subagent.state,
    ...(subagent.phaseName === undefined ? {} : { phaseName: subagent.phaseName }),
    ...(ask?.instructionsHead === undefined ? {} : { instructionsHead: ask.instructionsHead }),
    ...(ask?.startedAt === undefined ? {} : { startedAt: ask.startedAt }),
    ...(ask?.turn === undefined ? {} : { turn: ask.turn }),
    ...(ask?.toolCalls === undefined ? {} : { toolCalls: ask.toolCalls }),
    ...(ask?.lastTool === undefined ? {} : { lastTool: ask.lastTool }),
    ...(subagent.wait === undefined
      ? {}
      : {
          waitCause: subagent.wait.cause,
          ...(subagent.wait.retryAfterMs === undefined
            ? {}
            : { retryAfterMs: subagent.wait.retryAfterMs }),
          ...(subagent.wait.since === undefined ? {} : { waitSince: subagent.wait.since }),
        }),
    ...(subagent.parkedOn === undefined ? {} : { parkedOn: subagent.parkedOn }),
    stepsSettled: subagent.stepsSettled,
    stepsFailed: subagent.stepsFailed,
    tokens: subagent.tokens,
    ...(subagent.lastProgressAt === undefined ? {} : { lastProgressAt: subagent.lastProgressAt }),
  };
}

function createListWorkflowRunsDisplay(
  toolName: string,
  output: unknown,
): ToolResultDisplayPayload | undefined {
  if (toolName !== LIST_WORKFLOW_RUNS_TOOL_NAME) return undefined;
  const parsed = ListWorkflowRunsOutputSchema.safeParse(output);
  if (!parsed.success) return undefined;

// All fields in the run line are decimal values ​​/ There is bounded short text (label ≤ 80, name ≤ 64), and the limit is capped at 50.
// Just pass it through directly - the truncation semantics are expressed by the truncated of the output itself.
  return {
    kind: "list_workflow_runs",
    runs: parsed.data.runs,
    ...(parsed.data.truncated === true ? { truncated: true } : {}),
  };
}

function createEvalWorkflowSnippetDisplay(
  toolName: string,
  output: unknown,
): ToolResultDisplayPayload | undefined {
  if (toolName !== EVAL_WORKFLOW_SNIPPET_TOOL_NAME) return undefined;
  const parsed = EvalWorkflowSnippetOutputSchema.safeParse(output);
  if (!parsed.success) return undefined;

  const data = parsed.data;
  let truncated = false;

// Diagnosis is limited to long-term photos. Same as createCreateWorkflowDisplay: number of slices + single message truncation character.
  const diagnostics = data.diagnostics
    .slice(0, CREATE_WORKFLOW_DISPLAY_MAX_DIAGNOSTICS)
    .map((diagnostic) => ({
      line: diagnostic.line,
      column: diagnostic.column,
      code: diagnostic.code,
      message: diagnostic.message.slice(0, CREATE_WORKFLOW_DISPLAY_MAX_MESSAGE_CHARS),
    }));
  if (diagnostics.length < data.diagnostics.length) truncated = true;

// Get the tail of logs: snippet logs are in order of arrival, with the latest behavior at the tail.
  const droppedLogs = Math.max(0, data.logs.length - WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_ENTRIES);
  const logs = data.logs.slice(-WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_ENTRIES).map((entry) => {
    const bounded = boundDisplayText(entry, WORKFLOW_OBSERVATION_DISPLAY_MAX_LOG_CHARS);
    if (bounded.truncated) truncated = true;
    return bounded.value;
  });
  if (droppedLogs > 0) truncated = true;

  const boundedResponse = boundDisplayText(data.response, WORKFLOW_OBSERVATION_DISPLAY_MAX_RESULT_CHARS);
  if (boundedResponse.truncated) truncated = true;

  return {
    kind: "eval_workflow_snippet",
    ok: data.ok,
    diagnostics,
    logs,
    response: boundedResponse.value,
    durationMs: data.durationMs,
    ...(truncated ? { truncated: true } : {}),
  };
}

function createSavedWorkflowListDisplay(
  toolName: string,
  output: unknown,
): ToolResultDisplayPayload | undefined {
  if (toolName !== LIST_SAVED_WORKFLOWS_TOOL_NAME) return undefined;
  const parsed = ListSavedWorkflowsOutputSchema.safeParse(output);
  if (!parsed.success) return undefined;

  const data = parsed.data;
  let truncated = false;

  const boundMeta = (value: string | undefined): string | undefined => {
    if (value === undefined) return undefined;
    const bounded = boundDisplayText(value, WORKFLOW_OBSERVATION_DISPLAY_MAX_META_CHARS);
    if (bounded.truncated) truncated = true;
    return bounded.value;
  };

  const workflows = data.workflows.map((entry) => ({
    name: entry.name,
    description: boundMeta(entry.description),
    whenToUse: boundMeta(entry.whenToUse),
    scope: entry.scope,
    path: entry.path,
// args only retains names: declaration details (type/description/default value) are returned to the save confirmation window, and the list cards are not repeated.
    argNames: entry.args === undefined ? [] : Object.keys(entry.args),
  }));

  const invalid = data.invalid?.map((entry) => ({
    path: entry.path,
    reason: boundMeta(entry.reason),
  }));

  return {
    kind: "saved_workflow_list",
    workflows,
    ...(invalid !== undefined && invalid.length > 0 ? { invalid } : {}),
    ...(truncated ? { truncated: true } : {}),
  };
}

/**
* Scorecard for ListModels.
 *
* The directory lines themselves are all short ids and decimal values, which can be passed through as they are; providerLabel / disabledReason comes from the registry
* For free text, use boundDisplayText. The number of lines is capped at 100 - the model channel of the tool has a budget of 24k, display
* There is no channel. A machine connected to an aggregation provider can list thousands of lines. If you cut it off, it will say "Not all shown" and never report the number:
* The numbers on the catalog card should only be those that are actually drawn on it.
 */
function createListModelsDisplay(
  toolName: string,
  output: unknown,
): ToolResultDisplayPayload | undefined {
  if (toolName !== LIST_MODELS_TOOL_NAME) return undefined;
  const parsed = ListModelsOutputSchema.safeParse(output);
  if (!parsed.success) return undefined;

  const data = parsed.data;
  let truncated = false;

  const boundMeta = (value: string | undefined): string | undefined => {
    if (value === undefined) return undefined;
    const bounded = boundDisplayText(value, WORKFLOW_OBSERVATION_DISPLAY_MAX_META_CHARS);
    if (bounded.truncated) truncated = true;
    return bounded.value;
  };

  const models = data.models.slice(0, WORKFLOW_OBSERVATION_DISPLAY_MAX_MODELS).map((model) => {
    const providerLabel = boundMeta(model.providerLabel);
    const disabledReason = boundMeta(model.disabledReason);
    return {
      id: model.id,
      providerId: model.providerId,
      modelId: model.modelId,
      ...(providerLabel === undefined ? {} : { providerLabel }),
      reasoningLevels: [...model.reasoningLevels],
      ...(model.defaultReasoningLevel === undefined
        ? {}
        : { defaultReasoningLevel: model.defaultReasoningLevel }),
      ...(model.contextWindow === undefined ? {} : { contextWindow: model.contextWindow }),
      ...(disabledReason === undefined ? {} : { disabledReason }),
    };
  });
  if (models.length < data.models.length) truncated = true;

  return {
    kind: "list_models",
// Absent when no entry in the directory is marked with current (same as tool output: the session selection may point to a deleted provider).
    ...(data.current === undefined ? {} : { current: data.current }),
    models,
    ...(truncated ? { truncated: true } : {}),
  };
}

/**
* Results card for ResumeWorkflowRun. The payload is deliberately minimal {runId} - what the recovery card wants to convey is
* "Which run continues in the background", the response guide copy belongs to the model channel, and the UI has its own localized vocabulary;
* There is no truncated semantics for infinitely long faces.
 */
function createResumeWorkflowRunDisplay(
  toolName: string,
  output: unknown,
): ToolResultDisplayPayload | undefined {
  if (toolName !== RESUME_WORKFLOW_RUN_TOOL_NAME) return undefined;
  const parsed = ResumeWorkflowRunOutputSchema.safeParse(output);
  if (!parsed.success) return undefined;
  return { kind: "resume_workflow_run", runId: parsed.data.runId };
}
