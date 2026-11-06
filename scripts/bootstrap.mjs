#!/usr/bin/env bun

import process from "node:process";
import { existsSync, readFileSync } from "node:fs";
import { delimiter, dirname, join, resolve } from "node:path";
import { homedir } from "node:os";
import { fileURLToPath } from "node:url";
import { runCommand } from "./spawn-command.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const rootDir = resolve(scriptDir, "..");
const gitCommand = "git";
const withRemoteAssets = process.argv.includes("--with-remote");
const bunCommand = process.platform === "win32" ? "bun.exe" : "bun";

function prependPathEntries(pathValue, entries) {
  const currentEntries = pathValue ? pathValue.split(delimiter) : [];
  return [...entries, ...currentEntries].join(delimiter);
}

function resolvePinnedNodeBin() {
  if (process.platform === "win32") {
    return undefined;
  }

  const miseConfigPath = resolve(rootDir, "mise.toml");
  if (!existsSync(miseConfigPath)) {
    return undefined;
  }

  const miseConfig = readFileSync(miseConfigPath, "utf8");
  const match = miseConfig.match(/^\s*node\s*=\s*"([^"]+)"/m);
  const version = match?.[1]?.trim();
  if (!version) {
    return undefined;
  }

  const nvmNodeBin = join(homedir(), ".nvm", "versions", "node", `v${version}`, "bin");
  return existsSync(nvmNodeBin) ? nvmNodeBin : undefined;
}

function resolveBootstrapWithRemoteEnv(baseEnv = process.env) {
  if (!withRemoteAssets) {
    return {};
  }

  const pathEntries = [resolvePinnedNodeBin()].filter(Boolean);

  return {
    // bootstrap:with-remote is the manual remote-asset entry; keep Husky out of
    // the nested installs and serialize workspace builds to bound memory peaks.
    HUSKY: "0",
    ZCODE_BOOTSTRAP_WITH_REMOTE: "1",
    ...(pathEntries.length > 0
      ? {
          PATH: prependPathEntries(baseEnv.PATH, pathEntries),
        }
      : {}),
  };
}

const bootstrapWithRemoteEnv = resolveBootstrapWithRemoteEnv();

function runGit(args) {
  runCommand(gitCommand, args, {
    cwd: rootDir,
    env: process.env,
  });
}

function runBun(args, options = {}) {
  runCommand(bunCommand, args, {
    cwd: rootDir,
    env: {
      ...process.env,
      ...bootstrapWithRemoteEnv,
      ...options.env,
    },
  });
}

function runBootstrapServerBuild() {
  const serverDir = resolve(rootDir, "packages/server");

  // bootstrap:with-remote's final build used to reuse build:bootstrap, which
  // nested a second `build:remote`; keep the low-memory direct entry points.
  runCommand(bunCommand, ["x", "tsup"], {
    cwd: serverDir,
    env: {
      ...process.env,
      ...bootstrapWithRemoteEnv,
    },
  });
  runCommand(bunCommand, ["x", "tsx", "build-remote.ts"], {
    cwd: serverDir,
    env: {
      ...process.env,
      ...bootstrapWithRemoteEnv,
    },
  });
}

function runBootstrapDesktopBuild() {
  const desktopDir = resolve(rootDir, "packages/desktop");
  // The desktop app bundle build is intentionally skipped here: remote assets
  // already consumed a memory peak, and the Tauri migration owns packaging.
  runCommand(bunCommand, ["scripts/build-metadata.mjs"], {
    cwd: desktopDir,
    env: {
      ...process.env,
      ...bootstrapWithRemoteEnv,
    },
  });
  console.log("[bootstrap:with-remote] skip desktop app bundle build; runtime assets are prepared");
}

function runBootstrapWithRemoteBuild() {
  for (const filter of ["@zcode/rpc", "@zcode/web", "@zcode/formal-proof"]) {
    // Serialize the package builds: remote assets already consumed one memory peak.
    runBun(["run", "--filter", filter, "build"]);
  }
  runBootstrapServerBuild();
  runBootstrapDesktopBuild();
}

runGit(["submodule", "update", "--init", "--recursive", "apps/zcode-cli"]);

runBun(["install"]);

runBun(["run", "prepare:desktop-runtime"], {
  env: withRemoteAssets
    ? {}
    : {
        // Local bootstrap only prepares desktop runtime assets; remote mock-cdn
        // is opt-in via bootstrap:with-remote.
        ZCODE_SKIP_REMOTE_ASSETS: "1",
      },
});

if (withRemoteAssets) {
  runBootstrapWithRemoteBuild();
} else {
  runBun(["run", "build:bootstrap"]);
}
