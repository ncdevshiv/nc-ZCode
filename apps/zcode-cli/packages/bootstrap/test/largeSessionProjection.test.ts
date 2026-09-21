/**
 * 大会话性能回归测试（spec: specs/large-session-scaling.md）——运行时侧。
 *
 * command row actions 是行状态的派生物。修复把 materializeCommandRowActions 的触发
 * 时机收窄为"本帧 delta 可能改变 actions 输入"（纯 row.delta 帧跳过），并把它内部的
 * JSON.stringify 比较换成逐键浅比较。跳过必须在输出上不可观察，因此这里锁定的是
 * **语义不变量**而非实现细节：
 *   1. 纯文本增量帧只产生 row.delta，不夹带任何 actions upsert；
 *   2. 状态变化事件（轮次结束）之后 actions 照常物化（edit/retry 入口不消失）；
 *   3. 冷恢复收口（空 delta 集）仍然物化 actions。
 *
 * 运行方式（仓库无统一 test 脚本）：
 *   cd apps/zcode-cli/packages/bootstrap && node --import tsx --test test/largeSessionProjection.test.ts
 */
import assert from "node:assert/strict";
import test from "node:test";
import type { SessionEvent } from "@zcode/contracts";
import { SessionEventType } from "@zcode/contracts";
import { ProductProjection } from "../src/zcode-protocol-v4/product-projection.js";
import type { ConversationDelta } from "@zcode/shared/zcode-protocol-v4";
import type { ConversationRow } from "@zcode/shared/zcode-protocol-v4";

const SESSION_ID = "session-perf-test";

function makeEvent(
  sequenceNumber: number,
  type: SessionEvent["type"],
  payload: unknown,
  turnId = "turn-1",
): SessionEvent {
  return {
    id: `evt-${sequenceNumber}`,
    sessionId: SESSION_ID,
    turnId,
    type,
    timestamp: new Date(1_700_000_000_000 + sequenceNumber),
    traceId: "trace-perf-test",
    sequenceNumber,
    payload,
  };
}

function turnStarted(sequenceNumber: number): SessionEvent {
  return makeEvent(sequenceNumber, SessionEventType.TurnStarted, {
    turnNumber: 1,
    input: "帮我看一下这个仓库",
    // 持久 messageId 是 actions 的 canonical target 前提（投影据此把 userInput row
    // 反查到 message）；缺它 canEdit/canRetry 一律不出现，与性能修复无关。
    messageId: "user-msg-1",
  });
}

function textStart(sequenceNumber: number, assistantMessageId: string): SessionEvent {
  return makeEvent(sequenceNumber, SessionEventType.ModelStreaming, {
    kind: "text_start",
    delta: "",
    done: false,
    assistantMessageId,
  });
}

function textDelta(sequenceNumber: number, assistantMessageId: string, delta: string): SessionEvent {
  return makeEvent(sequenceNumber, SessionEventType.ModelStreaming, {
    kind: "text_delta",
    delta,
    done: false,
    assistantMessageId,
  });
}

function textEnd(sequenceNumber: number, assistantMessageId: string): SessionEvent {
  return makeEvent(sequenceNumber, SessionEventType.ModelStreaming, {
    kind: "text_end",
    delta: "",
    done: true,
    assistantMessageId,
  });
}

function turnComplete(sequenceNumber: number): SessionEvent {
  return makeEvent(sequenceNumber, SessionEventType.TurnComplete, {
    response: "好的，我来看看。",
    tokenCount: 1,
    toolCallCount: 0,
    duration: 100,
    resultType: "success",
  });
}

function rowsWithActions(snapshotRows: readonly ConversationRow[]): ConversationRow[] {
  return snapshotRows.filter((row) => row.actions !== undefined);
}

function actionUpserts(deltas: readonly ConversationDelta[]): ConversationDelta[] {
  return deltas.filter(
    (delta) => delta.op === "row.upserted" && delta.row.actions !== undefined,
  );
}

