/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { randomUUID } from "node:crypto";
import { resolve } from "node:path";
import { isDeepStrictEqual } from "node:util";
import {
  Client,
  ClientCredentialsProvider,
  computeScopeUnion,
  ProtocolError,
  SdkError,
  SdkErrorCode,
  SSEClientTransport,
  StreamableHTTPClientTransport,
  UnsupportedProtocolVersionError,
  type AuthProvider,
  type OAuthClientProvider,
  type VersionNegotiationMode,
  type VersionNegotiationOptions,
} from "@modelcontextprotocol/client";
import { StdioClientTransport } from "@modelcontextprotocol/client/stdio";
import type {
  Logger,
  McpCallToolOptions,
  McpCallToolRequest,
  McpConnectOptions,
  McpConnectionSnapshot,
  McpContentBlock,
  McpOAuthConfig,
  McpPort,
  McpServerConfig,
  McpServerStatus,
  McpToolCallResult,
  McpToolDescriptor,
  OfficialMcpAuthFailureReason,
  OfficialMcpAuthHeadersPort,
  OfficialMcpTrustedOriginRegistry,
  TraceContext,
} from "@zcode/contracts";
import { ZCODE_MCP_SERVER_REQUEST_ID_META_KEY } from "@zcode/contracts";
import { normalizeMcpToolDescriptor } from "./descriptor.js";
import {
  createOfficialMcpAuthFetch,
  OfficialMcpAuthError,
  type OfficialMcpServerResponseInfo,
} from "./official-auth.js";
import {
  OFFICIAL_MCP_AUTH_META_KEY,
  ZCODE_OFFICIAL_MCP_AUTH_TYPE,
  type McpServerFailureKind,
  type OfficialMcpAuthFailureKind,
} from "@zcode/shared";
import {
  buildMcpStdioEnv,
  createMcpTransportFetch,
  type NetworkEgressEnvPolicy,
} from "./network.js";
import {
  createMcpConnectionPool,
  type McpConnectionContext,
  type McpConnectionPool,
} from "./pool.js";
import {
  createCredentialKeyPrefix,
  type McpOAuthAuthorizationContext,
  type McpOAuthRuntimeOptions,
} from "./oauth.js";
import {
  classifyInteractiveAuthorizationTrigger,
  type InteractiveAuthorizationTrigger,
} from "./oauth-errors.js";
import {
  MCP_OAUTH_AUTHORIZATION_TRANSACTION_TTL_MS,
  runMcpInteractiveAuthorization,
  type McpInteractiveAuthorizationOutcome,
} from "./oauth-interactive.js";
import {
  createSharedZCodeCredentialStore,
  type SharedZCodeCredentialStore,
} from "../auth/shared-credentials.js";
import { loadCredentialPair } from "./oauth-credentials.js";
import { createMcpOAuthTokenProvider } from "./oauth-provider.js";
import { terminateMcpStdioProcessTree } from "./process-tree.js";
import { ProcessTreeStdioClientTransport } from "./stdio-transport.js";
import type { McpTelemetryTracker } from "./telemetry.js";
import {
  createMcpDeadline,
  McpTimeoutError,
  type McpDeadline,
  remainingMcpDeadlineMs,
  waitWithinMcpDeadline,
  withTimeout,
} from "./timeout.js";

const DEFAULT_MCP_TIMEOUT_MS = 30_000;
// The survival detection is only allowed to occupy a short time: it hangs on the synchronization path of the settings page refresh, and when it times out, it will be declared dead and trigger reconnection.
const MCP_PING_TIMEOUT_MS = 5_000;
const MAX_MCP_VERSION_PROBE_TIMEOUT_MS = 5_000;
const MCP_STDIO_STDERR_LOG_MAX_CHARS = 4_000;
/**
* span → The upper limit of the number of temporary storage items for request id. Under normal circumstances, each item will be taken away at the end of the same tool call.
* Only unclaimed ones (such as connection period requests) are left. A few dozen are enough, purely to prevent unbounded growth under long sessions.
 */
const MAX_TRACKED_SERVER_REQUEST_IDS = 64;

export interface CreateMcpAdapterOptions {
  clientName?: string;
  clientVersion?: string;
  connectionContext?: McpConnectionContext;
  env?: NodeJS.ProcessEnv;
  logger?: Logger;
  telemetry?: McpTelemetryTracker;
  mcpOAuth?: McpOAuthRuntimeOptions;
  network?: NetworkEgressEnvPolicy;
  /**
*Official Server MCP authentication dependency. Fail closed when trustedOrigins is missing; authHeadersPort
* Can be defaulted. At this time, each request will be downgraded anonymously and handed over to the server for authoritative judgment.
   */
  officialMcpAuth?: {
    authHeadersPort?: OfficialMcpAuthHeadersPort;
    trustedOrigins: OfficialMcpTrustedOriginRegistry;
    /**
* Current ZCode API origin. The stdio form does not have `url` for verification, and the targetOrigin can only be given by the host.
* - Plugins therefore cannot direct identity headers to other origins.
* The `resolveZCodeApiOrigin` of trustedOrigins must have the same origin, otherwise the judgments on both sides will diverge.
     */
    resolveZCodeApiOrigin?: () => string;
    workspaceIdentity?: string;
  };
  workingDirectory?: string;
}

type McpClient = Client;
type McpTransport = StdioClientTransport | StreamableHTTPClientTransport | SSEClientTransport;
type AuthorizationCodeOAuthConfig = Extract<McpOAuthConfig, { type: "authorization_code" }>;

/**
* The identity header load of stdio's official MCP is delivered with `_meta` of each outbound protocol message.
 *
* Issued even if it fails (`ok: false` + enumeration reason); the stdio plug-in will not hit the official endpoint when it cannot get the end. HTTP path rule
* The adapter initiates an unidentified tools/call, allowing the ZCode server to return an authoritative structured error. Give reason to stdio
* The plug-in can make it change "Not logged in" and "No Coding Plan"
* "Package" is presented to users truthfully, instead of silently downgrading to an inexplicable failure.
 */
type OfficialMcpAuthMetaPayload =
  | { ok: true; headers: Record<string, string> }
  | { ok: false; reason: OfficialMcpAuthFailureReason };

interface McpServerRecord {
  client?: McpClient;
  abortController?: AbortController;
  connecting?: Promise<McpServerStatus>;
  config: McpServerConfig;
  status: McpServerStatus;
  tools: McpToolDescriptor[];
  transport?: McpTransport;
}

export function createMcpAdapter(options: CreateMcpAdapterOptions = {}): McpPort {
  return new NodeMcpAdapter(options);
}

export function createMcpAdapterConnectionPool(
  options: CreateMcpAdapterOptions = {},
): McpConnectionPool {
  return createMcpConnectionPool({
    logger: options.logger,
    telemetry: options.telemetry,
    createAdapter: ({ connectionContext, workingDirectory }) =>
      createMcpAdapter({
        ...options,
        connectionContext,
        workingDirectory: workingDirectory ?? options.workingDirectory,
      }),
  });
}

export {
  createMcpConnectionPool,
  type McpConnectionPool,
  type McpConnectionPoolOptions,
} from "./pool.js";
export {
  createMcpTelemetryTracker,
  resolvePluginName,
  type McpTelemetryTracker,
  type McpTrackedProcess,
} from "./telemetry.js";

class NodeMcpAdapter implements McpPort {
  private readonly adapterInstanceId = randomUUID();
  private readonly clientName: string;
  private readonly clientVersion: string;
  private readonly connectionContext?: McpConnectionContext;
  private readonly env?: NodeJS.ProcessEnv;
  private readonly logger?: Logger;
  private readonly mcpOAuth?: McpOAuthRuntimeOptions;
  private readonly network?: NetworkEgressEnvPolicy;
  private readonly officialMcpAuth?: CreateMcpAdapterOptions["officialMcpAuth"];
  private readonly telemetry?: McpTelemetryTracker;
  private readonly connectionGenerations = new Map<string, number>();
  private credentialStore?: SharedZCodeCredentialStore;
  /**
* Temporary storage slot for official authentication failure classification. Cannot read from error object - version of SDK
* negotiation will repackage OfficialMcpAuthError into a normal Error, and instanceof will be invalid;
* Reverse interpretation based on incorrect text is also not allowed. Therefore, it is written at the throw point and cleared immediately after failConnection is retrieved.
   */
  private readonly lastOfficialAuthKind = new Map<string, OfficialMcpAuthFailureKind>();
  /**
* span → server request id. Only the official MCP will write (the only place you can see the response header is the auth fetch
* wrapper), for in-band failure (HTTP 200 + `isError`) to bring the id back to the tool result.
   *
* Use span instead of traceId as key: traceId covers the entire top-level session, multiple calls of the same session
* If you share it, the association will be serialized; span is the granularity of a tool call.
   *
* Bounded and on-the-fly: can't get matching spans (like initialize/tools/list, they don't have `_meta`)
* Let the entries be squeezed out naturally, and never "take the latest" - that will paste the id of the last call to this failure.
   */
  private readonly serverRequestIdBySpan = new Map<string, string>();
  private readonly connectionDiagnosticByServer = new Map<
    string,
    Pick<McpServerStatus, "failureKind" | "serverRequestId">
  >();
  private readonly records = new Map<string, McpServerRecord>();
  private readonly workingDirectory?: string;

