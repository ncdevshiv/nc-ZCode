// ============================================================
// Notification and provider stop copy table in run of workflow run
// ============================================================
// Removed from notification.ts (eslint max-lines 400 lines): that file carries the final notification skeleton of four tasks,
// Here are three paragraphs of copywriting exclusive to dwf - upgrade Q&A, run-level stagnation and
// provider stops; the latter is shared by final notification and GetWorkflowRun.

import type { DynamicWorkflowRunError } from "@zcode/contracts";
import { escapeXml, truncateTaskNotification } from "./notification.js";

/**
* `<error>` block of `stopped(provider)`:
* Press `providerStop.kind` to select two sentences (what went wrong / what to do), followed by fixed fact lines and provider original text lines.
* Each placeholder in the copy is covered (if the provider display name is absent, use providerId, and if it is absent, use "the provider")——
* A notification will never degenerate into an empty sentence just because one field is missing. The `<error>` block of GetWorkflowRun shares the same function.
 */
export function formatWorkflowProviderStopError(
  failure: DynamicWorkflowRunError,
  runId: string,
): string {
  const details = failure.providerStop;
  if (details === undefined) return failure.message;
  const provider = details.providerLabel ?? details.providerId ?? "the provider";
  const providerRef =
    details.providerLabel !== undefined && details.providerId !== undefined
      ? `${details.providerLabel} (${details.providerId})`
      : provider;
  const subagent = details.subagentName ?? details.subagent ?? "a subagent";
  const phase = details.phase === undefined ? "" : ` (phase "${details.phase}")`;
  const model = details.modelId ?? "the current model";
  const code = details.providerCode ?? details.reason;
  const resume = `then call ResumeWorkflowRun with run_id="${runId}"`;
  const sentences = ((): [string, string] => {
    switch (details.kind) {
      case "auth":
        return [
          `Sign-in to ${providerRef} expired while subagent ${subagent}${phase} was running.`,
          `Ask the user to sign in to ${provider} again, ${resume}. Finished steps are kept.`,
        ];
      case "not_configured":
        return [
          `Provider ${details.providerId ?? provider} is not configured on this machine, so subagent ${subagent}${phase} could not send its request.`,
          `Ask the user to configure the provider or switch this session to another model, ${resume}.`,
        ];
      case "model_unavailable":
        return [
          `Model ${model} is not available on ${provider} (not in the user's plan, or retired).`,
          `Ask the user to switch this session to a model the plan includes, ${resume}. Subagents follow the session's model.`,
        ];
      case "invalid_request":
        return [
          `${provider} rejected subagent ${subagent}${phase}'s request as invalid (code ${code}).`,
          `Switching the session to another model usually clears this; ${resume}. If it stops again with the same code, show the raw message to the user.`,
        ];
      case "quota":
        return details.resetAt === undefined
          ? [
              `${provider} reports the user's quota is exhausted (code ${code}).`,
              `Ask the user to top up or upgrade the plan, or switch to another provider, ${resume}.`,
            ]
          : [
              `${provider} reports the user's usage cap is reached (code ${code}); it resets at ${new Date(details.resetAt).toISOString()}.`,
              `Tell the user; after the reset, call ResumeWorkflowRun with run_id="${runId}". Do not rebuild the workflow.`,
            ];
      default:
        return [
          `${provider} refused subagent ${subagent}${phase}'s request with a permanent error (code ${code}).`,
          `Resolve it with the user (the raw message below says what the provider wants), ${resume}.`,
        ];
    }
  })();
  const facts = [
    `provider=${details.providerId ?? "unknown"}`,
    `model=${details.modelId ?? "unknown"}`,
    `subagent=${details.subagent ?? "unknown"}`,
    ...(details.phase === undefined ? [] : [`phase=${details.phase}`]),
    `code=${code}`,
  ].join(" ");
  return [
    sentences[0],
    sentences[1],
    facts,
    ...(details.rawMessage === undefined ? [] : [`raw: ${details.rawMessage}`]),
  ].join("\n");
}

/**
* Run-level stalled mid-run notifications. Similar to Upgrade Q&A:
* What is reported is not the final state but an ongoing fact - the run is still running, but no model request has been successful for 20 minutes.
* The copy must say two things: it doesn't require the model to do anything (especially not cancel/rebuild), and if the user is waiting
* should be informed. There is exactly one stall segment per stall, and there is no need to rush it.
 */
