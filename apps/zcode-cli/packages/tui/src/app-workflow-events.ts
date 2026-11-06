// Session event → bridge of workflowRuns mirroring (the dwf case of app-events.ts only calls this function).
//
// There are two reasons for making it a separate module: collect the payload type of contracts here, and let app-workflow-mirror.ts
// Maintain the discipline of "only rely on @zcode/shared" (the same attitude as shared reducers are not allowed to rely on contracts in reverse);
// By the way, keep the switch of app-events.ts within max-lines.
import type { DynamicWorkflowRunProgressPayload } from "@zcode/contracts";
import type { WorkflowRunProgressEnvelope } from "@zcode/shared/zcode-protocol-v4";
import { applyWorkflowProgressToMirror, type TuiWorkflowMirror } from "./app-workflow-mirror.js";

export type WorkflowMirrorSetter = (
  updater: (current: TuiWorkflowMirror) => TuiWorkflowMirror,
) => void;

/**
* Reduce a `dynamic_workflow_run_progress` event into the image.
 *
* The counterpart on the projection side is bootstrap's `onDynamicWorkflowRunProgress` - both sides call the same shared reducer,
* Therefore, it is impossible for TUI and desktop to calculate different states for the same series of events.
 */
export function applyWorkflowProgressEvent(
  payload: unknown,
  setWorkflowMirror?: WorkflowMirrorSetter,
): void {
  if (!setWorkflowMirror) return;
// First convert the bounded payload of contracts, and then assign it to the structured input parameter of shared: this line of assignment is "the shape of both sides does not drift"
// compile-time gate (same attitude as bootstrap's v4 projection).
  const envelope: WorkflowRunProgressEnvelope = payload as DynamicWorkflowRunProgressPayload;
// When there is no change, applyWorkflowProgressToMirror returns the same reference, so React skips re-rendering directly:
// The "null = no change in semantics" of the shared reducer means "no redrawing" on the UI side.
  setWorkflowMirror((current) => applyWorkflowProgressToMirror(current, envelope));
}
