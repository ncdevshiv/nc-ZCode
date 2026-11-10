/**
 * NC-AG operational dashboard (`/nc-ag`): the page shell.
 *
 * It owns project selection, the active tab and the one data hook; each tab is a real
 * panel over the API read-model (`tabs/*`). Every control calls the API through
 * `useNcagData().run`, and the server is the single source of truth.
 */
import { useMemo, useState } from "react";
import type { ChangeEvent } from "react";
import { cn } from "@/components/lib/utils.js";
import { ncagApi } from "./client.js";
import { NcagBadge, NcagErrorBanner } from "./primitives.js";
import { useNcagData } from "./useNcagData.js";
import { OverviewTab } from "./tabs/OverviewTab.js";
import { TasksTab } from "./tabs/TasksTab.js";
import { AgentsTab } from "./tabs/AgentsTab.js";
import { CommsTab } from "./tabs/CommsTab.js";
import { ResourcesTab } from "./tabs/ResourcesTab.js";
import { DecisionsTab } from "./tabs/DecisionsTab.js";
import { TimelineTab } from "./tabs/TimelineTab.js";
import { DeliveryTab } from "./tabs/DeliveryTab.js";

type Tab =
  | "overview"
  | "tasks"
  | "agents"
  | "comms"
  | "resources"
  | "decisions"
  | "timeline"
  | "delivery";

const TABS: { id: Tab; label: string }[] = [
  { id: "overview", label: "Overview" },
  { id: "tasks", label: "Tasks & DAG" },
  { id: "agents", label: "Agents" },
  { id: "comms", label: "Communication" },
  { id: "resources", label: "Leases & Integration" },
  { id: "decisions", label: "Decisions" },
  { id: "timeline", label: "Timeline" },
  { id: "delivery", label: "Delivery" },
];

