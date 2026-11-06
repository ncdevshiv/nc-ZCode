/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { randomUUID } from "node:crypto";
import type {
  Logger,
  McpCallToolOptions,
  McpCallToolRequest,
  McpConnectOptions,
  McpConnectionSnapshot,
  McpPort,
  McpServerConfig,
  McpServerStatus,
  McpToolCallResult,
  McpToolDescriptor,
} from "@zcode/contracts";
import type { McpTelemetryTracker } from "./telemetry.js";

const DEFAULT_IDLE_GRACE_MS = 30_000;

interface CreateMcpAdapterForPoolInput {
  connectionContext: McpConnectionContext;
  config: McpServerConfig;
  serverName: string;
  workingDirectory?: string;
}

export interface McpConnectionContext {
  mcpConnectionId: string;
  mcpIsolation: "session" | "workspace";
  sessionId?: string;
  workspaceKey?: string;
}

export interface McpConnectionPoolOptions {
  createAdapter(input: CreateMcpAdapterForPoolInput): McpPort;
  idleGraceMs?: number;
  logger?: Logger;
  telemetry?: McpTelemetryTracker;
}

export interface McpConnectionPool {
  acquireLease(options?: { leaseId?: string; sessionId?: string }): McpPort;
  close(): Promise<void>;
  stats(): { activeConnections: number; pendingCloseConnections: number };
}

interface PoolEntry {
  adapter: McpPort;
  closeTimer?: ReturnType<typeof setTimeout>;
  connectionContext: McpConnectionContext;
  connecting: Promise<McpServerStatus>;
  key: string;
  refs: Set<string>;
/** Concurrent survival checks of the same entry share one probe to avoid repeated pings/repeated reconnections. */
  revalidating?: Promise<void>;
  serverName: string;
}

