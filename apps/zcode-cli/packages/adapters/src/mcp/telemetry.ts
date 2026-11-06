import { Buffer } from "node:buffer";
import { createHmac, randomUUID } from "node:crypto";
import type { ZCodeMcpTelemetryEvent } from "@zcode/shared";
import {
  createMcpResourceTelemetry,
  type McpResourceTelemetryOptions,
} from "./resource-telemetry.js";

const BUILTIN_MCP_ID_PREFIX = "builtin:";
const BUILTIN_NODE_REPL_SERVER_NAME = "node_repl";
const MCP_ID_SAFE_CHARACTER_PATTERN = /^[A-Za-z0-9._~-]$/;
const PLUGIN_MCP_NAMESPACE_PREFIX = "plugin:";

type McpTelemetryEvent = ZCodeMcpTelemetryEvent;
export type McpTelemetrySource = Extract<
  ZCodeMcpTelemetryEvent,
  { kind: "process_start" }
>["mcpSource"];
export type McpTelemetryIsolation = Extract<
  ZCodeMcpTelemetryEvent,
  { kind: "process_start" }
>["mcpIsolation"];
export interface McpProcessTelemetryIdentity {
  mcpId: string;
  mcpInstanceId: string;
}

/**
* MCP subprocess view for resource manager: only returns pid and ownership, without any sampled values.
* Keep the plain text serverName / pluginName here - only transferred between the local host ↔ CLI,
* No need to leave the local machine, no need to desensitize; the Host uses it to assign the pid on the ps process tree to the specific plug-in.
 */
export interface McpTrackedProcess {
  pid: number;
  serverName: string;
  mcpSource: McpTelemetrySource;
/** `plugin:<name>:<key>` The plug-in name in the namespace; builtin host MCP (node_repl) is completed by the caller according to the official plug-in table */
  pluginName?: string;
}

export interface McpTelemetryTracker {
  acquireOwner(input: { connectionId: string; ownerId: string; sessionId?: string }): void;
  recordSessionStartup(input: {
    configuredCount: number;
    connectedCount: number;
    failedCount: number;
    processCount: number;
    sessionId: string;
  }): void;
  recordProcessCrashed(input: {
    connectionId: string;
    exitCode: number | null;
    signal: string | null;
  }): void;
  recordProcessClosed(input: { connectionId: string }): void;
  recordProcessStarted(input: {
    connectionId: string;
    pid: number;
  }): McpProcessTelemetryIdentity | undefined;
  releaseOwner(input: { connectionId: string; ownerId: string }): void;
  registerConnection(input: {
    connectionId: string;
    isolation: McpTelemetryIsolation;
    serverName: string;
    source?: McpTelemetrySource;
  }): void;
  unregisterConnection(input: { connectionId: string }): void;
/** There are currently MCP connections recorded by living processes (pure memory, no I/O) */
  listProcesses(): McpTrackedProcess[];
  sampleNow(): Promise<void>;
  start(): void;
  stop(): void;
}

interface CreateMcpTelemetryTrackerOptions {
  arch?: ZCodeMcpTelemetryEvent["arch"];
  idSalt: string;
  now?: () => number;
  onEvent(event: McpTelemetryEvent): void;
  platform?: ZCodeMcpTelemetryEvent["platform"];
  randomId?: () => string;
  onResourceSamples?: McpResourceTelemetryOptions["onResourceSamples"];
  processProbe?: McpResourceTelemetryOptions["processProbe"];
  logicalCpuCount?: number;
  totalMemoryGb?: number;
  timer?: McpResourceTelemetryOptions["timer"];
}

interface TrackedConnection {
  connectionId: string;
  isolation: McpTelemetryIsolation;
  mcpId: string;
  mcpSource: McpTelemetrySource;
  serverName: string;
  owners: Map<string, string | undefined>;
  process?: {
    instanceId: string;
    pid: number;
    startedAt: number;
  };
  unownedAt?: number;
}

