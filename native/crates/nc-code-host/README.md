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

启动：`zcode-host --workspace <path> --data-dir <path> [--provider-config <file>]
[--full-access]`。

## 本 increment 的能力面（与 TS host 的关系）

已实现：`v4/connection/flow`、`v4/conversation/subscribe|resync|unsubscribe`、
`v4/conversation/rowsRange`、`v4/conversation/fileChanges`、
`v4/conversation/fileRewindPreview`（ENTRY 061：只读查询面，读 turnHeader + 文件台账，
带 epoch/revision 陈旧读防护）、`v4/conversation/usage`（ENTRY 062：会话累计 token +
模型请求/失败计数）、`v4/command`（`createSession`、`sendText`、`stop`、
`compact`、`renameSession`、`deleteSession`、`setAutoDrain`、`setFollowupMode`、
`switchModelConfig`、`switchCollaborationMode`、`setAssistantFeedback`、
`sendQueuedNow`、`editQueueItem`、`reorderQueueItem`、`deleteQueueItem`、
`resolveInteraction`、`snoozeInteractionAutoResolution`、
`retryTurn`、`editUserQuery`（ENTRY 050：分支截断重放）、
`applyFileRewind`（ENTRY 051：工作区文件回滚，不动聊天历史）、
`forkAssistant`、`createSelectionSideSession`（ENTRY 052：子会话）、
`sendGoalCommand`、`pauseGoal`、`resumeGoal`（ENTRY 053：目标态持久化 +
裁判驱动的自动续跑、pause/resume 与 stop-pauses-goal）、
`respondWorkspaceHookReview`、`toggleWorkspaceHookReviewItem`、
`revokeWorkspaceHookTrust`、`requestWorkspaceHookReview`（ENTRY 054–057：
workspace hook 评审/信任——发现、摘要、台账、评审 flow 与命令面）；
其余命令 fail-closed `fault.command.unsupported:*`（清单与缺失子系统在
`commands::UNROUTED_COMMANDS`，同步进 ack `message`，并由
`commands::tests::command_inventory_is_the_exact_complement_of_the_v4_surface`
钉成 v4 命令全集的精确补集）且 `availability` 如实标为不允许）、
`v4/commands/query`（命令幂等查询面）、
`v4/conversation/frame` 通知、两套 delivery
profile、冷恢复 resume。**多回合 agent 循环**：模型回合与工具回合交替直到模型
给出无工具调用的终态文本；工具集 Bash/Read/Write/Edit/Glob/Grep（契约含
readOnly/destructive/concurrentSafe/sideEffectScope/timeout/maxOutput）；
toolCall 行投影（running → success/error/cancelled）；permission 交互（非只读工具在
有订阅者时打开 pendingInteraction，allowOnce/deny/fullAccess 三态，headless 或取消
时 fail-closed）；thinking/reasoning 行投影
（streaming → complete/interrupted，带 durationMs）；帧序保证（turn 期间 eager 投递，
response 与帧共用会话唯一 FIFO）；context 装配（系统提示 +
AGENTS.md/CLAUDE.md 发现 + 环境事实）；机械 microcompact（阈值 120k 字符，压最早
的工具结果）。**跨 turn 会话记忆**（ENTRY 027）：模型历史由 `Session` 独占并落进
`transcript.jsonl`（`t:"history"`），turn 只追加不重建——第 N 轮的请求体带着前 N-1
轮的全部问答，重启后回放同一份历史。其余 v4 方法返回 `-32601`（不静默吞掉）。

## 原生 Browser 工具（`BrowserControlPort`）

`Browser` 是 Rust host 的浏览器自动化工具。Electron 侧由 browser-use 插件的
node_repl `agent.browsers` 提供；Rust host 没有 JS 插件运行时，因此把同构的
`BrowserCommand` JSON 直接暴露在模型工具面：

- 命令：`navigate` / `back` / `forward` / `reload` / `getState` / `snapshot` /
  `evaluate` / `screenshot` / `click`（`ref` 或 `x,y`）/ `fill` / `type` / `press` /
  `scroll` / `waitFor` / `elementInfo` / `listUserTabs` / `activateTab` /
  `claimTab` / `newTab` / `finalize` / `finalizeTabs` / `markDeliverable` /
  `markHandoff` / `nameSession` / `closeSession` / `recordingStart` /
  `recordingStatus` / `recordingCancel` / `playwright` /
  `playwrightWaitForTimeout` / `close` / `list`；
