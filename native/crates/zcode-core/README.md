# zcode-core — S3 核心原语

目标：`specs/rust-native-crates.md` S3（`zcode-core` = `command-inbox.ts` admission +
owner/lease + identity key）。

## 状态所有者（owner 声明）

| 状态                                                                                   | 所有者                                                    | 共享方式                                                                       |
| -------------------------------------------------------------------------------------- | --------------------------------------------------------- | ------------------------------------------------------------------------------ |
| admission 全量状态（in-flight / pending FIFO / settled LRU / admissionSeq / live pin） | `CommandInbox` 内部 `Mutex<InboxState>` —— **唯一写路径** | `&self` 方法（互斥内建）；跨任务用 `SharedCommandInbox`（`Arc<CommandInbox>`） |
| owner/lease 表                                                                         | `OwnerLeaseTable`（`&mut self`，单所有者）                | 跨线程由调用方外层同步（镜 TS `taskRealtimeBus` 单线程 bus）                   |
| 身份 key                                                                               | 无状态纯函数 `identity_key`                               | —                                                                              |

**为什么不是 `&mut self`**：TS 调用点即共享形态——`v4-gateway.ts` 单个 `inbox` 实例被
并发 `handleCommand`（await session gate）、`queryCommands` 复用，settle 由执行完成方回调，
即多任务同时进入同一实例。Rust 侧唯一所有者落在内部 `Mutex<InboxState>`（单写路径），
对外 `&self` + `SharedCommandInbox(Arc)` 正是该共享形态的镜像（任务条款
“仅当共享所有权匹配 TS call site 才加 `Arc<Mutex>` 包装”——匹配，故采用）。

## 事件面（host 订阅什么）

TS 没有 emitter；host 的订阅面就是返回值与回调：

1. `handle(raw)` → `Outcome::Ack`（立即收口：invalidPayload / sessionNotFound /
   stale / reject / noop / duplicate）或 `Outcome::Execute`（admitted：`admission_seq`、
   `admitted_at`、`queue_item_id`、`settle` 凭证 `SettleTicket`）。
2. `settle(ticket, final)` → `Settled(ack)` / `AlreadySettled`（TS closure 幂等 noop）/
   `Err(Stale)`（stale-run 防护）；同时释放 session FIFO gate 并唤醒排队者与
   duplicate/query 等待者。
3. `query(keys)` → 在途 key 等待终态（TS `await inflight.final`）后返回原 ACK
   （不套 duplicate）；未命中 `Unknown`。
4. TS 中不可观测、Rust 侧新增的**诊断**（不改协议行为）：`pending_command_ids`、
   `waiter_stats`、`admission_seq_for`。

pending（排队）在 TS 是 `AsyncGateRegistry` 的隐式 promise 链，这里显式化为每桶
FIFO 队列——严格先到先得，无容量上限（TS 门链同样无上限；inbox 内唯一限额是
settled 幂等表 `PROTOCOL_V4_LIMITS.idempotencyTablePerSession = 512`）。

## 无 TS 源可镜像时的设计决策

1. **pending 取消**（`cancel_pending`）：TS inbox 无取消 API——排队者是阻塞的
   `handle`，不可中断；在途取消走 gateway `cancelCommandInput` + `settle` 终态。
   Rust 镜像：pending 取消 = 出队 + `handle` 返回
   `rejected / fault.command.admissionCancelled`（不消耗 admissionSeq，之后重提为全新
   裁决——与 TS「key gate 放行后重新裁决」一致）；**在途取消 = `settle` 带
   rejected 终态**（per source）。未知命令 `cancel_pending` → `NotPending`。
2. **TTL / renew / steal**：TS `taskRealtimeBus` lease 无过期（靠显式 release /
   host 退出清理），本 crate 按 S3 验收补齐 TTL 过期、`renew`、`steal_if_expired`、
   `assert_current`（fail closed）；key 组合 `\0`、`owned_by_other_host` 判定、
   同 owner+run 刷新保留 `acquiredAt`、release 不匹配静默 no-op 均镜像该 TS 源。
