/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import type { PluginDiagnostic, PluginManifest, PluginStoreListing } from "@zcode/contracts";
import { isOfficialMarketplaceId, ZCODE_OFFICIAL_PLUGIN_MARKETPLACE } from "@zcode/contracts";
import { DEFAULT_PLUGIN_MARKETPLACES, sanitizeZCodeRuntimeEnv } from "@zcode/shared";
import { loadPluginMcpServerDefinitions, resolvePluginMcpServers } from "./mcp.js";
import {
  appendPluginSourceCleanupError,
  cleanupPluginSourceBestEffort,
  directoryExists,
  fileExists,
  isRecord,
  resolveInside,
  sanitizePluginId,
} from "./helpers.js";
import { enumeratePluginComponents, type PluginComponentGroup } from "./plugin-components.js";
import { applyNetworkEgressEnv } from "../network/subprocess-env.js";
import { createNodeWebFetchHttpClientAdapter } from "../http/index.js";
import { writeCdnOfficialMarketplacePartitionSync } from "./official-marketplace.js";
import {
  isZipPluginUrlSource,
  readZipPluginSourceSha256,
  resolveZipPluginSource,
} from "./zip-source.js";
import {
  activateDirectoryAtomically,
  recoverAtomicTargetSync,
  writeFileAtomically,
  type AtomicDirectoryActivation,
} from "./atomic-directory.js";
import {
  resolveGitHubArchiveSource,
  shouldFallbackGitHubArchiveToGit,
} from "./github-archive-source.js";
import {
  createArchiveFetchError,
  createGitUnavailableError,
  getPluginSourceDiagnosticCode,
  isCommandUnavailableError,
} from "./source-errors.js";

const execFileAsync = promisify(execFile);
const KNOWN_MARKETPLACES_FILE = "known_marketplaces.json";
const INSTALLED_PLUGINS_FILE = "installed_plugins.json";
const MARKETPLACE_FILE = "marketplace.json";
const MARKETPLACE_JSON_MAX_BYTES = 10 * 1024 * 1024;
const MARKETPLACE_JSON_MAX_REDIRECTS = 5;
const MARKETPLACE_JSON_TIMEOUT_MS = 180_000;
const CLAUDE_MARKETPLACE_FILE = join(".claude-plugin", "marketplace.json");
const ZCODE_MANIFEST_PATH = join(".zcode-plugin", "plugin.json");
const CLAUDE_MANIFEST_PATH = join(".claude-plugin", "plugin.json");
const CODEX_MANIFEST_PATH = join(".codex-plugin", "plugin.json");
const DEFAULT_VERSION = "0.0.0";
const GIT_CLONE_MAX_ATTEMPTS = 3;
const GIT_COMMAND_TIMEOUT_MS = 90_000;
const GIT_CLONE_RETRY_DELAY_MS = 1_000;
const MARKETPLACE_NAME_PATTERN = /^[a-z0-9][a-z0-9._-]{0,127}$/;
const PLUGIN_NAME_PATTERN = /^[a-z0-9][a-z0-9._-]{0,127}$/;
const SOURCE_SHA256_PATTERN = /^[a-f0-9]{64}$/u;
const UNSUPPORTED_MANIFEST_FIELDS = ["channels", "lspServers", "outputStyles", "settings"] as const;

export type MarketplaceSource =
  | { source: "url"; headers?: Record<string, string>; url: string }
  | { path?: string; ref?: string; repo: string; source: "github"; sparsePaths?: string[] }
  | { path?: string; ref?: string; source: "git"; sparsePaths?: string[]; url: string }
  | { package: string; source: "npm" }
  | { source: "file"; path: string }
  | { source: "directory"; path: string }
  | { hostPattern: string; source: "hostPattern" }
  | { pathPattern: string; source: "pathPattern" }
  | { source: "settings"; marketplace: PluginMarketplaceManifest };

export interface PluginMarketplaceEntry {
  name: string;
  category?: string;
  description?: string;
  version?: string;
  source?: unknown;
// The absolute path of the cache directory written when the official plug-in seed is built in (source is "filesystem"/"sea").
// describe/When parsing, you can directly locate the root directory of the plug-in that has been placed on the disk. There is no need to parse source as a path.
  cachePath?: string;
  dependencies?: string[];
  strict?: boolean;
  tags?: string[];
// Store information (displayName/icon/hero/example prompt words/links and other display metadata), parsed from the entry raw;
// All optional, see contracts PluginStoreListing.
  listing?: PluginStoreListing;
  raw: Record<string, unknown>;
}

export interface PluginMarketplaceManifest {
  name: string;
  description?: string;
  plugins: PluginMarketplaceEntry[];
  allowCrossMarketplaceDependenciesOn?: string[];
  pluginRoot?: string;
// The curated list (plugin name, in order) of the Featured section of the "public" section of the store; remotely controlled by the featured field at the top of the directory JSON.
  featured?: string[];
  raw: Record<string, unknown>;
}

export interface KnownMarketplaceRecord {
  id: string;
  source: MarketplaceSource;
  name: string;
  description?: string;
  addedAt: string;
  lastUpdated?: string;
  lastRefreshFailure?: MarketplaceRefreshFailure;
  pluginCount: number;
/** Internal crash recovery generational; protocol/UI projections not exposed. */
  cacheTransactionId?: string;
}

export interface MarketplaceRefreshFailure {
  code: PluginDiagnostic["code"];
  failedAt: string;
  message: string;
}

export interface InstalledPluginRecord {
  id: string;
  name: string;
  marketplace: string;
  version: string;
  installPath: string;
  installedAt: string;
  updatedAt?: string;
  scope: "user" | "workspace";
  dependencies?: string[];
  source?: unknown;
/** Internal crash recovery generational; protocol/UI projections not exposed. */
  cacheTransactionId?: string;
}

interface InstalledPluginsState {
  version: 1;
  plugins: InstalledPluginRecord[];
}

interface MarketplaceInstallResult {
  closure: string[];
  installed: InstalledPluginRecord[];
}

export interface PluginValidationDiagnostic {
  code: PluginDiagnostic["code"];
  message: string;
  path?: string;
  pluginId?: string;
  severity: PluginDiagnostic["severity"];
}

// The type of component enumeration is defined in plugin-components.ts; it is exported here to keep the external contract of adapter barrel stable.
export type {
  PluginComponentGroup,
  PluginComponentItem,
  PluginComponentKind,
} from "./plugin-components.js";

export interface DescribeMarketplacePluginResult {
  components: PluginComponentGroup[];
  diagnostics: PluginValidationDiagnostic[];
// The display fallback field (author/homepage/version) of plugin.json in the plug-in package; it is used in the details page information area when the store information is missing.
  metadata?: PluginManifestDisplayMetadata;
}

export interface PluginManifestDisplayMetadata {
  author?: string;
  authorUrl?: string;
  homepage?: string;
  version?: string;
}

function buildMarketplaceGitEnv(
  sourceEnv: NodeJS.ProcessEnv = process.env,
): Record<string, string> {
  const env = sanitizeZCodeRuntimeEnv(sourceEnv);
// The marketplace installation will start the Git child process and cannot only rely on the shell agent inherited by the parent process.
// Here, HTTP(S)/NO_PROXY/CA is restored uniformly from the ZCode explicit network environment to avoid the installation button being stuck until the protocol times out.
  return applyNetworkEgressEnv(env, { sourceEnv });
}

function throwIfPluginOperationAborted(signal?: AbortSignal): void {
  if (signal?.aborted) {
    throw createPluginOperationCancelledError();
  }
}

function createPluginOperationCancelledError(): Error {
  const error = new Error("Plugin operation cancelled");
  error.name = "AbortError";
  return error;
}

interface KnownMarketplaceActivation {
  finalize: () => void;
  rollback: () => Promise<void>;
}

interface LoadMarketplaceResult {
  cleanup?: () => Promise<void>;
  manifest: PluginMarketplaceManifest;
  sourceRoot?: string;
}

interface ResolvedPluginSourceRoot {
  cleanup?: () => Promise<void>;
  path: string;
}

interface CachedMarketplacePluginResult {
  activation?: AtomicDirectoryActivation;
  record: InstalledPluginRecord;
}

export async function parseMarketplaceSourceInput(input: string): Promise<MarketplaceSource> {
  const trimmed = input.trim();
  if (trimmed.length === 0) {
    throw new Error("Marketplace source is empty");
  }

  if (trimmed.startsWith("http://") || trimmed.startsWith("https://")) {
    const { url, ref } = splitRef(trimmed);
    if (url.endsWith(".git") || url.includes("/_git/")) {
      return ref ? { source: "git", url, ref } : { source: "git", url };
    }
    const parsed = tryParseUrl(url);
    if (parsed && (parsed.hostname === "github.com" || parsed.hostname === "www.github.com")) {
      const match = parsed.pathname.match(/^\/([^/]+\/[^/]+?)(?:\/|\.git|$)/);
      if (match?.[1]) {
        const gitUrl = url.endsWith(".git") ? url : `${url}.git`;
        return ref ? { source: "git", url: gitUrl, ref } : { source: "git", url: gitUrl };
      }
    }
    return { source: "url", url };
  }

  if (isGitSshUrl(trimmed)) {
    const { url, ref } = splitRef(trimmed);
    return ref ? { source: "git", url, ref } : { source: "git", url };
  }

  const resolved = resolvePathInput(trimmed);
  if (resolved) {
    if (!existsSync(resolved))
      throw new Error(`Marketplace source path does not exist: ${resolved}`);
    if (fileExists(resolved)) {
      if (!resolved.endsWith(".json")) {
        throw new Error(`Marketplace file must be a .json file: ${resolved}`);
      }
      return { source: "file", path: resolved };
    }
    if (directoryExists(resolved)) return { source: "directory", path: resolved };
    throw new Error(`Marketplace source path is not a file or directory: ${resolved}`);
  }

  if (trimmed.includes("/") && !trimmed.includes(":")) {
    const { url: repo, ref } = splitGitHubShorthand(trimmed);
    return ref ? { source: "github", repo, ref } : { source: "github", repo };
  }

  throw new Error(`Unsupported marketplace source: ${input}`);
}

export function loadKnownMarketplacesSync(storageRoot: string): KnownMarketplaceRecord[] {
  const parsed = readJsonFileSync(join(storageRoot, KNOWN_MARKETPLACES_FILE));
  if (!isRecord(parsed)) return [];
  const value = parsed.marketplaces;
  if (Array.isArray(value)) return value.filter(isKnownMarketplaceRecord);
  if (isRecord(value)) return Object.values(value).filter(isKnownMarketplaceRecord);
  return [];
}

export function ensureDefaultPluginMarketplaces(storageRoot: string): KnownMarketplaceRecord[] {
  const known = loadKnownMarketplacesSync(storageRoot);
  const existingIds = new Set(known.map((record) => record.id));
  const now = new Date().toISOString();
  const missing = DEFAULT_PLUGIN_MARKETPLACES.filter(
    (marketplace) => !existingIds.has(marketplace.id),
  ).map(
    (marketplace): KnownMarketplaceRecord => ({
      id: marketplace.id,
      source: defaultMarketplaceSourceFromString(marketplace.source),
      name: marketplace.name,
      description: marketplace.description,
      addedAt: now,
      ...(marketplace.lastUpdated ? { lastUpdated: marketplace.lastUpdated } : {}),
      pluginCount: marketplace.pluginCount,
    }),
  );
  if (missing.length === 0) return known;
  const next = [...known, ...missing];
  writeKnownMarketplacesSync(storageRoot, next);
  return next;
}

export async function ensureMarketplaceManifestAvailable(input: {
  marketplace: string;
  signal?: AbortSignal;
  storageRoot: string;
}): Promise<KnownMarketplaceRecord | null> {
  throwIfPluginOperationAborted(input.signal);
  ensureDefaultPluginMarketplaces(input.storageRoot);
  if (loadMarketplaceManifestSync(input.storageRoot, input.marketplace)) {
    return (
      loadKnownMarketplacesSync(input.storageRoot).find(
        (record) => record.id === input.marketplace,
      ) ?? null
    );
  }
  const record = loadKnownMarketplacesSync(input.storageRoot).find(
    (item) => item.id === input.marketplace,
  );
  if (!record) return null;
// Trusted internal lazy loading: use the specification source of known record to pull, and use record.id as trustedId,
// Make the official id can only be obtained by refreshing the record that is the official id.
  return await addMarketplace({
    signal: input.signal,
    source: record.source,
    storageRoot: input.storageRoot,
    trustedId: record.id,
  });
}

