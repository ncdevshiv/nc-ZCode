import {
  COMPLETED_TOOL_PART_METADATA_SCHEMA_VERSION,
  type CompletedToolPartMetadata,
  type ToolExecutionResult,
} from "../deps.js";
import { createMcpToolDisplay } from "../../tool/executor/result-display.js";

export function mcpToolPartMetadata(
  presentation:
    | { serverName: string; toolName: string; description?: string }
    | undefined,
): CompletedToolPartMetadata | undefined {
  const display = createMcpToolDisplay(presentation);
  return display
    ? { schemaVersion: COMPLETED_TOOL_PART_METADATA_SCHEMA_VERSION, display }
    : undefined;
}

export function completedToolPartMetadata(
  result: ToolExecutionResult,
): CompletedToolPartMetadata {
  const serialization = result.serialization
    ? {
        truncated: result.serialization.truncated,
        originalBytes: result.serialization.originalBytes,
        returnedBytes: result.serialization.returnedBytes,
        budgetStrategy: result.serialization.budgetStrategy,
        ...(result.serialization.artifactPath
          ? { artifactPath: result.serialization.artifactPath }
          : {}),
      }
    : undefined;
  return {
    schemaVersion: COMPLETED_TOOL_PART_METADATA_SCHEMA_VERSION,
    ...(result.display ? { display: result.display } : {}),
    ...(serialization ? { serialization } : {}),
//resume needs to restore the file snapshot that the model actually read at that time; it only relies on tool_result text
// The main path will be tied to the provider display format, so the new session structured persistence read-state.
    ...(result.readFileStateMetadata ? { readFileState: result.readFileStateMetadata } : {}),
  };
}
