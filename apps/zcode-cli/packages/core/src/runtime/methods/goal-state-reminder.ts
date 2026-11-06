import { createMessageId, traceContextToLogContext } from "../deps.js";
import type { TraceContext } from "../deps.js";
import type { AgentRuntimeInternal } from "../internal.js";
import type { ActiveTurnSteeringState } from "../types.js";

export async function recordGoalStateChangeReminder(
  this: AgentRuntimeInternal,
  input: {
    text: string;
    traceContext?: TraceContext;
  },
): Promise<void> {
  const traceContext = input.traceContext ?? this.rootTraceContext;
  const activeTurn = this.activeTurn;
  if (
    activeTurn?.kind === "regular" &&
    activeTurn.goalStateChangeReminderDeferralOpen
  ) {
// Stop will first pause the goal and then abort the executing tool. If you write history immediately here,
// reminder will fall between tool_use and canceled tool_result, causing the next request to violate the provider grammar.
    activeTurn.pendingGoalStateChangeReminder = { text: input.text };
    return;
  }
  await materializeGoalStateChangeReminder.call(this, input.text, traceContext);
}

export function openGoalStateChangeReminderDeferral(
  activeTurn: ActiveTurnSteeringState | undefined,
): void {
  if (activeTurn?.kind === "regular") {
    activeTurn.goalStateChangeReminderDeferralOpen = true;
  }
}

export async function closeGoalStateChangeReminderDeferral(
  this: AgentRuntimeInternal,
  activeTurn: ActiveTurnSteeringState | undefined,
  traceContext: TraceContext,
): Promise<void> {
  if (activeTurn?.kind !== "regular") return;

// activeTurn will continue to survive until terminal/accounting is completed and cannot be used anymore.
// Determine whether pending ownership exists or not. It is closed synchronously first, and the reminder that arrives later will be directly materialized.
  activeTurn.goalStateChangeReminderDeferralOpen = false;
  const pending = activeTurn.pendingGoalStateChangeReminder;
  if (!pending) return;

  activeTurn.pendingGoalStateChangeReminder = undefined;
  try {
    await materializeGoalStateChangeReminder.call(this, pending.text, traceContext);
  } catch (error) {
// The authoritative target has been dropped into the library first; the failure of reminder persistence cannot prevent Stop turn from issuing a terminal event.
// Maintain a single materialization and do not introduce retry, cursor or additional persistent state machine at the end of the turn.
    this.logger?.warn("Failed to materialize pending goal state reminder", {
      ...traceContextToLogContext(traceContext),
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "target.reminder.materialize.failed",
      module: "core.runtime",
      status: "failed",
    });
  }
}

async function materializeGoalStateChangeReminder(
  this: AgentRuntimeInternal,
  text: string,
  traceContext: TraceContext,
): Promise<void> {
  await this.ensureContextInitialized(traceContext);
  this.messageHistory.addAttachment("goal_state_change", text);
  await this.persistSyntheticUserNoticeForSession({
    messageID: createMessageId(),
    sessionId: this.sessionId,
    source: "goal_state_change",
    text,
    traceContext,
    visibility: "model-only",
  });
}
