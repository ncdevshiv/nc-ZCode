// ============================================================
// Workflow Escalate Port - actor upgrades the boundary of blocking issues to the main agent
// ============================================================
// Completely isomorphic and deliberately separate from {@link WorkflowSubmitPort}:
// Submit settles **the result of this ask** (engine decision), escalate settles **a question and answer** (the main agent answers),
// The two endpoints, life cycles and failure modes are different. Merging them into one port will only make the two timings unclear.

import type { TraceContext } from "../tracing/tracer.js";
import type { ToolCallId } from "./shared.js";

export interface EscalateQuestionRequest {
/** The tool call id within the actor subsession that initiated the `escalate` call. */
  toolCallId: ToolCallId | string;
/** The choke point itself: a focused question that can be answered in one sentence. */
  question: string;
/** Optional supplementary context (what the actor has tried and which line it is stuck on). */
  context?: string;
  trace: TraceContext;
}

/**
* An upgraded ending. **Both are normal tool results**, not errors: returned when budget is exhausted
* The normal result of "self-propulsion" instead of throwing an error (an error will cause the model to treat it as a retryable failure and hit the same wall repeatedly).
 *
* Use discriminants instead of plain text because the caller (core's tool processor) needs to decide what to render based on this
* tool_result (house rule: error code instead of error text for process judgment).
 */
export type WorkflowEscalateOutcome =
  | {
      kind: "answered";
/** The answer text given by the host agent through `ResolveWorkflowQuestion` becomes the tool result as it is. */
      answer: string;
/** The globally unique id of this Q&A (for log and human traceability; the model does not need to read it). */
      qid: string;
    }
  | {
      kind: "refused";
      /**
* `budget_exhausted`: The number of upgrades for this ask has been exhausted (per-ask upper limit, same family as nudge budget).
* `no_active_ask`: There is no active ask in this session at the moment, and the question is nowhere to be parked (should not happen, but never hang).
       */
      reason: "budget_exhausted" | "no_active_ask";
/** Model-oriented copywriting, stating the current situation and next steps. */
      message: string;
    };

export interface WorkflowEscalatePort {
  /**
* Escalate a blocking question and **block** waiting for the master agent to answer.
   *
* Same discipline as {@link WorkflowSubmitPort.respond}: resolve may take arbitrarily long (not set by design
* Timeout - "Use your own judgment after timeout" is exactly the opportunistic bypass that reintroduction of this feature is intended to eliminate). Escape pods are existing cancellations:
* The driver's `cancelAsk` will be rejected together with the parked upgrade deferred, so run cancel and the process die
* behaves exactly as it does today.
   *
* The routing identity (run/actor/session/instance) is bound by the port closure and cannot be overridden by the model.
   */
  escalate(request: EscalateQuestionRequest): Promise<WorkflowEscalateOutcome>;
}
