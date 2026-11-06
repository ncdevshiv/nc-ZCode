import { ModelErrorCode } from "@zcode/contracts";
import { AiSdkModelAdapterError } from "./errors.js";

export function normalizeModelToolName(
  value: unknown,
  context: Record<string, unknown>,
): string {
  if (typeof value === "string") {
    const toolName = value.trim();
    if (toolName) {
      return toolName;
    }

    const toolCallId = context.toolCallId;
    const hasClosableToolCallId =
      typeof toolCallId === "string" && toolCallId.trim().length > 0;
    if (context.providerExecuted !== true && hasClosableToolCallId) {
// Client-executed calls with empty names can still be closed with the original id. In Adapter
// If an error is thrown directly, the model will not receive the tool error with the same id, and the entire turn will stop.
      return value;
    }
  }

  throw new AiSdkModelAdapterError(
    ModelErrorCode.InvalidModelResponse,
    "Model returned an invalid tool call: tool name is empty.",
    { context },
  );
}