test("纯文本增量帧只产生 row.delta，不夹带 actions upsert", () => {
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
  projection.applyEvent(turnStarted(1));
  projection.applyEvent(textStart(2, "msg-1"));

  const deltaDeltas = projection.applyEvent(textDelta(3, "msg-1", "好的"));
  assert.equal(deltaDeltas.length, 1, "流式追加帧应只有一条 row.delta");
  assert.equal(deltaDeltas[0]!.op, "row.delta");
  assert.equal(actionUpserts(deltaDeltas).length, 0, "不得夹带 actions 物化结果");

  // 连续多帧同理：actions 输入未变，跳过物化后输出必须仍然只有文本追加。
  for (let index = 4; index < 12; index += 1) {
    const frame = projection.applyEvent(textDelta(index, "msg-1", `片段${index}`));
    assert.ok(frame.every((delta) => delta.op === "row.delta"), `第 ${index} 帧必须是纯追加`);
    assert.equal(actionUpserts(frame).length, 0);
  }

  // 关键不变量：纯增量帧不改变快照的 actions 分布（跳过物化无残留、无丢失）。
  const actionsBefore = rowsWithActions(projection.getSnapshot().rows.window).map(
    (row) => `${row.rowId}:${JSON.stringify(row.actions)}`,
  );
  projection.applyEvent(textDelta(12, "msg-1", "再一段"));
  const actionsAfter = rowsWithActions(projection.getSnapshot().rows.window).map(
    (row) => `${row.rowId}:${JSON.stringify(row.actions)}`,
  );
  assert.deepEqual(actionsAfter, actionsBefore);

  projection.applyEvent(textEnd(13, "msg-1"));
});

test("轮次结束事件之后 actions 照常物化（edit/retry 入口不消失）", () => {
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
  projection.applyEvent(turnStarted(1));
  projection.applyEvent(textStart(2, "msg-1"));
  projection.applyEvent(textDelta(3, "msg-1", "好的，我来看看。"));
  projection.applyEvent(textEnd(4, "msg-1"));

  const completeDeltas = projection.applyEvent(turnComplete(5));
  assert.ok(
    actionUpserts(completeDeltas).length > 0,
    "状态变化事件必须触发 actions 物化（修复后入口不能消失）",
  );

  const rows = projection.getSnapshot().rows.window;
  const userRow = rows.find((row) => row.kind === "userInput");
  const assistantRow = rows.find((row) => row.kind === "assistantText");
  assert.equal(userRow?.actions?.canEdit, true, "最新 realUser 行必须带 canEdit");
  assert.equal(assistantRow?.actions?.canRetry, true, "最新完整 assistant 行必须带 canRetry");
  assert.equal(assistantRow?.actions?.canFork, true, "最新完整 assistant 行必须带 canFork");
});

test("冷恢复收口（空 delta 集）仍然物化 actions", () => {
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
  // hydration replay 期间 materializeActions=false，actions 延迟到收口一次物化。
  projection.beginHydrationReplay();
  projection.applyHydrationEvent(turnStarted(1));
  projection.applyHydrationEvent(textStart(2, "msg-1"));
  projection.applyHydrationEvent(textDelta(3, "msg-1", "好的，我来看看。"));
  projection.applyHydrationEvent(textEnd(4, "msg-1"));
  projection.applyHydrationEvent(turnComplete(5));

  const closingDeltas = projection.completeHydrationReplay();
  assert.ok(
    actionUpserts(closingDeltas).length > 0,
    "completeHydrationReplay 传空 delta 集，必须仍然物化 actions",
  );

  const rows = projection.getSnapshot().rows.window;
  const userRow = rows.find((row) => row.kind === "userInput");
  const assistantRow = rows.find((row) => row.kind === "assistantText");
  assert.equal(userRow?.actions?.canEdit, true);
  assert.equal(assistantRow?.actions?.canRetry, true);
});