- 只读命令（`getState` / `snapshot` / `elementInfo` / `listUserTabs` / `list`）
  不触发权限询问，其余命令需要审批；
- host 只依赖注入的 `BrowserControlPort`；未注入时工具以 `browser_unavailable`
  fail-closed，绝不伪造页面结果；
- Tauri 壳注入 `ShellBrowserPort`：Windows 通过 WebView2
  `CallDevToolsProtocolMethod` 驱动当前可见 child webview；macOS/Linux 返回
  `capability_unsupported`；
- `snapshot` 把 ref 写入 `data-zcode-ref`，后续 `click`/`fill` 按 ref 解析。

## 原生 Computer 工具（`ComputerControlPort`）

`Computer` 是 Rust host 的桌面自动化（computer use）工具，对应 Electron 侧由
外部 `computer-use` 插件 + native Helper 提供的能力：

- 命令：`listApps` / `getState` / `screenshot` / `activate` / `move` / `click` /
  `scroll` / `type` / `press` / `drag` / `wait`；
- 只读命令（`listApps` / `getState` / `screenshot` / `wait`）不触发权限询问，
  其余命令需要审批；
- host 在触碰 port 之前校验必填字段（`activate` 需 `windowId|pid`、`click`/
  `move` 需坐标、`drag` 需四角、`type` 需非空文本、`press` 需 key、
  `scroll` 需 delta、`wait` ≤ 30s）；
- host 只依赖注入的 `ComputerControlPort`；未注入时以 `computer_unavailable`
  fail-closed；
- Tauri 壳注入 `ShellComputerControl`：Windows 使用 `EnumWindows` /
  `GetForegroundWindow` / `GetCursorPos` / GDI `BitBlt` PNG / `SendInput`；
  其他平台返回 `capability_unsupported`（macOS/Linux 原生后端尚未实现：本机无
  Apple SDK / X11 display，未验证的输入注入不发布）；
- `screenshot` 支持 `windowId|pid`、显式区域或整块虚拟屏幕，返回
  `{base64, mimeType:"image/png", width, height, bytes}`。

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
19. `renameSession`/`deleteSession` 直接生效（无 turn 副作用的会话级控制）：
    rename 更新 title 并投影 `state.updated { meta }` + 落盘 meta.json（ENTRY 041）；
    delete 语义是 **close**——只从内存注册表移除，**不删 transcript**（ENTRY 040，
    对齐 TS「message 库无删除 API、历史仍在库里」）。
20. compact 有两层且都作用在**会话级**历史上（ENTRY 027 之前只作用于单轮）：
    机械 microcompact 阈值 120k 字符，LLM 摘要压缩在历史 > 24 条消息时触发。
    两者的压缩结果都经 `Session::replace_history` 落成一条 `t:"historyCompacted"`
    事件；`compact` 命令已接线（ENTRY 029）：无 provider / 历史不可写时 typed
    fault，成功时 settle accepted 并投影 `{"type":"compact"}`。
21. **分支裁剪只支持"最后一轮"**。`editUserQuery` / `retryTurn`（ENTRY 050）会
    同时截行集与模型历史：`t:"rowsTruncated"`（与 `row.removed{fromRowId}` 同语义）
    + 一条 `t:"historyCompacted"` 保留前缀，回放与内存走同一条裁剪规则。
    `forkAssistant` 仍是 fail-closed：真正的分支树（同会话多分支并存、按 entityId
    解析稳定目标）需要每轮带分支身份，本 host 的历史仍是一条线性追加。
22. **工具回合的 assistant 消息只带 `tool_calls`、不带正文**；TS 的
    `message-history.ts:179` `addAssistant` 会把 `content` 与 `toolCalls` 一起记录。
    本 host 只把**终态**回答作为 assistant 消息入史（历史里不会出现「有正文却无
    对应 tool_use」的悬空回合）。代价是：模型在工具回合说过的话不会出现在下一轮
    历史里，只能从 tool_result 推断。这条差异是有意的（保持 tool_use/tool_result
    配对干净），不是遗漏。
23. **历史写失败会让 turn 以 typed fault 结束**（`fault.turn.historyNotPersisted`）：
    本轮用户输入或回合增量无法落进 transcript 时，不发起（或提前结束）模型调用、
    不假装成功。方向是 fail-closed——宁可这一轮失败，也不让模型去回答一个从未收到的
    问题、同时 UI 上显示着一个对不上的回答。`append_history` / `replace_history`
    因此都返回 `Result` 而不是 `()`，两条写路径共用同一份失败记账。
    该 fault 作为**独立字段**进入 `session_error_info`，不经 `AdapterError` 的
    message（否则会被 `FAULT_MODEL_FAILED` 覆盖，永远到不了 `reasonCode`）。
