import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { ZCODE_OFFICIAL_PLUGIN_MARKETPLACE } from "@zcode/contracts";

const BUNDLED_PARTITION_FILE = "bundled-marketplace.json";
const CDN_PARTITION_FILE = "cdn-marketplace.json";
const MERGED_MARKETPLACE_FILE = "marketplace.json";

interface BundledMarketplacePartition {
  manifest: Record<string, unknown>;
  version: 1;
}

export function writeBundledOfficialMarketplacePartitionSync(input: {
  manifest: Record<string, unknown>;
  storageRoot: string;
}): Record<string, unknown> {
  assertOfficialManifest(input.manifest);
  writeJsonFileSync(partitionPath(input.storageRoot, BUNDLED_PARTITION_FILE), {
    manifest: input.manifest,
    version: 1,
  } satisfies BundledMarketplacePartition);
  return rebuildOfficialMarketplaceSync(input.storageRoot);
}

export function writeCdnOfficialMarketplacePartitionSync(input: {
  manifest: Record<string, unknown>;
  storageRoot: string;
}): Record<string, unknown> {
  assertOfficialManifest(input.manifest);
  writeJsonFileSync(partitionPath(input.storageRoot, CDN_PARTITION_FILE), input.manifest);
  return rebuildOfficialMarketplaceSync(input.storageRoot);
}

export function loadBundledOfficialPluginRootsSync(
  storageRoot: string,
): string[] | undefined {
  const bundledPartition = readBundledPartition(storageRoot);
  if (!bundledPartition) return undefined;

  const officialCacheRoot = resolve(
    storageRoot,
    "cache",
    ZCODE_OFFICIAL_PLUGIN_MARKETPLACE,
  );
  return readPluginEntries(bundledPartition.manifest).flatMap((plugin) => {
    const name = readPluginName(plugin);
    const cachePath = typeof plugin.cachePath === "string" ? plugin.cachePath : undefined;
    if (!name || !cachePath) return [];

    const pluginCacheRoot = resolve(officialCacheRoot, name);
    const resolvedCachePath = resolve(cachePath);
    if (
      !isStrictDescendant(officialCacheRoot, pluginCacheRoot) ||
      !isStrictDescendant(pluginCacheRoot, resolvedCachePath)
    ) {
      return [];
    }
    return [resolvedCachePath];
  });
}

function rebuildOfficialMarketplaceSync(storageRoot: string): Record<string, unknown> {
  const bundledPartition = readBundledPartition(storageRoot);
  const cdnManifest = readJsonRecord(partitionPath(storageRoot, CDN_PARTITION_FILE));
  const bundledManifest = bundledPartition?.manifest;
  const cdnPlugins = readPluginEntries(cdnManifest);
  const cdnPluginNames = new Set(cdnPlugins.map(readPluginName).filter(isDefined));
  const bundledPlugins = readPluginEntries(bundledManifest).filter((plugin) => {
    const name = readPluginName(plugin);
    return name !== undefined && !cdnPluginNames.has(name);
  });

// The built-in plug-in and the CDN plug-in have used two marketplace ids, and the UI will treat the built-in market as
// Independent market without source and not found when refreshing. The two shards must be persisted independently and then merged.
// Otherwise, the seed when the application starts will overwrite the CDN directory, or the CDN refresh will overwrite the built-in directory. When the same name is used
// The refreshable CDN market entries shall prevail, but only the merged directory will be filtered and the built-in cache of the application will not be deleted.
  const merged = {
    ...(bundledManifest ?? {}),
    ...(cdnManifest ?? {}),
    name: ZCODE_OFFICIAL_PLUGIN_MARKETPLACE,
    plugins: [...cdnPlugins, ...bundledPlugins],
  };
  writeJsonFileSync(partitionPath(storageRoot, MERGED_MARKETPLACE_FILE), merged);
  return merged;
}

function readBundledPartition(storageRoot: string): BundledMarketplacePartition | undefined {
  const value = readJsonRecord(partitionPath(storageRoot, BUNDLED_PARTITION_FILE));
  if (!value || value.version !== 1 || !isRecord(value.manifest)) return undefined;
  return {
    manifest: value.manifest,
    version: 1,
  };
}

function readPluginEntries(
  manifest: Record<string, unknown> | undefined,
): Record<string, unknown>[] {
  return Array.isArray(manifest?.plugins) ? manifest.plugins.filter(isRecord) : [];
}

function readPluginName(plugin: Record<string, unknown>): string | undefined {
  return typeof plugin.name === "string" && plugin.name.length > 0 ? plugin.name : undefined;
}

function isDefined<T>(value: T | undefined): value is T {
  return value !== undefined;
}

function isStrictDescendant(parentPath: string, childPath: string): boolean {
  const relativePath = relative(parentPath, childPath);
  return (
    relativePath.length > 0 &&
    !isAbsolute(relativePath) &&
    relativePath !== ".." &&
    !relativePath.startsWith(`..${sep}`)
  );
}

function assertOfficialManifest(manifest: Record<string, unknown>): void {
  if (manifest.name !== ZCODE_OFFICIAL_PLUGIN_MARKETPLACE) {
    throw new Error(
      `Official marketplace manifest must be named ${ZCODE_OFFICIAL_PLUGIN_MARKETPLACE}`,
    );
  }
}

function partitionPath(storageRoot: string, fileName: string): string {
  return join(storageRoot, "marketplaces", ZCODE_OFFICIAL_PLUGIN_MARKETPLACE, fileName);
}

function readJsonRecord(path: string): Record<string, unknown> | undefined {
  try {
    const value = JSON.parse(readFileSync(path, "utf8")) as unknown;
    return isRecord(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

function writeJsonFileSync(path: string, value: unknown): void {
  const contents = `${JSON.stringify(value, null, 2)}\n`;
  mkdirSync(dirname(path), { recursive: true });
  try {
// The official directory will be rebuilt every time it is started; repeatedly writing the same content to the disk will increase the number of files on Windows.
// Probability that marketplace files are occupied by antivirus/indexers. Only skip writing to a single file with exactly the same bytes,
// If the read fails or the content changes, writing will still be performed and the original failure semantics will be retained.
    if (readFileSync(path, "utf8") === contents) return;
  } catch {
// Continue writing when the file does not exist or is temporarily unreadable, allowing the actual update failure to continue to be exposed to the caller.
  }
  writeFileSync(path, contents, "utf8");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
