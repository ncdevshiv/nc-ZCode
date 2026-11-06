import { randomUUID } from "node:crypto";
import {
  AMEND_WORKFLOW_TOOL_NAME,
  boundWorkflowLaunchMeta,
  createWorkflowPhaseAlongside,
  createWorkflowPhaseNames,
  type DynamicWorkflowRunSnapshot,
  type TraceContext,
  type WorkflowSettingsAmendMeta,
} from "@zcode/contracts";
import { uuidv7 } from "@zcode/shared";
import {
  resolveAmendMaxConcurrency,
  resolveAmendSubagentModelChoice,
} from "../../tool/handlers/amend-workflow-resolve.js";
import { resolveKeptScriptFile } from "../../tool/handlers/amend-workflow-source.js";
import { parseWorkflowSubagentModel } from "../../tool/handlers/model-reference.js";
import {
  boundGraphOfAnalysis,
  displayOfAnalysis,
} from "../../tool/handlers/workflow-analysis-display.js";
import {
  resolveWorkflowDraftName,
  writeWorkflowDraft,
} from "../../tool/handlers/workflow-drafts.js";
import { analyzeScript } from "../../tool/handlers/workflow-script-analysis.js";
import type { ExecutableToolCall } from "../../tool/types.js";
import { createRuntimeCommandId } from "../command-queue.js";
import { traceContextToLogContext } from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";
import { boundedCompileDiagnostics } from "./dynamic-workflow-run-start.js";

/**
* GUI "configuration" request. Two settings keep the three states of the tool:
* Omitted = inherit, `null` = return to default (session model/native limit), value = setting.
 */
export interface AmendWorkflowRunSettingsInput {
  runId: string;
  subagentModel?: string | null;
  maxConcurrency?: number | null;
  traceContext?: TraceContext;
}

/** The rejection vocabulary is verbatim identical to shared's `workflowRunSettingsRejectionReasonSchema`. */
export type AmendWorkflowRunSettingsRejection =
  | "not_found"
  | "not_configurable"
  | "unchanged"
  | "script_missing"
  | "model_unavailable"
  | "compile_failed"
  | "missing_boundaries"
  | "start_failed";

export type AmendWorkflowRunSettingsResult =
  | { ok: true; runId: string; toolCallId: string; supersededRunId?: string }
  | { ok: false; reason: AmendWorkflowRunSettingsRejection; message?: string };

/** The normalized form of two settings for this run: Absence means default (session model / local upper limit). */
interface RunSettings {
  subagentModel?: string;
  maxConcurrency?: number;
}

/**
* GUI "Configuration": Revise a run with the same script and new settings.
 *
* It is the **second caller** of `port.amend`, which is isomorphic to the `AmendWorkflow` tool: the same three-state normalization
* (`resolveAmendSubagentModelChoice` / `resolveAmendMaxConcurrency`, do not copy), the same compilation, the same
* Background tracker (synthesized into an AmendWorkflow descriptor). The only difference is: no model wheel, no confirmation window (the user is in the pop-up layer)
* Click "Apply" to agree, the script is the one he approved), the actual parameters use the predecessor (`inheritArgs`), and use a queue
* controlOnly "setting wheel" records this event into the session.
 *
* Order fixed: every failure up to `port.amend` has zero side effects - the old run still runs, no new lines, no messages.
 */
