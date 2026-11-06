#!/usr/bin/env bun
// Bun-native replacement for `turbo run <task>` inside apps/zcode-cli.
//
// Turbo 2.9 cannot parse Bun 1.4's `bun.lock` (lockfile v2) and resolves the
// repository root as the workspace root, so it never finds
// `apps/zcode-cli/turbo.json`. This runner reads the same package set from the
// workspace manifests, orders packages by their workspace dependency edges
// (the `^task` semantics of turbo.json) and executes each package script with
// Bun.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { readFile, readdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";

const cliRoot = resolve(import.meta.dirname, "..");
const repositoryRoot = resolve(cliRoot, "../..");

function parseArgs(argv) {
  const options = { script: undefined, includeRoot: false, dryRun: false };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--include-root") options.includeRoot = true;
    else if (arg === "--dry-run") options.dryRun = true;
    else if (!options.script) options.script = arg;
    else throw new Error(`Unknown argument: ${arg}`);
  }
  if (!options.script) throw new Error("Usage: bun scripts/run-workspace-script.mjs <script>");
  return options;
}

async function readJson(path) {
  return JSON.parse(await readFile(path, "utf8"));
}

async function expandWorkspacePatterns(root, patterns) {
  const directories = [];
  for (const pattern of patterns) {
    if (pattern.startsWith("!")) continue;
    if (pattern.endsWith("/*")) {
      const base = resolve(root, pattern.slice(0, -2));
      if (!existsSync(base)) continue;
      const entries = await readdir(base, { withFileTypes: true });
      for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name, "en"))) {
        if (!entry.isDirectory()) continue;
        const directory = join(base, entry.name);
        if (existsSync(join(directory, "package.json"))) directories.push(directory);
      }
      continue;
    }
    const directory = resolve(root, pattern);
    if (existsSync(join(directory, "package.json"))) directories.push(directory);
  }
  return directories;
}

async function discoverPackages() {
  const cliManifest = await readJson(join(cliRoot, "package.json"));
  const rootManifest = await readJson(join(repositoryRoot, "package.json"));
  const rootPatterns = rootManifest.workspaces ?? [];
  // The CLI sub-workspace is declared in the repository root manifest; keep an
  // optional nested declaration for standalone checkouts.
  const cliPatterns = [
    ...rootPatterns.filter((pattern) => pattern.startsWith("apps/zcode-cli/")),
    ...(cliManifest.workspaces ?? []).map((pattern) => `apps/zcode-cli/${pattern}`),
  ];
  const cliDirectories = await expandWorkspacePatterns(repositoryRoot, cliPatterns);
  const rootDirectories = await expandWorkspacePatterns(repositoryRoot, rootPatterns);
  const packages = new Map();
  for (const [directory, inCliWorkspace] of [
    ...cliDirectories.map((value) => [value, true]),
    ...rootDirectories.map((value) => [value, false]),
  ]) {
    const manifest = await readJson(join(directory, "package.json"));
    if (typeof manifest.name !== "string") continue;
    if (packages.has(manifest.name)) continue;
    const dependencies = {
      ...manifest.dependencies,
      ...manifest.devDependencies,
      ...manifest.optionalDependencies,
    };
    packages.set(manifest.name, {
      directory,
      inCliWorkspace,
      scripts: manifest.scripts ?? {},
      dependencies: Object.keys(dependencies),
    });
  }
  return packages;
}

function orderPackages(packages, selected) {
  const order = [];
  const state = new Map();
  const visit = (name, chain) => {
    const current = state.get(name);
    if (current === "done") return;
    if (current === "visiting") {
      // Cycles are legal in a workspace graph; keep the first visit order and
      // let the package scripts fail if a real build cycle exists.
      return;
    }
    state.set(name, "visiting");
    const pkg = packages.get(name);
    for (const dependency of pkg?.dependencies ?? []) {
      if (packages.has(dependency)) visit(dependency, [...chain, name]);
    }
    state.set(name, "done");
    order.push(name);
  };
  for (const name of selected) visit(name, []);
  return order;
}

const options = parseArgs(process.argv.slice(2));
const packages = await discoverPackages();
const selected = [...packages.entries()]
  .filter(([, pkg]) => pkg.inCliWorkspace || options.includeRoot)
  .filter(([, pkg]) => typeof pkg.scripts[options.script] === "string")
  .map(([name]) => name);
const order = orderPackages(packages, selected).filter((name) => selected.includes(name));

console.log(`[workspace] ${options.script}: ${order.length} package(s)`);
if (options.dryRun) {
  for (const name of order) console.log(`  ${name}`);
  process.exit(0);
}

for (const name of order) {
  const pkg = packages.get(name);
  console.log(`[workspace] ${name}: ${pkg.scripts[options.script]}`);
  const result = spawnSync(process.execPath, ["run", options.script], {
    cwd: pkg.directory,
    env: process.env,
    stdio: "inherit",
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    console.error(`[workspace] ${name} ${options.script} failed with exit ${result.status}`);
    process.exit(result.status ?? 1);
  }
}
