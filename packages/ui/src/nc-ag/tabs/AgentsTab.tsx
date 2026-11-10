/** Live agent list and per-agent monitor with real controls. */
import { useState } from "react";
import { cn } from "@/components/lib/utils.js";
import { agentTone, formatCost, formatRelative, formatTime, formatTokens } from "../format.js";
import { NcagBadge, NcagEmpty, NcagMetric, NcagSurface } from "../primitives.js";
import type { NcagAgent, NcagTask } from "../types.js";

export function AgentsTab({
  agents,
  tasks,
  onPause,
  onResume,
  onCancel,
  onInstruct,
  onHandoff,
}: {
  agents: NcagAgent[];
  tasks: NcagTask[];
  onPause: (id: string) => Promise<boolean>;
  onResume: (id: string) => Promise<boolean>;
  onCancel: (id: string) => Promise<boolean>;
  onInstruct: (id: string, instruction: string) => Promise<boolean>;
  onHandoff: (taskId: string, toAgent: string) => Promise<boolean>;
}) {
  const [selected, setSelected] = useState<string | null>(null);
  const [instruction, setInstruction] = useState("");
  const agent = agents.find((item) => item.id === selected) ?? null;

  return (
    <div className="grid min-h-0 grid-cols-1 gap-3 xl:grid-cols-2">
      <NcagSurface title="Live agents" subtitle={`${agents.length} instance(s)`}>
        {agents.length === 0 ? (
          <NcagEmpty
            message="No agents spawned yet."
            hint="Agents are created per task when the scheduler dispatches work."
          />
        ) : (
          <ul className="flex flex-col gap-1">
            {agents.map((item) => (
              <li key={item.id}>
                <button
                  type="button"
                  onClick={() => setSelected(item.id)}
                  className={cn(
                    "flex w-full items-center gap-2 rounded-md border px-2 py-1.5 text-left",
                    selected === item.id
                      ? "border-border-hover bg-surface-hover"
                      : "border-border bg-surface hover:bg-surface-hover",
                  )}
                >
                  <NcagBadge tone={agentTone(item.status)} label={item.status} />
                  <span className="min-w-0 flex-1 truncate text-ui-sm">{item.role}</span>
                  <span className="shrink-0 text-ui-2xs text-foreground-subtle">
                    {formatRelative(item.lastHeartbeat)} ·{" "}
                    {formatTokens(item.usageInput + item.usageOutput)}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </NcagSurface>

      <NcagSurface title="Agent monitor" subtitle={agent?.id ?? "select an agent"}>
        {agent ? (
          <div className="flex flex-col gap-2">
            <div className="grid grid-cols-2 gap-2">
              <NcagMetric label="Role" value={agent.role} />
              <NcagMetric label="Model" value={agent.model ?? "unresolved"} />
              <NcagMetric label="Task" value={agent.taskId?.slice(0, 16) ?? "—"} />
              <NcagMetric label="Heartbeat" value={formatTime(agent.lastHeartbeat)} />
              <NcagMetric
                label="Tokens"
                value={formatTokens(agent.usageInput + agent.usageOutput)}
              />
              <NcagMetric label="Cost" value={formatCost(agent.costUsd)} />
            </div>
            <p className="text-ui-2xs text-foreground-subtle">persona: {agent.persona}</p>
            <p className="text-ui-2xs text-foreground-subtle">
              permissions: {agent.permissions.join(", ") || "none"}
            </p>
            <p className="text-ui-2xs text-foreground-subtle">
              worktree: {agent.worktree ?? "not assigned"}
            </p>
            {agent.error ? (
              <p className="rounded-md border border-destructive/40 bg-destructive/10 px-2 py-1 text-ui-xs text-destructive">
                {agent.error}
              </p>
            ) : null}
            <label className="flex flex-col gap-1">
              <span className="text-ui-2xs text-foreground-subtle">Send an instruction</span>
              <textarea
                className="min-h-16 rounded-md border border-input-border bg-input p-2 text-ui-sm"
                value={instruction}
                onChange={(event) => setInstruction(event.target.value)}
              />
            </label>
            <div className="flex flex-wrap gap-2">
              <button
                type="button"
                className="h-7 rounded-md border border-border px-2 text-ui-xs"
                onClick={() => void onPause(agent.id)}
              >
                Pause
              </button>
              <button
                type="button"
                className="h-7 rounded-md border border-border px-2 text-ui-xs"
                onClick={() => void onResume(agent.id)}
              >
                Resume
              </button>
              <button
                type="button"
                className="h-7 rounded-md border border-destructive/40 px-2 text-ui-xs text-destructive"
                onClick={() => void onCancel(agent.id)}
              >
                Cancel
              </button>
              <button
                type="button"
                className="h-7 rounded-md bg-primary px-2 text-ui-xs text-primary-foreground disabled:opacity-50"
                disabled={instruction.trim().length === 0}
                onClick={() => {
                  void onInstruct(agent.id, instruction.trim()).then((ok) => {
                    if (ok) setInstruction("");
                  });
                }}
              >
                Send
              </button>
            </div>
            {agent.taskId ? (
              <label className="flex flex-col gap-1">
                <span className="text-ui-2xs text-foreground-subtle">Hand the task off to</span>
                <select
                  className="h-7 rounded-md border border-input-border bg-input px-2 text-ui-sm"
                  defaultValue=""
                  onChange={(event) => {
                    if (event.target.value) {
                      void onHandoff(agent.taskId as string, event.target.value);
                    }
                  }}
                >
                  <option value="">choose an idle agent…</option>
                  {agents
                    .filter(
                      (candidate) => candidate.id !== agent.id && candidate.taskId === null,
                    )
                    .map((candidate) => (
                      <option key={candidate.id} value={candidate.id}>
                        {candidate.role} · {candidate.id.slice(0, 12)}
                      </option>
                    ))}
                </select>
              </label>
            ) : null}
            <p className="text-ui-2xs text-foreground-subtlest">
              {tasks.filter((task) => task.status === "running").length} task(s) running under this
              project.
            </p>
          </div>
        ) : (
          <NcagEmpty message="Select an agent to inspect identity, worktree, usage and controls." />
        )}
      </NcagSurface>
    </div>
  );
}
