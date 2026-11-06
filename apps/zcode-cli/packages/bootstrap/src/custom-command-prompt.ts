import {
  expandCustomCommandPrompt,
  expandCustomCommandTemplate,
  formatCustomCommandPrompt,
  type ExecutionPort,
  type SessionId,
  type TraceContext,
} from "@zcode/contracts";
import { loadZCodeCustomCommand, type ListZCodeCustomCommandsOptions } from "./custom-commands.js";
import { expandCustomCommandShellSyntax } from "./custom-command-shell-expansion.js";
import { isReservedZCodeSlashCommandName } from "./slash-command-surface.js";

const CUSTOM_COMMAND_NOT_FOUND_PATTERN = /not found/i;
const PROMPT_CUSTOM_COMMAND_PATTERN = /^\/([^\s]+)(?:\s+([\s\S]*))?$/;

interface ResolveZCodeCustomCommandPromptOptions extends ListZCodeCustomCommandsOptions {
  /**
* Custom command name (lowercase) not addressable for this session. Is the same as `isReservedZCodeSlashCommandName`
*Two sources of the concept: that one is a global reserved word, and this one is an access control issued by the caller on a per-session basis. Currently, the only user is
* `workflow` when dynamic workflow grayscale is turned off.
* Deliberately press **command name** instead of disabling path expression: `workflow` comes from the zcode-guide plugin packaged with the CLI,
* Its SKILL/command file falls in the plug-in cache directory, and the caller cannot get the stable path.
   */
  disabledCommandNames?: readonly string[];
  executionPort?: ExecutionPort;
  sessionId?: SessionId;
  signal?: AbortSignal;
  traceContext?: TraceContext;
}

export async function resolveZCodeCustomCommandPrompt(
  input: string,
  options: ResolveZCodeCustomCommandPromptOptions = {},
): Promise<string | undefined> {
  const invocation = parsePromptCustomCommandInvocation(input);
  if (!invocation || isReservedZCodeSlashCommandName(invocation.name)) {
    return undefined;
  }
// Minimum intrusion point: This is the only entry point for `/name` to become a command search, and it is also where the reserved word judgment has settled.
// If placed before load, the command blocked by the access control will not even read the file; returning undefined is the same as "command does not exist".
// The upper input facade will hand over the original text to the model as a normal prompt, and no semi-expanded prompt words will appear.
  if (options.disabledCommandNames?.includes(invocation.name)) {
    return undefined;
  }

  try {
    const command = await loadZCodeCustomCommand({
      ...options,
      name: invocation.name,
    });
    if (options.executionPort) {
// When calling slash command, execute it first and replace the output to avoid unsupported error
// Make the UI stay in "thinking" for a long time after throwing before turn is created.
      //
// Reuse the underlying template/format function of contracts instead of expandCustomCommandPrompt:
// The latter's internal detectUnsupportedDynamicSyntax will directly throw an error for `!` syntax, and this path
// To support shell expansion, it can only be used between expandCustomCommandTemplate and
// Insert expandCustomCommandShellSyntax between formatCustomCommandPrompt.
      const expanded = expandCustomCommandTemplate({
        args: invocation.args,
        command,
      });
      const body = await expandCustomCommandShellSyntax({
        command,
        content: expanded.body,
        executionPort: options.executionPort,
        sessionId: options.sessionId,
        signal: options.signal,
        traceContext: options.traceContext,
        workingDirectory: options.workingDirectory ?? process.cwd(),
      });
      return formatCustomCommandPrompt({
        argumentCount: expanded.argumentCount,
        body,
        command,
        usedArgumentsPlaceholder: expanded.usedArgumentsPlaceholder,
      }).prompt;
    }
    return expandCustomCommandPrompt({
      args: invocation.args,
      command,
    }).prompt;
  } catch (error) {
    if (error instanceof Error && CUSTOM_COMMAND_NOT_FOUND_PATTERN.test(error.message)) {
      return undefined;
    }
    throw error;
  }
}

function parsePromptCustomCommandInvocation(input: string): { args: string; name: string } | null {
  const match = PROMPT_CUSTOM_COMMAND_PATTERN.exec(input.trim());
  if (!match?.[1]) {
    return null;
  }
  return {
    args: match[2]?.trim() ?? "",
    name: match[1].toLowerCase(),
  };
}
