import { PERMISSION_FULL_ACCESS_OPTION_ID } from "@zcode/shared/zcode-protocol-v4";
// Permissions/Background Command Group: resolveInteraction/cancelBackgroundWork.
// - resolveInteraction: The forward command closes the reverse request (permission/AskUserQuestion), after
// host.interactions (V4InteractionRegistry) is delivered to the waiting deferred on the broker side.
// - cancelBackgroundWork: optional capability of direct drive core cancelBackgroundTask (workId ≡ taskId).
import type {
  CommandEnvelope,
  CommandPayloadMap,
  CommandResult,
  SavedWorkflowStartRejectionReason,
  WorkflowRunSettingsRejectionReason,
} from "@zcode/shared/zcode-protocol-v4";
import {
  BACKGROUND_WORK_CANCEL_REJECTED_FAULT_PREFIX,
  SAVED_WORKFLOW_START_REJECTED_FAULT_PREFIX,
  WORKFLOW_RUN_RESUME_REJECTED_FAULT_PREFIX,
  WORKFLOW_RUN_SETTINGS_REJECTED_FAULT_PREFIX,
} from "@zcode/shared/zcode-protocol-v4";
import { requireRecord } from "../record-access.js";
import type { V4CommandCoreHost } from "../types.js";

/**
* Capability not supported error: thrown when the session runtime does not implement optional capabilities (compare to old server-operations.ts
* ProtocolRequestError -32031 semantics of cancelBackgroundTask). JSON-RPC is no longer used on the v4 side
* For the error code, use the structured Error (fault namespace) with reasonCode instead, and the gateway will receive the ACK accordingly.
 */
export class V4CapabilityUnsupportedError extends Error {
  readonly reasonCode = "fault.command.capabilityUnsupported";

  constructor(capability: string, sessionId: string) {
    super(`capability not supported by this session runtime: ${capability} (session ${sessionId})`);
    this.name = "V4CapabilityUnsupportedError";
  }
}

/**
* resolveInteraction: Deliver the response to the waiting reverse request (interaction-broker's race deferred).
 *
* Semantic fidelity (exploration conclusion):
* - Missed (delivered === false, interaction has been answered/logged out/unknown id) and closed successfully according to idempotence, no error is thrown——
* Multi-terminal first come first served, late response is a harmless idempotent operation, throwing failed will mislead the client.
* - do not requireRecord: late responses may occur after the session has been closed/deleted, and must also be harmless;
* The registration table is globally addressed by interactionId and does not depend on the existence of record.
 */
async function resolveInteraction(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["resolveInteraction"];
  const delivered =
    payload.answer.optionId === PERMISSION_FULL_ACCESS_OPTION_ID
      ? ((await host.interactions?.resolveFullAccess(payload.interactionId, envelope.sessionId!)) ??
        false)
      : (host.interactions?.resolve(payload.interactionId, payload.answer) ?? false);
  if (!delivered) {
    host.logger?.info?.("v4 resolveInteraction no pending interaction (idempotent)", {
      event: "zcode_protocol.v4.interaction_already_resolved",
      interactionId: payload.interactionId,
      sessionId: envelope.sessionId,
    });
  }
  return undefined;
}

async function snoozeInteractionAutoResolution(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["snoozeInteractionAutoResolution"];
  const snoozed = (await host.interactions?.snoozeAutoResolution(payload.interactionId)) ?? false;
  if (!snoozed) {
    host.logger?.info?.("v4 snoozeInteractionAutoResolution no active countdown (idempotent)", {
      event: "zcode_protocol.v4.interaction_auto_resolution_already_snoozed",
      interactionId: payload.interactionId,
      sessionId: envelope.sessionId,
    });
  }
  return undefined;
}

class V4WorkspaceHookReviewRejectedError extends Error {
  constructor(readonly reasonCode: string) {
    super(`Workspace Hook review command rejected: ${reasonCode}`);
    this.name = "V4WorkspaceHookReviewRejectedError";
  }
}

async function respondWorkspaceHookReview(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["respondWorkspaceHookReview"];
  const record = requireWorkspaceHookReviewRecord(host, envelope, payload.sessionId);
  const result = await record.app.respondWorkspaceHookReview(payload);
  if (!result.accepted) throw new V4WorkspaceHookReviewRejectedError(result.reasonCode);
  return undefined;
}

async function toggleWorkspaceHookReviewItem(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["toggleWorkspaceHookReviewItem"];
  const record = requireWorkspaceHookReviewRecord(host, envelope, payload.sessionId);
  const result = await record.app.toggleWorkspaceHookReviewItem(payload);
  if (!result.accepted) throw new V4WorkspaceHookReviewRejectedError(result.reasonCode);
  return undefined;
}

async function revokeWorkspaceHookTrust(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["revokeWorkspaceHookTrust"];
  const record = requireWorkspaceHookReviewRecord(host, envelope, payload.sessionId);
  const result = await record.app.revokeWorkspaceHookTrust(payload);
  if (!result.accepted) throw new V4WorkspaceHookReviewRejectedError(result.reasonCode);
  return undefined;
}