export async function addMarketplace(input: {
  expectedId?: string;
  signal?: AbortSignal;
  source: MarketplaceSource;
  storageRoot: string;
// Trusted internal refresh passes in the canonical id of the known record being refreshed. The guard only declares the official id in the manifest
//Reject if the id is not equal to the trustedId of this refresh, to prevent the source from being renamed and used fraudulently during the refresh process:
// - Newly added (trustedId is missing) statement on the user side to declare official id → Reject;
// - The unofficial market will rename the manifest to the official id in the future, and the trustedId does not match when refreshing → reject;
// Unofficial manifest names are not subject to this restriction and maintain the existing behavior.
  trustedId?: string;
}): Promise<KnownMarketplaceRecord> {
// persist:false only parses the manifest first and does not save it to disk - otherwise the marketplace directory activation will be used
// Untrusted manifest.name as target, first rm to remove the local official directory and then cp, wait until the guard throws an error
// The official manifest has been tainted; it is persisted only after the guard has passed.
  throwIfPluginOperationAborted(input.signal);
  const operationSignal = input.signal;
  let loaded: LoadMarketplaceResult | undefined;
  let knownMarketplaceActivation: KnownMarketplaceActivation | undefined;
  let marketplaceActivation: AtomicDirectoryActivation | undefined;
  try {
    loaded = await loadMarketplaceFromSource(input.source, input.storageRoot, {
      persist: false,
      signal: operationSignal,
    });
    throwIfPluginOperationAborted(operationSignal);
    if (isOfficialMarketplaceId(loaded.manifest.name) && loaded.manifest.name !== input.trustedId) {
      throw new Error(
        `Cannot add a marketplace named "${loaded.manifest.name}": that id is reserved for the official marketplace.`,
      );
    }
    if (input.expectedId && loaded.manifest.name !== input.expectedId) {
      throw new Error(
        `Marketplace declaration id mismatch: expected ${input.expectedId}, received ${loaded.manifest.name}`,
      );
    }
    if (
      input.trustedId === ZCODE_OFFICIAL_PLUGIN_MARKETPLACE &&
      loaded.manifest.name !== ZCODE_OFFICIAL_PLUGIN_MARKETPLACE
    ) {
      throw new Error(
        `Official marketplace source must provide ${ZCODE_OFFICIAL_PLUGIN_MARKETPLACE}, received ${loaded.manifest.name}`,
      );
    }
    const persistedManifest =
      loaded.manifest.name === ZCODE_OFFICIAL_PLUGIN_MARKETPLACE
        ? parseRequiredMarketplaceManifest(
            writeCdnOfficialMarketplacePartitionSync({
              manifest: loaded.manifest.raw,
              storageRoot: input.storageRoot,
            }),
          )
        : loaded.manifest;
// The old process first deletes the marketplace target and then copies the source. If the refresh fails, the last successful snapshot will be lost.
// The source tree and the canonical manifest are renamed and activated once in the same staging directory after preparation.
    if (loaded.sourceRoot) {
      marketplaceActivation = await stageMarketplaceDirectoryPlugins(
        loaded.sourceRoot,
        input.storageRoot,
        loaded.manifest.name,
        persistedManifest.raw,
        operationSignal,
      );
    } else if (loaded.manifest.name !== ZCODE_OFFICIAL_PLUGIN_MARKETPLACE) {
      marketplaceActivation = await stageMarketplaceManifest(
        input.storageRoot,
        loaded.manifest.name,
        loaded.manifest.raw,
        operationSignal,
      );
    }
    throwIfPluginOperationAborted(operationSignal);
    const now = new Date().toISOString();
    const record: KnownMarketplaceRecord = {
      id: loaded.manifest.name,
      source: input.source,
      name: loaded.manifest.name,
      ...(loaded.manifest.description ? { description: loaded.manifest.description } : {}),
      addedAt: now,
      lastUpdated: now,
      pluginCount: persistedManifest.plugins.length,
      ...(marketplaceActivation ? { cacheTransactionId: marketplaceActivation.transactionId } : {}),
    };
    knownMarketplaceActivation = await upsertKnownMarketplace(input.storageRoot, record);
    if (marketplaceActivation) {
      throwIfPluginOperationAborted(operationSignal);
    }
// After the authority state has been placed, it enters the non-cancelable submission end, and then cleans up the backup/marker.
    await marketplaceActivation?.finalize();
    knownMarketplaceActivation.finalize();
    return record;
  } catch (error) {
    let rollbackError: unknown;
    try {
      await knownMarketplaceActivation?.rollback();
    } catch (currentRollbackError) {
      rollbackError = currentRollbackError;
    }
    try {
      if (rollbackError === undefined) {
        await marketplaceActivation?.rollback();
      } else {
// When the authority cannot be restored, retain the new manifest it points to to avoid creating a cross-generation state again.
        await marketplaceActivation?.finalize();
      }
    } catch (currentRollbackError) {
      rollbackError =
        rollbackError === undefined
          ? currentRollbackError
          : appendPluginSourceCleanupError(currentRollbackError, rollbackError);
    }
    throw appendPluginSourceCleanupError(error, rollbackError);
  } finally {
    await cleanupPluginSourceBestEffort(loaded?.cleanup);
  }
}

async function requestMarketplaceJson(
  url: string,
  headers?: Record<string, string>,
  signal?: AbortSignal,
  timeoutMs = MARKETPLACE_JSON_TIMEOUT_MS,
): Promise<unknown> {
  const client = createNodeWebFetchHttpClientAdapter({
    env: process.env,
    maxResponseBytes: MARKETPLACE_JSON_MAX_BYTES,
    timeoutMs,
  });
  let currentHeaders = headers;
  let currentUrl = url;
  for (let redirectCount = 0; redirectCount <= MARKETPLACE_JSON_MAX_REDIRECTS; redirectCount += 1) {
    const response = await client.request(
      {
        ...(currentHeaders ? { headers: currentHeaders } : {}),
        method: "GET",
        redirect: "manual",
        url: currentUrl,
      },
      { signal },
    );
    if (isMarketplaceJsonRedirectStatus(response.status)) {
      const location = response.headers.location;
      if (!location) {
        throw new Error(`Marketplace redirect is missing Location header: ${currentUrl}`);
      }
      const redirectUrl = new URL(location, currentUrl);
// The http.request of the proxy/custom CA branch will not execute redirect:follow;
// Here, unified bounded follow-up is implemented, and market custom headers are cleaned up when crossing origins to avoid leaking credentials to CDN.
      if (redirectUrl.origin !== new URL(currentUrl).origin) {
        currentHeaders = undefined;
      }
      currentUrl = redirectUrl.toString();
      continue;
    }
    if (response.status < 200 || response.status >= 300) {
      throw new Error(`Failed to fetch marketplace: ${response.status} ${response.statusText}`);
    }
    return JSON.parse(new TextDecoder().decode(response.body)) as unknown;
  }
  throw new Error(`Marketplace fetch exceeded redirect limit: ${url}`);
}

function isMarketplaceJsonRedirectStatus(status: number): boolean {
  return status === 301 || status === 302 || status === 303 || status === 307 || status === 308;
}

export async function updateMarketplace(input: {
  marketplace?: string;
  signal?: AbortSignal;
  storageRoot: string;
}): Promise<KnownMarketplaceRecord[]> {
  ensureDefaultPluginMarketplaces(input.storageRoot);
  const known = loadKnownMarketplacesSync(input.storageRoot);
  const selected = input.marketplace
    ? known.filter((record) => record.id === input.marketplace)
    : known;
  if (input.marketplace && selected.length === 0) {
    throw new Error(`Marketplace not found: ${input.marketplace}`);
  }
  const updated: KnownMarketplaceRecord[] = [];
  for (const record of selected) {
    throwIfPluginOperationAborted(input.signal);

// Trusted refresh will re-pull the source that comes with the known marketplace; record.id is used as trustedId,
// Make the official id can only be obtained by refreshing the record that originally contains the id.
    try {
      updated.push(
        await addMarketplace({
          signal: input.signal,
          source: record.source,
          storageRoot: input.storageRoot,
          trustedId: record.id,
        }),
      );
    } catch (error) {
// Cancellation is the control flow of the current operation, not the Marketplace health state; AbortError must not be
// Persistence becomes a refresh failure to avoid false reporting of official source failures on subsequent ordinary store pages.
      if (input.signal?.aborted) throw error;
      const diagnostic = toValidationDiagnostic(error, record.id);
      await persistMarketplaceRefreshFailure(input.storageRoot, record.id, {
        code: diagnostic.code,
        failedAt: new Date().toISOString(),
        message: diagnostic.message,
      });
    }
  }
  return updated;
}

export async function removeMarketplace(input: {
  marketplace: string;
  storageRoot: string;
}): Promise<void> {
  const known = loadKnownMarketplacesSync(input.storageRoot).filter(
    (record) => record.id !== input.marketplace,
  );
  await writeKnownMarketplaces(input.storageRoot, known);
}

export function loadMarketplaceManifestSync(
  storageRoot: string,
  marketplace: string,
): PluginMarketplaceManifest | null {
  const manifestPath = getMarketplaceManifestPath(storageRoot, marketplace);
// The crash residue is restored first; if the writer is still active, the backup is read before the authoritative known state is placed on disk.
// Read the new target after placement to avoid the overview seeing cross-generation manifest/summary.
  const readableDirectory = recoverAtomicTargetSync(dirname(manifestPath));
  const parsed = readJsonFileSync(join(readableDirectory, basename(manifestPath)));
  return parseMarketplaceManifest(parsed);
}

function loadInstalledPluginsSync(storageRoot: string): InstalledPluginsState {
  const parsed = readJsonFileSync(join(storageRoot, INSTALLED_PLUGINS_FILE));
  return normalizeInstalledPluginsState(parsed);
}

async function saveInstalledPlugins(
  storageRoot: string,
  state: InstalledPluginsState,
): Promise<void> {
  await writeJsonFile(join(storageRoot, INSTALLED_PLUGINS_FILE), state);
}

export function listInstalledPluginRecords(storageRoot: string): InstalledPluginRecord[] {
  return loadInstalledPluginsSync(storageRoot).plugins;
}

export function resolveInstalledPluginRoot(
  storageRoot: string,
  record: InstalledPluginRecord,
): string {
  const root =
    record.installPath ||
    getPluginCacheDir(storageRoot, record.marketplace, record.name, record.version);
  return recoverAtomicTargetSync(root);
}

export async function installMarketplacePlugin(input: {
  marketplace: string;
  name: string;
  signal?: AbortSignal;
  storageRoot: string;
  scope?: "user" | "workspace";
  allowCrossMarketplaces?: ReadonlySet<string>;
}): Promise<MarketplaceInstallResult> {
  await ensureMarketplaceManifestAvailable({
    marketplace: input.marketplace,
    signal: input.signal,
    storageRoot: input.storageRoot,
  });
  throwIfPluginOperationAborted(input.signal);
  const rootManifest = loadMarketplaceManifestSync(input.storageRoot, input.marketplace);
  const closure = resolveDependencyClosure({
    allowCrossMarketplaces:
      input.allowCrossMarketplaces ??
      new Set(rootManifest?.allowCrossMarketplaceDependenciesOn ?? []),
    marketplace: input.marketplace,
    name: input.name,
    storageRoot: input.storageRoot,
  });
  const state = loadInstalledPluginsSync(input.storageRoot);
  const installed: InstalledPluginRecord[] = [];
  const activations: AtomicDirectoryActivation[] = [];
  try {
    for (const pluginId of closure) {
      const { marketplace, name } = parsePluginId(pluginId);
      const manifest = loadMarketplaceManifestSync(input.storageRoot, marketplace);
      if (!manifest) throw new Error(`Marketplace not found: ${marketplace}`);
      const entry = manifest.plugins.find((plugin) => plugin.name === name);
      if (!entry) throw new Error(`Plugin not found: ${pluginId}`);
      const cached = await cacheMarketplacePlugin({
        entry,
        marketplace,
        signal: input.signal,
        scope: input.scope ?? "user",
        state,
        storageRoot: input.storageRoot,
      });
      installed.push(cached.record);
      if (cached.activation) activations.push(cached.activation);
    }
    throwIfPluginOperationAborted(input.signal);
    await saveInstalledPlugins(input.storageRoot, state);
  } catch (error) {
    let rollbackError: unknown;
    for (const activation of activations.reverse()) {
      try {
        await activation.rollback();
      } catch (currentRollbackError) {
        rollbackError ??= currentRollbackError;
      }
    }
    throw appendPluginSourceCleanupError(error, rollbackError);
  }
  for (const activation of activations) await activation.finalize();
  return { closure, installed };
}

