/**
* Large session performance regression test (spec: specs/large-session-scaling.md) - runtime side.
 *
* command row actions are derivatives of row status. Fix triggering of materializeCommandRowActions
* Narrow the timing to "this frame delta may change the actions input" (pure row.delta frame skipping) and put it internally
* JSON.stringify comparison is replaced by key-by-key shallow comparison. The skip must be unobservable on the output, so what is locked here is
* **Semantic invariants** rather than implementation details:
* 1. The plain text delta frame only generates row.delta, without any actions upsert;
* 2. After the state change event (end of round), actions are materialized as usual (edit/retry entry does not disappear);
* 3. Cold recovery closure (empty delta set) still materializes actions.
 *
* Running method (there is no unified test script in the warehouse):
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
    input: "Help me take a look at this warehouse",
// Persistent messageId is the canonical target premise of actions (projection accordingly puts userInput row
//reverse message); without it, canEdit/canRetry will not appear and has nothing to do with performance repair.
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
    response: "OK, let me take a look.",
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

test("Plain text delta frames only generate row.delta and do not include actions upsert.", () => {
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
  projection.applyEvent(turnStarted(1));
  projection.applyEvent(textStart(2, "msg-1"));

  const deltaDeltas = projection.applyEvent(textDelta(3, "msg-1", "OK"));
  assert.equal(deltaDeltas.length, 1, "Streaming append frames should have only one row.delta");
  assert.equal(deltaDeltas[0]!.op, "row.delta");
  assert.equal(actionUpserts(deltaDeltas).length, 0, "Actions materialized results must not be included");

// The same applies to consecutive multiple frames: the input of actions remains unchanged, and the output must still only have text appended after skipping materialization.
  for (let index = 4; index < 12; index += 1) {
    const frame = projection.applyEvent(textDelta(index, "msg-1", `fragment${index}`));
    assert.ok(frame.every((delta) => delta.op === "row.delta"), `Frame ${index} must be pure append`);
    assert.equal(actionUpserts(frame).length, 0);
  }

// Key invariant: Pure incremental frames do not change the actions distribution of the snapshot (no residue or loss after materialization is skipped).
  const actionsBefore = rowsWithActions(projection.getSnapshot().rows.window).map(
    (row) => `${row.rowId}:${JSON.stringify(row.actions)}`,
  );
  projection.applyEvent(textDelta(12, "msg-1", "One more paragraph"));
  const actionsAfter = rowsWithActions(projection.getSnapshot().rows.window).map(
    (row) => `${row.rowId}:${JSON.stringify(row.actions)}`,
  );
  assert.deepEqual(actionsAfter, actionsBefore);

  projection.applyEvent(textEnd(13, "msg-1"));
});

test("After the round end event, the actions are materialized as usual (the edit/retry entry does not disappear)", () => {
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
  projection.applyEvent(turnStarted(1));
  projection.applyEvent(textStart(2, "msg-1"));
  projection.applyEvent(textDelta(3, "msg-1", "OK, let me take a look."));
  projection.applyEvent(textEnd(4, "msg-1"));

  const completeDeltas = projection.applyEvent(turnComplete(5));
  assert.ok(
    actionUpserts(completeDeltas).length > 0,
    "The state change event must trigger the materialization of actions (the entrance cannot disappear after repair)",
  );

  const rows = projection.getSnapshot().rows.window;
  const userRow = rows.find((row) => row.kind === "userInput");
  const assistantRow = rows.find((row) => row.kind === "assistantText");
  assert.equal(userRow?.actions?.canEdit, true, "The latest realUser line must have canEdit");
  assert.equal(assistantRow?.actions?.canRetry, true, "The latest complete assistant line must have canRetry");
  assert.equal(assistantRow?.actions?.canFork, true, "The latest complete assistant line must have canFork");
});

test("Cold recovery closure (empty delta set) still materializes actions", () => {
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
//During hydration replay, materializeActions=false, actions are delayed until closing and materialized once.
  projection.beginHydrationReplay();
  projection.applyHydrationEvent(turnStarted(1));
  projection.applyHydrationEvent(textStart(2, "msg-1"));
  projection.applyHydrationEvent(textDelta(3, "msg-1", "OK, let me take a look."));
  projection.applyHydrationEvent(textEnd(4, "msg-1"));
  projection.applyHydrationEvent(turnComplete(5));

  const closingDeltas = projection.completeHydrationReplay();
  assert.ok(
    actionUpserts(closingDeltas).length > 0,
    "completeHydrationReplay passes an empty delta set and must still materialize actions",
  );

  const rows = projection.getSnapshot().rows.window;
  const userRow = rows.find((row) => row.kind === "userInput");
  const assistantRow = rows.find((row) => row.kind === "assistantText");
  assert.equal(userRow?.actions?.canEdit, true);
  assert.equal(assistantRow?.actions?.canRetry, true);
});

test("Pure row.delta frames do not trigger actions materialization (probe test, the defense door returns silently after being deleted)", () => {  // 跳过物化在上是不可观察的（旧实现这些帧也产出空 delta 集），语义断言抓不到它。
// Here the materialization entrance is counted directly: once the door is deleted or relaxed, each text increment frame will re-trigger the entire window materialization.
// The frame-by-frame cost for long sessions then comes back to O(session) - that's the regression this PR is trying to eliminate.
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

// 8 plain text delta frames: not a single materialization should occur.
    for (let index = 3; index < 11; index += 1) {
      projection.applyEvent(textDelta(index, "msg-1", `fragment${index}`));
    }
    assert.equal(
      materializeCalls,
      callsAfterOpen,
      "Pure row.delta frames must not trigger actions materialization (frame-by-frame O(window) cost as root cause)",
    );

// Second pure row.delta producer: toolCall's inputText appended. Doors are judged by delta op instead of by
//Event type/row type determination, so each producer should have a representative frame (subagent's summaryText appended
// Take the same appendToRow path, guaranteed to be covered by construction).
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
      "toolCall inputText incremental frame must also not trigger actions materialization",
    );

//State change events must still be materialized: the entry cannot disappear due to skip logic.
    projection.applyEvent(textEnd(16, "msg-1"));
    projection.applyEvent(turnComplete(17));
    assert.ok(
      materializeCalls > callsAfterToolOpen,
      "Text_end / TurnComplete and other state change frames must trigger actions materialization",
    );
  } finally {
    prototype.materializeCommandRowActions = original;
  }
});

test("A new round begins to revoke the old entry and do not rewrite the lines whose actions have not changed.", () => {
// Both directions of structure comparison require use cases:
// - Change → upsert must be issued (old latest editable cancels canEdit, old assistant cancels canRetry)
// - unchanged → must not be sent (otherwise the entire window actions will be resent every frame, which is the cost of the replaced JSON.stringify)
// Undo occurs at the moment of the second TurnStarted (new realUser row becomes latest editable).
// Note: The actions on the canceled upsert are undefined, and cannot be asserted by "only picking upserts with actions".
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
  projection.applyEvent(turnStarted(1));
  projection.applyEvent(textStart(2, "msg-1"));
  projection.applyEvent(textDelta(3, "msg-1", "first round of replies"));
  projection.applyEvent(textEnd(4, "msg-1"));
  projection.applyEvent(turnComplete(5));

  const rowsBefore = projection.getSnapshot().rows.window;
  const firstTurnUserRowId = rowsBefore.find((row) => row.kind === "userInput")!.rowId;
  const firstTurnAssistantRowId = rowsBefore.find((row) => row.kind === "assistantText")!.rowId;
  const firstTurnHeaderRowId = rowsBefore.find((row) => row.kind === "turnHeader")!.rowId;

  const secondTurnStarted = projection.applyEvent(
    makeEvent(6, SessionEventType.TurnStarted, {
      turnNumber: 2,
      input: "second question",
      messageId: "user-msg-2",
    }, "turn-2"),
  );
  const upsertedRows = secondTurnStarted.filter((delta) => delta.op === "row.upserted");

  const revokedEditable = upsertedRows.find(
    (delta) => delta.op === "row.upserted" && delta.row.rowId === firstTurnUserRowId,
  );
  assert.ok(revokedEditable !== undefined, "The old latest editable line must be overwritten");
  assert.equal(
    revokedEditable && revokedEditable.op === "row.upserted"
      ? revokedEditable.row.actions?.canEdit
      : "missing",
    undefined,
    "After revocation, the old row no longer has canEdit.",
  );

  const revokedRetry = upsertedRows.find(
    (delta) => delta.op === "row.upserted" && delta.row.rowId === firstTurnAssistantRowId,
  );
  assert.equal(
    revokedRetry && revokedRetry.op === "row.upserted"
      ? revokedRetry.row.actions?.canRetry
      : "missing",
    undefined,
// The trigger point is actually the state.updated of the new turn TurnStarted. Set control.activeWorks
// (completionBlockingActive see materializeCommandRowActions), not in the sense of the comments here
// latest-only rule; the assertion itself locks in the observable behavior of "old entry being revoked".
    "The old round's canRetry must be revoked",
  );
  assert.equal(
    revokedRetry && revokedRetry.op === "row.upserted" ? revokedRetry.row.actions?.canFork : "missing",
    true,
    "fork is not latest-only: old rounds are retained canFork",
  );

// Unchanged direction: Lines not involved in the action (turnHeader) must not appear in this batch of upserts.
  assert.ok(
    !upsertedRows.some((delta) => delta.op === "row.upserted" && delta.row.rowId === firstTurnHeaderRowId),
    "Lines whose actions have not changed must not be rewritten (a shallow comparison must identify 'unchanged')",
  );

  const userRows = projection
    .getSnapshot()
    .rows.window.filter((row) => row.kind === "userInput");
  assert.equal(userRows.length, 2);
  assert.equal(userRows[0]?.actions?.canEdit, undefined, "Old wheels are no longer editable");
  assert.equal(userRows[1]?.actions?.canEdit, true, "The new round becomes latest editable");
});

test("Actions whose content on the structure frame has not changed must not generate upsert (shallow comparison 'equal' direction)", () => {
// Proof of mutation: If conversationRowActionsEqual is simplified to "only reference equality" (for two objects with the same content
// Return false), materializeCommandRowActions will resend the unchanged rows of actions for the entire window on each structure frame
// A byte-for-byte identical upsert, row.upserted will also push up the revision - exactly what this PR is trying to eliminate per-frame redundancy.
// The existing test only covers the early exit of "both sides are undefined" and cannot catch this direction, so it is explicitly locked here.
  const projection = new ProductProjection(SESSION_ID, "epoch-1");
  projection.applyEvent(turnStarted(1));
  projection.applyEvent(textStart(2, "msg-1"));
  projection.applyEvent(textDelta(3, "msg-1", "first round of replies"));
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
// Structural frame: Open a new tool row (row.appended + state.updated), and do not move the actions of the existing row.
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
    "Actions lines whose contents have not changed must not be rewritten (shallow comparison must recognize 'equal')",
  );
  assert.deepEqual(
    materialized.map((delta) => delta.op),
    [],
    "No rows of actions in this structure frame need to be changed.",
  );
});
