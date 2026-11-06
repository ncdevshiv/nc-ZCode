// ============================================================
// Workflow concurrency manager (process level, bucketed by provider key, **per model request** admission)
// ============================================================
// All runs plus the main agent in a CLI process
// Share a provider quota, so the manager is **process level**: one bucket for each `${providerId}/${modelId}`,
// A pure AIMD state machine in the bucket (`ConcurrencyController`, @zcode/dynamic-workflow) + one that rotates according to run
//Admission queue.
//
// The gate granularity is **each attempt of the model request**: runner `acquire` before each attempt, `release` after the attempt,
// No slot is held during backoff sleep. The ticket is the status event sink of the attempt: the runner puts the
// The `ModelNetworkStatus` event is delivered to the ticket at the same time, and the manager reads the results from the ticket. This plan
// **Do not** use adapter-level `addStatusSink` - the same event cannot be fed through both the ticket and the adapter sink once.
//
// This is the **only** place in the manager where clocks and timers are touched: the controller only accepts `now`; when the cooldown expires, the waiter must be awakened.
// So a setTimeout (injectable, unref) is needed.

import type {
  ModelNetworkStatusEvent,
  ModelRequestAdmission,
  ModelRequestAdmissionTicket,
  ModelRequestTarget,
} from "@zcode/contracts";
import {
  ConcurrencyController,
  type ConcurrencyChange,
  type ConcurrencyControllerSnapshot,
  type ConcurrencyThrottleReason,
} from "@zcode/dynamic-workflow";
import { resolveWorkflowConcurrencyCeiling } from "./workflow-concurrency-ceiling.js";

/** provider key: The most specific quota key. */
export function workflowConcurrencyKey(model: ModelRequestTarget): string {
  return `${String(model.providerId)}/${String(model.modelId)}`;
}

/**
* The narrow port seen by the driver (not the entire manager).
 *
* - `tryAdmit`: synchronous fast path - the gate is open** and no one is queuing** before giving the ticket; otherwise undefined, the caller will go again
* `admit` and report "waiting for slot". It is fair not to take the fast path when there are waiters: otherwise a request-intensive run would
* The fast path crosses the queue of other runs.
* - `admit`: queue (rotate between runs) until `inFlight < cap` and not cool down in Retry-After; `signal` is abort i.e.
* Dequeue and reject (reject reason is `signal.reason`).
* - `subscribe`: Cap changes on any key touched by this run. Fanout only occurs if there are in-flight or queued requests on the key at the moment.
 *   run。
 */
export interface WorkflowConcurrencyPort {
  tryAdmit(runId: string, key: string): ModelRequestAdmissionTicket | undefined;
  admit(runId: string, key: string, signal: AbortSignal): Promise<ModelRequestAdmissionTicket>;
  subscribe(runId: string, listener: (change: ConcurrencyChange) => void): () => void;
}

interface WorkflowConcurrencyGovernor extends WorkflowConcurrencyPort {
  /**
* Admission used by the main agent: acquire immediately - no queuing, no cooling - but **counted into inFlight**
* And feed the signal (its request provider is also visible). The master agent's turn is never blocked by workflow traffic.
   */
  observer(): ModelRequestAdmission;
/** The controller is a read-only snapshot; it is undefined when there is no bucket with this key. */
  snapshot(key: string): ConcurrencyControllerSnapshot | undefined;
}

interface WorkflowConcurrencyGovernorOptions {
/** Ceiling: the initial value and upper bound when the bucket is created; counted once when the process starts. */
  ceiling: number;
/** Clock (injectable). */
  now?: () => number;
/** Timer (can be injected): Wake up the waiter when the cooldown expires. Returns the cancellation function. */
  schedule?: (callback: () => void, delayMs: number) => () => void;
}

