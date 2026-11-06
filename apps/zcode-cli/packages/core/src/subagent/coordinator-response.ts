import { randomUUID } from "node:crypto";
import type {
  CoordinatorResponsePort,
  CoordinatorResponseResult,
  SessionId,
} from "@zcode/contracts";
import type { EnqueueSubagentMessageInput } from "../runtime/types.js";

interface CreateCoordinatorResponsePortOptions {
  agentId: string;
  agentType: string;
  childSessionId: SessionId;
  parentToolCallId?: string;
  createResponseId?: () => string;
// void will accept async callback; undefined can allow the type system to maintain synchronous enqueueing ack.
  enqueue(input: EnqueueSubagentMessageInput): undefined;
}

export function createCoordinatorResponsePort(
  options: CreateCoordinatorResponsePortOptions,
): CoordinatorResponsePort {
  return {
    respond(request): CoordinatorResponseResult {
      const responseId = options.createResponseId?.() ?? `response_${randomUUID()}`;
      try {
        options.enqueue({
          responseId,
          agentId: options.agentId,
          agentType: options.agentType,
          childSessionId: options.childSessionId,
          childToolCallId: String(request.childToolCallId),
          ...(options.parentToolCallId ? { parentToolCallId: options.parentToolCallId } : {}),
          summary: request.summary,
          message: request.message,
          traceContext: request.trace,
        });
        return {
          status: "success",
          responseId,
          message: "Response was queued for the coordinator.",
        };
      } catch (error) {
        return {
          status: "failed",
          responseId,
          message: "Response could not be queued for the coordinator.",
          error: error instanceof Error ? error.message : String(error),
        };
      }
    },
  };
}
