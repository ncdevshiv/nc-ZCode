import { createHash, randomBytes } from "node:crypto";
import { chmod, mkdir, readFile, rename, rm, stat, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import { getRuntimeToolRuntime, type RuntimeToolId } from "@zcode/shared/runtime-tool-runtime";

type CliEnv = Record<string, string | undefined>;

interface SeaRuntimeModule {
  getAsset(key: string, encoding: "utf8"): string;
  getRawAsset(key: string): ArrayBuffer;
  isSea(): boolean;
}

interface SeaRuntimeTool {
  binaryName: string;
  id: RuntimeToolId;
  sha256: string;
  size: number;
  version: string;
}

interface SeaRuntimeToolManifest {
  target: string;
  tools: SeaRuntimeTool[];
  version: 1;
}

interface RuntimeToolMarker {
  binaryName: string;
  id: RuntimeToolId;
  sha256: string;
  size: number;
  target: string;
  toolVersion: string;
  version: 1;
}

interface EnsureSeaRuntimeToolsOptions {
  arch?: string;
  env?: CliEnv;
  platform?: NodeJS.Platform;
  sea?: SeaRuntimeModule;
  storageRoot?: string;
}

const assetPrefix = "zcode-runtime-tools/";
const manifestAssetKey = `${assetPrefix}manifest.json`;
const markerFileName = ".zcode-runtime-tool.json";
const runtimeToolIds = new Set<RuntimeToolId>(["bfs", "ripgrep", "ugrep"]);

export async function ensureSeaRuntimeTools(
  options: EnsureSeaRuntimeToolsOptions = {},
): Promise<CliEnv> {
  const platform = options.platform ?? process.platform;
  const target = runtimeTarget(platform, options.arch ?? process.arch);
  const sea = options.sea ?? getSeaModule();
  const source = sea?.isSea()
    ? seaRuntimeToolSource(sea)
    : await bunRuntimeToolSource();
  if (!source) return {};

  if (source.target !== target) {
    throw new Error(
      `Runtime tool target mismatch: binary contains ${source.target}, runtime is ${target}.`,
    );
  }

  const env = options.env ?? process.env;
  const configuredStorageRoot = options.storageRoot ?? env.ZCODE_STORAGE_DIR?.trim();
  const storageRoot = configuredStorageRoot || join(homedir(), ".zcode");
  const runtimeEnv: CliEnv = {};

  for (const tool of source.tools) {
    const runtime = getRuntimeToolRuntime(tool.id);
    if (env[runtime.binaryEnvVar]?.trim()) continue;

    runtimeEnv[runtime.binaryEnvVar] = await ensureRuntimeTool({
      source,
      platform,
      storageRoot,
      tool,
    });
  }

  return runtimeEnv;
}

async function ensureRuntimeTool(input: {
  source: RuntimeToolAssetSource;
  platform: NodeJS.Platform;
  storageRoot: string;
  tool: SeaRuntimeTool;
}): Promise<string> {
  const { source, platform, storageRoot, tool } = input;
  const cacheDirectory = join(
    storageRoot,
    "cache",
    "runtime_tools",
    source.target,
    tool.id,
    `${tool.version}-${tool.sha256}`,
  );
  const binaryPath = join(cacheDirectory, tool.binaryName);
  const marker = markerFor(
    { target: source.target, tools: source.tools, version: 1 },
    tool,
  );

  if (await isCacheCurrent(cacheDirectory, marker, platform)) return binaryPath;

  const temporaryDirectory = `${cacheDirectory}.tmp-${process.pid}-${Date.now()}-${randomBytes(4).toString("hex")}`;
  await rm(temporaryDirectory, {
    force: true,
    recursive: true,
  });
  await mkdir(temporaryDirectory, {
    recursive: true,
  });

  try {
    const bytes = await source.readTool(tool);
    if (bytes.byteLength !== tool.size) {
      throw new Error(`Runtime tool size mismatch for ${tool.id}.`);
    }
    const actualHash = createHash("sha256").update(bytes).digest("hex");
    if (actualHash !== tool.sha256) {
      throw new Error(`Runtime tool hash mismatch for ${tool.id}.`);
    }

    const temporaryBinaryPath = join(temporaryDirectory, tool.binaryName);
    await writeFile(temporaryBinaryPath, bytes);
    await chmod(temporaryBinaryPath, 0o755);
    await writeFile(join(temporaryDirectory, markerFileName), JSON.stringify(marker, null, 2));

    await mkdir(dirname(cacheDirectory), {
      recursive: true,
    });
    if (await isCacheCurrent(cacheDirectory, marker, platform)) return binaryPath;
    await installCacheDirectory(temporaryDirectory, cacheDirectory, marker, platform);
    return binaryPath;
  } finally {
    await rm(temporaryDirectory, {
      force: true,
      recursive: true,
    });
  }
}

async function installCacheDirectory(
  temporaryDirectory: string,
  cacheDirectory: string,
  marker: RuntimeToolMarker,
  platform: NodeJS.Platform,
): Promise<void> {
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      await rename(temporaryDirectory, cacheDirectory);
      return;
    } catch (error) {
      if (await isCacheCurrent(cacheDirectory, marker, platform)) return;
      if (!isDirectoryReplaceRace(error) || attempt === 1) throw error;
      await rm(cacheDirectory, {
        force: true,
        recursive: true,
      });
    }
  }
}

