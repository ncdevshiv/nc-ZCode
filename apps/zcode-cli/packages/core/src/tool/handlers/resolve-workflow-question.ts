// ============================================================
// ResolveWorkflowQuestion Tool Handler
// ============================================================
// Used by the master agent to answer blocking questions when an actor escalates from the **running** workflow. See port `DynamicWorkflowRunPort.resolveQuestion`.
//
// The handler is deliberately **very thin**: qid table lookup, driver settlement, and event dual-tracking are all on the port server. Only three things are done here:
// Get the port (typeof detection, follow the resume/listRuns precedent), transparently transmit the qid and answer, and project the result into a contract shape.
//
// Reject copywriting **Transparent transmission as it is** Server-side message: That text is written by the layer that writes the registration form (stating the current situation and next step),
// If the discrimination key and copy are maintained separately, the two places will say different things sooner or later, and the reader here is the model - what it reads is its next step.
// Therefore, the three rejection branches of this file are only responsible for selecting a stable errorCode and do not rewrite a word.

import {
  RESOLVE_WORKFLOW_QUESTION_TOOL_NAME,
  ResolveWorkflowQuestionInputJsonSchema,
  ResolveWorkflowQuestionInputSchema,
  ResolveWorkflowQuestionOutputJsonSchema,
  ResolveWorkflowQuestionOutputSchema,
  type ModelMessageContent,
  type ResolveWorkflowQuestionInput,
  type ResolveWorkflowQuestionOutput,
} from "@zcode/contracts";
import type {
  ToolEntry,
  ToolExecutionContext,
  ToolHandler,
  ToolHandlerFailure,
} from "../types.js";

const RESOLVE_WORKFLOW_QUESTION_TIMEOUT_MS = 15_000;
/** The output is only one paragraph of confirmation copy, 24k is more than enough (according to ResumeWorkflowRun). */
const RESOLVE_WORKFLOW_QUESTION_MODEL_BYTES = 24_000;

/**
* Local failure code table. The value is just a log bit (the executor projects it into a `code: "N"` string); what the model actually reads is
* message, and the messages of the three rejection branches are written by the server. Deliberately compiled from 21, with the introspection table (1/2) and
* The ResumeWorkflowRun tables (11–15) are visually separated - the value spaces of the three tables are independent of each other, and cross-table comparisons are prohibited.
 */
const RESOLVE_WORKFLOW_QUESTION_ERROR_CODE = {
  ANSWERING_UNAVAILABLE: 21,
  UNKNOWN_QUESTION: 22,
  ALREADY_RESOLVED: 23,
  RUN_NOT_IN_FLIGHT: 24,
} as const;

const RESOLVE_WORKFLOW_QUESTION_DESCRIPTION = [
  "Answers a blocking question that a subagent escalated from inside a RUNNING dynamic-workflow run.",
  "",
  "- Takes question_id — the ID from the escalation notification (it looks like `dwfq-...`). If that notification was lost, GetWorkflowRun lists the questions a run still owes an answer to.",
  "- The run keeps running the whole time: only the subagent that asked is parked on its call, while every other subagent and the script's control flow keep going. Your answer becomes that call's result verbatim and the subagent continues from there.",
  "- Answer directly and actionably. If you are not sure, look at the run first with GetWorkflowRun, or ask the user with AskUserQuestion, then come back and answer — nothing answers on your behalf, and the subagent waits indefinitely.",
  "- If the question reveals the SCRIPT is structurally broken (a broken gate, wrong control flow), a sentence cannot fix that: cancel the run and continue with a revised script via CreateWorkflow's `resume_from`.",
].join("\n");

/**
* "This session is not responsive". Absence of port (journal is unavailable → run service is not constructed at all) and method absence
* (stub without resolveQuestion) returns the same failure: this is the same thing for the model (as detected by resume's typeof
*Precedent). Never succeed silently - that would make an actor wait forever while the model thinks it has answered.
 */
function resolveQuestionUnavailableFailure(): ToolHandlerFailure {
  return {
    result: false,
    errorCode: RESOLVE_WORKFLOW_QUESTION_ERROR_CODE.ANSWERING_UNAVAILABLE,
    message:
      "workflow_question_answering_unavailable: this session cannot answer workflow escalations — workflow execution is not available here. This is a capability gap, not a bad question ID.",
  };
}