export async function uninstallMarketplacePlugin(input: {
  pluginId: string;
  storageRoot: string;
  removeCache?: boolean;
/** `zcode plugins uninstall --keep-data`: Delete the installation cache but keep the data/<plugin-id> user data directory. */
  keepData?: boolean;
}): Promise<InstalledPluginRecord | null> {
  const state = loadInstalledPluginsSync(input.storageRoot);
  const index = state.plugins.findIndex((record) => record.id === input.pluginId);
  if (index < 0) return null;
  const [removed] = state.plugins.splice(index, 1);
  await saveInstalledPlugins(input.storageRoot, state);
  if (removed && input.removeCache === true) {
    await rm(removed.installPath, { force: true, recursive: true });
// Complete uninstall: data/<plugin-id> is the persistent per-plugin directory (including materialize's generated-commands).
// According to the semantics of "delete when uninstalling the last installation", ensure that the reinstallation is clean.

    if (input.keepData !== true) {
      await rm(getPluginDataDir(input.storageRoot, removed.id), { force: true, recursive: true });
    }
  }
  return removed ?? null;
}

export async function validateMarketplacePlugin(input: {
  marketplace: string;
  name: string;
  storageRoot: string;
}): Promise<PluginValidationDiagnostic[]> {
  const diagnostics: PluginValidationDiagnostic[] = [];
  try {
    await ensureMarketplaceManifestAvailable({
      marketplace: input.marketplace,
      storageRoot: input.storageRoot,
    });
  } catch (error) {
    diagnostics.push(toValidationDiagnostic(error, `${input.name}@${input.marketplace}`));
    return diagnostics;
  }
  const manifest = loadMarketplaceManifestSync(input.storageRoot, input.marketplace);
  if (!manifest) {
    diagnostics.push({
      code: "plugin_marketplace_invalid",
      message: `Marketplace not found: ${input.marketplace}`,
      path: getMarketplaceManifestPath(input.storageRoot, input.marketplace),
      severity: "error",
    });
    return diagnostics;
  }
  const plugin = manifest.plugins.find((entry) => entry.name === input.name);
  if (!plugin) {
    diagnostics.push({
      code: "plugin_not_found",
      message: `Plugin not found: ${input.name}@${input.marketplace}`,
      path: getMarketplaceManifestPath(input.storageRoot, input.marketplace),
      severity: "error",
    });
    return diagnostics;
  }

  pushDependencyDiagnostics({
    diagnostics,
    marketplace: input.marketplace,
    name: input.name,
    storageRoot: input.storageRoot,
  });

  let resolved: ResolvedPluginSourceRoot | null = null;
  try {
    resolved = await resolvePluginSourceRoot({
      entry: plugin,
      marketplace: input.marketplace,
      storageRoot: input.storageRoot,
    });
    diagnostics.push(
      ...validatePluginRoot({
        entry: plugin,
        marketplace: input.marketplace,
        rootPath: resolved.path,
        storageRoot: input.storageRoot,
      }),
    );
  } catch (error) {
    diagnostics.push(toValidationDiagnostic(error, `${input.name}@${input.marketplace}`));
  } finally {
    await cleanupPluginSourceBestEffort(resolved?.cleanup);
  }
  return diagnostics;
}

/**
* Enumerate the component "name + description" of a single plug-in on demand for use by the marketplace details UI.
* - Installed plug-ins: directly read the local cache/installation directory, no need to connect to the Internet.
* - Candidate not installed: parse and temporarily clone plugin sources on demand (finally clean the temporary directory), refer to validateMarketplacePlugin.
* The component name and description come from the frontmatter of the component directory (.md of command/agent, SKILL.md of skill),
* and manifest (hooks event name, mcpServers name, commands/agents declared in object form).
* Failure to read any type of component will be downgraded to "return as much as you can get" + diagnosis, without throwing away the entire details.
 */
export async function describeMarketplacePlugin(input: {
  marketplace: string;
  name: string;
  storageRoot: string;
}): Promise<DescribeMarketplacePluginResult> {
  const diagnostics: PluginValidationDiagnostic[] = [];
  const pluginId = `${input.name}@${input.marketplace}`;

// Installed first: no need to clone the local directory, fast and available offline.
  const installedRecord = loadInstalledPluginsSync(input.storageRoot).plugins.find(
    (record) => record.marketplace === input.marketplace && record.name === input.name,
  );
  if (installedRecord) {
    const rootPath = resolveInstalledPluginRoot(input.storageRoot, installedRecord);
    if (directoryExists(rootPath)) {
      const read = readComponentsAtRoot({
        diagnostics,
        marketplace: input.marketplace,
        rootPath,
      });
      return {
        components: read.components,
        diagnostics,
        ...(read.metadata ? { metadata: read.metadata } : {}),
      };
    }
// The installation record exists but the cache is missing (cleared) - continue to go through source parsing instead of reporting an error directly.
  }

  let manifest: PluginMarketplaceManifest | null = null;
  try {
    await ensureMarketplaceManifestAvailable({
      marketplace: input.marketplace,
      storageRoot: input.storageRoot,
    });
    manifest = loadMarketplaceManifestSync(input.storageRoot, input.marketplace);
  } catch (error) {
    diagnostics.push(toValidationDiagnostic(error, pluginId));
    return { components: [], diagnostics };
  }
  if (!manifest) {
    diagnostics.push({
      code: "plugin_marketplace_invalid",
      message: `Marketplace not found: ${input.marketplace}`,
      path: getMarketplaceManifestPath(input.storageRoot, input.marketplace),
      severity: "error",
    });
    return { components: [], diagnostics };
  }
  const entry = manifest.plugins.find((candidate) => candidate.name === input.name);
  if (!entry) {
    diagnostics.push({
      code: "plugin_not_found",
      message: `Plugin not found: ${pluginId}`,
      path: getMarketplaceManifestPath(input.storageRoot, input.marketplace),
      severity: "error",
    });
    return { components: [], diagnostics };
  }

  let resolved: ResolvedPluginSourceRoot | null = null;
  try {
    resolved = await resolvePluginSourceRoot({
      entry,
      marketplace: input.marketplace,
      storageRoot: input.storageRoot,
    });
    const read = readComponentsAtRoot({
      diagnostics,
      entry,
      marketplace: input.marketplace,
      rootPath: resolved.path,
    });
    return {
      components: read.components,
      diagnostics,
      ...(read.metadata ? { metadata: read.metadata } : {}),
    };
  } catch (error) {
    diagnostics.push(toValidationDiagnostic(error, pluginId));
    return { components: [], diagnostics };
  } finally {
    await cleanupPluginSourceBestEffort(resolved?.cleanup);
  }
}

/** Read the manifest of the plug-in root directory (downgrade to null if failed), and then hand it over to the pure enumerator to list the component name + description. */
function readComponentsAtRoot(input: {
  diagnostics: PluginValidationDiagnostic[];
  entry?: PluginMarketplaceEntry;
  marketplace: string;
  rootPath: string;
}): { components: PluginComponentGroup[]; metadata?: PluginManifestDisplayMetadata } {
  let loadedManifest: { manifest: PluginManifest; manifestPath?: string } | null = null;
  try {
    loadedManifest = readPluginManifestFromRoot(
      input.rootPath,
      input.entry ?? { name: "__describe__", raw: {} },
    );
  } catch {
// Manifest parsing failure is not fatal: components can still be scanned according to the default directory convention.
    loadedManifest = null;
  }
  const loaded = loadedManifest
    ? {
        id: `${loadedManifest.manifest.name}@${input.marketplace}`,
        manifest: loadedManifest.manifest,
        manifestPath: loadedManifest.manifestPath ?? input.rootPath,
        marketplace: input.marketplace,
        rootPath: input.rootPath,
        source: "cache" as const,
      }
    : undefined;
  const components = enumeratePluginComponents(input.rootPath, loadedManifest?.manifest ?? null, {
    diagnostics: input.diagnostics as PluginDiagnostic[],
    ...(loaded ? { loaded } : {}),
  });
  const metadata = loadedManifest ? toManifestDisplayMetadata(loadedManifest.manifest) : undefined;
  return { components, ...(metadata ? { metadata } : {}) };
}

/** Extract the fallback fields that can be displayed in plugin.json; if there is none, undefined is returned. */
function toManifestDisplayMetadata(
  manifest: PluginManifest,
): PluginManifestDisplayMetadata | undefined {
  const author = normalizeAuthorValue(manifest.author);
  const homepage =
    typeof manifest.homepage === "string" && manifest.homepage.trim().length > 0
      ? manifest.homepage
      : undefined;
  const metadata: PluginManifestDisplayMetadata = {
    ...(author?.name ? { author: author.name } : {}),
    ...(author?.url ? { authorUrl: author.url } : {}),
    ...(homepage ? { homepage } : {}),
    ...(manifest.version ? { version: manifest.version } : {}),
  };
  return Object.keys(metadata).length > 0 ? metadata : undefined;
}

export async function validateMarketplaceSource(input: {
  expectedId?: string;
  pluginName?: string;
  signal?: AbortSignal;
  source: MarketplaceSource;
  storageRoot: string;
}): Promise<PluginValidationDiagnostic[]> {
  const diagnostics: PluginValidationDiagnostic[] = [];
  let loaded: LoadMarketplaceResult | null = null;
  try {
    loaded = await loadMarketplaceFromSource(input.source, input.storageRoot, {
      persist: false,
      signal: input.signal,
    });
    if (input.expectedId && loaded.manifest.name !== input.expectedId) {
      diagnostics.push({
        code: "plugin_marketplace_invalid",
        message:
          `Marketplace declaration id mismatch: expected ${input.expectedId}, ` +
          `received ${loaded.manifest.name}`,
        pluginId: input.expectedId,
        severity: "error",
      });
      return diagnostics;
    }
    if (loaded.manifest.plugins.length === 0) {
      diagnostics.push({
        code: "plugin_marketplace_invalid",
        message: `Marketplace has no plugins: ${loaded.manifest.name}`,
        severity: "warning",
      });
    }
    const entries = input.pluginName
      ? loaded.manifest.plugins.filter((entry) => entry.name === input.pluginName)
      : loaded.manifest.plugins;
    if (input.pluginName && entries.length === 0) {
      diagnostics.push({
        code: "plugin_not_found",
        message: `Plugin not found: ${input.pluginName}@${loaded.manifest.name}`,
        pluginId: `${input.pluginName}@${loaded.manifest.name}`,
        severity: "error",
      });
      return diagnostics;
    }
    for (const entry of entries) {
      diagnostics.push(
        ...validateMarketplaceEntryShape(entry, loaded.manifest.name, {
          includeEntryCompatibility: false,
        }),
      );
      pushDependencyDiagnosticsFromManifest({
        diagnostics,
        manifest: loaded.manifest,
        marketplace: loaded.manifest.name,
        name: entry.name,
        storageRoot: input.storageRoot,
      });
      const deferred = getMarketplaceSourceValidationDeferral(entry, loaded.manifest.name);
      if (deferred) {
        diagnostics.push(deferred);
        pushEntryCompatibilityDiagnostics({
          diagnostics,
          entry,
          marketplace: loaded.manifest.name,
        });
        continue;
      }
      let resolved: ResolvedPluginSourceRoot | null = null;
      try {
        resolved = await resolvePluginSourceRoot({
          entry,
          marketplace: loaded.manifest.name,
          manifest: loaded.manifest,
          signal: input.signal,
          sourceRoot: loaded.sourceRoot,
          storageRoot: input.storageRoot,
        });
        diagnostics.push(
          ...validatePluginRoot({
            entry,
            marketplace: loaded.manifest.name,
            rootPath: resolved.path,
            storageRoot: input.storageRoot,
          }),
        );
      } catch (error) {
        diagnostics.push(toValidationDiagnostic(error, `${entry.name}@${loaded.manifest.name}`));
// The validate source is dry-run, but it must also show the UI the capability risks declared in the marketplace entry.
// When the remote/relative plugin source is temporarily unresolvable, the diagnostic-only capability diagnosis is still output based on the original text of the entry.
        pushEntryCompatibilityDiagnostics({
          diagnostics,
          entry,
          marketplace: loaded.manifest.name,
        });
      } finally {
        await cleanupPluginSourceBestEffort(resolved?.cleanup);
      }
    }
  } catch (error) {
    diagnostics.push(toValidationDiagnostic(error));
  } finally {
    await cleanupPluginSourceBestEffort(loaded?.cleanup);
  }
  return diagnostics;
}

