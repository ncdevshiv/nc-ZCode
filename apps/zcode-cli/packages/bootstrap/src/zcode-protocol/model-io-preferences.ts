import { zcodeWorkspaceUpdateModelIoPreferencesParamsSchema } from "@zcode/shared";
import { parseParams, type ZCodeProtocolAgentServerContext } from "./server-types.js";

/**
* The ModelIO disk writing strategy is a global preference of the App, but each resident session holds an independent adapter.
* Therefore, the protocol layer also caches preferences for future sessions to inherit, and immediately updates existing sessions to avoid splitting of new and old task behaviors.
 */
export async function updateModelIoPreferences(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
) {
  const params = parseParams(zcodeWorkspaceUpdateModelIoPreferencesParamsSchema, rawParams);
  const enabled = params.preferences.fullRetentionEnabled;
  context.appRuntimePreferences.modelIoFullRetentionEnabled = enabled;

  let updatedSessionCount = 0;
  for (const record of context.sessions.values()) {
    if (!record.app.setModelIoFullRetentionEnabled) continue;
    record.app.setModelIoFullRetentionEnabled(enabled);
    updatedSessionCount += 1;
  }

  return {
    workspace: params.workspace,
    fullRetentionEnabled: enabled,
    updatedSessionCount,
  };
}
