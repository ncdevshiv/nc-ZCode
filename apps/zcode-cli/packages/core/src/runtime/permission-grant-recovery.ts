import type { AgentRuntimeInternal } from "./internal.js";

//Do not save the second permission/queue, only record the recovery actions in which the transaction has been submitted but the event has not yet been released.
export const unpublishedPermissionGrants = new WeakMap<
  AgentRuntimeInternal,
  {
    interactionId: string;
    recover: () => Promise<string>;
  }
>();

export async function recoverPendingPermissionGrant(runtime: AgentRuntimeInternal): Promise<void> {
  await unpublishedPermissionGrants.get(runtime)?.recover();
}