  constructor(options: CreateMcpAdapterOptions) {
    this.clientName = options.clientName ?? "zcode";
    this.clientVersion = options.clientVersion ?? "0.0.0";
    this.connectionContext = options.connectionContext;
    this.env = options.env;
    this.logger = options.logger?.child({
      ...this.connectionContext,
      module: "adapters.mcp",
    });
    this.mcpOAuth = options.mcpOAuth;
    this.network = options.network;
    this.officialMcpAuth = options.officialMcpAuth;
    this.telemetry = options.telemetry;
    this.workingDirectory = options.workingDirectory;
  }

  async connectConfiguredServers(
    servers: Record<string, McpServerConfig>,
    options: McpConnectOptions = {},
  ): Promise<McpConnectionSnapshot> {
    const startedAt = Date.now();
    const serverNames = Object.keys(servers);
    this.logger?.info("MCP configured servers connection started", {
      event: "mcp.configured_servers.connect.started",
      serverCount: serverNames.length,
      serverNames,
      status: "started",
    });
    const configuredNames = new Set(Object.keys(servers));
    await Promise.all(
      Array.from(this.records.keys())
        .filter((name) => !configuredNames.has(name))
        .map((name) => this.disconnectServer(name)),
    );

    await Promise.all(
      Object.entries(servers).map(([name, config]) => {
        const record = this.records.get(name);
        if (
          record?.connecting &&
          record.status.status === "connecting" &&
          record.status.authorization &&
          isDeepStrictEqual(record.config, config)
        ) {
// Full convergence of the same configuration may overlap with OAuth callback wait; reconnect
// The original session will be closed, invalidating the authorization URL, PKCE/state and callback that have been opened in the browser.
// The connection life cycle can be shared, but the 15-second waiting budget of the Session and the 5-minute budget of the AbortSignal cannot be inherited from the settings page.
          return this.waitForSharedConnection(name, record, options);
        }
        return this.connectServer(name, config, options);
      }),
    );

    const statuses = await this.status();
    const tools = await this.listTools();
    const statusCounts = Object.values(statuses).reduce<Record<string, number>>(
      (counts, status) => {
        counts[status.status] = (counts[status.status] ?? 0) + 1;
        return counts;
      },
      {},
    );
    this.logger?.info("MCP configured servers connection completed", {
      durationMs: Date.now() - startedAt,
      event: "mcp.configured_servers.connect.completed",
      serverCount: serverNames.length,
      status: "completed",
      statusCounts,
      toolCount: tools.length,
    });
    return {
      statuses,
      tools,
    };
  }

  async connectServer(
    name: string,
    config: McpServerConfig,
    options: McpConnectOptions = {},
  ): Promise<McpServerStatus> {
    const startedAt = Date.now();
    const timeoutMs = config.timeoutMs ?? DEFAULT_MCP_TIMEOUT_MS;
    const generation = this.nextConnectionGeneration(name);
    this.lastOfficialAuthKind.delete(name);
    this.connectionDiagnosticByServer.delete(name);
    this.logger?.info("MCP server connection started", {
      event: "mcp.server.connect.started",
      mcpServerName: name,
      status: "started",
      timeoutMs,
      transport: config.type,
    });
    await this.closeRecord(name);

    if (config.enabled === false) {
      const status = this.createStatus(config, "disabled");
      this.records.set(name, { config, status, tools: [] });
      this.logger?.info("MCP server connection skipped", {
        durationMs: Date.now() - startedAt,
        event: "mcp.server.connect.skipped",
        mcpServerName: name,
        status: "completed",
        transport: config.type,
      });
      return status;
    }

    const abortController = new AbortController();
    const abortExternal = () => {
      abortController.abort(
        options.signal?.reason instanceof Error ? options.signal.reason : undefined,
      );
    };
    if (options.signal?.aborted) {
      abortExternal();
    } else {
      options.signal?.addEventListener("abort", abortExternal, { once: true });
    }

    const connectingStatus = this.createStatus(config, "connecting");
    const record: McpServerRecord = {
      abortController,
      config,
      status: connectingStatus,
      tools: [],
    };
    this.records.set(name, record);
    const connecting = this.openServerConnection({
      config,
      generation,
      name,
      oauthAuthorizationTimeoutMs: options.oauthAuthorizationTimeoutMs,
      signal: abortController.signal,
      timeoutMs,
      workingDirectory: options.workingDirectory,
    }).finally(() => {
      options.signal?.removeEventListener("abort", abortExternal);
    });
    record.connecting = connecting;
// In the past `oauthAuthorizationTimeoutMs` (15 seconds of session) was regarded as the OAuth transaction lifetime,
// After 15 seconds, it will be closed together with the callback listener. The real person will not have time to complete the authorization in the browser (onsite evidence:
//A successful authorization takes about 74 seconds). Now it is only used as **this caller's waiting budget**: to return the current
// snapshot (including authorization URL), background connection and 300 seconds authorization transaction continue to survive.
    return await this.waitForSharedConnection(name, record, options);
  }

  async disconnectServer(name: string): Promise<McpServerStatus | undefined> {
    const record = this.records.get(name);
    if (!record) return undefined;

    this.nextConnectionGeneration(name);
    await this.closeRecord(name);
    const status = this.createStatus(record.config, "disconnected");
    this.records.set(name, {
      config: record.config,
      status,
      tools: [],
    });
    return status;
  }

  async status(): Promise<Record<string, McpServerStatus>> {
    return Object.fromEntries(
      Array.from(this.records.entries()).map(([name, record]) => [name, record.status]),
    );
  }

// When the HTTP/SSE MCP service is stopped, onclose will not be dispatched (there is no resident stream to interrupt), and the record will stop for a long time.
// connected; what is read when the settings page is refreshed is this old snapshot of "silent death". It looks like the refresh button does not take effect.
// Ping is the MCP basic protocol method, which is used to make transport survivability explicit.
  async pingServer(name: string, options: { timeoutMs?: number } = {}): Promise<boolean> {
    const record = this.records.get(name);
    if (!record?.client || record.status.status !== "connected") {
      return false;
    }
    const timeoutMs = Math.min(
      options.timeoutMs ?? MCP_PING_TIMEOUT_MS,
      record.config.timeoutMs ?? DEFAULT_MCP_TIMEOUT_MS,
    );
    const generation = this.connectionGenerations.get(name) ?? 0;
    try {
      await record.client.ping({ timeout: timeoutMs });
      return true;
    } catch (error) {
// The server returns a JSON-RPC error (for example, ping is not implemented), indicating that the connection itself is alive and cannot be disconnected based on this.
      if (isPeerAnsweredError(error)) {
        return true;
      }
      if (!this.isCurrentConnection(name, generation)) return false;
      const current = this.records.get(name);
      if (current && current.client === record.client) {
        current.status = this.createStatus(current.config, "disconnected", {
          error: "MCP server did not answer ping",
          failureKind: "unexpected_disconnect",
        });
      }
      this.logger?.warn("MCP server ping failed", {
        ...this.connectionContext,
        error: error instanceof Error ? error.message : String(error),
        event: "mcp.server.ping.failed",
        mcpServerName: name,
        status: "failed",
        transport: record.config.type,
      });
      return false;
    }
  }

  async listTools(): Promise<McpToolDescriptor[]> {
    return Array.from(this.records.values()).flatMap((record) => record.tools);
  }

