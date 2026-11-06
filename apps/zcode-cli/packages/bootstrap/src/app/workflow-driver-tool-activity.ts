// ============================================================
// Tool activity observation on the driver side: the subagent is about to rewrite the workspace → engine off import cache
// ============================================================
// Same attitude as the active side of the model in workflow-driver-concurrency.ts: read-only session event stream of actor runtime, without touching turn
// Arrangement. Only one event is recognized here - `ToolCallStarted`, which is issued by the executor after the permission is determined and before the handler takes action. On the payload
// With `readOnly` / `sideEffectScope` after parsing the push-in parameters (Bash’s read-only command judgment has been settled). Determine using contracts
// `isWorkspaceMutatingToolCall`; the same subload also counts the number of calls that "touched the external world" according to `isWorldTouchingToolCall`,
// That number determines whether a cache entry is pure (see below).
//
// The same subscription is also responsible for the **number** of tool calls for this ask (total number plus number of writes in them), since AskStats requires it, and
// There are no tool events in `TurnResult.events` - according to the number, `toolCalls` is always 0 (actual measurement: 2097 events in the production journal
// ask line without exception, including subagents who explicitly wrote the file). The count is reset to zero at startAsk and accumulated across repair / nudge rounds,
// So the last askStats reported is the full amount of this ask.
//
// The two counting divisions are different: `toolCalls` counts all calls (auditing is honest), `worldToolCalls` only counts those that have viewed or moved the external world
// ——Protocol tools (`submit_result`, `escalate`) do not count, otherwise each typed ask will appear to have "touched the world" because of the result.
// None of the pure ask exemptions are used.
//
// Only report the door closing once for each ask: the engine side door closing is idempotent, reporting fewer times just saves event traffic; reset when changing ask.
//
// The same subscription also records the name of the **most recent** tool call and a bounded target thread (`lastTool` of `node-progress`). The name is on `ToolCallStarted`, but the input parameter is not——
// It is only on the earlier `ToolCallScheduled`, so subscribe to both here: scheduled. Save the input parameter by toolCallId,
// started Take it back, crush it into a clue, and then delete that move. Only drop `lastTool` on started, at the same time as both counts,
// So a call that is denied permission and never actually runs won't pretend to be "what it's doing".

import {
  isWorkspaceMutatingToolCall,
  isWorldTouchingToolCall,
  SessionEventType,
  type SessionEvent,
  type SessionId,
  type ToolCallScheduledPayload,
  type ToolCallStartedPayload,
} from "@zcode/contracts";
import type { AgentRuntime } from "@zcode/core";
import type { AskLastTool } from "@zcode/dynamic-workflow";
import { summarizeToolCall } from "./workflow-driver-tool-target.js";

/**
* The maximum number of scheduled inputs to be claimed can be stored in this number of lots. Under normal circumstances, each hand will be taken away by the following started, but it is authorized
* Rejected, canceled, or whole batches of skipped calls will not - there is no upper limit, a long ask can accumulate them into a memory leak.
* Throw away the oldest hand on overflow (Map is traversed in insertion order): the new call is the answer to "what is it doing".
 */
const MAX_PENDING_SCHEDULED_CALLS = 64;

/** The count of tool calls observed within an ask (two fields of `AskStats` are it). */
export interface ActorToolCounts {
/** All calls, including protocol tools such as `submit_result` / `escalate`. */
  toolCalls: number;
/** Among them, those that have seen or touched the external world (reading files, running commands, accessing the network); if it is 0, it means that this ask is pure. */
  worldToolCalls: number;
}

interface ActorToolActivity {
/** Return to zero when changing ask: the count is re-counted, and the first write of the next ask must be reported again. */
  reset(): void;
/** Count of tool calls observed so far for this ask. */
  counts(): ActorToolCounts;
/** This ask is the **most recent** tool call that actually started running so far; it has not been absent even once. */
  lastTool(): AskLastTool | undefined;
/** Subscribe to the runtime's `ToolCallStarted` session event; the minimal stub runtime has no subscribeEvents spatio-temporal operation. */
  observe(runtime: AgentRuntime, sessionId: SessionId): void;
  unsubscribe(): void;
}

export function createActorToolActivity(handlers: {
/** The current ask's subagent is about to execute a tool that will overwrite the workspace (at most once per ask). */
  onMutating(): void;
}): ActorToolActivity {
  let reported = false;
  let toolCalls = 0;
  let worldToolCalls = 0;
  let lastTool: AskLastTool | undefined;
/** toolCallId → Name and input parameters when scheduled, wait for started to claim. */
  const scheduled = new Map<string, ToolCallSummaryHold>();
  let unsubscribeEvents: (() => void) | undefined;
  return {
    reset: () => {
      reported = false;
      toolCalls = 0;
      worldToolCalls = 0;
      lastTool = undefined;
      scheduled.clear();
    },
    counts: () => ({ toolCalls, worldToolCalls }),
    lastTool: () => lastTool,
    observe: (runtime, sessionId) => {
      if (typeof (runtime as Partial<AgentRuntime>).subscribeEvents !== "function") return;
      unsubscribeEvents = runtime.subscribeEvents({
        onSessionEvent: (event: SessionEvent) => {
          if (event.sessionId !== sessionId) return;
          if (event.type === SessionEventType.ToolCallScheduled) {
            holdScheduled(scheduled, event.payload as ToolCallScheduledPayload);
            return;
          }
          if (event.type !== SessionEventType.ToolCallStarted) return;
          const capability = event.payload as ToolCallStartedPayload;
          const hold = scheduled.get(String(capability.toolCallId));
          scheduled.delete(String(capability.toolCallId));
          lastTool =
            summarizeToolCall({
              ...(hold === undefined ? {} : { input: hold.input }),
              toolName: capability.toolName ?? hold?.toolName,
            }) ?? lastTool;
          toolCalls++;
          if (isWorldTouchingToolCall(capability)) worldToolCalls++;
          if (!isWorkspaceMutatingToolCall(capability)) return;
          if (reported) return;
          reported = true;
          handlers.onMutating();
        },
      });
    },
    unsubscribe: () => {
      unsubscribeEvents?.();
      unsubscribeEvents = undefined;
    },
  };
}

/** The first hand (name + input parameters) temporarily saved on the scheduled event, waiting for the corresponding started to be taken away. */
interface ToolCallSummaryHold {
  toolName?: string;
  input?: unknown;
}

/** Save a hand of scheduled input parameters, and press the table within {@link MAX_PENDING_SCHEDULED_CALLS} (throw away the oldest one). */
function holdScheduled(
  scheduled: Map<string, ToolCallSummaryHold>,
  payload: ToolCallScheduledPayload,
): void {
  scheduled.set(String(payload.toolCallId), { toolName: payload.toolName, input: payload.input });
  while (scheduled.size > MAX_PENDING_SCHEDULED_CALLS) {
    const oldest = scheduled.keys().next();
    if (oldest.done === true) return;
    scheduled.delete(oldest.value);
  }
}
