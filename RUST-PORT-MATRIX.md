# RUST-PORT-MATRIX.md

> 非 UI TypeScript → Rust 迁移的**全量能力清单与覆盖矩阵**。
> 生成方式：机器枚举（脚本逐模块提取 class/function/export + 行数）+
> 四个分区的只读代码审计（带 `path:line` 证据）。
>日期：2026-09-23；**文档刷新 2026-11-06（§0/§2 表格已按代码重验，结论口径见 §0.1 备注）**。本文件回答两个问题：**我们有没有完整可用的 Rust 移植？**
> **如果有，就替换 TS。**
><!-- trace:2026-11-06 docs sync — Rust totals glob-counted: 630 .rs files / 417,119 lines (src-only 448 files); 54 nc-code-* crates (native/Cargo.toml:7); host 198 files/83,922 lines; shell 32 files. Tools 43 (32 sync + 11 async, nc-code-host/src/tools/mod.rs:258-369). V4 31/31 (src/rpc.rs:56 empty). channel_router 8 routes (nc-code-server/src/lib.rs:283-286 + remote_api.rs:53-56). Backend real (nc-code-shell/src/backend.rs:66,93). Prior 98 files/30,065 lines, 12 tools, 2 routes are 2026-09-23 snapshot, retained as history. -->
>
> ⚠️ **口径与可复现性（2026-10-01 补）**：生成用的枚举脚本**没有入库**，所以下面
> §0 的 TS 侧数字（487,949 loc / 2,033 文件 / 11,873 导出 / 391 类 / 17,727 方法 /
> 4,587 类型）**无法从仓库复算**，只能当作 2026-09-23 那天的快照读。它们按「非 UI
> TypeScript + 模块枚举」口径统计，与 `specs/typescript-frontend-only.md` §9 的
> 「迁移债 2,282 文件 / 510,755 行」口径**不同**（后者按 git 枚举 + 行数口径），
> 两者不是同一把尺，不要互相印证。Rust 侧 2026-11-06 glob 复算：**630 `.rs` 文件 /
> 417,119 行（`src/` 内 448 文件）**，其中 host 198 / 83,922，shell 32 / 13,967；
> 2026-09-23 的 98 / 30,065 是当时快照，不再可比。**本结论**
> （S6 在进、五候选卡条件 3/4、零移动）由逐条 `path:line` 证据支撑，不依赖上述计数。

---

## 0. Conclusion (let’s talk about the results first)

**No. The port is far from complete, and no TS modules currently exist that can be legally replaced. (2026-11-06: S6 在进，零合法替换仍成立——四条件未满足。)**

| 口径        | TS（非 UI） |           Rust（2026-11-06 glob） |      比例 |
| ----------- | ----------: | -------------: | --------: |
| 代码行      | **487,949** (2026-09-23 快照) | **417,119** | **~85%** raw LOC（不同口径，不可比；code-only 估计 ~55–65%） |
| 模块 / 文件 |       2,033 | **630** `.rs` | 31.0% 文件 |
| crate       | — | **54** `nc-code-*` (`native/Cargo.toml:7`) | — |
| 导出函数    |      11,873 | **43 tools** (32 sync + 11 async, `tools/mod.rs:258-369`) | — |
| 类          |         391 |              — |         — |
| 方法        |      17,727 |              — |         — |
| 类型/接口   |       4,587 |              — |         — |

按能力组统计（四个分区审计 + 机器枚举的合并口径）：

| 分区                                                 |   能力组数 | RUST-PORTED | RUST-PARTIAL | TS-ONLY |
| ---------------------------------------------------- | ---------: | ----------: | -----------: | ------: |
| CLI core（core/adapters/bootstrap/contracts/cli）    |  21 + 6 包 |           0 |            6 |      15 |
| CLI runtime（tui/node-repl/workflow/telemetry/i18n） |         85 |           0 |            2 |      83 |
| services / shared / rpc / server / provider          |        610 |           5 |            9 |     596 |
| desktop main（Electron）                             |         26 |           0 |            4 |      22 |
| **合计**                                             | **约 750** |       **5** |       **21** | **716** |

