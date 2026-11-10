#!/usr/bin/env node
// Cargo build-cache lifecycle for the native workspace.
//
// WHY: `native/target/` is a pure build artifact directory (gitignored, fully
// reproducible) but nothing was pruning it. Measured 2026-11-08: 188 GB, of which
// 63.7 GB was 797 `.pdb` files and 51 GB was hundreds of stale incremental
// sessions. Cargo has no built-in garbage collection for `target/`, so the
// retention policy has to live outside cargo.
//
// Policy (docs/engineering/resource-budget.md):
//   incremental sessions : keep the newest N per crate, delete the rest
//   orphan PDBs          : delete any .pdb whose binary/rlib sibling is gone
//   full clean           : explicit `cargo clean`, never implicit
//
// Usage:
//   bun scripts/native-cache.mjs status
//   bun scripts/native-cache.mjs sweep [--max-age-days N] [--keep-per-crate N] [--dry-run]
//   bun scripts/native-cache.mjs gc    [--dry-run]     # sweep + orphan pdb purge
//   bun scripts/native-cache.mjs clean                     # cargo clean (full)
//   bun scripts/native-cache.mjs budget                    # compare against the recorded baseline

import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, rmSync, statSync } from "node:fs";
import { basename, dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "..");
const nativeRoot = join(repoRoot, "native");
const targetDir = join(nativeRoot, "target");
const debugDir = join(targetDir, "debug");

// Guard against ever pointing at a path outside the repository.
function assertInsideRepo(candidate) {
  const relativePath = relative(repoRoot, candidate);
  if (relativePath.startsWith("..") || isAbsolute(relativePath)) {
    throw new Error(`Refusing to touch path outside the repository: ${candidate}`);
  }
}