24. **历史锁中毒 = 本轮不跑**：`history_snapshot()` 返回 `Result`，中毒时
    `StoreError::HistoryPoisoned`。返回空 `Vec` 是 fail-**open**——那会让 turn 拿零
    上下文去请求模型，正是这条防线要防的事。
25. **transcript 回放是容错的，但降级一定会报出来**：坏行、未知 `t` 标签、以及
    **整份文件缺失/不可读**都走同一条通路——跳过并记入 `Transcript::skipped_lines`，
    `resume_sessions` 把降级写 stderr 并指名会话。旧行为是首行解析失败即整份
    transcript 报废、调用方 `unwrap_or_default()`：在它只存展示行时只是 UI 少几行，
    ENTRY 027 之后那会静默清空模型记忆。
26. live provider：**两种 wire 形状均已对真实 endpoint 实测通过**
    （2026-09-22）：openai-compatible（stream + tools，ENTRY 008）与
    anthropic（`/v1/messages` + Bearer + 非流式 + tool_use，ENTRY 009）。
    测试用凭据由用户提供，只进临时目录、不随代码提交（repo 规则：凭据不得进
    日志/提交）。仍 OPEN：runner-debug 录制的 live golden-request 语料、
    `generateText` parity、Linux/macOS 运行时。
27. `ProviderConfig` 的 `authStyle`/`stream` 覆盖：网关鉴权样式与流式能力因
    网关而异（该网关对两种形状都只认 Bearer，且 stream 请求被翻译成 OpenAI
    chunk 形状），配置显式声明即可，不必改代码。
28. full-access 是启动参数 `--full-access`（host 级），非 per-session grant。
29. **文件台账只覆盖文本写入**：`FileLedger` 记的是"写工具改之前/之后的文本"。
    二进制或读不出的文件（`read_to_string` 失败）不会有一条可靠的"改动前"记录，
    rewind 会把它判成 `unsupported_checkpoint` 并**整笔拒绝**——不做二进制回滚，
    也不猜一个内容写回去。Bash 改的文件压根不进台账，因此只能以
    `external_modified`（当前内容与我们写出的那版不一致）出现并被拒绝。
30. **edit/retry 不抢占在途 turn**。TS 的 `editUserQuery` 在有 activeAbortController
    时先 `preemptActiveTurnAndWait` 再截断；本 host 的 `retryTurn`/`editUserQuery`
    经 session inbox **串行 admission**，busy 时排在当前 turn 之后执行。差异是安全
    的（截断永远发生在已收口的状态上，latest-only 守卫不会与在途投影竞争），但桌面
    「立即打断再改」的手感不同；要对齐需要把这两条命令改成 `stop` 那样的前置控制位点。
31. **`attachments` 不渲染**：`sendText` / `editUserQuery` 的 `attachments`
    （AttachmentRef → TurnAttachment）本 host 不解析。空正文 + 仅附件的输入按
    `proto.invalidPayload` 拒绝（不伪造一条没有内容的用户消息），正文非空时附件被
    忽略并在 stderr 出声记一行。真正的附件通路（图像/文件进上下文）未接线。
32. **行定位的实体校验按行类型成立**：每一行都带 `entityId`（turnHeader 用
    `turn:{turnId}`、userInput 用 `ui:{commandId}`、assistantText 用
    `assistantResponseId`、toolCall 用 `toolCallId`、reasoning 用
    `reasoning:{responseId}`），`conversationRowTarget.entityId` 与之不符就按
    `fault.command.executionFailed` 拒绝（TS 的 `V4RowTranslationError` 同档）。
    差异在解析深度：TS 的稳定 fork 解析器另有 transcript 级的 message anchor
    （`orderedMessageIds` / `boundaryMessageId`），本 host 用行集 + 每轮的历史锚点
    固定同一个边界，因此**不产出** per-message anchor，也不会在 anchor 缺失时
    惰性补写（TS 的 legacy fallback）——那种数据在本 host 下直接
    `guard.forkTargetAmbiguous` 拒绝。
