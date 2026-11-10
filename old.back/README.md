# old.back — 已被 Rust 替代的 TS 实现的暂存区

> 本目录的第一次真实移动发生在 2026-11-09（见下），此前的"零移动"备注保留为历史。
> <!-- trace:2026-11-06 docs sync — S1 默认面已从 diff-only 变为 4-on，与下方四条件规则不冲突（默认开 ≠ 已零引用）。 -->

## 已移入的模块

| 日期 | 原路径 | 为什么可以移动（四条件） |
| ---- | ------ | ------------------------ |
| 2026-11-09 | `apps/zcode-cli/packages/swift-bridge/` | ① 无产品面使用：全仓 `isSwiftAvailable` / `detectSwiftVersion` / `@zcode/swift-bridge` 零 importer（macOS 原生能力已由 `nc-code-shell` 的 Rust 命令承担：keychain 导入、print-to-pdf、窗口控制）；② N/A（无对等运行时）；③ N/A（无流量可切）；④ `typecheck` + `knip`/全仓 grep 证明零引用。该包本身是带 `TODO` 的占位实现，违反 P-1 反占位门禁，保留的害处更大。 |


## 这是什么

`old.back/` 是 S6 cutover 的落地缓冲：当某个 TS 模块的 Rust 替代品完成产品面、
切缝真正切换流量、且 typecheck/knip 证明零引用后，原 TS 实现整体移入这里
（保留原始相对路径），而不是直接删除。好处：parity diff / 回滚都只需一次
`git mv`，历史不丢。

## 移动规则（四条件，缺一不可）

1. Rust 替代品覆盖**产品面**（真实命令路径存在，不只是单测 parity）；
2. conformance/parity 测试全绿（黄金向量 / e2e stdio / fixture）；
3. per-workspace flag 真正把生产流量切到 Rust 路径，且 flag 关时 TS 路径仍全绿
   （回滚保留）；
4. 切换后 typecheck + knip 证明该 TS 模块在工作区零引用，移动不会破坏
   `pnpm build`。

## 阶段顺序（每个阶段结束时决定本目录的第一次真实移动）

1. **Agent host + loop** — 完成（zcode-host：工具层 + 工具调用循环 + context）。
2. **Provider executor** — 每个 provider 一个 flag；TS `adapters/src/model/**`
   在 parity 语料绿后逐模块移入。
3. **S1 islands** — sidecar 接入 TS 应用（TS 保持业务所有权）→ parity →
   移动 `fileWatcherService.ts` / `fsWalker.ts` / taskkill 调用点 /
   jimp 热路径 / jsdiff 调用点。**← 第一个真实移动的候选阶段。**
4. **Server** — `zcode-server` 挂在 web 入口 flag 后；然后 `packages/server`。
5. **Shell** — Tauri 逐特性替代 Electron main（R4 buckets）；然后
   `packages/desktop/src/main`。
6. **CLI/SEA** — `zcode` 原生二进制；删除 `build:sea`/postject 脚本。
7. **S6 退出** — parity soak 窗口后删除本目录；Node 离开打包链。

## 当前状态（2026-11-06 刷新，ENTRY 010 之后）

S1 五岛已全部接线：`@zcode/sidecar` 桥 NDJSON stdio，`DEFAULT_ON_ISLANDS`
=`["diff","image","scan","watch"]` 4-on（`apps/zcode-cli/packages/sidecar/src/config.ts:44`），
`proc` 默认关（MCP stdio 生命周期 soak）。diff 88/88 fixture 与真实 jsdiff 逐字节一致；
image 语义 parity（用户 2026-09-22 决定翻默认）；scan/watch 语义镜像 TS。
默认开 ≠ 已可移动：四条件仍全未满足（TS 调用点仍是双路 facade，knip 仍有引用），
按规则 3 **本轮仍不移动任何文件**。第一个 `git mv` 需等零引用证明后才发生。
