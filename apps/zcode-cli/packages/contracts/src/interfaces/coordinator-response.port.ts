import type { TraceContext } from "../tracing/tracer.js";
import type { ToolCallId } from "./shared.js";

export interface CoordinatorResponseRequest {
  childToolCallId: ToolCallId | string;
  summary: string;
  message: string;
  trace: TraceContext;
}

export interface CoordinatorResponseResult {
  status: "success" | "failed";
  responseId: string;
  message: string;
  error?: string;
}

export interface CoordinatorResponsePort {
// child session/agent/parent identity is bound by port closure, the model cannot override routing.
// Synchronous return ensures that the child tool result can be completed only after the response command is entered into the parent queue.
  respond(request: CoordinatorResponseRequest): CoordinatorResponseResult;
}
