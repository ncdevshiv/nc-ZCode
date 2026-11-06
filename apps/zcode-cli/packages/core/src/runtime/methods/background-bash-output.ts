import type { BackgroundBashOutputResult } from "@zcode/shared";
import type { AgentRuntimeInternal } from "../internal.js";

/** The execution record is verified based on the sessionId at startup, and the ancestor runtime cannot read the workId of other sessions. */
export async function readBackgroundBashOutput(
  this: AgentRuntimeInternal,
  workId: string,
  sessionId = this.sessionId as string,
): Promise<BackgroundBashOutputResult> {
  if (!this.executionPort?.readBackgroundBashOutput) return { kind: "unsupported", workId };
  return this.executionPort.readBackgroundBashOutput(workId, sessionId);
}
