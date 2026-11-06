import {
  traceContextToLogContext,
  type PermissionOptionsPolicy,
  type ToolResultDisplayPayload,
  type TraceContext,
} from "@zcode/contracts";
import type { ExecutableToolCall, ToolEntry } from "../types.js";
import type { ToolExecutorDeps } from "./types.js";

interface ResolvedToolApproval {
  gate: "ask" | "proceed";
  display?: ToolResultDisplayPayload;
  optionsPolicy?: PermissionOptionsPolicy;
}

/**
* After the permission service has determined ask, call the tool's self-reported `prepareApproval`, and combine its reply with the tool's declared
* The option strategy is collapsed into "ask what to bring this time".
 *
* The direction is one-way narrowing: the hook can only release ask into proceed or add a preview to it, but can never change allow into ask.
* Tools that do not declare hooks will still pop up as usual.
 */
function resolveOptionsPolicy(
  allowAlways: false | "session" | undefined,
): PermissionOptionsPolicy | undefined {
  switch (allowAlways) {
    case false:
      return "no-always-allow";
    case "session":
      return "session-always-allow";
    default:
      return undefined;
  }
}

export function resolveToolApproval(
  deps: ToolExecutorDeps,
  toolCall: ExecutableToolCall,
  entry: ToolEntry,
  executionInput: unknown,
  traceContext: TraceContext,
): ResolvedToolApproval {
// `permission` is required in terms of type, but the executor will also be driven by an entry that declares only some fields.
// (Test stubs, dynamic registration tools). The surrounding code tolerates this by spreading rather than reading fields, and the same goes for gate.
  const optionsPolicy = resolveOptionsPolicy(entry.permission?.askOptions?.allowAlways);

  if (!entry.prepareApproval) {
    return { gate: "ask", ...(optionsPolicy ? { optionsPolicy } : {}) };
  }

  try {
// The working directory is obtained from the same source as the handler (getWorkingDirectory of deps has been set in impl.ts
// Static workingDirectory is included), otherwise the preview will look at one directory and the execution will write another.
    const gate = entry.prepareApproval(executionInput);
    if (gate.gate === "proceed") return { gate: "proceed" };
    return {
      gate: "ask",
      ...(gate.display ? { display: gate.display } : {}),
      ...(optionsPolicy ? { optionsPolicy } : {}),
    };
  } catch (error) {
// Bug prevention: The hook responsible for generating the preview must not determine "whether the user is asked". To the execution side fail-open is equal to
// An unapproved tool is silently run, so ask is still established here, and only the preview is downgraded.
    deps.logger?.warn("Tool approval preview failed; asking without a preview", {
      ...traceContextToLogContext(traceContext),
      error: error instanceof Error ? error.message : String(error),
      event: "tool.permission.approval_preview_failed",
      module: "core.tool.executor",
      status: "failed",
      toolCallId: toolCall.id,
      toolName: toolCall.name,
    });
    return { gate: "ask", ...(optionsPolicy ? { optionsPolicy } : {}) };
  }
}