export function NcagApp() {
  const [projectId, setProjectId] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>("overview");
  const [selectedTaskId, setSelectedTaskId] = useState<string | null>(null);
  const { data, loading, error, setError, streamState, refresh, run, selectedProject } =
    useNcagData(projectId);

  const activeProjectId = projectId ?? selectedProject?.id ?? data.projects.at(-1)?.id ?? null;
  const openDecisions = useMemo(
    () => data.decisions.filter((decision) => decision.status === "open"),
    [data.decisions],
  );
  const selectedTask = data.tasks.find((task) => task.id === selectedTaskId) ?? null;
  const pendingApproval =
    data.requests.find((request) => request.status === "awaiting_approval")?.id ?? null;

  return (
    <div className="flex h-full min-h-0 flex-col bg-background text-foreground">
      <header className="flex flex-wrap items-center gap-3 border-b border-border bg-header px-4 py-3">
        <div className="min-w-0">
          <h1 className="text-ui-lg font-semibold">NC-AG Orchestrator</h1>
          <p className="text-ui-xs text-foreground-subtle">
            Autonomous multi-agent execution · every row comes from the running service
          </p>
        </div>
        <div className="ml-auto flex flex-wrap items-center gap-2">
          <NcagBadge
            tone={streamState === "live" ? "success" : streamState === "down" ? "danger" : "warning"}
            label={streamState === "live" ? "live" : streamState === "down" ? "stream down" : "connecting"}
          />
          <NcagBadge
            tone={data.health?.auditChainVerified ? "success" : "danger"}
            label={data.health?.auditChainVerified ? "audit chain ok" : "audit chain broken"}
          />
          <NcagBadge
            tone={data.health?.provider.configured ? "info" : "warning"}
            label={data.health?.provider.configured ? "provider store wired" : "no provider store"}
          />
          <select
            aria-label="Project"
            className="h-7 rounded-md border border-input-border bg-input px-2 text-ui-sm"
            value={activeProjectId ?? ""}
            onChange={(event: ChangeEvent<HTMLSelectElement>) => {
              setProjectId(event.target.value || null);
              setSelectedTaskId(null);
            }}
          >
            <option value="">All projects</option>
            {data.projects.map((project) => (
              <option key={project.id} value={project.id}>
                {project.name}
              </option>
            ))}
          </select>
          <button
            type="button"
            className="h-7 rounded-md border border-border px-2 text-ui-sm hover:bg-surface-hover"
            onClick={() => void refresh()}
            disabled={loading}
          >
            {loading ? "Loading…" : "Refresh"}
          </button>
        </div>
      </header>

      <div className="flex min-h-0 flex-1 flex-col gap-3 p-3">
        {error ? <NcagErrorBanner message={error} onDismiss={() => setError(null)} /> : null}
        <nav className="flex flex-wrap gap-1" role="tablist" aria-label="NC-AG views">
          {TABS.map((item) => (
            <button
              key={item.id}
              type="button"
              role="tab"
              aria-selected={tab === item.id}
              className={cn(
                "h-7 rounded-md px-3 text-ui-sm",
                tab === item.id
                  ? "bg-surface-hover text-foreground"
                  : "text-foreground-subtle hover:bg-surface",
              )}
              onClick={() => setTab(item.id)}
            >
              {item.label}
              {item.id === "decisions" && openDecisions.length > 0
                ? ` (${openDecisions.length})`
                : ""}
            </button>
          ))}
        </nav>

        <div className="min-h-0 flex-1">
          {tab === "overview" ? (
            <OverviewTab
              data={data}
              projectName={selectedProject?.name ?? "all projects"}
              projectStatus={selectedProject?.status ?? null}
              onCreateProject={(name) => run(() => ncagApi.createProject({ name }))}
              onSubmitRequest={(instruction) =>
                activeProjectId
                  ? run(() => ncagApi.submitRequest(activeProjectId, instruction))
                  : Promise.resolve(false)
              }
              canSubmit={Boolean(activeProjectId)}
              onToggleProject={() =>
                activeProjectId && selectedProject
                  ? run(() =>
                      selectedProject.status === "active"
                        ? ncagApi.pauseProject(activeProjectId)
                        : ncagApi.resumeProject(activeProjectId),
                    )
                  : Promise.resolve(false)
              }
            />
          ) : null}
          {tab === "tasks" ? (
            <TasksTab
              tasks={data.tasks}
              selectedTaskId={selectedTaskId}
              selectedTask={selectedTask}
              pendingApproval={pendingApproval}
              onSelectTask={setSelectedTaskId}
              onCancel={(taskId) => run(() => ncagApi.cancelTask(taskId))}
              onReassign={(taskId) =>
                run(() => ncagApi.reassignTask(taskId, "reassigned from the dashboard"))
              }
              onApprovePlan={(requestId, approved) =>
                run(() => ncagApi.approvePlan(requestId, approved))
              }
            />
          ) : null}
          {tab === "agents" ? (
            <AgentsTab
              agents={data.agents}
              tasks={data.tasks}
              onPause={(id) => run(() => ncagApi.pauseAgent(id))}
              onResume={(id) => run(() => ncagApi.resumeAgent(id))}
              onCancel={(id) => run(() => ncagApi.cancelAgent(id))}
              onInstruct={(id, instruction) => run(() => ncagApi.instructAgent(id, instruction))}
              onHandoff={(taskId, toAgent) =>
                run(() => ncagApi.handoffTask(taskId, toAgent, "handoff from the dashboard"))
              }
            />
          ) : null}
          {tab === "comms" ? (
            <CommsTab
              messages={data.messages}
              canSend={Boolean(activeProjectId)}
              onAck={(id) => run(() => ncagApi.ackMessage(id))}
              onSend={(subject, body, recipient) =>
                activeProjectId
                  ? run(() =>
                      ncagApi.sendMessage({
                        projectId: activeProjectId,
                        subject,
                        body,
                        recipient: recipient || undefined,
                        requiresAck: true,
                      }),
                    )
                  : Promise.resolve(false)
              }
            />
          ) : null}
          {tab === "resources" ? (
            <ResourcesTab
              leases={data.leases}
              integrations={data.integrations}
              onRelease={(id) => run(() => ncagApi.releaseLease(id))}
            />
          ) : null}
          {tab === "decisions" ? (
            <DecisionsTab
              decisions={data.decisions}
              onResolve={(decisionId, resolution, rationale) =>
                run(() => ncagApi.resolveDecision(decisionId, resolution, rationale, "user"))
              }
            />
          ) : null}
          {tab === "timeline" ? <TimelineTab events={data.events} /> : null}
          {tab === "delivery" ? (
            <DeliveryTab
              delivery={data.delivery}
              requests={data.requests}
              hasOpenDecisions={openDecisions.length > 0}
            />
          ) : null}
        </div>
      </div>
    </div>
  );
}