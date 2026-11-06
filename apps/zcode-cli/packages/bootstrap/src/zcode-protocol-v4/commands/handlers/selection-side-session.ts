import type {
  CommandEnvelope,
  CommandPayloadMap,
  CommandResult,
} from "@zcode/shared/zcode-protocol-v4";
import { commandAdmissionOf } from "../executor.js";
import { inputIntentMetadata } from "../input-intent.js";
import { startPromptTurn } from "../prompt-turn.js";
import { requireRecord } from "../record-access.js";
import type { V4CommandCoreHost } from "../types.js";

async function createSelectionSideSession(
  host: V4CommandCoreHost,
  envelope: CommandEnvelope,
): Promise<CommandResult> {
  const payload = envelope.payload as CommandPayloadMap["createSelectionSideSession"];
  const record = requireRecord(host, envelope.sessionId);
  if (!host.createSelectionSideSession) {
    throw new Error("v4 createSelectionSideSession requires host capability");
  }
  const result = await host.createSelectionSideSession(record.app.sessionId, {
    sourceCommandId: envelope.commandId,
    revisionAtDecision: envelope.baseRevision ?? 0,
    ...(payload.firstInput?.modelSelection
      ? { modelSelection: payload.firstInput.modelSelection }
      : {}),
  });
  const childSessionId = result.sessionId;
  const firstInput = payload.firstInput;
  if (!firstInput) {
    return { type: "createSelectionSideSession", sessionId: childSessionId };
  }

// The first input is started after the child has been registered in the host; the input only falls to the child, parent session
// CommandInbox/queue does not participate in this admission, so it will not change the running state of the parent turn.
  const childRecord = requireRecord(host, childSessionId);
  const admission = commandAdmissionOf(envelope);
// The gateway does not regard createSelectionSideSession as the input command of the parent session; the handler directly
// The same envelope points to the child, reusing the session_input ledger without polluting the parent queue.
  const durableAdmission =
    (await host.admitInputCommand?.(envelope, childSessionId, admission)) ?? null;
  try {
    const started = await startPromptTurn(host, childRecord, {
      content: firstInput.text,
      inputId: envelope.commandId,
      intent: inputIntentMetadata(envelope, {
        text: firstInput.text,
        requestedDelivery: "startNow",
      }),
    });
    const input = {
      delivery: started.admission.kind === "queued" ? ("queue" as const) : ("startNow" as const),
      inputId: envelope.commandId,
      ...(started.messageId ? { messageId: started.messageId } : {}),
    };
    return {
      type: "createSelectionSideSession",
      sessionId: childSessionId,
      ...(input ? { input } : {}),
    };
  } catch (error) {
    if (durableAdmission) {
      try {
        await host.cancelInputCommand?.(
          childSessionId,
          admission.queueItemId,
          "fault.command.inputRejected",
        );
      } catch (cancelError) {
        host.logger?.warn?.("v4 selection side first input cancellation failed", {
          cancelError: cancelError instanceof Error ? cancelError.message : String(cancelError),
          inputError: error instanceof Error ? error.message : String(error),
          queueItemId: admission.queueItemId,
          sessionId: childSessionId,
        });
      }
    }
    throw error;
  }
}

export const selectionSideSessionHandlers = {
  createSelectionSideSession,
};
