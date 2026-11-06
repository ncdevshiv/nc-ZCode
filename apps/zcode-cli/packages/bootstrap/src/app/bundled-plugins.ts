/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve, sep } from "node:path";
import { writeBundledOfficialMarketplacePartitionSync } from "@zcode/adapters";
import { ZCODE_OFFICIAL_PLUGIN_MARKETPLACE, type Logger } from "@zcode/contracts";
import { isZCodeCuaInternalFeatureEnabled, ZCODE_CUA_OFFICIAL_PLUGIN_ID } from "@zcode/shared";
import {
  createOfficialPluginCacheRetryBudget,
  getOfficialPluginCacheRetryAttempts,
  isTransientOfficialPluginCacheFsError,
  type OfficialPluginCacheRetryBudget,
  removeOfficialPluginCacheDirectory,
  renameOfficialPluginCachePath,
} from "./official-plugin-cache-fs.js";
import {
  OFFICIAL_PLUGIN_DEFINITIONS,
  type OfficialPluginDefinition,
} from "./official-plugin-definitions.js";
import { writeOfficialPluginRuntimeManifest } from "./official-plugin-runtime.js";
import {
  isOfficialPluginSeedLockTimeoutError,
  withOfficialPluginSeedLock,
} from "./official-plugin-seed-lock.js";

const OFFICIAL_PLUGIN_MARKETPLACE = ZCODE_OFFICIAL_PLUGIN_MARKETPLACE;
const SEA_PLUGIN_ASSET_PREFIX = "zcode-official-plugins/";
const SEA_PLUGIN_MANIFEST_ASSET_KEY = `${SEA_PLUGIN_ASSET_PREFIX}manifest.json`;
const SEED_MARKER_FILE = ".zcode-plugin-seed.json";
const SEED_LOCK_TOTAL_BUDGET_MS = 15_000;

const includedTopLevelPaths = new Set([
  ".mcp.json",
  ".zcode-plugin",
  "README.md",
// After adding agents to the official content plug-in, the top-level whitelist of filesystem seed is not synchronized and the directory is silently pruned.
  "agents",
  "commands",
  "dist",
  "docs",
  "hooks",
  "output-styles",
  "package.json",
// Browser skill will dynamically import scripts/browser-client.mjs from the official plug-in root directory.
// If scripts are missing from the filesystem seed, Dev will successfully connect to node_repl but fail to import during the first Browser Use.
  "scripts",
  "skills",
  "templates",
]);

interface OfficialPluginSeedFile {
  mode?: number;
  path: string;
  sha256: string;
  sourcePath?: string;
}

interface OfficialPluginSeedPluginSource {
  definition: OfficialPluginDefinition;
  files: OfficialPluginSeedFile[];
  hash: string;
  missingSeedPaths: string[];
  rootPath?: string;
}

interface OfficialPluginSeedSource {
  kind: "filesystem" | "sea";
  plugins: OfficialPluginSeedPluginSource[];
}

interface SeaOfficialPluginManifest {
  hash: string;
  plugins: Array<{
    files: OfficialPluginSeedFile[];
    marketplace: string;
    name: string;
    version: string;
  }>;
  version: 1;
}

type SeaModule = typeof import("node:sea");

