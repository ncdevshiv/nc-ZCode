/** Overview: organization counters, project creation, request intake, project list. */
import { useState } from "react";
import { formatCost, formatTokens } from "../format.js";
import { NcagBadge, NcagEmpty, NcagMetric, NcagSurface } from "../primitives.js";
import type { useNcagData } from "../useNcagData.js";

export function OverviewTab({
  data,
  projectName,
  projectStatus,
  onCreateProject,
  onSubmitRequest,
  canSubmit,
  onToggleProject,
}: {
  data: ReturnType<typeof useNcagData>["data"];
  projectName: string;
  projectStatus: string | null;
  onCreateProject: (name: string) => Promise<boolean>;
  onSubmitRequest: (instruction: string) => Promise<boolean>;
  canSubmit: boolean;
  onToggleProject: () => Promise<boolean>;
}) {
  const [projectNameInput, setProjectNameInput] = useState("");
  const [instruction, setInstruction] = useState("");
  const counts = data.dashboard?.projects.reduce(
    (accumulator, item) => ({
      running: accumulator.running + item.counts.running,
      idle: accumulator.idle + item.counts.idle,
      blocked: accumulator.blocked + item.counts.blocked,
      failed: accumulator.failed + item.counts.failed,
      active: accumulator.active + item.counts.tasksActive,
      completed: accumulator.completed + item.counts.tasksCompleted,
      reviews: accumulator.reviews + item.counts.pendingReviews,
      cost: accumulator.cost + item.usage.costUsd,
      calls: accumulator.calls + item.usage.calls,
      tokens: accumulator.tokens + item.usage.inputTokens + item.usage.outputTokens,
    }),
    {
      running: 0,
      idle: 0,
      blocked: 0,
      failed: 0,
      active: 0,
      completed: 0,
      reviews: 0,
      cost: 0,
      calls: 0,
      tokens: 0,
    },
  ) ?? {
    running: 0,
    idle: 0,
    blocked: 0,
    failed: 0,
    active: 0,
    completed: 0,
    reviews: 0,
    cost: 0,
    calls: 0,
    tokens: 0,
  };

  return (
    <div className="grid min-h-0 grid-cols-1 gap-3 xl:grid-cols-3">
      <NcagSurface title="Organization" subtitle={projectName} className="xl:col-span-2">
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
          <NcagMetric label="Running agents" value={counts.running} />
          <NcagMetric label="Idle agents" value={counts.idle} />
          <NcagMetric
            label="Blocked"
            value={counts.blocked}
            tone={counts.blocked > 0 ? "warning" : undefined}
          />
          <NcagMetric
            label="Failed"
            value={counts.failed}
            tone={counts.failed > 0 ? "danger" : undefined}
          />
          <NcagMetric label="Active tasks" value={counts.active} />
          <NcagMetric label="Completed tasks" value={counts.completed} />
          <NcagMetric label="Pending reviews" value={counts.reviews} />
          <NcagMetric
            label="Open decisions"
            value={data.dashboard?.openDecisions ?? 0}
            tone={(data.dashboard?.openDecisions ?? 0) > 0 ? "warning" : undefined}
          />
          <NcagMetric label="Model calls" value={counts.calls} />
          <NcagMetric label="Spent" value={formatCost(counts.cost)} hint="from usage rows" />
          <NcagMetric label="Tokens" value={formatTokens(counts.tokens)} />
          <NcagMetric label="In flight" value={data.dashboard?.inFlight ?? 0} />
        </div>

        <div className="mt-3 grid gap-2">
          <div className="flex flex-wrap items-end gap-2">
            <label className="flex min-w-48 flex-1 flex-col gap-1">
              <span className="text-ui-2xs text-foreground-subtle">New project name</span>
              <input
                className="h-7 rounded-md border border-input-border bg-input px-2 text-ui-sm"
                value={projectNameInput}
                placeholder="e.g. Payments hardening"
                onChange={(event) => setProjectNameInput(event.target.value)}
              />
            </label>
            <button
              type="button"
              className="h-7 rounded-md bg-primary px-3 text-ui-sm text-primary-foreground disabled:opacity-50"
              disabled={projectNameInput.trim().length === 0}
              onClick={() => {
                void onCreateProject(projectNameInput.trim()).then((ok) => {
                  if (ok) setProjectNameInput("");
                });
              }}
            >
              Create project
            </button>
          </div>

          <label className="flex flex-col gap-1">
            <span className="text-ui-2xs text-foreground-subtle">
              New request — analysed, decomposed and scheduled immediately
            </span>
            <textarea
              className="min-h-20 rounded-md border border-input-border bg-input p-2 text-ui-sm"
              value={instruction}
              placeholder="Describe the outcome you want. NC-AG detects ambiguities, plans the DAG and challenges contradictions."
              onChange={(event) => setInstruction(event.target.value)}
            />
          </label>
          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              className="h-7 rounded-md bg-primary px-3 text-ui-sm text-primary-foreground disabled:opacity-50"
              disabled={!canSubmit || instruction.trim().length === 0}
              onClick={() => {
                void onSubmitRequest(instruction.trim()).then((ok) => {
                  if (ok) setInstruction("");
                });
              }}
            >
              Submit request
            </button>
            <button
              type="button"
              className="h-7 rounded-md border border-border px-3 text-ui-sm hover:bg-surface-hover disabled:opacity-50"
              disabled={projectStatus === null}
              onClick={() => void onToggleProject()}
            >
              {projectStatus === "active" ? "Pause project" : "Resume project"}
            </button>
            {!canSubmit ? (
              <span className="text-ui-xs text-foreground-subtle">Create or select a project first.</span>
            ) : null}
          </div>
        </div>
      </NcagSurface>

      <NcagSurface title="Projects" subtitle={`${data.projects.length} project(s)`}>
        {data.projects.length === 0 ? (
          <NcagEmpty
            message="No projects yet."
            hint="A project binds a git repository (task worktrees come from it) to its plan, agents and budget."
          />
        ) : (
          <ul className="flex flex-col gap-2">
            {data.projects.map((project) => {
              const summary = data.dashboard?.projects.find(
                (item) => item.project.id === project.id,
              );
              return (
                <li key={project.id} className="rounded-md border border-border bg-surface px-2 py-2">
                  <div className="flex items-center justify-between gap-2">
                    <p className="truncate text-ui-sm font-medium">{project.name}</p>
                    <NcagBadge
                      tone={
                        project.status === "active"
                          ? "success"
                          : project.status === "paused"
                            ? "warning"
                            : "neutral"
                      }
                      label={project.status}
                    />
                  </div>
                  <p className="truncate text-ui-2xs text-foreground-subtle">
                    {project.workspacePath}
                  </p>
                  <p className="text-ui-2xs text-foreground-subtlest">
                    branch {project.baseBranch} · {summary?.counts.tasksActive ?? 0} active ·{" "}
                    {formatCost(summary?.usage.costUsd ?? 0)} spent · budget{" "}
                    {formatCost(project.config.projectBudgetUsd)}
                  </p>
                </li>
              );
            })}
          </ul>
        )}
      </NcagSurface>
    </div>
  );
}
