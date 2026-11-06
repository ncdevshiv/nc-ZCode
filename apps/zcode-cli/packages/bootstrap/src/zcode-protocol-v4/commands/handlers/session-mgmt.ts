// Session management command group: createSession / renameSession / deleteSession.
// One file per command group: handler pure function (host, envelope) → CommandResult|undefined,
// The decision-making logic directly drives the core, and the environment capabilities use the host hook (see the transition annotation of ../types.ts).
import type {
  CommandEnvelope,
  CommandPayloadMap,
  CommandResult,
} from "@zcode/shared/zcode-protocol-v4";
import { mapAttachmentRefsToTurnAttachments } from "../attachment-refs.js";
import { inputIntentMetadata } from "../input-intent.js";
import { commandAdmissionOf } from "../executor.js";
import { startPromptTurn } from "../prompt-turn.js";
import { requireRecord } from "../record-access.js";
import type { V4CommandCoreHost } from "../types.js";
import { applyRequestedSessionConfig } from "./model-config.js";
import {
  hasPromptInput,
  V4InputAdmissionRejectedError,
  resolveSubmittedExecutionState,
} from "./session-flow.js";

/**
* createSession: Nativeization of the last item in the fallback surface.
* Semantic decision-making (held by the native layer):
* - draft semantics: new sessions are always deferred (do not enter sqlite), and are promoted by prompt-turn when the first one is sent
* immediate——record creation hook is fixed and passed deferred, and the promotion logic is not in the hook.
* - firstInput optional: if available, submit via native prompt turn (same writing path as sendText -
* The three semantics of draft promotion/submit return/ready boundary are obtained for free), no longer through the old sendPrompt op.
* - workspaceId: local workspace = workspacePath (local fallback of Workspace Identity constraint);
* Remote identity (remote:ssh/wsl/docker:...) is passed by host.createSessionRecord
* @zcode/shared parseRemoteWorkspaceIdentity unified parsing (cross-workspace split-screen pane).
* Execution surface (transition hook): record creation/event wiring/catalog synchronization/failure cleanup and entanglement with the old host,
* Go to host.createSessionRecord (see ../types.ts).
 */
async function createSession(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["createSession"];
  if (!host.createSessionRecord) {
    throw new Error("v4 createSession requires host.createSessionRecord capability");
  }
// A completely empty firstInput must be rejected before creating a record to avoid failed requests leaving an invalid deferred session.
  if (
    payload.firstInput &&
    !hasPromptInput(payload.firstInput.text, payload.firstInput.attachments)
  ) {
    throw new V4InputAdmissionRejectedError("proto.invalidPayload", "input must not be empty");
  }
  const { sessionId } = await host.createSessionRecord({
    workspaceId: payload.workspaceId,
    mcpServers: payload.mcpServers,
    offPeakToolEnabled: payload.offPeakToolEnabled,
    dynamicWorkflowEnabled: payload.dynamicWorkflowEnabled,
  });
// createSession.config consumption - advance selection of draft UI (model/thinking depth/
// mode) applies and reissues events before the first turn, the first turn uses the selected configuration. Must precede firstInput.
// Application failure does not involve session creation (record has been created, failed ACK will only leak the session): downgrade warn,
// The session remains runtime default.
  if (payload.config) {
    const record = requireRecord(host, sessionId);
    try {
      await applyRequestedSessionConfig(host, record, payload.config);
    } catch (error) {
      host.logger?.warn?.("v4 createSession config apply failed; session keeps runtime defaults", {
        error: error instanceof Error ? error.message : String(error),
        sessionId,
      });
    }
  }
  let firstInput:
    | {
        delivery: "startNow" | "queue" | "guide";
        inputId: string;
        messageId?: string;
      }
    | undefined;
  if (payload.firstInput) {
// Attachment command surface: firstInput.attachments (AttachmentRef → TurnAttachment) is sent with the first item.
    const record = requireRecord(host, sessionId);
    const admission = commandAdmissionOf(envelope);
    const durableAdmission =
      (await host.admitInputCommand?.(envelope, sessionId, admission)) ?? null;
    try {
      const attachments = await mapAttachmentRefsToTurnAttachments(
        record.app,
        payload.firstInput.attachments,
      );
      const intent = inputIntentMetadata(envelope, {
        text: payload.firstInput.text,
        requestedDelivery: "startNow",
        attachmentRefs: payload.firstInput.attachments,
        ...resolveSubmittedExecutionState(record, payload.firstInput),
      });
      const started = await startPromptTurn(host, record, {
        content: payload.firstInput.text,
        inputId: envelope.commandId,
        intent,
        ...(attachments ? { attachments } : {}),
      });
      firstInput = {
        delivery: started.admission.kind === "queued" ? "queue" : "startNow",
        inputId: envelope.commandId,
        ...(started.messageId ? { messageId: started.messageId } : {}),
      };
    } catch (error) {
      if (durableAdmission) {
        try {
          await host.cancelInputCommand?.(
            sessionId,
            admission.queueItemId,
            "fault.command.inputRejected",
          );
        } catch (cancelError) {
// Failure to cancel the ledger cannot cover the real initial failure; otherwise the client will get the wrong failure reason,
// Admission can still be closed by discarded when restarting the query, and it will not be misjudged as successful.
          host.logger?.warn?.("v4 createSession first input cancellation failed", {
            cancelError: cancelError instanceof Error ? cancelError.message : String(cancelError),
            inputError: error instanceof Error ? error.message : String(error),
            queueItemId: admission.queueItemId,
            sessionId,
          });
        }
      }
      throw error;
    }
  }
  return { type: "createSession", sessionId, ...(firstInput ? { input: firstInput } : {}) };
}

