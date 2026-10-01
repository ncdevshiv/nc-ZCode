# zcode-host

S3 Rust agent host（specs/rust-native-crates.md crate map）。stdio NDJSON RPC 服务，
把 `zcode-v4-wire`（帧编解码）、`zcode-core`（串行 admission / identity）、
`zcode-provider`（模型回合执行器）**in-process** 集成为第一个可运行的 Rust host 进程。

## 进程形态

```text
stdin  ──▶ JSON-RPC 请求（v4/connection/flow、v4/conversation/*、v4/command）
stdout ◀── 响应行 + v4/conversation/frame 通知行（共一根有序管道）
stderr  ── 诊断日志（协议管道零污染）
data-dir ── sessions/<id>/meta.json + transcript.jsonl（追加式）
```

启动：`zcode-host --workspace <path> --data-dir <path> [--provider-config <file>]`。

## 本 increment 的能力面（与 TS host 的关系）

已实现：`v4/connection/flow`、`v4/conversation/subscribe|resync|unsubscribe`、
`v4/conversation/rowsRange`、`v4/command`（`createSession`、`sendText`、`stop`、
`renameSession`、`deleteSession`）、`v4/commands/query`（命令幂等查询面）、
`v4/conversation/frame` 通知、两套 delivery
profile、冷恢复 resume。**多回合 agent 循环**：模型回合与工具回合交替直到模型
给出无工具调用的终态文本；工具集 Bash/Read/Write/Edit/Glob/Grep（契约含
readOnly/destructive/concurrentSafe/sideEffectScope/timeout/maxOutput）；
toolCall 行投影（running → success/error/cancelled）；context 装配（系统提示 +
AGENTS.md/CLAUDE.md 发现 + 环境事实）；机械 microcompact（阈值 120k 字符，压最早
的工具结果）。其余 v4 方法返回 `-32601`（不静默吞掉）。

## 集成边界

- **帧**：`zcode-v4-wire` 提供 topic base64 / CRC-32 / VQL / 三载体信封计量；
  超 `MAX_FRAME_BYTES` 拒绝发射并记 `fault.frame.envelopeTooLarge`。
- **admission**：`zcode-core::CommandInbox`（串行 gate、单调 admissionSeq、幂等
  重提、cancel）+ `CommandInboxHost` 回源（revision/logEpoch/transcript）。
  `stop` 不走 inbox FIFO（对齐 TS `sessionStop` 的即时取消路径），否则会排在
  在途 turn 之后死锁。
- **模型**：`zcode-provider` executor（attempt 重试、失败分类、SSE 解析、tool 拼装）。
  请求体由同一 crate 的 `request` 模块构造。

## 与 TS host 的差异（OPEN，不模拟）

8. **账本落盘失败会被吞掉**：`Session::persist_ack` 忽略 `store.append_ack` 的
   失败（既有问题，本轮不修）。后果是：进程重启后该 key 查不到，答 `unknown`。
   方向是 fail-closed（宁可说没有，不给出可能没落盘的事实），不是数据泄漏。
9. **账本只在进程内**：`v4/commands/query` 从 session 账本（每 session 一份，
   `Session::settled_ack`）与 global 账本（`sessionId: null` 桶，`persist_global_ack`
   是唯一写入口）回源；**进程重启后未恢复的 key 一律答 `unknown`**，fail-closed，
   不落盘反查、不猜。
10. **不等待 readyFlight**：TS `queryCommands` 会先 `await readyFlights[sessionId]`
    再查账本；这里没有该等待，所以轮次**在途**时发查询，已 admitted 但未落定的
    命令会答 `unknown`（协议上 = 无已落定记录，语义正确，但调用方要重试）。
11. **`createSession` 没有幂等重提去重**：同一 commandId 重复提交仍会再建一个
    session。这是本 increment 之前就存在的缺口，本轮**不修**；`v4/commands/query`
    如实报告已落定事实（第二次提交会拿到新的 ack）。
12. 工具集是六工具核心集；TS 的 ~114 个 handler（bash 只读分类全量表、workflow
    家族、skills、node-repl、webfetch/websearch……）未移植。
13. bash 用 shell 语义（与 TS 一致：Windows `cmd /D /C`，Unix `sh -c`）；只读
    分类是**保守**首词 + 静态 argv 分类（TS 的 argv 级分类器未移植）；超时杀树用
    `zcode-proc::kill_tree`。
14. input 校验为逐字段靶向校验（完整 JSON Schema 校验器未移植）。
15. 工具调度为模型序串行（TS 的拓扑/并行分组调度器未移植——本批工具以写为主，
    串行是更稳的语义）。
16. transcript 是 JSONL；TS 的 sqlite 单写者存储未移植（S3 后续）。
17. `fault.*` reasonCode 词表只移植了 inbox 已定义的集合；turn/模型失败用本 host
    定义的 `fault.turn.stopped` / `fault.turn.roundLimitExceeded` /
    `fault.turn.repeatedToolCall` / `fault.model.providerNotConfigured` /
    `fault.model.requestFailed` / `fault.command.unsupported:*`（前缀对齐 TS
    命名空间，值本身是本 crate 的稳定契约）。
18. subscribe 只做 `mode: "snapshot"`；`base.seq` 续传（resume delta 回放）未实现，
    resync 用全量 recovery snapshot。
19. `renameSession`/`deleteSession` 直接生效（无 turn 副作用的会话级控制）。
20. compact 只有机械 microcompact；LLM 摘要压缩（autoCompact）未实现（OPEN）。
21. live provider：**两种 wire 形状均已对真实 endpoint 实测通过**
    （2026-09-22）：openai-compatible（stream + tools，ENTRY 008）与
    anthropic（`/v1/messages` + Bearer + 非流式 + tool_use，ENTRY 009）。
    测试用凭据由用户提供，只进临时目录、不随代码提交（repo 规则：凭据不得进
    日志/提交）。仍 OPEN：runner-debug 录制的 live golden-request 语料、
    `generateText` parity、Linux/macOS 运行时。
22. `ProviderConfig` 的 `authStyle`/`stream` 覆盖：网关鉴权样式与流式能力因
    网关而异（该网关对两种形状都只认 Bearer，且 stream 请求被翻译成 OpenAI
    chunk 形状），配置显式声明即可，不必改代码。
23. full-access 是启动参数 `--full-access`（host 级），非 per-session grant。

## 验证

```sh
cargo test -p zcode-host            # 单元 + e2e stdio + 工具循环 e2e
cargo clippy -p zcode-host --all-targets -- -D warnings
cargo fmt -p zcode-host -- --check
```

三个 e2e 测试驱动真实二进制：

- `tests/e2e_stdio.rs`：create → subscribe → sendText → fail-closed 错误投影 →
  resync（recovery snapshot 行集一致）→ 重启进程 resume（行与幂等 ack 不丢）。
- `tests/tool_loop_e2e.rs`（full-access）：本地脚本化 SSE provider 返回
  tool_use(Write) → host 真实写文件 → tool_result 回灌 → 终态文本。断言文件真实
  落盘、toolCall 行 success、第二个请求体含 tool_result 与工具输出。
- `tests/tool_loop_e2e.rs`（denied）：同一 tool_use 被权限闸门拒绝 → 文件不存在
  → 拒绝以 typed 文本回灌模型。

启动：`zcode-host --workspace <path> --data-dir <path> [--provider-config <file>]
[--full-access]`。
