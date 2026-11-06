import { SessionEventType, type SessionEvent } from "@zcode/contracts";
import type { ZCodeComputerUseOperationEvent } from "@zcode/shared";

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

/**
* Whether this cell is using Computer Use.
 *
* Only do Boolean judgment, do not extract action names from the source code: the source code form will evolve with the node_repl SDK (such as changing to `getApp()` +
* Binding object + `computer.*`), sooner or later string matching will not hit any of them; the test fixture is also easy to feed into the SDK and never
* Flat shape of the output, making CI false green.
 *
* The anchor point is `setupComputerUseRuntime`, because it is a bootstrapping statement that the model **must copy** as it is, and this is architecture mandatory
* Instead of document soft requirements: each cell of ZCode's node_repl is a new Worker, and the SDK binding does not span cells, so
* Reference document states "The first executable statement of every CUA cell must be this bootstrap,
* and the bootstrap and the actions must be in the same cell". Any cell that uses CUA must contain it.
 *
* Browser Use's `agent.browsers.*` does not contain this guide and will not be hit.
 */
function usesComputerUse(input: unknown): boolean {
  const code = nonEmptyString(asRecord(input).code);
  if (!code) return false;
  return code.includes("setupComputerUseRuntime");
}

function baseEvent(event: SessionEvent) {
  return {
    eventId: String(event.id),
    sequenceNumber: event.sequenceNumber,
    sessionId: String(event.sessionId),
    timestamp: event.timestamp.getTime(),
  };
}

export function mapComputerUseOperationEvent(
  event: SessionEvent,
): ZCodeComputerUseOperationEvent | undefined {
  const turnId = event.turnId ? String(event.turnId) : undefined;
  const payload = asRecord(event.payload);
  switch (event.type) {
    case SessionEventType.TurnStarted:
      return turnId ? { ...baseEvent(event), kind: "turn-started", turnId } : undefined;
    case SessionEventType.TurnComplete:
      return turnId ? { ...baseEvent(event), kind: "turn-completed", turnId } : undefined;
    case SessionEventType.TurnError:
      return turnId ? { ...baseEvent(event), kind: "turn-failed", turnId } : undefined;
    case SessionEventType.ToolCallScheduled: {
      const toolCallId = nonEmptyString(payload.toolCallId);
      const toolName = nonEmptyString(payload.toolName);
      return turnId && toolCallId && toolName
        ? {
            ...baseEvent(event),
            kind: "tool-scheduled",
            turnId,
            toolCallId,
            toolName,
            ...(toolName === "mcp__node_repl__js" && usesComputerUse(payload.input)
              ? { computerUse: true as const }
              : {}),
          }
        : undefined;
    }
    case SessionEventType.ToolCallStarted: {
      const toolCallId = nonEmptyString(payload.toolCallId);
      if (!toolCallId) return undefined;
      const toolName = nonEmptyString(payload.toolName);
      return {
        ...baseEvent(event),
        kind: "tool-started",
        ...(turnId ? { turnId } : {}),
        toolCallId,
        ...(toolName ? { toolName } : {}),
      };
    }
    case SessionEventType.SessionEnded:
      return { ...baseEvent(event), kind: "session-closed" };
    default:
      return undefined;
  }
}
