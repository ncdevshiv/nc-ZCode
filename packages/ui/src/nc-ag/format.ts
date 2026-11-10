/**
 * Small, dependency-free presentation helpers for the NC-AG dashboard.
 *
 * They are deliberately dumb: formatting and colour mapping only. Any number shown here
 * came from the API — this module never invents a value.
 */
import type { AgentStatus, DecisionStatus, TaskStatus } from "./types.js";

export type Tone = "neutral" | "success" | "warning" | "danger" | "info";

const TASK_TONES: Record<TaskStatus, Tone> = {
  created: "neutral",
  planned: "neutral",
  ready: "info",
  assigned: "info",
  running: "warning",
  review: "info",
  verification: "info",
  completed: "success",
  rejected: "danger",
  failed: "danger",
  cancelled: "neutral",
  blocked: "warning",
};

const AGENT_TONES: Record<AgentStatus, Tone> = {
  created: "neutral",
  initializing: "neutral",
  starting: "info",
  idle: "success",
  assigned: "info",
  working: "warning",
  waiting: "warning",
  blocked: "warning",
  help_requested: "warning",
  reviewing: "info",
  handoff_pending: "info",
  recovering: "info",
  stopping: "neutral",
  stopped: "neutral",
  failed: "danger",
};

const DECISION_TONES: Record<DecisionStatus, Tone> = {
  open: "warning",
  resolved: "success",
  overridden: "info",
  superseded: "neutral",
};

export function taskTone(status: TaskStatus): Tone {
  return TASK_TONES[status] ?? "neutral";
}

export function agentTone(status: AgentStatus): Tone {
  return AGENT_TONES[status] ?? "neutral";
}

export function decisionTone(status: DecisionStatus): Tone {
  return DECISION_TONES[status] ?? "neutral";
}

export const toneClasses: Record<Tone, string> = {
  neutral: "border-border bg-surface text-foreground-subtle",
  success: "border-success/40 bg-success/10 text-success",
  warning: "border-warning/40 bg-warning/10 text-warning",
  danger: "border-destructive/40 bg-destructive/10 text-destructive",
  info: "border-border bg-surface-hover text-foreground",
};

export function formatCost(usd: number): string {
  if (!Number.isFinite(usd)) return "—";
  if (usd === 0) return "$0.00";
  if (usd < 0.01) return `$${usd.toFixed(4)}`;
  return `$${usd.toFixed(2)}`;
}

export function formatTokens(value: number): string {
  if (!Number.isFinite(value)) return "—";
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(2)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}k`;
  return String(value);
}

export function formatTime(ms: number | null | undefined): string {
  if (!ms) return "—";
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

export function formatRelative(ms: number | null | undefined, now = Date.now()): string {
  if (!ms) return "—";
  const delta = Math.max(0, now - ms);
  if (delta < 5_000) return "just now";
  if (delta < 60_000) return `${Math.round(delta / 1_000)}s ago`;
  if (delta < 3_600_000) return `${Math.round(delta / 60_000)}m ago`;
  return `${Math.round(delta / 3_600_000)}h ago`;
}

/**
 * A tiny SVG bar chart. Values are real; an empty series renders an explicit "no data"
 * label instead of a misleading baseline.
 */
export function barPath(values: number[], width: number, height: number): string {
  if (values.length === 0) return "";
  const max = Math.max(...values, 1);
  const step = width / values.length;
  return values
    .map((value, index) => {
      const barHeight = Math.round((value / max) * height);
      const x = Math.round(index * step + step * 0.15);
      const barWidth = Math.max(2, Math.round(step * 0.7));
      return `M${x} ${height} V${height - barHeight} h${barWidth} V${height} Z`;
    })
    .join(" ");
}