/** Current limiting class retry reason → controller’s throttled signal. */
const THROTTLE_REASONS: ReadonlySet<string> = new Set<ConcurrencyThrottleReason>([
  "rate_limited",
  "provider_overloaded",
  "offpeak_queued",
]);
/** Not a provider failed retry Reason: neither cap reduction nor streak - only attempt to terminate. */
const NON_FAILURE_RETRY_REASONS: ReadonlySet<string> = new Set([
  "reasoning_signature_repair",
  "auth_refresh",
]);

interface Waiter {
  runId: string;
  resolve: (ticket: ModelRequestAdmissionTicket) => void;
  reject: (reason: unknown) => void;
  signal: AbortSignal;
  onAbort: () => void;
}

interface Bucket {
  readonly key: string;
  readonly controller: ConcurrencyController;
/** The number of requests for each run that are in flight (admitted but not settled) (basis for fanout: only runs with in-flight or queued requests will receive cap changes). */
  readonly inFlightByRun: Map<string, number>;
/** Waiters are divided into queues according to runs (rotation between runs, large fan-out cannot starve to death of subsequent small runs). */
  readonly queues: Map<string, Waiter[]>;
/** Rotation cursor: The run released last time, start looking after it next time. */
  lastGrantedRun?: string;
  cancelCooldownWake?: () => void;
}

/** The run identity of observer (master agent): no queuing, no subscription, only appears in fan-out filtering as "not any run". */
const OBSERVER_RUN_ID = "\0observer";

const defaultSchedule = (callback: () => void, delayMs: number): (() => void) => {
  const timer = setTimeout(callback, delayMs);
// Don't let a timer waiting to cool down pin the process: it should not have voting rights when the run ends and the process wants to exit.
  if (typeof timer === "object" && timer !== null && "unref" in timer) timer.unref();
  return () => clearTimeout(timer);
};