  async callTool(
    request: McpCallToolRequest,
    options: McpCallToolOptions = {},
  ): Promise<McpToolCallResult> {
    const initialRecord = this.records.get(request.serverName);
    const timeoutMs =
      options.timeoutMs ?? initialRecord?.config.timeoutMs ?? DEFAULT_MCP_TIMEOUT_MS;
    const deadline = createMcpDeadline(timeoutMs);
    const timeoutMessage = `MCP tool ${request.serverName}/${request.toolName} timed out after ${timeoutMs}ms`;
    const pending = initialRecord?.connecting;
    if (pending) {
// connecting is the shared connection/OAuth recovery task held by the adapter. Past here naked await,
// The timeout/abort of the tool caller is completely invalid; but directly aborting the underlying task will close the shared tasks of other callers.
// callback listener. This only limits the current waiter, and the shared task continues to be held by the record life cycle.
      await waitWithinMcpDeadline(pending, deadline, timeoutMessage, options.signal);
    }

// After the stdio MCP child process dies (such as node_repl being hit by an asynchronous error), there is no recovery path before:
// The connection is only established once when the session is created, and the session resume will not rebuild it. The tool for this session will never fail from now on.
// Here, the disconnected record is reconnected before calling; the state in the server process (such as REPL variables) cannot be restored.
// But the tool itself is available again.
    const disconnected = this.records.get(request.serverName);
    if (disconnected && disconnected.status.status === "disconnected") {
      await waitWithinMcpDeadline(
        this.reconnectForCall(request.serverName, disconnected.config),
        deadline,
        timeoutMessage,
        options.signal,
      );
    }

    const record = this.records.get(request.serverName);
    if (!record?.client || record.status.status !== "connected") {
      throw new Error(`MCP server is not connected: ${request.serverName}`);
    }

    try {
      return await this.callToolOnClient(
        record.client,
        request,
        remainingMcpDeadlineMs(deadline, timeoutMessage),
        options.signal,
      );
    } catch (error) {
// When the token expires, is revoked or the scope is insufficient after the connection is established,
// In the past these authentication errors bubbled up as is, users saw naked errors and never self-healed - OAuth self-healing only existed
// startup connect path. Now the running period and the establishment period share the same Phase 2 → Phase 1 arrangement.
      const trigger = classifyInteractiveAuthorizationTrigger(error);
      if (trigger && record.config.type !== "stdio") {
        return await this.recoverToolCallAuthorization({
          error,
          record,
          request,
          deadline,
          timeoutMessage,
          trigger,
          ...(options.signal ? { signal: options.signal } : {}),
        });
      }
// Prevent the onclose race condition that has not yet been dispatched: the SDK throws "Not connected" when the transport is disconnected.
//Only retry this certain disconnection error once, and other errors will bubble up as they are.
      if (!(error instanceof Error) || error.message !== "Not connected") throw error;
      try {
        await waitWithinMcpDeadline(
          this.reconnectForCall(request.serverName, record.config),
          deadline,
          timeoutMessage,
          options.signal,
        );
      } catch (reconnectError) {
        this.logger?.warn("MCP server reconnect failed", {
          error: reconnectError instanceof Error ? reconnectError.message : String(reconnectError),
          event: "mcp.server.reconnect.failed",
          mcpServerName: request.serverName,
          status: "failed",
        });
        throw error;
      }
      const revived = this.records.get(request.serverName);
      if (!revived?.client || revived.status.status !== "connected") throw error;
      return await this.callToolOnClient(
        revived.client,
        request,
        remainingMcpDeadlineMs(deadline, timeoutMessage),
        options.signal,
      );
    }
  }

/** The connection period diagnosis is saved by server; the tool call request id continues to be isolated by span. */
  private rememberServerResponse(
    serverName: string,
    response: OfficialMcpServerResponseInfo,
  ): void {
    if (
      !response.spanId &&
      response.rpcMethod !== "tools/call" &&
      this.records.get(serverName)?.status.status === "connecting"
    ) {
      if (response.failureKind) {
        this.connectionDiagnosticByServer.set(serverName, {
          failureKind: response.failureKind,
          ...(response.serverRequestId ? { serverRequestId: response.serverRequestId } : {}),
        });
      }
      return;
    }
    if (!response.spanId) return;
    if (!response.serverRequestId) return;
// 401 retry will generate two responses for the same span, and the latter will be overwritten - the last one left is the one to be reported.
    this.serverRequestIdBySpan.set(response.spanId, response.serverRequestId);
    while (this.serverRequestIdBySpan.size > MAX_TRACKED_SERVER_REQUEST_IDS) {
      const oldest = this.serverRequestIdBySpan.keys().next();
      if (oldest.done) break;
      this.serverRequestIdBySpan.delete(oldest.value);
    }
  }

/** Get and clear the request id of the span. If it cannot be retrieved, undefined will be returned, and no guessing will be made. */
  private takeServerRequestId(spanId: string | undefined): string | undefined {
    if (!spanId) return undefined;
    const requestId = this.serverRequestIdBySpan.get(spanId);
    if (requestId !== undefined) this.serverRequestIdBySpan.delete(spanId);
    return requestId;
  }

  /**
* Parse the identity header of stdio's official MCP outbound protocol message this time.
* Returning undefined means "not an official stdio server" - at this time, the key must not appear in `_meta`, otherwise it will be equal to
* Broadcast the identity header to any third-party plugin.
   */
  private async resolveOfficialStdioAuthMeta(
    serverName: string,
    config: McpServerConfig,
    signal: AbortSignal | undefined,
  ): Promise<OfficialMcpAuthMetaPayload | undefined> {
    if (config.type !== "stdio" || !isOfficialAuthConfig(config) || !config.official) {
      return undefined;
    }
    const official = config.official;
    const authHeadersPort = this.officialMcpAuth?.authHeadersPort;
    const trustedOrigins = this.officialMcpAuth?.trustedOrigins;
    const resolveZCodeApiOrigin = this.officialMcpAuth?.resolveZCodeApiOrigin;
    const logBase = {
      event: "mcp.official_auth.stdio_meta",
      mcpKey: official.mcpKey,
      mcpServerName: serverName,
      module: "adapters.mcp",
    };
    const fail = (reason: OfficialMcpAuthFailureReason): OfficialMcpAuthMetaPayload => {
// Deliberately not write lastOfficialAuthKind: that map is only read by failConnection and is used to report **connection failure**
// Label the category. The missing identity header of stdio will not cause the connection to fail. It will be kept until the server is used.
// When the connection is actually disconnected due to other reasons (such as the death of the child process, etc.), it is recorded in the log as the reason for the disconnection, which is misleading.
// The observability of this path is borne by the following own event + the reason sent to the plug-in.
      this.logger?.warn("Official MCP stdio auth headers unavailable", {
        ...logBase,
        reason,
        status: "failed",
      });
      return { ok: false, reason };
    };

// The standalone CLI does not have a host auth port. Do not omit this key silently: the plug-in cannot distinguish between "host not supported" and
// "The host supports it but I'm not logged in", only explicit reason can give the correct user prompt.
    if (!authHeadersPort || !trustedOrigins || !resolveZCodeApiOrigin) {
      return fail("official_auth_unavailable");
    }

// stdio does not have a url, and the origin is given by the host rather than declared by the plug-in. isTrusted here degenerates into a true assertion, but still needs to be called:
// It also verifies https, rejects URLs with username/password, and leaves the dev loopback switch in effect.
    //
// These two steps were originally called naked. Origin resolution depends on settings/runtime environment, isTrusted is
// Injection implementation, both may throw. Exception naked bubbling will bypass the entire failure classification: the plugin does not receive `{ok:false, reason}`,
// And reason is a contract across adapter / host / UI (deciding the prompt copy and whether to retry). Therefore, the unified mapping is
// official_auth_unavailable - The host side cannot parse the trusted origin, which means "official authentication is not available" for the plug-in.
// The error text will only be entered into the log and will never be involved in process judgment.
    let targetOrigin: string;
    let trust: Awaited<ReturnType<OfficialMcpTrustedOriginRegistry["isTrusted"]>>;
    try {
      targetOrigin = resolveZCodeApiOrigin();
      trust = await trustedOrigins.isTrusted({
        mcpKey: official.mcpKey,
        origin: targetOrigin,
        pluginId: official.pluginId,
      });
    } catch (error) {
      this.logger?.warn("Official MCP stdio origin resolution failed", {
        ...logBase,
        error: error instanceof Error ? error.message : String(error),
        errorName: error instanceof Error ? error.name : "unknown",
        pluginId: official.pluginId,
      });
      return fail("official_auth_unavailable");
    }
    if (!trust.trusted) {
      this.logger?.warn("Official MCP stdio origin is not trusted", {
        ...logBase,
        detail: trust.detail ?? "unknown",
        pluginId: official.pluginId,
        targetOrigin,
      });
      return fail("official_mcp_origin_untrusted");
    }

    const resolved = await authHeadersPort.resolveHeaders({
      mcpKey: official.mcpKey,
      pluginId: official.pluginId,
      targetOrigin,
      ...(this.officialMcpAuth?.workspaceIdentity
        ? { workspaceIdentity: this.officialMcpAuth.workspaceIdentity }
        : {}),
      ...(this.workingDirectory ? { workspacePath: this.workingDirectory } : {}),
      ...(signal ? { signal } : {}),
    });
    if (!resolved.ok) return fail(resolved.reason);

//Only remember the header name and package dimensions, never the header value - the log retention period is not controlled.
    this.logger?.debug("Official MCP stdio auth headers attached", {
      ...logBase,
      identityHeaderNames: Object.keys(resolved.headers)
        .map((name) => name.toLowerCase())
        .sort(),
      ...(resolved.headers["Bigmodel-Target-Type"]
        ? { identityTargetType: resolved.headers["Bigmodel-Target-Type"] }
        : {}),
      status: "completed",
    });
    return { ok: true, headers: resolved.headers };
  }

  private async callToolOnClient(
    client: McpClient,
    request: McpCallToolRequest,
    timeoutMs: number,
    signal: AbortSignal | undefined,
  ): Promise<McpToolCallResult> {
// There is no log at all before the tool is called: when it times out, we can neither see what the budget is, nor can we distinguish between "server slow" and "server slow"
// "Client budget is too small". The budget and time consumption are recorded here, but only the key of the parameter is recorded (the value may be user input).
    const logBase = {
      event: "mcp.tool.call",
      mcpServerName: request.serverName,
      mcpToolName: request.toolName,
      module: "adapters.mcp",
      timeoutMs,
    };
    const argumentKeys = Object.keys(request.arguments ?? {}).sort();
    this.logger?.debug("MCP tool call started", {
      ...logBase,
      argumentKeys,
      status: "started",
    });

    const startedAt = Date.now();
    try {
      const result = await client.callTool(
        {
          name: request.toolName,
          arguments: request.arguments ?? {},
          ...((request.trace || request.runtimeScope || request.workspaceKey || request.workspacePath)
            ? { _meta: mcpRequestMeta(request) }
            : {}),
        },
        {
          signal,
          timeout: timeoutMs,
          resetTimeoutOnProgress: true,
        },
      );

      const durationMs = Date.now() - startedAt;
      const isError = typeof result.isError === "boolean" ? result.isError : false;
// Official MCP in-band failure (quota exhausted, no package) is HTTP 200 + isError, wrapper
// Non-2xx warn cannot be covered; the request id can only be seen by the wrapper, so press span here to retrieve it.
      const serverRequestId = this.takeServerRequestId(request.trace?.spanId);
      const outcome = {
        ...logBase,
        contentBlocks: Array.isArray(result.content) ? result.content.length : 0,
        durationMs,
        hasStructuredContent: result.structuredContent !== undefined,
// Business-level failures (isError) are different from transmission-level failures and must be counted separately.
        isError,
        ...(serverRequestId ? { serverRequestId } : {}),
      };
      if (isError) {
// Previously, there was only this debug when in-band failed, and the lowest level of the production logger was Info - equal to the quota being exhausted.
// Such failures are completely invisible in production logs.
        this.logger?.warn("MCP tool returned an error", { ...outcome, status: "failed" });
      } else {
        this.logger?.debug("MCP tool call completed", { ...outcome, status: "completed" });
      }

      const meta = isRecord(result._meta) ? result._meta : undefined;
      return {
        content: Array.isArray(result.content)
          ? (result.content as McpContentBlock[])
          : [{ type: "text", text: "" }],
        structuredContent: result.structuredContent,
        isError: typeof result.isError === "boolean" ? result.isError : undefined,
// Append only on failure: on the success path it's pure noise. Keys already given by the server will not be overwritten.
        _meta:
          isError && serverRequestId
            ? { ...meta, [ZCODE_MCP_SERVER_REQUEST_ID_META_KEY]: serverRequestId }
            : meta,
      };
    } catch (error) {
      const durationMs = Date.now() - startedAt;
      const message = error instanceof Error ? error.message : String(error);
// Determine whether it has timed out: SDK will throw MCP error code -32001 (RequestTimeout) when it times out.
// The underlying fetch abort throws AbortError. Both must be identifiable at a glance, otherwise you will only see the naked message.
      const timedOut =
        /timed?\s*out|timeout/i.test(message) ||
        (error instanceof Error && error.name === "AbortError");
//Transmission level failure also includes: there is no request id in the message thrown by the SDK in 4xx/5xx.
      const serverRequestId = this.takeServerRequestId(request.trace?.spanId);
      this.logger?.warn("MCP tool call failed", {
        ...logBase,
        argumentKeys,
        durationMs,
        error: message,
        ...(serverRequestId ? { serverRequestId } : {}),
        errorName: error instanceof Error ? error.name : "unknown",
        status: "failed",
        timedOut,
// Time-consuming and close to the budget ⇒ It was cut off by us; far less than the budget ⇒ It was cut off by the peer or the network.
        ...(timedOut ? { budgetExhausted: durationMs >= timeoutMs * 0.9 } : {}),
      });
      throw error;
    }
  }

