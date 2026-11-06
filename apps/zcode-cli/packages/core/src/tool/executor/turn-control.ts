import {
  isAutomationCreateLimitError,
  CoreErrorType,
  AMEND_WORKFLOW_TOOL_NAME,
  CREATE_WORKFLOW_TOOL_NAME,
  EXIT_PLAN_MODE_TOOL_NAME,
  type CollaborationMode,
} from "@zcode/contracts";
import type { ToolEntry, ToolExecutionResult } from "../types.js";

const DEFAULT_EXIT_PLAN_DENIED_MESSAGE = `Permission denied for ${EXIT_PLAN_MODE_TOOL_NAME}`;
const EXIT_PLAN_DENIED_BY_USER_MESSAGE = "The plan was not approved by the user.";
const WORKFLOW_REFINE_DENIED_BY_USER_MESSAGE = "The workflow run was not approved by the user.";
const AUTOMATION_CREATE_LIMIT_MODEL_MESSAGE =
  "Automation creation was not performed because the global retained-task limit of 20 was reached. " +
  "This limit cannot be recovered automatically in the current turn. Do not list, delete, overwrite, " +
  "retry, or use another tool. Reply once in the user's language that they must manually delete an " +
  "existing task on the Automations page and then retry.";

export function withAutomationCreateLimitTurnStop(
  result: ToolExecutionResult,
  input: { error: unknown; toolName: string },
): ToolExecutionResult {
  if (
    result.success ||
    input.toolName !== "CronCreate" ||
    !isAutomationCreateLimitError(input.error)
  ) {
    return result;
  }

// The creation upper limit is the product boundary that requires the user to manually release the quota, and is not an Agent recoverable error.
// Hide the original error with "Delete..." inducement from the model and request the executor to cancel the current step's subsequent tools.
  return {
    ...result,
    modelContent: AUTOMATION_CREATE_LIMIT_MODEL_MESSAGE,
    turnControl: {
      reason: "automation_create_limit",
      stopTurnAfterResult: true,
    },
  };
}

export function withPlanExitDeniedTurnStop(
  result: ToolExecutionResult,
  input: {
    mode: CollaborationMode;
    planEnabled?: boolean;
    toolName: string;
  },
): ToolExecutionResult {
  if (
    !(input.planEnabled ?? input.mode === "plan") ||
    input.toolName !== EXIT_PLAN_MODE_TOOL_NAME ||
    result.success
  ) {
    return result;
  }

  const feedback = readPlanExitDeniedFeedback(result);
  if (feedback) {
    return {
      ...result,
      followUpUserInput: {
        input: feedback,
        reasonSource: "plan_approval_feedback",
      },
// feedback will become a real user message through steer; tool_result can only express that the plan is rejected,
// We cannot promise that feedback will follow, otherwise the provider will see non-existent subsequent user messages when the steer is rejected.
      modelContent: EXIT_PLAN_DENIED_BY_USER_MESSAGE,
    };
  }

// Refusing to exit the plan means that the user wants to continue the discussion and cannot treat it as an ordinary tool error and continue to feed the model self-rewrite plan.
  return {
    ...result,
    turnControl: {
      reason: "plan_exit_denied",
      stopTurnAfterResult: true,
    },
  };
}

function readPlanExitDeniedFeedback(result: ToolExecutionResult): string | undefined {
  if (result.error?.type !== CoreErrorType.PermissionDenied) {
    return undefined;
  }
// project rule / hook / broker may also return deny + reason;
// Only the dedicated source on the ExitPlanMode approval custom input band can be interpreted as user modification opinions.
  if (result.error.reasonSource !== "plan_approval_feedback") {
    return undefined;
  }
  const message = result.error.message.trim();
  if (!message || message === DEFAULT_EXIT_PLAN_DENIED_MESSAGE) {
    return undefined;
  }
  return message;
}

/**
* Refine response for workflow execution confirmation window.
* Isomorphic to withPlanExitDeniedTurnStop but narrower: only feedback upgrade, no stop turn branch——
* Ordinary Deny follows the existing semantics (feed standard permission error, turn to continue), and the feedback becomes true through steer
* After the user message is received, the model will revise the script and resubmit it in the same turn. Resubmission will naturally trigger a new round of confirmation.
* Type without pressing mode: CreateWorkflow's ask ignores mode (alwaysAsk).
 */
export function withWorkflowRefineDeniedFollowUp(
  result: ToolExecutionResult,
  input: { toolName: string },
): ToolExecutionResult {
  if (
    (input.toolName !== CREATE_WORKFLOW_TOOL_NAME && input.toolName !== AMEND_WORKFLOW_TOOL_NAME) ||
    result.success
  ) {
    return result;
  }
  const feedback = readWorkflowRefineDeniedFeedback(result);
  if (!feedback) {
    return result;
  }
  return {
    ...result,
    followUpUserInput: {
      input: feedback,
      reasonSource: "workflow_refine_feedback",
    },
// tool_result can only express that the operation has not been approved, and cannot promise that feedback will be followed.
// Otherwise, when the steer is rejected, the provider will see subsequent user messages that do not exist.
    modelContent: WORKFLOW_REFINE_DENIED_BY_USER_MESSAGE,
  };
}

function readWorkflowRefineDeniedFeedback(result: ToolExecutionResult): string | undefined {
  if (result.error?.type !== CoreErrorType.PermissionDenied) {
    return undefined;
  }
//Same as readPlanExitDeniedFeedback: hook/project rule's deny + reason without this dedicated source.
// Must not be upgraded to user messages.
  if (result.error.reasonSource !== "workflow_refine_feedback") {
    return undefined;
  }
  const message = result.error.message.trim();
  return message || undefined;
}

// Final tools like submit_result declare stopTurnOnSuccess=true on their ToolEntry.metadata: their
// A successful result represents a final state submission, which must end the actor's turn. Read declarative metadata here instead of hard coding by tool name
// ——The final state is a general intrinsic ability (any future final state tool can be reused), and the failure side
// withPlanExitDeniedTurnStop / withAutomationCreateLimitTurnStop is typed according to tool specific error/state
// Conditional stops are different. The handlers of these tools are guaranteed to terminate successfully (gate throws an error, reject fails), so there is no need
// handler side signal channel.
export function withTerminalToolTurnStop(
  result: ToolExecutionResult,
  input: { entry: ToolEntry },
): ToolExecutionResult {
  if (!result.success || input.entry.metadata.stopTurnOnSuccess !== true) {
    return result;
  }
  return {
    ...result,
    turnControl: {
      reason: "subagent_terminal",
      stopTurnAfterResult: true,
    },
  };
}
