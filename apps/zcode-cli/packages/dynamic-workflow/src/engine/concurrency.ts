/**
* Adaptive concurrency controller: pure AIMD state machine on a provider key.
 *
* Pure package discipline: no clock reading, no I/O - `now` (ms since epoch) is always passed in by the caller; this class only answers
* "What is the cap now? Can it be admitted? Has the cap changed due to this signal?" Who feeds the signal and who queues it (bootstrap’s process level
* Manager) are outside the package.
 *
* The unit of measurement is **model requests** (per attempt): `inFlight` = number of requests that have been admitted but not yet released.
* There is no ask level count - a sub-agent has only one request flying at the same time, "≤ N requests" is equivalent to "≤ N sub-agent running".
 *
* Batch damping relies on **epoch**: the current epoch is recorded for each admission, and each current limit decision is epoch += 1. A 429 only in
* Its request is issued under the **current** cap (the epoch is the same) and only evaluates the current cap; 429 of the old epoch evaluates a cap that has been
* The cut off cap only clears streak and refreshes cooldown. The same goes for success: only the success of the current epoch is counted in the streak.
 *
* The signal methods all return a list of cap changes caused by this signal (usually 0 or 1; when an idle reset is followed by a current limit, it will be 2),
* The caller fans out unchanged into the `concurrency-changed` event.
 */

import type { ConcurrencyChange, ConcurrencyChangeReason } from "./types.js";

/**
* Current limiting is `cap = max(FLOOR, floor(cap × 0.75))` (the coefficient is changed from 0.5 to 0.75):
* Halving is an overreaction to "only one or two more" transgressions - the gate is request-level, once 429 means the cap is high, rarely means twice as high.
 */
export const CONCURRENCY_DECREASE_FACTOR = 0.75;
/** Additive increment step size. */
export const CONCURRENCY_INCREASE_STEP = 1;
/**
* K number of consecutive successful model requests required per +1. Only one gear. The value drops from 40 to 4:
* The cost of failed detection is just to hit 429 once per request and then return `lastGood` (the proven available water level), which is not worth using 40 times.
* Successfully go to another trial.
 */
export const CONCURRENCY_INCREASE_AFTER_SUCCESSES = 4;
/** There is always at least one probe running. */
export const CONCURRENCY_FLOOR = 1;
/** After a certain key has been idle for so long (and not flying), it forgets the learned cap and returns to the ceiling. */
export const CONCURRENCY_IDLE_RESET_MS = 300_000;

/** Current limiting reasons that will reduce cap. */
export type ConcurrencyThrottleReason = Extract<
  ConcurrencyChangeReason,
  "rate_limited" | "provider_overloaded" | "offpeak_queued"
>;

/** Read-only snapshot of the controller (used for testing assertions and the `concurrency` of the manager's projected run header). */
export interface ConcurrencyControllerSnapshot {
  readonly key: string;
  readonly ceiling: number;
  readonly cap: number;
/** Current limit ruling count; sent to the request during admission, and brought back for comparison at the end of the request. */
  readonly epoch: number;
  readonly inFlight: number;
  readonly waiters: number;
  readonly successStreak: number;
  readonly cooldownUntil?: number;
  readonly lastRequestAt?: number;
  readonly lastGood?: number;
  readonly lastBad?: number;
}

export class ConcurrencyController {
  private cap: number;
  private epoch = 0;
  private inFlight = 0;
  private waiters_ = 0;
  private successStreak = 0;
  private cooldownUntil?: number;
  private lastRequestAt?: number;
  private lastGood?: number;
  private lastBad?: number;

  /**
* @param key provider key (`${providerId}/${modelId}`), only used to fill in the change event - the controller itself
* Indifferent to it. Put it in the construction parameters instead of passing in each signal: a controller only serves one key, which is the identity, not the parameter.
* @param ceiling CPU derived ceiling: both initial value and upper bound.
   */
  constructor(
    readonly key: string,
    readonly ceiling: number,
  ) {
    this.cap = ceiling;
  }