  private async reconnectForCall(name: string, config: McpServerConfig): Promise<void> {
    this.logger?.warn("MCP server reconnecting after lost connection", {
      event: "mcp.server.reconnect.started",
      mcpServerName: name,
      status: "started",
      transport: config.type,
    });
    await this.connectServer(name, config);
  }

  /**
* Runtime authentication recovery: Phase 2 interactive authorization → Phase 1 reconnect → the original tool call can be safely retried once at most.
   *
* It shares `runInteractiveOAuthAuthorization` with Jianlian, so the semantics of solo flight, fencing, and caller budget are exactly the same.
   */
  private async recoverToolCallAuthorization(input: {
    deadline: McpDeadline;
    error: unknown;
    record: McpServerRecord;
    request: McpCallToolRequest;
    signal?: AbortSignal;
    timeoutMessage: string;
    trigger: InteractiveAuthorizationTrigger;
  }): Promise<McpToolCallResult> {
    const { record, request } = input;
    const config = record.config;
    if (config.type === "stdio") throw input.error;
    const oauthConfig = resolveAuthorizationCodeOAuthConfig(config);
    if (!oauthConfig) throw input.error;

    this.logger?.warn("MCP tool call requires OAuth authorization", {
      event: "mcp.oauth.tool_call.authorization_required",
      mcpServerName: request.serverName,
      oauthTriggerReason: input.trigger.reason,
      status: "started",
      toolName: request.toolName,
    });

    const recovery = this.ensureToolCallAuthorizationRecovery({
      config,
      name: request.serverName,
      oauthConfig,
      record,
      trigger: input.trigger,
    });
    const recoveredStatus = await waitWithinMcpDeadline(
      recovery,
      input.deadline,
      input.timeoutMessage,
      input.signal,
    );
    if (recoveredStatus.status !== "connected") {
      throw input.error;
    }

    const revived = this.records.get(request.serverName);
    if (!revived?.client || revived.status.status !== "connected") throw input.error;
    return await this.callToolOnClient(
      revived.client,
      request,
      remainingMcpDeadlineMs(input.deadline, input.timeoutMessage),
      input.signal,
    );
  }

  /**
* Create or reuse runtime OAuth recovery. Complete Phase 2 → Phase 1 is held by adapter-owned record;
* The tool caller can only wait and cannot terminate shared transactions with its own AbortSignal.
   */
  private ensureToolCallAuthorizationRecovery(input: {
    config: Extract<McpServerConfig, { type: "http" | "sse" }>;
    name: string;
    oauthConfig: AuthorizationCodeOAuthConfig;
    record: McpServerRecord;
    trigger: InteractiveAuthorizationTrigger;
  }): Promise<McpServerStatus> {
    const current = this.records.get(input.name);
    if (
      current?.connecting &&
      current.status.status === "connecting" &&
      isDeepStrictEqual(current.config, input.config)
    ) {
      return current.connecting;
    }

    const generation = this.nextConnectionGeneration(input.name);
    const abortController = new AbortController();
    const recoveryRecord: McpServerRecord = {
      abortController,
      config: input.config,
      status: this.createStatus(input.config, "connecting", {
        toolCount: input.record.tools.length,
      }),
// The runtime tool has been advertised to the core; the descriptor is retained during recovery to avoid misjudgment of the settings page/borrowed port tool from disappearing.
      tools: input.record.tools,
    };
    this.records.set(input.name, recoveryRecord);
    const connecting = this.runToolCallAuthorizationRecovery({
      abortController,
      config: input.config,
      generation,
      name: input.name,
      oauthConfig: input.oauthConfig,
      previousClient: input.record.client,
      previousTransport: input.record.transport,
      trigger: input.trigger,
    });
    recoveryRecord.connecting = connecting;
    return connecting;
  }

  private async runToolCallAuthorizationRecovery(input: {
    abortController: AbortController;
    config: Extract<McpServerConfig, { type: "http" | "sse" }>;
    generation: number;
    name: string;
    oauthConfig: AuthorizationCodeOAuthConfig;
    previousClient?: McpClient;
    previousTransport?: McpTransport;
    trigger: InteractiveAuthorizationTrigger;
  }): Promise<McpServerStatus> {
    const startedAt = Date.now();
    try {
// The handshake and token of the original transport have expired and must be retired by the shared owner; connectServer cannot be called.
// Otherwise, closeRecord will abort recoveryRecord's own controller, forming self-cancellation.
      await this.closeClientAndTransport(input.name, input.previousClient, input.previousTransport);
      const outcome = await this.runInteractiveOAuthAuthorization({
        config: input.config,
        generation: input.generation,
        name: input.name,
        oauthConfig: input.oauthConfig,
        serverUrl: input.config.url,
        signal: input.abortController.signal,
        trigger: input.trigger,
      });
      if (outcome.status === "authorized" || outcome.status === "already-authorized") {
        return await this.openServerConnection({
          config: input.config,
          generation: input.generation,
          name: input.name,
          oauthAuthorizationAttempted: true,
          signal: input.abortController.signal,
          timeoutMs: input.config.timeoutMs ?? DEFAULT_MCP_TIMEOUT_MS,
        });
      }
      return await this.failConnection({
        config: input.config,
        error:
          outcome.status === "pending"
            ? new Error(
                `MCP server ${input.name} OAuth authorization is still in progress; complete it in the browser and reconnect`,
              )
            : outcome.error,
        failureKind: "oauth_authorization_failed",
        generation: input.generation,
        name: input.name,
        startedAt,
      });
    } catch (error) {
// Defense boundary: Shared recovery promise must be total operation. Any new orchestration exceptions added in the future will only
// It converges to failed record, and rejected connecting promise cannot be left to contaminate subsequent snapshots.
      return await this.failConnection({
        config: input.config,
        error,
        failureKind: "oauth_authorization_failed",
        generation: input.generation,
        name: input.name,
        startedAt,
      });
    }
  }

  async close(): Promise<void> {
    const startedAt = Date.now();
    const serverCount = this.records.size;
    for (const name of this.records.keys()) {
      this.nextConnectionGeneration(name);
    }
    await Promise.all(Array.from(this.records.keys()).map((name) => this.closeRecord(name)));
    this.records.clear();
    this.connectionDiagnosticByServer.clear();
    this.logger?.info("MCP adapter closed", {
      durationMs: Date.now() - startedAt,
      event: "mcp.adapter.closed",
      serverCount,
      status: "completed",
    });
  }

  private waitForSharedConnection(
    name: string,
    record: McpServerRecord,
    options: McpConnectOptions,
  ): Promise<McpServerStatus> {
    const connecting = record.connecting;
    if (!connecting) return Promise.resolve(record.status);
    if (options.oauthAuthorizationTimeoutMs === undefined && !options.signal) {
      return connecting;
    }

    let abortHandler: (() => void) | undefined;
    let timeoutId: ReturnType<typeof setTimeout> | undefined;
    const currentStatus = () => this.records.get(name)?.status ?? record.status;
    const waiters: Promise<McpServerStatus>[] = [connecting];

    const timeoutMs = options.oauthAuthorizationTimeoutMs;
    if (timeoutMs !== undefined) {
      waiters.push(
        new Promise((resolvePromise) => {
          timeoutId = setTimeout(() => resolvePromise(currentStatus()), timeoutMs);
        }),
      );
    }
    if (options.signal) {
      waiters.push(
        new Promise((resolvePromise) => {
          if (options.signal?.aborted) {
            resolvePromise(currentStatus());
            return;
          }
          abortHandler = () => resolvePromise(currentStatus());
          options.signal?.addEventListener("abort", abortHandler, {
            once: true,
          });
        }),
      );
    }

    return Promise.race(waiters).finally(() => {
      if (timeoutId !== undefined) clearTimeout(timeoutId);
      if (abortHandler) options.signal?.removeEventListener("abort", abortHandler);
    });
  }

