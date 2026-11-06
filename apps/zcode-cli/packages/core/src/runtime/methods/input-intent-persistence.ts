import type { TurnInputIntentMetadata } from "../deps.js";

/**
* Assemble the runtime's protocol-independent parameters into complete input facts shared by transcript/ledger.
 *
* If only text and a set of metadata are saved separately, the recovery end must infer delivery,
* Steer and dispatch can easily cause live projection and cold snapshot to appear in different states. here at
* The admission/drain boundary is solidified once, and subsequent consumers can only read it and cannot re-guess it.
 */
export function buildPersistedConversationInputIntent(
  text: string,
  intent: TurnInputIntentMetadata | undefined,
  dispatchState: "queued" | "drained",
): Record<string, unknown> | undefined {
  if (!intent) return undefined;

  const steer = intent.fallbackReasonCode
    ? { state: "fellBack", reasonCode: intent.fallbackReasonCode }
    : intent.admittedDelivery === "guide"
      ? { state: dispatchState === "drained" ? "guided" : "steering" }
      : { state: "notRequested" };

  return {
    sourceCommandId: intent.sourceCommandId,
    queueItemId: intent.queueItemId,
    clientId: intent.clientId,
    kind: intent.kind,
// The message text of goal can be `/goal...` to display the copy; admission has runtime
// Parsed canonical objective, persistence must use it first to ensure live/cold equivalence.
    text: intent.text ?? text,
    attachments: intent.attachmentRefs ?? [],
    ...(intent.modelSelection ? { modelSelection: intent.modelSelection } : {}),
    ...(intent.mode ? { mode: intent.mode } : {}),
    ...(intent.planEnabled !== undefined ? { planEnabled: intent.planEnabled } : {}),
    ...(intent.sharedContextRefs ? { sharedContextRefs: intent.sharedContextRefs } : {}),
    delivery: {
      requested: intent.requestedDelivery,
      admitted: intent.admittedDelivery,
      ...(intent.fallbackReasonCode ? { fallbackReasonCode: intent.fallbackReasonCode } : {}),
    },
    order: {
      admissionSeq: intent.admissionSeq,
      ...(intent.queuePosition !== undefined ? { queuePosition: intent.queuePosition } : {}),
    },
    steer,
    dispatch: { state: dispatchState },
    admittedAt: intent.admittedAt,
    ...(intent.provenance ? { provenance: intent.provenance } : {}),
  };
}