function seedBundledOfficialPlugins(input: {
  logger?: Logger;
  storageRoot: string;
}): OfficialPluginDefinition[] {
  const source = resolveSeedSource();
  if (!source) return [];

// Catalog/cache is an immutable product asset of the built-in plug-in; whether Runtime is loaded is determined by the suppression state of discovery.
// Cannot be deleted or filtered in the seed stage, otherwise the details page cannot read the component after uninstalling, and it cannot be restored.
  writeOfficialMarketplace(input.storageRoot, source);
  const retryBudget = createOfficialPluginCacheRetryBudget();
// The loop will continue after the lock times out and degrades; if each plug-in is independently reset for 15 seconds and waits for the budget, the group of remaining
// The lock will freeze startup synchronization for N×15s. All plugins share the same deadline: non-contention locks are still acquired instantaneously (mkdir
// Budget is not checked for one success). After the budget is exhausted, the contentioned locks are immediately downgraded, and the seeding wait is capped for 15 seconds.
  const seedLockDeadlineAt = Date.now() + SEED_LOCK_TOTAL_BUDGET_MS;
  const failedSeeds: OfficialPluginDefinition[] = [];
  for (const plugin of source.plugins) {
    const pluginId = `${plugin.definition.name}@${OFFICIAL_PLUGIN_MARKETPLACE}`;
    const targetRoot = officialPluginCacheRoot(input.storageRoot, plugin.definition);
// The copy of the plugin next to the entry may mismatch the new definition (desktop package being upgraded, old checkout not building dist).
// When requiredSeedPaths is missing, the seed source parsing directly throws an error. A defective plug-in restores all plug-ins together with the session.
// Explode together into resumeFailed. The mutilation only affects a single plug-in: it refuses to write to the cache, warns according to the existing downgrade protocol, and falls back to the old available cache.
    if (plugin.missingSeedPaths.length > 0) {
      warnCacheDegraded(input.logger, {
        error: Object.assign(
          new Error(
            `Bundled official plugin ${plugin.definition.name} is missing required seed assets: ${plugin.missingSeedPaths.join(", ")}`,
          ),
          { code: "ZCODE_PLUGIN_SEED_INCOMPLETE" },
        ),
        missingSeedPaths: plugin.missingSeedPaths,
        operation: "seed_plugin",
        pluginId,
        targetRoot,
      });
      failedSeeds.push(plugin.definition);
      continue;
    }
    try {
      withOfficialPluginSeedLock(
        targetRoot,
        () => {
// The desktop will preheat multiple workspace agents concurrently; when copying plug-in resources,
//Multiple processes will delete each other's targets and trigger EPERM when Windows renames. After taking the lock, you must check it twice.
// Let the waiter directly reuse the complete cache that has been submitted by the first process.
          if (isSeedCurrent(targetRoot, plugin)) {
            cleanupLegacySeedBackup(targetRoot, retryBudget);
            const manifestWritten = tryWriteOfficialPluginRuntimeManifest({
              pluginName: plugin.definition.name,
              retryBudget,
              rootPath: targetRoot,
            });
            if (manifestWritten) return;
          }

          const temporaryRoot = `${targetRoot}.tmp-${process.pid}-${Date.now()}`;
          removeOfficialPluginCacheDirectory(temporaryRoot, retryBudget);
          mkdirSync(temporaryRoot, { recursive: true });

          try {
            for (const file of plugin.files) {
              const bytes = readSeedFileBytes(source, plugin, file);
              if (hashBytes(bytes) !== file.sha256) {
                throw new Error(
                  `Bundled plugin asset hash mismatch: ${plugin.definition.name}/${file.path}`,
                );
              }
              const outputPath = join(temporaryRoot, ...file.path.split("/"));
              mkdirSync(dirname(outputPath), { recursive: true });
              writeFileSync(outputPath, bytes);
              chmodSync(outputPath, modeForSeedFile(file.path, file.mode));
            }

            writeFileSync(
              join(temporaryRoot, SEED_MARKER_FILE),
              JSON.stringify(seedMarker(source, plugin), null, 2),
            );
            replaceSeedRoot(temporaryRoot, targetRoot, plugin, retryBudget);
            writeOfficialPluginRuntimeManifest({
              pluginName: plugin.definition.name,
              retryBudget,
              rootPath: targetRoot,
            });
          } catch (error) {
            try {
              removeOfficialPluginCacheDirectory(temporaryRoot, retryBudget);
            } catch {
// Failure to clean the temporary directory cannot cover the real seed error; the directory name is unique and will not contaminate subsequent loads.
            }
            throw error;
          }
        },
        { timeoutMs: Math.max(0, seedLockDeadlineAt - Date.now()) },
      );
    } catch (error) {
      if (
        isTransientOfficialPluginCacheFsError(error) ||
// The seed lock waiting timeout only means that the cache lock of the same version is held by another process or left behind (on Windows
// Legacy locks that cannot be deleted + PID reuse will prevent takeover from being triggered for a long time). seeding just refreshes the cache, the timeout must
// Use the existing downgrade protocol to fall back to the available cache and alert. The entire session recovery cannot be resumeFailed.
        isOfficialPluginSeedLockTimeoutError(error) ||
// Multiple workspace apps will seed the same official plug-in cache concurrently. current process
// When writing the runtime manifest, the concurrent winner may have atomically replaced the entire targetRoot, along with this process
// Temporary files are removed together, rename therefore returns ENOENT. Only if the new target has been certified by marker
// Downgrade when complete; continue to throw errors when the target is missing or still exists, which cannot cover up the real cache damage.
        (isNotFoundFsError(error) && isSeedCurrent(targetRoot, plugin))
      ) {
        warnCacheDegraded(input.logger, {
          error,
          operation: "seed_plugin",
          pluginId,
          targetRoot,
        });
        failedSeeds.push(plugin.definition);
        continue;
      }
      throw error;
    }
  }
  return failedSeeds;
}