function readManifest(sea: SeaRuntimeModule): SeaRuntimeToolManifest {
  let manifest: SeaRuntimeToolManifest;
  try {
    manifest = JSON.parse(sea.getAsset(manifestAssetKey, "utf8")) as SeaRuntimeToolManifest;
  } catch (error) {
    throw new Error("Invalid SEA runtime tool manifest.", {
      cause: error,
    });
  }

  validateManifest(manifest);
  return manifest;
}

interface RuntimeToolAssetSource {
  target: string;
  tools: SeaRuntimeTool[];
  readTool(tool: SeaRuntimeTool): Promise<Buffer>;
}

function seaRuntimeToolSource(sea: SeaRuntimeModule): RuntimeToolAssetSource {
  const manifest = readManifest(sea);
  return {
    target: manifest.target,
    tools: manifest.tools,
    readTool: async (tool) => Buffer.from(sea.getRawAsset(assetKeyFor(tool))),
  };
}

interface BunEmbeddedFile {
  readonly name: string;
  text(): Promise<string>;
  arrayBuffer(): Promise<ArrayBuffer>;
}

const bunRuntimeToolManifestName = "zcode-runtime-tools-manifest.json";
const bunRuntimeToolAssetPrefix = "zcode-runtime-tool-";

function bunRuntimeToolAssetName(tool: SeaRuntimeTool): string {
  const extension = tool.binaryName.endsWith(".exe") ? ".exe" : "";
  return `${bunRuntimeToolAssetPrefix}${tool.id}${extension}`;
}

/**
 * Bun single-file builds embed the per-target manifest and tool binaries with
 * `--asset`; this source materializes the same cache entries as the SEA path.
 */
async function bunRuntimeToolSource(): Promise<RuntimeToolAssetSource | undefined> {
  const bun = (globalThis as { Bun?: { embeddedFiles?: readonly BunEmbeddedFile[] } }).Bun;
  const files = bun?.embeddedFiles;
  if (!files) return undefined;
  const manifestFile = files.find((file) => file.name === bunRuntimeToolManifestName);
  if (!manifestFile) return undefined;

  let manifest: SeaRuntimeToolManifest;
  try {
    manifest = JSON.parse(await manifestFile.text()) as SeaRuntimeToolManifest;
  } catch (error) {
    throw new Error("Invalid Bun runtime tool manifest.", {
      cause: error,
    });
  }
  validateManifest(manifest);

  return {
    target: manifest.target,
    tools: manifest.tools,
    readTool: async (tool) => {
      const file = files.find((entry) => entry.name === bunRuntimeToolAssetName(tool));
      if (!file) {
        throw new Error(`Missing embedded runtime tool ${tool.id}.`);
      }
      return Buffer.from(await file.arrayBuffer());
    },
  };
}

