#!/usr/bin/env node
// Build and staging the Rust stdio host to a path already recognized by the desktop parser.
//
// WHY This script exists: `zcodeAgentProcessManager.ts` in `ZCODE_NATIVE_HOST=1` but cannot be found
// When using binary, the user is prompted to run `pnpm --dir packages/desktop prepare:native-host`, while in the warehouse
// There has never been this script - an error message that pushes people to a non-existent entrance. Complete it here and put "Where is the product?"
// Pinned with candidate paths for the TS parser (`platformArchKey()` = `${process.platform}-${process.arch}`).
//
// Explicitly exit on failure (no silent skipping): During migration and stream switching, a false "ready" is worse than failure.
// Because the desktop will actually hand over the workspace to this binary.

import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, statSync, utimesSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const nativeRoot = join(repoRoot, "native");

function binaryName(base = "zcode-host") {
  return process.platform === "win32" ? `${base}.exe` : base;
}

function fail(message, hint) {
  console.error(`[prepare:native-host] ${message}`);
  if (hint) {
    console.error(` ${hint}`);
  }
  process.exit(1);
}

function runCargoBuild({ release }) {
  const args = ["build", "-p", "nc-code-host", "--bin", "zcode-host"];
  if (release) {
    args.push("--release");
  }
  console.log(`==> Building zcode-host (cargo ${args.join(" ")})`);
  const result = spawnSync("cargo", args, {
    cwd: nativeRoot,
    stdio: "inherit",
    // No shell: Node's DEP0190 explicitly warns that "shell splicing with parameters" is an injection surface, and rustup
    // The real `cargo.exe` agent is installed in ~/.cargo/bin, and PATH search does not require cmd packaging.
  });
  if (result.error?.code === "ENOENT") {
    fail(
      "cargo not found on PATH",
      "install the Rust toolchain (rustup) and re-run; the native host cannot be built without it",
    );
  }
  if (result.status !== 0) {fail(`cargo build failed with exit code ${result.status}`, "see the compiler output above");
  }
}

function builtBinary({ release }) {
  return join(nativeRoot, "target", release ? "release" : "debug", binaryName());
}

function stage({ platformKey, release }) {
  const source = builtBinary({ release });
  if (!existsSync(source)) {
    fail(
      `build reported success but ${source} is missing`,
      "check `native/target` layout overrides (CARGO_TARGET_DIR) and re-run",
    );
  }
  const size = statSync(source).size;
  if (size === 0) {
    fail(`${source} is empty`, "the build produced a zero-byte binary; refusing to stage it");
  }

  const targetDir = join(
    repoRoot,
    "packages",
    "desktop",
    "bundled-tools",
    platformKey,
    "native-host",
  );
  mkdirSync(targetDir, { recursive: true });
  const target = join(targetDir, binaryName());
  copyFileSync(source, target);
  if (process.platform !== "win32") {
    chmodSync(target, 0o755);
  } else {
    // The parser on Windows only uses existsSync to determine, but copying and retaining the source file timestamp allows "after rebuilding"
    // Found immediately" can be checked; explicitly write mtime once to avoid the illusion of caching old files.
    const { atime, mtime } = statSync(source);
    utimesSync(target, atime, mtime);
  }
  console.log(`==> Staged zcode-host (${size} bytes)`);
  console.log(` source: ${source}`);
  console.log(` target: ${target}`);
  console.log(" run the desktop against it with: ZCODE_NATIVE_HOST=1 pnpm dev:desktop");
  return target;
}

const options = {
  platformKey:
    process.env.ZCODE_NATIVE_HOST_PLATFORM_KEY ||
    `${process.env.ZCODE_TARGET_OS || process.platform}-${process.env.ZCODE_TARGET_ARCH || process.arch}`,
  release: !process.argv.includes("--debug"),
};

if (options.release) {
  runCargoBuild({ release: true });
} else {
  runCargoBuild({ release: false });
}
stage(options);