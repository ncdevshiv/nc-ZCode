import { randomUUID } from "node:crypto";
import type { BrowserControlPort, TraceContext } from "@zcode/contracts";
import {
  zcodeBrowserExecuteResultSchema,
  zcodeBrowserListResultSchema,
  zcodeProtocolMethods,
} from "@zcode/shared";
import {
  protocolTraceFromTraceContext,
  requireSession,
  type ZCodeProtocolAgentServerContext,
  type ZCodeProtocolClientRequestOptions,
} from "./server-types.js";

/**
* ProtocolBrowserControlBroker - agent side BrowserControlPort implementation.
 *
* browser-client's agent.browsers.* Each call here turns a BrowserCommand into
* ZCode Protocol's interaction/browserExecute reverse request, by app (host→main WebContentsView/CDP)
* Execute and return results. Injected in parallel with permission broker (createWorkspaceZCodeApp options of server-operations).
 */
export function createProtocolBrowserControlBroker(
  context: ZCodeProtocolAgentServerContext,
): BrowserControlPort {
  const connectionsBySession = new Map<
    string,
    Map<string, { browserId: string; browserGeneration: number }>
  >();

  const rememberConnection = (sessionId: string, browserId: string, browserGeneration: number) => {
    const connections = connectionsBySession.get(sessionId) ?? new Map();
    connections.set(`${browserId}\u0000${browserGeneration}`, { browserId, browserGeneration });
    connectionsBySession.set(sessionId, connections);
  };

  const sendLifecycle = async (
    sessionId: string,
    turnId: string | undefined,
    command: { method: "turnEnded"; turnId?: string } | { method: "closeSession" },
  ): Promise<void> => {
    const connections = [...(connectionsBySession.get(sessionId)?.values() ?? [])];
    await Promise.allSettled(
      connections.map(({ browserId, browserGeneration }) =>
        context.requestClient(
          zcodeProtocolMethods.interactionBrowserExecute,
          {
            ...buildBrowserRequestContext(context, { sessionId, turnId }),
            browserId,
            browserGeneration,
            command,
          },
          zcodeBrowserExecuteResultSchema,
        ),
      ),
    );
  };

  return {
    async list({ sessionId, turnId, traceContext, signal }) {
      const result = await context.requestClient(
        zcodeProtocolMethods.interactionBrowserList,
        buildBrowserRequestContext(context, { sessionId, turnId, traceContext }),
        zcodeBrowserListResultSchema,
        buildRequestOptions(traceContext, signal),
      );
      return result.browsers;
    },

    async execute({
      browserId,
      browserGeneration,
      sessionId,
      turnId,
      command,
      traceContext,
      signal,
    }) {
      rememberConnection(sessionId, browserId, browserGeneration);
      const requestContext = buildBrowserRequestContext(context, {
        sessionId,
        turnId,
        traceContext,
      });
      const cancelBackendRequest = () => {
// Only canceling the agent-side requestClient will allow the CDP action of host/main to continue executing.
// Here, the same backend/generation is used to send the internal cancelRequest, and main interrupts the waiter according to the original requestId;
// When the issued action cannot be proven to have no side effects, the manager returns the uncertain flag.
        void context
          .requestClient(
            zcodeProtocolMethods.interactionBrowserExecute,
            {
              ...buildBrowserRequestContext(context, { sessionId, turnId, traceContext }),
              browserId,
              browserGeneration,
              command: { method: "cancelRequest", requestId: requestContext.requestId },
            },
            zcodeBrowserExecuteResultSchema,
            buildRequestOptions(traceContext, undefined),
          )
          .catch(() => undefined);
      };
      if (signal?.aborted) cancelBackendRequest();
      else signal?.addEventListener("abort", cancelBackendRequest, { once: true });
      try {
        return await context.requestClient(
          zcodeProtocolMethods.interactionBrowserExecute,
          {
            ...requestContext,
            browserId,
            browserGeneration,
            command,
          },
          zcodeBrowserExecuteResultSchema,
          buildRequestOptions(traceContext, signal),
        );
      } finally {
        signal?.removeEventListener("abort", cancelBackendRequest);
      }
    },

    async turnEnded({ sessionId, turnId }) {
      await sendLifecycle(sessionId, turnId, { method: "turnEnded", turnId });
    },

    async closeSession({ sessionId, turnId }) {
      await sendLifecycle(sessionId, turnId, { method: "closeSession" });
      connectionsBySession.delete(sessionId);
    },
  };
}

function buildBrowserRequestContext(
  context: ZCodeProtocolAgentServerContext,
  input: {
    sessionId: string;
    turnId?: string;
    traceContext?: TraceContext;
  },
) {
  const record = requireSession(context, input.sessionId);
  const workspaceIdentity = record.workspace.workspaceIdentity?.trim() || undefined;
  const remoteSessionId = record.workspace.remoteSessionId?.trim() || undefined;
  const workspacePath = record.workspace.workspacePath;

  return {
    requestId: randomUUID(),
    sessionId: input.sessionId,
    ...((input.turnId ?? input.traceContext?.turnId)
      ? { turnId: String(input.turnId ?? input.traceContext?.turnId) }
      : {}),
// workspacePath may be the same in different remote workspaces, and the isolation key must be used first
// workspaceIdentity, avoid browser backend/tab ownership cross-workspace thread.
    workspaceKey: workspaceIdentity ?? workspacePath,
    workspacePath,
    ...(workspaceIdentity ? { workspaceIdentity } : {}),
    ...(remoteSessionId ? { remoteSessionId } : {}),
    clientMode: record.deliveryKind ?? "desktop-continuous",
    sessionContext: "live" as const,
  };
}

function buildRequestOptions(
  traceContext: TraceContext | undefined,
  signal: AbortSignal | undefined,
): ZCodeProtocolClientRequestOptions {
  return {
    ...(signal ? { signal } : {}),
    ...(traceContext ? { trace: protocolTraceFromTraceContext(traceContext) } : {}),
  };
}
