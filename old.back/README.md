# old.back — 已被 Rust 替代的 TS 实现的暂存区

> 本目录当前**没有代码文件**——这是有意的，见下。

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

## 当前状态（2026-09-22，ENTRY 010）

S1 的 **diff island 已接线**（`@zcode/sidecar` + `ZCODE_NATIVE_SIDECARS=diff`，
88/88 fixture 与真实 jsdiff 逐字节一致），jsdiff 运行时依赖已从 core/bootstrap
**移除**（收敛进 adapter）。但 flag 默认关 = 尚无生产流量走 Rust，按规则 3
**本轮不移动任何文件**。第一个 `git mv` 预计发生在 S1 islands 全部接线并翻转
默认值之后。
