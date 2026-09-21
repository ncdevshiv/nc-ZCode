/**
 * 大会话性能回归测试（spec: specs/large-session-scaling.md）。
 *
 * 覆盖三处修复的可观察行为：
 * 1. render unit 记忆化：未变化的 turn 复用同一 unit 对象（memo 边界命中的前提），
 *    且缓存构建与无缓存构建逐字节一致（语义保持）。
 * 2. 轮次目录项草稿按 unit 标识复用，options 变化作废缓存。
 * 3. 目录补拉以用户意图为前提，宽屏不再自动补全量历史。
 *
 * 运行方式（仓库无统一 test 脚本，按包内 node:test 惯例）：
 *   cd packages/ui && node --import tsx --test test/largeSessionScaling.test.ts
 */
import assert from "node:assert/strict";
import test from "node:test";
import type { ConversationRow } from "../src/v4/conversationTurnRenderUnits.js";
import { buildConversationTurnRenderUnits } from "../src/v4/conversationTurnRenderUnits.js";
import { createConversationTurnRenderUnitsCache } from "../src/v4/conversationTurnRenderUnitsCache.js";
import { buildConversationTurnNavigatorItems } from "../src/v4/conversationTurnNavigatorHelpers.js";
import {
  resolveConversationTurnNavigatorHydrationRetryDelayMs,
  shouldHydrateConversationTurnNavigatorDirectory,
} from "../src/v4/conversationTurnNavigatorHelpers.js";

// 行夹具只跑纯函数，不需要 zod 校验；字段取投影真实形状，缺省值集中在助手函数里。
function turnHeader(
  rowId: number,
  turnId: string,
  state: "running" | "completedSuccess" = "completedSuccess",
): ConversationRow {
  return {
    rowId,
    turnId,
    kind: "turnHeader",
    origin: "userInput",
    state,
    startedAt: 1_000,
    createdAt: 1_000,
    createdAtSeq: rowId,
  } as ConversationRow;
}

function userInput(rowId: number, turnId: string, text: string): ConversationRow {
  return {
    rowId,
    turnId,
    kind: "userInput",
    text,
    origin: "realUser",
    createdAt: 1_000,
    createdAtSeq: rowId,
  } as ConversationRow;
}

function assistantText(
  rowId: number,
  turnId: string,
  text: string,
  state: "streaming" | "complete" = "complete",
): ConversationRow {
  return {
    rowId,
    turnId,
    kind: "assistantText",
    text,
    state,
    createdAt: 1_000,
    createdAtSeq: rowId,
  } as ConversationRow;
}

/** 两个完成态轮次 + 一个可切为运行态的轮次。 */
function twoTurnRows(): ConversationRow[] {
  return [
    turnHeader(1, "t1"),
    userInput(2, "t1", "第一个问题"),
    assistantText(3, "t1", "第一个回答"),
    turnHeader(4, "t2"),
    userInput(5, "t2", "第二个问题"),
    assistantText(6, "t2", "第二个回答"),
  ];
}

const NAVIGATOR_OPTIONS = {
  assistantEmptyPreview: "（无回复）",
  assistantRunningPreview: "正在回复…",
  userFallbackPreview: "（用户输入）",
};

test("行未变化时 render unit 数组与每个 unit 对象都复用同一标识", () => {
  const rows = twoTurnRows();
  const cache = createConversationTurnRenderUnitsCache();
  const first = buildConversationTurnRenderUnits(rows, {}, cache);
  const second = buildConversationTurnRenderUnits(rows, {}, cache);

  // 数组标识稳定，下游 useMemo（live tail 拆分、目录项、查询行集合）才能命中。
  assert.equal(second, first);
  assert.equal(second.length, first.length);
  for (let index = 0; index < first.length; index += 1) {
    assert.equal(second[index], first[index], `unit ${index} 应复用同一对象`);
  }
});

test("单个 turn 的文本增量只重物化该 turn，其余 turn 保持标识", () => {
  const rows = twoTurnRows();
  const cache = createConversationTurnRenderUnitsCache();
  const first = buildConversationTurnRenderUnits(rows, {}, cache);

  // 模拟 row.delta：只有被追加的行换对象，其余行引用不变。
  const streamed = [...rows.slice(0, 5), { ...rows[5]!, text: "第二个回答（续写）" }];
  const next = buildConversationTurnRenderUnits(streamed, {}, cache);

  assert.notEqual(next, first);
  assert.equal(next[0], first[0], "未触及的轮次必须复用 unit");
  assert.notEqual(next[1], first[1], "被追加的轮次必须重新物化");
});