export function createMcpTelemetryTracker(
  options: CreateMcpTelemetryTrackerOptions,
): McpTelemetryTracker {
  const arch = options.arch ?? (process.arch as ZCodeMcpTelemetryEvent["arch"]);
  const now = options.now ?? Date.now;
  const platform = options.platform ?? (process.platform as ZCodeMcpTelemetryEvent["platform"]);
  const randomId = options.randomId ?? randomUUID;
  const connections = new Map<string, TrackedConnection>();
  const emit = (event: McpTelemetryEvent): void => {
    try {
      options.onEvent(event);
    } catch {
// Telemetry is bypassed, downstream notifications or IPC shutdowns must not alter MCP connections, recycling, or crash handling.
    }
  };

  const resourceTelemetry = createMcpResourceTelemetry({
    ...options,
    arch,
    platform,
    now,
    getProcesses: () =>
      [...connections.values()].flatMap((connection) => {
        const trackedProcess = connection.process;
        if (!trackedProcess) return [];
        return [
          {
            ...trackedProcess,
            mcpId: connection.mcpId,
            isCurrent: () =>
              connections.get(connection.connectionId) === connection &&
              connection.process === trackedProcess,
            observed(samples, sampledAt, memoryScope) {
              if (!samples) {
                if (connection.owners.size === 0) {
                  connection.process = undefined;
                  connections.delete(connection.connectionId);
                }
                return;
              }
              const sessionIds = new Set(
                [...connection.owners.values()].filter((id) => id !== undefined),
              );
              const unownedMs =
                connection.owners.size === 0 && connection.unownedAt !== undefined
                  ? Math.max(0, sampledAt - connection.unownedAt)
                  : 0;
// Retain the tracker's internal orphan observation capabilities; bootstrap no longer sends old memory facts to the protocol.
              emit({
                arch,
                kind: "memory",
                mcpId: connection.mcpId,
                mcpInstanceId: trackedProcess.instanceId,
                mcpIsolation: connection.isolation,
                mcpSource: connection.mcpSource,
                platform,
                occurredAt: sampledAt,
                memoryKb: samples.reduce((total, sample) => total + sample.rssKb, 0),
                memoryScope,
                orphanSuspected: connection.owners.size === 0 && unownedMs > 60_000,
                ownerSessionCount: sessionIds.size,
                unownedSeconds: unownedMs / 1_000,
              });
            },
          },
        ];
      }),
  });

  return {
    acquireOwner(input) {
      const connection = connections.get(input.connectionId);
      if (!connection) return;
      connection.owners.set(input.ownerId, input.sessionId);
      connection.unownedAt = undefined;
    },
    recordProcessCrashed(input) {
      const connection = connections.get(input.connectionId);
      const trackedProcess = connection?.process;
      if (!connection || !trackedProcess) return;
      connection.process = undefined;
      const occurredAt = now();
      const sessionIds = new Set(
        [...connection.owners.values()].filter(
          (sessionId): sessionId is string => sessionId !== undefined,
        ),
      );
      emit({
        affectedSessionCount: sessionIds.size,
        arch,
        exitCode: input.exitCode,
        kind: "process_crash",
        mcpId: connection.mcpId,
        mcpInstanceId: trackedProcess.instanceId,
        mcpIsolation: connection.isolation,
        mcpSource: connection.mcpSource,
        occurredAt,
        platform,
        signal: input.signal,
        uptimeMs: Math.max(0, occurredAt - trackedProcess.startedAt),
      });
    },
    recordProcessClosed(input) {
      const connection = connections.get(input.connectionId);
      if (!connection) return;
      connection.process = undefined;
      if (connection.owners.size === 0) connections.delete(input.connectionId);
    },
    recordProcessStarted(input) {
      const connection = connections.get(input.connectionId);
      if (!connection || !Number.isInteger(input.pid) || input.pid <= 0) return undefined;
      const mcpInstanceId = randomId();
      const occurredAt = now();
      connection.process = {
        instanceId: mcpInstanceId,
        pid: input.pid,
        startedAt: occurredAt,
      };
      if (connection.owners.size === 0) connection.unownedAt ??= occurredAt;
      emit({
        arch,
        kind: "process_start",
        mcpId: connection.mcpId,
        mcpInstanceId,
        mcpIsolation: connection.isolation,
        mcpSource: connection.mcpSource,
        occurredAt,
        platform,
      });
      return { mcpId: connection.mcpId, mcpInstanceId };
    },
    recordSessionStartup(input) {
      emit({
        arch,
        configuredCount: input.configuredCount,
        connectedCount: input.connectedCount,
        failedCount: input.failedCount,
        kind: "session_startup",
        occurredAt: now(),
        platform,
        processCount: input.processCount,
        sessionId: input.sessionId,
      });
    },
    releaseOwner(input) {
      const connection = connections.get(input.connectionId);
      if (!connection || !connection.owners.delete(input.ownerId)) return;
      if (connection.owners.size !== 0) return;
      connection.unownedAt = now();
// When the last owner is released after a process crash, the pool entry will still survive within idle grace.
// Deleting the registration in advance here will cause the new process after the same workspace entry is reused to permanently lose telemetry.
// The final state of registration is explicitly unregistered by pool closeEntry. When the owner is released, only the ownerless time is recorded.
    },
    registerConnection(input) {
      const mcpSource = input.source ?? resolveMcpSource(input.serverName);
      connections.set(input.connectionId, {
        connectionId: input.connectionId,
        isolation: input.isolation,
        mcpId: resolveMcpId(input.serverName, mcpSource, options.idSalt),
        mcpSource,
        serverName: input.serverName,
        owners: new Map(),
      });
    },
    unregisterConnection(input) {
      const connection = connections.get(input.connectionId);
      if (!connection) return;
//The pool entry has entered the final state, and the remaining lease cannot continue to be regarded as the owner; if the process tree recycling fails, it will be retained.
// The process is marked as orphan for subsequent sampling, and the registration is deleted after confirming that the process is closed or the OS is no longer visible.
      connection.owners.clear();
      connection.unownedAt ??= now();
      if (!connection.process) connections.delete(input.connectionId);
    },
    listProcesses() {
      const processes: McpTrackedProcess[] = [];
      for (const connection of connections.values()) {
        if (!connection.process) continue;
        const pluginName = resolvePluginName(connection.serverName);
        processes.push({
          pid: connection.process.pid,
          serverName: connection.serverName,
          mcpSource: connection.mcpSource,
          ...(pluginName ? { pluginName } : {}),
        });
      }
      return processes;
    },
    sampleNow: resourceTelemetry.sampleNow,
    start: resourceTelemetry.start,
    stop: resourceTelemetry.stop,
  };
}

