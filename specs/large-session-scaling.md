# 大会话性能：流式逐帧成本与会话长度的解耦

状态：已实现（随 `perf/large-session-scaling` 提交）
日期：2026-09-21

## 背景与问题

会话/线程变大后，桌面与 Web 工作台出现流式卡顿、滚动迟滞。追踪结论：**协议本身健康**——
v4 会话流是增量协议（`row.appended` / `row.upserted` / `row.delta` / `state.updated`），
CLI 侧 30ms flush 合并 delta，文本 delta 不落盘，SQLite part 写入是索引 upsert。
问题在增量周边：逐帧（每个 30ms flush）存在多处与**整个会话长度**成正比的扫描与重建，
且宽屏（≥864px）会把全部历史合并进客户端 `rows.window`，使这些成本从 O(60) 变为 O(会话)。

## 目标行为

1. 流式期间，每个 delta frame 的运行时与渲染器成本只与**本帧变化的行**相关，
   不与会话总行数、总文本量相关。
2. 未发生变化的 turn：其 render unit 对象标识保持稳定，React `memo` 边界可以命中。
3. 轮次目录（turn navigator）仍能在交互后拿到完整历史，但不再因"屏幕宽"就无条件补拉全量。

## 所有权与单一事实源

- **行投影的唯一所有者**是 CLI 的 `ProductProjection`（`apps/zcode-cli/.../zcode-protocol-v4/product-projection.ts`）。
  command row actions（edit/retry/fork/rewindFiles）是该投影对行状态的**派生物**，必须与
  row materialization 属于同一次事务；本 spec 只改变其**触发时机**与**比较方式**，不改变其语义。
- **render unit 的所有者**是 `packages/ui/src/v4/conversationTurnRenderUnits.ts` 的纯函数。
  新增的缓存是**同一纯函数的记忆化**，不引入第二份事实：输入（行对象标识 + nowMs + sessionPhase）
  不变则输出逐字节一致。
- **导航目录的所有者**仍是 `ConversationTimeline`（持有 `rows.window` 与补拉触发器）。
  本 spec 把"何时补拉"的判定从宽度改为**首次交互意图**。
- **rowContext 的所有者**是 `SessionPane`。行窗口派生的两张 join 表（workflow graph / workflow draft）
  改为解析器函数 + ref，消除逐帧新建对象导致的 memo 失效；表内容仍由行窗口一遍建成。

## 关键不变量

1. **语义保持**：`buildConversationTurnRenderUnits` 的记忆化对同一输入必须返回与无缓存实现
   逐字节一致的结果（含 `normalizeRenderUnitPosition` 的位置归一）。测试固化。
2. **actions 语义保持**：跳过 actions materialization 的唯一条件是"本帧 delta 全部为
   `row.delta`（纯文本追加）"。`row.delta` 不改变行状态、`fileChanges`、轮结构与
   `pendingInteractions`，因此重算结果必然与上次相同（全部命中"未变化"分支）。
   冷恢复 `completeHydrationReplay()` **不经过本门**：它直接调用
   `materializeCommandRowActions([])`（hydration 期间 `materializeActions=false`，actions
   统一延迟到收口物化）；门的"空 delta 集按可能变化处理"只是保守默认，与它无关。
   **必须有用例固化"跳过确实发生"**：纯输出断言抓不到它（旧实现这些帧同样产出空 delta
   集），需对 `materializeCommandRowActions` 入口计数，并对"删门"做变异验证。
3. **结构性相等替代序列化相等**：actions 比较改为按键集合浅比较（不列举键，
   `rowActionsSchema` 新增动作键时无需同步），与 `JSON.stringify` 比较在当前构造路径
   （键集合同源、值只能是字面量/短枚举）下等价。**两个方向都要有用例**：变化 → 必须下发
   upsert；内容相同 → 必须不下发（后者防"简化成引用相等"的回归——那会让每个结构帧重发
   整窗逐字节相同的 upsert 并推高 revision）。
4. **导航目录最终一致**：延迟补拉不改变目录最终内容；补拉失败走有界退避
   （250ms / 1000ms / terminal）。`stale`（取数期间窗口游标移动，或宿主暂无 lease）与
   `retryable-failure` 走同一张退避表：前者下次尝试即用上新游标，后者重试无意义但必须有界，
   否则无 lease 时裸重试会无限自旋；任何情况不得"置 idle 后不重跑"——那会静默放弃用户
   已表达的目录意图。目录意图 = rail 的首次 hover/focus **或会话内查找词非空**（查找命中
   的轮次可能尚未加载进窗口，与 rail 交互同等对待）。
5. **rowContext 身份稳定**：行窗口派生的两张 join 表改为解析器 + ref 后，rowContext 在
   纯文本帧必须保持同一对象标识；表内容仍由行窗口一遍建成，不引入第二份事实。
