// ============================================================
// Tool call → a "target" clue for people to see
// ============================================================
// `lastTool.target` of `node-progress` answers "where is it moving": file tools give the path, Bash gives the command header,
// Search class for pattern. It is not an input parameter - the input parameter can hold an entire patch, a section of base64, and a file text, and this
// The event will be read into the context by the main agent and drawn on the card by the GUI, so only a bounded scalar key is taken, and the rest are treated as "unrecognized".
//
// Press **key name** instead of by tool name: the tool name will change, the name of the MCP tool is not in our hands at all, but `file_path` / `command`
// These key names are established conventions for the input parameters of this warehouse tool (read/edit/write/bash/glob/grep schema of contracts).
// If you don't recognize it, you will be absent - synthesizing a "(unknown)" placeholder string will only make the reader think that it is the real target.

import {
  LAST_TOOL_NAME_MAX_CHARS,
  LAST_TOOL_TARGET_MAX_CHARS,
  type AskLastTool,
} from "@zcode/dynamic-workflow";

/**
* Press the parameter input key of priority detection. The first **non-empty string** that hits is the target; the path family is tail-preserving (the file name is the resolution point),
* The rest of the headers are preserved (the beginning of the command, pattern, and url is the distinguishing point).
 */
const TARGET_KEYS: readonly { key: string; keep: "head" | "tail" }[] = [
  { key: "file_path", keep: "tail" },
  { key: "notebook_path", keep: "tail" },
  { key: "path", keep: "tail" },
  { key: "command", keep: "head" },
  { key: "pattern", keep: "head" },
  { key: "url", keep: "head" },
  { key: "query", keep: "head" },
];

/** Ellipsis mark (preceded when ending): readers should be able to tell that this clue has been truncated. */
const ELLIPSIS = "…";

/** Narrow view of a tool call: name + input parameters. Both may be absent (old events, empty name calls). */
interface ToolCallSummaryInput {
  toolName?: string;
  input?: unknown;
}

/**
* Flatten a tool call into `lastTool`. The absence of a name is a total absence - a "nearest object" without a name says nothing.
 */
export function summarizeToolCall(call: ToolCallSummaryInput): AskLastTool | undefined {
  const name = typeof call.toolName === "string" ? call.toolName.trim() : "";
  if (name.length === 0) return undefined;
  const target = deriveToolTarget(call.input);
  return {
    name: name.slice(0, LAST_TOOL_NAME_MAX_CHARS),
    ...(target === undefined ? {} : { target }),
  };
}

/** Get the target clue from the input parameter; absent when it is not an object, has no known key, or the key is not a non-empty string. */
function deriveToolTarget(input: unknown): string | undefined {
  if (typeof input !== "object" || input === null || Array.isArray(input)) return undefined;
  const record = input as Record<string, unknown>;
  for (const { key, keep } of TARGET_KEYS) {
    const value = record[key];
    if (typeof value !== "string") continue;
// For multi-line commands, only the first line is taken and continuous whitespace is suppressed: the command header is "what is running", and the entire heredoc is not.
    const flattened = value.split("\n", 1)[0]!.replace(/\s+/g, " ").trim();
    if (flattened.length === 0) continue;
    return bound(flattened, keep);
  }
  return undefined;
}

/** Cut to {@link LAST_TOOL_TARGET_MAX_CHARS}: cut directly after keeping the head, and cut after keeping the ellipsis in front of the tail. */
function bound(text: string, keep: "head" | "tail"): string {
  if (text.length <= LAST_TOOL_TARGET_MAX_CHARS) return text;
  if (keep === "head") return text.slice(0, LAST_TOOL_TARGET_MAX_CHARS);
  return ELLIPSIS + text.slice(text.length - (LAST_TOOL_TARGET_MAX_CHARS - ELLIPSIS.length));
}