  private async openServerConnection(input: {
    config: McpServerConfig;
    generation: number;
    name: string;
    oauthAuthorizationTimeoutMs?: number;
    signal: AbortSignal;
    timeoutMs: number;
    workingDirectory?: string;
    oauthAuthorizationAttempted?: boolean;
  }): Promise<McpServerStatus> {
    const {
      config,
      generation,
      name,
      oauthAuthorizationAttempted = false,
      oauthAuthorizationTimeoutMs,
      signal,
      timeoutMs,
      workingDirectory,
    } = input;
    const startedAt = Date.now();
    let client: McpClient | undefined;
    let connectDurationMs: number | undefined;
    let getRecentStderr: (() => string | undefined) | undefined;
    let listToolsDurationMs: number | undefined;
    let transport: McpTransport | undefined;
    let failureKind: McpServerFailureKind =
      config.type === "stdio" ? "process_start_failed" : "network_unreachable";

    try {
      const transportBundle = await this.createTransport(
        config,
        name,
        generation,
        oauthAuthorizationTimeoutMs,
        workingDirectory,
        signal,
      );
      transport = transportBundle.transport;
      getRecentStderr = this.attachStdioLogging(name, transport);
      client = new Client(
        {
          name: this.clientName,
          version: this.clientVersion,
        },
        {
          versionNegotiation: resolveVersionNegotiation(config, timeoutMs),
        },
      );
      this.updateCurrentRecord(name, generation, {
        client,
        transport,
      });

      const connectStartedAt = Date.now();
      await withTimeout(
        client.connect(transport),
        timeoutMs,
        `MCP server ${name} connection timed out after ${timeoutMs}ms`,
        signal,
      );
      connectDurationMs = Date.now() - connectStartedAt;

      failureKind = "tool_list_failed";
      const listToolsStartedAt = Date.now();
      const listed = await withTimeout(
        client.listTools(),
        timeoutMs,
        `MCP server ${name} tool listing timed out after ${timeoutMs}ms`,
        signal,
      );
      listToolsDurationMs = Date.now() - listToolsStartedAt;
      const tools = listed.tools.map((tool) =>
        normalizeMcpToolDescriptor(
          name,
          tool,
          config.timeoutMs,
// Only http mode is set. The purpose of this tag is **structured identification in trust results**
// (Quota exhausted / No package), so the criterion must be "who produced the result":
// - http: Results come from the ZCode backend. The fetch wrapper verifies the origin for each request; the login state is only in
// tools/call parsing, if missing, structured coding_plan_required will be returned by the same trusted backend;
// - stdio: The result is generated by the plug-in process itself and can be forged arbitrarily `{"error_code":"quota_exceeded"}`,
// As a result, a misleading prompt of "Quota exhausted/Please activate Coding Plan" pops up above the user input box.
// The original criterion is `type !== "sse"`, and stdio is put in together, which means that the threshold is zero on stdio.
// Note that this is not to prevent the leakage of credentials (that is responsible for origin verification), but to prevent **result forgery**.
          config.type === "http" && config.auth?.type === ZCODE_OFFICIAL_MCP_AUTH_TYPE,
        ),
      );
      const negotiatedProtocolEra = client.getProtocolEra();
      const negotiatedProtocolVersion = client.getNegotiatedProtocolVersion();
      const status = this.createStatus(config, "connected", {
        protocolEra: negotiatedProtocolEra,
        toolCount: tools.length,
      });
      if (!this.isCurrentConnection(name, generation)) {
        await this.closeClientAndTransport(name, client, transport);
        return this.records.get(name)?.status ?? status;
      }
      this.connectionDiagnosticByServer.delete(name);
      this.records.set(name, {
        client,
        config,
        status,
        tools,
        transport,
      });
      const mcpTransportPid = getStdioTransportPid(transport);
      const mcpProcessIdentity =
        mcpTransportPid != null && this.connectionContext
          ? this.telemetry?.recordProcessStarted({
              connectionId: this.connectionContext.mcpConnectionId,
              pid: mcpTransportPid,
            })
          : undefined;
// The death of the stdio MCP child process (such as node_repl being penetrated by the asynchronous error of the REPL cell) cannot be completely
// Silent - no logging, the status stays at connected, and subsequent calls will only throw "Not connected".
// Hang up onclose to make the unexpected disconnection explicit; actively closing the path will clear onclose first (see closeClientAndTransport).
      client.onclose = () => {
        if (!this.isCurrentConnection(name, generation)) return;
        const current = this.records.get(name);
        if (!current || current.client !== client) return;
        const recentStderr = getRecentStderr?.();
        const processExit = getStdioTransportExitInfo(transport);
        current.status = this.createStatus(current.config, "disconnected", {
          error: "MCP server connection closed unexpectedly",
          failureKind: "unexpected_disconnect",
        });
        this.logger?.warn("MCP server connection lost", {
          ...this.connectionContext,
          event: "mcp.server.connection_lost",
          mcpServerName: name,
          ...(mcpTransportPid != null ? { mcpTransportPid } : {}),
          ...(mcpProcessIdentity ?? {}),
          ...(processExit
            ? { exitCode: processExit.exitCode, signal: processExit.signal ?? undefined }
            : {}),
          status: "failed",
          transport: current.config.type,
          ...(recentStderr ? { stderr: recentStderr } : {}),
        });
        if (
          current.config.type === "stdio" &&
          this.connectionContext &&
          (processExit || !isStdioTransportProcessAlive(transport))
        ) {
          this.telemetry?.recordProcessCrashed({
            connectionId: this.connectionContext.mcpConnectionId,
            exitCode: processExit?.exitCode ?? null,
            signal: processExit?.signal ?? null,
          });
        }
      };
// Previously, the connection log only recorded transport. After auto negotiation, it was impossible to determine whether it was modern or legacy.
// Record the configuration policy and SDK handshake results at the same time to avoid mistaking `auto` for the final protocol version.
// The connection pool context and stdio transport PID have not entered the same event in the past and cannot be associated with session,
// workspace, protocol version and real child process; stdio PID only represents the final session transport, not the probe child.
      this.logger?.info("MCP server connected", {
        ...this.connectionContext,
        connectDurationMs,
        durationMs: Date.now() - startedAt,
        event: "mcp.server.connected",
        listToolsDurationMs,
        mcpClientName: this.clientName,
        mcpClientVersion: this.clientVersion,
        mcpProtocolEra: negotiatedProtocolEra ?? "unknown",
        mcpProtocolVersion: negotiatedProtocolVersion ?? "unknown",
        mcpServerName: name,
        ...(mcpProcessIdentity ?? {}),
        ...(mcpTransportPid != null ? { mcpTransportPid } : {}),
        mcpVersionNegotiationMode: formatVersionNegotiationMode(config),
        status: "completed",
        toolCount: tools.length,
        transport: config.type,
      });
      return status;
    } catch (error) {
// In the past, what was waiting here was the listener opened by the old provider itself, and used the session's 15-second budget.
// Hard shutdown - 15 seconds is the caller's wait budget, not the lifetime of the authorized transaction. Now determine whether it is necessary according to the error type
// Interactive authorization and hand over authorization to Phase 2 independent transactions (independent locks, fresh DCR, 300 seconds transaction TTL).
      const trigger = oauthAuthorizationAttempted
        ? undefined
        : classifyInteractiveAuthorizationTrigger(error);
      const authorizationCodeOAuthConfig =
        trigger && config.type !== "stdio"
          ? resolveAuthorizationCodeOAuthConfig(config)
          : undefined;
      if (trigger && authorizationCodeOAuthConfig && config.type !== "stdio") {
// When negotiation fails, the SDK has closed the transport and cannot be reused; Phase 2 does not require transport.
        await this.closeClientAndTransport(name, client, transport);
        const outcome = await this.runInteractiveOAuthAuthorization({
          config,
          generation,
          name,
          oauthConfig: authorizationCodeOAuthConfig,
          serverUrl: config.url,
          signal,
          trigger,
        });
        if (outcome.status === "authorized" || outcome.status === "already-authorized") {
          return await this.openServerConnection({
            ...input,
            oauthAuthorizationAttempted: true,
          });
        }
// client/transport has been closed before entering Phase 2, no longer passed here; failureKind will be used
// Diagnostic classification, let the settings page distinguish "Authorization not completed" from network/process failures.
        return this.failConnection({
          config,
          connectDurationMs,
          error:
            outcome.status === "pending"
              ? new Error(
                  `MCP server ${name} OAuth authorization is still in progress; complete it in the browser and reconnect`,
                )
              : outcome.error,
          failureKind: "oauth_authorization_failed",
          generation,
          getRecentStderr,
          listToolsDurationMs,
          name,
          startedAt,
        });
      }
// `protocol_negotiation_failed` enumeration and UI copy already exist in shared/i18n, but the adapter
// There has been no output side - the SDK's server/discover probe hard failed in auto/pin mode (typical: Feishu project
// MCP returns HTTP 200 + id:null non-standard JSON-RPC error for unknown methods, the body cannot pass
// JSONRPCMessageSchema) will fall all the way to the default failureKind "network_unreachable", so the settings page
// Display misleading "Network Unreachable". Here SDKs are identified by structured error type (SdkErrorCode/isInstance)
// Negotiation fails and outputs correct classification without relying on error text; withTimeout does not package errors (timeout.ts only transparently transmits
//reject), the cause chain is only used as a defensive cover.
      const negotiationFailureKind = isProtocolNegotiationFailure(error)
        ? ("protocol_negotiation_failed" as const)
        : undefined;
      return this.failConnection({
        client,
        config,
        connectDurationMs,
        error,
        generation,
        getRecentStderr,
        listToolsDurationMs,
        name,
        startedAt,
        transport,
        failureKind: negotiationFailureKind ?? failureKind,
      });
    }
  }

