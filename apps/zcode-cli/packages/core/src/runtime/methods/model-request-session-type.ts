import { ModelRequestSessionType, ModelRetryBudget, type SessionTaskType } from "@zcode/contracts";

/**
* Model requests are roughly classified according to host session; workflow child is not a subagent, and server-side statistics are unified into other.
 */
export function resolveModelRequestSessionTypeFromTaskType(
  taskType: SessionTaskType | undefined,
): ModelRequestSessionType {
  if (taskType === "subagent_child") return ModelRequestSessionType.Subagent;
  if (taskType === "workflow_child" || taskType === "nested_workflow_child") {
    return ModelRequestSessionType.Other;
  }
  return ModelRequestSessionType.Main;
}

/**
* Model requests for workflow actors (actors with nested workflows) take an **unlimited** retry budget: model errors are never
* workflow errors, best effort recovery by the runner.
* The remaining sessions (main session, subagent, fork...) follow the default budget of the adapter.
 */
export function resolveModelRetryBudgetFromTaskType(
  taskType: SessionTaskType | undefined,
): ModelRetryBudget {
  if (taskType === "workflow_child" || taskType === "nested_workflow_child") {
    return ModelRetryBudget.Unbounded;
  }
  return ModelRetryBudget.Default;
}
