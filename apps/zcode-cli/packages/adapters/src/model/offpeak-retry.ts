/* Special adaptation layer judgment for off-peak (idle time task) queuing protocol.
The semantics only take effect for requests in which Model Config explicitly declares the off-peak-queue protocol - business code 3105/3102
In other bigmodel APIs, it may have other meanings, prohibiting writing into the global failure-provider-business-codes mapping table.

- 429 (including business code 3105) = queued response: single waiting min(Retry-After, 5min) clamp × infinite idempotent detection;
The caller freezes the attempt budget (otherwise it will be misjudged as API failure after 11 times by default); abort runs through sleep.
- 400/3102 = Ticket unavailable (active 3h expiration / ready invalid ticket): immediately defeated with stable mark,
The desktop identification mark is changed to "same task_id retrieval number → resume", which is not an ordinary failure.
- The abandonment of the first party is not implemented in the adaptation layer: when the first party connects to the gateway, the ready 5min TTL expiration is triggered naturally.
3102 → Continue running back to the team, waiting for the upper bound ≈ TTL + one clamp detection ≤ 10min, which meets the intent of the rule and has one less set of states. */
import type { ClassifiedModelFailure } from "./failure-classifier.js";
import { isProviderBusinessError } from "./model-execution.js";

/**
* ⚠ The constant with the same name in @zcode/shared/src/off-peak-types.ts on the desktop side has the same value across packages (wire contract):
* The providerId is injected with the per-turn runtimeModel, and the error mark is returned with the task final error text. Changes must be synchronized on both sides.
 */
export const OFF_PEAK_TICKET_EXPIRED_MARKER = "off-peak-ticket-expired";

/** Single queue waiting clamp: min(Retry-After, 5min); when there is no Retry-After, the detection is conservatively 60s. */
const OFF_PEAK_QUEUE_WAIT_CAP_MS = 5 * 60_000;
const OFF_PEAK_QUEUE_WAIT_DEFAULT_MS = 60_000;

type OffPeakFailureDecision = { kind: "queued"; delayMs: number } | { kind: "ticketExpired" };

/**
* Determine the unique failure semantics of off-peak; non-idle plan providers will always return null (zero impact).
* error needs to pass the original error after unwrapRetryError (ProviderBusinessError can read the business code).
 */
export function resolveOffPeakFailureDecision(params: {
  offPeak: boolean;
  failure: ClassifiedModelFailure;
  error: unknown;
}): OffPeakFailureDecision | null {
  if (!params.offPeak) return null;
  const businessCode = isProviderBusinessError(params.error)
    ? params.error.providerCode
    : undefined;
// The latest contract on the server side uses 3102; retain 3001 to be compatible with the old gateway response during rolling release.
  if (businessCode === "3102" || businessCode === "3001") {
    return { kind: "ticketExpired" };
  }
//Queue real signal = HTTP 429 (3105 with Retry-After); naked 429 without business code is also processed as queued.
  if (businessCode === "3105" || params.failure.statusCode === 429) {
    return {
      kind: "queued",
      delayMs: Math.min(
        params.failure.retryAfterMs ?? OFF_PEAK_QUEUE_WAIT_DEFAULT_MS,
        OFF_PEAK_QUEUE_WAIT_CAP_MS,
      ),
    };
  }
  return null;
}

export function offPeakTicketExpiredMessage(original: string): string {
  return `${OFF_PEAK_TICKET_EXPIRED_MARKER}: ${original}`;
}