**Full replaceable: 0 pcs. ** Partial coverage: 21 (all with Rust adjacency implementation, missing key product aspects).

---

## 1. Why can’t the 5 1:1 candidates be replaced (determine the conditions one by one)

审计点名了 5 个「看起来 1:1」的候选。逐条走 `old.back/README.md` 的四条件，
**All fail on condition 3 or 4**:

| 候选                                               | Rust 对应                   | 条件1 产品面 | 条件2 parity | 条件3 flag 切流量 | 条件4 零引用 | 结论         |
| -------------------------------------------------- | --------------------------- | :----------: | :----------: | :---------------: | :----------: | ------------ |
| `services/storage/adapters/fsWalker.ts` (173)      | `zcode-scan/src/lib.rs`     |      ✅      |      ✅      |     ✅ 默认开     |      ❌      | **不可移动** |
| `services/fileWatcher/fileWatcherService.ts` (155) | `zcode-watch/src/lib.rs`    |      ✅      |      ✅      |     ✅ 默认开     |      ❌      | **不可移动** |
| `shared/zcode-protocol-v4/wire-binary.ts`          | `zcode-v4-wire/src/lib.rs`  |      ⚠️      |      ✅      |        ❌         |      ❌      | **不可移动** |
| `services/session/tasksDatabase/schema-v1.ts`      | `zcode-host/src/storage.rs` |      ⚠️      |  ✅ 跨语言   |        ❌         |      ❌      | **不可移动** |
| `zcode-protocol-v4/core.ts` `DELIVERY_PROFILES`    | `zcode-host/src/profile.rs` |      ⚠️      |      ✅      |        ❌         |      ❌      | **不可移动** |

失败原因不是「移植质量差」，而是**结构性的**：

### 1.1 scan / watch: TS file is a dual-channel facade and cannot be deleted.

`fsWalker.ts:49-73`of`tryNativeScan`Adjust first`nativeScanStorageRoot`（第 54 行），
成功就回流条目；**任何失败都 `return null` 落回文件下半部分的 TS 遍历**。
`fileWatcherService.ts:66`Same reason:`createNativeFileWatcher` 返回 null 时走下面的
`node:fs.watch`。

这正是设计意图（sidecar 缺失时降级不阻断用户轮次）。但它意味着：
**flag is off, sidecar is missing, and spawn fails—in these three production-reachable states, TS implementation is the only correct path. **
删掉它 = 删掉降级路径 = 违反条件 3「flag 关时 TS 路径仍全绿（回滚保留）」。

而且 `inProcessScanRunner.ts:40`and`fsCleaner.ts:11`Both import`walkStorageRoot`
——**条件 4（零引用）也不成立**。

> 这是 ENTRY 019 的结论，本轮独立复核**再次证实**：diff/scan/watch 的 TS 调用点是
> **活的转发代码，不是死实现**。**本轮不移动任何文件。**

### 1.2 wire-binary / schema-v1 / DELIVERY_PROFILES: flag does not cut traffic

- `wire-binary.ts` → Rust `nc-code-v4-wire` 存在且有 golden 向量，但**没有任何 TS 生产
  调用点切到它**；host 的 v4 通道已接线 **31/31**（`src/rpc.rs:56`
  `V4_METHODS_UNIMPLEMENTED = &[]`），不再是 6 个。但切流 flag 仍未把 **TS 调用点**
  切过去——条件 3 仍失败是「TS 没切走」，不再是「Rust 没实现」。
- `schema-v1.ts` → Rust `storage.rs` 复刻了 tasksDatabase schema（跨语言 parity 已绿，
  node:sqlite 能读 Rust 建的库），但**写路径仍由 TS 拥有**，Rust 只是读。
- `DELIVERY_PROFILES` → Rust `profile.rs` 有镜像实现，但 profile 选择由 TS bootstrap 决定。

三者都卡在条件 3：**flag 没有把 TS 生产调用点切到 Rust 路径（双路 facade 结构）。**

---

## 2. Partition list

### 2.1 CLI core（`apps/zcode-cli/packages`）

