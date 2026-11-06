import {
  SessionEventType,
  getCurrentModelInvocationContext,
  type Model,
  type ModelNetworkStatusEvent,
  type ModelStatusSink,
} from "@zcode/contracts";
import { withModelInvocationContext } from "../../runtime/methods/runtime-model.js";
import type { ToolExecutionContext } from "../types.js";

/**
* Default status export for tool internal model requests.
 *
* `statusSink` cannot be the responsibility of each call site - WebSearch remembers setting, WebFetch handles not.
* Requests without a sink still pass through the admission gate and are still queued, but the queued / admitted / 429 events sent by the runner have no place to go:
* The deadline of the executor does not know when to pause (actually measured 18 times. WebFetch waited in the queue for 20-45 s before pressing
* The 60 s timeout is canceled (`queuedMs: 0` in the error), and the driver cannot see that the subagent is waiting.
* The same treatment as the admission port: the default value is tied to the **boundary** instead of being remembered by the call point - the executor is handed over to the handler
* `context.model` is first applied to one layer, and if the call point does not set a sink, the session event exit will be added; if the call point sets a sink, it will be left as it is.
 */
export function createToolModelStatusSink(
  context: Pick<ToolExecutionContext, "emitEvent" | "sessionId" | "turnId" | "traceId">,
): ModelStatusSink | undefined {
  if (!context.emitEvent) return undefined;

  return {
    publish: async (statusEvent: ModelNetworkStatusEvent) => {
      await context.emitEvent?.({
        id: crypto.randomUUID() as never,
        sessionId: context.sessionId,
        turnId: context.turnId,
        type: SessionEventType.ModelNetworkStatus,
        timestamp: new Date(),
        traceId: context.traceId,
        sequenceNumber: 0,
        payload: statusEvent,
      });
    },
  };
}

/** If `statusSink` is not set at the call point, the default outlet will be supplemented; if it is set, it will be retained (the default value does not suppress the calling layer). */
export function withDefaultToolModelStatusSink(
  model: Model | undefined,
  sink: ModelStatusSink | undefined,
): Model | undefined {
  if (model === undefined || sink === undefined) return model;
  return withModelInvocationContext(model, () =>
    getCurrentModelInvocationContext()?.statusSink === undefined ? { statusSink: sink } : {},
  );
}