/**
* Verify the local plug-in or marketplace path, read-only parse the manifest and return structured diagnosis, and do not write to storage.
* Input can be a directory or manifest file; directories are identified in the order of marketplace first, plugin root directory second.
 */
export async function validateLocalPluginPath(input: {
  path: string;
  signal?: AbortSignal;
  storageRoot: string;
}): Promise<PluginValidationDiagnostic[]> {
  const resolved = resolve(input.path);
  if (!existsSync(resolved)) {
    return [
      {
        code: "plugin_manifest_not_found",
        message: `Path does not exist: ${resolved}`,
        path: resolved,
        severity: "error",
      },
    ];
  }
  const rootPath = statSync(resolved).isDirectory()
    ? resolved
    : resolveManifestRootFromFile(resolved);
  if (findMarketplaceManifestPath(rootPath)) {
    return validateMarketplaceSource({
      signal: input.signal,
      source: { source: "directory", path: rootPath },
      storageRoot: input.storageRoot,
    });
  }
  const manifestPath = findPluginManifestPath(rootPath);
  if (!manifestPath) {
    return [
      {
        code: "plugin_manifest_not_found",
        message: `Plugin manifest not found: ${rootPath}`,
        path: rootPath,
        severity: "error",
      },
    ];
  }
  let name = "";
  try {
    const parsed = JSON.parse(readFileSync(manifestPath, "utf8")) as unknown;
    if (isRecord(parsed) && typeof parsed.name === "string") name = parsed.name.trim();
  } catch (error) {
    return [
      {
        code: "plugin_manifest_invalid",
        message: error instanceof Error ? error.message : String(error),
        path: manifestPath,
        severity: "error",
      },
    ];
  }
// There is no marketplace entry in the local directory: use manifest's own name to synthesize a strict entry,
// Let validatePluginRoot do exactly the same manifest/MCP validation as the installed plugin.
  return validatePluginRoot({
    entry: { name: name || basename(rootPath), raw: {} },
    marketplace: "inline",
    rootPath,
    storageRoot: input.storageRoot,
  });
}

/** When the user passes in the manifest file, push back the corresponding plug-in root directory. */
function resolveManifestRootFromFile(filePath: string): string {
  const dir = dirname(filePath);
  const dirName = basename(dir);
  return dirName.startsWith(".") && dirName.endsWith("-plugin") ? dirname(dir) : dir;
}

function getMarketplaceManifestPath(storageRoot: string, marketplace: string): string {
  return join(storageRoot, "marketplaces", sanitizePluginId(marketplace), MARKETPLACE_FILE);
}

function resolveDependencyClosure(input: {
  allowCrossMarketplaces: ReadonlySet<string>;
  marketplace: string;
  name: string;
  storageRoot: string;
}): string[] {
  const rootId = `${input.name}@${input.marketplace}`;
  const closure: string[] = [];
  const visiting: string[] = [];
  const visited = new Set<string>();

  const walk = (pluginId: string, requiredBy: string): void => {
    const { marketplace, name } = parsePluginId(pluginId);
    if (marketplace !== input.marketplace && !input.allowCrossMarketplaces.has(marketplace)) {
      throw new Error(
        `Cross-marketplace dependency is blocked: ${pluginId} required by ${requiredBy}`,
      );
    }
    if (visiting.includes(pluginId)) {
      throw new Error(`Plugin dependency cycle: ${[...visiting, pluginId].join(" -> ")}`);
    }
    if (visited.has(pluginId)) return;
    const manifest = loadMarketplaceManifestSync(input.storageRoot, marketplace);
    if (!manifest) throw new Error(`Marketplace not found for dependency: ${marketplace}`);
    const entry = manifest.plugins.find((plugin) => plugin.name === name);
    if (!entry) throw new Error(`Dependency not found: ${pluginId} required by ${requiredBy}`);
    visiting.push(pluginId);
    for (const dependency of entry.dependencies ?? []) {
      walk(qualifyDependency(dependency, marketplace), pluginId);
    }
    visiting.pop();
    visited.add(pluginId);
    closure.push(pluginId);
  };

  walk(rootId, rootId);
  return closure;
}

async function cacheMarketplacePlugin(input: {
  entry: PluginMarketplaceEntry;
  marketplace: string;
  signal?: AbortSignal;
  scope: "user" | "workspace";
  state: InstalledPluginsState;
  storageRoot: string;
}): Promise<CachedMarketplacePluginResult> {
  throwIfPluginOperationAborted(input.signal);
  const sourceRoot = await resolvePluginSourceRoot({
    entry: input.entry,
    marketplace: input.marketplace,
    signal: input.signal,
    storageRoot: input.storageRoot,
  });
  let version: string;
  let target: string;
  let activation: AtomicDirectoryActivation | undefined;
  try {
// When the multi-top-level ZIP does not have an explicit path, the resolver will fall back to extract root.
// The original installation process did not verify the manifest before deleting the old cache. The installed record was still written and enabled by default. The final runtime
// Unable to discover. The ZIP source must first confirm that the root directory can form a legal plug-in; strict:false continues to reuse the synthetic manifest.
    if (isZipPluginUrlSource(input.entry.source)) {
      assertZipPluginInstallRoot(sourceRoot.path, input.entry, input.marketplace);
    }
// The version segment of the cache directory and the version of the installation record cannot be taken from the marketplace entry
// version field: git/url source plug-in entries usually do not contain version, and if they do, they will only cover the whole issue.
// "0.0.0" causes the Root path to fall to .../<name>/0.0.0; and the UI display reads the plugin.json that comes with the plugin.
//The real version, the two are separated. Therefore, in the clone/copied source root directory, the real value is parsed according to the same rules as the loader.
// Version (see resolveInstalledPluginVersion for details), make the cache path segment consistent with the installation record and UI display version.
    version = resolveInstalledPluginVersion(sourceRoot.path, input.entry);
    target = getPluginCacheDir(input.storageRoot, input.marketplace, input.entry.name, version);
// The cachePath of the built-in filesystem/sea plug-in is the cache directory itself, and the source root directory may be the same as the target;
// At this time, there is no need (and cannot) rm first and then copy yourself, otherwise the source will be deleted.
    if (resolve(sourceRoot.path) !== resolve(target)) {
      throwIfPluginOperationAborted(input.signal);
      activation = await activateDirectoryAtomically({
        authorityPath: join(input.storageRoot, INSTALLED_PLUGINS_FILE),
        prepare: async (stagedPath) => {
          await ensureMarketplaceEntryManifest({ entry: input.entry, target: stagedPath });
        },
        signal: input.signal,
        sourcePath: sourceRoot.path,
        targetPath: target,
      });
    }
    if (resolve(sourceRoot.path) === resolve(target)) {
      await ensureMarketplaceEntryManifest({ entry: input.entry, target });
    }
  } finally {
// After the cache has been successfully copied, failure to cleanup the temporary directory cannot prevent the installed record from being written to disk.
    await cleanupPluginSourceBestEffort(sourceRoot.cleanup);
  }

  const now = new Date().toISOString();
  const record: InstalledPluginRecord = {
    id: `${input.entry.name}@${input.marketplace}`,
    name: input.entry.name,
    marketplace: input.marketplace,
    version,
    installPath: target,
    installedAt: now,
    updatedAt: now,
    scope: input.scope,
    ...(input.entry.dependencies ? { dependencies: input.entry.dependencies } : {}),
    ...(input.entry.source !== undefined ? { source: input.entry.source } : {}),
    ...(activation ? { cacheTransactionId: activation.transactionId } : {}),
  };
  const existingIndex = input.state.plugins.findIndex((plugin) => plugin.id === record.id);
  if (existingIndex >= 0) {
    const { cacheTransactionId: _previousCacheTransactionId, ...previousRecord } =
      input.state.plugins[existingIndex] ?? record;
    input.state.plugins[existingIndex] = {
      ...previousRecord,
      ...record,
      installedAt: previousRecord.installedAt ?? record.installedAt,
    };
  } else {
    input.state.plugins.push(record);
  }
  return { ...(activation ? { activation } : {}), record };
}

