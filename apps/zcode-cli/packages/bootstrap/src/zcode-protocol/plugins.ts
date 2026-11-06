/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import {
  zcodePluginsConfigureParamsSchema,
  zcodePluginsResetConfigParamsSchema,
  zcodePluginsInstallParamsSchema,
  zcodePluginsMarketplaceAddParamsSchema,
  zcodePluginsMarketplaceRemoveParamsSchema,
  zcodePluginsMarketplaceUpdateParamsSchema,
  zcodePluginsOverviewParamsSchema,
  zcodePluginsListParamsSchema,
  zcodePluginsSetEnabledParamsSchema,
  zcodePluginsUninstallParamsSchema,
  zcodePluginsUpdateParamsSchema,
  zcodePluginsValidateParamsSchema,
  zcodePluginsDescribeParamsSchema,
  zcodePluginsRestoreBuiltinParamsSchema,
  type ZCodeAvailablePluginSummary,
  type ZCodeInstalledPluginSummary,
  type ZCodePluginComponentGroup,
  type ZCodePluginDiagnostic,
  type ZCodePluginInfo,
  type ZCodePluginMarketplaceSummary,
  type ZCodePluginsConfigureResult,
  type ZCodePluginsDescribeResult,
  type ZCodePluginsInstallResult,
  type ZCodePluginsListResult,
  type ZCodePluginsMarketplaceMutationResult,
  type ZCodePluginsOverviewResult,
  type ZCodePluginsRestoreBuiltinResult,
  type ZCodePluginsSetEnabledResult,
  type ZCodePluginsUninstallResult,
  type ZCodePluginsValidateResult,
} from "@zcode/shared";
import type { PluginDiagnostic, PluginMetadata } from "@zcode/contracts";
import {
  addZCodePluginMarketplace,
  configureZCodePlugin,
  describeZCodePlugin,
  getZCodePluginsOverview,
  installZCodeMarketplacePlugin,
  removeZCodePluginMarketplace,
  resolveZCodePlugins,
  resetZCodePluginConfig,
  restoreBuiltinPlugin as restoreBuiltinPluginCore,
  setZCodePluginEnabled,
  uninstallZCodeMarketplacePlugin,
  updateZCodePluginMarketplace,
  validateZCodePlugin,
} from "../plugins.js";
import { listInstalledPluginRecords } from "@zcode/adapters/plugins";
import { withPluginStorageLock } from "../lib/plugin-storage-lock.js";
import { getCliStorageRoot, getPluginStorageRoot } from "../app/paths.js";
import { resolveOfficialPluginHostMcpServerNames } from "../app/official-plugin-definitions.js";
import { createConfig, resolvePath, type ConfigResult } from "@zcode/adapters/config";
import { parseParams, type ZCodeProtocolAgentServerContext } from "./server-types.js";