export async function amendWorkflowRunSettings(
  this: AgentRuntimeInternal,
  input: AmendWorkflowRunSettingsInput,
): Promise<AmendWorkflowRunSettingsResult> {
  const traceContext = input.traceContext ?? this.rootTraceContext;
  const port = this.dynamicWorkflowRunPort;
  if (port === undefined || typeof port.amend !== "function") {
// The ability is only registered when the port has amend and getScript; the lack of it here is due to a wiring fault rather than user input.
    return { ok: false, reason: "start_failed", message: "dynamic workflow amend unavailable" };
  }

// (1) This run must exist and belong to this session - whichever session the command is sent to, you can only change that session's own run.
  const snapshot = await port.getTask(input.runId);
  if (snapshot === undefined || snapshot.parentSessionId !== this.sessionId) {
    return { ok: false, reason: "not_found" };
  }
// (2) Each ask of the completed run will be replayed from the cache, and the new settings will not take effect; the replaced run will be its successor.
  if (snapshot.runStatus === "completed" || snapshot.supersededBy !== undefined) {
    return { ok: false, reason: "not_configurable" };
  }
// (3) The inherited script (the same reading path as "Keeping the predecessor's script").
  const script =
    typeof port.getScript === "function" ? await port.getScript(input.runId) : undefined;
  if (script === undefined || script.length === 0) {
    return { ok: false, reason: "script_missing" };
  }

// (4)(5) The three-state normalization of the two settings is the same code as the tool.
  const current = runSettingsOfSnapshot(snapshot);
  const model = resolveAmendSubagentModelChoice(
    input.subagentModel,
    current.subagentModel,
    this.modelCatalogPort,
  );
  if (!model.ok) {
// The sentence when the directory is absent is written to the model ("omit subagent_model"), the GUI only needs the reason code; when there is a directory
// Parsing diagnosis (candidate names, etc.) is also useful for humans, just go with message.
    return {
      ok: false,
      reason: "model_unavailable",
      ...(this.modelCatalogPort === undefined ? {} : { message: model.message }),
    };
  }
  const ceiling = port.concurrencyCeiling?.();
  const bound = resolveAmendMaxConcurrency(input.maxConcurrency, current.maxConcurrency, ceiling);
// A bound that is equal to the ceiling means "it has no bounds of its own": the snapshot only has maxConcurrency when it is lower than the ceiling, and the pronunciation is the same on both sides.
// The comparison of "unchanged" is valid (the elastic layer also sends null when the stepper is pushed to the top, and the caller who directly gives the number is covered here).
  const nextBound =
    bound.max_concurrency === undefined || bound.max_concurrency === ceiling
      ? undefined
      : bound.max_concurrency;
  const next: RunSettings = {
    ...(model.canonical === undefined ? {} : { subagentModel: model.canonical }),
    ...(nextBound === undefined ? {} : { maxConcurrency: nextBound }),
  };
// (6) If nothing has changed, you can’t afford a new run: one revision will stop the running run, and there is no reason to pay this price for zero changes.
  const modelChanged = next.subagentModel !== current.subagentModel;
  const boundChanged = next.maxConcurrency !== current.maxConcurrency;
  if (!modelChanged && !boundChanged) return { ok: false, reason: "unchanged" };

// (7) Compile. The saved script may have been written on an earlier facade; if you can't edit it, it will stop here, and the old run will not move.
  const analysis = analyzeScript(script);
  if (!analysis.ok || analysis.diagnostics.length > 0) {
    return {
      ok: false,
      reason: "compile_failed",
      message: boundedCompileDiagnostics(
        `The stored script of run ${input.runId} has errors:`,
        analysis.diagnostics,
      ),
    };
  }

// -- Zero side effects so far. ——

// The new run script file follows the same rules as when the tool inherits the script.
//Same piece of code: If the predecessor script file still has this byte, continue to remember it, otherwise write a new draft as "script not from file".
// If you don't remember, the only way to revise this run after the model is to inline the entire script and copy it again. The draft was my best effort: I couldn’t write it.
// In case of absence, run will start as usual. It was the only release before amend, leaving at best a draft document that no one cited.
  const graph = boundGraphOfAnalysis(analysis);
  const keptFile = await resolveKeptScriptFile({
    cwd: this.workingDirectory,
    scriptPath: snapshot.scriptPath,
    script,
  });
  const scriptPath =
    keptFile?.path ??
    (
      await writeWorkflowDraft({
        cwd: this.workingDirectory,
        name: resolveWorkflowDraftName(snapshot.name, graph),
        source: script,
      })
    )?.path;

// (8) Revision. The `settings-` prefix allows logs and cards to distinguish it from model tool calls (`tool_*`) and hub launches (`launch-`).
  const toolCallId = `settings-${randomUUID()}`;
  const phaseNames = createWorkflowPhaseNames(graph);
  const phaseAlongside = phaseNames === undefined ? undefined : createWorkflowPhaseAlongside(graph);
  const subagentModel = parseWorkflowSubagentModel(next.subagentModel);
  let amended: Awaited<ReturnType<NonNullable<typeof port.amend>>>;
  try {
    amended = await port.amend({
      scriptText: script,
      cwd: this.workingDirectory,
      predecessorRunId: input.runId,
      parentSessionId: this.sessionId,
      toolCallId,
      ...(phaseNames === undefined ? {} : { phaseNames }),
      ...(phaseAlongside === undefined ? {} : { phaseAlongside }),
      ...(next.maxConcurrency === undefined ? {} : { maxConcurrency: next.maxConcurrency }),
      ...(subagentModel === undefined ? {} : { subagentModel }),
      ...(scriptPath === undefined ? {} : { scriptPath }),
// What is re-run is the precursor's own script, which reads the actual parameters when the precursor is started.
      inheritArgs: true,
      trace: traceContext,
    });
  } catch (error) {
    return {
      ok: false,
      reason: "start_failed",
      message: error instanceof Error ? error.message : String(error),
    };
  }
  if (!amended.ok) {
    return {
      ok: false,
      reason: amended.reason === "run_not_found" ? "not_found" : "missing_boundaries",
    };
  }

// Only use the run's own name: unnamed runs do not have names. Cards, side panels, and notifications follow the rules of any unnamed run and use the name.
// Here it has been summarized as `run <id>`, and the card and side panel title of the setting wheel have become a string of run ids.
// And this pseudonym will be passed down the revision chain.
  const trimmedName = snapshot.name?.trim();
  const name = trimmedName === undefined || trimmedName.length === 0 ? undefined : trimmedName;
  const amend: WorkflowSettingsAmendMeta = {
    predecessorRunId: input.runId,
    ...(modelChanged ? { subagentModel: fromTo(current.subagentModel, next.subagentModel) } : {}),
    ...(boundChanged
      ? { maxConcurrency: fromTo(current.maxConcurrency, next.maxConcurrency) }
      : {}),
    ...(ceiling === undefined ? {} : { ceiling }),
  };

// Failures after (9)(10) will only be logged and not rolled back: the new run is already flying and can be stopped on the side panel; withdrawing it will create orphans.
  try {
//Background tracking: synthesize an AmendWorkflow descriptor and go through the same registration of the executor (registry, final state waiter,
// settlement notification). `input.name` feeds the notification topic, the tool name to assign to "workflow".
    const toolCall: ExecutableToolCall = {
      id: toolCallId,
      name: AMEND_WORKFLOW_TOOL_NAME,
      input: { run_id: input.runId, ...(name === undefined ? {} : { name }) },
    };
    await this.executor.trackExternalBackgroundTask(
      toolCall,
      { backgroundTaskId: amended.runId, status: "backgrounded" },
      traceContext,
      undefined,
    );
    enqueueSettingsTurn.call(this, {
      text: buildSettingsMessageText({
        ...(name === undefined ? {} : { name }),
        previous: input.runId,
        runId: amended.runId,
        superseded: amended.supersededRunId !== undefined,
        amend,
      }),
      meta: boundWorkflowLaunchMeta({
        runId: amended.runId,
        toolCallId,
        ...(name === undefined ? {} : { name }),
        ...(() => {
          const display = displayOfAnalysis(analysis);
          return display?.kind === "create_workflow" ? { display } : {};
        })(),
        amend,
      }),
// The session must have been logged out at this point (it has run in its name), and the title seed will not be used; just give it an honest value.
      titleInput: name ?? input.runId,
      traceContext,
    });
  } catch (error) {
    this.logger?.error(
      "Workflow settings amended but post-amend bookkeeping failed",
      error instanceof Error ? error : new Error(String(error)),
      {
        ...traceContextToLogContext(traceContext),
        event: "dynamic_workflow.settings.post_amend_failed",
        module: "core.runtime",
        runId: amended.runId,
        toolCallId,
      },
    );
  }

  return {
    ok: true,
    runId: amended.runId,
    toolCallId,
    ...(amended.supersededRunId === undefined ? {} : { supersededRunId: amended.supersededRunId }),
  };
}

