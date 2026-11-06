import { runInContext, type Context } from "node:vm";
import {
  instrumentForContextPersistence,
  parseReplCode,
  rewriteDynamicImportsForRepl,
} from "./instrument.js";

/**
* ReplExecutor - "Execution + Cross-call Persistence" strategy seam (A-ready).
 *
* NodeReplSession.run only adjusts executor.run, and switching implementation means switching routes:
* - Route B (current): IifeContextExecutor -- instrument top-level declaration → async-IIFE → runInContext.
* - Route A (future, requires --experimental-vm-modules): SourceTextModuleExecutor - harvest version instrument
* → SourceTextModule.evaluate → harvest namespace. The body is left unchanged, only this implementation is replaced.
 *
* Contract: Return completion value; throw if error occurs (normalized to structured error by NodeReplSession's try/catch).
* signal supports cancellation (timeout/stop).
 */
export interface ReplExecutor {
  run(
    code: string,
    context: Context,
    signal?: AbortSignal,
    syncTimeoutMs?: number,
  ): Promise<unknown>;
}

/**
* Route B executor: parse user code, instrument top-level declaration to copy to persistent context (globalThis),
* Then wrap it into async-IIFE to use top-level await and runInContext to execute in the persistent context.
 *
* parse fails → fall back to the original code (guaranteed not to crash due to instrument). At this time, the top-level const/let is not persistent but can still be executed.
 */
export class IifeContextExecutor implements ReplExecutor {
  async run(
    code: string,
    context: Context,
    signal?: AbortSignal,
    syncTimeoutMs = 5_000,
  ): Promise<unknown> {
    if (signal?.aborted) {
      throw abortReason(signal);
    }
// instrument: Inject globalThis assignment after top-level declaration. If parse fails, fall back to the original state.
    const rewrittenCode = rewriteDynamicImportsForRepl(code);
    const rewrittenParsed = parseReplCode(rewrittenCode);
    const effectiveCode =
      "ast" in rewrittenParsed
        ? instrumentForContextPersistence(rewrittenCode, rewrittenParsed.ast)
        : rewrittenCode;

// Wrapped into async IIFE to support top-level await. Dynamically loaded using injected importModule (see NodeReplSession.buildContext),
// Do not pass importModuleDynamically to avoid triggering the --experimental-vm-modules requirement of vm.
    const wrapped = `(async () => {\n${effectiveCode}\n})()`;
// Promise race can only cancel asynchronous code that has returned control to the event loop; `while(true){}`
// The agent thread will be occupied permanently, so that the outer timeout/AbortSignal timer has no chance to execute.
// The vm's own synchronization execution budget is implemented by V8 interrupt check, which can truly interrupt the synchronization infinite loop. The budget only covers
// In the synchronization segment of runInContext, the asynchronous waiting after await is still responsible for the AbortSignal race.
    const promise = runInContext(wrapped, context, {
      timeout: Math.max(1, Math.trunc(syncTimeoutMs)),
    }) as Promise<unknown>;

    return signal ? await raceAbort(promise, signal) : await promise;
  }
}

function abortReason(signal: AbortSignal): unknown {
//The reason for AbortSignal.timeout is TimeoutError. Rewriting uniformly to AbortError will make
// MCP hard timeout is indistinguishable from user-initiated stop, and accurate kernel reset recovery guidance cannot be given.
  const reason = signal.reason;
  if (reason && typeof reason === "object" && "name" in reason) return reason;
  return new DOMException(typeof reason === "string" ? reason : "aborted", "AbortError");
}

/** Race promise with signal.abort: aborted immediately rejects signal.reason, otherwise the promise result is transparently transmitted. */
function raceAbort(promise: Promise<unknown>, signal: AbortSignal): Promise<unknown> {
  if (signal.aborted) {
    return Promise.reject(abortReason(signal));
  }
  return new Promise<unknown>((resolve, reject) => {
    const onAbort = () => reject(abortReason(signal));
    signal.addEventListener("abort", onAbort, { once: true });
    if (signal.aborted) onAbort();
    promise.then(
      (v) => {
        signal.removeEventListener("abort", onAbort);
        resolve(v);
      },
      (e) => {
        signal.removeEventListener("abort", onAbort);
        reject(e);
      },
    );
  });
}