test("缓存构建与无缓存构建逐字节一致（语义保持）", () => {
  const rows = twoTurnRows();
  const cache = createConversationTurnRenderUnitsCache();
  const cached = buildConversationTurnRenderUnits(rows, {}, cache);
  const plain = buildConversationTurnRenderUnits(rows, {});

  assert.deepStrictEqual(cached, plain);
});

test("追加新轮次只重物化原末尾轮次（isLastTurn 翻转）", () => {
  const rows = twoTurnRows();
  const cache = createConversationTurnRenderUnitsCache();
  const first = buildConversationTurnRenderUnits(rows, {}, cache);

  const withThirdTurn = [
    ...rows,
    turnHeader(7, "t3"),
    userInput(8, "t3", "第三个问题"),
    assistantText(9, "t3", "第三个回答"),
  ];
  const next = buildConversationTurnRenderUnits(withThirdTurn, {}, cache);

  assert.equal(next.length, 3);
  assert.equal(next[0], first[0], "更早的轮次不受位置变化影响");
  assert.notEqual(next[1], first[1], "原末尾轮次的 isLastTurn 翻转，必须重物化");
});

test("nowMs 只作废运行中轮的缓存，历史轮不因每秒 tick 重建", () => {
  const rows: ConversationRow[] = [
    turnHeader(1, "t1"),
    userInput(2, "t1", "第一个问题"),
    assistantText(3, "t1", "第一个回答"),
    turnHeader(4, "t2", "running"),
    userInput(5, "t2", "第二个问题"),
    assistantText(6, "t2", "第二个回答", "streaming"),
  ];
  const cache = createConversationTurnRenderUnitsCache();
  const atFirstTick = buildConversationTurnRenderUnits(rows, { nowMs: 1_000 }, cache);
  const atSecondTick = buildConversationTurnRenderUnits(rows, { nowMs: 2_000 }, cache);

  assert.equal(atSecondTick[0], atFirstTick[0], "已完成轮不消费 nowMs");
  assert.notEqual(atSecondTick[1], atFirstTick[1], "运行中轮的工时文案必须随时间推进");
});

test("sessionPhase 只作废缺 header 轮次的缓存", () => {
  // 有 header 的轮次不读 sessionPhase（resolveTurnRunning / isInterrupted /
  // forceOpenHistory 都以 header 为权威），相位变化不该重建历史轮。
  const withHeaders = twoTurnRows();
  const headerCache = createConversationTurnRenderUnitsCache();
  const live = buildConversationTurnRenderUnits(
    withHeaders,
    { sessionPhase: "running" },
    headerCache,
  );
  const interrupted = buildConversationTurnRenderUnits(
    withHeaders,
    { sessionPhase: "completedInterrupted" },
    headerCache,
  );
  assert.equal(interrupted[0], live[0], "有 header 的轮次不消费 sessionPhase");
  assert.equal(interrupted[1], live[1]);

  // 冷尾窗可能裁掉 turnHeader：这类轮的相位回退必须随 sessionPhase 重算。
  const headerless: ConversationRow[] = [
    userInput(2, "t1", "第一个问题"),
    assistantText(3, "t1", "第一个回答"),
  ];
  const headerlessCache = createConversationTurnRenderUnitsCache();
  const runningPhase = buildConversationTurnRenderUnits(
    headerless,
    { sessionPhase: "running" },
    headerlessCache,
  );
  const interruptedPhase = buildConversationTurnRenderUnits(
    headerless,
    { sessionPhase: "completedInterrupted" },
    headerlessCache,
  );
  assert.notEqual(interruptedPhase[0], runningPhase[0], "缺 header 的轮次必须随相位重建");
  assert.equal(
    interruptedPhase[0]!.assistantHistoryDefaultOpen,
    true,
    "中断态历史默认展开（异常终态兜底）",
  );
  assert.equal(runningPhase[0]!.assistantHistoryDefaultOpen, false);
  // workStatus 同样随相位重算：缓存键若漏掉 sessionPhase，这里会停留在 "completed"。
  assert.equal(interruptedPhase[0]!.workStatus?.state, "interrupted");
  assert.equal(runningPhase[0]!.workStatus?.state, "completed");
});

