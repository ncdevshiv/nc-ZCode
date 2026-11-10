/**
 * NC-AG API client. Every call hits the real `/api/nc-ag/*` routes; there is no fixture,
 * no optimistic fake state and no silently swallowed error. Failures surface to the UI.
 */
import type {
  NcagAgent,
  NcagDashboard,
  NcagDecision,
  NcagDelivery,
  NcagEvent,
  NcagEvidence,
  NcagHealth,
  NcagIntegration,
  NcagLease,
  NcagMessage,
  NcagProfile,
  NcagProject,
  NcagRequest,
  NcagTask,
} from "./types.js";

export class NcagApiError extends Error {
  readonly code: string;
  readonly status: number;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "NcagApiError";
    this.code = code;
    this.status = status;
  }
}

/** The server token travels in the page URL (`?token=`), same convention as the app. */
function authQuery(): string {
  const token = new URLSearchParams(window.location.search).get("token");
  return token ? `?token=${encodeURIComponent(token)}` : "";
}

function withToken(path: string): string {
  const suffix = authQuery();
  if (!suffix) return path;
  return path.includes("?") ? `${path}&${suffix.slice(1)}` : `${path}${suffix}`;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(withToken(`/api/nc-ag${path}`), {
    ...init,
    headers: {
      "content-type": "application/json",
      ...init?.headers,
    },
  });
  const text = await response.text();
  let body: unknown = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    throw new NcagApiError(response.status, "invalid_response", "NC-AG returned non-JSON");
  }
  if (!response.ok) {
    const error = (body as { error?: { code?: string; message?: string } } | null)?.error;
    throw new NcagApiError(
      response.status,
      error?.code ?? `http_${response.status}`,
      error?.message ?? `NC-AG request failed (${response.status})`,
    );
  }
  return body as T;
}

