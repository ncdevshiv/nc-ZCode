import type {
  PermissionBrokerPort,
  PermissionBrokerRequest,
  PermissionBrokerRequestOptions,
  PermissionBrokerResult,
} from "../deps.js";
import {
  buildSubagentInteractionOrigin,
  type SubagentInteractionOriginContext,
} from "../../subagent/interaction-origin.js";

interface SubagentInteractionBrokerContext extends SubagentInteractionOriginContext {
  parentToolCallId?: PermissionBrokerRequest["toolCallId"] | string;
}

export function createSubagentInteractionBroker(
  parentBroker: PermissionBrokerPort,
  context: SubagentInteractionBrokerContext,
): PermissionBrokerPort {
  return {
    requestPermission(
      request: PermissionBrokerRequest,
      options?: PermissionBrokerRequestOptions,
    ): Promise<PermissionBrokerResult> {
// The child agent's permission / AskUserQuestion / ExitPlanMode all require the UI response of the parent task;
// The broker request is externally routed to the parent session, and the origin retains the child ownership, which facilitates UI and log identification of the source.
      //
// This package can be stacked. `sessionId` is determined by the **outer layer** (the layer closer to the client)
// Finally rewrite, so any depth will eventually fall to the root session; `origin` in turn retains the existing value of **inner layer**,
// The attribution is always the sub-agent that actually initiated the request, and will not be overwritten by the outer layer into an intermediate layer.
      return parentBroker.requestPermission(
        {
          ...request,
          sessionId: context.parentSessionId,
          origin: request.origin ?? buildSubagentInteractionOrigin(context, request.turnId),
        },
        options,
      );
    },
  };
}
