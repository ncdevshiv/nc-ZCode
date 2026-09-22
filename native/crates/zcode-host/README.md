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
`renameSession`、`deleteSession`）、`v4/conversation/frame` 通知、两套 delivery
profile、冷恢复 resume。其余 v4 方法返回 `-32601`（不静默吞掉）。

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

1. 单模型回合 turn：无工具调度、无 compact、无 context 装配（系统提示是最小
   占位语义，README 明示而非伪造完整 prompt）。
2. transcript 是 JSONL；TS 的 sqlite 单写者存储未移植（S3 后续）。
3. `fault.*` reasonCode 词表只移植了 inbox 已定义的集合；turn/模型失败用本 host
   定义的 `fault.turn.stopped` / `fault.model.providerNotConfigured` /
   `fault.model.requestFailed` / `fault.command.unsupported:*`（前缀对齐 TS
   命名空间，值本身是本 crate 的稳定契约，完整词表移植在 S3 后续）。
4. subscribe 只做 `mode: "snapshot"`；`base.seq` 续传（resume delta 回放）未实现，
   resync 用全量 recovery snapshot。
5. `renameSession`/`deleteSession` 直接生效（无 turn 副作用的会话级控制）。
6. live provider 语料（runner-debug 录制）在本 checkout 不可用：有
   `--provider-config` 的请求路径存在但未经真实 provider 验证；无配置时
   fail-closed（`ModelErrorCode::ProviderNotConfigured` → turnHeader failed +
   sessionErrorInfo），已由 e2e 覆盖。

## 验证

```sh
cargo test -p zcode-host            # 单元 + e2e stdio（ CARGO_BIN_EXE_zcode-host ）
cargo clippy -p zcode-host --all-targets -- -D warnings
cargo fmt -p zcode-host -- --check
```

e2e（`tests/e2e_stdio.rs`）驱动真实二进制：create → subscribe → sendText →
fail-closed 错误投影 → resync（recovery snapshot 行集一致）→ 重启进程 resume
（行与幂等 ack 不丢）。
