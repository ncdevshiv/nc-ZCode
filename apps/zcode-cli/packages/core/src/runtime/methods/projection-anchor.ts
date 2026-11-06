import type {
  MessageAnchorOrigin,
  MessageProjectionAnchor,
  SyntheticUserMessageSource,
  TraceContext,
} from "../deps.js";

//── v4 transcript anchor (list)──
// additive JSON: old data has no anchor, and the reading side is tolerant of downgrades; the value of turnId is ready-made (traceContext).
// sourceCommandId is written with the command execution context after v4 command inbox wiring.
export function buildProjectionAnchor(
  traceContext: TraceContext,
  origin?: MessageAnchorOrigin,
  sourceCommandId?: string,
): MessageProjectionAnchor | undefined {
  if (traceContext.turnId === undefined && origin === undefined && sourceCommandId === undefined) {
    return undefined;
  }
  return {
    ...(traceContext.turnId ? { turnId: traceContext.turnId } : {}),
    ...(origin ? { origin } : {}),
    ...(sourceCommandId ? { sourceCommandId } : {}),
  };
}

// Read-only mapping of old SyntheticUserMessageSource → v4 userInput.origin vocabulary.
export function mapSyntheticSourceToAnchorOrigin(
  source: SyntheticUserMessageSource,
): MessageAnchorOrigin {
  switch (source) {
    case "background_task":
    case "subagent":
      return "backgroundResult";
    case "goal-continuation":
      return "goalContinuation";
    default:
      return "synthetic";
  }
}
