// ============================================================
// AgentRuntime-backed WorkflowDriver: upgrade question and answer bridge (escalate)
// ============================================================
// workflow-driver.ts reaches the upper limit of oxlint max-lines (400 lines), and combines the four methods of the fourth sequence (upgrade Q&A) - session-level escalate port, qid casting, answer settlement,
// Remove the entire session - split into this file into free functions; only a thin delegate is left on the driver class. The public aspect remains unchanged and remains from
// workflow-driver.ts export.
//
// All touches of the driver state by the four functions are explicitly passed in through {@link EscalationHost} (session table, qid lookup table,
// deps, per-run serial number, dual-track record), this file does not hold any state of its own - the original method body is retained verbatim, only the
// `this.` is replaced with `host.`.

import type {
  EscalateQuestionRequest,
  SessionId,
  WorkflowEscalateOutcome,
  WorkflowEscalatePort,
} from "@zcode/contracts";
import {
  refToString,
  WorkflowError,
  type ActorRef,
  type PersonaSpec,
  type RunEvent,
} from "@zcode/dynamic-workflow";
import {
  ESCALATION_BUDGET_EXHAUSTED,
  MAX_ESCALATIONS_PER_ASK,
  defer,
  effectiveActorName,
  normalizeEscalationContext,
  questionIdFragments,
} from "./workflow-driver-helpers.js";
import type { AgentRuntimeWorkflowDriverDeps, SessionState } from "./workflow-driver-types.js";

/**
* The driver is handed over to the host side of the upgrade bridge. All are **references** (not copies) of the driver's private state: `sessions` /
* `qidToSession` is the two tables in the class, `nextEscalationSeq` increments the per-run sequence number in the class, `record` is
* Dual-track implementation of the class (journal + emit, the order cannot be changed - see the driver's record comment).
 */
export interface EscalationHost {
  readonly deps: AgentRuntimeWorkflowDriverDeps;
  readonly sessions: ReadonlyMap<string, SessionState>;
  readonly qidToSession: Map<string, SessionState>;
/** Get the next per-run monotonous upgrade sequence number (the second segment of qid). */
  nextEscalationSeq(): number;
  record(event: RunEvent): void;
}

/**
* Settlement of a parked upgrade question (run service is called in via the registry). Same family as {@link AgentRuntimeWorkflowDriver.respondToSubmit}:
* Unlock the deferred so that the tool result of `escalate` becomes this answer, and the actor's turn continues in place.
 *
* Returning false means that this driver does not have this qid (the run is wrong, or it has just been canceled by cancelAsk). The registry is calling
* This method has already eliminated the qid before, so there is no need to write back to the registry here - deleting both places will make "Answered" and
* The two exit reasons of "removed" cover each other.
 */
export function respondToParkedEscalation(
  host: EscalationHost,
  qid: string,
  answer: string,
): boolean {
  const state = host.qidToSession.get(qid);
  if (state === undefined) return false;
  const deferred = state.pendingEscalations.get(qid);
  host.qidToSession.delete(qid);
  state.pendingEscalations.delete(qid);
  if (deferred === undefined) return false;
  try {
    host.record({ type: "escalation-resolved", qid, answer });
  } catch (error) {
// The **opposite** choice to the raise path, and the reason is also the opposite: if you can't write it there, no one will see the problem, and parking will only
// Let the actor block permanently, so withdraw and throw up; at this point, the actor is already waiting for this answer, for an observation event
// Keeping it hanging is the worse result. Remember to warn and send the answer accordingly.
    host.deps.logger?.warn?.("Dynamic workflow escalation resolved event not journaled", {
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "dynamic_workflow.escalation.resolved_journal_failed",
      module: "bootstrap.app",
      qid,
      runId: host.deps.runId ?? "run",
    });
  }
  deferred.resolve(answer);
  return true;
}

/**
* Remove all parked upgrade questions and answers on a session: the deferred is rejected (handler is no longer suspended), and the registry entry exits
* (Answering to these qids later yields `run_not_in_flight`).
 *
* Three call points cover all exception paths at the end of ask: `cancelAsk` (the engine actively cancels), `onTurnRejected`
* (turn throws an error by itself - no one is reading the tool results at this time), `startAsk` (changed to the next ask in the same session).
* The normal path does not require it: a parked escalate blocks the turn, and the ask cannot resolve while still owing an answer.
 */
export function withdrawSessionEscalations(host: EscalationHost, state: SessionState): void {
  if (state.pendingEscalations.size === 0) return;
  const entries = [...state.pendingEscalations];
  state.pendingEscalations.clear();
  for (const [qid, deferred] of entries) {
    host.qidToSession.delete(qid);
    host.deps.escalationRegistry.withdraw(qid);
    deferred.reject(
      new WorkflowError("Cancelled", `Escalation ${qid} was cancelled along with its ask.`),
    );
  }
}

