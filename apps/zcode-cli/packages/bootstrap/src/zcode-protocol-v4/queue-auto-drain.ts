import type { GoalStatus } from "@zcode/contracts";

/**
* The only automatic lifting gate for ordinary queues. fail-open does not open a bypass here: verifier still first
* The target is persisted to complete, and then shares this judgment with explicit pass.
 */
export function shouldAutoDrainV4QueueHead(input: {
  autoDrain: boolean;
  dispatchState: "queued" | "reserved" | "promoting";
  sessionBusy: boolean;
  targetStatus: GoalStatus | null;
}): boolean {
  return (
    input.autoDrain &&
    input.dispatchState === "queued" &&
    !input.sessionBusy &&
    (input.targetStatus === null || input.targetStatus === "complete")
  );
}
