import type { AgentRuntimeInternal } from "../internal.js";

/**
* Registration will cross the current synchronization call stack's runtime-owned work.
 *
* In the past, the session resident pool only observed active turn and task registry; title, MCP startup,
* Detached Promise such as ledger write is not among the two and may be closed as idle while still reading and writing the runtime.
* The count must be incremented in the same synchronization slice that the Promise was initiated in, and only released in the finally.
 */
export function trackResidencyBlockingWork<T>(
  this: AgentRuntimeInternal,
  work: Promise<T>,
): Promise<T> {
  this.residencyBlockingWorkCount += 1;
  return work.finally(() => {
    this.residencyBlockingWorkCount = Math.max(0, this.residencyBlockingWorkCount - 1);
  });
}

/** The Session resident pool only consumes this item, and the bootstrap guess list will no longer be modified when adding a sidecar. */
export function hasResidencyBlockingWork(this: AgentRuntimeInternal): boolean {
  return (
    this.hasActiveOrQueuedTurnWork() ||
    this.hasRunningBackgroundTasks() ||
    this.residencyBlockingWorkCount > 0 ||
    (this.memoryExtractionScheduler?.hasPendingWork() ?? false)
  );
}
