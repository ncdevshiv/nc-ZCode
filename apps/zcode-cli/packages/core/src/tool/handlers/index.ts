// ============================================================
// Built-in Tool Handlers
// ============================================================

import {
  AMEND_WORKFLOW_TOOL_NAME,
  CREATE_WORKFLOW_TOOL_NAME,
  EVAL_WORKFLOW_SNIPPET_TOOL_NAME,
  GET_WORKFLOW_RUN_TOOL_NAME,
  LIST_MODELS_TOOL_NAME,
  LIST_SAVED_WORKFLOWS_TOOL_NAME,
  LIST_WORKFLOW_RUNS_TOOL_NAME,
  RESOLVE_WORKFLOW_QUESTION_TOOL_NAME,
  RESUME_WORKFLOW_RUN_TOOL_NAME,
  SAVE_WORKFLOW_TOOL_NAME,
  SUBMIT_RESULT_TOOL_NAME,
  type JsonSchema,
} from "@zcode/contracts";
import type { ToolEntry } from "../types.js";
import type { AgentProfile } from "../../subagent/profile.js";
import { readToolEntry } from "./read.js";
import { writeToolEntry } from "./write.js";
import { editToolEntry } from "./edit.js";
import { bashToolEntry, createBashToolEntry } from "./bash.js";
import type { BashTimeoutPolicy } from "../bash-timeout-policy.js";
import { createJsToolEntry, jsToolEntry } from "./node-repl.js";
import { globToolEntry } from "./glob.js";
import { grepToolEntry } from "./grep.js";
import { webFetchToolEntry } from "./webfetch.js";
import { webSearchToolEntry } from "./websearch.js";
import {
  agentToolEntry,
  createAgentToolEntry,
  createTaskToolEntry,
  taskToolEntry,
} from "./agent.js";
import { isSubagentDispatchToolName } from "../compat.js";
import { skillToolEntry } from "./skill.js";
import { todoReadToolEntry, todoWriteToolEntry } from "./todo.js";
import {
  cronCreateToolEntry,
  cronDeleteToolEntry,
  cronListToolEntry,
  cronUpdateToolEntry,
} from "./cron.js";
import { offPeakCreateToolEntry, offPeakListToolEntry } from "./off-peak.js";
import {
  createEnterPlanModeToolEntry,
  enterPlanModeToolEntry,
  exitPlanModeToolEntry,
} from "./plan-mode.js";
import { askUserQuestionToolEntry } from "./ask-user-question.js";
import { sendMessageToolEntry } from "./send-message.js";
import { respondToCoordinatorToolEntry } from "./respond-to-coordinator.js";
import { createSubmitResultToolEntry, submitResultToolEntry } from "./submit-result.js";
import { escalateToolEntry } from "./escalate.js";
import { resolveWorkflowQuestionToolEntry } from "./resolve-workflow-question.js";
import { taskOutputToolEntry } from "./task-output.js";
import { taskStopToolEntry } from "./task-stop.js";
import { readSessionContextToolEntry } from "./read-session-context.js";
import { amendWorkflowToolEntry } from "./amend-workflow.js";
import { createWorkflowToolEntry } from "./create-workflow.js";
import { saveWorkflowToolEntry } from "./save-workflow.js";
import { listSavedWorkflowsToolEntry } from "./list-saved-workflows.js";
import { listModelsToolEntry } from "./list-models.js";
import { evalWorkflowSnippetToolEntry } from "./eval-workflow-snippet.js";
import { listWorkflowRunsToolEntry } from "./list-workflow-runs.js";
import { getWorkflowRunToolEntry } from "./get-workflow-run.js";
import { resumeWorkflowRunToolEntry } from "./resume-workflow-run.js";
// import { workflowToolEntry } from "./workflow.js";
import { createToolRuleNameSet } from "../tool-visibility.js";

// The direct branch retains the Glob/Grep tool implementation; the embedded search branch is implemented by registerBuiltInTools
//Hide Glob/Grep uniformly and take over the search through Bash find/grep.

