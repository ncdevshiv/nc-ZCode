// Spec: nc-tools 与 zcode 工具面的边界（2026-09-22，ENTRY 016）
//
// 背景：nc-tools 以 in-process Kernel 接入 zcode-host（ENTRY 015，不经 MCP），
// 两套工具面现在同时对模型可见。本文件裁定它们的关系，避免模型看到"两个半的
// 能力"或互相打架的写路径。
//
// ── 裁定一：nc-tools 是能力超集，不是替换 ──────────────────────────────
//
// 逐族核对（证据：core/src/tool/handlers/* 与 nc-tools crates/*/lib.rs 的
// 注册名清单）：
//
// | 能力         | zcode 面                    | nc-tools 面              | 关系                |
// |--------------|-----------------------------|--------------------------|---------------------|
// | 文件读/写     | Read / Write / Edit          | fs.read/write/... patch.* | **重叠**，见裁定二   |
// | 检索         | Glob / Grep                 | search.grep/files        | **重叠**，见裁定二   |
// | diff         | （无独立工具，走 Edit 前后对比）| text.diff / patch.stage  | nc-tools 独有        |
// | git          | **无**（只有 Bash 里的 git 命令）| git.status/diff/commit… | nc-tools 独有（补齐）|
// | 进程         | Bash 跑命令                 | proc.spawn/start/kill…   | nc-tools 独有（无 shell 逃生口）|
// | 网络         | WebFetch（单 URL 抓取）      | net.http/probePort       | nc-tools 独有        |
// | 测试/包       | （无）                       | test.run / pkg.*         | nc-tools 独有        |
// | 多 agent     | Agent 工具（TS 侧）          | agent.register/…/lock   | 两套并存，见裁定三  |
// | 工作区快照    | （无）                       | sys.snapshot/journal     | nc-tools 独有        |
// | 批处理        | （无）                       | batch.execute            | nc-tools 独有        |
//
// 关键发现：**zcode 没有独立 git 工具**（git 只经 Bash 命令，bash 工具面里还有
// 一整套 git 只读分类子策略）。所以 nc-tools 的 git.* 家族不是"重复造轮子"，而是
// 把 zcode 埋在 Bash 里的 git 能力提升成 typed 工具——补齐而非重叠。
//
// ── 裁定二：重叠面（文件/检索）保留两套，但标注关系 ────────────────────
//
// 理由：两套语义不同、不是同一实现的两个入口：
//   - zcode Read/Edit 带 read-before-edit 状态、model 专用截断、行号输出；
//   - nc-tools fs.* 走 Kernel 的 patch 精确性守卫（occurrence counts）、
//     append-only journal、声明式 patch set。
// 强行合并要重写其中一方的核心语义，收益低于风险。
// 处置：**都保留**，在系统提示里给模型一条明确路由指引（哪类用哪个），
// 并在本文件记录选择理由。journal（nc-tools）与 transcript（zcode）是两条独立
// 审计链：前者记工具调用，后者记会话行投影——职责不同，互补。
//
// ── 裁定三：多 agent 两套并存，职责分离 ──────────────────────────────
//
//   - zcode Agent/Task 工具 = 模型视角的"派活给子 agent"（子会话、继承 traceId）；
//   - nc-tools agent.* = 进程间协调（register/heartbeat/lock/peers），
//     面向多个 host 进程的会话级互斥与状态。
// 两者正交（一个是对模型的能力，一个是 runtime 的协调），并存，README 记录。
//
// ── 裁定四：write 类工具的权限面 ──────────────────────────────────────
//
// zcode 的 Bash/Write/Edit 走 ToolMetadata 声明（needsApproval/full-access 闸门）。
// nc-tools 工具在 host 侧以 readOnly=true 注册（保守放行），因为 nc-tools 自己做
// 变更管控：写入必须走校验过的 patch（occurrence count 守卫）或 stage/preview/
// commit 的可审阅变更集（SDK 原则 3）。**不把 zcode 的 full-access 闸门套在
// nc-tools 上**：那会拦下 patch.stage 这类只写变更集不落盘的安全操作，也拦不住
// fs.write 这种 SDK 认为安全的直写。权限边界的最终归属：host 侧的
// `--full-access` 只管 zcode 原生工具；nc-tools 家族遵守 SDK 自己的 guard。
// 这是有意的边界（见 nc_tools.rs contract() 注释），不是遗漏。

use std::collections::BTreeMap;

/// 重叠族 → 路由指引（注入系统提示，README 差异表同步）。
pub fn overlap_routing_hint() -> &'static str {
    "Tool routing: for ordinary file reads/writes and search use Read/Write/Edit/Glob/Grep; \
     for git, processes, tests, packages, network probing, multi-process coordination, \
     workspace snapshots, and reviewable patch sets use the nc-tools family \
     (git.*, proc.*, test.run, pkg.*, net.*, agent.*, sys.*, patch.stage, batch.execute). \
     fs.* / search.* duplicate the basics with stricter change guards and a separate audit \
     journal — prefer Read/Write/Edit/Glob/Grep for those unless you need the patch semantics."
}

/// 本 host 同时暴露的工具面规模（zcode 原生 + nc-tools），供自省/提示注入。
pub fn tool_surface_summary(zcode_tools: &[String], nc_tools: &[String]) -> String {
    let mut families: BTreeMap<&str, usize> = BTreeMap::new();
    for name in nc_tools {
        let family = name.split('.').next().unwrap_or("other");
        *families.entry(family).or_default() += 1;
    }
    let nc_summary: Vec<String> = families
        .iter()
        .map(|(family, count)| format!("{family}:{count}"))
        .collect();
    format!(
        "native tools: {}; nc-tools: {} ({})",
        zcode_tools.len(),
        nc_tools.len(),
        nc_summary.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_hint_names_both_surfaces_and_the_pick() {
        let hint = overlap_routing_hint();
        assert!(hint.contains("Read/Write/Edit/Glob/Grep"));
        assert!(hint.contains("git.*") && hint.contains("proc.*"));
        assert!(hint.contains("patch.stage"));
    }

    #[test]
    fn summary_counts_families() {
        let summary = tool_surface_summary(
            &["Read".to_string(), "Bash".to_string()],
            &[
                "fs.read".to_string(),
                "fs.write".to_string(),
                "git.status".to_string(),
            ],
        );
        assert!(summary.contains("native tools: 2"));
        assert!(summary.contains("nc-tools: 3"));
        assert!(summary.contains("fs:2"));
        assert!(summary.contains("git:1"));
    }
}
