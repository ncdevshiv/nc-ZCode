import { chmod, readFile, rm } from "node:fs/promises";
import { readThirdPartyNotices, stageThirdPartyNotices } from "../../../../../scripts/third-party-notices.mjs";
import { basename, dirname, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { build } from "esbuild";
import { stageBuiltinProviderConfig } from "../../../../../scripts/builtin-provider-config.mjs";

const cliRoot = resolve(import.meta.dirname, "..");
const projectRoot = resolve(cliRoot, "../..");
const executableFileMode = 0o755;
const packageJsonFile = "package.json";
const rootPackageVersionError = "Root package.json must define a non-empty string version.";
const desktopAgentBuildFlag = "--desktop-agent";
export const resolveBuildExternal = () => ["@zcode/tui", "playwright-core", "koffi"];

export const readZodBuildVersion = async () => {
  const sharedPackage = JSON.parse(
    await readFile(resolve(projectRoot, "../../packages/shared/package.json"), "utf8"),
  );
  const version = sharedPackage.dependencies?.zod;
  if (typeof version !== "string" || !/^4\.\d+\.\d+$/.test(version)) {
    throw new Error("packages/shared must pin one exact Zod v4 version.");
  }
  return version;
};

async function readZodPackage(file, cache) {
  for (
    let directory = dirname(file);
    directory !== dirname(directory);
    directory = dirname(directory)
  ) {
    if (basename(directory) !== "zod") continue;
    if (!cache.has(directory)) {
      cache.set(
        directory,
        readFile(resolve(directory, "package.json"), "utf8").then((text) => {
          const manifest = JSON.parse(text);
          if (manifest.name !== "zod" || typeof manifest.version !== "string") {
            throw new Error(`Invalid Zod package metadata: ${directory}`);
          }
          return { root: directory, version: manifest.version };
        }),
      );
    }
    return cache.get(directory);
  }
  return undefined;
}

function assertZodVersion(pkg, expectedV4Version) {
  if (pkg.version.startsWith("4.") && pkg.version !== expectedV4Version) {
    throw new Error(`Expected Zod v4 ${expectedV4Version}, found ${pkg.version}: ${pkg.root}`);
  }
}

export async function assertZodBundleIdentity(metafile, { workingDirectory, expectedV4Version }) {
  if (!metafile) throw new Error("Zod bundle validation requires an esbuild metafile.");
  const packages = new Map();
  const cache = new Map();
  for (const input of Object.keys(metafile.inputs)) {
    if (!/(?:^|[/\\])zod[/\\]/.test(input)) continue;
    const pkg = await readZodPackage(resolve(workingDirectory, input), cache);
    if (!pkg) continue;
    assertZodVersion(pkg, expectedV4Version);
    const previous = packages.get(pkg.version);
    if (previous && previous !== pkg.root) {
      throw new Error(`Duplicate Zod ${pkg.version} in bundle: ${previous}, ${pkg.root}`);
    }
    packages.set(pkg.version, pkg.root);
  }
}

export function createZodDedupePlugin({ expectedV4Version }) {
  return {
    name: "zcode-zod-dedupe",
    setup(builder) {
      const packages = new Map();
      const cache = new Map();
// A hoisted installation may leave multiple consumers with Zods with the same bytes and different paths.
//You must first parse the version and exports according to the original consumer, and then merge and install the root. You cannot alias v3 to v4.
      builder.onResolve({ filter: /^zod(?:\/|$)/ }, async (args) => {
        if (args.pluginData?.zcodeZodResolving) return;
        const resolved = await builder.resolve(args.path, {
          importer: args.importer,
          resolveDir: args.resolveDir,
          kind: args.kind,
          pluginData: { zcodeZodResolving: true },
        });
        if (resolved.errors.length || resolved.external) return resolved;
        const pkg = await readZodPackage(resolved.path, cache);
        if (!pkg) throw new Error(`Cannot identify resolved Zod package: ${resolved.path}`);
        assertZodVersion(pkg, expectedV4Version);
        if (!packages.has(pkg.version)) packages.set(pkg.version, pkg.root);
        return {
          ...resolved,
//Retain the selected sub-entry and .js/.cjs of exports, and you can no longer use require.resolve to rewrite the conditions.
          path: resolve(packages.get(pkg.version), relative(pkg.root, resolved.path)),
          pluginData: args.pluginData,
        };
      });
      builder.onEnd(async (result) => {
        if (result.errors.length) return;
        await assertZodBundleIdentity(result.metafile, {
          workingDirectory: builder.initialOptions.absWorkingDir ?? process.cwd(),
          expectedV4Version,
        });
      });
    },
  };
}

export const readRootPackageVersion = async ({ root = projectRoot } = {}) => {
  const packageJson = JSON.parse(await readFile(resolve(root, packageJsonFile), "utf8"));

  if (typeof packageJson.version !== "string" || packageJson.version.trim() === "") {
    throw new Error(rootPackageVersionError);
  }

  return packageJson.version;
};

export const resolveBuildOptions = (args = [], env = process.env) => {
  const desktopAgent = args.includes(desktopAgentBuildFlag);
  const e2eCoverage = env.ZCODE_E2E_COVERAGE === "1";

  return {
// Desktop-agent normal publishing still needs to be compressed and does not carry map; E2E coverage
// Dedicated builds must retain the original symbols and source map so that c8 can reflect the TS source code of each package.
    minify: desktopAgent && !e2eCoverage,
    sourcemap: e2eCoverage || !desktopAgent,
  };
};

export const resolveBuildAliases = ({
  cliDirectory = cliRoot,
  rootDirectory = projectRoot,
} = {}) => ({
  "@zcode/shared-types": resolve(cliDirectory, "../shared-types/dist/index.js"),
// Plugin-host only needs these independent entries for startup, and the shared general entry cannot be re-evaluated through general alias.
  "@zcode/shared/runtime-env": resolve(rootDirectory, "../../packages/shared/src/runtimeEnv.ts"),
  "@zcode/shared/mcp": resolve(rootDirectory, "../../packages/shared/src/mcp.ts"),
  "@zcode/shared/runtime-tool-runtime": resolve(
    rootDirectory,
    "../../packages/shared/src/runtime-tool-runtime.ts",
  ),
// esbuild alias rewrites the import path by prefix. All shared subpaths must be declared exactly before the universal entry,
// Otherwise, it will be incorrectly parsed as `src/index.ts/<subpath>` and the Desktop agent/SEA packaging will fail.
  "@zcode/shared/zcode-protocol-v4": resolve(
    rootDirectory,
    "../../packages/shared/src/zcode-protocol-v4/index.ts",
  ),
//ModelSelection schema was changed to shared single source of fact and this sub-path reference was added.
// esbuild alias is rewritten according to the prefix; if it is not declared accurately before the general entry, it will be spelled incorrectly
// `src/index.ts/model-selection`, causing Desktop agent packaging to fail.
  "@zcode/shared/model-selection": resolve(
    rootDirectory,
    "../../packages/shared/src/model-selection.ts",
  ),
// The new subpath of the shared Model Schema cannot be spelled behind index.ts by general alias.
  "@zcode/shared/model-config": resolve(rootDirectory, "../../packages/shared/src/model-config.ts"),
// The process exception boundary uses this lightweight contract before bootstrap and cannot fall into the common prefix alias of shared.
  "@zcode/shared/process-diagnostic": resolve(
    rootDirectory,
    "../../packages/shared/src/process-diagnostic.ts",
  ),
  "@zcode/shared/config-schema": resolve(
    rootDirectory,
    "../../packages/shared/src/config-schema.ts",
  ),
  "@zcode/shared/workspace-hook-discovery": resolve(
    rootDirectory,
    "../../packages/shared/src/workspace-hook-discovery.ts",
  ),
// review controller is directly connected to WorkspaceHookMutationError and requires this accuracy
// alias (esbuild prefix rewriting rules are the same as above, missing declarations will fail in Desktop agent/SEA packaging).
  "@zcode/shared/workspace-hook-mutation": resolve(
    rootDirectory,
    "../../packages/shared/src/workspace-hook-mutation.ts",
  ),
// verdict direct import requires this exact alias; missing declarations will be universal
// "@zcode/shared" prefix is ​​rewritten to `src/index.ts/workspace-hook-review-monotonicity`,
// Desktop agent/SEA packaging fails directly.
  "@zcode/shared/workspace-hook-review-monotonicity": resolve(
    rootDirectory,
    "../../packages/shared/src/workspace-hook-review-monotonicity.ts",
  ),
// The new subpath of trust store file schema after single source sinking; missing declarations will be universal
// "@zcode/shared" prefix is ​​rewritten to `src/index.ts/workspace-hook-trust-store-file`,
// Desktop agent/SEA packaging failed (the same as the existing rules of the above two categories).
  "@zcode/shared/workspace-hook-trust-store-file": resolve(
    rootDirectory,
    "../../packages/shared/src/workspace-hook-trust-store-file.ts",
  ),
  "@zcode/shared/zcodeEndpoint": resolve(
    rootDirectory,
    "../../packages/shared/src/zcodeEndpoint.ts",
  ),
  "@zcode/shared/node": resolve(rootDirectory, "../../packages/shared/src/node.ts"),
  "@zcode/shared": resolve(rootDirectory, "../../packages/shared/src/index.ts"),
  "@zcode/core": resolve(cliDirectory, "../core/dist/index.js"),
});

export const buildCli = async ({
  cliDirectory = cliRoot,
  rootDirectory = projectRoot,
  minify = false,
  sourcemap = true,
  env = process.env,
  version = readRootPackageVersion({
    root: rootDirectory,
  }),
} = {}) => {
  const cliVersion = await version;
  const outfile = resolve(cliDirectory, "dist/zcode.cjs");
  const sourcemapFile = `${outfile}.map`;
  const notices = await readThirdPartyNotices(resolve(rootDirectory, "../.."));

  await stageBuiltinProviderConfig({
    root: resolve(rootDirectory, "../.."),
    directory: resolve(cliDirectory, "dist/provider"),
    env,
  });

  await build({
    banner: {
// SEA shares the same entry point with the normal CLI; the declaration must be independently readable before Agent initialization and native resource decompression.
      js: `#!/usr/bin/env node\n"use strict";\nif (process.argv.length === 3 && process.argv[2] === "--licenses") { const sea = require("node:sea"); const nodeNotice = sea.isSea() ? "\\n\\n## Bundled Node.js runtime\\n\\n" + sea.getAsset("zcode-node-license", "utf8") : ""; process.stdout.write(${JSON.stringify(notices.toString("utf8"))} + nodeNotice, () => process.exit(0)); } else {`,
    },
    footer: { js: "}" },
    bundle: true,
    define: {
      __CLI_VERSION__: JSON.stringify(cliVersion),
    },
    entryPoints: [resolve(cliDirectory, "src/main.ts")],
    // Ink 7 and yoga-layout use top-level await, so the CJS CLI bundle loads the TUI
    // through Node's native dynamic import path instead of forcing esbuild to lower it.
// playwright-core relies on runtime package assets and require.resolve, which must remain external compared to inline bundles.
// managed headless adapter is only lazy loaded when --browser-use=headless is explicit, does not affect app-server/normal CLI.
// koffi will dynamically require native `.node` files according to the current platform; inlining will cause esbuild to traverse all
// Platform products and directly report "No loader is configured for .node". Still loaded from dependency packages at runtime,
// SEA resources are processed separately by the native asset collection phase of build-sea.
    external: resolveBuildExternal(),
    format: "cjs",
// Desktop app integration only has built-in zcode.cjs, and the old desktop-agent builds reused CLI debugging products.
// Uncompressed and will leave a pointer to a sourcemap that is not copied with the package. Desktop agent mode compresses JS while retaining
// Function/class name to avoid diagnosis and registration logic that relies on name being affected by esbuild identifier compression.
    keepNames: minify,
    legalComments: "none",
    logLevel: "info",
    minify,
    metafile: true,
    plugins: [createZodDedupePlugin({ expectedV4Version: await readZodBuildVersion() })],
    outfile,
    platform: "node",
    sourcemap,
// target takes the lowest Node version among all hosted runtimes: Electron for desktop has built-in Node 24,
// Remote SSH reuses the deployed independent Node v22.16 to run the same zcode.cjs. Downgrade to node22 to ensure this product
// Syntax/features not supported by the target runtime will not be used on either end.
    target: "node22",
    alias: resolveBuildAliases({ cliDirectory, rootDirectory }),
  });

  if (!sourcemap) {
    await rm(sourcemapFile, { force: true });
  }

  await chmod(outfile, executableFileMode);
  await stageThirdPartyNotices(resolve(cliDirectory, "dist"), resolve(rootDirectory, "../.."));
};

const entryPath = process.argv[1];
if (entryPath && import.meta.url === pathToFileURL(entryPath).href) {
  await buildCli(resolveBuildOptions(process.argv.slice(2)));
}
