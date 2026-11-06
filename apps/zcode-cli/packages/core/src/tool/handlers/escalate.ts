// ============================================================
// escalate Tool Handler
// ============================================================
//The workflow actor (child AgentRuntime) uses it to upgrade a **true blocking** to the main agent that created this workflow and park it
// Wait for the answer in your ask. The handler hands the problem to the injected WorkflowEscalatePort and blocks waiting for the outcome:
// - answered → Returns the answer text given by the main agent; the actor's turn continues on the spot, and the ask settles as usual.
// - refused → Return to the copy written by the port (the budget has been exhausted/nothing to ask).
//
// Both are **normal tool results**, not ToolHandlerFailure. This is separate from submit_result's reject:
// The error tool_result there is the "repair channel" (the model should be retried), and there is nothing repairable here - put
// "Upgrade budget exhausted" rendering as an error will only cause the model to hit the same wall repeatedly, and opportunistic bypassing is exactly the behavior this feature aims to eliminate.
//
// If the port is absent, ConfigurationError will still be thrown (according to submit_result): this tool only injects the port into the actor session
//Register, getting here is a wiring failure, not an ending.

import {
  CoreErrorType,
  ESCALATE_TOOL_NAME,
  EscalateInputJsonSchema,
  EscalateInputSchema,
  EscalateOutputJsonSchema,
  EscalateOutputSchema,
  createCoreError,
  type EscalateInput,
  type EscalateOutput,
  type TraceContext,
} from "@zcode/contracts";
import type { ToolEntry, ToolHandler } from "../types.js";

/** The answer may be an entire description; the upper limit of the model area in the same file as submit_result. */
const MAX_ESCALATE_MODEL_BYTES = 16_000;

/**
* Tool description = **usage discipline** on the actor side, which is more important than the mechanics of the tool: the mechanics only allow actors to ask questions,
* Discipline makes it possible to ask questions when it is time to ask. Four constraints on copywriting: last resort (not curiosity),
* Focus on one problem at a time, blocking may take a long time, per-ask upper limit 3.
 */
const ESCALATE_DESCRIPTION = [
  "Escalates a question that is BLOCKING you to the main agent that created this workflow, and waits here for the answer.",
  "",
  "This is a LAST RESORT, for when you are genuinely stuck on something outside your reach:",
  "- a gate that is broken or impossible to pass (a check capped at 95 when the threshold is 96);",
  "- instructions that contradict each other, so no output can satisfy both;",
  "- a fact you cannot obtain (a tradeoff, an external convention, what 'good' means here) that only whoever started this run knows.",
  "",
  "Do NOT use it for curiosity, progress reports, asking permission, confirming a conclusion you could verify yourself, or thinking out loud. None of those are blocked — keep working.",
  "",
  "Ask ONE focused question that can be answered in a sentence, and put your evidence in `context`: what you already tried, and exactly where you are stuck. The quality of the answer depends on it.",
  "",
  "The cost: this call BLOCKS until the main agent answers, which may take a long time. You get at most 3 escalations per ask; the 4th tells you the budget is spent and to proceed on your own best judgement. Do not spend them on questions not worth waiting for.",
].join("\n");

const escalateHandler: ToolHandler = async (input, context) => {
  const parsed = EscalateInputSchema.parse(input) as EscalateInput;

// Gate is the same as submit_result: based on the existence of the port, it has nothing to do with runtimeScope / taskType.
  if (!context.workflowEscalatePort) {
    throw createCoreError(
      CoreErrorType.ConfigurationError,
      "Workflow escalate port is not configured for escalate",
      {
        context: {
          toolCallId: context.toolCallId,
          toolName: ESCALATE_TOOL_NAME,
        },
        recoverable: false,
      },
    );
  }

  const outcome = await context.workflowEscalatePort.escalate({
    toolCallId: context.toolCallId,
    question: parsed.question,
    ...(parsed.context === undefined ? {} : { context: parsed.context }),
    trace: resolveToolTraceContext(context),
  });

  if (outcome.kind === "answered") {
    return {
      status: "answered",
      message: outcome.answer,
      qid: outcome.qid,
    } satisfies EscalateOutput;
  }

// The rejected copy is written by the port (states the current situation and next step), and is transparently transmitted here as it is - the identification key and copy are maintained separately.
// Sooner or later the two places will say different things, and the reader here is the model.
  return {
    status: "refused",
    message: outcome.message,
    reason: outcome.reason,
  } satisfies EscalateOutput;
};

export const escalateToolEntry: ToolEntry = {
  capability: "Escalate a blocking question from a workflow subagent to the main agent and wait",
  metadata: {
    name: ESCALATE_TOOL_NAME,
    description: ESCALATE_DESCRIPTION,
    readOnly: false,
    destructive: false,
// Deliberately different from submit_result: upgrade is not a final tool. It does not end the turn, nor does it exclude sibling tools——
// What is parked is this call, not the entire actor.
    concurrentSafe: true,
    maxOutputBytes: MAX_ESCALATE_MODEL_BYTES,
    sideEffectScope: "session",
    riskLevel: "low",
    needsApproval: false,
  },
  handler: escalateHandler,
  formatModelContent: formatEscalateModelContent,
  inputSchema: EscalateInputJsonSchema,
  outputSchema: EscalateOutputJsonSchema,
  runtimeInputSchema: EscalateInputSchema,
  runtimeOutputSchema: EscalateOutputSchema,
  permission: {
    permission: "workflow.escalate",
    reason: "escalate asks the main agent a blocking question from inside a workflow run",
    riskLevel: "low",
    sideEffectScope: "session",
    needsApproval: false,
    patternSources: ["toolName"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
  },
  resultBudget: {
    maxInlineBytes: MAX_ESCALATE_MODEL_BYTES,
    maxModelBytes: MAX_ESCALATE_MODEL_BYTES,
    strategy: "truncate",
    preview: {
      maxBytes: MAX_ESCALATE_MODEL_BYTES,
      direction: "head",
    },
  },
// The wait may be arbitrarily long (no timeout by design - "judge after timeout" just reintroduces speculative bypass). The escape pod is
// Existing cancellation: The driver's cancelAsk will be rejected along with the pending upgrade deferred.
  timeout: {
    kind: "none",
  },
  cancellation: {
    supported: true,
    cleanup: "none",
    userVisibleMessage: "escalate was cancelled before the main agent answered",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};

/** The model only reads one piece of text: the answer itself, or the rejection copy. The discriminant position does not enter the model surface. */
function formatEscalateModelContent(output: unknown): string {
  const parsed = EscalateOutputSchema.safeParse(output);
  if (!parsed.success) return "escalate returned an invalid result.";
  return parsed.data.message;
}

function resolveToolTraceContext(context: Parameters<ToolHandler>[1]): TraceContext {
  return (
    context.traceContext ?? {
      traceId: context.traceId,
      spanId: context.spanId,
      parentSpanId: context.parentSpanId,
      sessionId: context.sessionId,
      turnId: context.turnId,
    }
  );
}
