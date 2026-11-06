// ============================================================
// Tool surface for workflow actor (AgentRuntime tool configuration)
// ============================================================
//
// The tool surface of the child agent must fall on the tool registration of the child runtime, otherwise the real offer transcript
//The referee actor here also gets a complete set of interactive tools. Two types of risks:
// 1. Suspension: AskUserQuestion / EnterPlanMode There is no one to ask in the headless child, and the turn never ends;
// 2. Override and recursion: CreateWorkflow allows the actor to submit another workflow, and ReadSessionContext reads the parent session out of bounds.
// This module is the missing mapping, which is expanded by the runtime factory on the driver side when creating AgentRuntime.
//
// persona tool-less gear: each actor takes the complete working toolset minus the following subtraction table;
// "The referee should not change the file" is made clear by the ask text - this is also done by ordinary subagents (Explore retains Bash,
// Read only by prompt).

import {
  ASK_USER_QUESTION_TOOL_NAME,
  ENTER_PLAN_MODE_TOOL_NAME,
  EXIT_PLAN_MODE_TOOL_NAME,
  READ_SESSION_CONTEXT_TOOL_NAME,
  RESOLVE_WORKFLOW_QUESTION_TOOL_NAME,
} from "@zcode/contracts";

/** Toolface slice of AgentRuntimeConfig. */
interface WorkflowActorToolPolicy {
  toolDisallowlist: readonly string[];
}

/**
* Tools subtracted from the collection: the first three would be stuck on a non-existent human (headless child, no one answered,
* turn does not end); CreateWorkflow will make the actor recursively submit the workflow; ReadSessionContext reads the parent session out of bounds.
* Leave the rest (Bash/Edit/Write/Search/web) as usual - actors are there to do the work.
 */
const ACTOR_DISALLOWED_TOOLS: readonly string[] = [
  ASK_USER_QUESTION_TOOL_NAME,
  ENTER_PLAN_MODE_TOOL_NAME,
  EXIT_PLAN_MODE_TOOL_NAME,
  "CreateWorkflow",
// The revision entry is the same nested arrangement as CreateWorkflow, and the same root cause is listed.
  "AmendWorkflow",
  READ_SESSION_CONTEXT_TOOL_NAME,
// Subagents are not allowed to answer upgrade questions for the main agent.
// Different from the root causes of the above items: this is not suspension or unauthorized reading, but **identity** - the whole meaning of the upgrade is to transfer the right to judge
//Leave it to the party who created this workflow; letting another actor answer it is equivalent to quietly degenerating it into a communication between actors.
// Persuade each other. Actors use `escalate` (constant registration) to ask questions, and answers only belong to the main session.
  RESOLVE_WORKFLOW_QUESTION_TOOL_NAME,
];

/**
* AgentRuntime tool configuration for workflow actors. Pure function for driver-side runtime factory expansion
* AgentRuntimeConfig: Do not narrow, only remove interactive/meta-tools that will hang or exceed authority.
 *
* Covers only built-in tools; filtering of MCP/plugin tools is left to production wiring (same factory seam).
 */
export function workflowActorToolPolicy(): WorkflowActorToolPolicy {
  return { toolDisallowlist: ACTOR_DISALLOWED_TOOLS };
}