/**
* Soft access control: open audit flow on demand.
 *
* Called when the user clicks "Go to Review". via controller.requestReview → openOrReuseFlow +
*superviseFlow. Idempotent reuse when there is already an active flow. It is a safe no-op when there are no pending items.
 */
async function requestWorkspaceHookReview(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["requestWorkspaceHookReview"];
  const record = requireWorkspaceHookReviewRecord(host, envelope, payload.sessionId);
  const result = await record.app.requestWorkspaceHookReview({
    workspaceIdentity: payload.workspaceIdentity,
    bundleDigest: payload.bundleDigest,
  });
  if (!result.accepted) throw new V4WorkspaceHookReviewRejectedError(result.reasonCode);
  return undefined;
}

function requireWorkspaceHookReviewRecord(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
  payloadSessionId: string,
) {
  if (envelope.sessionId !== payloadSessionId) {
    throw new V4WorkspaceHookReviewRejectedError("workspace_hooks_snapshot_mismatch");
  }
  return requireRecord(host, envelope.sessionId);
}

/**
* Business rejection of cancelBackgroundWork: core clearly returns "nothing was canceled" (reason is present).
 *
* When the old run is mistakenly left running by cold playback, you can click Cancel on the details page.
* The command goes directly to core but no such task is found (`background_task_not_found`); the old handler discards the entire structured result.
* Return to accepted, so "no response after clicking" in front of the user. The reason of core is the only authority, only prefix transfer is done here.
 */
class V4BackgroundWorkCancelRejectedError extends Error {
  readonly reasonCode: string;
  constructor(reason: string, workId: string) {
    super(`background work ${workId} was not cancelled: ${reason}`);
    this.name = "V4BackgroundWorkCancelRejectedError";
    this.reasonCode = `${BACKGROUND_WORK_CANCEL_REJECTED_FAULT_PREFIX}${reason.replace(/^background_task_/, "")}`;
  }
}

/**
* cancelBackgroundWork: workId ≡ old taskId is passed directly to core.
* - cancelBackgroundTask is an optional capability of ZCodeApp: does not exist → throw capability is not supported (see above).
* - core returns `reason` (does not exist / finalized / type not supported) → with `fault.command.backgroundWorkCancelRejected.<reason>`
* The reasonCode returns ACK; it is accepted if it is really canceled or the return value is absent (stub host).
* - No legacy broadcast required: BackgroundTask* (Started/Updated/Completed) lifecycle events
* Direct emit from core, v4 projection (product-projection backgroundWorks) self-closing——
* Compared with afterStateMutation("background_task_cancelled") of the old op, v4 does not have this obligation.
 */
async function cancelBackgroundWork(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["cancelBackgroundWork"];
  const record = requireRecord(host, envelope.sessionId);
  if (!record.app.cancelBackgroundTask) {
    throw new V4CapabilityUnsupportedError("cancelBackgroundTask", record.app.sessionId);
  }
// Note: The method must be called by app (cannot be deconstructed, the implementation may rely on this binding).
  const result = await record.app.cancelBackgroundTask(payload.workId);
  if (result?.reason !== undefined) {
    throw new V4BackgroundWorkCancelRejectedError(result.reason, payload.workId);
  }
  return undefined;
}

/**
* resumeWorkflowRun: workId ≡ runId directly transfers app capabilities.
* - Capability absent (journal is not available / port has no resume) → Capability not supported error (same semantics as cancel).
* - Business rejection (not_found/not_resumable/already_running/script_missing/
* script_mismatch / compile_failed) with reasonCode of `fault.command.workflowRunResumeRejected.<reason>`
* Return ACK - the gateway will upload the field error carrying reasonCode as it is, and the UI will be diverted according to the vocabulary; no error text will be used for judgment.
* Bounded diagnostics for compile_failed are folded into `ack.message` via `error.message` (same convention as startSavedWorkflow).
 */
class V4WorkflowRunResumeRejectedError extends Error {
  readonly reasonCode: string;
  constructor(reason: string, message?: string) {
    super(message ?? `workflow run resume rejected: ${reason}`);
    this.name = "V4WorkflowRunResumeRejectedError";
    this.reasonCode = `${WORKFLOW_RUN_RESUME_REJECTED_FAULT_PREFIX}${reason}`;
  }
}

async function resumeWorkflowRun(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["resumeWorkflowRun"];
  const record = requireRecord(host, envelope.sessionId);
  if (!record.app.resumeWorkflowRun) {
    throw new V4CapabilityUnsupportedError("resumeWorkflowRun", record.app.sessionId);
  }
// Note: The method must be called by app (cannot be deconstructed, the implementation may rely on this binding).
  const result = await record.app.resumeWorkflowRun({
    workId: payload.workId,
    ...(payload.name === undefined ? {} : { name: payload.name }),
  });
  if (!result.ok) throw new V4WorkflowRunResumeRejectedError(result.reason, result.message);
  return undefined;
}

