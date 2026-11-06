// ============================================================
// workflow concurrency ceiling: the only implementation of CPU derived values
// ============================================================
// `max(1, min(16, availableParallelism() − 2))`
// It is not only the concurrency upper bound of each run (commit timing, dwf_run, and resume are still used), but also the upper bound of each process-level manager.
// The **starting point and ceiling** of the provider bucket. The formula must converge to a single function: copy in run service / snippet
// service / legacy `Workflow` tool drifts in three places, which will make the "run upper bound" and "bucket ceiling" misaligned.

import { availableParallelism as osAvailableParallelism } from "node:os";

/** Hard cap for concurrency upper bound (follow legacy concurrency). */
const WORKFLOW_CONCURRENCY_CEILING_MAX = 16;
/** The number of cores set aside for the main agent and host processes. */
const RESERVED_PARALLELISM = 2;
/** Floor: parallelism − 2 == 0 on dual-core machines, at least one probe must be left running. */
const WORKFLOW_CONCURRENCY_FLOOR = 1;

/**
* CPU-derived concurrency ceiling. `availableParallelism` can be injected for testing a fixed number of cores (floor/hardtop two use cases).
 */
export function resolveWorkflowConcurrencyCeiling(
  availableParallelism: () => number = osAvailableParallelism,
): number {
  return Math.max(
    WORKFLOW_CONCURRENCY_FLOOR,
    Math.min(WORKFLOW_CONCURRENCY_CEILING_MAX, availableParallelism() - RESERVED_PARALLELISM),
  );
}
