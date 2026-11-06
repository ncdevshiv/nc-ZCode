import type { SessionId, SessionInfo } from "@zcode/contracts";
import type { BackgroundBashOutputResult } from "@zcode/shared";
import type { ZCodeProtocolAgentServerContext } from "./server-types.js";

/** Only query existing executors, never restore the runtime for viewing output. */
export async function readBackgroundBashOutputFromOwner(
  context: ZCodeProtocolAgentServerContext,
  sessionId: string,
  workId: string,
): Promise<BackgroundBashOutputResult> {
  const visited = new Set<string>();
  let current: string | undefined = sessionId;
  while (current && !visited.has(current)) {
    visited.add(current);
    const live = context.sessions.get(current);
    if (live) {
// The child record of cold recovery may use a new adapter, and the old task is still in the ancestor; survival cannot be equated with holding the task.
//Always pass the original sessionId to verify ownership, continue only when the task does not exist, and return as is if the read fails or the capability is missing.
      const result = await live.app.readBackgroundBashOutput(workId, sessionId);
      if (result.kind !== "unavailable") return result;
    }
    const stored: SessionInfo | null | undefined = await context.deps.sessionStore?.getSession(
      current as SessionId,
    );
    current = stored?.parentID ? String(stored.parentID) : undefined;
  }
  return { kind: "unavailable", workId };
}
