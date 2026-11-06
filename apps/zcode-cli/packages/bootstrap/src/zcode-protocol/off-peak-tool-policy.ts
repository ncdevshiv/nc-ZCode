import { zcodeWorkspaceUpdateOffPeakToolPolicyParamsSchema } from "@zcode/shared";
import { parseParams, type ZCodeProtocolAgentServerContext } from "./server-types.js";

/**
* Off-Peak toolface access (valid in grayscale && local workspace) is a workspace level fact. CLI process by workspace
* Isolation, so just cache one copy; createRecord supports legacy create/resume, v4 createSession and v4 cold recovery
* (subscribe → resumePersistedSession, no host parameter channel) read uniformly.
* Only affects records created/restored later; the tool surface of active records is not recycled (consistent with the grayscale midway flipping strategy).
 */
export async function updateOffPeakToolPolicy(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
) {
  const params = parseParams(zcodeWorkspaceUpdateOffPeakToolPolicyParamsSchema, rawParams);
  context.appRuntimePreferences.offPeakToolEnabled = params.enabled;
  return { workspace: params.workspace, enabled: params.enabled };
}
