//! sidecar 开关与二进制解析。
//!
//! 开关：环境变量 `ZCODE_NATIVE_SIDECARS`（逗号分隔的 sidecar 名，或 `all`）。
//! 用途/优先级/错误行为在 `specs/rust-native-crates.md` 的 cutover 章节登记：
//!   缺省 = 全部关闭（TS 路径，零行为变化）；
//!   `diff` = diff 热路径走 Rust sidecar，不可用时降级 jsdiff 并告警；
//!   协议错误（版本不匹配/Illegal 帧）冒泡——那是构建/配置事故，不静默。
//!
//! 二进制解析（按优先级）：
//!   1. `ZCODE_SIDECAR_DIR/<name>[.exe]`（发布包与 CI 用它指向随包二进制）；
//!   2. 仓库相对路径 `<repo>/native/target/{release,debug}/<name>[.exe]`
//!      （repo 根由本包位置推导：packages/sidecar → repo 根四级上行；开发态用）。

import { existsSync } from "node:fs";
import path from "node:path";

const FLAG_ENV = "ZCODE_NATIVE_SIDECARS";
const BIN_DIR_ENV = "ZCODE_SIDECAR_DIR";

function readFlag(): string {
  return process.env[FLAG_ENV]?.trim() ?? "";
}

export function sidecarEnabled(name: string): boolean {
  const flag = readFlag();
  if (flag.length === 0) return false;
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
  for (const profile of ["release", "debug"]) {
    const candidate = path.join(root, "native", "target", profile, `${name}${suffix}`);
    if (existsSync(candidate)) return candidate;
  }
  return null;
}