/**
* The setting wheel is not dropped directly in the command processing: the main agent may be in a round and the user message cannot be inserted. Queued into the runtime queue, when idle
* Run immediately, when busy, etc. when the current round ends; it has the same priority as the notification, so it precedes any notification of the new run.
 */
function enqueueSettingsTurn(
  this: AgentRuntimeInternal,
  turn: {
    text: string;
    meta: ReturnType<typeof boundWorkflowLaunchMeta>;
    titleInput: string;
    traceContext: TraceContext;
  },
): void {
  this.enqueueRuntimeCommand({
    branchGeneration: this.branchGeneration,
    createdAt: new Date(),
    id: createRuntimeCommandId(),
    inputId: uuidv7(),
    mode: "control-only-turn",
    priority: "next",
    text: turn.text,
    titleInput: turn.titleInput,
    traceContext: turn.traceContext,
    workflowLaunch: turn.meta,
  });
}

function fromTo<T>(from: T | undefined, to: T | undefined): { from?: T; to?: T } {
  return { ...(from === undefined ? {} : { from }), ...(to === undefined ? {} : { to }) };
}

/**
* Model surface canonical sentence for setting wheels (English, not localized - it goes into provider transcript). Only the changed settings are mentioned; two `null`
* Each has a sentence. The last sentence dissuades the model from running this run again: it is already running, and the progress will be reported back.
 */
