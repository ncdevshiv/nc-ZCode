//Session search common parts (handler reuse for each command group).
import type { V4CommandCoreHost, V4SessionRecordView } from "./types.js";

export class V4SessionNotFoundError extends Error {
  constructor(sessionId: string) {
    super(`v4 command session not found: ${sessionId}`);
    this.name = "V4SessionNotFoundError";
  }
}

export function requireRecord(
  host: V4CommandCoreHost,
  sessionId: string | null,
): V4SessionRecordView {
  if (sessionId === null) {
    throw new V4SessionNotFoundError("(null)");
  }
  const record = host.getRecord(sessionId);
  if (!record) {
    throw new V4SessionNotFoundError(sessionId);
  }
  return record;
}