`core/src` 21 个能力组，**95,227 loc**，Rust 覆盖情况：

| 能力组             | files |    loc | Rust 状态                                                        |
| ------------------ | ----: | -----: | ---------------------------------------------------------------- |
| `runtime`          |   163 | 37,015 | `turn.rs`+`lib.rs` 共 1,150 loc；✅ **V4 31/31** (`rpc.rs:56` 空)【2026-09-23 快照 6/33 已过时】 |
| `tool`             |   173 | 32,166 | ✅ **43 工具 (32 sync + 11 async)** vs TS 111 handler【快照 12 已过时】 |
| `workflow`         |    31 |  5,282 | ✅ DAG 引擎 `nc-code-workflow` + `workflow_service/`；7 读方法派发 `lib.rs:906-955` |
| `subagent`         |    21 |  3,846 | ❌                                                               |
| `browser-client`   |     7 |  3,063 | ❌                                                               |
| `hooks`            |    19 |  2,788 | ❌                                                               |
| `agent`            |    11 |  2,295 | ❌ 无 agent tree owner                                           |
| `permission`       |     6 |  1,023 | ⚠️ 只有 bash readonly 分类器；无规则存储、无审批门               |
| `memory`           |    10 |  1,013 | ⚠️ `memory.rs` 111 loc（MEMORY.md + TS 上限）                    |
| `context`          |    13 |    932 | ⚠️ `turn_context.rs` 266 loc（仅 skills/memory 路由）            |
| `mcp`              |     3 |    920 | ⚠️ `mcp.rs`+`mcp_tools.rs` 539 loc；http/OAuth/annotation 未接线 |
| `runtime-task`     |     5 |    918 | ❌                                                               |
| `repl`             |     5 |    914 | ❌ 无 JS eval host                                               |
| `compact`          |     6 |    863 | ⚠️ `compact.rs` 226 loc                                          |
| `session-context`  |     4 |    523 | ❌                                                               |
| `plugin-reference` |     4 |    498 | ❌                                                               |
| `system-reminder`  |     3 |    445 | ❌                                                               |
| `errors`           |     1 |    382 | ⚠️ 仅 ToolError 码                                               |
| `telemetry`        |     1 |    255 | ❌                                                               |
| `embedded-search`  |     2 |     71 | ❌                                                               |
| `model`            |     1 |     15 | ✅ `zcode-provider` 7,445 loc（executor/stream/retry/transport） |

其他包：`bootstrap/src` 63,568 · `adapters/src` 51,868 · `contracts/src` 21,538 ·
`dynamic-workflow/src` 20,034 · `cli/src` 10,690 · `tui/src` 8,087。

**Tool face comparison (product face)**: TS ~45 tool names, Rust **43**（32 sync `tools/mod.rs:258-352`
+ 11 async `:359-369`；Agent/Task 别名不另计）。2026-11-06 后已接：`WebSearch`、`ApplyPatch`、
`js`（NodeRepl）、`EnterPlanMode`/`ExitPlanMode`、`AskUser`、`SendMessage`、`TaskOutput`/`TaskStop`、
`Cron*`、`ReadSessionContext`、`submit_result`、`RespondToCoordinator`、`escalate`、`OffPeak*`、
Workflow 家族 8 个（`CreateWorkflow`/`AmendWorkflow`/`SaveWorkflow`/`EvalWorkflowSnippet`/
`ResumeWorkflowRun`/`ListWorkflowRuns`/`GetWorkflowRun`/`ResolveWorkflowQuestion`/`ListSavedWorkflows`）。
长尾仍 OPEN：bash argv 级分类全量表、TS 拓扑并行调度器。

### 2.2 CLI runtime（tui / node-repl / workflow / telemetry / i18n）

**85 competency groups · 0 RUST-PORTED · 2 RUST-PARTIAL · 83 TS-ONLY**

- `tui` 非视觉运行时 25 组 + 视觉/布局组件 28 文件 4,423 loc（另有 theme 6 文件 341 loc）。
  依赖 TS-only 渲染器 `@mbears/opentui-core`。**Rust 存在 0 %。**