  /**
* Phase 2: Interactive authorization.
   *
* The lifespan of an authorized transaction is 300 seconds, regardless of the caller's waiting budget (session 15 seconds); the closure on the caller side occurs in
* `connectServer` / `waitForSharedConnection`, this method is not aware of the caller budget.
   */
  private async runInteractiveOAuthAuthorization(input: {
    config: Extract<McpServerConfig, { type: "http" | "sse" }>;
    generation: number;
    name: string;
    oauthConfig: AuthorizationCodeOAuthConfig;
    serverUrl: string;
    signal: AbortSignal;
    trigger: InteractiveAuthorizationTrigger;
  }): Promise<McpInteractiveAuthorizationOutcome> {
    try {
      const oauthOptions = this.createAuthorizationCodeOAuthOptions(
        input.config,
        input.name,
        input.generation,
      );
      const credentialStore = oauthOptions?.credentialStore ?? createSharedZCodeCredentialStore();
      const keyPrefix = createCredentialKeyPrefix(input.name, input.serverUrl, input.oauthConfig);
// The final scope of the 403 step-up must be config ∪ token.scope ∪ challenge
// Union of . When re-authorizing with only challenge scope, the authorization server may revoke the previously granted scope according to the new request.
// Change the challenge to 403 for the next request, forming a re-authorization ping-pong. The scope of token response is allowed to be missing
// (RFC 6749 §3.3), so the scope declared in the configuration must be explicitly incorporated, and you cannot just look at the token echo.
      let requestedScope: string | undefined = input.oauthConfig.scope;
      if (input.trigger.requiredScope) {
        const currentPair = await loadCredentialPair(credentialStore, keyPrefix);
        requestedScope = computeScopeUnion(
          input.oauthConfig.scope,
          currentPair?.tokens?.scope,
          input.trigger.requiredScope,
        );
      }
      return await runMcpInteractiveAuthorization({
        adapterInstanceId: this.adapterInstanceId,
        config: input.oauthConfig,
        credentialStore,
        fetchFn: createMcpTransportFetch({ env: this.env, network: this.network }),
// 403 step-up: When requiredScope is a strict superset of the current token scope, refresh cannot expand the rights.
// (RFC 6749 §6), reauthorization must be forced, otherwise the new scope will be silently discarded and 403ed again.
        ...(input.trigger.reason === "insufficient_scope" ? { forceReauthorization: true } : {}),
        keyPrefix,
        logger: this.logger,
        ...(oauthOptions?.onAuthorizationRequired
          ? { onAuthorizationRequired: oauthOptions.onAuthorizationRequired }
          : {}),
        ...(oauthOptions?.openAuthorizationUrl
          ? { openAuthorizationUrl: oauthOptions.openAuthorizationUrl }
          : {}),
        ...(requestedScope ? { requestedScope } : {}),
        ...(input.trigger.resourceMetadataUrl
          ? { resourceMetadataUrl: new URL(input.trigger.resourceMetadataUrl) }
          : {}),
        serverName: input.name,
        serverUrl: input.serverUrl,
        signal: input.signal,
        transactionTtlMs: MCP_OAUTH_AUTHORIZATION_TRANSACTION_TTL_MS,
      });
    } catch (error) {
// The return type of this method already models orchestration failure as outcome. past credential load,
// Exceptions in authz lease or follower callback will be rejected naked, bypassing failConnection, leaving
// status=connecting + rejected record.connecting, and let the entire batch of connectConfiguredServers fail.
      this.logger?.warn("MCP OAuth authorization orchestration failed", {
        error: error instanceof Error ? error.message : String(error),
        errorName: error instanceof Error ? error.name : "unknown",
        event: "mcp.oauth.authorization.orchestration_failed",
        mcpServerName: input.name,
        status: "failed",
      });
      return { status: "failed", error };
    }
  }

  private async failConnection(input: {
    client?: McpClient;
    config: McpServerConfig;
    connectDurationMs?: number;
    error: unknown;
    failureKind?: McpServerFailureKind;
    generation: number;
    getRecentStderr?: () => string | undefined;
    listToolsDurationMs?: number;
    name: string;
    startedAt: number;
    transport?: McpTransport;
  }): Promise<McpServerStatus> {
    const {
      client,
      config,
      connectDurationMs,
      error,
      failureKind: fallbackFailureKind,
      generation,
      getRecentStderr,
      listToolsDurationMs,
      name,
      startedAt,
      transport,
    } = input;
    const message = error instanceof Error ? error.message : String(error);
// The stable classification of official MCP authentication failure must be entered into the log: failConnection was originally only recorded
// error.message, and most categories do not appear in the message text (only the auth-port one is included),
// As a result, official_mcp_origin_untrusted / official_auth_rejected etc. cannot be grep in the production log.
    const officialAuthKind =
      (error instanceof OfficialMcpAuthError ? error.kind : undefined) ??
      this.lastOfficialAuthKind.get(name);
    this.lastOfficialAuthKind.delete(name);
    const responseDiagnostic = this.connectionDiagnosticByServer.get(name);
    this.connectionDiagnosticByServer.delete(name);
    const failureKind =
      (officialAuthKind === "official_mcp_origin_untrusted"
        ? "official_origin_untrusted"
        : undefined) ??
      responseDiagnostic?.failureKind ??
      (error instanceof McpTimeoutError && fallbackFailureKind !== "tool_list_failed"
        ? "connection_timeout"
        : undefined) ??
      fallbackFailureKind ??
      "connection_failed";
    const displayMessage = responseDiagnostic?.serverRequestId
      ? `${message} - ${responseDiagnostic.serverRequestId}`
      : message;
    const status = this.createStatus(config, "failed", {
      error: displayMessage,
      failureKind,
      ...(responseDiagnostic?.serverRequestId
        ? { serverRequestId: responseDiagnostic.serverRequestId }
        : {}),
    });
    const recentStderr = getRecentStderr?.();
    const mcpTransportPid = getStdioTransportPid(transport);
    await this.closeClientAndTransport(name, client, transport);
    if (!this.isCurrentConnection(name, generation)) {
      return this.records.get(name)?.status ?? status;
    }
    this.records.set(name, { config, status, tools: [] });
    this.logger?.warn("MCP server connection failed", {
      ...this.connectionContext,
      connectDurationMs,
      durationMs: Date.now() - startedAt,
      error: displayMessage,
      event: "mcp.server.failed",
      listToolsDurationMs,
      mcpServerName: name,
      ...(officialAuthKind ? { officialAuthKind } : {}),
      ...(mcpTransportPid != null ? { mcpTransportPid } : {}),
      status: "failed",
      ...(recentStderr ? { stderr: recentStderr } : {}),
      transport: config.type,
    });
    return status;
  }

  private async createTransport(
    config: McpServerConfig,
    serverName: string,
    generation: number,
    _oauthAuthorizationTimeoutMs?: number,
    workingDirectory?: string,
    signal?: AbortSignal,
  ): Promise<{ transport: McpTransport }> {
    if (config.type === "stdio") {
      return {
        transport: new ProcessTreeStdioClientTransport({
          command: config.command,
          args: config.args ?? [],
          cwd: config.cwd
            ? resolve(workingDirectory ?? this.workingDirectory ?? process.cwd(), config.cwd)
            : (workingDirectory ?? this.workingDirectory),
          env: {
            ...buildMcpStdioEnv({ env: this.env, network: this.network }),
            ...config.env,
          },
          stderr: "pipe",
          ...(isOfficialAuthConfig(config) && config.official
            ? {
                requestMetaProvider: async () => {
                  const authMeta = await this.resolveOfficialStdioAuthMeta(
                    serverName,
                    config,
                    signal,
                  );
                  return authMeta ? { [OFFICIAL_MCP_AUTH_META_KEY]: authMeta } : undefined;
                },
              }
            : {}),
        }),
      };
    }

    const fetch = createMcpTransportFetch({
      env: this.env,
      network: this.network,
    });
    if (config.type === "http") {
      const officialAuthFetch = this.createOfficialAuthFetch(config, serverName, generation);
      return {
        transport: new StreamableHTTPClientTransport(new URL(config.url), {
// The authProvider under the official authentication path must be undefined: OAuth credentials are not included.
// Cannot afford localhost callback server, 401/403 does not transfer the authorization process.
          authProvider: this.createOAuthClientProvider(serverName, config),
          fetch: officialAuthFetch ?? fetch,
          requestInit: config.headers ? { headers: config.headers } : undefined,
        }),
      };
    }

    return {
      transport: new SSEClientTransport(new URL(config.url), {
        authProvider: this.createOAuthClientProvider(serverName, config),
        fetch,
        requestInit: config.headers ? { headers: config.headers } : undefined,
      }),
    };
  }

