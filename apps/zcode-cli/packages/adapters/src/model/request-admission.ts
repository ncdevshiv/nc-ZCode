import type {
  ModelRequestAdmission,
  ModelRequestAdmissionTicket,
  ModelRequestTarget,
} from "@zcode/contracts";

/**
* Admission on one attempt.
 *
* The runner ** `admitAttempt` before issuing each attempt, and only sends the request after getting the ticket; the attempt ends (success, failure, throw, consumer
* Abandon the stream early) that is, `release` - no tickets are held during the back-off sleep, so the process-level cap constrains what the provider actually sees on the fly.
*Number of requests. The ticket is also the attempt's status event sink: `publishModelStatus` also delivers the attempt's events to it (see
* `statusPublishOptions`), the manager determines the result accordingly; `release` is just a cover-up, idempotent.
 *
* Returns an empty implementation when requesting without `modelRequestAdmission`: the call site does not have to fork and the runner behavior remains unchanged verbatim.
 */
export interface AttemptAdmission {
/** Admission ticket; absent when the request does not have an admission port (publish will not redirect at this time). */
  readonly ticket?: ModelRequestAdmissionTicket;
/** Return the slot; idempotent (both finally and "before retreating to sleep" will be adjusted). */
  release(): void;
}

const NO_ADMISSION: AttemptAdmission = { release() {} };

/**
* Waiting for admission. First try the synchronization fast path `tryAcquire`; queue `acquire` only if it fails, and call back `onQueued` / `onAdmitted` at both ends.
* (runner sends `model_request_queued` / `model_request_admitted` accordingly). Ports without fast paths cannot be distinguished clearly
* "Queue" and "Released immediately", so there is no callback. Reject when `signal` is abort (with `signal.reason`, same as `sleep`
* abort errors have the same shape and are classified by canceled by the caller).
 */
export async function admitAttempt(input: {
  admission?: ModelRequestAdmission;
  model: ModelRequestTarget;
  signal?: AbortSignal;
  onQueued?: () => Promise<void>;
  onAdmitted?: (queuedMs: number) => Promise<void>;
}): Promise<AttemptAdmission> {
  if (input.admission === undefined) return NO_ADMISSION;
  const hasFastPath = typeof input.admission.tryAcquire === "function";
  let ticket = hasFastPath ? input.admission.tryAcquire!({ model: input.model }) : undefined;
  if (ticket === undefined) {
    const queuedAt = Date.now();
    if (hasFastPath) await input.onQueued?.();
    ticket = await input.admission.acquire({
      model: input.model,
      ...(input.signal === undefined ? {} : { signal: input.signal }),
    });
    if (hasFastPath) await input.onAdmitted?.(Date.now() - queuedAt);
  }
  let released = false;
  return {
    ticket,
    release() {
      if (released) return;
      released = true;
      ticket.release();
    },
  };
}