33. **子会话不继承文件台账**：`forkAssistant` 复制父会话到分叉点为止的行与模型历史，
    并剥掉复制行上的 `actions.canRewindFiles`；父会话在共享 workspace 里写的文件仍由
    父会话回滚。TS 的这条路径同样是 "conversation-only fork（workspace 不动）"。
34. **goal 续跑与 admitted 命令共用同一把会话 gate**：`sendGoalCommand` / `resumeGoal`
    的 admission ack 立即返回，续跑轮在该命令的执行任务里串行跑；每轮之间只要
    有排队输入、或有命令在 inbox 等 gate、或目标不再 active、或会话处于 plan 模式，
    循环就让位（TS `yieldToPendingCommands` 的等价）。`pauseGoal` 是**控制位点**
    （与 stop 同理由：排在在途 turn 后面会死锁，且它必须能打断在途裁判），
    CAS 规则与 inbox 一致（缺 baseRevision → `proto.missingBaseRevision`、
    不匹配 → `proto.staleRevision`），但由 dispatcher 显式校验。
35. **`budget_limited` 没有输入源**：v4 的 `sendGoalCommand` payload 不带 token budget，
    所以 durable 状态只可能是 active/paused/complete；投影侧的
    `verifying / verified / notSatisfied / failed` 全部由裁判生命周期驱动，从不落库。
    裁判请求失败（failed_closed）按 fail-open 停止续跑但**不**改 durable 状态，
    投影显示 `failed`（与 TS 相同）。
36. **目标落 `meta.json`**：`goal` 是会话元数据的一部分（重启后原样恢复）。
    在途裁判（`verification_in_flight`）与 `activeRunStartedAtMs` 是运行态，不落盘——
    重启后不可能还有在途请求，持久化它等于伪造"正在验证"。
37. **goalSet 不产 timeline row**（TS 同一裁决：目标展示归 goal 面板，不污染轮分组）；
    `goalVerify` marker 走 `timelineMarker` 行、`lane = turnTailBoundary`，按
    `targetId_iteration` 生命周期先建 running 行、终态 upsert 同一行，
    `verifications[].anchorRowId` 指向该行。
38. **workspace hook 子系统分步移植中（ENTRY 054–055）**：领域层已落地并有单测——
    配置发现/解析（`zcode.json` + `.zcode/config.json`，向上走到 worktree 根）、
    声明与 bundle 摘要（与 TS 逐字节一致，用 Node 参考摘要钉死）、信任状态机、
    严格信任台账（未知字段/重复键/坏摘要 = 文件无效 → fail-closed）、台账落盘
    （原子 rename、损坏文件移开存证且 corrupt 判定粘滞到成功写入、按 key 合并
    不丢其它 workspace 的记录）与评审 flow 状态机（generation/supersede/
    deadline/逐条校验）。
    准入层也已落地：评估（策略/撤销/台账三态）、`workspaceHookAdmission` 投影、
    评审交互 payload（展示命令经 `sanitizeHookDisplayText` 打码，参考值由真实
    `display-metadata.ts` 生成）、request/respond/toggle/revoke/expire/supersede
    与 `.zcode/config.json` 的 `enabled` 原子写回（声明摘要守卫）。
    与 TS 的记录在案差异：台账没有跨进程文件锁（本 host 是数据目录唯一写者）；
    评审 target 只校验 sessionId/workspaceIdentity/bundleDigest（host 没有
    taskId/runId/remoteSessionId 三层任务身份，payload 里用 sessionId 填充）；
    评审请求里的 `displayCommand` 与 TS 一样是**未打码**的原始命令（打码用在
    descriptor/hookInvocation 一侧）。
    **评审与信任面已接线**（ENTRY 057）：会话打开即发现工作区 hook，快照投影
    `workspaceHookAdmission` + 一条 `pendingInteraction(kind=workspaceHookReview)`；
    四条命令真的读写台账/配置，deadline 由每会话 ticker 收口，部分授权/撤销/开关
    之后评审换 generation 重开（旧代应答一律
    `workspace_hooks_review_superseded`）。
    执行层（ENTRY 058–059）已接进 turn 循环：`SessionStart`（打开/恢复时，上下文注入
    本会话第一条用户消息）、`UserPromptSubmit`（block/deny → 本轮不调模型）、
    `PreToolUse`（deny → 工具**不执行**，allow → 跳过 permission 询问，updatedInput
    覆盖参数，additionalContext 回灌工具结果）、`PostToolUse` /
    `PostToolUseFailure`（上下文随结果回灌）；每次事件投影一条 `hookInvocation` 行
    （didExecute/outcome/时长/命令展示）。执行器语义见 ENTRY 058（超时杀进程树、
    输出上限、异步 fire-and-forget、退出码 0/2/其它）。
    七个事件全部接线（ENTRY 060）：`Stop` 的 `continue:true` 在**同一 product turn
    内**续跑（与 TS `finishModelStepWithoutToolCalls` 同位置），条件同为
    `stopShouldContinue && additionalContext 非空 && 续跑次数 < 3`，上下文带
    `Stop hook additional context:` 标题注入历史，`stop_hook_active` 标记续跑；
    `PermissionRequest` 的 `decision.behavior` 直接决定 allow/deny，未表态才进 broker。
    hook 是同步外部进程，turn 循环里的调用全部走 `spawn_blocking`（60s 超时不会占住
    tokio worker）；matcher 正则按字符串缓存（每个 tool call 都要匹配）。
    差异：`hookExecutionProjection.outcome` 对"JSON 里 deny"也标成 blocked（TS 的
    projection 用 didExecute+status 表达，取值等价）。
