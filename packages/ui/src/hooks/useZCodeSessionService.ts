import type { IZCodeSessionService } from "@zcode/services";
import { useWorkspaceServices } from "@/hooks/useWorkspaceServices.js";

export function useZCodeSessionService(
  workspacePath?: string,
  preferredRemoteSessionId?: string | null,
  workspaceIdentity?: string | null,
): IZCodeSessionService {
  // Always call the same hooks. `workspacePath` flips from undefined to a value after the
  // initial workspace boot; conditionally calling `useWorkspaceServices` changed hook order
  // and surfaced as "Cannot read properties of undefined (reading 'length')" inside React.
  // `useWorkspaceServices` already falls back to the current/base services when no path is set.
  const services = useWorkspaceServices(workspacePath, preferredRemoteSessionId, workspaceIdentity);
  return services.zcodeSessionService;
}
