// v4 workspace-config only hosts workspace presentation; model candidates and preferences are determined by the target Host
// Provided by ModelSelectionView, the second model directory cannot be created reversely from live Session settings.
import { getZCodeAgentModeSelectOptions, normalizeAvailableZCodeMode } from "@zcode/shared";
import type { ZCodeSessionSettingsState, ZCodeSlashCommand } from "@zcode/shared";
import type { WorkspaceConfigState } from "@zcode/shared/zcode-protocol-v4";
import { mapSessionSettings } from "./mapper.js";
import { listProtocolSlashCommands } from "./slash-commands.js";
import type { ZCodeProtocolAgentServerContext } from "./server-types.js";

/** Session settings only project non-model workspace mode and slash commands. */
function toV4WorkspaceConfigState(
  settings: ZCodeSessionSettingsState,
  slashCommands: readonly ZCodeSlashCommand[],
): WorkspaceConfigState {
  return {
    configOptions: [
      {
        id: "mode",
        name: "Mode",
        category: "mode",
        type: "select",
        currentValue: normalizeAvailableZCodeMode(settings.mode.current),
        options: getZCodeAgentModeSelectOptions(),
      },
    ],
    slashCommands: slashCommands.map((command) => ({
      name: command.name,
      description: command.description,
      ...(command.inputHint !== undefined ? { inputHint: command.inputHint } : {}),
      ...(command.source !== undefined ? { source: command.source } : {}),
    })),
  };
}

/**
* Seed when subscribing: only take the live session fast path (mapSessionSettings is read directly into the app).
* Deliberately do not build a temporary app - the host starts preheating and reads the workspace presentation first when there is no active session.
* Create a temporary app and hold the protocol channel by MCP close (see comments for desktop warmUpZCodeAgent).
* Returns null (empty directory seed) when there is no registered session; the model selection directory is determined by the Host's own process Registry View
* Provided, this session protocol only releases task-level configuration after a live session occurs.
 */
export async function buildLiveWorkspaceConfigStateV4(
  context: ZCodeProtocolAgentServerContext,
  workspaceId: string,
): Promise<WorkspaceConfigState | null> {
  const record = Array.from(context.sessions.values()).find(
    (candidate) => candidate.workspace.workspaceKey === workspaceId,
  );
  if (!record) return null;
  const settings = await mapSessionSettings(record.app);
  const slashCommands = await listProtocolSlashCommands({
//The gray gate is the workspace level fact determined by the Host, and the directory is equipped with a read process cache.
    dynamicWorkflowEnabled: context.appRuntimePreferences.dynamicWorkflowEnabled,
    env: context.deps.env,
    logger: context.logger,
    workingDirectory: record.workspace.workspacePath,
  });
  return toV4WorkspaceConfigState(settings, slashCommands);
}