// Project the CLI's PluginMetadata into the protocol-serializable ZCodePluginInfo (retaining only the fields required by the UI).
function toPluginInfo(plugin: PluginMetadata, configResult?: ConfigResult): ZCodePluginInfo {
  const hostMcpServerNames = resolveOfficialPluginHostMcpServerNames(plugin.id);
  const configuredOptions = Object.fromEntries(
    Object.entries(plugin.configuredOptions ?? {}).filter(
      ([key]) => plugin.userConfig?.[key]?.sensitive !== true,
    ),
  );
  const enabledSource = configResult?.sources.plugins.enabled[plugin.id];
  const optionSources = configResult?.sources.plugins.options[plugin.id];
  const rootSource =
    plugin.source === "inline" && configResult
      ? resolveInlinePluginRootSource(plugin.rootPath, configResult)
      : undefined;
  return {
    id: plugin.id,
    name: plugin.name,
    ...(plugin.description !== undefined ? { description: plugin.description } : {}),
    ...(plugin.version !== undefined ? { version: plugin.version } : {}),
    enabled: plugin.enabled,
    source: plugin.source,
    marketplace: plugin.marketplace,
// Author/home page fallback field for manifest (store listing takes precedence).
    ...(plugin.author !== undefined ? { author: plugin.author } : {}),
    ...(plugin.authorUrl !== undefined ? { authorUrl: plugin.authorUrl } : {}),
    ...(plugin.homepage !== undefined ? { homepage: plugin.homepage } : {}),
    skillCount: plugin.skillCount,
    skillRootCount: plugin.skillRootCount,
    commandRootCount: plugin.commandRootCount,
// The list of authoritative components is delivered with the list, and the name + description is enumerated by the loader (independent of the enabled state) for direct display in the detailed UI.
    components: plugin.components.map((group) => ({
      kind: group.kind,
      items: group.items.map((item) => ({
        name: item.name,
        ...(item.description ? { description: item.description } : {}),
      })),
    })),
    declaredMcpServerNames: plugin.declaredMcpServerNames,
    mcpServerNames: plugin.mcpServerNames,
    ...(hostMcpServerNames.length > 0 ? { hostMcpServerNames } : {}),
    hookDetails: plugin.hookDetails,
    rootPath: plugin.rootPath,
    ...(plugin.userConfig ? { userConfig: plugin.userConfig } : {}),
    ...(Object.keys(configuredOptions).length > 0 ? { configuredOptions } : {}),
    ...(rootSource ? { rootSource } : {}),
    ...(enabledSource ? { enabledSource } : {}),
    ...(optionSources && Object.keys(optionSources).length > 0 ? { optionSources } : {}),
  };
}

function resolveInlinePluginRootSource(
  pluginRootPath: string,
  configResult: ConfigResult,
): "user" | "workspace" | undefined {
  const resolvedPluginRoot = normalizePluginRootForComparison(pluginRootPath);
// Workspace priority: When the same path appears in two-tier configurations at the same time, the project statement is the ownership evidence of the higher priority.
  if (
    configResult.sources.plugins.dirs.workspace.some(
      (rootPath) => normalizePluginRootForComparison(rootPath) === resolvedPluginRoot,
    )
  ) {
    return "workspace";
  }
  if (
    configResult.sources.plugins.dirs.user.some(
      (rootPath) => normalizePluginRootForComparison(rootPath) === resolvedPluginRoot,
    )
  ) {
    return "user";
  }
  return undefined;
}

function createPluginConfigView(
  context: ZCodeProtocolAgentServerContext,
  workspacePath: string,
  configScope: "user" | "workspace" | undefined,
): ConfigResult {
// User and Workspace of Settings are now two configuration views of the same batch of Host Plugin.
// If the User view continues to load project config, Workspace override will be projected into the current value of User;
// Not passing workingDirectory can retain the User/default layer, and it is still determined by the caller's workspacePath.
// package storage and relative execution context.
  return createConfig({
    env: context.deps?.env,
    ...(configScope === "user" ? {} : { workingDirectory: workspacePath }),
  });
}

function createMissingConfiguredPluginInfos(
  configResult: ConfigResult,
  discoveredPluginIds: ReadonlySet<string>,
): ZCodePluginInfo[] {
  const configuredPluginIds = new Set([
    ...Object.keys(configResult.config.plugins.enabledPlugins),
    ...Object.keys(configResult.config.plugins.options),
  ]);
  return [...configuredPluginIds].flatMap((pluginId) => {
    if (discoveredPluginIds.has(pluginId)) return [];
    const separatorIndex = pluginId.lastIndexOf("@");
    if (separatorIndex <= 0 || separatorIndex === pluginId.length - 1) {
      return [];
    }
    const enabledSource = configResult.sources.plugins.enabled[pluginId];
    const optionSources = configResult.sources.plugins.options[pluginId];
    return [
      {
        id: pluginId,
        name: pluginId.slice(0, separatorIndex),
        enabled: configResult.config.plugins.enabledPlugins[pluginId] ?? false,
        source: "missing",
        marketplace: pluginId.slice(separatorIndex + 1),
        skillCount: 0,
        skillRootCount: 0,
        commandRootCount: 0,
        components: [],
        declaredMcpServerNames: [],
        mcpServerNames: [],
        rootPath: "",
        packageStatus: "missing",
        ...(enabledSource ? { enabledSource } : {}),
        ...(optionSources && Object.keys(optionSources).length > 0 ? { optionSources } : {}),
      },
    ];
  });
}