export function createMcpConnectionPool(options: McpConnectionPoolOptions): McpConnectionPool {
  const entries = new Map<string, PoolEntry>();
  const idleGraceMs = options.idleGraceMs ?? DEFAULT_IDLE_GRACE_MS;
  const logger = options.logger?.child({ module: "adapters.mcp.pool" });
  let closed = false;
  let leaseSequence = 0;

  const closeEntry = async (entry: PoolEntry): Promise<void> => {
    const startedAt = Date.now();
    if (entry.closeTimer) clearTimeout(entry.closeTimer);
    entry.closeTimer = undefined;
    if (entries.get(entry.key) === entry) entries.delete(entry.key);
    try {
      await entry.adapter.close();
      logger?.info("MCP pooled connection closed", {
        ...entry.connectionContext,
        durationMs: Date.now() - startedAt,
        event: "mcp.pool.connection.closed",
        mcpServerName: entry.serverName,
        status: "completed",
      });
    } catch (error) {
      logger?.warn("MCP pooled connection close failed", {
        ...entry.connectionContext,
        error: error instanceof Error ? error.message : String(error),
        event: "mcp.pool.connection.close.failed",
        mcpServerName: entry.serverName,
      });
    } finally {
      options.telemetry?.unregisterConnection({
        connectionId: entry.connectionContext.mcpConnectionId,
      });
    }
  };

  const scheduleClose = (entry: PoolEntry): void => {
    if (entry.closeTimer) return;
    if (idleGraceMs <= 0) {
      void closeEntry(entry);
      return;
    }
    entry.closeTimer = setTimeout(() => {
      entry.closeTimer = undefined;
      if (entry.refs.size === 0) void closeEntry(entry);
    }, idleGraceMs);
    entry.closeTimer.unref?.();
  };

// The mcpPort of the settings page is a process-level `protocol-settings` lease, and the connectionKey is only
// It is composed of serverName + leaseId + config. When the configuration does not change, mcp/list will hit the same entry every time and return directly.
// Connect for the first time to the already resolved promise - no reconnection, no detection, no logging. HTTP/SSE MCP is stopped but will not
// Dispatch onclose, so the settings page will always display "Connected and available", no matter how many times you click to refresh.
// Here, when explicitly requesting revalidate, first confirm that the connection is still alive. If it dies, reconnect in place on the same entry.
// (Keep the entry identity, other leases sharing the connection will not be broken into "not leased").
  const revalidateEntry = async (
    entry: PoolEntry,
    config: McpServerConfig,
    connectOptions: McpConnectOptions,
  ): Promise<void> => {
    if (entry.revalidating) {
      await entry.revalidating;
      return;
    }
    const run = (async () => {
      const connected = await entry.connecting.then(
        () => true,
        () => false,
      );
      const state = connected
        ? (await entry.adapter.status())[entry.serverName]?.status
        : undefined;
// Don't bother with ongoing handshakes (including OAuth pending authorization) and explicit deactivation/pending trust status:
// Reconnecting will invalidate the authorization URL and PKCE/state that have been opened in the browser.
      if (state === "connecting" || state === "disabled" || state === "untrusted") {
        return;
      }
      if (state === "connected") {
        const alive = (await entry.adapter.pingServer?.(entry.serverName)) ?? true;
        if (alive) {
          logger?.debug("MCP pooled connection revalidated", {
            ...entry.connectionContext,
            event: "mcp.pool.connection.revalidated",
            mcpServerName: entry.serverName,
            status: "completed",
          });
          return;
        }
      }
      logger?.warn("MCP pooled connection is stale; reconnecting", {
        ...entry.connectionContext,
        event: "mcp.pool.connection.stale",
        mcpConnectionState: state ?? "unknown",
        mcpServerName: entry.serverName,
        status: "started",
      });
      entry.connecting = entry.adapter.connectServer(entry.serverName, config, connectOptions);
// Failure is expressed by status()/caller await entry.connecting, no repeated bubbling here.
      await entry.connecting.catch(() => undefined);
    })();
    entry.revalidating = run.finally(() => {
      entry.revalidating = undefined;
    });
    await entry.revalidating;
  };

  const acquireLease = (leaseOptions: { leaseId?: string; sessionId?: string } = {}): McpPort => {
    if (closed) throw new Error("MCP connection pool is closed");
    const leaseId = `${++leaseSequence}:${leaseOptions.leaseId ?? "lease"}`;
    const sessionId = leaseOptions.sessionId?.trim() || undefined;
    const leased = new Map<string, string>();
    const configuredServers = new Map<string, McpServerConfig>();
    let leaseClosed = false;
    let sessionStartupReported = false;

    const requireEntry = (serverName: string): PoolEntry => {
      const key = leased.get(serverName);
      const entry = key ? entries.get(key) : undefined;
      if (!entry) throw new Error(`MCP server is not leased by this session: ${serverName}`);
      return entry;
    };

    const release = (serverName: string): void => {
      const key = leased.get(serverName);
      if (!key) return;
      leased.delete(serverName);
      const entry = entries.get(key);
      if (!entry) return;
      if (!entry.refs.delete(leaseId)) return;
      options.telemetry?.releaseOwner({
        connectionId: entry.connectionContext.mcpConnectionId,
        ownerId: leaseId,
      });
      logger?.info("MCP connection lease released", {
        ...entry.connectionContext,
        event: "mcp.pool.lease.released",
        mcpLeaseId: leaseId,
        mcpServerName: serverName,
        refCount: entry.refs.size,
        ...(sessionId ? { sessionId } : {}),
      });
      if (entry.refs.size === 0) scheduleClose(entry);
    };

    const acquire = async (
      serverName: string,
      config: McpServerConfig,
      connectOptions: McpConnectOptions = {},
    ): Promise<McpServerStatus> => {
      const key = connectionKey({
        config,
        connectOptions,
        leaseId,
        serverName,
      });
      const previousKey = leased.get(serverName);
      let entry = entries.get(key);
      let ownerAdded = false;
      if (entry) {
        if (entry.closeTimer) clearTimeout(entry.closeTimer);
        entry.closeTimer = undefined;
        const previousRefCount = entry.refs.size;
        entry.refs.add(leaseId);
        ownerAdded = entry.refs.size !== previousRefCount;
        if (connectOptions.revalidate) {
          await revalidateEntry(entry, config, connectOptions);
        }
      } else {
// In the past, the logs of pool, adapter and stdio PID did not have stable correlation keys with each other and could not be obtained from one
// Session catches up to the actual MCP child process. The connection context is fixed when the entry is created, and subsequent leases share the same ID.
        const connectionContext = createConnectionContext({
          config,
          connectOptions,
          sessionId,
        });
        options.telemetry?.registerConnection({
          connectionId: connectionContext.mcpConnectionId,
          isolation: connectionContext.mcpIsolation,
          serverName,
          ...(config.source ? { source: config.source.kind } : {}),
        });
        const adapter = options.createAdapter({
          connectionContext,
          config,
          serverName,
          workingDirectory: connectOptions.workingDirectory,
        });
        entry = {
          adapter,
          connectionContext,
          connecting: adapter.connectServer(serverName, config, connectOptions),
          key,
          refs: new Set([leaseId]),
          serverName,
        };
        entries.set(key, entry);
        ownerAdded = true;
        logger?.info("MCP pooled connection created", {
          ...connectionContext,
          event: "mcp.pool.connection.created",
          mcpServerName: serverName,
          transport: config.type,
        });
      }
      if (previousKey && previousKey !== key) {
        const previous = entries.get(previousKey);
        if (previous) {
          if (previous.refs.delete(leaseId)) {
            options.telemetry?.releaseOwner({
              connectionId: previous.connectionContext.mcpConnectionId,
              ownerId: leaseId,
            });
            logger?.info("MCP connection lease released", {
              ...previous.connectionContext,
              event: "mcp.pool.lease.released",
              mcpLeaseId: leaseId,
              mcpServerName: serverName,
              refCount: previous.refs.size,
              ...(sessionId ? { sessionId } : {}),
            });
            if (previous.refs.size === 0) scheduleClose(previous);
          }
        }
      }
      leased.set(serverName, key);
      if (ownerAdded) {
        options.telemetry?.acquireOwner({
          connectionId: entry.connectionContext.mcpConnectionId,
          ownerId: leaseId,
          ...(sessionId ? { sessionId } : {}),
        });
      }
      if (previousKey !== key) {
// The workspace isolation connection will be shared by multiple sessions, and the first session cannot be recorded as the only owner;
// Recording the lease life cycle separately can accurately express the many-to-one relationship.
        logger?.info("MCP connection lease acquired", {
          ...entry.connectionContext,
          event: "mcp.pool.lease.acquired",
          mcpLeaseId: leaseId,
          mcpServerName: serverName,
          refCount: entry.refs.size,
          ...(sessionId ? { sessionId } : {}),
        });
      }
      return await entry.connecting;
    };

    const snapshot = async (): Promise<McpConnectionSnapshot> => {
      const statuses: Record<string, McpServerStatus> = {};
      const tools: McpToolDescriptor[] = [];
      for (const [serverName, key] of leased) {
        const entry = entries.get(key);
        if (!entry) continue;
        const status = (await entry.adapter.status())[serverName];
        if (status) statuses[serverName] = status;
        tools.push(...(await entry.adapter.listTools()));
      }
      if (sessionId && !sessionStartupReported) {
        sessionStartupReported = true;
        const enabledServers = [...configuredServers].filter(
          ([, config]) => config.enabled !== false,
        );
        const connectedCount = enabledServers.filter(
          ([serverName]) => statuses[serverName]?.status === "connected",
        ).length;
        options.telemetry?.recordSessionStartup({
          configuredCount: enabledServers.length,
          connectedCount,
          failedCount: enabledServers.length - connectedCount,
          processCount: enabledServers.filter(
            ([serverName, config]) =>
              config.type === "stdio" && statuses[serverName]?.status === "connected",
          ).length,
          sessionId,
        });
      }
      return { statuses, tools };
    };

    return {
      async callTool(
        request: McpCallToolRequest,
        callOptions?: McpCallToolOptions,
      ): Promise<McpToolCallResult> {
        return await requireEntry(request.serverName).adapter.callTool(request, callOptions);
      },
      async close(): Promise<void> {
        if (leaseClosed) return;
        leaseClosed = true;
        for (const serverName of [...leased.keys()]) release(serverName);
      },
      async connectConfiguredServers(
        servers: Record<string, McpServerConfig>,
        connectOptions: McpConnectOptions = {},
      ): Promise<McpConnectionSnapshot> {
        configuredServers.clear();
        for (const [serverName, config] of Object.entries(servers)) {
          configuredServers.set(serverName, config);
        }
        const configuredNames = new Set(Object.keys(servers));
        for (const serverName of [...leased.keys()]) {
          if (!configuredNames.has(serverName)) release(serverName);
        }
        await Promise.all(
          Object.entries(servers).map(([serverName, config]) =>
            acquire(serverName, config, connectOptions),
          ),
        );
        return await snapshot();
      },
      async connectServer(
        serverName: string,
        config: McpServerConfig,
        connectOptions: McpConnectOptions = {},
      ): Promise<McpServerStatus> {
        return await acquire(serverName, config, connectOptions);
      },
      async disconnectServer(serverName: string): Promise<McpServerStatus | undefined> {
        const key = leased.get(serverName);
        const entry = key ? entries.get(key) : undefined;
        const status = entry ? (await entry.adapter.status())[serverName] : undefined;
        release(serverName);
        return status
          ? {
              ...status,
              status: "disconnected",
              toolCount: 0,
              updatedAt: new Date().toISOString(),
            }
          : undefined;
      },
      async listTools(): Promise<McpToolDescriptor[]> {
        return (await snapshot()).tools;
      },
      async pingServer(serverName: string, pingOptions?: { timeoutMs?: number }): Promise<boolean> {
        const key = leased.get(serverName);
        const entry = key ? entries.get(key) : undefined;
        if (!entry) return false;
        return (await entry.adapter.pingServer?.(serverName, pingOptions)) ?? true;
      },
      async status(): Promise<Record<string, McpServerStatus>> {
        return (await snapshot()).statuses;
      },
    };
  };

  return {
    acquireLease,
    async close(): Promise<void> {
      closed = true;
      const pending = [...entries.values()];
      entries.clear();
      await Promise.all(pending.map(closeEntry));
    },
    stats() {
      return {
        activeConnections: entries.size,
        pendingCloseConnections: [...entries.values()].filter((entry) => entry.refs.size === 0)
          .length,
      };
    },
  };
}

