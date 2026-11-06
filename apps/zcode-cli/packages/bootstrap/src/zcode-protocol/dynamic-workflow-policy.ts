import { zcodeWorkspaceUpdateDynamicWorkflowPolicyParamsSchema } from "@zcode/shared";
import { parseParams, type ZCodeProtocolAgentServerContext } from "./server-types.js";

/**
* Dynamic workflow grayscale gate. The decision lies with the Host:
* It reads `dynamicWorkflow.mode` in `/client/configs`, the CLI only caches the conclusion and never reads the feature key
* or override environment variables locally. Isomorphic to off-peak-tool-policy.ts: CLI processes are isolated by workspace,
* Just cache one copy; createRecord supports legacy create/resume, v4 createSession and v4 cold recovery
* (subscribe → resumePersistedSession, no host parameter channel) read uniformly.
* Only affects records created/restored later; the tool surface of active records is not recycled (the grayscale midway flipping strategy is the same).
 */
export async function updateDynamicWorkflowPolicy(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
) {
  const params = parseParams(zcodeWorkspaceUpdateDynamicWorkflowPolicyParamsSchema, rawParams);
  context.appRuntimePreferences.dynamicWorkflowEnabled = params.enabled;
  return { workspace: params.workspace, enabled: params.enabled };
}
