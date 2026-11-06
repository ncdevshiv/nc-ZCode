// ============================================================
// Attribution facts of dynamic workflow sub-agent (sub-agent token burying point)
// ============================================================
// DynamicWorkflowRunProgress event of the parent session → `workflow.lifecycle` bury the facts. Only on two engine events
// Derived: actor-created (registered subagent ↔ subsession ↔ initiated round) and run-settled (the final state of all subagents in this run).
// If there is no `launchInputId` (the run initiated before the upgrade), it will not be launched: the step without anchor point has nowhere to hang, it is better to lack it than to create it.

import type { DynamicWorkflowRunProgressPayload } from "@zcode/contracts";
import {
  conversationTelemetryFactSchema,
  type ConversationTelemetryFact,
} from "@zcode/shared/zcode-protocol-v4";

const ACTOR_CREATED_EVENT_TYPE = "actor-created";
const RUN_SETTLED_EVENT_TYPE = "run-settled";
const RUN_STOPPED_ERROR_MESSAGE = "Workflow run stopped";

/** The three final words and stop reasons of run are the same as the engine RunStatus / RunStopReason. */
type WorkflowRunSettledStatus = "completed" | "errored" | "stopped";
type WorkflowRunStopReason = "user" | "model" | "provider" | "interrupted" | "superseded";

/** The derived field on the progress event envelope (linked to toProgressPayload, see DynamicWorkflowRunProgressPayload for the contract). */
interface WorkflowProgressDerivedFields {
  actorSessionId?: unknown;
  launchInputId?: unknown;
}

export function workflowLifecycleFactFromProgress(
  base: Record<string, unknown>,
  progress: DynamicWorkflowRunProgressPayload,
): ConversationTelemetryFact | null {
  const derived = progress as DynamicWorkflowRunProgressPayload & WorkflowProgressDerivedFields;
  const launchInputId = optionalString(derived.launchInputId);
  if (launchInputId === undefined) return null;
  const toolCallId = optionalString(progress.toolCallId);

  if (progress.eventType === ACTOR_CREATED_EVENT_TYPE) {
    const childSessionId = optionalString(derived.actorSessionId);
    const agentId = actorRefString(progress.payload.actor);
    if (childSessionId === undefined || agentId === undefined) return null;
    return conversationTelemetryFactSchema.parse({
      ...base,
      kind: "workflow.lifecycle",
      phase: "actor-spawned",
// The anchor point is sourceCommandId: the sub-agent step is hung under the message of the round that initiated the run.
      sourceCommandId: launchInputId,
      runId: progress.runId,
      ...(toolCallId === undefined ? {} : { toolCallId }),
      agentId,
      childSessionId,
    });
  }

  if (progress.eventType === RUN_SETTLED_EVENT_TYPE) {
    const status = settledStatus(progress.payload.status);
    if (status === undefined) return null;
    const stopReason =
      status === "stopped" ? settledStopReason(progress.payload.stopReason) : undefined;
// The original text of the error is according to the engine event: errored always has error; stopped only has error for provider / interrupted.
// There is no original text for user/model stop. Use fixed copy to add the reason so that the board can still tell who stopped.
    const engineMessage = optionalString(errorRecord(progress.payload.error)?.message);
    const errorMessage =
      status === "completed"
        ? undefined
        : (engineMessage ?? (status === "stopped" ? stoppedMessage(stopReason) : undefined));
    return conversationTelemetryFactSchema.parse({
      ...base,
      kind: "workflow.lifecycle",
      phase: "run-settled",
      sourceCommandId: launchInputId,
      runId: progress.runId,
      ...(toolCallId === undefined ? {} : { toolCallId }),
      status,
      ...(stopReason === undefined ? {} : { stopReason }),
      ...(errorMessage === undefined ? {} : { errorMessage }),
    });
  }

  return null;
}

/** `siteId@ordinal`: the same string as the dwf engine refToString (the sub-agent tag of the run side panel is also it). */
function actorRefString(value: unknown): string | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const ref = value as { siteId?: unknown; ordinal?: unknown };
  const siteId = optionalString(ref.siteId);
  if (siteId === undefined || typeof ref.ordinal !== "number" || !Number.isFinite(ref.ordinal)) {
    return undefined;
  }
  return `${siteId}@${ref.ordinal}`;
}

function settledStatus(value: unknown): WorkflowRunSettledStatus | undefined {
  return value === "completed" || value === "errored" || value === "stopped" ? value : undefined;
}

function settledStopReason(value: unknown): WorkflowRunStopReason | undefined {
  return value === "user" ||
    value === "model" ||
    value === "provider" ||
    value === "interrupted" ||
    value === "superseded"
    ? value
    : undefined;
}

function stoppedMessage(reason: WorkflowRunStopReason | undefined): string {
  return reason === undefined
    ? RUN_STOPPED_ERROR_MESSAGE
    : `${RUN_STOPPED_ERROR_MESSAGE} (${reason})`;
}

function errorRecord(value: unknown): { message?: unknown } | undefined {
  return typeof value === "object" && value !== null ? (value as { message?: unknown }) : undefined;
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}