function parseArgs(argv) {
  const flags = {};
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (!argument.startsWith("--")) throw new Error(`unexpected argument "${argument}"`);
    const name = argument.slice(2);
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) {
      flags[name] = true;
      continue;
    }
    flags[name] = value;
    index += 1;
  }
  return flags;
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(2)} ${units[unit]}`;
}

function directorySize(path) {
  if (!existsSync(path)) return 0;
  let total = 0;
  const stack = [path];
  while (stack.length > 0) {
    const current = stack.pop();
    let entries;
    try {
      entries = readdirSync(current, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      const entryPath = join(current, entry.name);
      if (entry.isDirectory()) {
        stack.push(entryPath);
      } else if (entry.isFile()) {
        try {
          total += statSync(entryPath).size;
        } catch {
          // File vanished between readdir and stat; nothing to account for.
        }
      }
    }
  }
  return total;
}

function listFilesRecursive(path, predicate) {
  const matches = [];
  if (!existsSync(path)) return matches;
  const stack = [path];
  while (stack.length > 0) {
    const current = stack.pop();
    let entries;
    try {
      entries = readdirSync(current, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      const entryPath = join(current, entry.name);
      if (entry.isDirectory()) {
        stack.push(entryPath);
      } else if (entry.isFile() && predicate(entryPath)) {
        matches.push(entryPath);
      }
    }
  }
  return matches;
}

function commandStatus() {
  const sections = [
    ["target/", targetDir],
    ["target/debug/deps", join(debugDir, "deps")],
    ["target/debug/incremental", join(debugDir, "incremental")],
    ["target/debug/build", join(debugDir, "build")],
  ];
  const report = {};
  for (const [label, path] of sections) {
    report[label] = formatBytes(directorySize(path));
  }
  const pdbs = listFilesRecursive(targetDir, (file) => file.endsWith(".pdb"));
  const pdbBytes = pdbs.reduce((sum, file) => {
    try {
      return sum + statSync(file).size;
    } catch {
      return sum;
    }
  }, 0);
  report[".pdb files"] = `${pdbs.length} (${formatBytes(pdbBytes)})`;
  const incrementalSessions = existsSync(join(debugDir, "incremental"))
    ? readdirSync(join(debugDir, "incremental"), { withFileTypes: true }).filter((e) => e.isDirectory()).length
    : 0;
  report["incremental sessions"] = String(incrementalSessions);
  return report;
}

function removePath(path, dryRun) {
  assertInsideRepo(path);
  if (!existsSync(path)) return false;
  if (dryRun) {
    console.log(`  [dry-run] would remove ${path}`);
    return true;
  }
  rmSync(path, { force: true, recursive: true });
  return true;
}

// Keep the newest `keepPerCrate` sessions per crate, plus anything newer than
// `maxAgeDays`. Everything else is dead weight: cargo never revisits a session
// whose crate hash changed.
function commandSweep({ dryRun, maxAgeDays, keepPerCrate }) {
  const incrementalDir = join(debugDir, "incremental");
  if (!existsSync(incrementalDir)) {
    console.log("no incremental directory; nothing to sweep");
    return 0;
  }
  const now = Date.now();
  const maxAgeMs = maxAgeDays * 24 * 60 * 60 * 1000;

  const byCrate = new Map();
  for (const entry of readdirSync(incrementalDir, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const sessionDir = join(incrementalDir, entry.name);
    const crateName = entry.name.slice(0, entry.name.lastIndexOf("-"));
    let mtimeMs = 0;
    try {
      mtimeMs = statSync(sessionDir).mtimeMs;
    } catch {
      continue;
    }
    if (!byCrate.has(crateName)) byCrate.set(crateName, []);
    byCrate.get(crateName).push({ sessionDir, mtimeMs });
  }

  let removed = 0;
  for (const [crateName, sessions] of byCrate) {
    sessions.sort((a, b) => b.mtimeMs - a.mtimeMs);
    sessions.forEach((session, index) => {
      const tooOld = now - session.mtimeMs > maxAgeMs;
      const beyondRetention = index >= keepPerCrate;
      if (!tooOld && !beyondRetention) return;
      const reason = tooOld ? `older than ${maxAgeDays}d` : `beyond keep-per-crate ${keepPerCrate}`;
      console.log(`  ${crateName}: ${reason} (${new Date(session.mtimeMs).toISOString()})`);
      if (removePath(session.sessionDir, dryRun)) removed += 1;
    });
  }
  console.log(`${dryRun ? "[dry-run] " : ""}swept ${removed} incremental session(s)`);
  return removed;
}

// A PDB without its binary/rlib sibling cannot be loaded by a debugger and is
// never referenced again.
function commandPrunePdb({ dryRun }) {
  const pdbs = listFilesRecursive(targetDir, (file) => file.endsWith(".pdb"));
  let removed = 0;
  let reclaimed = 0;
  for (const pdb of pdbs) {
    const stem = pdb.slice(0, -".pdb".length);
    const siblings = [".exe", ".rlib", ".dll", ".so", ".dylib", ".a"].map((extension) => `${stem}${extension}`);
    if (siblings.some((candidate) => existsSync(candidate))) continue;
    let size = 0;
    try {
      size = statSync(pdb).size;
    } catch {
      continue;
    }
    console.log(`  orphan ${basename(pdb)} (${formatBytes(size)})`);
    if (removePath(pdb, dryRun)) {
      removed += 1;
      reclaimed += size;
    }
  }
  console.log(
    `${dryRun ? "[dry-run] " : ""}pruned ${removed} orphan .pdb file(s), reclaiming ${formatBytes(reclaimed)}`,
  );
  return { removed, reclaimed };
}

function commandClean() {
  console.log("==> cargo clean (full target/ rebuild on next build)");
  const result = spawnSync("cargo", ["clean", "--manifest-path", join(nativeRoot, "Cargo.toml")], {
    cwd: repoRoot,
    stdio: "inherit",
    shell: false,
  });
  if (result.error?.code === "ENOENT") {
    console.error("cargo not found on PATH; cannot clean the native target");
    process.exitCode = 1;
    return;
  }
  if (result.status !== 0) {
    console.error(`cargo clean failed with exit code ${result.status}`);
    process.exitCode = 1;
  }
}

// Thresholds from docs/engineering/resource-budget.md. Exceeding one is a
// warning, not an error: growth is expected, unbounded growth is not.
const BUDGET = {
  "target/": 20 * 1024 * 1024 * 1024,
  "target/debug/deps": 12 * 1024 * 1024 * 1024,
  "target/debug/incremental": 4 * 1024 * 1024 * 1024,
};

function commandBudget() {
  const status = commandStatus();
  console.log("budget check (thresholds from docs/engineering/resource-budget.md)");
  let overBudget = false;
  for (const [label, limit] of Object.entries(BUDGET)) {
    const path = label === "target/" ? targetDir : join(debugDir, label.replace("target/debug/", ""));
    const bytes = directorySize(path);
    const ratio = bytes / limit;
    const flag = ratio > 1 ? "OVER" : ratio > 0.8 ? "near" : "ok";
    if (ratio > 1) overBudget = true;
    console.log(`  ${flag.padEnd(4)} ${label.padEnd(26)} ${formatBytes(bytes).padStart(12)} / ${formatBytes(limit)}`);
  }
  console.log(`  ${"".padEnd(4)} ${".pdb files".padEnd(26)} ${status[".pdb files"].padStart(12)}`);
  console.log(`  ${"".padEnd(4)} ${"incremental sessions".padEnd(26)} ${status["incremental sessions"].padStart(12)}`);
  if (overBudget) {
    console.log("\nover budget: run `bun scripts/native-cache.mjs gc`");
    process.exitCode = 1;
  }
}

const [command, ...rest] = process.argv.slice(2);
const flags = parseArgs(rest);
const dryRun = Boolean(flags["dry-run"]);
const maxAgeDays = Number(flags["max-age-days"] ?? 7);
const keepPerCrate = Number(flags["keep-per-crate"] ?? 2);

switch (command) {
  case "status":
    console.table(commandStatus());
    break;
  case "sweep":
    commandSweep({ dryRun, maxAgeDays, keepPerCrate });
    break;
  case "prune-pdb":
    commandPrunePdb({ dryRun });
    break;
  case "gc":
    commandSweep({ dryRun, maxAgeDays, keepPerCrate });
    commandPrunePdb({ dryRun });
    break;
  case "clean":
    commandClean();
    break;
  case "budget":
    commandBudget();
    break;
  default:
    console.error(
      "usage: bun scripts/native-cache.mjs <status|sweep|gc|clean|budget|prune-pdb> [--dry-run] [--max-age-days N] [--keep-per-crate N]",
    );
    process.exitCode = 1;
}
