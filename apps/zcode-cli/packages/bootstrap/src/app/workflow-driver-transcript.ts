// ============================================================
// AgentRuntime-backed WorkflowDriver: Transcription orchestration (two driver-private things of amend-resume)
// ============================================================
// workflow-driver.ts reaches the upper limit of oxlint max-lines (400 lines), and arranges the transcription on the driver side——
//Different actor transcription truncation (seedActorSession), ask boundary accounting count (countSessionTranscript) and writeback
// (journalAskMessageBoundary) - split this file into free functions; the public side remains unchanged. The mechanism itself still lives in
// workflow-actor-transcript.ts (copying and counting), here are just the driver’s three calls to it and its failure choices,
// The original method body is retained verbatim, only `this.deps` / `this.journal` is replaced with the deps passed in explicitly.

import type { SessionId } from "@zcode/contracts";
import type { AgentRuntime } from "@zcode/core";
import {
  refToString,
  WorkflowError,
  type ActorSessionSeed,
  type InstanceRef,
} from "@zcode/dynamic-workflow";
import { countActorTranscript, seedActorTranscript } from "./workflow-actor-transcript.js";
import type { AgentRuntimeWorkflowDriverDeps, SessionState } from "./workflow-driver-types.js";

/**
* Different actors' transcription truncation (amend-resume): Copy the first N messages of the source session into the newly cast session, and then let the runtime
* Start with this paragraph above.
 *
* The sequence is **load-sensitive** and cannot be replaced in any of the three steps:
* 1. The factory has returned ⇒ The session row has been dropped into the library (`message.session_id` has an FK for `session(id)`, and the first copy will fail);
* 2. Copy to the **persistence layer** instead of the memory history - the copy that the two readers rely on: the rehydration of this run,
* and **future revisions to this run** (chain walking will read this session as a transcript source);
* 3. Rehydration will take the existing `resumeFromStore` (which is the same machine as resume) and will never create another hydration path.
* At this moment, the session line and message have just been placed, so the `SessionNotFound → New` on the launch side is not copied here.
*'s downgraded branch: The cause of that branch is "session cleared", whereas here it can only mean that the wiring is wrong and it should fail loudly.
 *
* **Conditions** of step 3: if it is really copied, it must be hydrated; no one is copied (idempotent skip) and the journal has recorded the session id.
* Note that the runtime factory has just been re-hanged according to the resume path (launch's attachActorSession) and rehydrated again
* Only one more SessionResumed will be sent and one more SessionStart hook will be run. The case where neither condition is true (skip copying
* And the journal has no records = the previous life collapsed between copying and putActor) still needs to be hydrated, otherwise the runtime will bring a full
* The message session starts from an empty context.
 */
export async function seedActorSession(
  deps: AgentRuntimeWorkflowDriverDeps,
  input: {
    journaledSessionId: string | undefined;
    runtime: AgentRuntime;
    seed: ActorSessionSeed;
    sessionId: SessionId;
  },
): Promise<void> {
  const { journaledSessionId, runtime, seed, sessionId } = input;
  const store = deps.actorTranscriptStore;
  if (store === undefined) {
    throw new WorkflowError(
      "DriverError",
      `Subagent session ${sessionId} carries a transcript seed, but the driver has no ` +
        `transcript store (wiring error).`,
    );
  }
  const copied = await seedActorTranscript({ seed, store, targetSessionId: sessionId });
  if (copied === undefined && journaledSessionId !== undefined) return;
  await runtime.resumeFromStore();
}

/** Counts the number of messages that have been dropped in this session; if failed, the accounting will be abandoned (the boundary is absent, not a wrong boundary). */
export async function countSessionTranscript(
  deps: AgentRuntimeWorkflowDriverDeps,
  state: SessionState,
  instance: InstanceRef,
): Promise<number | undefined> {
  const store = deps.actorTranscriptStore;
  if (store === undefined) return undefined;
  try {
    return await countActorTranscript(store, state.sessionId);
  } catch (error) {
    deps.logger?.warn?.("Dynamic workflow ask message boundary count failed", {
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "dynamic_workflow.ask.message_boundary.count_failed",
      instance: refToString(instance),
      module: "bootstrap.app",
      sessionId: state.sessionId,
    });
    return undefined;
  }
}

/**
* Write the message number boundary into the journal line of this ask (`NodeRecord.messageBoundary`).
 *
* Same family as stats backfill**: the record written by the engine during settlement does not contain this field (it is a fact owned by the driver), so here
* Use the same set of read, modify and write - `getNode` to get the entire record that has just been settled, only add the boundary and then `putNode`, status / result /
* actorSeq / inputHash / stats one does not move. It is necessary to write after the settlement: the engine's settlement is the entire replacement, written after
* It will be erased before.
 *
* Write every ask, without distinguishing between named/anonymous, revision/normal run: **Anonymous or not is determined only when imported**, and any run
* are potential precursors to future revisions. The cost is one small write per ask.
 *
* There are two reasons for early exit: the record is not there (the run has been cleared) and there is nowhere to write; `currentInstance` has been replaced to indicate this session
* The next ask has already started, and the length counted at this moment no longer belongs to this exchange - rather let the boundary be absent (this ask cannot be imported),
* Don't write a larger value (truncation will add an extra paragraph to the opening of the next ask).
 */
export function journalAskMessageBoundary(
  deps: AgentRuntimeWorkflowDriverDeps,
  state: SessionState,
  instance: InstanceRef,
  boundary: number,
): void {
  const key = refToString(instance);
  if (state.currentInstance !== undefined && refToString(state.currentInstance) !== key) return;
  const runId = deps.runId ?? "run";
  try {
    const recorded = deps.journal.getNode(runId, instance.siteId, instance.ordinal);
    if (recorded === undefined) return;
    deps.journal.putNode({ ...recorded, messageBoundary: boundary });
  } catch (error) {
// Failure in accounting writing should not trigger an already settled ask (and this is running on a free promise, the throw will only become
// unhandled rejection). The cost is the same as counting failure: the ask is not importable, and run is therefore not a precursor.
    deps.logger?.warn?.("Dynamic workflow ask message boundary write failed", {
      errorMessage: error instanceof Error ? error.message : String(error),
      event: "dynamic_workflow.ask.message_boundary.write_failed",
      instance: key,
      module: "bootstrap.app",
      runId,
    });
  }
}
