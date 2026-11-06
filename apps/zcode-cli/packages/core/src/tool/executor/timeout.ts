import {
  CoreErrorType,
  SessionEventType,
  createCoreError,
  type SessionEvent,
} from "@zcode/contracts";
import type { ToolExecutionContext, ToolEntry, ToolExecutionModelContext } from "../types.js";
import { isRecord } from "./utils.js";

/**
* Pauseable tool deadline.
 *
* The model request inside the tool pauses the timing when queuing in front of the process-level admission gate, and then renews after getting the ticket, and the remaining time is conserved. The one who keeps time is
* "The provider is down", not "our own queue leader": otherwise when the gate lowers the cap, WebSearch / WebFetch will be processed one by one
* The timeout is forced to 60 seconds, and the model is re-searched. The more the flow is limited, the more noisy it is (7 times of such cancel in the deep-research instance).
* Multiple requests and access to the union (counter); back-off sleep does not pause (that is the provider is slow); `timeoutMs` only accumulates when absent
* The queue length is not timed.
 */
export class ToolDeadline {
  private remainingMs: number | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private armedAt = 0;
  private pausedAt = 0;
  private pauseDepth = 0;
  private queuedTotalMs = 0;
  private onExpire: (() => void) | undefined;

  constructor(readonly timeoutMs: number | undefined) {
    this.remainingMs = timeoutMs;
  }

  start(onExpire: () => void): void {
    this.onExpire = onExpire;
    this.arm();
  }

  pause(): void {
    this.pauseDepth += 1;
    if (this.pauseDepth !== 1) return;
    this.pausedAt = Date.now();
    if (this.timer === undefined) return;
    clearTimeout(this.timer);
    this.timer = undefined;
    this.remainingMs = Math.max(0, (this.remainingMs ?? 0) - (this.pausedAt - this.armedAt));
  }

  resume(): void {
    if (this.pauseDepth === 0) return;
    this.pauseDepth -= 1;
    if (this.pauseDepth !== 0) return;
    this.queuedTotalMs += Date.now() - this.pausedAt;
    this.arm();
  }

  clear(): void {
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.timer = undefined;
    this.onExpire = undefined;
  }

/** The accumulated queuing time (including the period that is still paused); it is included in the context of the timeout error to facilitate the distinction between "slow" and "waiting". */
  get queuedMs(): number {
    return this.queuedTotalMs + (this.pauseDepth > 0 ? Date.now() - this.pausedAt : 0);
  }

  private arm(): void {
    if (this.onExpire === undefined || this.remainingMs === undefined || this.pauseDepth > 0)
      return;
    this.armedAt = Date.now();
    this.timer = setTimeout(() => {
      this.timer = undefined;
      this.onExpire?.();
    }, this.remainingMs);
  }
}

/**
* Read the two ends of the admission wait from the model status event of this tool call: `queued` pause, `admitted` continued.
* Only recognize events with this toolCallId - the same emitEvent will also flow through the status of other tool calls.
 */
export function observeToolAdmissionClock(
  event: SessionEvent,
  toolCallId: string,
  deadline: ToolDeadline,
): void {
  if (event.type !== SessionEventType.ModelNetworkStatus) return;
  const payload = event.payload as { type?: unknown; toolCallId?: unknown } | undefined;
  if (payload?.toolCallId !== toolCallId) return;
  if (payload.type === "model_request_queued") deadline.pause();
  else if (payload.type === "model_request_admitted") deadline.resume();
}

export async function executeWithTimeout<TInput, TOutput>(
  handler: (input: TInput, context: ToolExecutionContext) => Promise<TOutput>,
  input: TInput,
  context: ToolExecutionContext,
  deadline: ToolDeadline,
  abortController: AbortController,
  entry: ToolEntry,
): Promise<TOutput> {
  return new Promise((resolve, reject) => {
    if (context.abortSignal.aborted) {
      reject(
        createCoreError(
          CoreErrorType.ToolCancelled,
          entry.cancellation?.userVisibleMessage ?? "Tool execution cancelled",
        ),
      );
      return;
    }

    let settled = false;
    let timedOut = false;
    const cleanup = () => {
      deadline.clear();
      context.abortSignal.removeEventListener("abort", abortHandler);
    };
    const rejectOnce = (error: Error) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(error);
    };
    const resolveOnce = (result: TOutput) => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve(result);
    };

// The timeout-less tool still retains the parent abort listener, but does not create a wall clock timer (deadline.start is a no-op for absent timeoutMs).
    const timeoutMs = deadline.timeoutMs;
    deadline.start(() => {
      timedOut = true;
      const error = createCoreError(
        CoreErrorType.ToolTimeout,
        `Tool execution timed out after ${timeoutMs}ms`,
        {
          context: {
            cancellation: entry.cancellation?.cleanup ?? "none",
            queuedMs: deadline.queuedMs,
            timeoutMs,
            toolName: entry.metadata.name,
          },
          recoverable: true,
        },
      );
      abortController.abort(error);
      rejectOnce(error);
    });

    const abortHandler = () => {
      if (timedOut) return;
      rejectOnce(
        createCoreError(
          CoreErrorType.ToolCancelled,
          entry.cancellation?.userVisibleMessage ?? "Tool execution cancelled",
          {
            context: {
              cancellation: entry.cancellation?.cleanup ?? "none",
              toolName: entry.metadata.name,
            },
            recoverable: true,
          },
        ),
      );
    };

    context.abortSignal.addEventListener("abort", abortHandler);

    handler(input, context)
      .then((result) => {
        resolveOnce(result);
      })
      .catch((error) => {
        rejectOnce(error);
      });
  });
}

export function resolveTimeoutMs(
  entry: ToolEntry,
  input: unknown,
  defaultTimeoutMs: number,
  context?: ToolExecutionModelContext,
): number | undefined {
  const policy = entry.timeout;
  if (policy?.kind === "none") {
    return undefined;
  }

  const defaultMs = policy?.defaultMs ?? entry.metadata.timeoutMs ?? defaultTimeoutMs;
  const entryResolvedMs = entry.resolveTimeoutBudgetMs?.(input, context);
  const requestedMs =
    entryResolvedMs ??
    (policy?.allowCallOverride && isRecord(input) && typeof input.timeout_ms === "number"
      ? input.timeout_ms
      : policy?.allowCallOverride && isRecord(input) && typeof input.timeout === "number"
        ? input.timeout
        : defaultMs);
  const cappedMs = policy?.maxMs === undefined ? requestedMs : Math.min(requestedMs, policy.maxMs);
  const cleanupGraceMs = Math.max(0, Math.trunc(policy?.cleanupGraceMs ?? 0));
  return Math.max(1, Math.trunc(cappedMs)) + cleanupGraceMs;
}

export function linkAbortSignal(
  parentSignal: AbortSignal | undefined,
  childController: AbortController,
): () => void {
  if (!parentSignal) {
    return () => {};
  }

  const abortChild = () => {
    if (!childController.signal.aborted) {
      childController.abort(parentSignal.reason);
    }
  };

  if (parentSignal.aborted) {
    abortChild();
    return () => {};
  }

  parentSignal.addEventListener("abort", abortChild);
  return () => {
    parentSignal.removeEventListener("abort", abortChild);
  };
}
