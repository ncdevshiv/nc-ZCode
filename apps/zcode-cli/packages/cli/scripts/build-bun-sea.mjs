#!/usr/bin/env bun
// Bun-native replacement for the Node SEA + postject pipeline (`build-sea.mjs`).
//
// Produces the same release artifact names (`zcode-<platform>-<arch>[.exe]`)
// using `bun build --compile --target=bun-<platform>-<arch>`, so downstream
// upload/download scripts keep working.
//
// Documented limitations of the Bun single-file artifact:
// - the TUI is not bundled (opentui resolves yoga-layout with
//   `createRequire` + `readFileSync`, which Bun's embedded filesystem cannot
//   serve); `zcode tui` fails with an explicit message;
// - `koffi` (CUA helper) is external; CUA is currently fail-closed;
// - `chromium-bidi` is external (optional Playwright BiDi backend).
// `playwright-core` is bundled, so the CDP browser path works standalone.

import { spawnSync } from "node:child_process";
import { copyFile, readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { stageBuiltinProviderConfig } from "../../../../../scripts/builtin-provider-config.mjs";
import { collectSeaRuntimeToolAssets } from "./sea-runtime-tool-assets.mjs";
import {
  hostTarget,
  isHostTarget,
  outputBinaryName,
  parseBuildSeaArgs,
  supportedTargets,
  targetParts,
} from "./sea-targets.mjs";

const cliRoot = resolve(import.meta.dirname, "..");
const projectRoot = resolve(cliRoot, "../..");
const repositoryRoot = resolve(projectRoot, "../..");
const entryPoint = resolve(cliRoot, "src/main.ts");
const dist = resolve(cliRoot, "dist");

const usage = `Usage:
  bun scripts/build-bun-sea.mjs
  bun scripts/build-bun-sea.mjs --target linux-x64 --target win-x64
  bun scripts/build-bun-sea.mjs --targets linux-x64,win-x64
  bun scripts/build-bun-sea.mjs --all

Supported targets:
  ${supportedTargets.join(", ")}

Native search tools (rg/bfs/ugrep) are embedded per target. Prepare each
target first, e.g.:
  bun run --filter @zcode/desktop prepare:native-search --platform win32 --arch x64
`;

const parsed = parseBuildSeaArgs(process.argv.slice(2));
if (parsed.help) {
  console.log(usage);
  process.exit(0);
}

const cliPackage = JSON.parse(await readFile(resolve(projectRoot, "package.json"), "utf8"));
const version = cliPackage.version;
if (typeof version !== "string" || version.trim() === "") {
  throw new Error("apps/zcode-cli/package.json must define a non-empty string version.");
}

const bunTargetFor = (target) => {
  const { arch, releasePlatform } = targetParts(target);
  const platform = releasePlatform === "win" ? "windows" : releasePlatform;
  return `bun-${platform}-${arch}`;
};

// The compiled binary resolves externals from the process cwd/NODE_PATH; the
// Node SEA pipeline staged these as `node:sea` assets instead. Keep them
// external and document the limitation rather than embedding an incompatible
// runtime.
const externalPackages = ["@zcode/tui", "koffi", "chromium-bidi"];

// The built-in provider config sits beside the executable at runtime and is
// platform-independent, so stage it once for all targets.
await stageBuiltinProviderConfig({
  root: repositoryRoot,
  directory: resolve(dist, "provider"),
  env: process.env,
});

const built = [];
for (const target of parsed.targets) {
  const outfile = resolve(dist, outputBinaryName(target));
  const bunTarget = bunTargetFor(target);
  // Per-target native search tools (rg/bfs/ugrep) are embedded as flat assets;
  // the runtime materializes them into the cache with size/hash checks.
  const runtimeAssetsDirectory = resolve(dist, "bun-runtime-assets", target);
  const { assets, manifest } = await collectSeaRuntimeToolAssets({
    root: repositoryRoot,
    stagingDirectory: runtimeAssetsDirectory,
    target,
  });
  const runtimeAssetArgs = [];
  for (const tool of manifest.tools) {
    const source = assets[`zcode-runtime-tools/${tool.id}/${tool.sha256}/${tool.binaryName}`];
    if (!source) {
      throw new Error(`Missing collected runtime tool asset for ${target}: ${tool.id}`);
    }
    const embeddedName = `zcode-runtime-tool-${tool.id}${tool.binaryName.endsWith(".exe") ? ".exe" : ""}`;
    const embeddedPath = resolve(runtimeAssetsDirectory, embeddedName);
    await copyFile(source, embeddedPath);
    runtimeAssetArgs.push("--asset", embeddedPath);
  }
  const manifestPath = resolve(runtimeAssetsDirectory, "zcode-runtime-tools-manifest.json");
  await writeFile(manifestPath, JSON.stringify(manifest, null, 2));
  runtimeAssetArgs.push("--asset", manifestPath);

  const args = [
    "build",
    entryPoint,
    "--compile",
    `--target=${bunTarget}`,
    "--outfile",
    outfile,
    "--define",
    `__CLI_VERSION__=${JSON.stringify(version)}`,
    "--asset",
    resolve(repositoryRoot, "config/provider/zcode-builtin.json"),
    ...runtimeAssetArgs,
    ...externalPackages.flatMap((name) => ["--external", name]),
  ];
  console.log(`[build-bun-sea] ${target} (${bunTarget}) -> ${outfile}`);
  const result = spawnSync(process.execPath, args, {
    cwd: projectRoot,
    stdio: "inherit",
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`bun build --compile failed for ${target} (exit ${result.status})`);
  }
  built.push({ outfile, target });
}

if (parsed.targets.includes(hostTarget())) {
  const host = built.find((entry) => isHostTarget(entry.target));
  if (host) {
    const probe = spawnSync(host.outfile, ["--version"], { encoding: "utf8" });
    const printed = (probe.stdout ?? "").trim();
    if (probe.error) throw probe.error;
    if (probe.status !== 0 || printed !== version) {
      throw new Error(
        `Host binary failed the version probe: exit=${probe.status} stdout=${JSON.stringify(printed)} expected=${JSON.stringify(version)}`,
      );
    }
    console.log(`[build-bun-sea] verified: ${host.outfile} --version -> ${printed}`);
  }
}

console.log(`[build-bun-sea] built ${built.length} target(s):`);
for (const entry of built) console.log(`  ${entry.target}: ${entry.outfile}`);