/**
* Create a session-level upgrade port: `escalate` handler mid-turn calls it and blocks until the main agent responds.
 *
* Symmetrical to {@link AgentRuntimeWorkflowDriver.makeSubmitPort}. The only difference is that the peer is not an engine but a main agent:
* 1. **Do not report to sink**. Zero changes to the engine core - the entire upgrade process occurs within the boundaries of the driver executing ask (similar to
* repair / nudge round), zero I/O state machine is not aware of it. The event takes two tracks: journal + emit.
* 2. **Docks can have multiple** (key = qid), because the model can issue several escalate calls in parallel in a round.
 *
* Both early exits return **normal tool results** instead of throwing an error: throwing an error when the budget is exhausted will just make the model
* Treat it as a retryable failure, hitting the same wall repeatedly - and the reason for this feature is to eliminate that kind of idling.
 */
export function makeSessionEscalatePort(
  host: EscalationHost,
  sessionId: SessionId,
  actor: ActorRef,
  persona: PersonaSpec,
): WorkflowEscalatePort {
  const actorName = effectiveActorName(persona);
  return {
    escalate: (request: EscalateQuestionRequest): Promise<WorkflowEscalateOutcome> => {
      const state = host.sessions.get(sessionId);
      if (state === undefined || state.currentInstance === undefined) {
// Nothing is flying ask: The question has nowhere to stop. Not parked, not suspended (same argument as submit's guard of the same name).
        return Promise.resolve({
          kind: "refused",
          reason: "no_active_ask",
          message:
            "No ask is in flight, so there is nowhere to park this question and nobody " +
            "would answer it. Escalate only while working on an ask.",
        });
      }
      if (state.escalationsUsed >= MAX_ESCALATIONS_PER_ASK) {
// per-ask upper limit (nudge budget sibling): short circuit starting from the 4th time, never stopping.
        return Promise.resolve({
          kind: "refused",
          reason: "budget_exhausted",
          message: ESCALATION_BUDGET_EXHAUSTED,
        });
      }
      state.escalationsUsed++;

      const qid = mintEscalationQuestionId(host);
      const deferred = defer<string>();
      state.pendingEscalations.set(qid, deferred);
      host.qidToSession.set(qid, state);
      const context = normalizeEscalationContext(request.context);
// The clock **only reads once**, and the event is shared with the dock record: calling Date.now() once in each place will cause the same problem to be reported in the event track
// There are two question times that differ by a few milliseconds from the snapshot, and the downstream needs to use this to calculate "how long to wait."
      const askedAt = Date.now();
      host.deps.escalationRegistry.park(
        {
          qid,
          runId: host.deps.runId ?? "run",
          actor: refToString(actor),
          ...(actorName === undefined ? {} : { actorName }),
          question: request.question,
          ...(context === undefined ? {} : { context }),
          askedAt,
        },
        (answer) => {
          respondToParkedEscalation(host, qid, answer);
        },
      );
      try {
        host.record({
          type: "escalation-raised",
          qid,
          actor,
          ...(actorName === undefined ? {} : { actorName }),
          question: request.question,
          ...(context === undefined ? {} : { context }),
          askedAt,
        });
      } catch (error) {
// If the journal cannot be written, there will be no durable question and answer records, and parking will cause the actor to block indefinitely in a
// On issues that no one can see. Unregistering and handing the error over to the handler is more honest than parking quietly.
        state.pendingEscalations.delete(qid);
        host.qidToSession.delete(qid);
        host.deps.escalationRegistry.withdraw(qid);
        throw error;
      }
      return deferred.promise.then((answer) => ({ kind: "answered", answer, qid }) as const);
    },
  };
}

/**
* Cast a globally unique issue id: `dwfq-<runId fragment>-<seq>`.
 *
* The fragment is for human debugging and identification (semantically opaque - the model only passes it back as a token), seq is per-run monotonic.
* Short fragments (8 characters) may collide with another run's fragment in extreme cases, so candidates are advanced by length, taking the first one
* Not occupied in the registry: **The last candidate is the complete runId**, and "runId unique × per-run monotonic seq" is constructed according to
* No collision, so this loop must terminate on an unoccupied id. All occupied can only mean that two drivers are shared
* Same runId (assembly error), now failing loudly instead of sending an id that would mismatch the answer.
 */
function mintEscalationQuestionId(host: EscalationHost): string {
  const seq = host.nextEscalationSeq();
  for (const fragment of questionIdFragments(host.deps.runId ?? "run")) {
    const candidate = `dwfq-${fragment}-${seq}`;
    if (!host.deps.escalationRegistry.isTaken(candidate)) return candidate;
  }
  throw new WorkflowError(
    "DriverError",
    `Cannot mint a free escalation question id for run ${host.deps.runId ?? "run"} ` +
      `(seq=${seq}): every candidate is already taken in the registry. Two drivers may be ` +
      `sharing one runId.`,
  );
}
