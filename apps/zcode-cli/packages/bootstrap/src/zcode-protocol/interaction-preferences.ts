import { zcodeWorkspaceUpdateInteractionPreferencesParamsSchema } from "@zcode/shared";
import { parseParams, type ZCodeProtocolAgentServerContext } from "./server-types.js";

/**
* Apply workspace interaction preferences. CLI processes are isolated by workspace, so registry is that workspace
* The authoritative gate shared by internal main tasks, background tasks and sub-Agents.
 */
export async function updateInteractionPreferences(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
) {
  const params = parseParams(zcodeWorkspaceUpdateInteractionPreferencesParamsSchema, rawParams);
  const enabled = params.preferences.askUserQuestionAutoResolutionEnabled;
  context.appRuntimePreferences.askUserQuestionAutoResolutionEnabled = enabled;
  const snoozedInteractionCount =
    await context.v4Interactions.setAskUserQuestionAutoResolutionEnabled(enabled);

  return {
    workspace: params.workspace,
    askUserQuestionAutoResolutionEnabled: enabled,
    snoozedInteractionCount,
  };
}