39. **side session 的限制命令集未实现**：TS 的 `selection_side_chat` 禁止
    `sendGoalCommand`/`pauseGoal`/`resumeGoal`/`editUserQuery`/`retryTurn`/
    `forkAssistant`/`discardSharedContext`（`V4SelectionSideChatRestrictedCommandError`）。
    本 host 没有 taskType 标记，因此不对副屏会话做这层拒绝——记在这里而不是假装有。
40. **`usage.inputBaselineBySource` 恒为空对象**：本 host 不按 querySource 记上下文基线
    （那是 CLI 侧 session 库里 `input_baseline` 的事实），空对象表示"没有基线"而不是
    "有基线但丢了"。`modelRequestCount` 按 HTTP 请求计（executor 内部 attempt 重试算
    同一次），`totalTokens = inputTokens + outputTokens`。
41. **workflow 引擎边界是已记录的产品决策**（`specs/workflow-capability-boundary.md`）：
    host 是会话运行时（turn/tool/hooks/goal/文件/队列/读模型），workflow 引擎留在 TS
    侧——它依赖 TS compiler API，重写等于换产品格式。4 条命令与 7 个 run 读取方法
    因此保持 fail-closed 并逐条登记；若未来上 sidecar，契约是"host 拥有 runId ≡
    workId、幂等代理命令、只从 journal 事件投影运行态"。
    **v4 方法清单的剩余缺口是"子系统/产品决策"，不是待办遗漏**：
    `rpc::V4_METHODS_ALL` 列出协议全部 33 个方法，已接线 11 个（10 请求 +
    `v4/conversation/frame` 通知），`V4_METHODS_UNIMPLEMENTED` 的 22 条每条写明缺什么（附件事务、后台 bash 会话、plan 工具、workflow run 引擎、usage 聚合库、
    telemetry、controller 别名），并有与命令清单同款的精确补集测试。
    其中 **dynamic workflow（4 条命令 + 8 个 run 读取方法）需要一个依赖 TS compiler API
    的引擎**（约 2 万行）——是"是否用 Rust 重写引擎"的产品决策（同 `RUST-PORT-MATRIX.md`
    里 BrowserView 的定位），本 host 在此之前不接线、不伪造运行态。
    `discardSharedContext` 同理：本 host 没有 shared-context 生产者，接一条只会拒绝的
    命令等于假面，故保留在 `UNROUTED_COMMANDS`。

## 验证

```sh
cargo test -p zcode-host            # 单元 + e2e stdio + 工具循环 e2e
cargo clippy -p zcode-host --all-targets -- -D warnings
cargo fmt -p zcode-host -- --check
```

集成测试全部驱动**真实二进制**（`tests/common` 是共享夹具：stdio client + 本地脚本化
SSE provider；产品代码里没有测试替身）：

- `e2e_stdio`：create → subscribe → sendText → fail-closed 错误投影 → resync
  （recovery snapshot 行集一致）→ 重启进程 resume（行与幂等 ack 不丢）。
- `tool_loop_e2e`：full-access 真写文件 + tool_result 回灌；denied 路径以 typed 文本
  回灌且文件不落盘；`permission_allow_once_executes_tool_without_full_access` 走真实
  broker；`queued_input_drains_after_active_turn` 验证 turn 收口后的队列提升；
  reasoning 行（thinking 块）投影与 durationMs。