- `node-repl-host` 8 组（1,407 loc）——**最接近 1:1**：有真实结构对应
  （`zcode-host/src/mcp.rs` 的 MCP 客户端已覆盖 **stdio + streamable HTTP + legacy
  HTTP+SSE** 三种传输（2026-11-09 ENTRY 131），外加 `zcode-proc` 进程监管），
  但 JS eval、per-call worker、browser/CUA 注入仍缺。
- `dynamic-workflow`30 groups +`-runtime` 5 组（20,034+1,299 loc）。
**Largest subsystem**; of which 11,621 loc 静态分析构建于 **TypeScript compiler API**。
引擎核心已移植 Rust（`nc-code-workflow` + `nc-code-host/src/workflow_service/`，7 读方法
派发 `lib.rs:906-955`，枚举面 `service.rs:1076`、孤儿对账 `service.rs:1356`）；
TS compiler 前端在 compiled-kernel 路径按设计复用（`specs/disabled-features.md §3.3`），
Node 仍为沙箱运行时。旧「没有 Rust 等价物」已过时。
- `telemetry`12 groups (3,884 locs) ·`i18n` 5 组（1,144 loc）· 2 个 partial：
  node-repl 进程守卫（邻接 `zcode-proc`）、workflow 结算/并发/事件扇出
  （邻接 `zcode-provider/retry.rs:279-380` + `zcode-core/protocol.rs:66-68`）。

### 2.3 services / shared / rpc / server / provider

**610 modules · ~4,513 exports · ~154,300 locs (Rust side 78 files / 21,885 locs, 0.14:1)**
**5 RUST-PORTED · 9 RUST-PARTIAL · 596 TS-ONLY**

协议面已接线（2026-11-06）：

| 面                            |  TS |                                   Rust（2026-11-06） |
| ----------------------------- | --: | --------------------------------------: |
| v4 方法                       |  31 | **31/31**（`V4_METHODS_UNIMPLEMENTED=&[]`, `rpc.rs:56`） |
| v4 通知                       |   4 | **4**（frame/telemetry/local-ttft/cua 常量） |
| v4 命令类型                   |  30+ | 命令分发在 `commands/mod.rs`（workflow 4 条经 `workflow_commands.rs` `run_*`） |
| legacy `zcodeProtocolMethods` |  74 | 废弃同构：channel 层用 `nc-code-channels` registry（见下） |

TS services 实际发出 26 个 v4 + 51 个 legacy 方法；v4 31/31 已对齐 TS `V4_METHODS`。
5 个 RUST-PORTED 即上表 5 个 1:1 候选（全部因条件 3/4 不可移动）。
Server：Rust `nc-code-server` **8 live routes**（4 base `lib.rs:283-286` + 4
`/api/remote/*` `remote_api.rs:53-56`，`channel_router` `lib.rs:274-293`；
`/ws` live `lib.rs:286`），经 `nc-code-app` 真 registry + `nc-code-shell/src/backend.rs:66,93`
真实 serve；非 181 loc stub。`conversation-share` 等 service 域已在 `nc-code-services-*` 35 个 crate 中。

### 2.4 desktop main（Electron）

**26 Capability Groups · 0 RUST-PORTED · 4 RUST-PARTIAL · 22 TS-ONLY**

`nc-code-shell` 实测 **32 文件 / 13,967 loc**（对照 main 47,468 loc = ~29% loc，非 0.43% 快照）；
嵌入真实后端（`src/backend.rs:66` 装配 registry + `:93` serve），`dev:desktop`→`dev:tauri` live（`package.json:15,21`）。
Tauri commands 已远超 9 个（见 shell README：tray/dialogs/clipboard/notifications/deep-link/updater/
print_pdf/chrome_import/browser_view/computer 等）。

- 托盘已实现。`nc-code-server` 8 路由全 live（见 §2.3），`ProcessLivenessHost` 仅为
  无管理 session-host 场景的诚实默认（`lib.rs:ProcessLivenessHost`），不再是
  **replace `desktopRemoteSessions.ts`（890）零行**——`/api/remote/*` 4 路由已接
  （`remote_api.rs:53-56`，经 `RemoteConnectionRegistry`）。完全替代仍 OPEN：桌面 e2e `ZCODE_NATIVE_HOST=1` 未验。