async function resolvePluginSourceRoot(input: {
  entry: PluginMarketplaceEntry;
  manifest?: PluginMarketplaceManifest;
  marketplace: string;
  signal?: AbortSignal;
  sourceRoot?: string;
  storageRoot: string;
}): Promise<ResolvedPluginSourceRoot> {
  throwIfPluginOperationAborted(input.signal);
  const source = input.entry.source;
  const marketplaceDir =
    input.sourceRoot ?? dirname(getMarketplaceManifestPath(input.storageRoot, input.marketplace));
  const manifest =
    input.manifest ?? loadMarketplaceManifestSync(input.storageRoot, input.marketplace);
  const pluginBaseDir = resolveMarketplacePluginBaseDir(marketplaceDir, manifest);
// When the built-in official plug-in is seeded to marketplace.json, the source is written as a bare kind string.
// "filesystem"/"sea" (see bootstrap/app/bundled-plugins.ts writeOfficialMarketplace),
// The original logic falls into the `typeof source === "string"` branch below, and "filesystem" is parsed as a relative path and thrown
// "Unsupported or missing plugin source: filesystem" causes the market details page to be unable to enumerate components for built-in plugins.
// This type of plug-in has been placed in the cachePath (if missing, press cache/<marketplace>/<name>/<version> to find out), and you can locate it directly.
  if (source === "filesystem" || source === "sea") {
    const cachePath = input.entry.cachePath;
    if (cachePath && directoryExists(cachePath)) return { path: cachePath };
    const computed = getPluginCacheDir(
      input.storageRoot,
      input.marketplace,
      input.entry.name,
      input.entry.version ?? DEFAULT_VERSION,
    );
    if (directoryExists(computed)) return { path: computed };
    throw new Error(
      `Bundled plugin cache directory missing: ${input.entry.name}@${input.marketplace}`,
    );
  }
  if (typeof source === "string") {
    const local = resolveInside(pluginBaseDir, source.replace(/^\.\//, ""));
    if (local && directoryExists(local)) return { path: local };
    const fallback = resolve(source);
    if (directoryExists(fallback)) return { path: fallback };
    throw new Error(`Unsupported or missing plugin source: ${source}`);
  }
  if (isRecord(source)) {
    const sourceKind = typeof source.source === "string" ? source.source : "";
    if (sourceKind === "directory") {
      const path = resolve(readRequiredPluginSourceString(source, "path", "directory path"));
      if (directoryExists(path)) return { path };
      throw new Error(`Plugin source directory does not exist: ${path}`);
    }
    if (sourceKind === "github") {
      const repo = readRequiredPluginSourceString(source, "repo", "GitHub repo");
      const url = `https://github.com/${repo}.git`;
      return resolveRepositoryPluginSource({
        path: typeof source.path === "string" ? source.path : undefined,
        ref: typeof source.ref === "string" ? source.ref : undefined,
        signal: input.signal,
        sha: readPluginSourceIdentityPin(source),
        url,
      });
    }
    if (sourceKind === "git") {
      return resolveRepositoryPluginSource({
        path: typeof source.path === "string" ? source.path : undefined,
        ref: typeof source.ref === "string" ? source.ref : undefined,
        signal: input.signal,
        sha: readPluginSourceIdentityPin(source),
        url: readRequiredPluginSourceString(source, "url", "Git URL"),
      });
    }
    if (sourceKind === "url") {
      const url = readRequiredPluginSourceString(source, "url", "URL");
      const sourceType = typeof source.type === "string" ? source.type : "";
      if (sourceType === "zip") {
        return resolveZipPluginSource({
          headers: readPluginSourceHeaders(source),
          path: readOptionalZipPluginSourcePath(source),
          sha256: readRequiredZipPluginSourceSha256(source),
          signal: input.signal,
          stripRoot: readOptionalZipPluginSourceStripRoot(source),
          url,
        });
      }
      if (sourceType && sourceType !== "git") {
        throw new UnsupportedPluginSourceError(`url:${sourceType}`);
      }
      return resolveRepositoryPluginSource({
        path: typeof source.path === "string" ? source.path : undefined,
        ref: typeof source.ref === "string" ? source.ref : undefined,
        signal: input.signal,
        sha: readPluginSourceIdentityPin(source),
        url,
      });
    }
    if (sourceKind === "git-subdir") {
      return resolveRepositoryPluginSource({
        path: readRequiredPluginSourceString(source, "path", "git-subdir path"),
        ref: typeof source.ref === "string" ? source.ref : undefined,
        signal: input.signal,
        sha: readPluginSourceIdentityPin(source),
        url: normalizeGitUrl(readRequiredPluginSourceString(source, "url", "git-subdir URL")),
      });
    }
    if (sourceKind === "npm" || sourceKind === "pip") {
      throw new UnsupportedPluginSourceError(sourceKind);
    }
// If the explicit object source configuration is incorrect, it cannot be downgraded to the directory with the same name in the marketplace, otherwise the wrong source will be installed.
    throw new Error(
      `Plugin source is invalid or unsupported for ${input.entry.name}@${input.marketplace}: ${sourceKind || "missing kind"}`,
    );
  }
  const localByName = join(pluginBaseDir, input.entry.name);
  if (directoryExists(localByName)) return { path: localByName };
  throw new Error(`Plugin source is not supported for ${input.entry.name}@${input.marketplace}`);
}

export function readPluginSourceSha(source: unknown): string | undefined {
  return readPluginSourceIdentityPin(source);
}

export function readPluginSourceIdentityPin(source: unknown): string | undefined {
  if (!isRecord(source)) return undefined;
  const zipSha256 = readZipPluginSourceSha256(source);
  if (zipSha256) return zipSha256;
  if (typeof source.sha === "string") return source.sha;

// Compatible with old version and third-party marketplace's source identity writing method.
  if (typeof source.commit === "string") return source.commit;
  return undefined;
}

function readRequiredZipPluginSourceSha256(source: Record<string, unknown>): string {
  if (typeof source.sha256 === "string") return source.sha256;
  throw new Error("Plugin zip source sha256 is required");
}

function readRequiredPluginSourceString(
  source: Record<string, unknown>,
  field: string,
  label: string,
): string {
  const value = source[field];
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new Error(`Plugin ${label} source requires a non-empty ${field}`);
  }
  return value;
}

function readOptionalZipPluginSourcePath(source: Record<string, unknown>): string | undefined {
  if (source.path === undefined) return undefined;
  if (typeof source.path === "string") return source.path;
  throw new Error("Plugin zip source path must be a string");
}

function readOptionalZipPluginSourceStripRoot(
  source: Record<string, unknown>,
): boolean | undefined {
  if (source.stripRoot === undefined) return undefined;
  if (typeof source.stripRoot === "boolean") return source.stripRoot;
  throw new Error("Plugin zip source stripRoot must be a boolean");
}

function readPluginSourceHeaders(
  source: Record<string, unknown>,
): Record<string, string> | undefined {
  if (source.headers === undefined) return undefined;
  if (!isRecord(source.headers)) {
    throw new Error("Plugin zip source headers must be an object");
  }
  const headers: Record<string, string> = {};
  for (const [key, value] of Object.entries(source.headers)) {
    if (typeof value !== "string") {
      throw new Error(`Plugin zip source header must be a string: ${key}`);
    }
    headers[key] = value;
  }
  return headers;
}

async function resolveGitPluginSource(input: {
  path?: string;
  ref?: string;
  signal?: AbortSignal;
  sha?: string;
  url: string;
}): Promise<ResolvedPluginSourceRoot> {
  const dir = await clonePluginSource(input.url, input.ref, input.sha, input.signal);
  const cleanup = async (): Promise<void> => {
    await rm(dir, { force: true, recursive: true });
  };
  if (!input.path) return { cleanup, path: dir };
  throwIfPluginOperationAborted(input.signal);
  const subdir = resolveInside(dir, input.path);
  if (!subdir || !directoryExists(subdir)) {
    const primaryError = new Error(`Plugin source subdirectory does not exist: ${input.path}`);
    const cleanupError = await cleanupPluginSourceBestEffort(cleanup);
    throw appendPluginSourceCleanupError(primaryError, cleanupError);
  }
  return { cleanup, path: subdir };
}

async function resolveRepositoryPluginSource(input: {
  path?: string;
  ref?: string;
  signal?: AbortSignal;
  sha?: string;
  url: string;
}): Promise<ResolvedPluginSourceRoot> {
  try {
    return await resolveGitHubArchiveSource({
      path: input.path,
      pin: input.sha ?? input.ref,
      signal: input.signal,
      url: input.url,
    });
  } catch (error) {
    if (!shouldFallbackGitHubArchiveToGit(error)) {
      throw createArchiveFetchError(input.url, error);
    }
  }
  return resolveGitPluginSource(input);
}

async function clonePluginSource(
  url: string,
  ref: string | undefined,
  sha?: string,
  signal?: AbortSignal,
): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "zcode-plugin-src-"));
  const args = ["clone"];
  if (!sha) args.push("--depth", "1");
  if (ref) args.push("--branch", ref);
  args.push(url, dir);
  try {
    await execGitCloneWithRetry(args, dir, signal);
    if (sha) await execGitCommand(["-C", dir, "checkout", sha], signal);
    return dir;
  } catch (error) {
    const cleanupError = await cleanupPluginSourceBestEffort(async () => {
      await rm(dir, { force: true, recursive: true });
    });
    throw appendPluginSourceCleanupError(error, cleanupError);
  }
}

function normalizeGitUrl(value: string): string {
  if (/^[^/]+\/[^/]+$/u.test(value) && !value.includes(":")) {
    return `https://github.com/${value}.git`;
  }
  return value;
}

function resolveMarketplacePluginBaseDir(
  marketplaceDir: string,
  manifest: PluginMarketplaceManifest | null,
): string {
  const pluginRoot = manifest?.pluginRoot;
  if (!pluginRoot) return marketplaceDir;
  const resolved = resolveInside(marketplaceDir, pluginRoot);
  return resolved && directoryExists(resolved) ? resolved : marketplaceDir;
}

async function ensureMarketplaceEntryManifest(input: {
  entry: PluginMarketplaceEntry;
  target: string;
}): Promise<void> {
  if (findPluginManifestPath(input.target)) return;
  if (input.entry.strict !== false) return;
  const manifestDir = join(input.target, ".claude-plugin");
  await mkdir(manifestDir, { recursive: true });
  await writeJsonFile(
    join(manifestDir, "plugin.json"),
    createManifestFromMarketplaceEntry(input.entry),
  );
}

function assertZipPluginInstallRoot(
  rootPath: string,
  entry: PluginMarketplaceEntry,
  marketplace: string,
): void {
  const loaded = readPluginManifestFromRoot(rootPath, entry);
  const pluginId = `${entry.name}@${marketplace}`;
  if (!loaded) {
    throw new Error(`Plugin manifest not found: ${pluginId}`);
  }
  if (loaded.manifest.name !== entry.name) {
    throw new Error(
      `Plugin manifest name '${loaded.manifest.name}' does not match marketplace entry '${entry.name}'`,
    );
  }
}

function createManifestFromMarketplaceEntry(
  entry: PluginMarketplaceEntry,
): Record<string, unknown> {
  const raw = { ...entry.raw };
  delete raw.source;
  delete raw.category;
  delete raw.tags;
  delete raw.strict;
// Store information (Store Listing) is directory layer display metadata and does not belong to the plug-in manifest;
// Eliminate it when synthesizing the manifest to avoid polluting the semantics of plugin.json (author/homepage is a legal manifest field and is reserved).
  delete raw.displayName;
  delete raw.displayName_i18n;
  delete raw.description_i18n;
  delete raw.icon;
  delete raw.privacyPolicy;
  delete raw.termsOfService;
  delete raw.heroImage;
  delete raw.examplePrompts;
  delete raw.examplePrompts_i18n;
  delete raw.requiresPaidPlan;
  return {
    ...raw,
    name: entry.name,
    version: entry.version ?? DEFAULT_VERSION,
  };
}

async function loadMarketplaceFromSource(
  source: MarketplaceSource,
  storageRoot: string,
  options: { persist: boolean; signal?: AbortSignal },
): Promise<LoadMarketplaceResult> {
  throwIfPluginOperationAborted(options.signal);
  switch (source.source) {
    case "settings":
      return { manifest: normalizeMarketplaceManifest(source.marketplace) };
    case "file": {
      const parsed = JSON.parse(await readFile(source.path, "utf8")) as unknown;
      return {
        manifest: parseRequiredMarketplaceManifest(parsed),
        sourceRoot: dirname(source.path),
      };
    }
    case "directory": {
      const file = findMarketplaceManifestPath(source.path);
      if (!file) throw new Error(`Marketplace manifest not found in directory: ${source.path}`);
      const parsed = JSON.parse(await readFile(file, "utf8")) as unknown;
      const manifest = parseRequiredMarketplaceManifest(parsed);
      if (options.persist) {
        const activation = await stageMarketplaceDirectoryPlugins(
          source.path,
          storageRoot,
          manifest.name,
          manifest.raw,
          options.signal,
        );
        await activation.finalize();
      }
      return { manifest, sourceRoot: source.path };
    }
    case "url": {
      const parsed = await requestMarketplaceJson(source.url, source.headers, options.signal);
      return { manifest: parseRequiredMarketplaceManifest(parsed) };
    }
    case "github": {
      const resolved = await resolveRepositoryMarketplaceSource(
        `https://github.com/${source.repo}.git`,
        source.ref,
        source.sparsePaths,
        options.signal,
      );
      const cleanup = resolved.cleanup;
      try {
        const file = findMarketplaceManifestPath(resolved.path, source.path);
        if (!file) throw new Error(`Marketplace manifest not found in GitHub repo: ${source.repo}`);
        const parsed = JSON.parse(await readFile(file, "utf8")) as unknown;
        const manifest = parseRequiredMarketplaceManifest(parsed);
        if (options.persist) {
          const activation = await stageMarketplaceDirectoryPlugins(
            resolved.path,
            storageRoot,
            manifest.name,
            manifest.raw,
            options.signal,
          );
          await activation.finalize();
        }
        return { cleanup, manifest, sourceRoot: resolved.path };
      } catch (error) {
        const cleanupError = await cleanupPluginSourceBestEffort(cleanup);
        throw appendPluginSourceCleanupError(error, cleanupError);
      }
    }
    case "git": {
      const resolved = await resolveRepositoryMarketplaceSource(
        source.url,
        source.ref,
        source.sparsePaths,
        options.signal,
      );
      const cleanup = resolved.cleanup;
      try {
        const file = findMarketplaceManifestPath(resolved.path, source.path);
        if (!file) throw new Error(`Marketplace manifest not found in git repo: ${source.url}`);
        const parsed = JSON.parse(await readFile(file, "utf8")) as unknown;
        const manifest = parseRequiredMarketplaceManifest(parsed);
        if (options.persist) {
          const activation = await stageMarketplaceDirectoryPlugins(
            resolved.path,
            storageRoot,
            manifest.name,
            manifest.raw,
            options.signal,
          );
          await activation.finalize();
        }
        return { cleanup, manifest, sourceRoot: resolved.path };
      } catch (error) {
        const cleanupError = await cleanupPluginSourceBestEffort(cleanup);
        throw appendPluginSourceCleanupError(error, cleanupError);
      }
    }
    case "npm":
      throw new UnsupportedMarketplaceSourceError("npm");
    case "hostPattern":
      throw new UnsupportedMarketplaceSourceError("hostPattern");
    case "pathPattern":
      throw new UnsupportedMarketplaceSourceError("pathPattern");
  }
}

async function resolveRepositoryMarketplaceSource(
  url: string,
  ref: string | undefined,
  sparsePaths: string[] | undefined,
  signal?: AbortSignal,
): Promise<ResolvedPluginSourceRoot> {
// sparsePaths is an existing MarketplaceSource contract. Archive needs to download the entire archive first.
// This will cause large warehouses that could have been sparse cloned to fail due to the download limit; before Archive has implemented equivalent projection,
// Explicitly retain the system Git's sparse checkout route.
  if (!sparsePaths?.length) {
    try {
      return await resolveGitHubArchiveSource({ pin: ref, signal, url });
    } catch (error) {
      if (!shouldFallbackGitHubArchiveToGit(error)) {
        throw createArchiveFetchError(url, error);
      }
    }
  }
  const dir = await cloneMarketplaceSource(url, ref, sparsePaths, signal);
  return {
    cleanup: async () => {
      await rm(dir, { force: true, recursive: true });
    },
    path: dir,
  };
}

