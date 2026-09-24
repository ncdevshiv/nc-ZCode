//! sidecar 开关与二进制解析。
//!
//! 开关：环境变量 `ZCODE_NATIVE_SIDECARS`（逗号分隔 island 名，或 `all`）**可选**
//! 覆盖默认；`ZCODE_NATIVE_SIDECARS_OFF=1` 是整体 kill switch。
//! 默认开启的 island：`diff`（byte-parity 88/88）、`image`（语义 parity，
//! 用户 2026-09-22 决定翻转；尺寸/格式语义一致，字节级差异记录在 crate README）。
//! 其余 island 默认关（接线或逐条差异核对未完成）。
//! 语义：
//!   `ZCODE_NATIVE_SIDECARS_OFF=1` → 全部关（排障退回 TS 路径）；
//!   未设置 → 默认集合（diff 开，其余关）；
//!   设置为名单/`all` → 精确控制（不含 kill switch）。
//! 二进制缺失/spawn 失败 → facade 告警并降级 TS 路径，不阻断用户轮次；
//! 协议错误（版本不匹配/Illegal 帧/4xx/5xx）→ 抛错（配置与版本偏移是构建事故）。
//!
//! 二进制解析（按优先级）：
//!   1. `ZCODE_SIDECAR_DIR/<name>[.exe]`（桌面 runtime 显式注入；发布包指向
//!      resources/tools/sidecar）；
//!   2. `<repo>/packages/desktop/bundled-tools/<platformKey>/sidecar/<name>[.exe]`
//!      （`prepare:native-sidecars` 暂存点；dev 与直跑 CLI 都能找到）；
//!   3. 仓库相对路径 `<repo>/native/target/{release,debug}/<name>[.exe]`
//!      （repo 根由本包位置逐级上行发现）。

import { existsSync } from "node:fs";
import path from "node:path";

const FLAG_ENV = "ZCODE_NATIVE_SIDECARS";
const DISABLE_ENV = "ZCODE_NATIVE_SIDECARS_OFF";
const BIN_DIR_ENV = "ZCODE_SIDECAR_DIR";

/** 默认开启的 island（parity 已绿、发布包已随带；见 spec cutover 章节）。 */
const DEFAULT_ON_ISLANDS: readonly string[] = ["diff", "image"];

function readFlag(): string {
  return process.env[FLAG_ENV]?.trim() ?? "";
}

export function sidecarEnabled(name: string): boolean {
  // 单一 kill switch：排障时可整体退回 TS 路径而不改代码。
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
  return existsSync(path.join(candidate, "native")) && existsSync(path.join(candidate, "pnpm-workspace.yaml"));
}

/// repo 根发现：从本模块位置逐级上行，找同时含 `native/` 与 `pnpm-workspace.yaml`
/// 的目录。源码（tsx）与构建产物（dist）路径深度不同，上行到根即止，不猜固定级数。
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
  // 2) prepare:native-sidecars 暂存点（发布包解包后的 dev 布局与直跑一致）。
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
  // 3) cargo 产物（未跑 prepare 时的开发态兜底）。
  for (const profile of ["release", "debug"]) {
    const candidate = path.join(root, "native", "target", profile, `${name}${suffix}`);
    if (existsSync(candidate)) return candidate;
  }
  return null;
}