function normalizePluginRootForComparison(
  rootPath: string,
  platform: NodeJS.Platform = process.platform,
): string {
  const resolvedRoot = resolvePath(rootPath);
// Windows paths are not case-sensitive, and the configuration and loader may return forward slashes and backslashes respectively.
// If string comparison is performed directly, the same Workspace plugins.dirs root will be misjudged as unowned.
  return platform === "win32" ? resolvedRoot.replaceAll("\\", "/").toLowerCase() : resolvedRoot;
}

function toPluginDiagnostic(diagnostic: PluginDiagnostic): ZCodePluginDiagnostic {
  return {
    code: diagnostic.code,
    message: diagnostic.message,
    severity: diagnostic.severity,
    ...(diagnostic.pluginId !== undefined ? { pluginId: diagnostic.pluginId } : {}),
  };
}

export async function listPlugins(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodePluginsListResult> {
  const params = parseParams(zcodePluginsListParamsSchema, rawParams);
  const configResult = createPluginConfigView(
    context,
    params.workspace.workspacePath,
    params.configScope,
  );
  const outcome = resolveZCodePlugins({
    configResult,
    logger: context.logger,
    workingDirectory: params.workspace.workspacePath,
  });
  const plugins = outcome.plugins.map((plugin) => toPluginInfo(plugin, configResult));
  return {
    plugins: [
      ...plugins,
      ...createMissingConfiguredPluginInfos(
        configResult,
        new Set(plugins.map((plugin) => plugin.id)),
      ),
    ],
    diagnostics: outcome.diagnostics.map(toPluginDiagnostic),
  };
}

export async function setPluginEnabled(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
  abortSignal?: AbortSignal,
): Promise<ZCodePluginsSetEnabledResult> {
  const params = parseParams(zcodePluginsSetEnabledParamsSchema, rawParams);
  abortSignal?.throwIfAborted();
  const result = await setZCodePluginEnabled({
    enabled: params.enabled,
    logger: context.logger,
    plugin: params.pluginId,
    scope: params.scope,
    workingDirectory: params.workspace.workspacePath,
  });
// Enable configuration writing is currently not rollable; if cancellation arrives during IO, only subsequent responses and UI writing will be blocked.
  abortSignal?.throwIfAborted();
  return {
    plugin: {
      ...toPluginInfo(result.plugin),
      enabledSource: params.scope ?? "user",
    },
    enabled: result.enabled,
  };
}

export async function getPluginsOverview(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodePluginsOverviewResult> {
  const params = parseParams(zcodePluginsOverviewParamsSchema, rawParams);
  const overview = getZCodePluginsOverview({
    configResult: createPluginConfigView(
      context,
      params.workspace.workspacePath,
      params.configScope,
    ),
    logger: context.logger,
    workingDirectory: params.workspace.workspacePath,
  });
  return {
    marketplaces: overview.marketplaces.map(toMarketplaceSummary),
    availablePlugins: overview.availablePlugins.map(toAvailablePluginSummary),
    installedPlugins: overview.installedPlugins.map(toInstalledPluginSummary),
    restorableBuiltins: overview.restorableBuiltins.map(toAvailablePluginSummary),
    diagnostics: overview.diagnostics.map(toPluginDiagnostic),
    capability: { supported: true },
  };
}

export async function addPluginMarketplace(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
  abortSignal?: AbortSignal,
): Promise<ZCodePluginsMarketplaceMutationResult> {
  const params = parseParams(zcodePluginsMarketplaceAddParamsSchema, rawParams);
  const pluginStorageRoot = resolvePluginStorageRoot(params.workspace.workspacePath);
  const marketplace = await withPluginStorageLock(pluginStorageRoot, async () =>
    addZCodePluginMarketplace({
      abortSignal,
      dryRun: params.dryRun,
      logger: context.logger,
      source: params.source,
      workingDirectory: params.workspace.workspacePath,
    }),
  );
  return { marketplace: toMarketplaceSummary(marketplace), diagnostics: [] };
}

export async function removePluginMarketplace(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodePluginsMarketplaceMutationResult> {
  const params = parseParams(zcodePluginsMarketplaceRemoveParamsSchema, rawParams);
  const pluginStorageRoot = resolvePluginStorageRoot(params.workspace.workspacePath);
  await withPluginStorageLock(pluginStorageRoot, async () =>
    removeZCodePluginMarketplace({
      logger: context.logger,
      marketplace: params.marketplace,
      workingDirectory: params.workspace.workspacePath,
    }),
  );
  return { diagnostics: [] };
}

export async function updatePluginMarketplace(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
  abortSignal?: AbortSignal,
): Promise<ZCodePluginsMarketplaceMutationResult> {
  const params = parseParams(zcodePluginsMarketplaceUpdateParamsSchema, rawParams);
  const pluginStorageRoot = resolvePluginStorageRoot(params.workspace.workspacePath);
  const result = await withPluginStorageLock(pluginStorageRoot, async () =>
    updateZCodePluginMarketplace({
      abortSignal,
      logger: context.logger,
      marketplace: params.marketplace,
      workingDirectory: params.workspace.workspacePath,
    }),
  );
  return {
    marketplaces: result.marketplaces.map(toMarketplaceSummary),
    diagnostics: result.diagnostics.map(toPluginDiagnostic),
  };
}

export async function installPlugin(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
  abortSignal?: AbortSignal,
): Promise<ZCodePluginsInstallResult> {
  const params = parseParams(zcodePluginsInstallParamsSchema, rawParams);
  const pluginStorageRoot = resolvePluginStorageRoot(params.workspace.workspacePath);
  const result = await withPluginStorageLock(pluginStorageRoot, async () =>
    installZCodeMarketplacePlugin({
      abortSignal,
      dryRun: params.dryRun,
      logger: context.logger,
      marketplace: params.marketplace,
      pluginName: params.pluginName,
      scope: params.scope,
      workingDirectory: params.workspace.workspacePath,
    }),
  );
  return {
    dependencyClosure: result.dependencyClosure,
    installedPlugins: result.installedPlugins.map(toInstalledPluginSummary),
    diagnostics: result.diagnostics.map(toPluginDiagnostic),
  };
}

export async function uninstallPlugin(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodePluginsUninstallResult> {
  const params = parseParams(zcodePluginsUninstallParamsSchema, rawParams);
  const removed = await uninstallZCodeMarketplacePlugin({
    logger: context.logger,
    marketplace: params.marketplace,
    pluginId: params.pluginId,
    pluginName: params.pluginName,
    removeCache: params.removeCache,
    workingDirectory: params.workspace.workspacePath,
  });
  return {
    ...(removed ? { removedPlugin: toInstalledPluginSummary(removed) } : {}),
    diagnostics: [],
  };
}

export async function updatePlugin(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodePluginsInstallResult> {
  const params = parseParams(zcodePluginsUpdateParamsSchema, rawParams);
  const pluginStorageRoot = resolvePluginStorageRoot(params.workspace.workspacePath);
  const installed = listInstalledPluginRecords(pluginStorageRoot).filter((record) => {
    if (params.pluginId) return record.id === params.pluginId;
    if (params.marketplace) return record.marketplace === params.marketplace;
    return true;
  });
//Serialize the entire reinstallation cycle into the in-process lock of the same storageRoot like uninstall.
// Avoid concurrent update/install interleaved reading and rewriting installed_plugins.json/cache.
  return withPluginStorageLock(pluginStorageRoot, async () => {
    const installedPlugins: ZCodeInstalledPluginSummary[] = [];
    const dependencyClosure: string[] = [];
// Aggregate the diagnosis generated by reinstallation of each record: do not throw an error when installZCodeMarketplacePlugin fails, but return
// PluginDiagnostic in CLI form (see toMarketplaceInstallDiagnostic in its error branch),
// Here, each item is projected to ZCodePluginDiagnostic via protocol side toPluginDiagnostic and returned.
// Make failed reinstalls explicit, rather than silently "successful".
    const diagnostics: ZCodePluginDiagnostic[] = [];
    for (const record of installed) {
      const result = await installZCodeMarketplacePlugin({
        logger: context.logger,
        marketplace: record.marketplace,
        pluginName: record.name,
        scope: record.scope,
        workingDirectory: params.workspace.workspacePath,
      });
      installedPlugins.push(...result.installedPlugins.map(toInstalledPluginSummary));
      dependencyClosure.push(...result.dependencyClosure);
      diagnostics.push(...result.diagnostics.map(toPluginDiagnostic));
    }
    return { dependencyClosure, installedPlugins, diagnostics };
  });
}

// Restore a suppressed ("uninstalled") builtin: clear the suppressedBuiltins flag and reseed immediately.
// The function with the same name on the bootstrap side is aliased as restoreBuiltinPluginCore to avoid duplication of names with this protocol processor.
export async function restoreBuiltinPlugin(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodePluginsRestoreBuiltinResult> {
  const params = parseParams(zcodePluginsRestoreBuiltinParamsSchema, rawParams);
  await restoreBuiltinPluginCore({
    logger: context.logger,
    pluginId: params.pluginId,
    workingDirectory: params.workspace.workspacePath,
  });
  return { pluginId: params.pluginId, diagnostics: [] };
}

export async function configurePlugin(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodePluginsConfigureResult> {
  const params = parseParams(zcodePluginsConfigureParamsSchema, rawParams);
  await configureZCodePlugin({
    clearOptionKeys: params.clearOptionKeys,
    dryRun: params.dryRun,
    logger: context.logger,
    options: params.options,
    pluginId: params.pluginId,
    scope: params.scope,
    workingDirectory: params.workspace.workspacePath,
  });
  return { pluginId: params.pluginId, diagnostics: [] };
}

export async function resetPluginConfig(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodePluginsConfigureResult> {
  const params = parseParams(zcodePluginsResetConfigParamsSchema, rawParams);
  await resetZCodePluginConfig({
    logger: context.logger,
    pluginId: params.pluginId,
    scope: params.scope,
    workingDirectory: params.workspace.workspacePath,
  });
  return { pluginId: params.pluginId, diagnostics: [] };
}

export async function validatePlugin(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodePluginsValidateResult> {
  const params = parseParams(zcodePluginsValidateParamsSchema, rawParams);
  const pluginStorageRoot = resolvePluginStorageRoot(params.workspace.workspacePath);
  const diagnostics = await withPluginStorageLock(pluginStorageRoot, async () =>
    validateZCodePlugin({
      logger: context.logger,
      marketplace: params.marketplace,
      pluginName: params.pluginName,
      source: params.source,
      workingDirectory: params.workspace.workspacePath,
    }),
  );
  return {
    ok: diagnostics.every((diagnostic) => diagnostic.severity !== "error"),
    diagnostics: diagnostics.map(toPluginDiagnostic),
    compatibility: {
      runnable: ["skills", "commands", "hooks", "mcpServers", "userConfig"],
      diagnosticOnly: ["agents", "lspServers", "outputStyles", "channels", "settings"],
      unsupported: ["mcpb", "dxt", "npm", "hostPattern", "pathPattern"],
    },
  };
}

export async function describePlugin(
  context: ZCodeProtocolAgentServerContext,
  rawParams: unknown,
): Promise<ZCodePluginsDescribeResult> {
  const params = parseParams(zcodePluginsDescribeParamsSchema, rawParams);
  const pluginStorageRoot = resolvePluginStorageRoot(params.workspace.workspacePath);
  const result = await withPluginStorageLock(pluginStorageRoot, async () =>
    describeZCodePlugin({
      logger: context.logger,
      marketplace: params.marketplace,
      pluginName: params.pluginName,
      workingDirectory: params.workspace.workspacePath,
    }),
  );
  const components: ZCodePluginComponentGroup[] = result.components.map((group) => ({
    kind: group.kind,
    items: group.items.map((item) => ({
      name: item.name,
      ...(item.description ? { description: item.description } : {}),
    })),
  }));
  const diagnostics = result.diagnostics.map(toPluginDiagnostic);
  return {
    components,
    ...(diagnostics.length > 0 ? { diagnostics } : {}),
    ...(result.metadata ? { metadata: result.metadata } : {}),
  };
}

function toMarketplaceSummary(input: {
  id: string;
  name: string;
  source: Record<string, unknown>;
  description?: string;
  lastUpdated?: string;
  pluginCount: number;
  isOfficial?: boolean;
  featured?: string[];
  refreshFailure?: ZCodePluginMarketplaceSummary["refreshFailure"];
}): ZCodePluginMarketplaceSummary {
  return {
    id: input.id,
    name: input.name,
    source: input.source,
    ...(input.description ? { description: input.description } : {}),
    ...(input.lastUpdated ? { lastUpdated: input.lastUpdated } : {}),
    pluginCount: input.pluginCount,
    ...(input.isOfficial !== undefined ? { isOfficial: input.isOfficial } : {}),
    ...(input.featured ? { featured: input.featured } : {}),
    ...(input.refreshFailure ? { refreshFailure: input.refreshFailure } : {}),
  };
}

function toAvailablePluginSummary(input: {
  id: string;
  name: string;
  marketplace: string;
  description?: string;
  version?: string;
  installed: boolean;
  componentTypes?: string[];
  listing?: ZCodeAvailablePluginSummary["listing"];
}): ZCodeAvailablePluginSummary {
  return {
    id: input.id,
    name: input.name,
    marketplace: input.marketplace,
    ...(input.description ? { description: input.description } : {}),
    ...(input.version ? { version: input.version } : {}),
    installed: input.installed,
    ...(input.componentTypes ? { componentTypes: input.componentTypes } : {}),
    ...(input.listing ? { listing: input.listing } : {}),
  };
}

function toInstalledPluginSummary(input: {
  id: string;
  name: string;
  marketplace: string;
  description?: string;
  version?: string;
  enabled: boolean;
  scope: "user" | "workspace";
  installPath?: string;
  installedAt?: string;
  componentTypes?: string[];
  hookDetails?: ZCodeInstalledPluginSummary["hookDetails"];
  updateStatus?: "none" | "update-available" | "version-changed";
  latestVersion?: string;
  listing?: ZCodeInstalledPluginSummary["listing"];
}): ZCodeInstalledPluginSummary {
  return {
    id: input.id,
    name: input.name,
    marketplace: input.marketplace,
    ...(input.description ? { description: input.description } : {}),
    ...(input.version ? { version: input.version } : {}),
    enabled: input.enabled,
    scope: input.scope,
    ...(input.installPath ? { installPath: input.installPath } : {}),
    ...(input.installedAt ? { installedAt: input.installedAt } : {}),
    ...(input.componentTypes ? { componentTypes: input.componentTypes } : {}),
    ...(input.hookDetails ? { hookDetails: input.hookDetails } : {}),
    ...(input.updateStatus ? { updateStatus: input.updateStatus } : {}),
    ...(input.latestVersion ? { latestVersion: input.latestVersion } : {}),
    ...(input.listing ? { listing: input.listing } : {}),
  };
}

function resolvePluginStorageRoot(workingDirectory: string): string {
  const config = createConfig({ workingDirectory });
  const storageRoot = resolvePath(config.config.storage.dir);
  return getPluginStorageRoot(getCliStorageRoot(storageRoot));
}
