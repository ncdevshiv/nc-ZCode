/**
* driver's **observation** glossary of an ask: usage ({@link AskStats}) and progress ({@link AskProgress})——
* The same turn parses the two halves of the report, half is the account, and the other half is "are you still moving?"
 *
* The reason for detaching from types.ts is the same as imported-cache-types.ts: that contract has reached oxlint’s max-lines
* Upper limit. The public side remains unchanged - types.ts exports each name here in place, and the import path is still `./types.js`.
 */

/** Usage statistics reported by the driver each time it completes an ask. */
export interface AskStats {
  tokens: number;
  toolCalls: number;
  turns: number;
  /**
* Among them, the number of calls to **view or move the external world**: read files, run commands, and access the network.
* Does not include protocol tools (`submit_result`, `escalate`) that only return results or questions to the engine, so a typed ask that only answers
* Here is 0 - such ask is **pure**, relying only on instructions and transcription prefixes, and can still be settled from the cache after the import cache is turned off.
* Old journal rows do not have this key and are treated as "touched" (conservative).
   */
  worldToolCalls?: number;
}

/**
* Maximum character limit (240) for `instructionsHead` on `node-queued`. One or two sentences are enough to explain this ask clearly.
* Without causing the event table to grow a copy of the instruction - the complete instruction is on the `dwf_node.input_json` side.
 */
export const INSTRUCTIONS_HEAD_MAX_CHARS = 240;

/** Character limit (64) for `lastTool.name` on `node-progress`: tool name, not description. */
export const LAST_TOOL_NAME_MAX_CHARS = 64;

/**
* Character limit (120) for `lastTool.target` on `node-progress`: a clue for humans (file path /
* Command header), not the input parameter itself - the input parameter can hold anything, and this event will be read by the main agent and drawn by the GUI.
 */
export const LAST_TOOL_TARGET_MAX_CHARS = 120;

/**
* The last observed tool call (`lastTool` of {@link AskProgress}).
* `target` is a **clue** for people to see, not an input parameter: file tools give the path, Bash gives the command header, if it is not recognized, it will be absent.
 */
export interface AskLastTool {
/** Tool name, ≤ {@link LAST_TOOL_NAME_MAX_CHARS}. */
  name: string;
/** Short target, ≤ {@link LAST_TOOL_TARGET_MAX_CHARS}; absent when it cannot be determined (placement string is not synthesized). */
  target?: string;
}

/**
* The progress of an ask at the moment when **turn is parsed**.
* and {@link AskStats} are two halves of the same driver report: stats is the token, and progress is "are you still moving?"
 */
export interface AskProgress {
/** The number of parsed turns in this ask, starting from 1 (nudge turns count; repair turns do not count - they do not end the turn). */
  turn: number;
/** The accumulated number of tool calls in this ask is the same counter as {@link AskStats.toolCalls}. */
  toolCalls: number;
  lastTool?: AskLastTool;
}
