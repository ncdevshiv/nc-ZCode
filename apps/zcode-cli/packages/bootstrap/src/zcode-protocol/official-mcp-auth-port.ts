/* Official MCP identity header port on the Agent side.
Agent process is not an identity authority: reverse request to host via interaction/requestOfficialMcpAuthHeaders,
The host parses the current Coding Plan credentials and returns the identity header of this request. The credentials do not fall into runtime config,
No persistence, no logging. */
import {
  zcodeOfficialMcpAuthHeadersResponseSchema,
  zcodeProtocolMethods,
  type ZCodeWorkspaceRef,
} from "@zcode/shared";
import type { OfficialMcpAuthHeadersPort } from "@zcode/contracts";
import type { ZCodeProtocolAgentServerContext } from "./server-types.js";

let requestSequence = 0;

/** The port only needs the ability to send reverse requests, not the entire server context. */
export type OfficialMcpAuthRequestContext = Pick<
  ZCodeProtocolAgentServerContext,
  "requestClient"
>;

/**
* Construct the port for obtaining the identity header through protocol reverse request.
 *
* `resolveContext` is lazy: the MCP connection pool was constructed earlier than ZCodeProtocolAgentServer,
* Returns undefined before the server is ready. At this time, it will be treated as unavailable (it will not be downgraded to an anonymous request).
 *
* In case of failure, the return value (ok:false + enumerable reason) will be returned, and no exception will be thrown:
* Transport layer exceptions are uniformly classified as official_auth_unavailable, and the MCP adapter sets the server to failed.
 *
*Usage and known gaps of workspace:
* - The host side does not use it to route responses - responses are returned to the requester via `client.respond(request.id, ...)`
* For that stdio connection, routing is determined by the connection itself. This field is only for request context/auditing purposes;
* - but must still adhere to the warehouse convention `workspaceKey = workspaceIdentity?.trim() || workspacePath`,
* Otherwise, remote workspaces with the same path and different identities cannot be distinguished in the audit context;
* - **Remaining Gap**: The CLI agent process currently has no workspaceIdentity source
* (`RunZCodeProtocolAgentOptions` only has cwd/env/... and no corresponding environment variables), so this field is actually
* Degenerates into workspacePath. What is guaranteed here is "correct and transparent transmission once the identity is obtained", not "identity must exist".
* If the real remote identity is required for future audits, the identity needs to be passed to the agent at the spawn or protocol layer, which is not within the scope of this stage.
 */
export function createOfficialMcpAuthHeadersPort(input: {
  resolveContext: () => OfficialMcpAuthRequestContext | undefined;
  resolveWorkspace: (input: {
    workspaceIdentity?: string;
    workspacePath?: string;
  }) => ZCodeWorkspaceRef | undefined;
}): OfficialMcpAuthHeadersPort {
  return {
    async resolveHeaders(request) {
      const context = input.resolveContext();
      const workspace = input.resolveWorkspace({
        ...(request.workspaceIdentity ? { workspaceIdentity: request.workspaceIdentity } : {}),
        ...(request.workspacePath ? { workspacePath: request.workspacePath } : {}),
      });
      if (!context || !workspace) {
        return { ok: false, reason: "official_auth_unavailable" };
      }
      requestSequence += 1;
      try {
        return await context.requestClient(
          zcodeProtocolMethods.interactionRequestOfficialMcpAuthHeaders,
          {
            mcpKey: request.mcpKey,
            pluginId: request.pluginId,
            requestId: `official-mcp-auth:${requestSequence}`,
            targetOrigin: request.targetOrigin,
            workspace,
          },
          zcodeOfficialMcpAuthHeadersResponseSchema,
          request.signal ? { signal: request.signal } : {},
        );
      } catch {
// Host unreachable/protocol error: handled as unavailable. Error details are not brought out (may include request context),
// The host side has recorded classified logs without secrets.
        return { ok: false, reason: "official_auth_unavailable" };
      }
    },
  };
}
