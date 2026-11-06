//! sidecar switch and binary parsing.
//!
//! Switch: environment variable `ZCODE_NATIVE_SIDECARS` (comma separated island names, or `all`) **optional**
//! Override the default; `ZCODE_NATIVE_SIDECARS_OFF=1` is the overall kill switch.
//! Island enabled by default: `diff` (byte-parity 88/88), `image` (semantic parity,
//! User 2026-09-22 decided to flip; size/format semantics are consistent, byte-level differences are recorded in crate README),
//! `scan` / `watch` (event/anti-shake semantic mirroring TS, see the respective crate README for differences one by one).
//! `proc` is off by default (the recycling timing affects the MCP stdio life cycle, etc. soak).
//! The rest of the islands are off by default (wiring or item-by-item difference verification has not been completed).
//! Semantics:
//! `ZCODE_NATIVE_SIDECARS_OFF=1` → All off (return to TS path for troubleshooting);
//! Not set → default set (diff on, others off);
//! Set to list/`all` → precise control (without kill switch).
//! Binary missing/spawn failed → facade alerts and downgrades the TS path without blocking user turns;
//! Protocol error (version mismatch/Illegal frame/4xx/5xx) → throw error (configuration and version offset is a build accident).
//!
//! Binary parsing (by priority):
//! 1. `ZCODE_SIDECAR_DIR/<name>[.exe]` (desktop runtime explicit injection; release package points to
//!      resources/tools/sidecar）；
//!   2. `<repo>/packages/desktop/bundled-tools/<platformKey>/sidecar/<name>[.exe]`
//! (`prepare:native-sidecars` staging point; both dev and direct-running CLI can find it);
//! 3. Warehouse relative path `<repo>/native/target/{release,debug}/<name>[.exe]`
//! (The repo root is discovered step by step upstream from the location of this package).

import { existsSync } from "node:fs";
import path from "node:path";

const FLAG_ENV = "ZCODE_NATIVE_SIDECARS";
const DISABLE_ENV = "ZCODE_NATIVE_SIDECARS_OFF";
const BIN_DIR_ENV = "ZCODE_SIDECAR_DIR";

/**
* The island is enabled by default (parity is green, and the release package comes with it; see the spec cutover chapter).
 *
* - diff: byte-parity (88/88 fixture, real jsdiff comparison)
* - image: semantic parity (user 2026-09-22 decided to flip; size/format consistent, byte differences recorded)
* - scan: entry/error set consistent with TS fsWalker (order is TS concurrency=1 equivalent semantics,
* Concurrency only affects the order and not the collection - see zcode-scan README difference table 1–5)
* - watch: event and anti-shake semantic mirroring TS fileWatcherService (150ms anti-shake, single clear path transparent transmission)
 *
* proc is temporarily off by default: it changes the recycling timing of the MCP stdio sub-process (the impact is greater than other islands),
* Wait for soak to be observed before turning over.
 */
const DEFAULT_ON_ISLANDS: readonly string[] = ["diff", "image", "scan", "watch"];

function readFlag(): string {
  return process.env[FLAG_ENV]?.trim() ?? "";
}

export function sidecarEnabled(name: string): boolean {
// Single kill switch: When troubleshooting, you can return to the TS path as a whole without changing the code.
  if (process.env[DISABLE_ENV]?.trim() === "1") return false;
  const flag = readFlag();
  if (flag.length === 0) return DEFAULT_ON_ISLANDS.includes(name);
  if (flag === "all" || flag === "*") return true;
  return flag
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0)
    .includes(name);
}

function looksLikeRepoRoot(candidate: string): boolean {
  return existsSync(path.join(candidate, "native")) && existsSync(path.join(candidate, "bunfig.toml"));
}

/// Repo root discovery: Go up step by step from the location of this module, and find both `native/` and `bunfig.toml`
/// directory. The source code (tsx) and the built product (dist) paths have different depths. They go up to the root and do not guess the fixed series.
function repoRootFromModule(): string | null {
  let current = path.dirname(new URL(import.meta.url).pathname);
  if (process.platform === "win32" && current.startsWith("/")) {
    current = current.slice(1); // file URL → 路径（/F:/… → F:/…）
  }
  for (let depth = 0; depth < 8; depth += 1) {
    if (looksLikeRepoRoot(current)) return current;
    const parent = path.dirname(current);
    if (parent === current) break;
    current = parent;
  }
  return null;
}

export function resolveSidecarBin(name: string): string | null {
  const suffix = process.platform === "win32" ? ".exe" : "";
  const explicitDir = process.env[BIN_DIR_ENV]?.trim();
  if (explicitDir) {
    const explicit = path.join(explicitDir, `${name}${suffix}`);
    return existsSync(explicit) ? explicit : null;
  }
  const root = repoRootFromModule();
  if (!root) return null;
// 2) prepare:native-sidecars staging point (the dev layout after unpacking the release package is consistent with direct running).
  const bundled = path.join(
    root,
    "packages",
    "desktop",
    "bundled-tools",
    `${process.platform}-${process.arch}`,
    "sidecar",
    `${name}${suffix}`,
  );
  if (existsSync(bundled)) return bundled;
// 3) cargo product (the development state when prepare is not run).
  for (const profile of ["release", "debug"]) {
    const candidate = path.join(root, "native", "target", profile, `${name}${suffix}`);
    if (existsSync(candidate)) return candidate;
  }
  return null;
}
