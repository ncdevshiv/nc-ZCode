import {
  SessionEventType,
  type UserInputAutoResolutionUpdatedPayload,
  type TraceContext,
} from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";

/**
* Log AskUserQuestion auto-end phase. Events must go through the runtime's durable append/sink link.
* Make both desktop continuous and web remote replayable restore the same set of absolute times.
 */
export async function recordUserInputAutoResolutionUpdate(
  this: AgentRuntimeInternal,
  input: UserInputAutoResolutionUpdatedPayload & { traceContext?: TraceContext },
): Promise<void> {
  const traceContext = input.traceContext ?? this.rootTraceContext;
  await this.appendEvent(
    this.createEvent(
      SessionEventType.UserInputAutoResolutionUpdated,
      {
        interactionId: input.interactionId,
        toolCallId: input.toolCallId,
        autoResolution: input.autoResolution,
      } satisfies UserInputAutoResolutionUpdatedPayload,
      traceContext,
    ),
    traceContext,
  );
}