export const builtInTools: ToolEntry[] = [
  readToolEntry,
  writeToolEntry,
  editToolEntry,
  // applyPatchToolEntry,
  bashToolEntry,
  globToolEntry,
  grepToolEntry,
  webFetchToolEntry,
  webSearchToolEntry,
  todoReadToolEntry,
  todoWriteToolEntry,
  cronCreateToolEntry,
  cronListToolEntry,
  cronUpdateToolEntry,
  cronDeleteToolEntry,
  offPeakCreateToolEntry,
  offPeakListToolEntry,
  enterPlanModeToolEntry,
  exitPlanModeToolEntry,
  askUserQuestionToolEntry,
  sendMessageToolEntry,
  respondToCoordinatorToolEntry,
  submitResultToolEntry,
// Upgrade channel for actors. Completely isomorphic to submit_result:
// The port is registered when present (includeEscalate), and is determined by the allowlist of workflow_child under `tools:"none"`
// Make up for the logic and save it. Default disallow for actors - the actors most likely to hit an unforeseen wall are precisely
// The one not marked by the author.
  escalateToolEntry,
  taskOutputToolEntry,
  taskStopToolEntry,
  readSessionContextToolEntry,
  agentToolEntry,
  taskToolEntry,
  skillToolEntry,
  jsToolEntry,
  createWorkflowToolEntry,
  amendWorkflowToolEntry,
// Saved definition: The write-side gate is in the same file as CreateWorkflow (alwaysAsk), and there is no gate on the read-side.
  saveWorkflowToolEntry,
// Experimental channels for workflow creation: synchronous, read-only (v1), fully transient.
  evalWorkflowSnippetToolEntry,
// run two read-only tools for introspection: always-on, gateless. They don't enter WORKFLOW_CHILD_DISALLOWED_TOOLS——
// The reason for that ban is that CreateWorkflow's alwaysAsk has no window to pop up in the sub-runtime, and read-only queries are not applicable.
  listWorkflowRunsToolEntry,
  getWorkflowRunToolEntry,
// The recovery entry of run: the same family as the above two read-only introspection tools (same model for run_id key and port detection failure), but it is
// Execution semantics - alwaysAsk must be yolo (cancelled is the user's explicit stop decision, and resurrection must be asked first),
// Therefore, WORKFLOW_CHILD_DISALLOWED_TOOLS must be entered (child yolo has no window to pop up). Inserted after GetWorkflowRun: run tool cluster next to list/get/resume.
  resumeWorkflowRunToolEntry,
// Upgrade the main agent side of Q&A: the same family as the above three run tools——
// The same dwf run port and the same typeof detection failed. The difference with them is downstream: it enters the actor session
// Disable the list (bootstrap's workflowActorToolPolicy), subagents are not allowed to answer for the main agent.
  resolveWorkflowQuestionToolEntry,
// Definition list (two different things from the two run tools above: that's history, that's something runnable). Both are read-only and have no gate.
  listSavedWorkflowsToolEntry,
// Model directory: also a read-only, gate-less discovery surface, serving CreateWorkflow / AmendWorkflow
// `subagent_model`. Not entering WORKFLOW_CHILD_DISALLOWED_TOOLS
// ——The reason for that ban is that alwaysAsk has no window to pop up in the child, and read-only queries are not applicable.
  listModelsToolEntry,
  // workflowToolEntry,
];

/**
* Ten tools that do not register when the dynamic workflow grayscale gate is closed.
* The semantics of grayscale is "there is no way to start a workflow", so create, revise, save, snapshot experiments and four
* The run tools are removed from the shelves together; the read-only run introspection tools are also listed, because in the closed state they will only point to the history that the user can no longer operate.
* `ListModels` is also listed: its only purpose is to select `subagent_model` for a run, leaving it when there is no CreateWorkflow to fill in will only lead the model to a non-existent tool.
* The old `Workflow` tool (`/expert` script channel) is another feature that is **not** on this list.
 */
const DYNAMIC_WORKFLOW_TOOL_NAMES: ReadonlySet<string> = new Set([
  CREATE_WORKFLOW_TOOL_NAME,
  AMEND_WORKFLOW_TOOL_NAME,
  SAVE_WORKFLOW_TOOL_NAME,
  LIST_SAVED_WORKFLOWS_TOOL_NAME,
  LIST_MODELS_TOOL_NAME,
  EVAL_WORKFLOW_SNIPPET_TOOL_NAME,
  LIST_WORKFLOW_RUNS_TOOL_NAME,
  GET_WORKFLOW_RUN_TOOL_NAME,
  RESUME_WORKFLOW_RUN_TOOL_NAME,
  RESOLVE_WORKFLOW_QUESTION_TOOL_NAME,
]);

interface RegisterBuiltInToolsOptions {
  bashTimeoutPolicy?: BashTimeoutPolicy;
  includeSkill?: boolean;
  includeAgent?: boolean;
  includeSendMessage?: boolean;
  includeRespondToCoordinator?: boolean;
  includeSubmitResult?: boolean;
  /**
* When present submit_result is registered with typed declaration (`{ result: <schema> }`, strict qualification) for dwf mono
* Subagent; absence means universal declaration. Only meaningful when includeSubmitResult is true.
   */
  submitResultSchema?: JsonSchema;
/** Actor's upgrade channel; the gate is the same as includeSubmitResult (registered only after WorkflowEscalatePort is injected). */
  includeEscalate?: boolean;
  includeWorkflow?: boolean;
  includeAutomation?: boolean;
/** Off-Peak creates tool surfaces within the session; driven by the host's offPeakToolEnabled flag (grayscale/remote gate). */
  includeOffPeak?: boolean;
  /**
* Dynamic workflow grayscale gate. **Only explicit false
* Only removed** DYNAMIC_WORKFLOW_TOOL_NAMES: Absence means that the caller does not participate in grayscale (TUI, headless,
* workflow_child), they must retain all tool surfaces; the default value of fail-closed falls on the protocol server
* appRuntimePreferences, not at this level.
   */
  includeDynamicWorkflow?: boolean;
/** node_repl (js) is turned off by default, enabled by the official browser-use plugin. */
  includeNodeRepl?: boolean;
/** browser-use instructions and agent.browsers injection are enabled by the official browser-use plugin + the host browser bridge. */
  includeBrowserUse?: boolean;
  embeddedSearchEnabled?: boolean;
  agentProfiles?: readonly AgentProfile[];
  allowedTools?: readonly string[];
  disallowedTools?: readonly string[];
  silentDuplicateWarnings?: boolean;
}

