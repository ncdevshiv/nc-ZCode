# zcode-provider

Provider 执行层 island（`lib`，暂无 `bin`——接线到 host 的调用面尚未接通，见
OPEN 第 10 条），按 `specs/rust-native-crates.md` §Provider 承载 TS
`apps/zcode-cli/packages/adapters/src/model/` 的可观察契约：请求体构造、HTTP
传输、SSE 帧解析、业务失败分类、重试边界投影与 attempt 执行循环。所有 TS 语义
均逐条对照源码核读后镜像（runner-stream.ts / retry / classify / business /
openai+anthropic stream 读取面 100% 完成）。

## 模块地图

| 模块                      | 职责                                                                  |
| ------------------------- | --------------------------------------------------------------------- |
| `request.ts → request.rs` | ModelRequest 收窄子集 → openai / anthropic 请求体 JSON                |
| `transport.rs`            | `ProviderTransport` / `AttemptClock` 契约 + `TransportFailure`        |
| `http_transport.rs`       | reqwest 实现：open / read_body / next_chunk，idle 窗口逐调用计时      |
| `sse.rs`                  | SSE 分帧（feed / finish 残帧），`data` 空与 `[DONE]` 丢弃             |
| `business.rs`             | fetch 层浅业务：非 2xx 响应体 + 帧内 error 形状 → 业务失败结构        |
| `business_finish.rs`      | runner 层深业务：finish 形状门控 + 候选记录深扫（512 节点上限）       |
| `classify.rs`             | 失败 → retry / terminal / queued / ticket 分类（含 off-peak 标记）    |
| `retry.rs`                | `calculate_retry_delay`：`round(capped × 0.75)` + 注入抖动            |
| `openai_stream.rs`        | openai 兼容帧 → 归一 parts（tool_calls 增量拼装、usage、finish）      |
| `anthropic_stream.rs`     | anthropic 事件 → 归一 parts（content_block 索引投影、stop_reason）    |
| `assemble.rs`             | `RetryBoundaryBuffer`：重试边界投影 + prelude 暂存/提交（6 内联测试） |
| `frame_pipeline.rs`       | 帧处理总线：浅业务 → 诊断 → preserve 投影 → 深门 → 解析 → 拼装        |
| `executor.rs`             | attempt 循环：预算、queued 冻结、取消探针、idle、空完成重试           |
| `video.rs` / `schema.rs`  | 附件媒体类型收敛 / 请求 schema 校验                                   |

## 帧处理顺序（单帧，`process_frame`）

```text
SSE 字节 ──feed──▶ ①浅业务（每帧无条件，data 空与 [DONE] 已在 sse 层丢弃）
                    ②诊断 note_raw_finish_reason + preserve 边界投影/提交
                    ③深业务门（finish 形状才进门，进门才深扫）
                    ④adapter 解析（anthropic.handle / openai.on_chunk）
                    ⑤error 拦截 → 拼装 → 缓冲/hold → visible 事件
自然 EOF ──finish_stream──▶ 残帧复用 ①–⑤ → provider flush（openai 闭合活动
                    tool 并产 Finish；anthropic 守卫防重复）→ assembler.flush
```

- **浅业务**要求非零 `code` 或 `success:false`（仅 message 永不触发）；**深门**
  只认 finish / error 形状，进门后无条件深扫。
- **psf（`provider_success_false`）信号源唯一**：feed 时 `note_raw_finish_reason`
  记录的原始 finish_reason 与深扫的 top / `finish_reason` /
  `providerMetadata.rawFinishReason` / `choices[0]` 同源 → feed 时拦截总是先于
  EOF 深扫，结果一致（分类 → 不可重试 → Terminal）。EOF 侧深扫保留为防御网
  （`eof_business_failure` 文档注明预占关系）。
- **emitted-boundary 重试门无条件生效（两种 preserve 模式）**：失败前有任何已
  提交输出 → Terminal，不再 adapter 重试（对齐 TS `canRetryStreamFailure`）；
  idle 超时只在首帧提交前可重试。

## attempt 循环要点

