import type { SessionTaskType } from "@zcode/contracts";

/**
* Session type projection of the task list on the left.
 *
* List visibility cannot be replaced by hierarchical query for `parent_id is null`: explicit fork
* This type of main task with parent will be filtered by cold start seeds after CLI restart. Visibility must be determined by taskType,
* The auxiliary dialogue, subagent and workflow child continue to be hosted by their own dedicated projections.
 */
export const TASK_LIST_SESSION_TYPES = [
  "interactive",
  "fork",
  "workflow_parent",
] as const satisfies readonly SessionTaskType[];

const TASK_LIST_SESSION_TYPE_SET = new Set<SessionTaskType>(
  TASK_LIST_SESSION_TYPES,
);

export function isTaskListSessionType(
  taskType: SessionTaskType | undefined,
): boolean {
  return TASK_LIST_SESSION_TYPE_SET.has(taskType ?? "interactive");
}
