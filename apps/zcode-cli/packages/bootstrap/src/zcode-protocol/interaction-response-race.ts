import type {
  V4InteractionAnswer,
  V4InteractionRegistrationOptions,
} from "../zcode-protocol-v4/interaction-registry.js";
import type { ZCodeProtocolAgentServerContext } from "./server-types.js";

/**
* Let legacy reverse RPC (interaction/requestPermission, etc.) work with the v4 resolveInteraction command
* Racing: Whoever delivers the response first will take effect.
 *
* The implementation method is "use RPC as the main await, and cancel RPC when v4 hits":
* - Register interactionId in v4Interactions (= business requestId, with v4 projection
* PendingInteraction.interactionId (same origin); when v4 response arrives, record answer and abort internally
* controller, causing requestClient to reject with ProtocolRequestError(-32021).
* - If v4 answer is found in the catch, the mapping result will be returned (swallowing the cancellation error); otherwise, it will be thrown as is
* (including cancellation triggered by the caller's outerSignal), keeping the old path error semantics unchanged.
* - finally unregister unified + remove outerSignal monitoring, late v4 responses are treated as misses
* (resolveInteraction handler idempotent closure).
 */
export async function raceClientRequestWithV4Interaction<T>(
  context: ZCodeProtocolAgentServerContext,
  interactionId: string,
  outerSignal: AbortSignal | undefined,
  startRequest: (signal: AbortSignal) => Promise<T>,
  mapAnswer: (answer: V4InteractionAnswer) => T,
  registrationOptions?: V4InteractionRegistrationOptions,
): Promise<T> {
  const controller = new AbortController();
// Each attempt independently waits for failure notification; success is still released by the registry's V4 answer.
// Simply resetting the Boolean value will not wake up the awaited legacy response; reusing the one-time notification will allow retries to be released in advance.
  let fullAccessFailure: Promise<void> | undefined;
  let finishAnswer!: () => void;
  const answerReady = new Promise<void>((resolve) => {
    finishAnswer = resolve;
  });
  const forwardAbort = () => {
    controller.abort();
    finishAnswer();
  };
  if (outerSignal?.aborted) {
    controller.abort();
  } else {
    outerSignal?.addEventListener("abort", forwardAbort, { once: true });
  }

// Use containers instead of raw values ​​to distinguish "response is an undefined field" from "not yet answered".
  let v4Answer: { answer: V4InteractionAnswer } | undefined;
  const unregister = context.v4Interactions.register(
    interactionId,
    (answer) => {
      v4Answer = { answer };
      controller.abort();
      finishAnswer();
    },
    registrationOptions?.fullAccess
      ? {
          ...registrationOptions,
          fullAccess: async () => {
            let notifyFailure!: () => void;
            const failure = new Promise<void>((resolve) => {
              notifyFailure = resolve;
            });
            fullAccessFailure = failure;
            try {
              await registrationOptions.fullAccess!();
            } catch (error) {
              if (fullAccessFailure === failure) fullAccessFailure = undefined;
              notifyFailure();
              throw error;
            }
          },
        }
      : registrationOptions,
  );

  const waitForFullAccess = async () => {
    while (fullAccessFailure && !v4Answer && !outerSignal?.aborted) {
      await Promise.race([fullAccessFailure, answerReady]);
    }
  };

  try {
    const result = await startRequest(controller.signal);
    if (fullAccessFailure) {
      await waitForFullAccess();
      if (v4Answer) return mapAnswer(v4Answer.answer);
      outerSignal?.throwIfAborted();
    }
    return result;
  } catch (error) {
    if (fullAccessFailure) await waitForFullAccess();
    if (v4Answer) {
      return mapAnswer(v4Answer.answer);
    }
    throw error;
  } finally {
    unregister();
    outerSignal?.removeEventListener("abort", forwardAbort);
  }
}
