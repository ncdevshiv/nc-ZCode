import type { IZCodeAgentService } from "@zcode/services";
import { useWorkspaceServices } from "@/hooks/useWorkspaceServices.js";

export function useZCodeAgentService(
  workspacePath?: string,
  preferredRemoteSessionId?: string | null,
  workspaceIdentity?: string | null,
): IZCodeAgentService {
  // Keep the hook order stable when the initial workspace path arrives after first render.
  const services = useWorkspaceServices(workspacePath, preferredRemoteSessionId, workspaceIdentity);
  return services.zcodeAgentService;
}
