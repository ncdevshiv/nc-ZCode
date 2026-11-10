/** Task board + dependency graph + selected task inspector. */
import { useMemo } from "react";
import { cn } from "@/components/lib/utils.js";
import { taskTone } from "../format.js";
import { DagLegend, NcagDagView, type DagNode } from "../NcagDagView.js";
import { NcagBadge, NcagEmpty, NcagSurface } from "../primitives.js";
import type { NcagTask } from "../types.js";

export function TasksTab({
  tasks,
  selectedTaskId,
  selectedTask,
  pendingApproval,
  onSelectTask,
  onCancel,
  onReassign,
  onApprovePlan,
}: {
  tasks: NcagTask[];
  selectedTaskId: string | null;
  selectedTask: NcagTask | null;
  pendingApproval: string | null;
  onSelectTask: (id: string | null) => void;
  onCancel: (id: string) => Promise<boolean>;
  onReassign: (id: string) => Promise<boolean>;
  onApprovePlan: (requestId: string, approved: boolean) => Promise<boolean>;
}) {
  const dagNodes: DagNode[] = useMemo(
    () =>
      tasks.map((task) => ({
        id: task.id,
        title: task.title,
        status: task.status,
        role: task.role,
        priority: task.priority,
        criticalPathWeight: tasks.filter((candidate) => candidate.dependsOn.includes(task.id))
          .length,
        dependsOn: task.dependsOn,
        agentId: task.agentId,
      })),
    [tasks],
  );

  return (
    <div className="grid min-h-0 grid-cols-1 gap-3 xl:grid-cols-2">
      <NcagSurface
        title="Task board"
        subtitle={`${tasks.length} task(s)`}
        actions={
          pendingApproval ? (
            <div className="flex items-center gap-1">
              <NcagBadge tone="warning" label="plan awaiting approval" />
              <button
                type="button"
                className="h-6 rounded-md bg-primary px-2 text-ui-xs text-primary-foreground"
                onClick={() => void onApprovePlan(pendingApproval, true)}
              >
                Approve
              </button>
              <button
                type="button"
                className="h-6 rounded-md border border-border px-2 text-ui-xs"
                onClick={() => void onApprovePlan(pendingApproval, false)}
              >
                Reject
              </button>
            </div>
          ) : null
        }
      >
        {tasks.length === 0 ? (
          <NcagEmpty message="No tasks yet." hint="Submit a request to create the plan." />
        ) : (
          <ul className="flex flex-col gap-1">
            {tasks.map((task) => (
              <li key={task.id}>
                <button
                  type="button"
                  onClick={() => onSelectTask(task.id)}
                  className={cn(
                    "flex w-full items-center gap-2 rounded-md border px-2 py-1.5 text-left",
                    selectedTaskId === task.id
                      ? "border-border-hover bg-surface-hover"
                      : "border-border bg-surface hover:bg-surface-hover",
                  )}
                >
                  <NcagBadge tone={taskTone(task.status)} label={task.status} />
                  <span className="min-w-0 flex-1 truncate text-ui-sm">{task.title}</span>
                  <span className="shrink-0 text-ui-2xs text-foreground-subtle">
                    {task.role} · p{task.priority} · try {task.attempts}/{task.maxRetries}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </NcagSurface>

      <div className="flex min-h-0 flex-col gap-3">
        <NcagSurface title="Dependency graph" subtitle="readiness is computed from this DAG">
          <NcagDagView nodes={dagNodes} onSelect={onSelectTask} />
          <DagLegend />
        </NcagSurface>
        <NcagSurface title="Selected task" subtitle={selectedTask?.title ?? "select a task"}>
          {selectedTask ? (
            <div className="flex flex-col gap-2">
              <p className="text-ui-xs text-foreground-subtle">{selectedTask.description}</p>
              <div className="flex flex-wrap gap-1">
                <NcagBadge tone={taskTone(selectedTask.status)} label={selectedTask.status} />
                <NcagBadge tone="neutral" label={`role ${selectedTask.role}`} />
                <NcagBadge tone="neutral" label={`capability ${selectedTask.capability}`} />
                {selectedTask.agentId ? (
                  <NcagBadge tone="info" label={`agent ${selectedTask.agentId.slice(0, 12)}`} />
                ) : null}
              </div>
              {selectedTask.blocker ? (
                <p className="rounded-md border border-warning/40 bg-warning/10 px-2 py-1 text-ui-xs text-warning">
                  {selectedTask.blocker}
                </p>
              ) : null}
              <div>
                <p className="text-ui-2xs uppercase text-foreground-subtlest">
                  Acceptance criteria
                </p>
                <ul className="mt-1 flex flex-col gap-1">
                  {selectedTask.acceptance.map((check) => (
                    <li key={check.id} className="text-ui-xs text-foreground-subtle">
                      <span className="text-foreground">{check.description}</span>
                      {check.command ? (
                        <span className="text-foreground-subtlest"> · {check.command}</span>
                      ) : null}
                    </li>
                  ))}
                </ul>
              </div>
              {selectedTask.result ? (
                <pre className="max-h-40 overflow-auto rounded-md border border-border bg-input p-2 text-ui-2xs text-foreground-subtle">
                  {JSON.stringify(selectedTask.result, null, 2)}
                </pre>
              ) : null}
              <div className="flex gap-2">
                <button
                  type="button"
                  className="h-7 rounded-md border border-destructive/40 px-2 text-ui-xs text-destructive"
                  onClick={() => void onCancel(selectedTask.id)}
                >
                  Cancel task
                </button>
                <button
                  type="button"
                  className="h-7 rounded-md border border-border px-2 text-ui-xs"
                  onClick={() => void onReassign(selectedTask.id)}
                >
                  Reassign
                </button>
              </div>
            </div>
          ) : (
            <NcagEmpty message="Select a task to inspect its criteria, blocker and result." />
          )}
        </NcagSurface>
      </div>
    </div>
  );
}