test("目录项：unitIndex 跟位置走，preview 随文本增量更新", () => {
  const rows = twoTurnRows();
  const cache = createConversationTurnRenderUnitsCache();
  const units = buildConversationTurnRenderUnits(rows, {}, cache);
  const items = buildConversationTurnNavigatorItems(units, NAVIGATOR_OPTIONS);

  assert.equal(items.length, 2);
  assert.equal(items[0]!.unitIndex, 0);
  assert.equal(items[1]!.unitIndex, 1);
  assert.match(items[0]!.userPreview, /第一个问题/);
  assert.match(items[0]!.assistantPreview, /第一个回答/);

  // 同一 units 再建一遍：草稿按 unit 标识复用，内容不变。
  assert.deepStrictEqual(buildConversationTurnNavigatorItems(units, NAVIGATOR_OPTIONS), items);

  // 文本增量后，该轮 preview 必须反映新文本，另一轮不受影响。
  const streamed = [...rows.slice(0, 5), { ...rows[5]!, text: "第二个回答（续写）" }];
  const streamedUnits = buildConversationTurnRenderUnits(streamed, {}, cache);
  const streamedItems = buildConversationTurnNavigatorItems(streamedUnits, NAVIGATOR_OPTIONS);
  assert.match(streamedItems[1]!.assistantPreview, /第二个回答（续写）/);
  assert.equal(streamedItems[0]!.userPreview, items[0]!.userPreview);

  // 位置子集：unitIndex 相对当前数组，不从缓存里带旧位置。
  const subset = buildConversationTurnNavigatorItems(streamedUnits.slice(1), NAVIGATOR_OPTIONS);
  assert.ok(subset.every((item) => item.unitIndex === 0));
});

test("目录项：options 变化作废草稿缓存", () => {
  const rows = twoTurnRows();
  const units = buildConversationTurnRenderUnits(rows, {});
  const first = buildConversationTurnNavigatorItems(units, NAVIGATOR_OPTIONS);
  const localized = buildConversationTurnNavigatorItems(units, {
    ...NAVIGATOR_OPTIONS,
    userFallbackPreview: "（用户查询）",
  });

  assert.equal(localized[0]!.userPreview, first[0]!.userPreview);
  // 用无正文轮验证 fallback 文案确实换了。
  const markerOnly = buildConversationTurnRenderUnits(
    [turnHeader(1, "t1"), userInput(2, "t1", "")],
    {},
  );
  const before = buildConversationTurnNavigatorItems(markerOnly, NAVIGATOR_OPTIONS);
  const after = buildConversationTurnNavigatorItems(markerOnly, {
    ...NAVIGATOR_OPTIONS,
    userFallbackPreview: "（用户查询）",
  });
  assert.equal(before[0]!.userPreview, "（用户输入）");
  assert.equal(after[0]!.userPreview, "（用户查询）");
});

test("目录补拉以用户意图为前提：宽屏不再自动补全量历史", () => {
  const base = {
    canLoadOlder: true,
    containerWidthPx: 1_200,
    hasLoadHandler: true,
    loadingOlder: false,
  };

  // 修复前：宽屏 + 可补拉即补全量，桌面用户一进会话就付 O(会话) 窗口。
  assert.equal(
    shouldHydrateConversationTurnNavigatorDirectory({ ...base, directoryRequested: false }),
    false,
  );
  assert.equal(
    shouldHydrateConversationTurnNavigatorDirectory({ ...base, directoryRequested: true }),
    true,
  );
  // 窄容器 rail 不可见，意图也不触发。
  assert.equal(
    shouldHydrateConversationTurnNavigatorDirectory({
      ...base,
      containerWidthPx: 400,
      directoryRequested: true,
    }),
    false,
  );
  // 其余门槛不变。
  assert.equal(
    shouldHydrateConversationTurnNavigatorDirectory({
      ...base,
      canLoadOlder: false,
      directoryRequested: true,
    }),
    false,
  );
  assert.equal(
    shouldHydrateConversationTurnNavigatorDirectory({
      ...base,
      loadingOlder: true,
      directoryRequested: true,
    }),
    false,
  );
  assert.equal(
    shouldHydrateConversationTurnNavigatorDirectory({
      ...base,
      hasLoadHandler: false,
      directoryRequested: true,
    }),
    false,
  );
});

test("补拉退避表：250ms / 1000ms / terminal", () => {
  // stale 与 retryable-failure 两条路径都走这张表（含 2026-09-21 把 stale 并入退避的修复）:
  // 有界是关键——无 lease 的 stale 若裸重试会无限自旋。
  assert.equal(resolveConversationTurnNavigatorHydrationRetryDelayMs(1), 250);
  assert.equal(resolveConversationTurnNavigatorHydrationRetryDelayMs(2), 1_000);
  assert.equal(resolveConversationTurnNavigatorHydrationRetryDelayMs(3), null);
  assert.equal(resolveConversationTurnNavigatorHydrationRetryDelayMs(99), null);
});
