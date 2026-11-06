import { ModelRetryBudget } from "@zcode/contracts";

/**
* Retry the determination of budget gear.
 *
* "Unlimited" **only** relaxes the conditions for giving up on transient failures: the runner's attempt loop and the "can you try again" gate after failure;
* Backoff curve (2s→60s, jitter, Retry-After priority), classification of `isRetryableFailure`,
* No retry after `emittedRetryBoundaryEvent`, empty completion retry and compact path will not work.
 */

/** `maxAttempts` in status events represents "unlimited" sentinels (Infinity is not serializable, 0 does not occupy the legal count). */
export const UNBOUNDED_RETRY_MAX_ATTEMPTS = 0;

export function isUnboundedRetryBudget(budget: ModelRetryBudget | undefined): boolean {
  return budget === ModelRetryBudget.Unbounded;
}

/** Is it allowed to try again after failure (equivalent to the existing `retryBudgetAttempt < maxAttempts`, unbounded is always true). */
export function retryBudgetAllows(
  budget: ModelRetryBudget | undefined,
  retryBudgetAttempt: number,
  maxAttempts: number,
): boolean {
  return isUnboundedRetryBudget(budget) || retryBudgetAttempt < maxAttempts;
}

/** attempt loop continuation condition (equivalent to the existing `attempt <= loopMaxAttempts`, unbounded is always true). */
export function retryAttemptLoopContinues(
  budget: ModelRetryBudget | undefined,
  attempt: number,
  loopMaxAttempts: number,
): boolean {
  return isUnboundedRetryBudget(budget) || attempt <= loopMaxAttempts;
}

/** Written to status events/log under maxAttempts: unbounded is sentinel 0. */
export function retryBudgetMaxAttempts(
  budget: ModelRetryBudget | undefined,
  maxAttempts: number,
): number {
  return isUnboundedRetryBudget(budget) ? UNBOUNDED_RETRY_MAX_ATTEMPTS : maxAttempts;
}