/** The three reasons for the port → their respective stable error codes; the message comes from the server as it is. */
function resolveQuestionFailureFor(reason: string, message: string): ToolHandlerFailure {
  switch (reason) {
    case "unknown_question":
      return {
        result: false,
        errorCode: RESOLVE_WORKFLOW_QUESTION_ERROR_CODE.UNKNOWN_QUESTION,
        message,
      };
    case "already_resolved":
      return {
        result: false,
        errorCode: RESOLVE_WORKFLOW_QUESTION_ERROR_CODE.ALREADY_RESOLVED,
        message,
      };
    case "run_not_in_flight":
      return {
        result: false,
        errorCode: RESOLVE_WORKFLOW_QUESTION_ERROR_CODE.RUN_NOT_IN_FLIGHT,
        message,
      };
    default:
//Reason outside the port contract: still returns structural failure (throw is the channel of wiring failure), the copy contains the original word for troubleshooting.
      return {
        result: false,
        errorCode: RESOLVE_WORKFLOW_QUESTION_ERROR_CODE.UNKNOWN_QUESTION,
        message: `${message} (reason: ${reason})`,
      };
  }
}

const resolveWorkflowQuestionHandler: ToolHandler = async (
  input,
  context: ToolExecutionContext,
) => {
  const parsed = ResolveWorkflowQuestionInputSchema.parse(
    input,
  ) as ResolveWorkflowQuestionInput;

  const port = context.dynamicWorkflowRunPort;
  if (port === undefined || typeof port.resolveQuestion !== "function") {
    return resolveQuestionUnavailableFailure();
  }

  const result = await port.resolveQuestion(parsed.question_id, parsed.answer);
  if (!result.ok) {
    return resolveQuestionFailureFor(result.reason, result.message);
  }

  return {
    ok: true,
    qid: result.qid,
// Make two things clear: that the answer has been delivered, and that the run has not stopped - the master agent does not have to guard it.
    response: `Answer delivered for question ${result.qid}. The subagent that asked has resumed its turn with your answer; the run keeps going as before.`,
  } satisfies ResolveWorkflowQuestionOutput;
};

function formatResolveWorkflowQuestionModelContent(output: unknown): ModelMessageContent {
  const parsed = ResolveWorkflowQuestionOutputSchema.safeParse(output);
  if (!parsed.success) return "ResolveWorkflowQuestion returned an invalid result.";
  return parsed.data.response;
}

export const resolveWorkflowQuestionToolEntry: ToolEntry = {
  capability: "Answer a blocking question escalated by a subagent inside a running workflow run",
  metadata: {
    name: RESOLVE_WORKFLOW_QUESTION_TOOL_NAME,
    description: RESOLVE_WORKFLOW_QUESTION_DESCRIPTION,
// Answering will cause a parked actor to continue working with this text - not read-only.
    readOnly: false,
    destructive: false,
    concurrentSafe: true,
    timeoutMs: RESOLVE_WORKFLOW_QUESTION_TIMEOUT_MS,
    maxOutputBytes: RESOLVE_WORKFLOW_QUESTION_MODEL_BYTES,
    sideEffectScope: "none",
    riskLevel: "low",
// Confirmation-free: answering just sends a piece of text to an approved run (the run itself is in the confirmation window of CreateWorkflow
// Passed), the same risk profile as ResumeWorkflowRun. Pop-ups can also hang an actor waiting for an answer longer.
    needsApproval: false,
  },
  handler: resolveWorkflowQuestionHandler,
  inputSchema: ResolveWorkflowQuestionInputJsonSchema,
  outputSchema: ResolveWorkflowQuestionOutputJsonSchema,
  runtimeInputSchema: ResolveWorkflowQuestionInputSchema,
  runtimeOutputSchema: ResolveWorkflowQuestionOutputSchema,
  formatModelContent: formatResolveWorkflowQuestionModelContent,
  permission: {
    permission: "resolveWorkflowQuestion",
    reason: "resolveWorkflowQuestion answers a blocked subagent inside a running workflow run",
    riskLevel: "low",
    sideEffectScope: "none",
    needsApproval: false,
// question_id enters the pattern matching surface (according to the run_id of ResumeWorkflowRun).
    patternSources: ["toolName", "input"],
    alwaysAllowPatternSources: ["toolName"],
    denyPriority: "beforeAsk",
  },
  resultBudget: {
    maxInlineBytes: RESOLVE_WORKFLOW_QUESTION_MODEL_BYTES,
    maxModelBytes: RESOLVE_WORKFLOW_QUESTION_MODEL_BYTES,
    strategy: "truncate",
    preview: {
      maxBytes: RESOLVE_WORKFLOW_QUESTION_MODEL_BYTES,
      direction: "head",
    },
  },
  timeout: {
    kind: "timed",
    defaultMs: RESOLVE_WORKFLOW_QUESTION_TIMEOUT_MS,
    maxMs: RESOLVE_WORKFLOW_QUESTION_TIMEOUT_MS,
    allowCallOverride: false,
  },
  cancellation: {
    supported: false,
    cleanup: "none",
    userVisibleMessage:
      "ResolveWorkflowQuestion delivers the answer synchronously and cannot be cancelled",
  },
  trace: {
    required: true,
    propagateToAdapters: true,
    recordInput: "summary",
    recordOutput: "summary",
  },
};