function tryWriteOfficialPluginRuntimeManifest(input: {
  pluginName: string;
  retryBudget: OfficialPluginCacheRetryBudget;
  rootPath: string;
}): boolean {
  try {
    writeOfficialPluginRuntimeManifest(input);
    return true;
  } catch (error) {
    if (isTransientOfficialPluginCacheFsError(error)) throw error;
    return false;
  }
}

export function resolveOfficialPluginRoots(input: {
  env?: NodeJS.ProcessEnv;
  extraRoots?: string[];
  logger?: Logger;
  storageRoot: string;
  suppressedBuiltins?: ReadonlySet<string>;
}): string[] {
  const suppressedBuiltins = new Set(input.suppressedBuiltins ?? []);
// The zcode-cua built-in plugin is not enabled by default, and is loaded by the feature flag. Gating at the seed/discovery level
// (instead of just hiding a certain UI surface), so that when the switch is turned off, the user cannot see it through the plugin list/marketplace/MCP settings/CLI command.
  if (!isZCodeCuaInternalFeatureEnabled(input.env ?? process.env)) {
    suppressedBuiltins.add(ZCODE_CUA_OFFICIAL_PLUGIN_ID);
  }
  const failedSeeds = seedBundledOfficialPlugins({
    logger: input.logger,
    storageRoot: input.storageRoot,
  });

  const fallbackRoots = failedSeeds.flatMap((definition) => {
// CUA's frame contract is atomically upgraded along with wrapper and producer. Load old version
// The cache will connect the old block layout to the new consumer; it would rather not register CUA when the current cache is unavailable.
    if (`${definition.name}@${OFFICIAL_PLUGIN_MARKETPLACE}` === ZCODE_CUA_OFFICIAL_PLUGIN_ID) {
      return [];
    }
    const fallbackRoot = findUsableOfficialPluginFallback(input.storageRoot, definition);
    return fallbackRoot ? [fallbackRoot] : [];
  });
  return uniquePaths([...(input.extraRoots ?? []), ...fallbackRoots]);
}

function resolveSeedSource(): OfficialPluginSeedSource | undefined {
  const seaSource = resolveSeaSeedSource();
  if (seaSource) return seaSource;
  return resolveFilesystemSeedSource();
}

function resolveSeaSeedSource(): OfficialPluginSeedSource | undefined {
  const sea = getSeaModule();
  if (!sea?.isSea()) return undefined;

  const manifest = readSeaManifest(sea);
  if (!manifest) return undefined;
  const plugins = OFFICIAL_PLUGIN_DEFINITIONS.flatMap((definition) => {
    const plugin = manifest.plugins.find(
      (item) =>
        item.marketplace === OFFICIAL_PLUGIN_MARKETPLACE &&
        item.name === definition.name &&
        item.version === definition.version,
    );
    if (!plugin) return [];
    return [
      {
        definition,
        files: plugin.files,
        hash: hashSeedFiles(plugin.files),
        missingSeedPaths: findMissingOfficialPluginSeedPaths(definition, plugin.files),
      },
    ];
  });
  if (plugins.length === 0) return undefined;

  return {
    kind: "sea",
    plugins,
  };
}

