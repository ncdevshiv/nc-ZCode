#!/usr/bin/env bun
// Bun single-file build for the ZCode CLI.
//
// This is the Bun replacement for the Node SEA pipeline (`build-sea.mjs`).
// `bun build --compile` embeds the JS bundle into a copy of the Bun runtime, so
// the result runs without a Node installation. `__CLI_VERSION__` is normally
// injected by esbuild in `build.mjs`; the Bun compiler needs the same define or
// the CLI falls back to "0.0.0".

import { spawnSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { stageBuiltinProviderConfig } from "../../../../../scripts/builtin-provider-config.mjs";

const cliRoot = resolve(import.meta.dirname, "..");
const projectRoot = resolve(cliRoot, "../..");
const entryPoint = resolve(cliRoot, "src/main.ts");

// Packages that cannot be embedded must stay outside the compiled bundle:
// the TUI's opentui core resolves yoga-layout with createRequire + readFileSync
// (Bun's embedded filesystem cannot serve it), koffi loads platform `.node`
// binaries, and chromium-bidi is Playwright's optional BiDi backend.
// `playwright-core` itself is bundled so the CDP browser path works standalone.
const externalPackages = ["@zcode/tui", "koffi", "chromium-bidi"];

function parseArgs(argv) {
  const options = {
    minify: false,
    outfile: resolve(cliRoot, "dist", process.platform === "win32" ? "zcode.exe" : "zcode"),
    skipVerify: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--outfile") {
      const value = argv[index + 1];
      if (!value) throw new Error("--outfile requires a path");
      options.outfile = resolve(value);
      index += 1;
    } else if (arg === "--minify") {
      options.minify = true;
    } else if (arg === "--skip-verify") {
      options.skipVerify = true;
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }
  return options;
}

const options = parseArgs(process.argv.slice(2));
const cliPackage = JSON.parse(await readFile(resolve(projectRoot, "package.json"), "utf8"));
const version = cliPackage.version;
if (typeof version !== "string" || version.trim() === "") {
  throw new Error("apps/zcode-cli/package.json must define a non-empty string version.");
}

const buildArgs = [
  "build",
  entryPoint,
  "--compile",
  "--outfile",
  options.outfile,
  "--define",
  `__CLI_VERSION__=${JSON.stringify(version)}`,
  "--asset",
  resolve(projectRoot, "../../config/provider/zcode-builtin.json"),
  ...externalPackages.flatMap((name) => ["--external", name]),
];
if (options.minify) buildArgs.push("--minify");

console.log(`[build-bun] zcode ${version} -> ${options.outfile}`);
// The compiled binary resolves its built-in provider config beside the
// executable, so stage it into `dist/provider` before compiling.
await stageBuiltinProviderConfig({
  root: resolve(projectRoot, "../.."),
  directory: resolve(cliRoot, "dist/provider"),
  env: process.env,
});

const build = spawnSync(process.execPath, buildArgs, {
  cwd: projectRoot,
  stdio: "inherit",
});
if (build.error) throw build.error;
if (build.status !== 0) {
  process.exit(build.status ?? 1);
}

if (!options.skipVerify) {
  const probe = spawnSync(options.outfile, ["--version"], { encoding: "utf8" });
  if (probe.error) throw probe.error;
  const printed = (probe.stdout ?? "").trim();
  if (probe.status !== 0 || printed !== version) {
    throw new Error(
      `Compiled binary failed the version probe: exit=${probe.status} stdout=${JSON.stringify(printed)} expected=${JSON.stringify(version)}`,
    );
  }
  console.log(`[build-bun] verified: ${options.outfile} --version -> ${printed}`);
}