最高风险未接管桶：

1. **BrowserView / Playwright ~11,900 loc**（占 main 的 25%），`browserGuestManager.ts`
   单文件 4,439。**Tauri 无 BrowserView —— 这是 OPEN 产品决策，不是工程问题。**
2. **自动更新 3,052 loc**（自研 `electron-updater` Provider + 4 个 Linux 后端 + Windows 文件锁清理）。
3. **DPAPI / 凭据 ~1,275 loc**：Windows PowerShell `ProtectedData.Unprotect`
   （`chromeCredentialManager.ts:76`）、macOS Keychain（`:53`）、
   Windows App-Bound 需提权 C# helper exe（`windowsChromeAppBoundKey.ts:418`）。
4. **托盘 102 loc**（Windows-only，8 项菜单）。
5. **原生桥接**：无 `.node`/`.dll`/FFI；1 个 Swift + 1 个 C# 文件，均脚本编译。
   `apps/zcode-cli/packages/swift-bridge` 是 20 行 stub、**零 importer**——
   desktop main 并不依赖它。

约 2,700 loc 是纯 renderer 服务的 chrome，随 Electron 一起消亡，不需要 Rust 替代。

---

## 3. Access control actual test (this round of review)

| 门禁                                | 结果                                                                                                               |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `cargo test --workspace`            | ✅ 321 tests（快照 2026-09-23；2026-11-06 docs sync 未重跑，用 glob 计数代替） |
| `cargo clippy -- -D warnings`       | ✅ 0                                                                                                               |
| `cargo fmt --check`                 | ✅ 0                                                                                                               |
| `bun run typecheck` / `fmt:check`      | ✅                                                                                                                 |
| `bun run architecture:check --changed` | ✅ 0 violations                                                                                                    |
| `bun run knip`| ⚠️ Report unused files/exports (**existing baseline**,`node-repl-host`Wait for no entry; include`swift-bridge` 零 importer）——非本轮引入 |
| `bun run lint`                         | ✅ 70 warnings / 0 errors（基线）                                                                                  |

`ZCODE_NATIVE_HOST`Default **OFF**(`zcodeAgentProcessManager.ts:357,368`）——
host 本身尚未承载生产流量。`ZCODE_NATIVE_SIDECARS` 默认开
`diff, image, scan, watch`；`proc` default-off (4-on islands: diff,image,scan,watch) (`config.ts:44`）。

---

## 4. What is the difference between "replaceable" and "replaceable"?

按依赖顺序（前一阶段不完成，后一阶段无法验证）：

1. **host default flip** —— ✅ v4 方法已 31/31、工具已 43；剩余仅
   `ZCODE_NATIVE_HOST` 把 TS host spawn 缺省切到 Rust（及桌面 e2e `ZCODE_NATIVE_HOST=1` 未验）。
2. **JS 工具删除** —— 一旦 host 承载生产流量，`nativeStructuredPatch` 等 facade 的
   调用方（core/bootstrap/desktop）要真正切走，TS 双路实现才变成死代码。
3. **首个合法 `old.back` 移动** —— 需要一个「flag 开 → Rust，flag 关 → 不存在」
   的真切换点。当前所有 island 都是**双路 facade**，结构上永远满足不了条件 4。
4. **workflow 引擎**（20,034 loc，其中 11,621 依赖 TS compiler API）——
   引擎核心已移植 Rust（`nc-code-workflow`）；静态分析部分走 compiled-kernel
   + Node 沙箱（设计保留 `disabled-features.md §3.3`）。
5. **Electron → Rust shell**（47,468 loc）—— BrowserView 决策未定，是产品决策。

---

## 5. Actions in this round

- **未移动任何文件到 `old.back/`。** 5 个候选逐条走四条件，全部在条件 3/4 失败。
- 建立了这份能力清单（首次有全量 function-wise 口径）。
- 复核确认 ENTRY 019 结论：island 的 TS 调用点是活转发代码，不能删。