function createWorkflowConcurrencyGovernor(
  options: WorkflowConcurrencyGovernorOptions,
): WorkflowConcurrencyGovernor {
  const now = options.now ?? Date.now;
  const schedule = options.schedule ?? defaultSchedule;
  const buckets = new Map<string, Bucket>();
/** Run-level subscription (not by key): Filter by the engaged set of the bucket during fanout. */
  const listeners = new Map<string, Set<(change: ConcurrencyChange) => void>>();

  const bucketFor = (key: string): Bucket => {
    let bucket = buckets.get(key);
    if (bucket === undefined) {
// Lazy bucket creation, initial value = ceiling; after that, it will not be reset when the run comes and goes (only if it is idle for 5 minutes).
      bucket = {
        key,
        controller: new ConcurrencyController(key, options.ceiling),
        inFlightByRun: new Map(),
        queues: new Map(),
      };
      buckets.set(key, bucket);
    }
    return bucket;
  };

  const waiterCount = (bucket: Bucket): number => {
    let total = 0;
    for (const queue of bucket.queues.values()) total += queue.length;
    return total;
  };

/** Fanout: only given to runs that have in-flight or queued requests on the key at the moment. */
  const fanOut = (bucket: Bucket, changes: ConcurrencyChange[]): void => {
    if (changes.length === 0) return;
    for (const [runId, set] of listeners) {
      const engaged =
        (bucket.inFlightByRun.get(runId) ?? 0) > 0 || (bucket.queues.get(runId)?.length ?? 0) > 0;
      if (!engaged) continue;
      for (const change of changes) {
        for (const listener of set) listener(change);
      }
    }
  };

  const bumpInFlight = (bucket: Bucket, runId: string, delta: number): void => {
    const next = (bucket.inFlightByRun.get(runId) ?? 0) + delta;
    if (next <= 0) bucket.inFlightByRun.delete(runId);
    else bucket.inFlightByRun.set(runId, next);
  };

  /**
* Ticket: event mapping remains consistent for an admission attempt. Settled after any final mapping (idempotent); `release()`
* If the final event is not seen, it will be processed as `ended`. Post-settlement publish/release is always inert - runner is on very few paths
* (Backup events after the attempt has ended) may still be delivered.
   */
  const mintTicket = (
    bucket: Bucket,
    runId: string,
    epoch: number,
  ): ModelRequestAdmissionTicket => {
    let settled = false;
    const settle = (signal: (at: number) => ConcurrencyChange[]): void => {
      if (settled) return;
      settled = true;
      const changes = signal(now());
// First fan out and then reduce in flight: the request that produced this change is for this run, it is still considered engaged at this moment - otherwise the only one for a run
// The halved request for collision while flying will be missed to itself because it is "no longer flying".
      fanOut(bucket, changes);
      bumpInFlight(bucket, runId, -1);
// Settlement releases a quota (or changes the cap), and those in line may be released.
      drain(bucket);
    };
    return {
      publish(event: ModelNetworkStatusEvent) {
        if (settled) return;
        switch (event.type) {
          case "model_request_started":
// InFlight is already accounted for on admission, no new information here.
            return;
          case "model_request_completed":
            settle((at) => bucket.controller.succeeded(at, epoch));
            return;
          case "model_retry_scheduled": {
            const reason: string = event.reason;
            if (NON_FAILURE_RETRY_REASONS.has(reason)) {
              settle((at) => bucket.controller.ended(at));
              return;
            }
            if (THROTTLE_REASONS.has(reason)) {
              settle((at) =>
                bucket.controller.throttled(
                  at,
                  epoch,
                  reason as ConcurrencyThrottleReason,
                  event.retryAfterMs,
                ),
              );
              return;
            }
            settle((at) => bucket.controller.failedTransient(at));
            return;
          }
          case "model_request_failed":
// The failed retryable:true is followed by a retry_scheduled - that one is the signal.
            if (event.retryable) return;
// Non-retryable current limit:
// Main dialogue / Tool side collision 3008 This type of 429 that is terminated by the classifier is still a concurrent signal in the literal sense——
// Only when "chain ends" will the cap never be lowered due to it. The quota code also goes like this: if you reduce it one and a half times more, the run will stop immediately, which is harmless.
            if (event.reason === "rate_limited") {
              settle((at) =>
                bucket.controller.throttled(at, epoch, "rate_limited", event.retryAfterMs),
              );
              return;
            }
            settle((at) => bucket.controller.ended(at));
            return;
          default:
            return;
        }
      },
      release() {
        settle((at) => bucket.controller.ended(at));
      },
    };
  };

/** Release a request: `observe` comes first (admission is also a "signal"), and then `admitted` takes the epoch. */
  const grant = (bucket: Bucket, runId: string): ModelRequestAdmissionTicket => {
    fanOut(bucket, bucket.controller.observe(now()));
    const epoch = bucket.controller.admitted(now());
    bumpInFlight(bucket, runId, 1);
    bucket.lastGrantedRun = runId;
    return mintTicket(bucket, runId, epoch);
  };

/** Pick the next run with a waiter in rotation. */
  const nextRunWithWaiters = (bucket: Bucket): string | undefined => {
    const runs = [...bucket.queues.keys()].filter(
      (runId) => (bucket.queues.get(runId)?.length ?? 0) > 0,
    );
    if (runs.length === 0) return undefined;
    const last = bucket.lastGrantedRun;
    const lastIndex = last === undefined ? -1 : runs.indexOf(last);
    return runs[(lastIndex + 1) % runs.length];
  };

  const armCooldownWake = (bucket: Bucket): void => {
    bucket.cancelCooldownWake?.();
    bucket.cancelCooldownWake = undefined;
    if (waiterCount(bucket) === 0) return;
    const until = bucket.controller.snapshot().cooldownUntil;
    if (until === undefined) return;
    const delay = Math.max(0, until - now());
    bucket.cancelCooldownWake = schedule(() => {
      bucket.cancelCooldownWake = undefined;
      drain(bucket);
    }, delay);
  };

/** Release as many waiters as possible (gate: inFlight < cap and not cooling), feed waiters, and set cooling alarm if necessary. */
  const drain = (bucket: Bucket): void => {
    fanOut(bucket, bucket.controller.observe(now()));
    for (;;) {
      fanOut(bucket, bucket.controller.waiters(now(), waiterCount(bucket)));
      if (!bucket.controller.canAdmit(now())) break;
      const runId = nextRunWithWaiters(bucket);
      if (runId === undefined) break;
      const queue = bucket.queues.get(runId)!;
      const waiter = queue.shift()!;
      if (queue.length === 0) bucket.queues.delete(runId);
      waiter.signal.removeEventListener("abort", waiter.onAbort);
      waiter.resolve(grant(bucket, runId));
    }
    fanOut(bucket, bucket.controller.waiters(now(), waiterCount(bucket)));
    armCooldownWake(bucket);
  };

  const tryAdmit: WorkflowConcurrencyPort["tryAdmit"] = (runId, key) => {
    const bucket = bucketFor(key);
    fanOut(bucket, bucket.controller.observe(now()));
    if (waiterCount(bucket) > 0 || !bucket.controller.canAdmit(now())) return undefined;
    return grant(bucket, runId);
  };

  const admit: WorkflowConcurrencyPort["admit"] = (runId, key, signal) => {
    const bucket = bucketFor(key);
    if (signal.aborted) return Promise.reject(abortedError(signal));
    return new Promise<ModelRequestAdmissionTicket>((resolve, reject) => {
      const waiter: Waiter = { runId, resolve, reject, signal, onAbort: () => {} };
      waiter.onAbort = () => {
        const queue = bucket.queues.get(runId);
        if (queue !== undefined) {
          const index = queue.indexOf(waiter);
          if (index >= 0) queue.splice(index, 1);
          if (queue.length === 0) bucket.queues.delete(runId);
        }
        reject(abortedError(signal));
        fanOut(bucket, bucket.controller.waiters(now(), waiterCount(bucket)));
        armCooldownWake(bucket);
      };
      signal.addEventListener("abort", waiter.onAbort, { once: true });
      const queue = bucket.queues.get(runId) ?? [];
      queue.push(waiter);
      bucket.queues.set(runId, queue);
      drain(bucket);
    });
  };

  const subscribe: WorkflowConcurrencyPort["subscribe"] = (runId, listener) => {
    const set = listeners.get(runId) ?? new Set();
    set.add(listener);
    listeners.set(runId, set);
    return () => {
      set.delete(listener);
      if (set.size === 0) listeners.delete(runId);
    };
  };

// No queuing, no cooling; observe in grant. The fast path always hits, so the runner is never the primary agent
// The request is queued / admitted.
  const observerAdmission: ModelRequestAdmission = {
    tryAcquire: ({ model }) => grant(bucketFor(workflowConcurrencyKey(model)), OBSERVER_RUN_ID),
    acquire: ({ model }) =>
      Promise.resolve(grant(bucketFor(workflowConcurrencyKey(model)), OBSERVER_RUN_ID)),
  };

  return {
    tryAdmit,
    admit,
    subscribe,
    observer: () => observerAdmission,
    snapshot(key) {
      return buckets.get(key)?.controller.snapshot();
    },
  };
}

function abortedError(signal: AbortSignal): Error {
  const reason = signal.reason;
  if (reason instanceof Error) return reason;
  return new Error("workflow request admission aborted");
}

let processGovernor: WorkflowConcurrencyGovernor | undefined;

/**
* Process-level singleton: the ceiling is counted once when it is first retrieved; thereafter the main runtime of each app (session) hangs it
* observer, run service gives the same port to the driver.
 */
export function getWorkflowConcurrencyGovernor(): WorkflowConcurrencyGovernor {
  processGovernor ??= createWorkflowConcurrencyGovernor({
    ceiling: resolveWorkflowConcurrencyCeiling(),
  });
  return processGovernor;
}