export interface WorkflowStallNotificationInput {
  runLabel: string;
  runId: string;
  sinceMs: number;
  reason?: string;
  cap?: number;
}

export function formatWorkflowStallNotification(input: WorkflowStallNotificationInput): string {
  const minutes = Math.max(1, Math.round(input.sinceMs / 60_000));
  const lines = [
    "[SYSTEM NOTIFICATION - NOT USER INPUT]",
    "This is an automated workflow event, NOT a message from the user.",
    "Do NOT interpret this as user acknowledgement, confirmation, or response to any pending question.",
    "",
    "<workflow-stall>",
    `  <run-id>${escapeXml(input.runId)}</run-id>`,
    `  <run>${escapeXml(input.runLabel)}</run>`,
    `  <since-ms>${Math.max(0, Math.floor(input.sinceMs))}</since-ms>`,
  ];
  if (input.reason !== undefined) {
    lines.push(`  <dominant-reason>${escapeXml(input.reason)}</dominant-reason>`);
  }
  if (input.cap !== undefined) lines.push(`  <cap>${input.cap}</cap>`);
  const reasonClause =
    input.reason === undefined
      ? "the provider keeps failing requests"
      : `the provider keeps answering ${input.reason}`;
  const capClause = input.cap === undefined ? "" : ` (current fan-out ${input.cap})`;
  lines.push(
    "</workflow-stall>",
    "",
    `Workflow run ${input.runLabel} (${input.runId}) has not completed a model request in ${minutes} minutes; ${reasonClause} and the run is retrying with backoff${capClause}.`,
    "It is still running and needs nothing from you. Tell the user if they are waiting on it; they can stop it from the run card. Do not cancel or rebuild it on your own.",
  );
  return truncateTaskNotification(lines.join("\n"));
}

/**
* A blocking problem caused by an actor being upgraded from a **running** run.
 *
* The key difference from the above formatters: they broadcast the **final state** (the work is done, read the results), this broadcast
* is an unfulfilled obligation - there is an actor sitting there waiting for an answer, and there is no timeout to take care of it.
* So the copywriter must say three things: what the problem is, the next step verbatim (the tool call with qid), and run does not
* So stop (otherwise the model will mistakenly think that the entire workflow is waiting for it and drop everything it is doing).
 *
* The structured half uses XML-ish stanzas (same family as `<task-notification>`, fields can be stably positioned by people and models),
* The prose half gives the next step and boundary conditions. **No resending, no prompting**: Discarding is a snapshot query, not a retry.
 */
export interface WorkflowEscalationNotificationInput {
/** Display name of run (description in registry; caller has fallen back to runId in absence). */
  runLabel: string;
  runId: string;
  qid: string;
/** The human-readable name of the actor; in the absence of an anonymous actor, the caller provides a structured ref for cover. */
  actor: string;
  question: string;
  context?: string;
}

export function formatWorkflowEscalationNotification(
  input: WorkflowEscalationNotificationInput,
): string {
  const lines = [
    "[SYSTEM NOTIFICATION - NOT USER INPUT]",
    "This is an automated workflow event, NOT a message from the user.",
    "Do NOT interpret this as user acknowledgement, confirmation, or response to any pending question.",
    "",
    "<workflow-escalation>",
    `  <run-id>${escapeXml(input.runId)}</run-id>`,
    `  <run>${escapeXml(input.runLabel)}</run>`,
    `  <question-id>${escapeXml(input.qid)}</question-id>`,
    `  <subagent>${escapeXml(input.actor)}</subagent>`,
    `  <question>${escapeXml(input.question)}</question>`,
  ];
  if (input.context !== undefined && input.context.length > 0) {
    lines.push(`  <context>${escapeXml(input.context)}</context>`);
  }
  lines.push(
    "</workflow-escalation>",
    "",
    `Subagent ${input.actor} in workflow run ${input.runLabel} (${input.runId}) escalated a blocking question and is parked on that call waiting for your answer.`,
    `Next step: call ResolveWorkflowQuestion with question_id="${input.qid}" and your answer.`,
    "The run is still running: only the subagent that asked is parked — every other subagent and the script's control flow keep going. So do not drop what you are doing, but do not leave it unanswered either: nothing times out on its behalf.",
    "If this notification is ever lost, GetWorkflowRun lists the questions this run still owes an answer to.",
  );
  return truncateTaskNotification(lines.join("\n"));
}
