/**
* workflow run progress event → additional sink of the parent session (create-app's `onRunEvent` is it).
 *
* Separate files instead of leaving them in the closure of create-app are so that the following three **downgrade paths** can be directly tested individually:
* They all belong to the category of "run is flying and there is a problem on the observation surface". Any one that releases an exception will interrupt a running run.
* The truth about run is in the journal - the progress surface is just an observation surface and should never have the ability to terminate it.
 */

import type { DynamicWorkflowRunProgressPayload, Logger, SessionId } from "@zcode/contracts";
import type { AgentRuntime } from "@zcode/core";

interface DynamicWorkflowRunProgressSinkDeps {
/** Lazy access to the runtime: the run service is built before the runtime is constructed (it is one of the dependencies of the runtime). */
  getRuntime: () => AgentRuntime;
/** The session this app belongs to. The parentSessionId of run must be equal to this, see Identity Gate below. */
  sessionId: SessionId;
  logger?: Logger;
}

/**
* Create a progress sink. The returned function **never throws an exception and never returns a rejected promise**.
 *
* ## Identity gate (load-bearing)
 *
* The event must fall within the session that initiated the run. Today this holds true 1:1 by construction:
 *
 *   ZCodeApp ──1:1── AgentRuntime ──1:1── sessionId        （create-app：`new AgentRuntime(sessionId, …)`）
* └──1:1── run service (this app is built-in, the port cannot be borrowed)
* CreateWorkflow handler is submitted with `parentSessionId = context.sessionId`
 *
* And **only** the top-level runtime of the app can get `dynamicWorkflowRunPort`: the dependent object of the subagent sub-runtime
* (`new AgentRuntime(...)` of `core/src/runtime/methods/subagent.ts`) and workflow actor runtime
* (`script-workflow-child-runtime.ts`) does not **contain** this port (checked verbatim), so the child session cannot be reached at all
* submit——Their CreateWorkflow falls back to the path of "port absence → occupancy diagnosis".
 *
* So `getRuntime()` is the correct runtime. **But if this invariant is not written down, it will be changed by future people**:
* As long as someone adds this port to the dependency of the child runtime, the run initiated by the child session will throw the event into the **parent** session
* transcript - a bug that will not report an error, but will only cause events to appear in the wrong dialogue. So here we explicitly compare identities,
* If not equal, ** do not append ** and record a log with guidance: it is better to lose one projection than to pollute the transcript of another session.
* (If you really want to support sub-session initiating run, you need to find the runtime registry by sessionId, but there is no such thing on the bootstrap side——
* The sub-runtime lives inside the core. That was a change with my own design. )
 */
export function createDynamicWorkflowRunProgressSink(
  deps: DynamicWorkflowRunProgressSinkDeps,
): (progress: DynamicWorkflowRunProgressPayload, routing?: { parentSessionId?: string }) => void {
  const warn = (message: string, runId: string, extra: Record<string, unknown> = {}): void => {
    deps.logger?.warn?.(message, {
      event: "dynamic_workflow.run_progress.append_failed",
      module: "bootstrap.app",
      runId,
      ...extra,
    });
  };

  return (progress, routing) => {
    const parentSessionId = routing?.parentSessionId;
// Absence is legal (submit without parentSessionId): the port of this app can only be reached by this session.
// So absence is equivalent to "this session". Only **explicit wait** is a wiring error.
    if (parentSessionId !== undefined && parentSessionId !== deps.sessionId) {
      warn("Dynamic workflow run progress dropped: parent session is not this app", progress.runId, {
        event: "dynamic_workflow.run_progress.session_mismatch",
        expectedSessionId: deps.sessionId,
        parentSessionId,
        reason: "run_parent_session_not_owned_by_this_app",
      });
      return;
    }

// getRuntime() **throws an error synchronously** when the runtime has not yet been constructed; the closed runtime may also be in the append link
// (Event library/persistence/sink fanout) throw up. Both must degenerate into "a no-op that keeps a log".
    let appended: Promise<void>;
    try {
      appended = deps.getRuntime().recordDynamicWorkflowRunProgress(progress);
    } catch (error) {
      warn("Dynamic workflow run progress append failed", progress.runId, {
        errorMessage: error instanceof Error ? error.message : String(error),
        reason: "runtime_unavailable",
      });
      return;
    }
    void appended.catch((error: unknown) => {
      warn("Dynamic workflow run progress append failed", progress.runId, {
        errorMessage: error instanceof Error ? error.message : String(error),
        reason: "append_rejected",
      });
    });
  };
}