function resolveFilesystemSeedSource(): OfficialPluginSeedSource | undefined {
  const plugins = OFFICIAL_PLUGIN_DEFINITIONS.flatMap((definition) => {
    const rootPath = resolveFilesystemPluginRoot(definition);
    if (!rootPath) return [];
    const files = collectFilesystemPluginFiles(rootPath, definition);
    return [
      {
        definition,
        files,
        hash: hashSeedFiles(files),
        missingSeedPaths: findMissingOfficialPluginSeedPaths(definition, files),
        rootPath,
      },
    ];
  });
  if (plugins.length === 0) return undefined;
  return {
    kind: "filesystem",
    plugins,
  };
}

function findMissingOfficialPluginSeedPaths(
  definition: Pick<OfficialPluginDefinition, "requiredSeedPaths">,
  files: ReadonlyArray<{ path: string }>,
): string[] {
  const availablePaths = new Set(files.map((file) => file.path));
  return (definition.requiredSeedPaths ?? []).filter(
    (requiredPath) => !availablePaths.has(requiredPath),
  );
}

function getSeaModule(): SeaModule | undefined {
  const getBuiltinModule = process.getBuiltinModule as ((id: "node:sea") => SeaModule) | undefined;
  try {
    return getBuiltinModule?.("node:sea");
  } catch {
    return undefined;
  }
}

function readSeaManifest(sea: SeaModule): SeaOfficialPluginManifest | undefined {
  try {
    const raw = sea.getAsset(SEA_PLUGIN_MANIFEST_ASSET_KEY, "utf8");
    const manifest = JSON.parse(raw) as SeaOfficialPluginManifest;
    return manifest.version === 1 && Array.isArray(manifest.plugins) ? manifest : undefined;
  } catch {
    return undefined;
  }
}

function resolveFilesystemPluginRoot(definition: OfficialPluginDefinition): string | undefined {
  for (const baseDir of candidateBaseDirs()) {
    for (const relativePath of definition.rootCandidates) {
      const rootPath = resolve(baseDir, relativePath);
      if (existsSync(join(rootPath, ".zcode-plugin", "plugin.json"))) return rootPath;
    }
  }
  return undefined;
}

function collectFilesystemPluginFiles(
  rootPath: string,
  definition: OfficialPluginDefinition,
): OfficialPluginSeedFile[] {
  const files: OfficialPluginSeedFile[] = [];
  const allowedTopLevelPaths = new Set([
    ...includedTopLevelPaths,
    ...(definition.runtimeTopLevelPaths ?? []),
  ]);
  for (const sourcePath of walkFiles(rootPath, allowedTopLevelPaths)) {
    const relativePath = toPosixPath(sourcePath.slice(rootPath.length + 1));
    if (!shouldIncludePluginFile(relativePath, allowedTopLevelPaths)) continue;
    const bytes = readFileSync(sourcePath);
    files.push({
      mode: modeForSeedFile(relativePath, statSync(sourcePath).mode),
      path: relativePath,
      sha256: hashBytes(bytes),
      sourcePath,
    });
  }
  return files.sort((left, right) => left.path.localeCompare(right.path));
}

function* walkFiles(
  directory: string,
  allowedTopLevelPaths: ReadonlySet<string>,
  depth = 0,
): Generator<string> {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (shouldSkipDirectory(entry.name, depth, allowedTopLevelPaths)) continue;
    const fullPath = join(directory, entry.name);
    if (entry.isDirectory()) {
      yield* walkFiles(fullPath, allowedTopLevelPaths, depth + 1);
      continue;
    }
    if (entry.isFile()) yield fullPath;
  }
}

