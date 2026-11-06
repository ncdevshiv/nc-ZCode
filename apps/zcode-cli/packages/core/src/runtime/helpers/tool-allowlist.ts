import {
  AMEND_WORKFLOW_TOOL_NAME,
  CREATE_WORKFLOW_TOOL_NAME,
  RESOLVE_WORKFLOW_QUESTION_TOOL_NAME,
  RESPOND_TO_COORDINATOR_TOOL_NAME,
  RESUME_WORKFLOW_RUN_TOOL_NAME,
  SAVE_WORKFLOW_TOOL_NAME,
} from "@zcode/contracts";
import { EXPLORE_AGENT_ALLOWED_TOOLS } from "../../subagent/explore-tools.js";
import type { AgentRuntimeConfig } from "../types.js";
import { normalizeToolNameAlias } from "../../tool/tool-visibility.js";

const EXPLORE_AGENT_ALLOWED_TOOL_SET = new Set<string>(EXPLORE_AGENT_ALLOWED_TOOLS);

/**
* Additional tools that are not registered at workflow child runtime.
 *
* Root cause: workflow child (agent call of `/workflow`, `/expert` and script workflow) is forced to
* yolo, while its interaction events are only mirrored to the parent session on the subagent path (`runtime/methods/subagent.ts` is
* The only access point for `mirrorSubagentToolEvent`). CreateWorkflow declares alwaysAsk, so a section can be compiled
* The script will issue a confirmation request in the child that cannot be seen by the parent interface, and hang all the way until the permission times out. Do not register the tool directly.
* The child gets a clean "tool unavailable" error instead of an invisible hang.
 *
* The long-term solution is to mirror the interaction events of the workflow child to the parent session and process them when the execution engine is implemented.
 */
const WORKFLOW_CHILD_DISALLOWED_TOOLS = [
  CREATE_WORKFLOW_TOOL_NAME,
// AmendWorkflow and CreateWorkflow have the same alwaysAsk gate, the same nested arrangement, and are enqueued due to the same root cause.
  AMEND_WORKFLOW_TOOL_NAME,
// SaveWorkflow is enqueued due to the same root cause: it also declares alwaysAsk, so in the child it will also emit an
// The confirmation request cannot be seen by the parent interface and will time out. ListSavedWorkflows is not listed - the reason for that ban is that there is no window to pop up,
// Read-only queries are not applicable (same as the two run introspection tools).
  SAVE_WORKFLOW_TOOL_NAME,
// **Structural Disabled** - No further layout is allowed within the child. Recovery = re-execute the entire script (completed node replay, unfinished
//Redistribute), which is the same capability file as CreateWorkflow new startup; even if the technical problem of "no window to pop up" disappears after no confirmation is required,
// Nested arrangement (child is pulled up or resurrected again) is still not open.
  RESUME_WORKFLOW_RUN_TOOL_NAME,
// ResolveWorkflowQuestion and ResumeWorkflowRun are both **structurally disabled**, but they adhere to another invariant:
// The semantics of upgraded Q&A are "the actor asks a question, and the party who created this workflow answers".
// Let a child answer easily, which is equivalent to "turning the power of judgment to the person who can change the door" quietly degenerated into a conflict between actors.
// Persuade each other - and an actor who is also blocked is the one least qualified to make a decision.
  //
// **Intentional duplication** with bootstrap's `ACTOR_DISALLOWED_TOOLS` (CreateWorkflow already has the same double column
// Precedent): That one is the subtraction of the persona tool surface on the driver side; this one covers all workflows by taskType
// child, does not depend on driver, remember to write it.
  RESOLVE_WORKFLOW_QUESTION_TOOL_NAME,
] as const;

/**
* Final tool disallow list at runtime: turn-level `toolDisallowlist` overlays structural disables derived by taskType.
* is placed here instead of the construction points of each child runtime because there are two construction points
* (`workflow-facade.ts` and `script-workflow-child-runtime.ts`), the two lists will inevitably drift.
 */
export function resolveRuntimeDisallowedTools(
  config: AgentRuntimeConfig,
): readonly string[] | undefined {
  if (config.taskType !== "workflow_child") return config.toolDisallowlist;

  const disallowed = new Set(config.toolDisallowlist ?? []);
  for (const toolName of WORKFLOW_CHILD_DISALLOWED_TOOLS) disallowed.add(toolName);
  return [...disallowed];
}

/**
* The value of dynamic workflow grayscale gate on registerBuiltInTools.
* **Enable in absence**: TUI, headless `-p` and workflow_child do not write this field, they must retain the complete tool surface;
* Only protocol sessions created by trusted hosts will explicitly write false. The default value of fail-closed is on the protocol server side.
* appRuntimePreferences, not at this level.
 *
* The reason why it is stored here like resolveRuntimeDisallowedTools instead of written at the call point: there are **two** entrances on the registration page
* (First assembly of helpers/runtime-tools.ts, branch refresh of methods/embedded-search-branch.ts),
*Writing two copies each will inevitably drift. I just floated it once - I missed this field when I refreshed it, so
* "Open in absence" adds the ten tools back to the registry as they are, and the model is still adjusted to ListSavedWorkflows in the grayscale closed session.
 */
export function resolveRuntimeDynamicWorkflowToolsIncluded(config: AgentRuntimeConfig): boolean {
  return config.dynamicWorkflowEnabled !== false;
}

export function resolveBuiltInToolAllowlist(
  config: AgentRuntimeConfig,
): readonly string[] | undefined {
  const normalizedAllowlist = normalizeBuiltInToolAllowlist(config.toolAllowlist);

  if (config.toolset !== "explore") {
    return appendChildControlTool(config, normalizedAllowlist);
  }

  if (!normalizedAllowlist) {
    return appendChildControlTool(config, EXPLORE_AGENT_ALLOWED_TOOLS);
  }

  return appendChildControlTool(
    config,
    normalizedAllowlist.filter((toolName) => EXPLORE_AGENT_ALLOWED_TOOL_SET.has(toolName)),
  );
}

function appendChildControlTool(
  config: AgentRuntimeConfig,
  allowlist: readonly string[] | undefined,
): readonly string[] | undefined {
  if (!allowlist) {
    return allowlist;
  }

  if (config.taskType === "subagent_child") {
    if (allowlist.includes(RESPOND_TO_COORDINATOR_TOOL_NAME)) {
      return allowlist;
    }
// Explore will seek tool intersection again before runtime registration, and the child control tool must make up for the final result.
    return [...allowlist, RESPOND_TO_COORDINATOR_TOOL_NAME];
  }

// workflow child does not have allowlist narrowing (persona has no tool gear, tool surface only has subtraction, see
// bootstrap workflow-actor-tools.ts), submit_result / escalate by includeSubmitResult /
// includeEscalate Two gates are registered, no need to make up for it here.
  return allowlist;
}

function normalizeBuiltInToolAllowlist(
  allowlist: readonly string[] | undefined,
): readonly string[] | undefined {
  return allowlist?.map((toolName) => normalizeToolNameAlias(toolName));
}