function createConnectionContext(input: {
  config: McpServerConfig;
  connectOptions: McpConnectOptions;
  sessionId?: string;
}): McpConnectionContext {
  const mcpIsolation = input.config.isolation === "workspace" ? "workspace" : "session";
  const workspaceKey = resolveWorkspaceKey(input.connectOptions);
  return {
    mcpConnectionId: randomUUID(),
    mcpIsolation,
    ...(workspaceKey ? { workspaceKey } : {}),
    ...(mcpIsolation === "session" && input.sessionId ? { sessionId: input.sessionId } : {}),
  };
}

function resolveWorkspaceKey(connectOptions: McpConnectOptions): string | undefined {
  return (
    connectOptions.workspaceIdentity?.trim() || connectOptions.workingDirectory?.trim() || undefined
  );
}

function connectionKey(input: {
  config: McpServerConfig;
  connectOptions: McpConnectOptions;
  leaseId: string;
  serverName: string;
}): string {
//Default session isolation; only stateless servers that explicitly declare workspace are allowed to be reused across sessions.
  const scope =
    input.config.isolation === "workspace"
      ? (resolveWorkspaceKey(input.connectOptions) ?? "")
      : input.leaseId;
  return [input.serverName, scope, stableStringify(input.config)].join("\u0000");
}

function stableStringify(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(",")}]`;
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record)
    .toSorted()
    .filter((key) => record[key] !== undefined)
    .map((key) => `${JSON.stringify(key)}:${stableStringify(record[key])}`)
    .join(",")}}`;
}