  snapshot(): ConcurrencyControllerSnapshot {
    return {
      key: this.key,
      ceiling: this.ceiling,
      cap: this.cap,
      epoch: this.epoch,
      inFlight: this.inFlight,
      waiters: this.waiters_,
      successStreak: this.successStreak,
      ...(this.cooldownUntil === undefined ? {} : { cooldownUntil: this.cooldownUntil }),
      ...(this.lastRequestAt === undefined ? {} : { lastRequestAt: this.lastRequestAt }),
      ...(this.lastGood === undefined ? {} : { lastGood: this.lastGood }),
      ...(this.lastBad === undefined ? {} : { lastBad: this.lastBad }),
    };
  }

  /**
* Gate (admission condition): The number of in-flight requests is lower than the cap and not in the Retry-After cooldown. Pure query, no idle reset——
* The caller (manager) first adjusts {@link observe} on the admission path (admission is also a "signal", the first one after one hour of idle time
* run starts from the ceiling immediately).
   */
  canAdmit(now: number): boolean {
    return (
      this.inFlight < this.cap && (this.cooldownUntil === undefined || now >= this.cooldownUntil)
    );
  }

/** Only do "empty signal" for idle reset check (used before admission, before observer release). */
  observe(now: number): ConcurrencyChange[] {
    return this.idleReset(now);
  }

  /**
* A request is admitted: `inFlight++`, refresh `lastRequestAt`, and return the epoch to which it belongs (brought back when the request ends).
* No idle reset - the caller has already done it in {@link observe}; if it is done again here, the one that has just been observed is determined to be "not idle"
*'s admission cannot become idle.
   */
  admitted(now: number): number {
    this.lastRequestAt = now;
    this.inFlight += 1;
    return this.epoch;
  }

  /**
* A request ends successfully: `inFlight--`; only the success of **current epoch** uses
* `successStreak++` - The success of the old epoch proves the load after the backoff is reduced under the old cap, not that the new cap can be higher.
* `successStreak ≥ K` means **prove** that the current cap is available: `lastGood = max(lastGood, cap)` (only
* LastGood can be set for completed streaks, but not for reduced caps). Then satisfy that there are waiters **and** `cap < ceiling` → +1.
* When there is no waiter, the streak will be accumulated but not cashed out.
   */
  succeeded(now: number, epoch: number): ConcurrencyChange[] {
    const changes = this.idleReset(now);
    this.inFlight = Math.max(0, this.inFlight - 1);
    if (epoch !== this.epoch) return changes;
    this.successStreak += 1;
    if (this.successStreak < CONCURRENCY_INCREASE_AFTER_SUCCESSES) return changes;
// This level is proven usable by an entire streak - regardless of whether anyone is waiting to climb up at the moment.
    if (this.lastGood === undefined || this.cap > this.lastGood) this.lastGood = this.cap;
    if (this.waiters_ <= 0 || this.cap >= this.ceiling) return changes;
    const previous = this.cap;
    this.cap = Math.min(this.ceiling, previous + CONCURRENCY_INCREASE_STEP);
    this.successStreak = 0;
    changes.push(this.change(previous, "recovered"));
    return changes;
  }

