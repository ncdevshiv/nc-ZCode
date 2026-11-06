import {
  AMEND_WORKFLOW_TOOL_NAME,
  ASK_USER_QUESTION_TOOL_NAME,
  AskUserQuestionInputSchema,
  CREATE_WORKFLOW_TOOL_NAME,
  type PermissionBrokerResult,
} from "@zcode/contracts";
import type React from "react";
import type { ApprovalPrompt } from "./app-model.js";
import { createQuestionPromptState } from "./app-question-state.js";
import type { TuiRequestPermission } from "./types.js";

/**
* The confirmation gate of CreateWorkflow is automatically released on the CLI side.
 *
* This is a **documented CLI exception** (user ruled:
* The end user is already in the command line context, friction outweighs protection). Bypass is only in the client response layer: core's `alwaysAsk` semantics,
* The hook sequence, permission events, and desktop confirmation window are all unchanged.
 *
* **Never bring permissionUpdates**: That will persist an allow rule and turn "skip one confirmation" into a true authorization.
* Gate bypass is not equal to permission bypass - actors in the run still inherit the permission profile of the session.
 */
function createWorkflowBypassResult(toolName: string): PermissionBrokerResult {
  return {
    decision: "allow",
    reason: `${toolName} auto-allowed in CLI (confirmation gate bypass)`,
    resolvedAt: new Date(),
  };
}

export function createTuiPermissionRequester(input: {
  setApprovalQueue: React.Dispatch<React.SetStateAction<ApprovalPrompt[]>>;
  setStatus: (status: string) => void;
}): TuiRequestPermission {
  return (request, requestOptions) =>
    new Promise<PermissionBrokerResult>((resolve, reject) => {
      if (requestOptions?.signal?.aborted) {
        reject(new Error("Permission request cancelled"));
        return;
      }

// Approval bypass: short-circuit before building approval, so the approval panel will not be rendered at all (setApprovalQueue is not touched).
// AmendWorkflow and CreateWorkflow have the same door and the same exception.
      if (
        request.toolName === CREATE_WORKFLOW_TOOL_NAME ||
        request.toolName === AMEND_WORKFLOW_TOOL_NAME
      ) {
        resolve(createWorkflowBypassResult(request.toolName));
        return;
      }

      let settled = false;
      let approval: ApprovalPrompt;
      const parsedQuestion =
        request.toolName === ASK_USER_QUESTION_TOOL_NAME
          ? AskUserQuestionInputSchema.safeParse(request.input)
          : undefined;

      if (parsedQuestion && !parsedQuestion.success) {
        resolve({
          decision: "deny",
          reason: `Invalid AskUserQuestion input: ${
            parsedQuestion.error.issues[0]?.message ?? "schema validation failed"
          }`,
          resolvedAt: new Date(),
        });
        return;
      }

      const cleanup = () => {
        requestOptions?.signal?.removeEventListener("abort", abortHandler);
      };
      const settle = (result: PermissionBrokerResult) => {
        if (settled) return;
        settled = true;
        cleanup();
        resolve({
          ...result,
          resolvedAt: result.resolvedAt ?? new Date(),
        });
      };
      const fail = (error: Error) => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(error);
      };
      const abortHandler = () => {
        input.setApprovalQueue((current) => current.filter((item) => item !== approval));
        fail(new Error("Permission request cancelled"));
      };

      approval = {
        cleanup,
        questionState: parsedQuestion?.success
          ? createQuestionPromptState(parsedQuestion.data)
          : undefined,
        reject: fail,
        request,
        resolve: settle,
        selectedDecision: "deny",
      };

      requestOptions?.signal?.addEventListener("abort", abortHandler, { once: true });
      input.setApprovalQueue((current) => [...current, approval]);
      input.setStatus(
        approval.questionState
          ? "Answer the clarification question."
          : `Approval required for ${request.toolName}.`,
      );
    });
}