```text
execute() ──探针 cancelled（can_retry_empty 用，先于 run_attempt 一次）
  └─ run_attempt: open → head
       ├─ 非 2xx → read_body → 浅业务/分类 → AttemptFailure
       ├─ 2xx 非 SSE → 整体缓冲 → JSON 业务（queued 走 off-peak 60s 等待）
       └─ 2xx SSE → feed 循环（逐调用 idle 窗口）→ finish_stream
  失败后: force_terminal / emitted_boundary → Terminal
          queued（429/3105…）→ attempt -= 1（冻结预算）+ 强制重试
          退避 = round(capped × 0.75)，抖动注入（测试固定 0.5 → base11/factor2
          实得 8ms / 17ms；off-peak queued 默认 60_000ms，cap 300_000ms）
          预算耗尽 → 自然返回（failure: None，不是错误）
  取消: open 前终态；failure 与退避之间有取消探针（sleep 中途无检查点）
```

## 与 TS 源的差异（有意为之 / 已记录，OPEN）

1. **anthropic 截断 flush 残差**：TS 在流截断时 flush 未闭合 content_block 的
   行为依赖其 reader 状态机；Rust 侧 `finish_stream` 对残帧尽力 flush，极端截断
   （半行 + 无分隔符）下事件数可能少于 TS —— 未用真实截断语料验证，OPEN。
2. **深门 wire-vs-typed 残差**：深门判定在「原始 wire JSON」上做，TS 判定部分
   路径先经类型化读取；对畸形形状（如 `finish_reason` 为对象）两者可能分歧，
   未构造对照片段，OPEN。
3. **深扫 512 节点上限**：TS 深扫无显式上限（受其 JSON 读取递归约束）；Rust 设
   512 节点预算防病态 payload，超限截断可能漏掉深层 `code` —— 已测
   `deep_scan_stops_at_node_cap`，行为差异记录在案。
4. **compact context 字段跳过**：请求体构造跳过 TS 侧 compact 上下文的若干部
   字段（源码读取面标记为不回灌）；若上游引入新 context 字段需同步，OPEN。
5. **不做 signature 修复**：anthropic reasoning 往返 signature 缺席时序列化空串
   （对齐 `providerOptionsForReasoningBlock`），不尝试重算/修补签名。
6. **absent vs null 边界字段**：边界投影中缺席字段与显式 `null` 统一映射为
   缺席（`present` 标志表达 TS 的 `!== undefined` 语义）；对显式 `null` 的 wire
   与 TS `undefined` 检查存在理论分歧面，OPEN。
7. **Retry-After HTTP-date**：只解析秒数形式；HTTP-date 形式返回 None（回退指
   数退避）。TS 用 `Date.parse` 可解析两种形式 —— 已知差异，未在真实网关观测
   到 date 形式，OPEN。
8. **sleep 中途无取消探针**：退避 sleep 整段一次性等待；TS 在 timer 与取消竞争。
   取消最坏晚一个退避周期生效（failure→sleep 之间已有探针），行为等价但时延略
   差，OPEN。
9. **EOF 深扫被 feed 预占**：见上文 psf 节 —— 结果等价（同源信号、同一分类），
   EOF 路径仅作防御网，不是行为分歧。
10. **宿主接线未通**：crate 目前以库形态交付（`lib` only），`execute()` 尚未被
    Tauri host / RPC 面调用；live provider 语料与 `generateText` 面未验证，
    OPEN（S6 验收项）。

## 验证

```sh
cargo fmt   --manifest-path native/Cargo.toml -p zcode-provider
cargo clippy --manifest-path native/Cargo.toml -p zcode-provider --all-targets -- -D warnings
cargo test   --manifest-path native/Cargo.toml -p zcode-provider
```

最近一次实测：fmt check 0 / clippy `-D warnings` 0 / **46 通过 0 失败**（20 单元

- 18 `tests/executor_stream.rs` + 8 `tests/frame_pipeline.rs`）。覆盖：自然流、
  500 重试（sleep [8]）、400 终态（`InvalidModelRequest`）、预算耗尽（[8,17]）、
  open 网络失败重试、idle 帧前重试 / 帧后终态保事件、preserve 已提交块终态、
  off-peak queued 冻结与 ticket 过期（`off-peak-ticket-expired`）、空完成 hold 重
  试与预算自然返回、帧 error 3105 queued、psf 终态业务、非 SSE 2xx JSON 业务、
  open 前取消（0 次 open）、failure→退避间取消探针、anthropic 边界投影顺序与
  `present` 标志、ping 不提交、零输出 hold、深扫 psf 与节点上限、浅层
  message-only 不触发。