6. **补拉/rewind 触发器不丢**：解析器身份稳定不能顺带丢掉"窗口变化"触发器。
   `rowsWindowRevision`（窗口首行 rowId）只在补拉前插 / rewind 时变化，必须进入轮尾
   digest 的 memo 依赖——否则前插进来的 CreateWorkflow 行永远联不上更早轮的
   ResumeWorkflowRun 卡。

## 失败语义

- 记忆化缓存命中失败（输入变化）时退化为原有全量重建，不存在错误路径。
- 补拉退避有界：250ms / 1000ms 之后 terminal；`stale` 与可重试失败共用该表（见不变量 4）。
- 记忆化依赖协议 apply 的结构共享（未触及的行保持对象标识）：若未来出现原地改写行的
  producer，行标识指纹会失效并退化为重建（正确性不受影响，仅失去加速）。
- **跨轮 draft superseded 的 append 方向（已知残留，显式接受）**：同一血缘的新
  CreateWorkflow 行落在**更晚的轮次**时，更早那张编译失败卡的 superseded 指示灯不会即时
  翻转——该行属于未触及轮次，memo 命中、行标识不变，而 `rowsWindowRevision` 只在
  前插/rewind 时变化（append 不动首行）。影响面是 8px 状态灯的延迟翻转（非数据错误），
  且在下次 rowContext 变化（新 run 注册、补拉、主题/logEpoch 变化）或虚拟列表重挂时自愈。
  根因修复是让 supersession 成为行级事实（CLI 投影在追加新稿时 upsert 被取代的旧行），
  属于投影行为变更，需独立 spec 与测试，不在本性能 PR 范围内。

## 迁移边界

- 无持久化格式变更、无协议变更、无 wire 兼容性影响（delta 集合不变，仅产生时机不变：
  纯文本帧不再产生空转的 actions 物化，且原本这些帧的物化结果就是空 delta 集）。
- 渲染侧无持久化影响；`ConversationRowRenderContext` 的两个字段由 Map 改为解析器，
  仅影响本仓 UI 内部消费方（`ConversationTurnGroup` 的 digest 联接与 `ConversationRowView`
  的 draft 查询两处，外加 context 接口声明本身）。

## 后续（不在本 PR）

- `rowContext` 中其余逐帧变化项与 `ConversationTurnRow` 的 memo 化。
- `session/read` 快照路径的多趟全量扫描与整快照 sha256（`zcodeTaskServiceAdapter`）。
- `ORDER BY sequence is null, ...` 无法走序列索引导致的整会话排序（session-store 读取）。
- `messageLimit` 下推：`limitMessages` 在整会话读取之后才截断，存在性探测
  （`messageLimit: 1`）为丢弃的行付全量 SQL 与逐 part JSON.parse。未随本 PR 实现，
  因为 `projectActiveSessionMessages` 的 rewind 投影（`applyRewindBranch`）按 messageId
  在全表定位 target/created/kept，**有界读取与全量读取在 rewind 会话上不可证等价**
  （id 落在窗外时投影结果不同）。正确做法是先给投影证明后缀稳定性，或为 meta/存在性
  探测增加不读消息的协议入口，再下推 limit。

## 显式延后与理由（2026-09-21 评审记录）

- **首次 hover 的加载反馈**：补拉进行中 rail 只有 `aria-busy`，没有视觉占位。rail 是
  36px 窄条，加骨架条属于视觉设计决策（需过 DESIGN.md 评审），且补拉通常一次完成；
  本 PR 只补 `aria-busy` 语义，视觉反馈留给 UI 迭代。
- **交互接线的 DOM 级测试**：`onPointerEnter/onFocus → onDirectoryRequest → 门` 的接线与
  rowContext 身份不变量都没有函数级用例（rowContext 是 React 上下文，其"身份在纯文本帧
  不变"目前只由依赖推导与单测前提——unit 标识稳定——支撑）；仓库当前无 jsdom / 组件渲染
  测试设施，引入新测试框架超出本 PR 范围。接入线是 3 行事件转发，已由代码评审确认。
- **`conversationRowActionsEqual` 的边界单测**：函数为模块私有，不为其导出测试钩子；
  等价性由两组投影级用例双向锁定——"新一轮撤销旧入口"（变化方向）与"结构帧上内容未变的
  actions 不得产生 upsert"（相等方向，防退化成引用相等）。
- **导航目录草稿缓存与 render-unit 缓存淘汰的删除型变异**：两者都是纯性能机制，删除后
  公开输出值不变（前者草稿是模块内 WeakMap，后者只在超 2048 轮次后才影响淘汰顺序），
  因此没有能抓住"删除缓存"的断言；锁定它们的是构造本身（键 = unit 标识 + optionsKey），
  以及 render-unit 侧可断言的数组/对象标识稳定性用例。