/**
* renameSession: Explicit rename by user → core runtime.setCustomSessionTitle.
* - Stickiness of titleSource=custom (after which automatic title generation is skipped by custom_title short-circuit) is guaranteed by core
* (core/src/runtime/methods/session-title.ts), handler is not implemented repeatedly.
* - traceContext transparently transmits the session root traceContext (record narrow view field): renames the session root
* Task chain, no new trace (observability discipline).
* - No legacy broadcast: core sends SessionTitleUpdated event and closes via gateway projection.
* v4 consumers thus perceive title changes.
 */
async function renameSession(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["renameSession"];
  const record = requireRecord(host, envelope.sessionId);
// Note: Methods must be called by runtime (cannot be deconstructed, implementation depends on this binding, see methods/index.ts mounting method).
  await record.app.runtime.setCustomSessionTitle({
    title: payload.title,
    traceContext: record.traceContext,
  });
  return undefined;
}

/**
* deleteSession: semantics = closeSession (close + clean up runtime resources), non-true record deletion——
* The message library has no deletion API, which is consistent with the old protocol path (the "delete" of the old protocol is also just close, and the history is still in the library.
* just no longer appears in the active registry).
* The session registry still belongs to the host, and the host.closeSession transition hook is used to actually close it (destination: v4 self-sustained session registry, see ../types.ts).
 */
async function deleteSession(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const record = requireRecord(host, envelope.sessionId);
  if (!host.closeSession) {
// Shut down and cannot be downgraded silently: The missing hook indicates that the binder wiring is incomplete and fails directly (ACK failed).
    throw new Error("v4 deleteSession requires host.closeSession capability");
  }
  await host.closeSession(record.app.sessionId);
  return undefined;
}

async function discardSharedContext(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult | undefined> {
  const payload = envelope.payload as CommandPayloadMap["discardSharedContext"];
  const sessionId = envelope.sessionId;
  if (!sessionId || !host.discardSharedContext) {
    throw new Error("v4 discardSharedContext requires a session-scoped storage capability");
  }
  const updated = await host.discardSharedContext(sessionId, payload.contextId);
  if (!updated)
    throw new V4InputAdmissionRejectedError(
      "fault.command.inputRejected",
      "shared context is not pending",
    );
  return undefined;
}

export const sessionMgmtHandlers = {
  createSession,
  renameSession,
  deleteSession,
  discardSharedContext,
};
