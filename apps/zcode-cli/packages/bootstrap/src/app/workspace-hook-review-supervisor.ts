import { SessionEventType } from "@zcode/contracts";
import type { WorkspaceHookReviewFlow, WorkspaceHookReviewFlowRegistry } from "@zcode/core";
import type { WorkspaceHookReviewHostPort } from "./workspace-hook-review-types.js";
import type { WorkspaceHookReviewTelemetry } from "./workspace-hook-review-telemetry.js";

/**
* Supervise a review flow until it terminates: follow the supersede chain and complete it on timeout
* telemetry and ReviewSettled.
 *
* This logic is the **only** place to await flow.result: if no one awaits it, its 10-minute deadline will be in the registry after expiration
* The internal silent settlement becomes timed_out, the front end cannot receive ReviewSettled, and the panel continues to be rendered as pending.
* Each subsequent click will be judged as workspace_hooks_review_superseded by registry.validate
* (In actual testing, all 9 consecutive clicks were rejected and will not be automatically reopened).
 *
* Separate modules are used to share requestReview with the reopening path after revoke: the entrance to any newly opened flow must be
* Leave it to be supervised, otherwise it will also produce unsupervised orphan flows.
 */
export async function superviseWorkspaceHookReviewFlow(input: {
  flow: WorkspaceHookReviewFlow;
  host: WorkspaceHookReviewHostPort;
  registry: WorkspaceHookReviewFlowRegistry;
  sessionId: string;
  telemetry: WorkspaceHookReviewTelemetry;
}): Promise<void> {
  let flow = input.flow;
  while (true) {
    const outcome = await flow.result;
    if (outcome.reasonCode === "workspace_hooks_review_superseded") {
      const current = input.registry.getCurrentFlow(input.sessionId);
      if (!current || current === flow) return;
      flow = current;
      continue;
    }
    if (outcome.reasonCode === "workspace_hooks_interaction_timeout") {
      input.telemetry.timeout(flow.request);
      await input.host.emit({
        type: SessionEventType.WorkspaceHookReviewSettled,
        payload: {
          interactionId: flow.request.interactionId,
          state: "timed_out",
          reasonCode: outcome.reasonCode,
        },
      });
    }
    return;
  }
}
