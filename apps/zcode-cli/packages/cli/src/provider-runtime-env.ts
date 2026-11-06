import { existsSync, realpathSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import {
  materializeZCodeBuiltinProviderConfig,
  NodeZCodeBuiltinProviderConfigSource,
  PERSONAL_PROVIDER_CONFIG_FILE_NAME,
  resolveZCodeBuiltinCachePaths,
  resolveZCodeBuiltinClientPlatform,
  ZCODE_BUILTIN_PROVIDER_BUNDLED_CONFIG_FILE_ENV,
  ZCODE_BUILTIN_PROVIDER_CONFIG_FILE_ENV,
  ZCODE_PERSONAL_PROVIDER_CONFIG_FILE_ENV,
  type ZCodeBuiltinRefreshEvent,
} from "@zcode/provider-node";
import { resolveRuntimeZCodeEndpointOrigin, ZCODE_VERSION } from "@zcode/shared";
import type { CliEnv } from "./env.js";

export const SEA_ZCODE_BUILTIN_PROVIDER_CONFIG_ASSET_KEY = "zcode-provider/zcode-builtin.json";

export function createCliProviderRefreshReporter(
  stderr: Pick<NodeJS.WriteStream, "write"> = process.stderr,
) {
  return {
    onBuiltinRefreshError(error: unknown) {
      stderr.write(
        `ZCode Built-in refresh failed: ${error instanceof Error? error.message: "unknown error"}\n`,
      );
    },
    onBuiltinRefreshResult(event: ZCodeBuiltinRefreshEvent) {
// TTL check is not a production event; only successful updates will leave traces by default, and CDN URL query parameters or content cannot be output.
      if (event.result === "updated" || process.env.NODE_ENV !== "production") {
        stderr.write(
          `ZCode Built-in ${event.result}${event.reason ? ` (${event.reason})` : ""}${event.revision === undefined ? "" : ` revision=${event.revision} source=CDN`}\n`,
        );
      }
    },
  };
}

type SeaProviderConfigAssets = Pick<typeof import("node:sea"), "getAsset" | "isSea">;

interface PrepareCliProviderRuntimeEnvOptions {
  readonly argv: readonly string[];
  readonly env: CliEnv;
  readonly dataBaseDir?: string;
  readonly entrypoint?: string;
  readonly sea?: SeaProviderConfigAssets;
  readonly appVersion?: string;
  readonly platform?: string;
}

/** Provider Config that locates the same Environment for the CLI Entry selected to run Core or write the model. */
export async function prepareCliProviderRuntimeEnv(
  options: PrepareCliProviderRuntimeEnvOptions,
): Promise<Record<string, string>> {
  if (!requiresProviderRuntime(options.argv)) return {};

  const explicitZCodeBuiltin = options.env[ZCODE_BUILTIN_PROVIDER_CONFIG_FILE_ENV]?.trim();
  const explicitPersonal = options.env[ZCODE_PERSONAL_PROVIDER_CONFIG_FILE_ENV]?.trim();
  const dataBaseDir = options.dataBaseDir ?? options.env.ZCODE_DATA_BASE_DIR?.trim() ?? homedir();
  if (explicitZCodeBuiltin && explicitPersonal) {
    return {
      [ZCODE_BUILTIN_PROVIDER_CONFIG_FILE_ENV]: explicitZCodeBuiltin,
      [ZCODE_PERSONAL_PROVIDER_CONFIG_FILE_ENV]: explicitPersonal,
    };
  }

  const zcodeBuiltinFilePath =
    explicitZCodeBuiltin ??
    (await resolveBundledZCodeBuiltinProviderConfig({
      dataBaseDir,
      entrypoint: options.entrypoint ?? process.argv[1],
      sea: options.sea ?? getSeaProviderConfigAssets(),
    }));
  const personalFilePath =
    explicitPersonal ?? join(dataBaseDir, ".zcode", "v2", PERSONAL_PROVIDER_CONFIG_FILE_NAME);
  const appVersion = options.appVersion ?? ZCODE_VERSION;
  const platform = options.platform ?? resolveZCodeBuiltinClientPlatform();
  const zcodeEndpointOrigin = resolveRuntimeZCodeEndpointOrigin(options.env);
  const cachePaths = resolveZCodeBuiltinCachePaths({
    environmentConfigRoot: join(dataBaseDir, ".zcode", "v2"),
    platform,
    appVersion,
    zcodeEndpointOrigin,
  });
  const source = new NodeZCodeBuiltinProviderConfigSource({
    bundledFilePath: zcodeBuiltinFilePath,
    activeFilePath: cachePaths.activeFilePath,
    watch: false,
  });
// The entry only prepares resources and paths; the download is held and canceled by the Prompt/TUI long life cycle Runtime.
  try {
    await source.read();
  } finally {
    source.dispose();
  }

  return {
    [ZCODE_BUILTIN_PROVIDER_CONFIG_FILE_ENV]: cachePaths.activeFilePath,
    [ZCODE_BUILTIN_PROVIDER_BUNDLED_CONFIG_FILE_ENV]: zcodeBuiltinFilePath,
    [ZCODE_PERSONAL_PROVIDER_CONFIG_FILE_ENV]: personalFilePath,
  };
}

function requiresProviderRuntime(argv: readonly string[]): boolean {
  if (argv.some((arg) => arg === "--help" || arg === "-h" || arg === "--version" || arg === "-v")) {
    return false;
  }
  if (
    argv.some(
      (arg) =>
        arg === "--prompt" ||
        arg.startsWith("--prompt=") ||
        arg === "--target" ||
        arg.startsWith("--target="),
    )
  ) {
    return true;
  }

  const command = argv[0];
  if (command === undefined || command.startsWith("-")) return true;
  return (
    command === "tui" ||
    command === "app-server" ||
    command === "agent-server" ||
    command === "login" ||
    command === "logout"
  );
}

async function resolveBundledZCodeBuiltinProviderConfig(input: {
  readonly dataBaseDir: string;
  readonly entrypoint: string | undefined;
  readonly sea: SeaProviderConfigAssets | undefined;
}): Promise<string> {
  if (input.sea?.isSea()) {
    const content = input.sea.getAsset(SEA_ZCODE_BUILTIN_PROVIDER_CONFIG_ASSET_KEY, "utf8");
    return materializeZCodeBuiltinProviderConfig({
      environmentConfigRoot: join(input.dataBaseDir, ".zcode", "v2"),
      content,
    });
  }

  // Bun single-file builds embed `zcode-builtin.json` with `--asset`; read it
  // from the embedded filesystem and materialize it under the data dir so the
  // release artifact does not need a sibling `provider/` directory.
  const bunEmbedded = await readBunEmbeddedProviderConfig();
  if (bunEmbedded) {
    return materializeZCodeBuiltinProviderConfig({
      environmentConfigRoot: join(input.dataBaseDir, ".zcode", "v2"),
      content: bunEmbedded,
    });
  }

  const entrypoint = input.entrypoint?.trim();
  if (!entrypoint) throw new Error("Unable to locate CLI ZCode Built-in Provider Config: Missing entry path");
// The global bin can be a soft link, and the package configuration must be positioned relative to the real entry.
// Bun compiled binaries expose a virtual argv[1] (~BUN/$bunfs) that realpathSync cannot resolve.
  const bundled = /~BUN|\$bunfs/u.test(entrypoint);
  const entryDirectory = dirname(realpathSync(bundled ? process.execPath : resolve(entrypoint)));
  const candidates = [
    join(entryDirectory, "provider", "zcode-builtin.json"),
    resolve(entryDirectory, "../../../../../config/provider/zcode-builtin.json"),
  ];
  const candidate = candidates.find((filePath) => existsSync(filePath));
  if (candidate) return candidate;
  throw new Error(`Unable to locate CLI ZCode Built-in Provider Config: ${candidates.join(", ")}`);
}

interface BunEmbeddedFile {
  readonly name: string;
  text(): Promise<string>;
}

async function readBunEmbeddedProviderConfig(): Promise<string | undefined> {
  const bun = (globalThis as { Bun?: { embeddedFiles?: readonly BunEmbeddedFile[] } }).Bun;
  const files = bun?.embeddedFiles;
  if (!files) return undefined;
  const config = files.find((file) => file.name === "zcode-builtin.json");
  return config ? await config.text() : undefined;
}

function getSeaProviderConfigAssets(): SeaProviderConfigAssets | undefined {
  const getBuiltinModule = process.getBuiltinModule as
    | ((id: "node:sea") => typeof import("node:sea"))
    | undefined;
  return getBuiltinModule?.("node:sea");
}