  /**
* Dynamic fetch of official authentication MCP. Returning undefined means taking the normal MCP path.
   *
* Trusted origin directly fails closed when dependencies are missing. The auth port can be missing: the wrapper still verifies origin.
* Each request is downgraded anonymously and authoritatively determined by the server.
   */
  private createOfficialAuthFetch(
    config: McpServerConfig,
    serverName: string,
    generation: number,
  ): typeof globalThis.fetch | undefined {
    if (!isOfficialAuthConfig(config) || config.type !== "http" || !config.official) {
      return undefined;
    }
    const official = config.official;
    const authHeadersPort = this.officialMcpAuth?.authHeadersPort;
    const trustedOrigins = this.officialMcpAuth?.trustedOrigins;
    if (!trustedOrigins) {
      return (() => {
        throw new OfficialMcpAuthError(
          "official_auth_unavailable",
          `official MCP trusted origin registry is not available in this runtime: ${serverName}`,
        );
      }) as unknown as typeof globalThis.fetch;
    }
    return createOfficialMcpAuthFetch({
      baseFetch: createMcpTransportFetch({ env: this.env, network: this.network }),
      official,
      onAuthFailure: (kind) => this.lastOfficialAuthKind.set(serverName, kind),
      onServerResponse: (response) => {
        if (this.isCurrentConnection(serverName, generation)) {
          this.rememberServerResponse(serverName, response);
        }
      },
      serverName,
      trustedOrigins,
      url: config.url,
      ...(authHeadersPort ? { authHeadersPort } : {}),
      ...(this.logger ? { logger: this.logger } : {}),
      ...(this.officialMcpAuth?.workspaceIdentity
        ? { workspaceIdentity: this.officialMcpAuth.workspaceIdentity }
        : {}),
      ...(this.workingDirectory ? { workspacePath: this.workingDirectory } : {}),
    });
  }

  /**
* Runtime auth provider.
   *
* This used to be useful for any HTTP/SSE MCP without an Authorization header
* Create a complete OAuth session - and the session `listen(0)` sets up a callback server before returning,
* Even if the credentials are completely valid and no authorization is required at all. At the same time, complete `OAuthClientProvider` will make 401 go to the SDK
* `auth()`, bypassing our refresh solo lock.
   *
* Now authorization_code always uses pure AuthProvider: passive connection zero listener, zero discovery, zero DCR,
* Interactive authorization only occurs in Phase 2 transactions.
   */
  private createOAuthClientProvider(
    serverName: string,
    config: McpServerConfig,
  ): AuthProvider | OAuthClientProvider | undefined {
    if (config.type === "stdio") return undefined;
//Official authentication and OAuth are mutually exclusive: the failure of official MCP can only be solved by ZCode login/package.
// Handing over any authProvider will cause 401 to be mistakenly converted to MCP authorization flow.
    if (isOfficialAuthConfig(config)) return undefined;
    const authorizationCodeOAuthConfig = resolveAuthorizationCodeOAuthConfig(config);
    if (authorizationCodeOAuthConfig) {
      return createMcpOAuthTokenProvider({
        config: authorizationCodeOAuthConfig,
        credentialStore: this.resolveCredentialStore(),
        fetchFn: createMcpTransportFetch({ env: this.env, network: this.network }),
        keyPrefix: createCredentialKeyPrefix(serverName, config.url, authorizationCodeOAuthConfig),
        ...(this.logger ? { logger: this.logger } : {}),
        serverName,
        serverUrl: config.url,
      });
    }
    if (config.oauth?.type === "client_credentials") {
      return new ClientCredentialsProvider({
        clientId: config.oauth.clientId,
        clientName: config.oauth.clientName ?? `${this.clientName}-${serverName}`,
        clientSecret: config.oauth.clientSecret,
        scope: config.oauth.scope,
      });
    }
    return undefined;
  }

  private resolveCredentialStore(): SharedZCodeCredentialStore {
    this.credentialStore ??= this.mcpOAuth?.credentialStore ?? createSharedZCodeCredentialStore();
    return this.credentialStore;
  }

  private createAuthorizationCodeOAuthOptions(
    config: McpServerConfig,
    serverName: string,
    generation: number,
    oauthAuthorizationTimeoutMs?: number,
  ): McpOAuthRuntimeOptions | undefined {
    if (config.type === "stdio") return this.mcpOAuth;
    return {
      ...this.mcpOAuth,
      authorizationTimeoutMs: oauthAuthorizationTimeoutMs ?? this.mcpOAuth?.authorizationTimeoutMs,
      onAuthorizationRequired: async (context) => {
        this.updateCurrentRecordStatus(serverName, generation, {
          authorization: createOAuthAuthorizationStatus(context),
          status: "connecting",
        });
        await this.mcpOAuth?.onAuthorizationRequired?.(context);
      },
    };
  }

  private attachStdioLogging(name: string, transport: McpTransport): () => string | undefined {
    const stderrBuffer = createBoundedTextBuffer(MCP_STDIO_STDERR_LOG_MAX_CHARS);
    const stderr = (
      transport as {
        stderr?: { on(event: "data", handler: (chunk: Buffer) => void): void };
      }
    ).stderr;
    stderr?.on("data", (chunk) => {
      const text = chunk.toString("utf8");
      stderrBuffer.append(text);
      this.logger?.debug("MCP stdio stderr", {
        event: "mcp.stdio.stderr",
        mcpServerName: name,
        stderr: sanitizeMcpStdioStderr(text).slice(0, MCP_STDIO_STDERR_LOG_MAX_CHARS),
      });
    });
    return () => {
      const text = stderrBuffer.read();
      if (!text) return undefined;
// The separate Connection closed in the production log cannot locate the reason why the stdio MCP child process exited.
// Only append the tail stderr to the failure event, and desensitize it first to avoid writing credentials or high-frequency output to the production log.
      return sanitizeMcpStdioStderr(text).slice(-MCP_STDIO_STDERR_LOG_MAX_CHARS);
    };
  }

  private async closeRecord(name: string): Promise<void> {
    const record = this.records.get(name);
    if (!record) return;
    record.abortController?.abort(new Error(`MCP server ${name} connection closed`));
    if (!record.client && !record.transport) return;
    await this.closeClientAndTransport(name, record.client, record.transport);
  }

  private async closeClientAndTransport(
    name: string,
    client?: McpClient,
    transport?: McpTransport,
  ): Promise<void> {
    const startedAt = Date.now();
    const mcpTransportPid = getStdioTransportPid(transport);
// Remove the connection_lost listener before actively closing to avoid normal recycling being mistakenly reported as accidental disconnection.
    if (client) client.onclose = undefined;
// MCP SDK close only ensures that the stdio child process exits directly, and the MCP server pulled up by npx/npm wrapper
// Or chrome-devtools-mcp watchdog may remain; here, first explicitly recycle according to the process tree, and then use SDK close to clean up the protocol status.
    await this.terminateStdioProcessTree(name, transport);

    try {
      await client?.close();
    } catch (error) {
      this.logger?.debug("MCP client close failed", {
        error: error instanceof Error ? error.message : String(error),
        event: "mcp.client.close.failed",
        mcpServerName: name,
      });
    }

    try {
      await transport?.close();
    } catch (error) {
      this.logger?.debug("MCP transport close failed", {
        error: error instanceof Error ? error.message : String(error),
        event: "mcp.transport.close.failed",
        mcpServerName: name,
      });
    }
    if (
      this.connectionContext &&
      transport instanceof ProcessTreeStdioClientTransport &&
      !transport.processAlive
    ) {
      this.telemetry?.recordProcessClosed({
        connectionId: this.connectionContext.mcpConnectionId,
      });
    }
    this.logger?.info("MCP server closed", {
      ...this.connectionContext,
      durationMs: Date.now() - startedAt,
      event: "mcp.server.closed",
      mcpServerName: name,
      ...(mcpTransportPid != null ? { mcpTransportPid } : {}),
      status: "completed",
    });
  }

  private async terminateStdioProcessTree(name: string, transport?: McpTransport): Promise<void> {
    const pid = getStdioTransportPid(transport);
    if (pid == null) return;

    try {
      await terminateMcpStdioProcessTree(pid);
    } catch (error) {
      this.logger?.warn("MCP stdio process tree cleanup failed", {
        ...this.connectionContext,
        error: error instanceof Error ? error.message : String(error),
        event: "mcp.stdio.process_tree_cleanup.failed",
        mcpServerName: name,
        mcpTransportPid: pid,
        pid,
        status: "failed",
      });
    }
  }

  private nextConnectionGeneration(name: string): number {
    const generation = (this.connectionGenerations.get(name) ?? 0) + 1;
    this.connectionGenerations.set(name, generation);
    return generation;
  }

  private isCurrentConnection(name: string, generation: number): boolean {
    return this.connectionGenerations.get(name) === generation;
  }

  private updateCurrentRecord(
    name: string,
    generation: number,
    patch: Partial<Pick<McpServerRecord, "client" | "transport">>,
  ): void {
    if (!this.isCurrentConnection(name, generation)) return;
    const record = this.records.get(name);
    if (!record) return;
    Object.assign(record, patch);
  }

  private updateCurrentRecordStatus(
    name: string,
    generation: number,
    patch: Partial<McpServerStatus>,
  ): void {
    if (!this.isCurrentConnection(name, generation)) return;
    const record = this.records.get(name);
    if (!record) return;
    record.status = {
      ...record.status,
      ...patch,
      updatedAt: new Date().toISOString(),
    };
  }

  private createStatus(
    config: McpServerConfig,
    status: McpServerStatus["status"],
    extra: {
      authorization?: McpServerStatus["authorization"];
      error?: string;
      failureKind?: McpServerStatus["failureKind"];
      protocolEra?: McpServerStatus["protocolEra"];
      serverRequestId?: string;
      toolCount?: number;
    } = {},
  ): McpServerStatus {
    return {
      status,
      transport: config.type,
      toolCount: extra.toolCount ?? 0,
      updatedAt: new Date().toISOString(),
      authorization: extra.authorization,
      error: extra.error,
      failureKind: extra.failureKind,
      protocolEra: extra.protocolEra,
      serverRequestId: extra.serverRequestId,
    };
  }
}