function buildSettingsMessageText(input: {
  name?: string;
  previous: string;
  runId: string;
  superseded: boolean;
  amend: WorkflowSettingsAmendMeta;
}): string {
  const changes: string[] = [];
  const model = input.amend.subagentModel;
  if (model !== undefined) {
    changes.push(
      model.to === undefined
        ? "its subagents are back on the session model"
        : `its subagents now run on ${model.to}`,
    );
  }
  const bound = input.amend.maxConcurrency;
  if (bound !== undefined) {
    changes.push(
      bound.to === undefined
        ? "the limit on subagents at once is removed"
        : `at most ${bound.to} of them run at once`,
    );
  }
  const relation = input.superseded ? "supersedes" : "takes over from";
  return [
    `Changed the settings of workflow run ${input.previous}${input.name === undefined ? "" : ` ("${input.name}")`} from the GUI: ${changes.join("; ")}.`,
    `The same script continues as run ${input.runId}, which ${relation} run ${input.previous} and imports everything run ${input.previous} finished as cache.`,
    "Progress and results arrive as background notifications; do not amend, resume or restart it.",
  ].join(" ");
}

/**
* Two settings on the snapshot. Both are "absent": no model specified = session model, bounds not lower than the ceiling = no bounds of its own,
* So absence is the default, which is the same as {@link RunSettings}.
 */
function runSettingsOfSnapshot(snapshot: DynamicWorkflowRunSnapshot): RunSettings {
  return {
    ...(snapshot.subagentModel === undefined ? {} : { subagentModel: snapshot.subagentModel }),
    ...(snapshot.maxConcurrency === undefined ? {} : { maxConcurrency: snapshot.maxConcurrency }),
  };
}
