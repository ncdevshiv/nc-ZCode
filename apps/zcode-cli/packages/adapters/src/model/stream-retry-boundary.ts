import type { ModelStreamEvent } from "@zcode/contracts";

// Note: these events are buffered, not yielded, until a committed stream boundary appears.
// If a network reset happens first, the partial tool-input attempt can be discarded.
// Non-empty reasoning_delta is the first output token perceived by the user and cannot wait for the text or tool boundary to be released.
const RETRY_SAFE_PRELUDE_STREAM_EVENT_TYPES = new Set<ModelStreamEvent["type"]>([
  "start",
  "text_start",
  "text_end",
  "reasoning_start",
  "reasoning_end",
  "tool_input_start",
  "tool_input_delta",
  "tool_input_end",
]);

export function isRetrySafePreludeStreamEvent(event: ModelStreamEvent): boolean {
// SDK converts pure signature to empty reasoning_delta; treating it as output will stop adapter retry,
// And core cannot be restored when there is no text or complete tool call. The empty delta is temporarily stored with the prelude, and will be released in the original order with the signature retained on success.
  if (event.type === "reasoning_delta" || event.type === "text_delta") {
    return event.text.length === 0;
  }
  return RETRY_SAFE_PRELUDE_STREAM_EVENT_TYPES.has(event.type);
}
