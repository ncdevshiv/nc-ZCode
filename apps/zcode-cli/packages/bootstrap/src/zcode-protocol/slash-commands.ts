import { BUILTIN_ZCODE_SLASH_COMMAND_HELP_ENTRIES, type ZCodeSlashCommand } from "@zcode/shared";
import {
  listZCodeCustomCommands,
  type ListZCodeCustomCommandsOptions,
} from "../custom-commands.js";
import {
  APP_PROTOCOL_APP_ONLY_BUILTIN_SLASH_COMMANDS,
  APP_PROTOCOL_VISIBLE_BUILTIN_SLASH_COMMAND_NAMES,
  isReservedZCodeSlashCommandName,
} from "../slash-command-surface.js";

/**
* `workflow` is a custom command of the zcode-guide built-in plug-in, packaged with the CLI, and is not subject to user commandOverrides
* Impact; when grayscale is turned off, it can only be eliminated by name when assembling the directory.
 */
const DYNAMIC_WORKFLOW_SLASH_COMMAND_NAME = "workflow";

export interface ListProtocolSlashCommandsOptions extends ListZCodeCustomCommandsOptions {
  /**
* Dynamic workflow grayscale gate. **Only explicit false
* Only removed** `workflow`: CLI's own directory assembly (TUI / caller not participating in grayscale) lacks this field,
* Must remain as is. The protocol server always passes an explicit boolean from appRuntimePreferences.
   */
  dynamicWorkflowEnabled?: boolean;
}

export async function listProtocolSlashCommands(
  options: ListProtocolSlashCommandsOptions = {},
): Promise<ZCodeSlashCommand[]> {
  const builtins = listAppProtocolBuiltinSlashCommands();
  let customCommands: Awaited<ReturnType<typeof listZCodeCustomCommands>>["commands"] = [];
  try {
    const outcome = await listZCodeCustomCommands(options);
    customCommands = outcome.commands;
  } catch {
// Failure to discover custom commands should not block the session snapshot; retain executable built-in protocol commands.
    customCommands = [];
  }

  return pinWorkflowAfterGoal([
    ...builtins,
    ...customCommands
      .filter((command) => !command.disableNonInteractive)
      .filter((command) => !isReservedZCodeSlashCommandName(command.name))
// Grayscale off: Composer's plus menu and `/` panel only read this directory, and if they are removed, the two entries will disappear together. The subsequent pinWorkflowAfterGoal when turned on continues to pin it after the goal.
      .filter(
        (command) =>
          options.dynamicWorkflowEnabled !== false ||
          command.name !== DYNAMIC_WORKFLOW_SLASH_COMMAND_NAME,
      )
      .map((command) => ({
        description: command.description,
        inputHint: `/${command.name}${command.argumentHint ? ` ${command.argumentHint}` : ""}`,
        name: command.name,
        source: "custom" as const,
      })),
  ]);
}

/**
* The App `/` panel is displayed in the order of this directory, and this function is the only sorting point (the UI does not maintain a sorting whitelist).
* `workflow` is a custom command of the zcode-guide built-in plug-in.
* It will be buried at the end of the custom section in the order of discovery; the product requires it to be the entrance to "start a piece of work" like `goal`.
* Immediately after goal.
* Only the order is adjusted: the source, deduplication and reserved rules remain unchanged; the original order is maintained when either party is absent.
 */
function pinWorkflowAfterGoal(commands: ZCodeSlashCommand[]): ZCodeSlashCommand[] {
  const workflowIndex = commands.findIndex(
    (command) => command.name === DYNAMIC_WORKFLOW_SLASH_COMMAND_NAME,
  );
  if (workflowIndex < 0 || !commands.some((command) => command.name === "goal")) return commands;
  const [workflow] = commands.splice(workflowIndex, 1);
  const goalIndex = commands.findIndex((command) => command.name === "goal");
  commands.splice(goalIndex + 1, 0, workflow!);
  return commands;
}

function listAppProtocolBuiltinSlashCommands(): ZCodeSlashCommand[] {
  const sharedBuiltins = APP_PROTOCOL_VISIBLE_BUILTIN_SLASH_COMMAND_NAMES.flatMap((name) => {
    const command = BUILTIN_ZCODE_SLASH_COMMAND_HELP_ENTRIES.find((entry) => entry.name === name);
    if (!command) return [];
    return [
      {
        description: command.summary,
        inputHint: command.usage,
        name: command.name,
        source: "builtin" as const,
      },
    ];
  });
  return [...sharedBuiltins, ...APP_PROTOCOL_APP_ONLY_BUILTIN_SLASH_COMMANDS];
}