export function registerBuiltInTools(
  registry: {
    register(entry: ToolEntry, options?: { silentDuplicateWarning?: boolean }): void;
  },
  options: RegisterBuiltInToolsOptions = {},
): void {
  const allowedTools = options.allowedTools ? new Set(options.allowedTools) : undefined;
  const disallowedTools = createToolRuleNameSet(options.disallowedTools);

  for (const entry of builtInTools) {
    if (
      options.embeddedSearchEnabled === true &&
      (entry.metadata.name === "Glob" || entry.metadata.name === "Grep")
    ) {
      continue;
    }
    if (allowedTools && !allowedTools.has(entry.metadata.name)) {
      continue;
    }
    if (disallowedTools?.has(entry.metadata.name)) {
      continue;
    }
    if (isSubagentDispatchToolName(entry.metadata.name) && options.includeAgent !== true) {
      continue;
    }
    if (entry.metadata.name === "Skill" && options.includeSkill === false) {
      continue;
    }
    if (entry.metadata.name === "SendMessage" && options.includeSendMessage !== true) {
      continue;
    }
    if (
      entry.metadata.name === "RespondToCoordinator" &&
      options.includeRespondToCoordinator !== true
    ) {
      continue;
    }
    if (entry.metadata.name === "submit_result" && options.includeSubmitResult !== true) {
      continue;
    }
    if (entry.metadata.name === "escalate" && options.includeEscalate !== true) {
      continue;
    }
    if (entry.metadata.name === "Workflow" && options.includeWorkflow !== true) {
      continue;
    }
    if (
      (entry.metadata.name === "CronCreate" ||
        entry.metadata.name === "CronList" ||
        entry.metadata.name === "CronUpdate" ||
        entry.metadata.name === "CronDelete") &&
      options.includeAutomation !== true
    ) {
      continue;
    }
    if (
      (entry.metadata.name === "OffPeakCreate" || entry.metadata.name === "OffPeakList") &&
      options.includeOffPeak !== true
    ) {
      continue;
    }
    if (
      options.includeDynamicWorkflow === false &&
      DYNAMIC_WORKFLOW_TOOL_NAMES.has(entry.metadata.name)
    ) {
      continue;
    }
    if (entry.metadata.name === "js" && options.includeNodeRepl !== true) {
      continue;
    }
    registry.register(resolveBuiltInToolEntryForBranch(entry, options), {
      silentDuplicateWarning: options.silentDuplicateWarnings,
    });
  }
}

function resolveBuiltInToolEntryForBranch(
  entry: ToolEntry,
  options: RegisterBuiltInToolsOptions,
): ToolEntry {
  if (entry.metadata.name === "Bash") {
    return createBashToolEntry({
      bashTimeoutPolicy: options.bashTimeoutPolicy,
      embeddedSearchEnabled: options.embeddedSearchEnabled,
    });
  }
  if (entry.metadata.name === SUBMIT_RESULT_TOOL_NAME && options.submitResultSchema !== undefined) {
    return createSubmitResultToolEntry(options.submitResultSchema);
  }
// The grayscale gate manages both the tool surface and **description**: the description of Agent/Task contains a statement "Workflow requests must be used instead."
//CreateWorkflow", that tool does not exist when it is closed, leaving it will only point the model to the non-existent tool. Use the same one as the registration filter
// options.includeDynamicWorkflow, so the description of the first assembly and branch refresh output must be consistent.
  if (entry.metadata.name === "Agent") {
    return createAgentToolEntry({
      embeddedSearchEnabled: options.embeddedSearchEnabled,
      profiles: options.agentProfiles,
      dynamicWorkflowEnabled: options.includeDynamicWorkflow !== false,
    });
  }
  if (entry.metadata.name === "Task") {
    return createTaskToolEntry({
      embeddedSearchEnabled: options.embeddedSearchEnabled,
      profiles: options.agentProfiles,
      dynamicWorkflowEnabled: options.includeDynamicWorkflow !== false,
    });
  }
  if (entry.metadata.name === "EnterPlanMode") {
    return createEnterPlanModeToolEntry({
      embeddedSearchEnabled: options.embeddedSearchEnabled,
    });
  }
  if (entry.metadata.name === "js") {
    return createJsToolEntry({
      browserUseEnabled: options.includeBrowserUse === true,
    });
  }
  return entry;
}