/** `plugin:<name>:<key>` → `<name>`; non-plugin namespace returns undefined */
export function resolvePluginName(serverName: string): string | undefined {
  if (!serverName.startsWith(PLUGIN_MCP_NAMESPACE_PREFIX)) return undefined;
  const name = serverName.slice(PLUGIN_MCP_NAMESPACE_PREFIX.length).split(":")[0]?.trim();
  return name ? name : undefined;
}

function resolveMcpSource(serverName: string): McpTelemetrySource {
  if (serverName === BUILTIN_NODE_REPL_SERVER_NAME) return "builtin";
  return serverName.startsWith(PLUGIN_MCP_NAMESPACE_PREFIX) ? "plugin" : "custom";
}

function resolveMcpId(serverName: string, source: McpTelemetrySource, idSalt: string): string {
  if (source === "builtin") {
    const publicName = serverName.startsWith(PLUGIN_MCP_NAMESPACE_PREFIX)
      ? serverName.slice(PLUGIN_MCP_NAMESPACE_PREFIX.length)
      : serverName;
    return `${BUILTIN_MCP_ID_PREFIX}${publicName.split(":").map(encodeMcpIdSegment).join(":")}`;
  }
  const digest = createHmac("sha256", idSalt).update(serverName).digest("hex").slice(0, 12);
  return `${source}:${digest}`;
}

function encodeMcpIdSegment(value: string): string {
// Reason: encodeURIComponent will throw URIError when encountering an isolated surrogate, and telemetry encoding cannot reversely block MCP startup.
// Buffer's UTF-8 encoding will replace the malformed sequence with U+FFFD, and then escape it byte by byte into the stable `%HH` allowed by the protocol.
  let encoded = "";
  for (const byte of Buffer.from(value, "utf8")) {
    const character = String.fromCharCode(byte);
    encoded += MCP_ID_SAFE_CHARACTER_PATTERN.test(character)
      ? character
      : `%${byte.toString(16).toUpperCase().padStart(2, "0")}`;
  }
  return encoded;
}
