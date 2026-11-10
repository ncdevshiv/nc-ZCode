/**
 * Data hook for the NC-AG dashboard: one place that loads, refreshes and streams state.
 *
 * Owner rule: the server is the single source of truth. This hook keeps a read-model
 * only; every mutation calls the API and then re-reads the affected slice. Nothing is
 * optimistically invented.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  NcagApiError,
  ncagApi,
  subscribeNcagEvents,
} from "./client.js";
import type {
  NcagAgent,
  NcagDashboard,
  NcagDecision,
  NcagDelivery,
  NcagEvent,
  NcagHealth,
  NcagIntegration,
  NcagLease,
  NcagMessage,
  NcagProfile,
  NcagProject,
  NcagRequest,
  NcagTask,
} from "./types.js";

export interface NcagData {
  health: NcagHealth | null;
  dashboard: NcagDashboard | null;
  projects: NcagProject[];
  agents: NcagAgent[];
  tasks: NcagTask[];
  messages: NcagMessage[];
  decisions: NcagDecision[];
  leases: NcagLease[];
  integrations: NcagIntegration[];
  events: NcagEvent[];
  profiles: NcagProfile[];
  requests: NcagRequest[];
  delivery: NcagDelivery | null;
}

const EMPTY: NcagData = {
  health: null,
  dashboard: null,
  projects: [],
  agents: [],
  tasks: [],
  messages: [],
  decisions: [],
  leases: [],
  integrations: [],
  events: [],
  profiles: [],
  requests: [],
  delivery: null,
};

const STREAM_LIMIT = 200;

function messageOf(error: unknown): string {
  if (error instanceof NcagApiError) return `${error.code}: ${error.message}`;
  if (error instanceof Error) return error.message;
  return "unknown error";
}

export function useNcagData(selectedProjectId: string | null) {
  const [data, setData] = useState<NcagData>(EMPTY);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [streamState, setStreamState] = useState<"connecting" | "live" | "down">("connecting");
  const eventCursor = useRef(0);
  const refreshToken = useRef(0);

  const load = useCallback(
    async (projectId: string | null) => {
      const token = ++refreshToken.current;
      try {
        const health = await ncagApi.health();
        const dashboard = await ncagApi.dashboard();
        const projects = await ncagApi.projects();
        const profiles = await ncagApi.profiles();
        const effectiveProject =
          projectId ?? projects.projects[projects.projects.length - 1]?.id ?? null;
        const [agents, tasks, messages, decisions, leases, integrations, events] =
          await Promise.all([
            ncagApi.agents(effectiveProject ?? undefined),
            ncagApi.tasks(effectiveProject ?? undefined),
            ncagApi.messages(effectiveProject ?? undefined),
            ncagApi.decisions(),
            ncagApi.leases(effectiveProject ?? undefined),
            ncagApi.integrations(effectiveProject ?? undefined),
            ncagApi.events(0, STREAM_LIMIT),
          ]);
        const requests = effectiveProject ? await ncagApi.requests(effectiveProject) : { requests: [] };
        const latest = requests.requests[requests.requests.length - 1] ?? null;
        const delivery = latest ? (await ncagApi.delivery(latest.id)).delivery : null;
        if (token !== refreshToken.current) return; // a newer load won
        eventCursor.current = events.latestId;
        setData({
          health,
          dashboard,
          projects: projects.projects,
          profiles: profiles.profiles,
          agents: agents.agents,
          tasks: tasks.tasks,
          messages: messages.messages,
          decisions: decisions.decisions,
          leases: leases.leases,
          integrations: integrations.integrations,
          events: events.events,
          requests: requests.requests,
          delivery,
        });
        setError(null);
      } catch (loadError) {
        if (token !== refreshToken.current) return;
        setError(messageOf(loadError));
      } finally {
        setLoading(false);
      }
    },
    [],
  );

  // Initial + project-change load.
  useEffect(() => {
    setLoading(true);
    void load(selectedProjectId);
  }, [load, selectedProjectId]);

  // Live stream: append and, on notable kinds, re-read the authoritative slices.
  useEffect(() => {
    const dispose = subscribeNcagEvents(
      (event) => {
        setStreamState("live");
        setData((previous) => ({
          ...previous,
          events: [...previous.events, event].slice(-STREAM_LIMIT),
        }));
        if (event.id > eventCursor.current) eventCursor.current = event.id;
        if (
          event.kind.startsWith("task.") ||
          event.kind.startsWith("agent.") ||
          event.kind.startsWith("decision.") ||
          event.kind.startsWith("integration.") ||
          event.kind.startsWith("budget.") ||
          event.kind === "verification.passed" ||
          event.kind === "verification.failed"
        ) {
          void load(selectedProjectId);
        }
      },
      () => setStreamState("down"),
    );
    return dispose;
  }, [load, selectedProjectId]);

  // Fallback polling keeps the dashboard honest even when SSE is unavailable.
  useEffect(() => {
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") void load(selectedProjectId);
    }, 15_000);
    return () => window.clearInterval(timer);
  }, [load, selectedProjectId]);

  const refresh = useCallback(() => {
    setLoading(true);
    return load(selectedProjectId);
  }, [load, selectedProjectId]);

  /** Run one API mutation; errors land in the banner and the slice is re-read. */
  const run = useCallback(
    async (operation: () => Promise<unknown>) => {
      try {
        await operation();
        await load(selectedProjectId);
        return true;
      } catch (mutationError) {
        setError(messageOf(mutationError));
        return false;
      }
    },
    [load, selectedProjectId],
  );

  const selectProject = useMemo(
    () => data.projects.find((project) => project.id === selectedProjectId) ?? null,
    [data.projects, selectedProjectId],
  );

  return { data, loading, error, setError, streamState, refresh, run, selectedProject: selectProject };
}