function validateManifest(manifest: SeaRuntimeToolManifest): void {
  if (
    manifest?.version !== 1 ||
    typeof manifest.target !== "string" ||
    !/^[a-z0-9]+-(?:arm64|x64)$/u.test(manifest.target) ||
    !Array.isArray(manifest.tools) ||
    manifest.tools.length === 0
  ) {
    throw new Error("Invalid SEA runtime tool manifest.");
  }

  const seenToolIds = new Set<RuntimeToolId>();
  for (const tool of manifest.tools) {
    if (
      !runtimeToolIds.has(tool?.id) ||
      seenToolIds.has(tool.id) ||
      typeof tool.binaryName !== "string" ||
      basename(tool.binaryName) !== tool.binaryName ||
      !/^[A-Za-z0-9._-]+$/u.test(tool.binaryName) ||
      !/^[a-f0-9]{64}$/u.test(tool.sha256) ||
      !Number.isSafeInteger(tool.size) ||
      tool.size < 0 ||
      typeof tool.version !== "string" ||
      !/^[A-Za-z0-9][A-Za-z0-9._-]*$/u.test(tool.version)
    ) {
      throw new Error("Invalid SEA runtime tool manifest.");
    }
    seenToolIds.add(tool.id);
  }
}

async function isCacheCurrent(
  cacheDirectory: string,
  expected: RuntimeToolMarker,
  platform: NodeJS.Platform,
): Promise<boolean> {
  try {
    const marker = JSON.parse(
      await readFile(join(cacheDirectory, markerFileName), "utf8"),
    ) as RuntimeToolMarker;
    if (
      marker.version !== expected.version ||
      marker.id !== expected.id ||
      marker.toolVersion !== expected.toolVersion ||
      marker.sha256 !== expected.sha256 ||
      marker.target !== expected.target ||
      marker.binaryName !== expected.binaryName ||
      marker.size !== expected.size
    ) {
      return false;
    }

    const binaryPath = join(cacheDirectory, expected.binaryName);
    const binaryStat = await stat(binaryPath);
    if (!binaryStat.isFile() || binaryStat.size !== expected.size) return false;
    if (platform !== "win32" && (binaryStat.mode & 0o111) === 0) return false;

    const actualSha256 = createHash("sha256")
      .update(await readFile(binaryPath))
      .digest("hex");
    return actualSha256 === expected.sha256;
  } catch {
    return false;
  }
}

function markerFor(manifest: SeaRuntimeToolManifest, tool: SeaRuntimeTool): RuntimeToolMarker {
  return {
    binaryName: tool.binaryName,
    id: tool.id,
    sha256: tool.sha256,
    size: tool.size,
    target: manifest.target,
    toolVersion: tool.version,
    version: 1,
  };
}

function runtimeTarget(platform: NodeJS.Platform, arch: string): string {
  const releasePlatform = platform === "win32" ? "win" : platform;
  if (!["darwin", "linux", "win"].includes(releasePlatform) || !["arm64", "x64"].includes(arch)) {
    throw new Error(`Unsupported SEA runtime target ${platform}-${arch}.`);
  }
  return `${releasePlatform}-${arch}`;
}

function assetKeyFor(tool: SeaRuntimeTool): string {
  return `${assetPrefix}${tool.id}/${tool.sha256}/${tool.binaryName}`;
}

function getSeaModule(): SeaRuntimeModule | undefined {
  const getBuiltinModule = process.getBuiltinModule as
    | ((id: string) => SeaRuntimeModule | undefined)
    | undefined;
  try {
    return getBuiltinModule?.("node:sea");
  } catch {
    return undefined;
  }
}

function isDirectoryReplaceRace(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    ["EEXIST", "ENOTEMPTY"].includes(String((error as NodeJS.ErrnoException).code))
  );
}