async function cloneMarketplaceSource(
  url: string,
  ref: string | undefined,
  sparsePaths: string[] | undefined,
  signal?: AbortSignal,
): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "zcode-marketplace-src-"));
  const args = ["clone", "--depth", "1"];
  if (ref) args.push("--branch", ref);
  if (sparsePaths?.length) args.push("--filter=blob:none", "--sparse");
  args.push(url, dir);
  try {
    await execGitCloneWithRetry(args, dir, signal);
    if (sparsePaths?.length) {
      await execGitCommand(["-C", dir, "sparse-checkout", "set", ...sparsePaths], signal);
    }
    return dir;
  } catch (error) {
    await rm(dir, { force: true, recursive: true });
    throw error;
  }
}

async function execGitCloneWithRetry(
  args: string[],
  targetDir: string,
  signal?: AbortSignal,
): Promise<void> {
  let lastError: unknown;
  for (let attempt = 1; attempt <= GIT_CLONE_MAX_ATTEMPTS; attempt += 1) {
    try {
      throwIfPluginOperationAborted(signal);
      if (attempt > 1) {
        await rm(targetDir, { force: true, recursive: true });
        await mkdir(targetDir, { recursive: true });
      }
      await execGitCommand(args, signal);
      return;
    } catch (error) {
      lastError = error;
      if (attempt >= GIT_CLONE_MAX_ATTEMPTS || !isRetryableGitCloneError(error)) {
        throw error;
      }
// GitHub's occasional RPC/recv timeout will cause the official marketplace add to fail.
// Only do short retries for explicit network clone errors to avoid masking deterministic errors such as permissions, paths, or repositories not existing.
      await delay(GIT_CLONE_RETRY_DELAY_MS * attempt, signal);
    }
  }
  throw lastError;
}

async function execGitCommand(args: string[], signal?: AbortSignal): Promise<void> {
  throwIfPluginOperationAborted(signal);
  try {
// Explicit binary coverage not only supports non-standard Git installation locations, but also allows cross-process E2E to point Git to non-existent
// Absolute path, truly proves that the Archive main link does not rely on Git that accidentally exists on the development machine.
    const gitBinary = process.env.ZCODE_GIT_BINARY?.trim() || "git";
    await execFileAsync(gitBinary, args, {
      env: buildMarketplaceGitEnv(),
      killSignal: "SIGTERM",
      maxBuffer: 1024 * 1024 * 10,
      signal,
      timeout: GIT_COMMAND_TIMEOUT_MS,
    });
  } catch (error) {
    if (isCommandUnavailableError(error)) {
      const source = args.find(
        (arg) => arg.includes("://") || arg.startsWith("git@") || arg.startsWith("git+"),
      );
      throw createGitUnavailableError(source ?? args.at(-1) ?? "Git operation");
    }
    throw error;
  }
  throwIfPluginOperationAborted(signal);
}

function isRetryableGitCloneError(error: unknown): boolean {
  const output = getErrorOutput(error);
  return /RPC failed|Operation timed out|Recv failure|expected flush|early EOF|remote end hung up|HTTP\/2 stream|Connection reset|ETIMEDOUT|ECONNRESET|network timeout/iu.test(
    output,
  );
}

function getErrorOutput(error: unknown): string {
  if (!isRecord(error)) {
    return error instanceof Error ? error.message : String(error);
  }
  const chunks = [
    error instanceof Error ? error.message : "",
    typeof error.stdout === "string" ? error.stdout : "",
    typeof error.stderr === "string" ? error.stderr : "",
  ];
  return chunks.filter(Boolean).join("\n");
}

function delay(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolveDelay, rejectDelay) => {
    if (signal?.aborted) {
      rejectDelay(createPluginOperationCancelledError());
      return;
    }
    let timeout: ReturnType<typeof setTimeout> | undefined;
    const cleanup = (): void => {
      if (timeout) {
        clearTimeout(timeout);
        timeout = undefined;
      }
      signal?.removeEventListener("abort", handleAbort);
    };
    const handleAbort = (): void => {
      cleanup();
      rejectDelay(createPluginOperationCancelledError());
    };
    timeout = setTimeout(() => {
      cleanup();
      resolveDelay();
    }, ms);
    signal?.addEventListener("abort", handleAbort, { once: true });
  });
}

async function stageMarketplaceDirectoryPlugins(
  sourceDir: string,
  storageRoot: string,
  marketplace: string,
  manifest: Record<string, unknown>,
  signal?: AbortSignal,
): Promise<AtomicDirectoryActivation> {
  throwIfPluginOperationAborted(signal);
  const targetDir = dirname(getMarketplaceManifestPath(storageRoot, marketplace));
  return activateDirectoryAtomically({
    authorityPath: join(storageRoot, KNOWN_MARKETPLACES_FILE),
    prepare: async (stagedPath) => {
      await writeJsonFile(join(stagedPath, MARKETPLACE_FILE), manifest);
    },
    signal,
    sourcePath: sourceDir,
    targetPath: targetDir,
  });
}

async function stageMarketplaceManifest(
  storageRoot: string,
  marketplace: string,
  manifest: Record<string, unknown>,
  signal?: AbortSignal,
): Promise<AtomicDirectoryActivation> {
  const targetDir = dirname(getMarketplaceManifestPath(storageRoot, marketplace));
// URL/settings source does not have sourceRoot; when directly overwriting the manifest, if it is written during
// It cannot be rolled back when the deadline is reached or the known state fails to be placed. prepare-only activation lets manifest
// Submit using the same transactionId as known_marketplaces.json, and continue reading the previous generation snapshot if it fails.
  return activateDirectoryAtomically({
    authorityPath: join(storageRoot, KNOWN_MARKETPLACES_FILE),
    prepare: async (stagedPath) => {
      await writeJsonFile(join(stagedPath, MARKETPLACE_FILE), manifest);
    },
    signal,
    targetPath: targetDir,
  });
}

async function upsertKnownMarketplace(
  storageRoot: string,
  record: KnownMarketplaceRecord,
): Promise<KnownMarketplaceActivation> {
  const known = loadKnownMarketplacesSync(storageRoot);
  const index = known.findIndex((item) => item.id === record.id);
  const previous = index >= 0 ? known[index] : undefined;
  if (index >= 0) {
    const {
      cacheTransactionId: _previousCacheTransactionId,
      lastRefreshFailure: _lastRefreshFailure,
      ...successfulPrevious
    } = previous ?? record;
    known[index] = {
      ...successfulPrevious,
      ...record,
      addedAt: previous?.addedAt ?? record.addedAt,
    };
  } else {
    known.push(record);
  }
  await writeKnownMarketplaces(storageRoot, known);
  let settled = false;
  return {
    finalize: () => {
      settled = true;
    },
    rollback: async () => {
      if (settled) return;
      const current = loadKnownMarketplacesSync(storageRoot);
      const currentIndex = current.findIndex((item) => item.id === record.id);
      const currentRecord = currentIndex >= 0 ? current[currentIndex] : undefined;
      if (
        !currentRecord ||
        currentRecord.lastUpdated !== record.lastUpdated ||
        currentRecord.cacheTransactionId !== record.cacheTransactionId
      ) {
        throw new Error(
          `Cannot roll back marketplace authority after concurrent update: ${record.id}`,
        );
      }
      if (previous) {
        current[currentIndex] = previous;
      } else {
        current.splice(currentIndex, 1);
      }
      await writeKnownMarketplaces(storageRoot, current);
      settled = true;
    },
  };
}

async function persistMarketplaceRefreshFailure(
  storageRoot: string,
  marketplace: string,
  failure: MarketplaceRefreshFailure,
): Promise<void> {
  const known = loadKnownMarketplacesSync(storageRoot);
  const index = known.findIndex((record) => record.id === marketplace);
  if (index < 0 || !known[index]) return;
  known[index] = { ...known[index], lastRefreshFailure: failure };
  await writeKnownMarketplaces(storageRoot, known);
}

async function writeKnownMarketplaces(
  storageRoot: string,
  marketplaces: KnownMarketplaceRecord[],
): Promise<void> {
  await writeJsonFile(join(storageRoot, KNOWN_MARKETPLACES_FILE), {
    version: 1,
    marketplaces,
  });
}

function writeKnownMarketplacesSync(
  storageRoot: string,
  marketplaces: KnownMarketplaceRecord[],
): void {
  const path = join(storageRoot, KNOWN_MARKETPLACES_FILE);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(
    path,
    `${JSON.stringify(
      {
        version: 1,
        marketplaces,
      },
      null,
      2,
    )}\n`,
    "utf8",
  );
}