- `session_memory_e2e`：连续两轮的真实请求体断言 + 重启后第三轮带满前两轮内容。
- `commands_wired_e2e` / `commands_query_e2e`：配置命令的 `state.updated` 投影、
  meta 跨重启、rename 的 meta delta、stop 的迟到护栏、deleteSession 保留历史；
  幂等查询面的 global/session 桶与 `unknown` 边界。
- `queue_mutations_e2e`：CAS 拒绝旧 revision、edit/reorder/delete 真生效、
  sendQueuedNow 缺失项拒绝。
- `branch_rerun_e2e`：retryTurn / editUserQuery 的 `row.removed`、editRerun origin、
  **模型历史切回轮起点**（请求体只含那一条 user 消息）与三个守卫；顺带钉住
  revision 与权威投影同步前移（否则 CAS 命令全部 stale-by-one）。
- `file_rewind_e2e`：真实 Write 工具落盘 → `v4/conversation/fileChanges` 返回
  文件/增删行/`patches`（单 hunk unified diff）与 `state:"active"`，
  `fileRewindPreview` 给出 `canApply`/`safeFiles(delete)`，陈旧 `baseLogEpoch`
  被 `proto.staleLogEpoch` 拒绝 → turnHeader 带 `fileChanges{state:"active"}`
  与 `actions.canRewindFiles` → 用户手删文件时 preview 判 `external_modified` 并**零副作用**
  拒绝 → 内容回到 host 写出的那一版时 applied（本轮新建的文件被删除）、行标成 `reverted`
  → 第二次 rewind `guard.actionUnavailable`。
- `child_sessions_e2e`：fork 出来的 child 只看到分叉点为止的行/历史、复制行不带
  `canRewindFiles`、父会话行集与 revision 一字不变；side session 的 `firstInput` 落进
  **child 自己的命令账本**且父队列保持为空；目标不是轮尾 assistant 时 `guard.forkTargetNotStable`。
- `goal_e2e`：`/goal` → 续跑轮（`turnHeader.origin=goalContinuation`、不产生
  userInput 行）→ 裁判（无 tools、notSatisfied + nextAction）→ 第二轮 → pass →
  `goal verified`；pause/resume 往返；stop 打断在途裁判 → `paused` 且不记伪结论；
  用户轮之后的 verify-first 续跑；重启后目标身份/裁判账本原样恢复；
  fork 继承父目标（身份/结论/裁判账本）；空目标/plan 互斥在写入前就拒绝。
- `hook_review_e2e`：真实工作区配置 → 启动即生成待审横幅与评审交互 → 授权写进
  `<data-dir>/security/` 台账且横幅消失 → 撤销删除记录并换代重开 → 开关真的写回
  `.zcode/config.json`（无关键保留）并因 bundle 变化重开 → 过期 bundle 的按需评审
  被拒 → 重启后信任仍在。
- `hook_execution_e2e`：受信任的 PreToolUse hook（真实 node 进程）返回 deny →
  工具不执行（文件未落盘）、`tool.permissionDenied` 与 hook 上下文回灌模型、
  `hookInvocation` 行 outcome=blocked；重启后 SessionStart hook 的上下文注入下一条
  用户消息。
- `late_subscribe_e2e`：turn 已收口、订阅者列表为空时的第一个 subscribe 仍拿到
  response + initial snapshot（带超时读取，阻塞即失败而不是挂住套件）。
- `history_failclosed_e2e` / `tool_async_isolation` / `nc_tools_integration` /
  `mcp_e2e`：历史写失败的 typed fault、长工具不阻塞 tokio 运行时、外部工具面接线、
  stdio MCP server 往返。

`mcp_e2e` / `mcp_tools` 的用例需要 PATH 上有 `node`（用 `node -e` 起脚本化 MCP
server）；缺 node 的机器上这三条会失败，其余全绿。这是环境依赖，不是产品路径的
mock。

基准：`cargo bench -p zcode-host --bench transcript_replay`（冷恢复回放、分支截断后
回放、append/upsert/snapshot 投影）、
`cargo bench -p zcode-host --bench file_rewind`（台账归并、200 路径的分桶预览、
台账汇总查询）、`cargo bench -p zcode-host --bench goal_domain`（裁判落账与保留上限、
满账本投影、4000 字符提示词、裁判文本解析）与
`cargo bench -p zcode-provider --bench sse_streaming`。

启动：`zcode-host --workspace <path> --data-dir <path> [--provider-config <file>]
[--full-access]`。
