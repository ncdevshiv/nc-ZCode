import type { ToolPart } from "@zcode/contracts";

export function selectToolPartsForHistory(parts: ToolPart[]): ToolPart[] {
// If the early execution fails or the stream is cut off, another part will be created; the same statement will only restore the last newly created record, and the late update of the old part will not affect the selection.
// Preserve the physical order of the selected records, the original parts are still available for UI and file read state recovery.
  const latestByCallId = new Map(parts.map((part) => [part.callID, part]));
  const latestParts = parts.filter((part) => latestByCallId.get(part.callID) === part);
  if (latestParts.some((part) => part.declarationIndex === undefined)) {
// Old records or mixed versions may lack the declaration sequence number, and the entire group retains the original sequence.
    return latestParts;
  }
//Read-only tools may be released first; provider calls/results should share the same declaration order without changing the original parts of the UI.
  return latestParts.sort((left, right) => left.declarationIndex! - right.declarationIndex!);
}
