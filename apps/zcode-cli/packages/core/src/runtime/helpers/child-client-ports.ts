// ============================================================
// The "external interaction" port derivation of the child runtime (the only exit)
// ============================================================
//
// The child runtime has two session identity axes:
// Ledger identity sessionId - event persistence / transcript / trace / session store, use your own;
// Routing identity - all agent → app reverse request (permission, AskUserQuestion, provider
// runtime headers), the child must use the parent's, up to the root session.
// The client only knows the root session; when asking about the sub-session, the session cannot be found on the desktop side, the response is never sent, and the sub-agent hangs.
//
// In the past, this rule was scattered in various child assembly points: core's subagent included two layers of private wrappers, dwf actor and legacy
// workflow child directly transparently transmits the port of appOptions - so two things are wrong and one is right. Here the derivation is converged into one place, and is given by
// **Parent runtime** calls (`AgentRuntime.createChildClientPorts`), `parentSessionId` is filled in by the parent itself,
// The caller cannot give the wrong value.

import type { PermissionBrokerPort, SessionId } from "../deps.js";
import type { ProviderRuntimeHeadersPort } from "../types.js";
import type { SubagentInteractionOriginContext } from "../../subagent/interaction-origin.js";
import { createSubagentInteractionBroker } from "./subagent-interaction-broker.js";

/** A runtime set of ports for protocol clients. */
export interface ClientFacingPorts {
  permissionBroker?: PermissionBrokerPort;
  providerRuntimeHeadersPort?: ProviderRuntimeHeadersPort;
}

/**
* Cast the ownership information required for a child. `parentSessionId` is not here - it can only be provided by the parent runtime,
* This is the mechanical guarantee of "good routing identity selection".
 */
export type ChildClientPortsContext = Omit<SubagentInteractionOriginContext, "parentSessionId">;

/**
* The child's external port is derived from the parent's external port.
 *
* - `providerRuntimeHeadersPort`: The package layer rewrites the `sessionId` of the input parameter into the parent session (the main runtime reports its own
* Session, see methods/model-runtime-headers.ts). When there are multiple levels of nesting, **write after the outer layer** (the layer closer to the client ends last).
* execution), the final value must be the root session.
* - `permissionBroker`: includes a layer of rewriting `request.sessionId`, the same is written after the outer layer; `origin` retains the innermost existing value,
* Subagent ownership is not erased by the outer layer.
 */
export function deriveChildClientPorts(
  parent: ClientFacingPorts,
  context: ChildClientPortsContext & { parentSessionId: SessionId },
): ClientFacingPorts {
  return {
    ...(parent.permissionBroker === undefined
      ? {}
      : { permissionBroker: createSubagentInteractionBroker(parent.permissionBroker, context) }),
    ...(parent.providerRuntimeHeadersPort === undefined
      ? {}
      : {
          providerRuntimeHeadersPort: rerouteProviderRuntimeHeadersPort(
            parent.providerRuntimeHeadersPort,
            context.parentSessionId,
          ),
        }),
  };
}

/**
* Provider runtime headers are independent reverse protocol requests without sub-event mirroring; the sessionId of the sub-session is just CLI
* For the internal ledger, the desktop only subscribes to the session of the parent task, so the parent session route must be used when refreshing the account credentials header. Service level meeting
* Identify whether the request model is consistent with the current model of the parent session. If they are inconsistent, only this header will be synchronized, and the parent session will not be switched to the child model.
 */
function rerouteProviderRuntimeHeadersPort(
  parentPort: ProviderRuntimeHeadersPort,
  parentSessionId: SessionId,
): ProviderRuntimeHeadersPort {
  return {
    shouldRefreshBeforeModelRequest(input) {
      return parentPort.shouldRefreshBeforeModelRequest?.(input) ?? true;
    },
    refreshBeforeModelRequest(input) {
      return parentPort.refreshBeforeModelRequest({ ...input, sessionId: parentSessionId });
    },
  };
}