function mcpRequestMeta(request: McpCallToolRequest): Record<string, unknown> {
// nodeRepl.requestMeta exposed. All ZCode MCP servers can ignore these extended keys; node_repl browser
// bridge uses them as the only association basis to return to the current BrowserControlPort session. runtime_scope
// It cannot be guessed from the child session id, it must be explicitly passed through by the runtime of the actual execution tool.
  const requestContext = {
    ...(request.trace ? { trace_id: request.trace.traceId } : {}),
    ...(request.trace?.spanId ? { span_id: request.trace.spanId } : {}),
    ...(request.trace?.parentSpanId ? { parent_span_id: request.trace.parentSpanId } : {}),
    ...(request.trace?.sessionId ? { session_id: request.trace.sessionId } : {}),
    ...(request.trace?.turnId ? { turn_id: request.trace.turnId } : {}),
    ...(request.runtimeScope ? { runtime_scope: request.runtimeScope } : {}),
    ...(request.workspacePath ? { workspace_path: request.workspacePath } : {}),
    ...(request.workspaceIdentity ? { workspace_identity: request.workspaceIdentity } : {}),
    ...(request.workspaceKey ? { workspace_key: request.workspaceKey } : {}),
    ...(request.remoteSessionId ? { remote_session_id: request.remoteSessionId } : {}),
    ...(request.clientMode ? { client_mode: request.clientMode } : {}),
    ...(request.deliveryKind ? { delivery_kind: request.deliveryKind } : {}),
    ...(request.turnId && !request.trace?.turnId ? { turn_id: request.turnId } : {}),
  };
  return {
    ...requestContext,
    "com.zcode/request-context": requestContext,
  };
}

function resolveVersionNegotiationMode(config: McpServerConfig): VersionNegotiationMode {
  if (config.protocolVersion === "2026-07-28") return { pin: "2026-07-28" };
// The deprecated SSE transport itself only carries legacy era; explicit modern pins should still fail and not be downgraded silently.
  if (config.type === "sse") return "legacy";
  if (config.protocolVersion === "legacy") return "legacy";
  return "auto";
}

/**
* Stable identification of SDK version negotiation (auto/pin's server/discover probe) failure.
 *
* Structured judgment, prohibiting matching of wrong text:
* - `SdkError(SdkErrorCode.EraNegotiationFailed)`: probe hard failure (including non-standard legacy server)
* malformed 200 response, the transport layer Zod verification fails + normalizeReply falls into the network-error branch);
* - `UnsupportedProtocolVersionError`: recognized modern error, pin version is not accepted by the server.
 */
function isProtocolNegotiationFailure(error: unknown): boolean {
  if (SdkError.isInstance(error) && error.code === SdkErrorCode.EraNegotiationFailed) {
    return true;
  }
  if (UnsupportedProtocolVersionError.isInstance(error)) return true;
  const cause = (error as { cause?: unknown } | undefined)?.cause;
  if (cause !== undefined && cause !== error) {
    return isProtocolNegotiationFailure(cause);
  }
  return false;
}

function formatVersionNegotiationMode(config: McpServerConfig): string {
  const mode = resolveVersionNegotiationMode(config);
  return typeof mode === "object" ? mode.pin : mode;
}

function resolveVersionNegotiation(
  config: McpServerConfig,
  timeoutMs: number,
): VersionNegotiationOptions {
  const mode = resolveVersionNegotiationMode(config);
  if (mode === "legacy") return { mode };

// There is no legacy fallback for pin, probe is the only initialize path; use the 5 seconds of auto
// The upper limit of protection will ignore the server's long connection budget and silently remove node_repl from the tool pool if the cold start is normal but exceeds 5 seconds.
  const probeTimeoutMs =
    typeof mode === "object"
      ? Math.max(1, Math.floor(timeoutMs))
      : Math.min(MAX_MCP_VERSION_PROBE_TIMEOUT_MS, Math.max(1, Math.floor(timeoutMs / 2)));

  return {
    mode,
    probe: {
// Reason: SDK's stdio auto/pin will start disposable sibling first; if the SDK default value of 60s is used,
// ZCode's total connection timeout may end first and leave the probe remaining, without leaving any budget for legacy initialize.
      timeoutMs: probeTimeoutMs,
    },
  };
}

function createOAuthAuthorizationStatus(
  context: McpOAuthAuthorizationContext,
): NonNullable<McpServerStatus["authorization"]> {
  return {
    type: "oauth_authorization_code",
    authorizationUrl: context.authorizationUrl,
    startedAt: new Date().toISOString(),
  };
}

function resolveAuthorizationCodeOAuthConfig(
  config: McpServerConfig,
): AuthorizationCodeOAuthConfig | undefined {
  if (config.type === "stdio") return undefined;
//Official authentication and MCP OAuth are mutually exclusive. Must precede all existing branches:
// The official MCP neither writes the oauth field nor prohibits static authorization headers. If you don't short-circuit here, you will fall into
// The authorization_code below is a cover-up, causing the MCP authorization UI to pop up when 401 occurs - and the official authentication fails
// This can only be solved by ZCode's own login/package, and cannot be solved by the target MCP's OAuth authorization.
  if (isOfficialAuthConfig(config)) return undefined;
  if (config.oauth?.type === "authorization_code") return config.oauth;
  if (config.oauth?.type === "client_credentials") return undefined;
  if (hasAuthorizationHeader(config.headers)) return undefined;

// New HTTP/SSE MCP usually only saves the URL; OAuth support should be provided by the server
// WWW-Authenticate / discovery trigger, the oauth field cannot be required to be pre-written in the configuration.
  return {
    type: "authorization_code",
  };
}

/**
* True when auth.type/provider is accurately hit and provenance exists; missing provenance indicates that it is not a Plugin loader
* Output configuration.
 *
* Covers both http and stdio forms - the certificate delivery channels of the two are different, but "whether it is officially authenticated"
* is determined to have the same origin. If the caller only cares about a certain form, it needs to determine `config.type` by itself (such as createOfficialAuthFetch
* Only handles http, _meta injection only handles stdio).
 */
function isOfficialAuthConfig(config: McpServerConfig): boolean {
  return (
    (config.type === "http" || config.type === "stdio") &&
    config.auth?.type === "zcode_official" &&
    config.auth.provider === "jwt_token" &&
    config.official !== undefined
  );
}

function hasAuthorizationHeader(headers: Record<string, string> | undefined): boolean {
  if (!headers) return false;
  return Object.keys(headers).some((name) => name.toLowerCase() === "authorization");
}

function createBoundedTextBuffer(maxChars: number): {
  append(text: string): void;
  read(): string;
} {
  let value = "";
  return {
    append(text: string) {
      if (!text) return;
      value = `${value}${text}`;
      if (value.length > maxChars) {
        value = value.slice(-maxChars);
      }
    },
    read() {
      return value;
    },
  };
}

function sanitizeMcpStdioStderr(text: string): string {
  const sensitiveKey = String.raw`(?:api[_-]?key|access[_-]?key|secret(?:[_-]?key)?|private[_-]?key|token|password|passwd|pass|mysql_pass|mysql_password)`;
  let result = text.replace(/(bearer\s+)[^\s"']+/gi, "$1[Redacted]");
  result = result.replace(
    /(\bauthorization\b\s*[:=]\s*)(bearer\s+)?[^\r\n]+/gi,
    (_match, prefix: string, bearer: string | undefined) =>
      `${prefix}${bearer ? "Bearer " : ""}[Redacted]`,
  );
  result = result.replace(new RegExp(`([?&]${sensitiveKey}=)[^&\\s]+`, "gi"), "$1[Redacted]");
  result = result.replace(
    new RegExp(`(["']${sensitiveKey}["']\\s*:\\s*)(["'])(?:(?!\\2).)*\\2`, "gi"),
    "$1$2[Redacted]$2",
  );
  result = result.replace(
    new RegExp(`(\\b${sensitiveKey}\\b\\s*[:=]\\s*)(["']?)[^\\s"',;)}]+`, "gi"),
    "$1$2[Redacted]",
  );
  return result.replace(/([a-z][a-z0-9+.-]*:\/\/)[^:\s/@]+:[^@\s/]+@/gi, "$1[Redacted]@");
}

function getStdioTransportPid(transport?: McpTransport): number | undefined {
  if (!(transport instanceof StdioClientTransport)) return undefined;
  const pid = transport.pid;
  return typeof pid === "number" && Number.isInteger(pid) && pid > 0 ? pid : undefined;
}

function getStdioTransportExitInfo(transport?: McpTransport):
  | {
      exitCode: number | null;
      signal: NodeJS.Signals | null;
    }
  | undefined {
  return transport instanceof ProcessTreeStdioClientTransport ? transport.processExit : undefined;
}

function isStdioTransportProcessAlive(transport?: McpTransport): boolean {
  return transport instanceof ProcessTreeStdioClientTransport && transport.processAlive;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// The server responded with a JSON-RPC error response (numeric code) indicating that the request went through and the connection is alive;
// SDK local error (SdkError, string code: REQUEST_TIMEOUT / CONNECTION_CLOSED / NOT_CONNECTED
// / SEND_FAILED) means that the transport has been disconnected. Code type judgment covers the situation when instanceof fails in multiple SDK instances.
function isPeerAnsweredError(error: unknown): boolean {
  if (error instanceof ProtocolError) return true;
  return isRecord(error) && typeof error.code === "number";
}
