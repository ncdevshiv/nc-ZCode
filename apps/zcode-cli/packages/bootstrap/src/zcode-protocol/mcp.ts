import { createConfig, resolvePath } from "@zcode/adapters/config";
import type { Logger, McpConnectionSnapshot, McpPort, McpServerStatus } from "@zcode/contracts";
import {
  zcodeMcpListParamsSchema,
  zcodeMcpListResultSchema,
  type ZCodeMcpListResult,
} from "@zcode/shared";
import {
  listMcpServerStatuses,
  omitMcpServers,
  resolveTrustedOfficialCuaServerNames,
} from "../mcp-config.js";
import { StartupTimer, startupNow } from "../startup-logging.js";
import { getCliStorageRoot } from "../app/paths.js";
import { resolveStartupPlugins } from "../app/startup-marks.js";
import { protocolMcpServersToRuntimeMcpConfig } from "./protocol-mcp-config.js";
import { parseParams, type ZCodeProtocolAgentServerContext } from "./server-types.js";

const noopLogger: Logger = {
  debug: () => undefined,
  info: () => undefined,
  warn: () => undefined,
  error: () => undefined,
  child: () => noopLogger,
};

const MCP_OAUTH_AUTHORIZATION_STATUS_WAIT_MS = 5_000;
const MCP_OAUTH_AUTHORIZATION_STATUS_POLL_MS = 100;

export async function listMcpServers(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodeMcpListResult> {
  const params = parseParams(zcodeMcpListParamsSchema, rawParams);
  const workingDirectory = params.workspace.workspacePath;
  const configResult = createConfig({
    env: context.deps.env,
    workingDirectory,
  });
  const cliStorageRoot = getCliStorageRoot(resolvePath(configResult.config.storage.dir));
  const startupTimer = new StartupTimer(
    context.logger ?? noopLogger,
    {
      module: "bootstrap.zcode_protocol.mcp",
      workspaceKey: params.workspace.workspaceKey,
      workspacePath: params.workspace.workspacePath,
    },
    startupNow(),
  );
  const pluginOutcome = resolveStartupPlugins({
    cliStorageRoot,
    configResult,
    env: context.deps.env,
    logger: context.logger,
    options: {},
    startupTimer,
    workingDirectory,
  });
  const explicitMcpServersProvided = params.mcpServers !== undefined;
  const explicitRuntimeMcp = protocolMcpServersToRuntimeMcpConfig(params.mcpServers);
  const configuredMcpServers = {
    ...pluginOutcome.mcpServers,
// The local MCP list of the settings page is parsed by desktop main `.zcode` / `.agents` fallback,
// The session runtime also uses this batch of params.mcpServers. mcp/list can no longer rely solely on agent createConfig.
// Otherwise the `.agents` fallback line will be missing the status snapshot and will be mistakenly marked red by the UI.
    ...(explicitMcpServersProvided
      ? (explicitRuntimeMcp?.servers ?? {})
      : configResult.config.mcp.servers),
  };
  const trustedOfficialCuaServerNames = resolveTrustedOfficialCuaServerNames(
    configuredMcpServers,
    pluginOutcome.mcpServers,
  );
// Product decision workspace MCP is available out of the box: project scope MCP is trusted by default and connects automatically.
  const untrustedProjectMcpServers = new Set<string>();
  const mcpPort = context.deps.mcpPort;
  if (mcpPort) {
    if (params.mode === "status") {
// OAuth polling only needs to read the current running status. The subset passed in pending will fall into
// The replace semantics of connectConfiguredServers, causing unlisted MCPs to be disconnected.
      const statuses = await listMcpServerStatuses(
        mcpPort,
        configuredMcpServers,
        untrustedProjectMcpServers,
      );
      return zcodeMcpListResultSchema.parse({ statuses });
    }

// OAuth polling has been isolated by mode=status; default/connect must continue to perform replace convergence,
// Otherwise, when any server is waiting for authorization, configuration addition/deletion and stale MCP cleanup will be skipped.
//The default /connect mode serves "re-detection" requests such as setting page refresh, while mcpPort is a process-level
// `protocol-settings` lease; without revalidate, the connection pool will directly reuse the old entry and return the stale snapshot.
// A stopped HTTP MCP will always show connected (see adapters/src/mcp/pool.ts revalidateEntry).
    const connectPromise = mcpPort.connectConfiguredServers(
      omitMcpServers(
        configuredMcpServers,
        untrustedProjectMcpServers,
        trustedOfficialCuaServerNames,
      ),
      {
        revalidate: true,
        workingDirectory,
      },
    );
    const pendingAuthorizationSnapshot = await waitForOAuthAuthorizationSnapshot(
      mcpPort,
      connectPromise,
    );
    if (pendingAuthorizationSnapshot) {
      void connectPromise.catch((error) => {
        context.logger?.warn("MCP background authorization connection failed", {
          error: error instanceof Error ? error.message : String(error),
          event: "mcp.authorization.background.failed",
          workspaceKey: params.workspace.workspaceKey,
          workspacePath: params.workspace.workspacePath,
        });
      });
      return zcodeMcpListResultSchema.parse({
        statuses: pendingAuthorizationSnapshot.statuses,
      });
    }

    await connectPromise;
  }
  const statuses = await listMcpServerStatuses(
    mcpPort,
    configuredMcpServers,
    untrustedProjectMcpServers,
  );
  return zcodeMcpListResultSchema.parse({ statuses });
}

async function waitForOAuthAuthorizationSnapshot(
  mcpPort: McpPort,
  connectPromise: Promise<McpConnectionSnapshot>,
): Promise<McpConnectionSnapshot | undefined> {
  const startedAt = Date.now();
  while (Date.now() - startedAt < MCP_OAUTH_AUTHORIZATION_STATUS_WAIT_MS) {
    const race = await Promise.race([
      connectPromise.then(
        (snapshot) => ({ kind: "completed" as const, snapshot }),
        (error: unknown) => ({ kind: "failed" as const, error }),
      ),
      delay(MCP_OAUTH_AUTHORIZATION_STATUS_POLL_MS).then(() => ({ kind: "poll" as const })),
    ]);

    if (race.kind === "completed") {
      return undefined;
    }
    if (race.kind === "failed") {
      throw race.error;
    }

    const statuses = await mcpPort.status();
    if (hasPendingOAuthAuthorization(statuses)) {
      return {
        statuses,
        tools: await mcpPort.listTools(),
      };
    }
  }

  return undefined;
}

function hasPendingOAuthAuthorization(statuses: Record<string, McpServerStatus>): boolean {
  return Object.values(statuses).some((status) => Boolean(status.authorization?.authorizationUrl));
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