test("纯 row.delta 帧不触发 actions 物化（探针测试，防门被删后静默回归）", () => {  // 跳过物化在上是不可观察的（旧实现这些帧也产出空 delta 集），语义断言抓不到它。
  // 这里直接对物化入口计数：门一旦被删或放宽，每个文本增量帧都会重新触发整窗物化，
  // 长会话的逐帧成本随之回到 O(会话)——那正是本 PR 要消除的回归。
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
  const prototype = Object.getPrototypeOf(projection) as {
    materializeCommandRowActions: (...args: unknown[]) => unknown;
  };
  const original = prototype.materializeCommandRowActions;
  let materializeCalls = 0;
  prototype.materializeCommandRowActions = function patched(
    this: unknown,
    ...args: unknown[]
  ) {
    materializeCalls += 1;
    return original.apply(this, args);
  };

  try {
    projection.applyEvent(turnStarted(1));
    projection.applyEvent(textStart(2, "msg-1"));
    const callsAfterOpen = materializeCalls;

    // 8 个纯文本增量帧：一次物化都不该发生。
    for (let index = 3; index < 11; index += 1) {
      projection.applyEvent(textDelta(index, "msg-1", `片段${index}`));
    }
    assert.equal(
      materializeCalls,
      callsAfterOpen,
      "纯 row.delta 帧不得触发 actions 物化（逐帧 O(窗口) 成本即根因）",
    );

    // 第二个纯 row.delta 生产者：toolCall 的 inputText 追加。门按 delta op 判定而不是按
    // 事件类型/行类型判定，因此每个生产者都该有一个代表帧（subagent 的 summaryText 追加
    // 走同一条 appendToRow 路径，由构造保证覆盖）。
    projection.applyEvent(
      makeEvent(11, SessionEventType.ModelStreaming, {
        kind: "tool_input_start",
        delta: "",
        done: false,
        assistantMessageId: "msg-1",
        toolCallId: "tc-1",
        toolName: "Bash",
      }),
    );
    const callsAfterToolOpen = materializeCalls;
    for (let index = 12; index < 16; index += 1) {
      projection.applyEvent(
        makeEvent(index, SessionEventType.ModelStreaming, {
          kind: "tool_input_delta",
          delta: `{"chunk${index}":`,
          done: false,
          assistantMessageId: "msg-1",
          toolCallId: "tc-1",
          toolName: "Bash",
        }),
      );
    }
    assert.equal(
      materializeCalls,
      callsAfterToolOpen,
      "toolCall inputText 增量帧同样不得触发 actions 物化",
    );

    // 状态变化事件必须仍然物化：入口不能因跳过逻辑消失。
    projection.applyEvent(textEnd(16, "msg-1"));
    projection.applyEvent(turnComplete(17));
    assert.ok(
      materializeCalls > callsAfterToolOpen,
      "text_end / TurnComplete 等状态变化帧必须触发 actions 物化",
    );
  } finally {
    prototype.materializeCommandRowActions = original;
  }
});