  /**
* Throttled/overloaded. Always use `inFlight--`, clear streak, and use Retry-After to cooldown
* Push to later date.
   *
* Only 429 of `epoch === current epoch` will move the cap: it is a request issued under the current cap and is an evaluation of the current cap;
* 429 of old epochs are ignored (no events are emitted). The current epoch is divided into two levels: above `lastGood` **detection is current limited → returned
* `lastGood` (remember `lastBad`, not reduced by coefficient); otherwise (`lastGood` is absent or `cap ≤ lastGood`: the wall has moved down) →
* `cap = max(FLOOR, floor(cap × 0.75))`, remember `lastBad = old cap`, and **clear** `lastGood` - it has just been
* Falsification, and the new cap has not been proven by any streak (minus cap does not have lastGood). After two gears `epoch += 1`——
* Even if the cap is on the floor and the value has not changed, turn the page: the same batch of requests can only trigger one ruling.
   */
  throttled(
    now: number,
    epoch: number,
    reason: ConcurrencyThrottleReason,
    retryAfterMs?: number,
  ): ConcurrencyChange[] {
    const changes = this.idleReset(now);
    this.inFlight = Math.max(0, this.inFlight - 1);
    this.successStreak = 0;
    const cooldownMs = retryAfterMs !== undefined && retryAfterMs > 0 ? retryAfterMs : undefined;
    if (cooldownMs !== undefined) {
      this.cooldownUntil = Math.max(this.cooldownUntil ?? 0, now + cooldownMs);
    }
    if (epoch !== this.epoch) return changes;

    const previous = this.cap;
    if (this.lastGood !== undefined && this.cap > this.lastGood) {
      this.lastBad = this.cap;
      this.cap = this.lastGood;
    } else {
      this.cap = Math.max(CONCURRENCY_FLOOR, Math.floor(this.cap * CONCURRENCY_DECREASE_FACTOR));
      this.lastBad = previous;
      this.lastGood = undefined;
    }
    this.epoch += 1;
// When the cap is already on the floor and there is no Retry-After, nothing changes and no event is sent; even if the current limit with Retry-After does not move the cap
// Also let the run header know "cooling to...", so all with cooldownMs are sent.
    if (previous === this.cap && cooldownMs === undefined) return changes;
    changes.push(this.change(previous, reason, cooldownMs));
    return changes;
  }

/** Transient but non-current limiting failure (timeout / 5xx / network): `inFlight--`, clear streak, cap does not move. */
  failedTransient(now: number): ConcurrencyChange[] {
    const changes = this.idleReset(now);
    this.inFlight = Math.max(0, this.inFlight - 1);
    this.successStreak = 0;
    return changes;
  }

/** A request terminates with permanent failure/cancellation, or the ticket only sees release but not the final event: just do `inFlight--`. */
  ended(now: number): ConcurrencyChange[] {
    const changes = this.idleReset(now);
    this.inFlight = Math.max(0, this.inFlight - 1);
    return changes;
  }

/** The number of waiters fed by the manager when the queue changes (cap is added only when needed). */
  waiters(now: number, count: number): ConcurrencyChange[] {
    const changes = this.idleReset(now);
    this.waiters_ = Math.max(0, count);
    return changes;
  }

// ——————————————————————————————— Internal ——————————————————————————————

  /**
* Idle reset: `now − lastRequestAt ≥ IDLE_RESET_MS` and nothing is flying → cap back to the ceiling,
* Clear streak/cooldown/lastGood/lastBad, epoch += 1. Laziness occurs when any signal arrives (no timer).
   */
  private idleReset(now: number): ConcurrencyChange[] {
    if (
      this.lastRequestAt === undefined ||
      this.inFlight !== 0 ||
      now - this.lastRequestAt < CONCURRENCY_IDLE_RESET_MS
    ) {
      return [];
    }
    const previous = this.cap;
// Idempotent: do nothing when already at the ceiling and the state is clean (otherwise turn the page for each idle signal epoch).
    if (
      previous === this.ceiling &&
      this.successStreak === 0 &&
      this.cooldownUntil === undefined &&
      this.lastGood === undefined &&
      this.lastBad === undefined
    ) {
      return [];
    }
    this.cap = this.ceiling;
    this.successStreak = 0;
    this.cooldownUntil = undefined;
    this.lastGood = undefined;
    this.lastBad = undefined;
    this.epoch += 1;
// lastRequestAt reserved: signals that are still idle next time should not be "reset" again; the status is already at the ceiling at this time.
    return previous === this.cap ? [] : [this.change(previous, "idle_reset")];
  }

  private change(
    previous: number,
    reason: ConcurrencyChangeReason,
    cooldownMs?: number,
  ): ConcurrencyChange {
    return {
      key: this.key,
      previous,
      next: this.cap,
      reason,
      ...(this.lastGood === undefined ? {} : { lastGood: this.lastGood }),
      ...(this.lastBad === undefined ? {} : { lastBad: this.lastBad }),
      ...(cooldownMs === undefined ? {} : { cooldownMs }),
    };
  }
}
