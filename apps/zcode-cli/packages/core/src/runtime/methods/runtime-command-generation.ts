import { SessionEventType, traceContextToLogContext } from "../deps.js";
import type { SessionEvent } from "../deps.js";
import type { RuntimeCommand } from "../command-queue.js";
import type { AgentRuntimeInternal } from "../internal.js";

export function isStaleBranchRuntimeCommand(
  runtime: AgentRuntimeInternal,
  command: RuntimeCommand,
): boolean {
  if (
    command.mode !== "task-notification" &&
    command.mode !== "subagent-message" &&
    command.mode !== "control-only-turn"
  ) {
    return false;
  }
  if (command.branchGeneration === runtime.branchGeneration) return false;
// There is a race condition between rewind and background completion; even if the command is enqueued, it must be in the persistence and
// Verify generation again before provider injection, leaving only diagnostic logs for old branch results.
  runtime.logger?.debug("Dropped queued stale-branch runtime command", {
    ...traceContextToLogContext(command.traceContext),
    branchGeneration: command.branchGeneration,
    commandId: command.id,
    currentBranchGeneration: runtime.branchGeneration,
    event: "runtime.command.stale_branch_dropped",
    mode: command.mode,
    module: "core.runtime",
  });
  return true;
}

export function isStaleBranchRuntimeTaskEvent(
  runtime: AgentRuntimeInternal,
  event: SessionEvent,
): boolean {
  if (
    event.type !== SessionEventType.BackgroundTaskUpdated &&
    event.type !== SessionEventType.BackgroundTaskCompleted &&
    event.type !== SessionEventType.SubagentMessage &&
    event.type !== SessionEventType.SubagentStopped
  ) {
    return false;
  }
  const payload =
    event.payload && typeof event.payload === "object" && !Array.isArray(event.payload)
      ? (event.payload as Record<string, unknown>)
      : {};
  const taskId =
    typeof payload.taskId === "string"
      ? payload.taskId
      : typeof payload.agentId === "string"
        ? payload.agentId
        : undefined;
  if (!taskId) return false;
  const task = runtime.runtimeTaskRegistry.get(taskId);
  if (!task || task.branchGeneration === runtime.branchGeneration) return false;
  runtime.logger?.debug("Dropped stale-branch runtime task event", {
    branchGeneration: task.branchGeneration,
    currentBranchGeneration: runtime.branchGeneration,
    event: "runtime.task_event.stale_branch_dropped",
    eventType: event.type,
    module: "core.runtime",
    taskId,
  });
  return true;
}