function readSeedFileBytes(
  source: OfficialPluginSeedSource,
  plugin: OfficialPluginSeedPluginSource,
  file: OfficialPluginSeedFile,
): Buffer {
  if (source.kind === "filesystem" && file.sourcePath) return readFileSync(file.sourcePath);
  const sea = getSeaModule();
  if (!sea?.isSea()) throw new Error("SEA plugin asset is unavailable outside SEA runtime.");
  return Buffer.from(
    sea.getRawAsset(
      `${SEA_PLUGIN_ASSET_PREFIX}${OFFICIAL_PLUGIN_MARKETPLACE}/${plugin.definition.name}/${plugin.definition.version}/${file.path}`,
    ),
  );
}

function writeOfficialMarketplace(storageRoot: string, source: OfficialPluginSeedSource): void {
  writeBundledOfficialMarketplacePartitionSync({
    manifest: {
      name: OFFICIAL_PLUGIN_MARKETPLACE,
      plugins: source.plugins.map((plugin) => {
// Store information (listing) and description are delivered with the directory entry: the key name is consistent with the CDN directory schema,
// Parsed by the same parseEntryStoreListing of the adapter, the UI can be rendered to the built-in plug-in
//Display name/category/author/example prompt words. The description is taken from plugin.json (single source of truth) within the plugin package.
        const description = readSeedPluginDescription(source, plugin);
        return {
          cachePath: officialPluginCacheRoot(storageRoot, plugin.definition),
          ...(description ? { description } : {}),
          name: plugin.definition.name,
          source: source.kind,
          version: plugin.definition.version,
          ...(plugin.definition.listing ?? {}),
        };
      }),
      version: 1,
    },
    storageRoot,
  });
}

/** Read the description of the plug-in plugin.json from the seed file set; if the reading/parsing fails, it will be downgraded to undefined. */
function readSeedPluginDescription(
  source: OfficialPluginSeedSource,
  plugin: OfficialPluginSeedPluginSource,
): string | undefined {
  const manifestFile = plugin.files.find((file) => file.path === ".zcode-plugin/plugin.json");
  if (!manifestFile) return undefined;
  try {
    const parsed = JSON.parse(readSeedFileBytes(source, plugin, manifestFile).toString("utf8")) as {
      description?: unknown;
    };
    return typeof parsed.description === "string" && parsed.description.trim().length > 0
      ? parsed.description
      : undefined;
  } catch {
    return undefined;
  }
}

function isSeedCurrent(targetRoot: string, plugin: OfficialPluginSeedPluginSource): boolean {
  const markerPath = join(targetRoot, SEED_MARKER_FILE);
  if (!existsSync(markerPath)) return false;
  try {
    const marker = JSON.parse(readFileSync(markerPath, "utf8")) as ReturnType<typeof seedMarker>;
    return marker.hash === plugin.hash && marker.pluginVersion === plugin.definition.version;
  } catch {
    return false;
  }
}

/**
* The old version cache can continue to serve the current session as long as the plugin manifest and files required to run are complete.
* A mismatch of marker hashes only indicates the need for an upgrade, and an available old cache cannot be regarded as a startup failure.
 */
function isSeedUsable(targetRoot: string, definition: OfficialPluginDefinition): boolean {
  try {
    const manifest = JSON.parse(
      readFileSync(join(targetRoot, ".zcode-plugin", "plugin.json"), "utf8"),
    ) as { name?: unknown };
    if (manifest.name !== definition.name) return false;
  } catch {
    return false;
  }

  return (definition.requiredSeedPaths ?? []).every((requiredPath) =>
    existsSync(join(targetRoot, ...requiredPath.split("/"))),
  );
}

