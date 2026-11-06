// ============================================================
// The background stop branch of workflow run (local_dynamic_workflow dispatch of runtime.stopBackgroundTask)
// ============================================================
//
// This is the end of the only cancellation path. Both entries flow into the same implementation:
//   1. GUI：v4 `cancelBackgroundWork {workId}` → `app.cancelBackgroundTask`
//      → `runtime.cancelBackgroundTask` → `runtime.stopBackgroundTask`（lenient）
// 2. Model: `TaskStop` tool → `backgroundTaskControlPort.stopBackgroundTask`
// (runtime-tools.ts binds it to the same `runtime.stopBackgroundTask`, strict: true)
// The two only differ in strict (strict allows terminated tasks to return not_running instead of relaxed success),
// Dispatch behaves exactly as this module does later - there is no second cancel semantics.
//
// Separate modules are used to limit the size of background.ts and avoid further increasing branch complexity.
// Only one import and one line of dispatch remain on the background.ts side.

import type { AgentRuntimeInternal } from "../internal.js";
import type {
  RuntimeBackgroundStopInitiator,
  RuntimeBackgroundStopResult,
  TypedRuntimeBackgroundStopTarget,
} from "./background-stop-types.js";

/**
* Stop a workflow run: hand over taskId (≡ runId ≡ workId, no identity mapping table)
 * `DynamicWorkflowRunPort.cancel`。
 *
* AbortController in the abort registry inside the port: terminate the on-the-fly ask, kill sandbox process, and let the engine process
* `stop(initiator)` resolves run to `stopped(user | model)` (**not** `errored`——journal is app
* Read persistent records, the semantics of both states are different from resume UX, harness never encodes failure as stop).
 *
* Both downgrades return structured results rather than pretending to succeed:
* - Port completely absent (unwired host) → capability not supported;
* - Port returns false → not_found for unknown/settled runs.
 */
export async function stopDynamicWorkflowBackgroundTask(
  this: AgentRuntimeInternal,
  target: TypedRuntimeBackgroundStopTarget,
  unsupported: (target: TypedRuntimeBackgroundStopTarget) => RuntimeBackgroundStopResult,
  initiator?: RuntimeBackgroundStopInitiator,
): Promise<RuntimeBackgroundStopResult> {
  const port = this.dynamicWorkflowRunPort;
  if (!port) {
    return unsupported(target);
  }
// First remember "who stopped it" and then abort: the final notification is cast when the waiter is settled later, and the registry entry is between the two.
// The only shared state. Written before abort,
// Otherwise, the settlement may read the null value first.
  if (initiator !== undefined && target.registryTask !== undefined) {
    this.runtimeTaskRegistry.update(target.taskId, (current) => ({
      ...current,
      stopInitiator: initiator,
    }));
  }
//The reason depends on cancel: run is settled into `stopped(user|model)`,
// Final state notification and GetWorkflowRun read "who stopped" from the journal, instead of relying solely on the registry above.
// The fact that the user stopped by himself is also the one read by AmendWorkflow's confirmation-free rule: the user has just expressed "stop", and the model should be revised and asked again.
  const cancelled = await port.cancel(target.taskId, initiator);
  if (!cancelled) {
    return {
      ok: false,
      reason: "background_task_not_found",
      status: "lost",
      taskId: target.taskId,
      type: "local_dynamic_workflow",
    };
  }
  return {
    ok: true,
    status: "cancelled",
    taskId: target.taskId,
    type: "local_dynamic_workflow",
  };
}
