// ============================================================
// Workflow Actor Identity Section Builder
// ============================================================
//
// The identity segment of the dynamic workflow child (workflow child).
//
// It replaces the interactive Agent Identity ("You are an interactive ZCode agent that helps
// users"): The subagent's readers are scripts, not people. It **does** not replace other segments of the base - safe IMPORTANT lines with
// `# Harness` blocks are reused verbatim from identity, and memory / skills / project directives are appended by the builder as usual.
// The persona written by the author is superimposed after the opening sentence and before the contract: the character is more frontward and eye-catching than the general rules, but the opening sentence puts it first
// "Who do you belong to, and who do you export it to?" Persona cannot overturn it.
//
//The contract was branched according to persona's tool level; the GLM subagent with zero tools in the real offer was
// "Ground every claim in something you read or ran" forces you to read directories and run commands, but it does not have these tools.
// So emit a degenerate `escalate("placeholder")`. The method of practice is to talk about the tool surface to death: the sub-agent first knows what he has,
// Then be told where the evidence comes from. After the tool stall exits,
// Each sub-agent has a complete set of working tools, and the contract returns to a text; the principle of "talking about the tool surface" remains unchanged.

import type { ContextSection, WorkflowActorContext } from "../types.js";
import { estimateTokens } from "../utils.js";
import { buildHarnessBlock, buildSecurityNotice } from "./identity.js";

/** The self-description on the tool surface: one sentence "something" + one sentence "nothing" to prevent the model from guessing. */
const TOOL_SURFACE =
  "You have the regular working tools — reading, searching, editing, running commands — plus `submit_result` and `escalate`. There is no tool that asks a person anything.";

/** Evidence standard: Quote what you have read/ran. If you bring your own materials with ask, you will cite the materials. You have to run it to count. Run according to the scale of ask. */
const EVIDENCE_RULE =
// "You have to run before you have passed" is about honesty, regardless of scale——
// The real disk agent takes a test file and a build to replace the entire package named by ask. Add a word about the scale.
  "Ground every claim in something you read or ran in this session, or in the material the ask gave you, and say which. Cite code as `path:line`. A check counts as passed only if you executed it here; if you could not run it, report it as not run. Run the check an ask names rather than a faster substitute, and say exactly which command you ran.";

function buildWorkflowContract(): string {
  return [
    "# Working inside a workflow",
    `- ${TOOL_SURFACE}`,
    "- Each ask states what to do. When the ask carries a result schema, finish by calling `submit_result` with a conforming value; otherwise your final message is the result.",
    `- ${EVIDENCE_RULE}`,
    "- Report outcomes faithfully. If part of the task is impossible, out of scope, or contradicted by what you found, say so in the result instead of filling a field with a plausible guess. Never fake a passing result to satisfy an instruction.",
    "- When you are blocked by something outside your reach — a gate that cannot pass, instructions that contradict each other, a fact only the run's owner knows — call `escalate`. Questions written in prose reach nobody.",
// Product terms prevent "documents written by sub-agents" from being both discouraged and not tracked.
// Become a channel with an exit: the subagent still does not have any production tools (only scripts can be published, the trust boundary does not change), but
// When ask specifies an output path, write there and hand the path back, and the script will publish it to the user.
    "- Do not write report or summary files on your own initiative; findings go in the result. When the ask names an output path, write exactly there and return that path in the result — the script publishes it to the user.",
  ].join("\n");
}

function buildWorkflowActorIdentityPrompt(actor: WorkflowActorContext): string {
  const name = actor.name?.trim();
  const named = name ? `, named "${name}"` : "";
  const opening = [
    `You are a subagent inside a dynamic workflow run${named}. A script created you and hands you work one ask at a time; the script — not a person — consumes what you return. There is no user in this conversation to talk to.`,
  ];
  const persona = actor.persona?.trim();
// No more CLI prefix ahead ("You are ZCode, an interactive coding agent"
// The identity of the sub-agent is wrong), so this paragraph is the first line of system and no longer starts with a blank line.
  const parts = [
    opening.join("\n"),
    ...(persona ? ["", persona] : []),
    "",
    buildSecurityNotice(),
    "",
    buildHarnessBlock(),
    "",
    buildWorkflowContract(),
  ];
  return parts.join("\n");
}

export function buildWorkflowActorIdentitySection(actor: WorkflowActorContext): ContextSection {
  const content = buildWorkflowActorIdentityPrompt(actor);
  return {
    name: "Workflow Actor Identity",
    source: "workflow_actor_identity",
    injectionTarget: "system",
    cacheHint: "stable",
    chars: content.length,
    tokens: estimateTokens(content),
    content,
    preview: content.slice(0, 100),
  };
}