function findUsableOfficialPluginFallback(
  storageRoot: string,
  definition: OfficialPluginDefinition,
): string | undefined {
  const targetRoot = officialPluginCacheRoot(storageRoot, definition);
  if (isSeedUsable(targetRoot, definition)) return undefined;

  let entries;
  try {
    entries = readdirSync(dirname(targetRoot), { withFileTypes: true });
  } catch (error) {
    if (isNotFoundFsError(error)) return undefined;
    throw error;
  }

  return entries
    .filter(
      (entry) =>
        entry.isDirectory() &&
        entry.name !== definition.version &&
        !entry.name.includes(".backup") &&
// The lock directory (including .seed-lock.stale-*) is at the same level as the version directory; the lock must be present after timeout and downgrade.
// Cannot rely on the content check of isSeedUsable, and exclude it directly by name.
        !entry.name.includes(".seed-lock") &&
        !entry.name.includes(".tmp-"),
    )
    .sort((left, right) =>
      right.name.localeCompare(left.name, undefined, { numeric: true, sensitivity: "base" }),
    )
    .map((entry) => join(dirname(targetRoot), entry.name))
    .find((rootPath) => isSeedUsable(rootPath, definition));
}

function replaceSeedRoot(
  temporaryRoot: string,
  targetRoot: string,
  plugin: OfficialPluginSeedPluginSource,
  retryBudget: OfficialPluginCacheRetryBudget,
): void {
  const backupRoot = createSeedBackupRoot(targetRoot);
  let movedTargetToBackup = false;
  mkdirSync(dirname(targetRoot), { recursive: true });
  if (existsSync(targetRoot)) {
    try {
      renameOfficialPluginCachePath(targetRoot, backupRoot, retryBudget);
      movedTargetToBackup = true;
    } catch (error) {
// The official plug-in cache is shared by the desktop window, protocol and CLI entry. After existsSync,
// Another process may move the target first; here only the ENOENT of this TOCTOU is converged, and then continues
// promote or identify the concurrent winner by isSeedCurrent, other missing errors still maintain the original fatal semantics.
      if (!isNotFoundFsError(error)) throw error;
    }
  }

  try {
    renameOfficialPluginCachePath(temporaryRoot, targetRoot, retryBudget);
  } catch (error) {
    if (isSeedCurrent(targetRoot, plugin)) {
      removeOfficialPluginCacheDirectory(temporaryRoot, retryBudget);
      if (movedTargetToBackup) {
        removeOfficialPluginCacheDirectory(backupRoot, retryBudget);
      }
      cleanupLegacySeedBackup(targetRoot, retryBudget);
      return;
    }
    if (movedTargetToBackup && !existsSync(targetRoot) && existsSync(backupRoot)) {
      renameOfficialPluginCachePath(backupRoot, targetRoot, retryBudget);
    }
    throw error;
  }

  if (movedTargetToBackup) {
    removeOfficialPluginCacheDirectory(backupRoot, retryBudget);
  }
  cleanupLegacySeedBackup(targetRoot, retryBudget);
}

function cleanupLegacySeedBackup(
  targetRoot: string,
  retryBudget: OfficialPluginCacheRetryBudget,
): void {
  const backupRoot = `${targetRoot}.backup`;
  if (!existsSync(backupRoot)) return;

// The old version of fixed backup has no transaction ownership. When the target is temporarily missing, it may belong to another one that still exists.
// The promoted process cannot be restored. Clean this legacy directory only after the current seed has been confirmed to be available.
  removeOfficialPluginCacheDirectory(backupRoot, retryBudget);
}

function createSeedBackupRoot(targetRoot: string): string {
// Fixed backup will be used as a rollback point by concurrent startup processes; use a unique directory for each replacement,
// The catch branch only restores the target it moved out to prevent one process from stealing the transaction status of another process.
  return `${targetRoot}.backup-${process.pid}-${Date.now()}`;
}

function isNotFoundFsError(error: unknown): error is NodeJS.ErrnoException {
  return (
    typeof error === "object" &&
    error !== null &&
    String((error as NodeJS.ErrnoException).code) === "ENOENT"
  );
}

