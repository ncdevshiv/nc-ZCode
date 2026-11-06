import { createPartId } from "../deps.js";
import type { MessageId, TraceContext } from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";
import type { RuntimeModelStreamSnapshot } from "../types.js";
import { hasAssistantReasoningContent } from "./turn-output-token-continuation.js";

export async function persistCancelledStreamSnapshot(
  runtime: AgentRuntimeInternal,
  options: {
    assistantCreatedAt: number;
    assistantMessageId: MessageId;
    snapshot: RuntimeModelStreamSnapshot;
    traceContext: TraceContext;
  },
): Promise<void> {
// When the user stops, the model request will exit with an exception, and the final text/reasoning in the success path
// Persistence will not be executed; here only flush has reached text/reasoning of this process, and the tool is still waiting for final path processing.
  const completedAt = Date.now();
  for (const reasoning of options.snapshot.reasoning) {
    if (!hasAssistantReasoningContent(reasoning)) continue;
    await runtime.persistPart(
      {
        id: createPartId(),
        sessionID: runtime.sessionId,
        messageID: options.assistantMessageId,
        type: "reasoning",
        text: reasoning.text,
        metadata: reasoning.providerOptions,
        time: {
          start: options.assistantCreatedAt,
          end: completedAt,
        },
      },
      options.traceContext,
    );
  }
  if (!options.snapshot.text) {
    return;
  }
  await runtime.persistPart(
    {
      id: createPartId(),
      sessionID: runtime.sessionId,
      messageID: options.assistantMessageId,
      type: "text",
      text: options.snapshot.text,
      time: {
        start: options.assistantCreatedAt,
        end: completedAt,
      },
    },
    options.traceContext,
  );
}