export const ncagApi = {
  health: () => request<NcagHealth>("/health"),
  dashboard: () => request<NcagDashboard>("/dashboard"),
  profiles: () => request<{ profiles: NcagProfile[] }>("/profiles"),
  config: () => request<Record<string, unknown>>("/config"),
  projects: () => request<{ projects: NcagProject[] }>("/projects"),
  createProject: (input: { name: string; workspace?: string; baseBranch?: string }) =>
    request<{ project: NcagProject }>("/projects", {
      method: "POST",
      body: JSON.stringify(input),
    }),
  project: (id: string) =>
    request<{ project: NcagProject; requests: NcagRequest[] }>(`/projects/${encodeURIComponent(id)}`),
  requests: (projectId: string) =>
    request<{ requests: NcagRequest[] }>(`/projects/${encodeURIComponent(projectId)}/requests`),
  submitRequest: (projectId: string, instruction: string) =>
    request<{ request: NcagRequest; tasks: NcagTask[] }>(
      `/projects/${encodeURIComponent(projectId)}/requests`,
      { method: "POST", body: JSON.stringify({ instruction }) },
    ),
  pauseProject: (projectId: string) =>
    request<{ project: NcagProject }>(`/projects/${encodeURIComponent(projectId)}/pause`, { method: "POST" }),
  resumeProject: (projectId: string) =>
    request<{ project: NcagProject }>(`/projects/${encodeURIComponent(projectId)}/resume`, { method: "POST" }),
  request: (id: string) =>
    request<{ request: NcagRequest; tasks: NcagTask[]; requirements: unknown[]; decisions: NcagDecision[] }>(
      `/requests/${encodeURIComponent(id)}`,
    ),
  approvePlan: (id: string, approved: boolean, note?: string) =>
    request<{ request: NcagRequest }>(`/requests/${encodeURIComponent(id)}/approve`, {
      method: "POST",
      body: JSON.stringify({ approved, note }),
    }),
  delivery: (requestId: string) =>
    request<{ delivery: NcagDelivery }>(`/requests/${encodeURIComponent(requestId)}/delivery`),
  tasks: (projectId?: string) =>
    request<{ tasks: NcagTask[] }>(projectId ? `/tasks?projectId=${encodeURIComponent(projectId)}` : "/tasks"),
  task: (id: string) =>
    request<{ task: NcagTask; evidence: NcagEvidence[]; agent: NcagAgent | null }>(
      `/tasks/${encodeURIComponent(id)}`,
    ),
  cancelTask: (id: string) =>
    request<{ task: NcagTask }>(`/tasks/${encodeURIComponent(id)}`, { method: "DELETE" }),
  dag: (id: string) =>
    request<{ nodes: Record<string, unknown>[] }>(`/tasks/${encodeURIComponent(id)}/dag`),
  reassignTask: (id: string, note: string) =>
    request<{ task: NcagTask }>(`/tasks/${encodeURIComponent(id)}/reassign`, {
      method: "POST",
      body: JSON.stringify({ note }),
    }),
  handoffTask: (id: string, toAgent: string, note: string) =>
    request<{ task: NcagTask }>(`/tasks/${encodeURIComponent(id)}/handoff`, {
      method: "POST",
      body: JSON.stringify({ toAgent, note }),
    }),
  agents: (projectId?: string) =>
    request<{ agents: NcagAgent[] }>(projectId ? `/agents?projectId=${encodeURIComponent(projectId)}` : "/agents"),
  agent: (id: string) => request<{ agent: NcagAgent; messages: NcagMessage[] }>(`/agents/${encodeURIComponent(id)}`),
  pauseAgent: (id: string) =>
    request<{ agent: NcagAgent }>(`/agents/${encodeURIComponent(id)}/pause`, { method: "POST" }),
  resumeAgent: (id: string) =>
    request<{ agent: NcagAgent }>(`/agents/${encodeURIComponent(id)}/resume`, { method: "POST" }),
  cancelAgent: (id: string) =>
    request<{ agent: NcagAgent }>(`/agents/${encodeURIComponent(id)}/cancel`, { method: "POST" }),
  instructAgent: (id: string, instruction: string) =>
    request<{ ok: boolean }>(`/agents/${encodeURIComponent(id)}/instruct`, {
      method: "POST",
      body: JSON.stringify({ instruction }),
    }),
  messages: (projectId?: string) =>
    request<{ messages: NcagMessage[] }>(
      projectId ? `/messages?projectId=${encodeURIComponent(projectId)}` : "/messages",
    ),
  sendMessage: (input: {
    projectId: string;
    subject: string;
    body: string;
    taskId?: string;
    recipient?: string;
    requiresAck?: boolean;
  }) =>
    request<{ message: NcagMessage }>("/messages", {
      method: "POST",
      body: JSON.stringify(input),
    }),
  ackMessage: (id: string) =>
    request<{ acknowledged: boolean }>(`/messages/${encodeURIComponent(id)}/ack`, { method: "POST" }),
  decisions: () => request<{ decisions: NcagDecision[]; open: NcagDecision[] }>("/decisions"),
  resolveDecision: (id: string, resolution: string, rationale?: string, decidedBy?: string) =>
    request<{ decision: NcagDecision }>(`/decisions/${encodeURIComponent(id)}/resolve`, {
      method: "POST",
      body: JSON.stringify({ resolution, rationale, decidedBy }),
    }),
  leases: (projectId?: string) =>
    request<{ leases: NcagLease[] }>(
      projectId ? `/leases?projectId=${encodeURIComponent(projectId)}` : "/leases",
    ),
  waiters: (projectId: string) =>
    request<{ waiters: Record<string, unknown>[] }>(`/leases/waiters?projectId=${encodeURIComponent(projectId)}`),
  releaseLease: (id: string) =>
    request<{ released: boolean }>(`/leases/${encodeURIComponent(id)}`, { method: "DELETE" }),
  integrations: (projectId?: string) =>
    request<{ integrations: NcagIntegration[] }>(
      projectId ? `/integrations?projectId=${encodeURIComponent(projectId)}` : "/integrations",
    ),
  events: (afterId = 0, limit = 100) =>
    request<{ events: NcagEvent[]; latestId: number }>(`/events?afterId=${afterId}&limit=${limit}`),
  audit: () => request<{ entries: Record<string, unknown>[] }>("/audit"),
  verifyAudit: () =>
    request<{ verified: boolean; firstBrokenId: number | null; entries: number }>("/audit/verify"),
  usage: (projectId: string) =>
    request<{ usage: Record<string, number>; rows: Record<string, unknown>[] }>(
      `/usage?projectId=${encodeURIComponent(projectId)}`,
    ),
};

/**
 * Live event subscription (SSE). Returns a disposer. `onError` is called when the
 * stream drops so the caller can show a real connection state instead of pretending.
 */
export function subscribeNcagEvents(
  onEvent: (event: NcagEvent) => void,
  onError?: (message: string) => void,
): () => void {
  const source = new EventSource(withToken("/api/nc-ag/events/stream"));
  const handler = (raw: MessageEvent<string>) => {
    try {
      onEvent(JSON.parse(raw.data) as NcagEvent);
    } catch {
      // A malformed frame is reported, never treated as a valid event.
      onError?.("NC-AG event stream sent a malformed frame");
    }
  };
  source.onmessage = handler;
  source.onerror = () => onError?.("NC-AG event stream is disconnected");
  return () => source.close();
}