test("新一轮开始撤销旧入口、且不重写动作未变化的行", () => {
  // 结构比较的两个方向都要有用例：
  //   - 变化 → 必须下发 upsert（旧 latest editable 撤销 canEdit、旧 assistant 撤销 canRetry）
  //   - 未变化 → 不得下发（否则每帧重发整窗 actions，正是被替换掉的 JSON.stringify 成本）
  // 撤销发生在第二轮 TurnStarted（新 realUser 行成为 latest editable）那一刻。
  // 注意：撤销态的 upsert 上 actions 是 undefined，不能用“只挑带 actions 的 upsert”来断言。
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
  projection.applyEvent(turnStarted(1));
  projection.applyEvent(textStart(2, "msg-1"));
  projection.applyEvent(textDelta(3, "msg-1", "第一轮回复"));
  projection.applyEvent(textEnd(4, "msg-1"));
  projection.applyEvent(turnComplete(5));

  const rowsBefore = projection.getSnapshot().rows.window;
  const firstTurnUserRowId = rowsBefore.find((row) => row.kind === "userInput")!.rowId;
  const firstTurnAssistantRowId = rowsBefore.find((row) => row.kind === "assistantText")!.rowId;
  const firstTurnHeaderRowId = rowsBefore.find((row) => row.kind === "turnHeader")!.rowId;

  const secondTurnStarted = projection.applyEvent(
    makeEvent(6, SessionEventType.TurnStarted, {
      turnNumber: 2,
      input: "第二个问题",
      messageId: "user-msg-2",
    }, "turn-2"),
  );
  const upsertedRows = secondTurnStarted.filter((delta) => delta.op === "row.upserted");

  const revokedEditable = upsertedRows.find(
    (delta) => delta.op === "row.upserted" && delta.row.rowId === firstTurnUserRowId,
  );
  assert.ok(revokedEditable !== undefined, "旧 latest editable 行必须被改写");
  assert.equal(
    revokedEditable && revokedEditable.op === "row.upserted"
      ? revokedEditable.row.actions?.canEdit
      : "missing",
    undefined,
    "撤销后旧行不再带 canEdit",
  );

  const revokedRetry = upsertedRows.find(
    (delta) => delta.op === "row.upserted" && delta.row.rowId === firstTurnAssistantRowId,
  );
  assert.equal(
    revokedRetry && revokedRetry.op === "row.upserted"
      ? revokedRetry.row.actions?.canRetry
      : "missing",
    undefined,
    // 触发点实际是新轮 TurnStarted 的 state.updated 把 control.activeWorks 置上
    // （completionBlockingActive 见 materializeCommandRowActions），并非此处注释意义上的
    // latest-only 规则；断言本身锁定的是"旧入口被撤销"这一可观察行为。
    "旧轮的 canRetry 必须被撤销",
  );
  assert.equal(
    revokedRetry && revokedRetry.op === "row.upserted" ? revokedRetry.row.actions?.canFork : "missing",
    true,
    "fork 不是 latest-only：旧轮保留 canFork",
  );

  // 未变化方向：动作未涉及的行（turnHeader）不得出现在这批 upsert 里。
  assert.ok(
    !upsertedRows.some((delta) => delta.op === "row.upserted" && delta.row.rowId === firstTurnHeaderRowId),
    "动作未变化的行不得被重写（浅比较必须识别“未变化”）",
  );

  const userRows = projection
    .getSnapshot()
    .rows.window.filter((row) => row.kind === "userInput");
  assert.equal(userRows.length, 2);
  assert.equal(userRows[0]?.actions?.canEdit, undefined, "旧轮不再可编辑");
  assert.equal(userRows[1]?.actions?.canEdit, true, "新一轮成为 latest editable");
});

test("结构帧上内容未变的 actions 不得产生 upsert（浅比较“相等”方向）", () => {
  // 变异证明：若把 conversationRowActionsEqual 简化成“仅引用相等”（对内容相同的两个对象
  // 返回 false），materializeCommandRowActions 会在每个结构帧上为整窗 actions 未变的行重发
  // 逐字节相同的 upsert，row.upserted 还会推高 revision——正是本 PR 要消除的每帧冗余。
  // 既有测试只覆盖“两侧都是 undefined”的早退，抓不到这个方向，这里显式锁定。
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
  projection.applyEvent(turnStarted(1));
  projection.applyEvent(textStart(2, "msg-1"));
  projection.applyEvent(textDelta(3, "msg-1", "第一轮回复"));
  projection.applyEvent(textEnd(4, "msg-1"));
  projection.applyEvent(turnComplete(5));

  const assistantRowId = projection
    .getSnapshot()
    .rows.window.find((row) => row.kind === "assistantText")!.rowId;

  const prototype = Object.getPrototypeOf(projection) as {
    materializeCommandRowActions: (...args: unknown[]) => ConversationDelta[];
  };
  const original = prototype.materializeCommandRowActions;
  let materialized: ConversationDelta[] = [];
  prototype.materializeCommandRowActions = function patched(
    this: unknown,
    ...args: unknown[]
  ) {
    const result = original.apply(this, args) as ConversationDelta[];
    materialized = result;
    return result;
  };

  try {
    // 结构帧：新开一个工具行（row.appended + state.updated），不动已有行的 actions。
    projection.applyEvent(
      makeEvent(6, SessionEventType.ModelStreaming, {
        kind: "tool_input_start",
        delta: "",
        done: false,
        assistantMessageId: "msg-1",
        toolCallId: "tc-1",
        toolName: "Bash",
      }),
    );
  } finally {
    prototype.materializeCommandRowActions = original;
  }

  assert.ok(
    materialized.every((delta) => delta.op !== "row.upserted" || delta.row.rowId !== assistantRowId),
    "actions 内容未变的行不得被重写（浅比较必须识别“相等”）",
  );
  assert.deepEqual(
    materialized.map((delta) => delta.op),
    [],
    "该结构帧没有任何行的 actions 需要变化",
  );
});