/**
* startSavedWorkflow: The hub directly starts a saved workflow.
* - Capability absent (no dwf port/stub host) → Capability not supported error (same semantics as resume family), GUI displayed as is
* "The current agent does not support direct startup" and recycle empty sessions.
* - Business Denied (invalid_name/not_found/invalid_args/compile_failed/session_busy/start_failed)
* Return ACK with reasonCode of `fault.command.savedWorkflowStartRejected.<reason>`; `message` carries
* Human-readable diagnostics (bounded truncation after compilation diagnostics are merged) for in-line display in the actual parameter window. Gateway pair realm error with reasonCode
* Upload the code as it is, and collect `error.message` into `ack.message`, and the UI will be divided according to the word list - no error text is used for process judgment.
* - Successfully returns ACK.result (linked to tool card → details page) with `{ type: "startSavedWorkflow", runId, toolCallId }`.
* Note: Non-input commands (not queued, without baseRevision) are similar to resume/cancel and are registered in the interaction-background group.
 */
class V4SavedWorkflowStartRejectedError extends Error {
  readonly reasonCode: string;
  constructor(reason: SavedWorkflowStartRejectionReason, message?: string) {
// The message goes directly to ack.message (gateway convention: error.message receives ACK), and it is readable in case of absence.
    super(message ?? `saved workflow start rejected: ${reason}`);
    this.name = "V4SavedWorkflowStartRejectedError";
    this.reasonCode = `${SAVED_WORKFLOW_START_REJECTED_FAULT_PREFIX}${reason}`;
  }
}

async function startSavedWorkflow(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["startSavedWorkflow"];
  const record = requireRecord(host, envelope.sessionId);
  if (!record.app.startSavedWorkflow) {
    throw new V4CapabilityUnsupportedError("startSavedWorkflow", record.app.sessionId);
  }
// Note: The method must be called by app (cannot be deconstructed, the implementation may rely on this binding).
  const result = await record.app.startSavedWorkflow({
    name: payload.name,
    ...(payload.scope === undefined ? {} : { scope: payload.scope }),
    ...(payload.args === undefined ? {} : { args: payload.args }),
  });
  if (!result.ok) throw new V4SavedWorkflowStartRejectedError(result.reason, result.message);
  return { type: "startSavedWorkflow", runId: result.runId, toolCallId: result.toolCallId };
}

/**
* amendWorkflowRunSettings: "Configuration" of run card/details page. workId ≡ runId; the three-state settings of the two settings are passed to the runtime as they are.
* - Capability is absent (no dwf port, or the port does not have amend / getScript) → Capability not supported error, the pop-up layer displays "Not Supported".
* - Business rejection returns ACK with `fault.command.workflowRunSettingsRejected.<reason>`, `message` carries diagnosis
* (Compilation diagnosis/model analysis diagnosis/start failure reason); the old run is still running when rejected.
* - Successfully returns ACK.result with `{ type, runId, toolCallId, supersededRunId? }` - the two connection keys of the new run,
* The details page changes the tab to the new run according to it.
 */
class V4WorkflowRunSettingsRejectedError extends Error {
  readonly reasonCode: string;
  constructor(reason: WorkflowRunSettingsRejectionReason, message?: string) {
    super(message ?? `workflow run settings rejected: ${reason}`);
    this.name = "V4WorkflowRunSettingsRejectedError";
    this.reasonCode = `${WORKFLOW_RUN_SETTINGS_REJECTED_FAULT_PREFIX}${reason}`;
  }
}

async function amendWorkflowRunSettings(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["amendWorkflowRunSettings"];
  const record = requireRecord(host, envelope.sessionId);
  if (!record.app.amendWorkflowRunSettings) {
    throw new V4CapabilityUnsupportedError("amendWorkflowRunSettings", record.app.sessionId);
  }
// Note: The method must be called by app (cannot be deconstructed, the implementation may rely on this binding).
  const result = await record.app.amendWorkflowRunSettings({
    runId: payload.workId,
    ...(payload.subagentModel === undefined ? {} : { subagentModel: payload.subagentModel }),
    ...(payload.maxConcurrency === undefined ? {} : { maxConcurrency: payload.maxConcurrency }),
  });
  if (!result.ok) throw new V4WorkflowRunSettingsRejectedError(result.reason, result.message);
  return {
    type: "amendWorkflowRunSettings",
    runId: result.runId,
    toolCallId: result.toolCallId,
    ...(result.supersededRunId === undefined ? {} : { supersededRunId: result.supersededRunId }),
  };
}

export const interactionBackgroundHandlers = {
  resolveInteraction,
  respondWorkspaceHookReview,
  toggleWorkspaceHookReviewItem,
  revokeWorkspaceHookTrust,
  requestWorkspaceHookReview,
  snoozeInteractionAutoResolution,
  cancelBackgroundWork,
  resumeWorkflowRun,
  startSavedWorkflow,
  amendWorkflowRunSettings,
};