function defaultMarketplaceSourceFromString(source: string): MarketplaceSource {
  const trimmed = source.trim();
  if (/^[^/]+\/[^/]+(?:[#@].+)?$/u.test(trimmed) && !trimmed.includes(":")) {
    const { ref, url } = splitGitHubShorthand(trimmed);
    return ref ? { source: "github", repo: url, ref } : { source: "github", repo: url };
  }
  if (trimmed.startsWith("http://") || trimmed.startsWith("https://")) {
    return { source: "url", url: trimmed };
  }
  return { source: "url", url: trimmed };
}

function normalizeInstalledPluginsState(value: unknown): InstalledPluginsState {
  if (!isRecord(value)) return { version: 1, plugins: [] };
  const rawPlugins = value.plugins;
  if (isRecord(rawPlugins)) {
    return {
      version: 1,
      plugins: Object.entries(rawPlugins).flatMap(([pluginId, entry]) =>
        normalizeInstalledPluginRecordFromMap(pluginId, entry),
      ),
    };
  }
  const plugins = Array.isArray(rawPlugins) ? rawPlugins : [];
  return {
    version: 1,
    plugins: plugins.filter(isInstalledPluginRecord),
  };
}

function normalizeInstalledPluginRecordFromMap(
  pluginId: string,
  entry: unknown,
): InstalledPluginRecord[] {
  const entries = Array.isArray(entry) ? entry : [entry];
  return entries.flatMap((item): InstalledPluginRecord[] => {
    if (!isRecord(item)) return [];
    const installPath = typeof item.installPath === "string" ? item.installPath : "";
    if (!installPath) return [];
    let parsed: { marketplace: string; name: string };
    try {
      parsed = parsePluginId(pluginId);
    } catch {
      return [];
    }
    const scope = item.scope === "project" || item.scope === "local" ? "workspace" : "user";
    return [
      {
        id: pluginId,
        name: parsed.name,
        marketplace: parsed.marketplace,
        version: typeof item.version === "string" ? item.version : DEFAULT_VERSION,
        installPath,
        installedAt:
          typeof item.installedAt === "string" ? item.installedAt : new Date(0).toISOString(),
        ...(typeof item.lastUpdated === "string" ? { updatedAt: item.lastUpdated } : {}),
        scope,
      },
    ];
  });
}

function parseRequiredMarketplaceManifest(value: unknown): PluginMarketplaceManifest {
  const parsed = parseMarketplaceManifest(value);
  if (!parsed) throw new Error("Marketplace manifest is invalid");
  return parsed;
}

function parseMarketplaceManifest(value: unknown): PluginMarketplaceManifest | null {
  if (!isRecord(value)) return null;
  const name = typeof value.name === "string" ? value.name.trim() : "";
  if (!MARKETPLACE_NAME_PATTERN.test(name)) return null;
  const rawPlugins = value.plugins;
  const pluginEntries = Array.isArray(rawPlugins)
    ? rawPlugins
    : isRecord(rawPlugins)
      ? Object.entries(rawPlugins).map(([pluginName, plugin]) =>
          isRecord(plugin) ? { name: pluginName, ...plugin } : { name: pluginName },
        )
      : [];
  return normalizeMarketplaceManifest({
    ...value,
    name,
    plugins: pluginEntries,
  });
}

function normalizeMarketplaceManifest(
  value: PluginMarketplaceManifest | Record<string, unknown>,
): PluginMarketplaceManifest {
  if (isPluginMarketplaceManifest(value)) return value;
  const metadata = isRecord(value.metadata) ? value.metadata : {};
  const plugins = Array.isArray(value.plugins)
    ? value.plugins
        .filter(isRecord)
        .map((entry): PluginMarketplaceEntry | null => {
          const name = typeof entry.name === "string" ? entry.name.trim() : "";
          if (name.length === 0) return null;
          const dependencies = Array.isArray(entry.dependencies)
            ? entry.dependencies
                .map(normalizeDependencyRef)
                .filter((item): item is string => item !== null)
            : undefined;
          const tags = Array.isArray(entry.tags)
            ? entry.tags.filter((item): item is string => typeof item === "string")
            : undefined;
          const listing = parseEntryStoreListing(entry);
          return {
            name,
            ...(typeof entry.category === "string" ? { category: entry.category } : {}),
            ...(typeof entry.description === "string" ? { description: entry.description } : {}),
            ...(typeof entry.version === "string" ? { version: entry.version } : {}),
            ...(entry.source !== undefined ? { source: entry.source } : {}),
            ...(typeof entry.cachePath === "string" ? { cachePath: entry.cachePath } : {}),
            ...(dependencies ? { dependencies } : {}),
            ...(typeof entry.strict === "boolean" ? { strict: entry.strict } : {}),
            ...(tags ? { tags } : {}),
            ...(listing ? { listing } : {}),
            raw: entry,
          };
        })
        .filter((entry): entry is PluginMarketplaceEntry => entry !== null)
    : [];
  const allowCrossMarketplaceDependenciesOn = Array.isArray(
    value.allowCrossMarketplaceDependenciesOn,
  )
    ? value.allowCrossMarketplaceDependenciesOn.filter(
        (item): item is string => typeof item === "string",
      )
    : undefined;
// Featured curated list at the top level of the directory: only accepts non-empty string arrays, and removes blank items.
  const featured = Array.isArray(value.featured)
    ? value.featured.filter(
        (item): item is string => typeof item === "string" && item.trim().length > 0,
      )
    : undefined;
  return {
    name: String(value.name),
    ...(typeof value.description === "string"
      ? { description: value.description }
      : typeof metadata.description === "string"
        ? { description: metadata.description }
        : {}),
    plugins,
    ...(allowCrossMarketplaceDependenciesOn ? { allowCrossMarketplaceDependenciesOn } : {}),
    ...(typeof metadata.pluginRoot === "string" ? { pluginRoot: metadata.pluginRoot } : {}),
    ...(featured && featured.length > 0 ? { featured } : {}),
    raw: value,
  };
}

/**
* Parse optional store display information from catalog entries. Compatible with author, i18n map and multi-valued fields in string or object form;
* Return undefined when no valid content is parsed to avoid hanging empty objects for each entry.
 */
export function parseEntryStoreListing(
  entry: Record<string, unknown>,
): PluginStoreListing | undefined {
  const readString = (key: string): string | undefined => {
    const value = entry[key];
    return typeof value === "string" && value.trim().length > 0 ? value : undefined;
  };
  const readStringMap = (key: string): Record<string, string> | undefined => {
    const value = entry[key];
    if (!isRecord(value)) return undefined;
    const map: Record<string, string> = {};
    for (const [locale, text] of Object.entries(value)) {
      if (typeof text === "string") map[locale] = text;
    }
    return Object.keys(map).length > 0 ? map : undefined;
  };
  const readStringListMap = (key: string): Record<string, string[]> | undefined => {
    const value = entry[key];
    if (!isRecord(value)) return undefined;
    const map: Record<string, string[]> = {};
    for (const [locale, list] of Object.entries(value)) {
      if (!Array.isArray(list)) continue;
      const items = list.filter((item): item is string => typeof item === "string");
      if (items.length > 0) map[locale] = items;
    }
    return Object.keys(map).length > 0 ? map : undefined;
  };

  const listing: PluginStoreListing = {};
  const displayName = readString("displayName");
  if (displayName) listing.displayName = displayName;
  const displayNameI18n = readStringMap("displayName_i18n");
  if (displayNameI18n) listing.displayNameI18n = displayNameI18n;
  const descriptionI18n = readStringMap("description_i18n");
  if (descriptionI18n) listing.descriptionI18n = descriptionI18n;
  for (const key of [
    "icon",
    "category",
    "homepage",
    "privacyPolicy",
    "termsOfService",
    "heroImage",
  ] as const) {
    const value = readString(key);
    if (value) listing[key] = value;
  }
  const author = normalizeAuthorValue(entry.author);
  if (author?.name) listing.author = author.name;
  if (author?.url) listing.authorUrl = author.url;
  const examplePrompts = Array.isArray(entry.examplePrompts)
    ? entry.examplePrompts.filter(
        (item): item is string => typeof item === "string" && item.trim().length > 0,
      )
    : undefined;
  if (examplePrompts && examplePrompts.length > 0) listing.examplePrompts = examplePrompts;
  const examplePromptsI18n = readStringListMap("examplePrompts_i18n");
  if (examplePromptsI18n) listing.examplePromptsI18n = examplePromptsI18n;
// The paid package prompt only recognizes explicit Boolean true; ambiguous writing methods such as string "true" and 1 will be treated as no package required.
// To avoid miswriting the directory, put a payment reminder on the free plug-in.
  if (entry.requiresPaidPlan === true) listing.requiresPaidPlan = true;
  return Object.keys(listing).length > 0 ? listing : undefined;
}

/** The author field is compatible with string and {name,url} (plugin.json shares this rule with directory entries). */
export function normalizeAuthorValue(value: unknown): { name?: string; url?: string } | undefined {
  if (typeof value === "string") {
    const name = value.trim();
    return name.length > 0 ? { name } : undefined;
  }
  if (!isRecord(value)) return undefined;
  const name = typeof value.name === "string" ? value.name.trim() : "";
  const url = typeof value.url === "string" ? value.url.trim() : "";
  if (!name && !url) return undefined;
  return {
    ...(name ? { name } : {}),
    ...(url ? { url } : {}),
  };
}

function normalizeDependencyRef(value: unknown): string | null {
  if (typeof value === "string") return value.replace(/@\^[^@]*$/u, "");
  if (!isRecord(value)) return null;
  const name = typeof value.name === "string" ? value.name.trim() : "";
  if (!name) return null;
  const marketplace = typeof value.marketplace === "string" ? value.marketplace.trim() : "";
  return marketplace ? `${name}@${marketplace}` : name;
}

function findMarketplaceManifestPath(rootPath: string, explicitPath?: string): string | null {
  const candidates = [
    ...(explicitPath ? [explicitPath] : []),
    CLAUDE_MARKETPLACE_FILE,
    MARKETPLACE_FILE,
  ];
  for (const candidate of candidates) {
    const path = resolveInside(rootPath, candidate);
    if (path && fileExists(path)) return path;
  }
  return null;
}

function findPluginManifestPath(rootPath: string): string | null {
  for (const candidate of [ZCODE_MANIFEST_PATH, CLAUDE_MANIFEST_PATH, CODEX_MANIFEST_PATH]) {
    const path = join(rootPath, candidate);
    if (fileExists(path)) return path;
  }
  return null;
}

//Cache path segments and version sources of installation records. Prioritize the actual content in plugin.json where the plug-in is placed.
// version (same origin as the loader readPluginManifestFromRoot/index.ts display version), will fall back to the marketplace when missing
// The version of the entry, finally DEFAULT_VERSION. If the read fails, the relaxed fallback is maintained, and the verification is handed over to validateMarketplacePlugin.
function resolveInstalledPluginVersion(rootPath: string, entry: PluginMarketplaceEntry): string {
  const manifestPath = findPluginManifestPath(rootPath);
  if (manifestPath) {
    try {
      const parsed = JSON.parse(readFileSync(manifestPath, "utf8")) as unknown;
      if (
        isRecord(parsed) &&
        typeof parsed.version === "string" &&
        parsed.version.trim().length > 0
      ) {
        return parsed.version;
      }
    } catch {
// Fall back to the bottom: The installation should not be interrupted when the manifest is unreadable/illegal, and the version is listed as an entry or default value.
    }
  }
  return entry.version ?? DEFAULT_VERSION;
}

function readPluginManifestFromRoot(
  rootPath: string,
  entry: PluginMarketplaceEntry,
): { manifest: PluginManifest; manifestPath?: string } | null {
  const manifestPath = findPluginManifestPath(rootPath);
  if (manifestPath) {
    const parsed = JSON.parse(readFileSync(manifestPath, "utf8")) as unknown;
    if (!isRecord(parsed)) throw new Error("Plugin manifest must be a JSON object");
    const name = typeof parsed.name === "string" ? parsed.name.trim() : "";
    if (!PLUGIN_NAME_PATTERN.test(name)) throw new Error(`Invalid plugin name: ${name}`);
    return {
      manifest: {
        ...parsed,
        name,
        version: typeof parsed.version === "string" ? parsed.version : DEFAULT_VERSION,
      } as PluginManifest,
      manifestPath,
    };
  }
  if (entry.strict === false) {
    const rawManifest = createManifestFromMarketplaceEntry(entry);
    return {
      manifest: rawManifest as unknown as PluginManifest,
    };
  }
  return null;
}

function validatePluginRoot(input: {
  entry: PluginMarketplaceEntry;
  marketplace: string;
  rootPath: string;
  storageRoot: string;
}): PluginValidationDiagnostic[] {
  const diagnostics: PluginValidationDiagnostic[] = [];
  const pluginId = `${input.entry.name}@${input.marketplace}`;
  let loadedManifest: { manifest: PluginManifest; manifestPath?: string } | null = null;
  try {
    loadedManifest = readPluginManifestFromRoot(input.rootPath, input.entry);
  } catch (error) {
    diagnostics.push({
      code: "plugin_manifest_invalid",
      message: error instanceof Error ? error.message : String(error),
      path: input.rootPath,
      pluginId,
      severity: "error",
    });
    return diagnostics;
  }

  if (!loadedManifest) {
    diagnostics.push({
      code: "plugin_manifest_not_found",
      message: `Plugin manifest not found: ${pluginId}`,
      path: input.rootPath,
      pluginId,
      severity: "error",
    });
    return diagnostics;
  }

  const manifestPath = loadedManifest.manifestPath ?? input.rootPath;
  if (loadedManifest.manifest.name !== input.entry.name) {
    diagnostics.push({
      code: "plugin_manifest_invalid",
      message: `Plugin manifest name '${loadedManifest.manifest.name}' does not match marketplace entry '${input.entry.name}'`,
      path: manifestPath,
      pluginId,
      severity: "error",
    });
  }
  pushManifestCompatibilityDiagnostics({
    diagnostics,
    manifest: loadedManifest.manifest,
    manifestPath,
    pluginId,
    source: "cache",
  });
  pushMcpValidationDiagnostics({
    diagnostics,
    manifest: loadedManifest.manifest,
    manifestPath,
    marketplace: input.marketplace,
    pluginId,
    rootPath: input.rootPath,
    storageRoot: input.storageRoot,
  });
  return diagnostics;
}

function pushManifestCompatibilityDiagnostics(input: {
  diagnostics: PluginValidationDiagnostic[];
  manifest: PluginManifest;
  manifestPath: string;
  pluginId: string;
  source: "cache" | "inline" | "official";
}): void {
  for (const key of UNSUPPORTED_MANIFEST_FIELDS) {
    if (key in input.manifest) {
      input.diagnostics.push({
        code: "plugin_unsupported_component",
        message: `Plugin component is diagnostic-only in this ZCode runtime: ${key}`,
        path: input.manifestPath,
        pluginId: input.pluginId,
        severity: "warning",
      });
    }
  }
  for (const [key, option] of Object.entries(input.manifest.userConfig ?? {})) {
    if (option.required === true && option.default === undefined) {
      input.diagnostics.push({
        code: "plugin_variable_missing",
        message: `Required plugin userConfig has no default and must be configured: ${key}`,
        path: input.manifestPath,
        pluginId: input.pluginId,
        severity: "warning",
      });
    }
  }
  if (containsMcpBundleSource(input.manifest.mcpServers)) {
    input.diagnostics.push({
      code: "plugin_marketplace_source_unsupported",
      message: "MCPB/DXT plugin bundles are recognized but not supported in this runtime",
      path: input.manifestPath,
      pluginId: input.pluginId,
      severity: "warning",
    });
  }
}

function pushMcpValidationDiagnostics(input: {
  diagnostics: PluginValidationDiagnostic[];
  manifest: PluginManifest;
  manifestPath: string;
  marketplace: string;
  pluginId: string;
  rootPath: string;
  storageRoot: string;
}): void {
  const diagnostics = input.diagnostics as PluginDiagnostic[];
  const loaded = {
    id: input.pluginId,
    manifest: input.manifest,
    manifestPath: input.manifestPath,
    marketplace: input.marketplace,
    rootPath: input.rootPath,
    source: "cache" as const,
  };
  const definitions = loadPluginMcpServerDefinitions({ diagnostics, loaded });
  resolvePluginMcpServers({
    dataPath: join(input.storageRoot, "data", sanitizePluginId(input.pluginId)),
    definitions,
    diagnostics,
    env: {},
    loaded,
    options: {},
    workingDirectory: process.cwd(),
  });
}

function containsMcpBundleSource(value: unknown): boolean {
  if (typeof value === "string") return value.endsWith(".mcpb") || value.endsWith(".dxt");
  if (Array.isArray(value)) return value.some(containsMcpBundleSource);
  return false;
}

function validateMarketplaceEntryShape(
  entry: PluginMarketplaceEntry,
  marketplace: string,
  options: { includeEntryCompatibility?: boolean } = {},
): PluginValidationDiagnostic[] {
  const diagnostics: PluginValidationDiagnostic[] = [];
  const pluginId = `${entry.name}@${marketplace}`;
  if (entry.source === undefined) {
    diagnostics.push({
      code: "plugin_marketplace_invalid",
      message: `Plugin has no install source: ${pluginId}`,
      pluginId,
      severity: "error",
    });
  }
  if (isRecord(entry.source)) {
    const sourceKind = typeof entry.source.source === "string" ? entry.source.source : "";
    if (sourceKind === "npm" || sourceKind === "pip") {
      diagnostics.push({
        code: "plugin_marketplace_source_unsupported",
        message: `Plugin source is recognized but not supported in V1 install: ${sourceKind}`,
        pluginId,
        severity: "warning",
      });
    }
    if (sourceKind === "url") {
      const sourceType = typeof entry.source.type === "string" ? entry.source.type : "";
      if (sourceType && sourceType !== "git" && sourceType !== "zip") {
        diagnostics.push({
          code: "plugin_marketplace_source_unsupported",
          message: `Plugin URL source type is not supported: ${sourceType}`,
          pluginId,
          severity: "error",
        });
      }
      try {
        readRequiredPluginSourceString(entry.source, "url", "URL");
        if (sourceType === "zip") {
          const sha256 = readRequiredZipPluginSourceSha256(entry.source).toLowerCase();
          if (!SOURCE_SHA256_PATTERN.test(sha256)) {
            throw new Error("Plugin zip source sha256 must be a 64 character hex string");
          }
          readPluginSourceHeaders(entry.source);
          readOptionalZipPluginSourcePath(entry.source);
          readOptionalZipPluginSourceStripRoot(entry.source);
        }
      } catch (error) {
        diagnostics.push({
          code: "plugin_marketplace_invalid",
          message: error instanceof Error ? error.message : String(error),
          pluginId,
          severity: "error",
        });
      }
    }
  }
  if (options.includeEntryCompatibility !== false) {
    pushEntryCompatibilityDiagnostics({ diagnostics, entry, marketplace });
  }
  return diagnostics;
}

function pushEntryCompatibilityDiagnostics(input: {
  diagnostics: PluginValidationDiagnostic[];
  entry: PluginMarketplaceEntry;
  marketplace: string;
}): void {
  const pluginId = `${input.entry.name}@${input.marketplace}`;
  pushManifestCompatibilityDiagnostics({
    diagnostics: input.diagnostics,
    manifest: createManifestFromMarketplaceEntry(input.entry) as unknown as PluginManifest,
    manifestPath: pluginId,
    pluginId,
    source: "cache",
  });
}

function getMarketplaceSourceValidationDeferral(
  entry: PluginMarketplaceEntry,
  marketplace: string,
): PluginValidationDiagnostic | null {
  if (!isRecord(entry.source)) return null;
  const sourceKind = typeof entry.source.source === "string" ? entry.source.source : "";
  if (sourceKind === "url") {
    const sourceType = typeof entry.source.type === "string" ? entry.source.type : "";
    if (sourceType && sourceType !== "git" && sourceType !== "zip") return null;
  }
  if (!["github", "git", "url", "git-subdir"].includes(sourceKind)) return null;
  const pluginId = `${entry.name}@${marketplace}`;
  const sourceLabel =
    typeof entry.source.repo === "string"
      ? entry.source.repo
      : typeof entry.source.url === "string"
        ? entry.source.url
        : sourceKind;
  return {
    code: "plugin_validation_deferred",

// The aggregation market may contain a large number of external git sources; market-level validate does not clone one by one, when installing or verifying a single plug-in
// Then scan the target root deeply to prevent the settings page from being dragged by network operations until the protocol times out.
    message: `Remote plugin source validation is deferred until install or single-plugin validate: ${sourceLabel}`,
    pluginId,
    severity: "warning",
  };
}

function pushDependencyDiagnosticsFromManifest(input: {
  diagnostics: PluginValidationDiagnostic[];
  manifest: PluginMarketplaceManifest;
  marketplace: string;
  name: string;
  storageRoot: string;
}): void {
  try {
    resolveDependencyClosureFromManifest({
      allowCrossMarketplaces: new Set(input.manifest.allowCrossMarketplaceDependenciesOn ?? []),
      marketplace: input.marketplace,
      manifest: input.manifest,
      name: input.name,
      storageRoot: input.storageRoot,
    });
  } catch (error) {
    input.diagnostics.push(toValidationDiagnostic(error, `${input.name}@${input.marketplace}`));
  }
}

function resolveDependencyClosureFromManifest(input: {
  allowCrossMarketplaces: ReadonlySet<string>;
  marketplace: string;
  manifest: PluginMarketplaceManifest;
  name: string;
  storageRoot: string;
}): string[] {
  const rootId = `${input.name}@${input.marketplace}`;
  const closure: string[] = [];
  const visiting: string[] = [];
  const visited = new Set<string>();

  const loadManifest = (marketplace: string): PluginMarketplaceManifest | null =>
    marketplace === input.marketplace
      ? input.manifest
      : loadMarketplaceManifestSync(input.storageRoot, marketplace);

  const walk = (pluginId: string, requiredBy: string): void => {
    const { marketplace, name } = parsePluginId(pluginId);
    if (marketplace !== input.marketplace && !input.allowCrossMarketplaces.has(marketplace)) {
      throw new Error(
        `Cross-marketplace dependency is blocked: ${pluginId} required by ${requiredBy}`,
      );
    }
    if (visiting.includes(pluginId)) {
      throw new Error(`Plugin dependency cycle: ${[...visiting, pluginId].join(" -> ")}`);
    }
    if (visited.has(pluginId)) return;

    const manifest = loadManifest(marketplace);
    if (!manifest) throw new Error(`Marketplace not found for dependency: ${marketplace}`);
    const entry = manifest.plugins.find((plugin) => plugin.name === name);
    if (!entry) throw new Error(`Dependency not found: ${pluginId} required by ${requiredBy}`);

    visiting.push(pluginId);
    for (const dependency of entry.dependencies ?? []) {
      walk(qualifyDependency(dependency, marketplace), pluginId);
    }
    visiting.pop();
    visited.add(pluginId);
    closure.push(pluginId);
  };

  walk(rootId, rootId);
  return closure;
}

function pushDependencyDiagnostics(input: {
  diagnostics: PluginValidationDiagnostic[];
  marketplace: string;
  name: string;
  storageRoot: string;
}): void {
  try {
    const rootManifest = loadMarketplaceManifestSync(input.storageRoot, input.marketplace);
    resolveDependencyClosure({
      allowCrossMarketplaces: new Set(rootManifest?.allowCrossMarketplaceDependenciesOn ?? []),
      marketplace: input.marketplace,
      name: input.name,
      storageRoot: input.storageRoot,
    });
  } catch (error) {
    input.diagnostics.push(toValidationDiagnostic(error, `${input.name}@${input.marketplace}`));
  }
}

function toValidationDiagnostic(error: unknown, pluginId?: string): PluginValidationDiagnostic {
  if (
    error instanceof UnsupportedMarketplaceSourceError ||
    error instanceof UnsupportedPluginSourceError
  ) {
    return {
      code: "plugin_marketplace_source_unsupported",
      message: error.message,
      ...(pluginId ? { pluginId } : {}),
      severity: "error",
    };
  }
  const sourceCode = getPluginSourceDiagnosticCode(error);
  if (sourceCode) {
    return {
      code: sourceCode,
      message: error instanceof Error ? error.message : String(error),
      ...(pluginId ? { pluginId } : {}),
      severity: "error",
    };
  }
  const message = error instanceof Error ? error.message : String(error);
  if (message.includes("Cross-marketplace dependency")) {
    return {
      code: "plugin_dependency_cross_marketplace",
      message,
      ...(pluginId ? { pluginId } : {}),
      severity: "error",
    };
  }
  if (message.includes("dependency cycle")) {
    return {
      code: "plugin_dependency_cycle",
      message,
      ...(pluginId ? { pluginId } : {}),
      severity: "error",
    };
  }
  if (
    message.includes("Dependency not found") ||
    message.includes("Marketplace not found for dependency")
  ) {
    return {
      code: "plugin_dependency_missing",
      message,
      ...(pluginId ? { pluginId } : {}),
      severity: "error",
    };
  }
  return {
    code: "plugin_marketplace_invalid",
    message,
    ...(pluginId ? { pluginId } : {}),
    severity: "error",
  };
}

class UnsupportedMarketplaceSourceError extends Error {
  constructor(source: string) {
    super(`Marketplace source is recognized but not supported in this runtime: ${source}`);
  }
}

class UnsupportedPluginSourceError extends Error {
  constructor(source: string) {
    super(`Plugin source is recognized but not supported in this runtime: ${source}`);
  }
}

function isPluginMarketplaceManifest(value: unknown): value is PluginMarketplaceManifest {
  return (
    isRecord(value) &&
    typeof value.name === "string" &&
    Array.isArray(value.plugins) &&
    isRecord(value.raw)
  );
}

function readJsonFileSync(path: string): unknown {
  const readablePath = recoverAtomicTargetSync(path);
  try {
    return JSON.parse(readFileSync(readablePath, "utf8")) as unknown;
  } catch {
    return undefined;
  }
}

async function writeJsonFile(path: string, value: unknown): Promise<void> {
  await writeFileAtomically(path, `${JSON.stringify(value, null, 2)}\n`);
}

function getPluginCacheDir(
  storageRoot: string,
  marketplace: string,
  name: string,
  version: string,
): string {
  return join(
    storageRoot,
    "cache",
    sanitizePluginId(marketplace),
    sanitizePluginId(name),
    sanitizePluginId(version),
  );
}

export function getPluginDataDir(storageRoot: string, pluginId: string): string {
// Consistent with the dataPath resolution of NodePluginAdapter.discoverPluginsSync: <storageRoot>/data/<sanitized-id>.
  return join(storageRoot, "data", sanitizePluginId(pluginId));
}

function isKnownMarketplaceRecord(value: unknown): value is KnownMarketplaceRecord {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    typeof value.name === "string" &&
    typeof value.pluginCount === "number" &&
    isRecord(value.source)
  );
}

function isInstalledPluginRecord(value: unknown): value is InstalledPluginRecord {
  return (
    isRecord(value) &&
    typeof value.id === "string" &&
    typeof value.name === "string" &&
    typeof value.marketplace === "string" &&
    typeof value.version === "string" &&
    typeof value.installPath === "string" &&
    typeof value.installedAt === "string" &&
    (value.scope === "user" || value.scope === "workspace")
  );
}

function parsePluginId(pluginId: string): { marketplace: string; name: string } {
  const at = pluginId.lastIndexOf("@");
  if (at <= 0 || at === pluginId.length - 1) {
    throw new Error(`Plugin id must use <name>@<marketplace>: ${pluginId}`);
  }
  return {
    name: pluginId.slice(0, at),
    marketplace: pluginId.slice(at + 1),
  };
}

function qualifyDependency(dependency: string, marketplace: string): string {
  return dependency.includes("@") ? dependency : `${dependency}@${marketplace}`;
}

function resolvePathInput(input: string): string | null {
  if (
    input.startsWith("./") ||
    input.startsWith("../") ||
    input.startsWith("/") ||
    input.startsWith("~") ||
    /^[a-zA-Z]:[/\\]/.test(input)
  ) {
    return input.startsWith("~") ? join(process.env.HOME ?? "", input.slice(1)) : resolve(input);
  }
  return null;
}

function splitRef(input: string): { ref?: string; url: string } {
  const index = input.lastIndexOf("#");
  if (index < 0) return { url: input };
  return { url: input.slice(0, index), ref: input.slice(index + 1) };
}

function splitGitHubShorthand(input: string): { ref?: string; url: string } {
  const hash = input.lastIndexOf("#");
  const at = input.lastIndexOf("@");
  const index = Math.max(hash, at);
  if (index <= 0) return { url: input };
  return { url: input.slice(0, index), ref: input.slice(index + 1) };
}

function isGitSshUrl(input: string): boolean {
  return /^[a-zA-Z0-9._-]+@[^:]+:.+/.test(input);
}

function tryParseUrl(input: string): URL | null {
  try {
    return new URL(input);
  } catch {
    return null;
  }
}
