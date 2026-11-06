export class McpTimeoutError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "McpTimeoutError";
  }
}

export interface McpDeadline {
  expiresAt: number;
  timeoutMs: number;
}

export function createMcpDeadline(timeoutMs: number): McpDeadline {
  const normalizedTimeoutMs = Math.max(0, Math.floor(timeoutMs));
  return {
    expiresAt: Date.now() + normalizedTimeoutMs,
    timeoutMs: normalizedTimeoutMs,
  };
}

export function remainingMcpDeadlineMs(deadline: McpDeadline, timeoutMessage: string): number {
  const remainingMs = deadline.expiresAt - Date.now();
  if (remainingMs <= 0) {
    throw new McpTimeoutError(timeoutMessage);
  }
  return remainingMs;
}

/**
* Only limit the current waiter and do not cancel the incoming shared promise. Whether the underlying task is canceled is determined by its own owner signal.
 */
export function waitWithinMcpDeadline<T>(
  promise: Promise<T>,
  deadline: McpDeadline,
  timeoutMessage: string,
  signal?: AbortSignal,
): Promise<T> {
  return withTimeout(
    promise,
    remainingMcpDeadlineMs(deadline, timeoutMessage),
    timeoutMessage,
    signal,
  );
}

export function withTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number,
  timeoutMessage: string,
  signal?: AbortSignal,
): Promise<T> {
  return new Promise((resolvePromise, rejectPromise) => {
    if (signal?.aborted) {
      rejectPromise(
        signal.reason instanceof Error ? signal.reason : new Error("Operation aborted"),
      );
      return;
    }

    let settled = false;
    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      cleanup();
      rejectPromise(new McpTimeoutError(timeoutMessage));
    }, timeoutMs);

    const cleanup = () => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", abortHandler);
    };

    const abortHandler = () => {
      if (settled) return;
      settled = true;
      cleanup();
      rejectPromise(
        signal?.reason instanceof Error ? signal.reason : new Error("Operation aborted"),
      );
    };

    signal?.addEventListener("abort", abortHandler);

    promise.then(
      (value) => {
        if (settled) return;
        settled = true;
        cleanup();
        resolvePromise(value);
      },
      (error) => {
        if (settled) return;
        settled = true;
        cleanup();
        rejectPromise(error);
      },
    );
  });
}