3. **stale settle ticket**：TS settle 是绑定 closure（`settled` 标志 + 捕获 commandId），
   “settling a non-current commandId” 无法由 TS 表达；Rust 用 `SettleTicket`
   （bucket + commandId + run token）显式表达：token 不匹配或非当前在途 →
   `Err(NotCurrent)`；同 token 重复 → `AlreadySettled`（= TS noop）。

## 与 TS 的已知移植差异（有据）

| 项                    | TS                                                         | 本 crate                                                                                    |
| --------------------- | ---------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| payload 逐类型校验    | zod `commandPayloadSchemas`，失败 → `proto.invalidPayload` | **不移植**（schema 归 `packages/shared`，roadmap S0 source of truth）；payload 透传 `Value` |
| `ttft` 字段           | `localTtftContextSchema` 校验（非法 → parse 失败）         | 透传不校验                                                                                  |
| parse 错误文案        | zod 原文                                                   | 自有文案（reasonCode 同为 `proto.invalidPayload`）                                          |
| `baseRevision` 非整数 | zod 接受，decide 判 stale                                  | parse 直接拒绝（revision 契约为整数）                                                       |
| query 回源失败        | 可 reject → `fault.command.queryUnavailable`               | host 回调不可失败；queryUnavailable 仅保留为在途事实被 clearSession 清除时的兑底            |
| 同 commandId 在途等待 | key gate 内 `await pinned.final`                           | 谓词等待（可观察量一致：等终态、回 duplicate）                                              |

## 测试

`tests/`（共 26 个，全部确定性：并发用线程 + 有界自旋 `spin_until`（yield，无墙钟
sleep），时间全部注入——host `now_ms` 与 `ManualClock`）：

- `inbox_tests.rs`（13）：解析失败 ACK 与 `extractCommandId`；CAS/ROW 字段缺失
  parse 拒绝（TS 合成文案）；并发 admission 串行（活跃窗口 max-active == 1 +
  admissionSeq 恰为 1..=N 唯一）；在途等待/收口后幂等 duplicate；pending 严格 FIFO；
  cancel pending 出队不消耗 admissionSeq（在途/未知 → NotPending）；settle 凭证
  stale token 拒绝 / 同 token 幂等 / 清库后 NotCurrent；query 等终态（不套 duplicate）
  - retryAck 规则（failed 保留状态、其余转 duplicate）；guard noop 记忆 vs reject
    不记忆；sessionNotFound、`@global` 桶与会话桶 admissionSeq 独立、clearSession 清零；
    CAS staleRevision / staleLogEpoch（epoch 先于 revision）与 row resolver 裁决；
    settled LRU 512 淘汰最旧；live pin → settle → release → clear 生命周期。
- `lease_tests.rs`（11）：`\0` key 组合；acquire/renew 保留 acquiredAt；owner/run
  冲突（`owned_by_other_host`）；注入时钟过期 steal 与到点边界；同 owner 换 run 接管
  后旧 run 的 stale token 拒绝；renew NotFound/Expired；release owner+run 全匹配才删
  （否则静默 no-op）；`assert_current` 在 `now >= expires_at` fail closed；非正 TTL
  全操作拒绝；惰性过期视图。
- `identity_tests.rs`（2）：`identity_key` 全矩阵（None/空/纯空白 → path 回退、
  带空格 trim、NBSP、ZWNBSP、NEL、unicode、path 不 trim）与 `js_trim` JS 空白集
  直测（NBSP 裁 / ZWNBSP 裁 / NEL 保留 / ZWSP 不裁 / LS/PS 裁）。

**TS 测试移植说明**：仓库内 `command-inbox.ts`、lease、identity **没有对应 TS 测试
文件**（已 grep `*.test.ts`/`*.spec.ts` 核实，0 匹配），不存在可移植的 TS 断言，
**无跳过项**；上列用例按 TS 源码行为（`command.ts`、`command-inbox.ts`、
`taskRealtimeBus.ts`）与 S3 验收编写。测试同时抓出并已修复 `js_trim` 全空白输入的
切片越界 panic（`&s[len..0]`，见 `src/identity.rs` 修复注释）。