function warnCacheDegraded(
  logger: Logger | undefined,
  input: {
    error: NodeJS.ErrnoException;
    missingSeedPaths?: readonly string[];
    operation: "remove_suppressed_plugin" | "seed_plugin";
    pluginId: string;
    targetRoot: string;
  },
): void {
  logger?.warn("Official plugin cache operation degraded", {
    attempts: getOfficialPluginCacheRetryAttempts(input.error),
    degraded: true,
    errorCode: input.error.code,
    ...(input.missingSeedPaths ? { missingSeedPaths: input.missingSeedPaths } : {}),
    module: "bootstrap.official_plugin_cache",
    operation: input.operation,
    pluginId: input.pluginId,
    targetRoot: input.targetRoot,
  });
}

function seedMarker(source: OfficialPluginSeedSource, plugin: OfficialPluginSeedPluginSource) {
  return {
    hash: plugin.hash,
    marketplace: OFFICIAL_PLUGIN_MARKETPLACE,
    plugin: plugin.definition.name,
    pluginVersion: plugin.definition.version,
    source: source.kind,
    version: 1,
  };
}

function officialPluginCacheRoot(
  storageRoot: string,
  definition: OfficialPluginDefinition,
): string {
  return join(
    storageRoot,
    "cache",
    OFFICIAL_PLUGIN_MARKETPLACE,
    definition.name,
    definition.version,
  );
}

function candidateBaseDirs(): string[] {
// Electron app-server runs in resources/glm/zcode.cjs, and official plug-in resources are also included in the desktop package
// stage to sibling packages/*-plugin. The candidate directory must first look at the entry file directory to avoid the production state from returning to
// monorepo-only __dirname lookup assumption.
  return [entrypointDir(), runtimeDir(), process.cwd()].filter(
    (dir): dir is string => typeof dir === "string",
  );
}

function runtimeDir(): string | undefined {
  return typeof __dirname === "string" ? __dirname : undefined;
}

function entrypointDir(): string | undefined {
  return process.argv[1] ? dirname(process.argv[1]) : undefined;
}

function shouldSkipDirectory(
  name: string,
  depth: number,
  allowedTopLevelPaths: ReadonlySet<string>,
): boolean {
  if (name === ".turbo" || name === "coverage" || name === ".venv" || name === "__pycache__") {
    return true;
  }
  return name === "node_modules" && !(depth === 0 && allowedTopLevelPaths.has(name));
}

function shouldIncludePluginFile(
  relativePath: string,
  allowedTopLevelPaths: ReadonlySet<string>,
): boolean {
  const segments = relativePath.split("/");
  if (segments.includes(".DS_Store") || segments.some((segment) => segment.endsWith(".pyc"))) {
    return false;
  }
  const [topLevel] = relativePath.split("/");
  return topLevel !== undefined && allowedTopLevelPaths.has(topLevel);
}

function modeForSeedFile(filePath: string, sourceMode?: number): number {
  if (sourceMode !== undefined && (sourceMode & 0o111) !== 0) return 0o755;

  const normalizedPath = toPosixPath(filePath);
// official plugin seed will override cache file permissions. Some plugins use the polyglot shell wrapper
// Execute the hook script directly. If the disk is set to 0644, permission denied will occur. The source code execution bit is retained here,
// And take a closer look at the hook script that lacks mode in SEA/old manifest.
  if (/(?:^|\/)dist\/mcp\/server\.js$/i.test(normalizedPath)) return 0o755;
  if (/^hooks\//u.test(normalizedPath) && !/\.(json|md|txt)$/iu.test(normalizedPath)) {
    return 0o755;
  }

  return 0o644;
}

function hashSeedFiles(files: OfficialPluginSeedFile[]): string {
  return hashText(
    JSON.stringify(
      files.map((file) => [file.path, file.sha256, modeForSeedFile(file.path, file.mode)]),
    ),
  );
}

function toPosixPath(value: string): string {
  return value.split(sep).join("/");
}

function uniquePaths(paths: string[]): string[] {
  return paths.filter((path, index) => paths.indexOf(path) === index);
}

function hashBytes(bytes: Buffer): string {
  return createHash("sha256").update(bytes).digest("hex");
}

function hashText(text: string): string {
  return hashBytes(Buffer.from(text));
}
