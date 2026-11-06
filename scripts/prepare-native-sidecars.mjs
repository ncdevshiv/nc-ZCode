#!/usr/bin/env node
// Stage Rust sidecar binaries (native/crates/*/bin) into
// packages/desktop/bundled-tools/<platformKey>/sidecar/ so release builds ship
// them under resources/tools, on the same release chain as the native-search
// tools.
//
// Idempotent: .bundle-meta.json records a source-manifest fingerprint and each
// binary sha256; when both are current the build is skipped. ZCODE_TARGET_OS /
// ZCODE_TARGET_ARCH select a cross target (defaults to the host).
//
// Run: node scripts/prepare-native-sidecars.mjs
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(import.meta.url);
const repoRoot = resolve(dirname(scriptPath), "..");
const nativeManifest = join(repoRoot, "native", "Cargo.toml");
const cargoLock = join(repoRoot, "native", "Cargo.lock");

/** Binaries published as sidecars (lib-only crates such as core/v4-wire excluded). */
export const SIDECAR_CRATES = [
  "nc-code-diff",
  "nc-code-scan",
  "nc-code-watch",
  "nc-code-proc",
  "nc-code-image",
];

const BUNDLE_META_FILE = ".bundle-meta.json";

function sha256File(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

const CRATE_TO_BIN = {
  "nc-code-diff": "zcode-diff",
  "nc-code-scan": "zcode-scan",
  "nc-code-watch": "zcode-watch",
  "nc-code-proc": "zcode-proc",
  "nc-code-image": "zcode-image",
};

function binaryFileName(crate) {
  const bin = CRATE_TO_BIN[crate] || crate.replace(/^nc-code-/, "zcode-");
  return process.platform === "win32" ? `${bin}.exe` : bin;
}

function manifestFingerprint() {
  const hash = createHash("sha256");
  hash.update(readFileSync(nativeManifest));
  hash.update(readFileSync(cargoLock));
  for (const crate of SIDECAR_CRATES) {
    const manifest = join(repoRoot, "native", "crates", crate, "Cargo.toml");
    if (existsSync(manifest)) hash.update(readFileSync(manifest));
  }
  return hash.digest("hex");
}

function isBundleCurrent(bundleDir, fingerprint, entries) {
  const metaPath = join(bundleDir, BUNDLE_META_FILE);
  if (!existsSync(metaPath)) return false;
  try {
    const meta = JSON.parse(readFileSync(metaPath, "utf8"));
    if (meta.manifestFingerprint !== fingerprint) return false;
    for (const entry of entries) {
      const binaryPath = join(bundleDir, entry.file);
      if (!existsSync(binaryPath)) return false;
      if (sha256File(binaryPath) !== entry.sha256) return false;
    }
    return true;
  } catch {
    return false;
  }
}

export async function prepareNativeSidecars({
  platform = process.env.ZCODE_TARGET_OS ?? process.platform,
  arch = process.env.ZCODE_TARGET_ARCH ?? process.arch,
} = {}) {
  const key = `${platform}-${arch}`;
  const bundleDir = join(repoRoot, "packages", "desktop", "bundled-tools", key, "sidecar");
  const fingerprint = manifestFingerprint();
  const targetDir = join(repoRoot, "native", "target", "release");

  const staged = SIDECAR_CRATES.map((crate) => ({
    crate,
    file: binaryFileName(crate),
    source: join(targetDir, binaryFileName(crate)),
  }));

  const needsBuild = staged.some((entry) => !existsSync(entry.source));
  if (needsBuild) {
    console.log("==> Building native sidecars (cargo release)");
    const args = ["build", "--manifest-path", nativeManifest, "--release"];
    for (const crate of SIDECAR_CRATES) args.push("-p", crate);
    execFileSync("cargo", args, { stdio: "inherit", cwd: repoRoot });
  }

  const entries = staged.map((entry) => {
    if (!existsSync(entry.source)) {
      throw new Error(`sidecar binary missing after build: ${entry.source}`);
    }
    return { ...entry, sha256: sha256File(entry.source) };
  });

  mkdirSync(bundleDir, { recursive: true });
  if (isBundleCurrent(bundleDir, fingerprint, entries)) {
    console.log(`==> Native sidecars already current (${key}) - skipped`);
    return bundleDir;
  }

  for (const entry of entries) {
    const destination = join(bundleDir, entry.file);
    copyFileSync(entry.source, destination);
    if (platform !== "win32") chmodSync(destination, 0o755);
  }
  writeFileSync(
    join(bundleDir, BUNDLE_META_FILE),
    `${JSON.stringify(
      {
        provider: "native-sidecar",
        crates: SIDECAR_CRATES,
        manifestFingerprint: fingerprint,
        platform: key,
        binaries: entries.map((entry) => ({
          crate: entry.crate,
          file: entry.file,
          sha256: entry.sha256,
        })),
      },
      null,
      2,
    )}\n`,
  );
  console.log(`==> Staged ${entries.length} sidecars to ${bundleDir}`);
  return bundleDir;
}

if (process.argv[1] && resolve(process.argv[1]) === resolve(scriptPath)) {
  prepareNativeSidecars().catch((error) => {
    console.error(`prepare-native-sidecars failed: ${error.message}`);
    process.exitCode = 1;
  });
}

export function describeSidecarBundleStats(bundleDir) {
  if (!existsSync(bundleDir)) return { files: 0, bytes: 0 };
  const files = SIDECAR_CRATES.map((crate) => join(bundleDir, binaryFileName(crate)));
  let bytes = 0;
  for (const file of files) {
    if (existsSync(file)) bytes += statSync(file).size;
  }
  return { files: files.filter((file) => existsSync(file)).length, bytes };
}
