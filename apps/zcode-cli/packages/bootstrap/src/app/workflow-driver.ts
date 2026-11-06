// ============================================================
// AgentRuntime-backed WorkflowDriver（Boundary B）
// ============================================================
//Put the downward side effect port WorkflowDriver of the dynamic-workflow engine core onto the real ZCode AgentRuntime:
// One **persistent** child runtime per actor (repeat executeTurn to accumulate messageHistory), once per ask
// executeTurn, the results of typed ask are bridged back to the engine for judgment via the session-level WorkflowSubmitPort.
//
// The other two halves of Boundary B - `executeWorldRead` and `executeArtifactPublish` - live in
// workflow-world-read.ts and workflow-artifact-publish.ts, this file is only forwarded. There is neither here nor there
// Common state (does not touch session, turn, submit bridge), so after separation, only "session and turn arrangement" remain in this document
// This thing.
//
//Three timings (core invariants of this document):
// 1. accept: model calls submit_result → handler blocks in port.respond → reported by this driver
// askSubmitAttempted → Engine verification passed → respondToSubmit(accept) → Unlock deferred to {accept:true}
// → handler returns success (hang turnControl to stop turn) → executeTurn resolve → only report askStats (no longer report
// askTurnEnded because the ask has been resolved by the engine).
// 2. reject (same as repair in turn): respondToSubmit(reject, violations) → Unlock deferred as
// {accept:false, violations} → handler throws an error → error tool_result (no turnControl) → continue with the same turn
// → model retry → respond again → deferred again. The repair engine changes cancelAsk when the budget is exhausted.
// 3. nudge (turn ends but is not submitted): executeTurn resolve and not accepted → report askTurnEnded → engine decides nudge
// → respondToSubmit(nudge) (**no** parked deferred at this time, turn has ended) → initiated on the same persistent runtime
// A **new** executeTurn (nudge prompt).
//
//Three-value→two-value ruling mapping (engine SubmitVerdict three-value; contracts WorkflowSubmitPort two-value):
// accept → {accept:true}; reject → {accept:false, violations} (Violation 1:1 mapping); nudge → no deferred,
// Start a new turn. See respondToSubmit.
//
//The fourth sequence (upgrade Q&A) is on the same level as the above three:
// 4. escalate: model calls escalate → handler blocks in escalatePort → driver casts qid, stops deferred,
//Register the problem into the upgrade registry and issue escalation-raised (journal + emit) in two ways → the main agent runs
// Service's resolveQuestion lookup table → driver.respondToEscalation unwraps the deferred and issues it
// escalation-resolved → tool result = answer text → actor's turn continues in place, ask settles as usual.
// The engine core has **zero awareness** of this: the upgrade occurs within the boundaries of the driver execution ask (same layer as the repair/nudge round),
// Don't write the dwf_node line. The escape hatch is cancelAsk - it rejects along with the parked upgrade deferred.
//
// amend-resume adds two driver-private things to this file, both of which only
// It is related to session transcription, so the mechanism lives in workflow-actor-transcript.ts, and only orchestration is done here (the three implementations of orchestration are in
// workflow-driver-transcript.ts）：
// - **ask boundary accounting**: After an exchange (including the repair / nudge round and the closing message after submit), the actor
//The number of messages that the session has been dropped into the database is written into the journal line of this ask. Every ask is written - any run is for future revisions
// Potential precursor.
// - **Transcription truncation**: When `createActorSession` is seeded, copy the first N messages of the source session into the newly cast session and then rehydrate.

import type {
  SessionId,
  SubmitResultRequest,
  SubmitVerdict as ContractsSubmitVerdict,
  WorkflowEscalatePort,
  WorkflowSubmitPort,
} from "@zcode/contracts";
import type { TurnResult } from "@zcode/core";
import {
  GENERIC_SUBMIT_PROFILE,
  refToString,
  WorkflowError,
  type ActorRef,
  type ActorSessionSeed,
  type ArtifactPublishRequest,
  type ArtifactVersionRecord,
  type AskMessage,
  type InstanceRef,
  type JournalStorePort,
  type PersonaSpec,
  type RunEvent,
  type SessionRef,
  type SubmitVerdict as EngineSubmitVerdict,
  type WorkflowDriver,
  type WorkflowReportSink,
  type WorldReadOp,
} from "@zcode/dynamic-workflow";
import { executeArtifactPublish } from "./workflow-artifact-publish.js";
import { qualityEpilogue } from "./workflow-ask-epilogue.js";
import { ensureSubmitProfileFits } from "./workflow-driver-submit-profile.js";
import { executeWorldRead } from "./workflow-world-read.js";
import {
  createActorModelActivity,
  createRunStallClock,
  type RunStallClock,
} from "./workflow-driver-concurrency.js";
import { handleModelTurnFailure, type ModelFailureHost } from "./workflow-driver-model-failure.js";
import {
  makeSessionEscalatePort,
  respondToParkedEscalation,
  withdrawSessionEscalations,
  type EscalationHost,
} from "./workflow-driver-escalation.js";
import {
  NUDGE_PROMPT,
  TYPED_TOOL_EPILOGUE,
  defer,
  effectiveActorName,
  isTurnCancelled,
  mapViolations,
  mintActorSessionId,
  rejectWith,
  reportTurnObservations,
  schemaEpilogue,
  toWorkflowError,
} from "./workflow-driver-helpers.js";
import {
  countSessionTranscript,
  journalAskMessageBoundary,
  seedActorSession,
} from "./workflow-driver-transcript.js";
import type { AgentRuntimeWorkflowDriverDeps, SessionState } from "./workflow-driver-types.js";

/**
* Real implementation of WorkflowDriver. Constructed via {@link createAgentRuntimeWorkflowDriver} (bind deps, backfill sink).
 */
class AgentRuntimeWorkflowDriver implements WorkflowDriver {
  readonly journal: JournalStorePort;
  readonly emit: (event: RunEvent) => void;

  private readonly sink: WorkflowReportSink;
  private readonly deps: AgentRuntimeWorkflowDriverDeps;
  private readonly sessions = new Map<string, SessionState>();
/** refToString(instance) → The session it belongs to, for respondToSubmit / cancelAsk to check back. */
  private readonly instanceToSession = new Map<string, SessionState>();
/** qid → Park its session for respondToEscalation to check (same family as instanceToSession). */
  private readonly qidToSession = new Map<string, SessionState>();
/** per-run monotonous upgrade sequence number; the second segment of qid. Combined with runId to form a collision-free id. */
  private escalationSeq = 0;
/** dispose idempotent gate (the engine contract is exactly once, but the gate is cheaper and more stable here). */
  private disposed = false;
/** This run subscribes to manager cap changes (once at run level, unsubscribes at dispose). */
  private readonly concurrencyUnsubscribe?: () => void;
  /**
* Handed over to the host side of the upgrade bridge (workflow-driver-escalation.ts): two tables are shared by reference, serial number and record
* Return to this class through closure - private state is not exposed, and the bridge function does not need to know the shape of the class.
   */
  private readonly escalationHost: EscalationHost;
/** Handed over to the host side of model-side failure containment (workflow-driver-model-failure.ts): the same set of ideas for sharing by reference. */
  private readonly modelFailureHost: ModelFailureHost;
/** run-level stall clock: success/retry beats of all actors are pooled into this clock. */
  private readonly stallClock: RunStallClock;

  constructor(deps: AgentRuntimeWorkflowDriverDeps, sink: WorkflowReportSink) {
    this.deps = deps;
    this.sink = sink;
    this.journal = deps.journal;
    this.emit = deps.emit;
    this.escalationHost = {
      deps,
      sessions: this.sessions,
      qidToSession: this.qidToSession,
      nextEscalationSeq: () => ++this.escalationSeq,
      record: (event) => this.record(event),
    };
    this.modelFailureHost = {
      deps,
      sink,
      isDisposed: () => this.disposed,
      runTurn: (state, instance, input, epilogueStart) =>
        this.runTurn(state, instance, input, epilogueStart),
    };
    this.stallClock = createRunStallClock({
      ...(deps.clock?.now === undefined ? {} : { now: deps.clock.now }),
      ...(deps.clock?.schedule === undefined ? {} : { schedule: deps.clock.schedule }),
      ...(deps.clock?.stallAfterMs === undefined ? {} : { afterMs: deps.clock.stallAfterMs }),
      onStalled: (info) => this.sink.runStalled(info),
    });
    if (deps.concurrency !== undefined) {
// Fanout only goes to runs that have in-flight/queued requests on that key, so the subscription itself can be done in one go at construction time.
      this.concurrencyUnsubscribe = deps.concurrency.subscribe(deps.runId ?? "run", (change) => {
        this.stallClock.noteCap(change.next);
        this.sink.concurrencyChanged(change);
      });
    }
  }

  async createActorSession(
    actor: ActorRef,
    persona: PersonaSpec,
    seed?: ActorSessionSeed,
  ): Promise<SessionRef> {
    const sessionId = mintActorSessionId(this.deps.runId ?? "run", actor);
// resume session identity mutual authentication: journal's dwf_actor.session_id is a **record**, and the casting function is authoritative
// ((runId, actorRef) is purely deterministic, and the same id will be cast when re-hanging). The inconsistency between the two can only be due to casting rules
// Drift (renamed/second implementation) - the consequence is that the rehydration reads the wrong session and the details page opens a non-existent session, which is far from the cause.
// So the first step of rehanging fails loudly with a structured mismatch (the same shape as the two hash mismatch errors).
    const journaled = this.journal.getActor(
      this.deps.runId ?? "run",
      actor.siteId,
      actor.ordinal,
    )?.sessionId;
    if (journaled !== undefined && journaled !== sessionId) {
      throw new WorkflowError(
        "DriverError",
        `Subagent session identity mismatch for ${refToString(actor)}: the journaled session ` +
          `id and the minted one differ.`,
        { mismatch: { expected: journaled, got: sessionId } },
      );
    }
    const ref: SessionRef = { id: sessionId };
// Session-level submit port: closure is bound to this session, and the model cannot override the routing identity (instance is taken from currentInstance).
    const submitPort = this.makeSubmitPort(sessionId);
//Upgrade port isomorphism: also bound by session closure, also constant injection (see field comments of ActorRuntimeFactory).
// Once persona is merged into closure: the effective name is **frozen** (set by the engine during createActor and remains unchanged thereafter),
// So counting it once here is cheaper than checking it every time it escalates, and the name will not be changed midway.
    const escalatePort = this.makeEscalatePort(sessionId, actor, persona);
// Model active surface: The access port is downloaded with the runtime deps (the runner tries to pass the gate first every time),
// waiting / executing observations are only reported when there is an ask in flight and it has not been resolved/cancelled by the engine - a late observation is very important to a
// The completed ask is meaningless, and the engine side will block it again. The state is built below, so it is bound late through closure.
    let state: SessionState | undefined;
    const live = (): InstanceRef | undefined =>
      state === undefined ||
      state.currentInstance === undefined ||
      state.accepted ||
      state.cancelled
        ? undefined
        : state.currentInstance;
    const modelActivity = createActorModelActivity({
      port: this.deps.concurrency,
      runId: this.deps.runId ?? "run",
      live,
      handlers: {
// First workspace write for subagent ⇒ Engine off import cache.
        onMutating: (instance) => this.sink.askMutating(instance),
        onWaiting: (info) => {
          const instance = live();
          if (instance !== undefined) this.sink.askWaiting(instance, info);
        },
        onExecuting: () => {
          const instance = live();
          if (instance !== undefined) this.sink.askExecuting(instance);
        },
//Two beats of the run-level stall clock: any actor's successful reset to zero, any retry to load.
        onRequestCompleted: () => this.stallClock.noteSuccess(),
        onRetryScheduled: (reason) => this.stallClock.noteRetryScheduled(reason),
      },
    });
//submit profile: Check by actor **site** - the same site
// Each ordinal (each lane of fan-out) runs the same set of ask sites, and the profiles are naturally the same. absent = generic.
    const submitProfile =
      this.deps.actorSubmitProfiles?.get(actor.siteId) ?? GENERIC_SUBMIT_PROFILE;
// await: The production factory drops the session into the database and creates a task link before returning (FK requires the session line to exist first).
// The engine's ensureSession will await this method, so persistence is completed before the first ask is dispatched.
    const runtime = await this.deps.runtimeFactory({
      sessionId,
      actor,
      persona,
      submitPort,
      submitProfile,
      escalatePort,
      ...(seed === undefined ? {} : { seed }),
      ...(modelActivity.admission === undefined
        ? {}
        : { modelRequestAdmission: modelActivity.admission }),
    });
    if (seed !== undefined) {
      await seedActorSession(this.deps, { journaledSessionId: journaled, runtime, seed, sessionId });
    }
    state = {
      ref,
      sessionId,
      runtime,
      submitProfile,
      currentTyped: false,
      accepted: false,
      cancelled: false,
      turnGeneration: 0,
      pendingEscalations: new Map(),
      escalationsUsed: 0,
      modelActivity,
      actor,
      actorName: effectiveActorName(persona),
      transientAttempts: 0,
    };
    modelActivity.observe(runtime, sessionId);
    this.sessions.set(sessionId, state);
    return ref;
  }

  startAsk(session: SessionRef, instance: InstanceRef, message: AskMessage): void {
    const state = this.sessions.get(session.id);
    if (state === undefined) {
// The theory will not happen (the engine creates a session first and then ask); it is normalized to report DriverError instead of silently.
      this.sink.askFailed(
        instance,
        new WorkflowError("DriverError", `Unknown subagent session: ${session.id}`),
      );
      return;
    }
// If the previous ask left a stop on the abnormal path (the turn was interrupted by a method other than abort), remove it here:
// Once the new ask is run, no one will read the answers to those questions. Staying in the registry will only let the snapshot lie.
    this.withdrawEscalations(state);
    state.currentInstance = instance;
    state.currentTyped = message.typed;
    state.accepted = false;
    state.cancelled = false;
    state.pendingSubmit = undefined;
// per-ask budget is reset to zero (nudge takes runTurn, not through here - nudge is still in the same ask).
    state.escalationsUsed = 0;
    state.abortController = new AbortController();
// The waiting / executing phase and tool count of the previous ask cannot be brought to this ask; the same applies to the transient redrive count.
    state.modelActivity.reset();
    state.transientAttempts = 0;
    state.cancelRedrive?.();
    state.cancelRedrive = undefined;
    this.instanceToSession.set(refToString(instance), state);

// Quality endnotes treat typed / untyped equally; schema endnotes are only available in typed. Both sections are calculated in the scheduler.
// Append after inputHash, so the identity is not cached.
// The schema endnotes of typed ask are forked according to the submit profile: the schema of the mono subagent is already in the tool declaration, and only the endnotes are left
// One sentence; the generic subagent still writes the entire schema into the endnote. The guard runs first: when the static profile does not match this ask, it will
// Reduce the session to generic (or make ask fail), and what you read below is the corrected form.
    if (message.typed && !ensureSubmitProfileFits(this.deps, this.sink, state, instance, message)) {
      return;
    }
    const input = message.typed
      ? `${message.instructions}${qualityEpilogue(message.schema)}${
          state.submitProfile.kind === "mono" ? TYPED_TOOL_EPILOGUE : schemaEpilogue(message.schema)
        }`
      : `${message.instructions}${qualityEpilogue(undefined)}`;
// Endnote boundary: GUI wraps endnotes into disclosures based on this. Instruction text
// After that, it is all engine text, and the boundary is the length of the text; the model still receives the full text, as well as the persistent text part.
// fire-and-forget: Never await turn completion within startAsk (Boundary B contract).
    this.runTurn(state, instance, input, message.instructions.length);
  }

  respondToSubmit(instance: InstanceRef, verdict: EngineSubmitVerdict): void {
    const state = this.instanceToSession.get(refToString(instance));
    if (state === undefined) return;
    switch (verdict.kind) {
      case "accept": {
// Mark accept: AskTurnEnded will no longer be reported when the ask's turn resolves (the engine has settledOk).
        state.accepted = true;
        const deferred = state.pendingSubmit;
        state.pendingSubmit = undefined;
        deferred?.resolve({ accept: true });
        return;
      }
      case "reject": {
// Fixed in the same turn: handler will throw an error when receiving {accept:false} → error tool_result → model retry.
        const deferred = state.pendingSubmit;
        state.pendingSubmit = undefined;
        deferred?.resolve({ accept: false, violations: mapViolations(verdict.violations) });
        return;
      }
      case "nudge": {
// The turn has ended and there is no parked deferred: initiate a new turn on the same persistent runtime to promote its submission.
// The entire nudge is engine text: border 0.
        this.runTurn(state, instance, NUDGE_PROMPT, 0);
        return;
      }
    }
  }

  cancelAsk(instance: InstanceRef): void {
    const state = this.instanceToSession.get(refToString(instance));
    if (state === undefined) return;
// Engine proactive cancellation (repair/nudge budget exhausted, run canceled/failed): abort the in-flight turn and unblock the possible pending submit
// deferred, to avoid permanent blocking of the handler; mark canceled so that turn reject will no longer report askFailed.
    state.cancelled = true;
    const deferred = state.pendingSubmit;
    state.pendingSubmit = undefined;
    deferred?.reject(new WorkflowError("Cancelled", "The ask was cancelled by the engine."));
// The parked upgrade Q&A and submit deferred **same treatment**: rejected together, otherwise the `escalate` handler will be in one
// Permanently blocked in a canceled ask. This is the escape hatch of "no answer = indefinite blocking" (no timeout by design):
// The behavior of run cancel and CLI process death is therefore consistent with today's byte by byte - ask rejects Canceled, run transfers
// Interrupted, after resume, the ask will be run again, the actor will ask again and get the new qid.
    this.withdrawEscalations(state);
// Back off the waiting redrive and remove it at the same time: the ask has been resolved by the engine, and another round will only burn tokens on a node that no one listens to.
    state.cancelRedrive?.();
    state.cancelRedrive = undefined;
    state.abortController?.abort(new Error("workflow ask cancelled"));
  }

  /**
* Resource release after run settlement (the engine is adjusted exactly once after run-settled, see WorkflowDriver.dispose).
   *
* For each actor runtime, run the app to close the session in the same chain - `closeBrowserSession` internally in sequence
* beginShutdown, node_repl session release, browser session closure; if you don't create another set of sub-agent closure chain, it will drift.
* Don't care about execution/MCP/session store: subagents don't own them. If there is a session in flight, wait for it to land and then close it.
* (see SessionState.turn); only warn when closing fails, settlement will not be thrown due to it. The three tables are then cleared.
   */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.stallClock.dispose();
    for (const state of this.sessions.values()) {
      state.modelActivity.unsubscribe();
      state.cancelRedrive?.();
      state.cancelRedrive = undefined;
      const close = (): void => this.closeActorRuntime(state);
      if (state.turn === undefined) close();
      else state.turn.then(close, close);
    }
    this.concurrencyUnsubscribe?.();
    this.sessions.clear();
    this.instanceToSession.clear();
    this.qidToSession.clear();
  }

  private closeActorRuntime(state: SessionState): void {
// Promise.resolve().then(...): Put synchronization throws into the same warn path (the minimum stub runtime does not have this method).
    void Promise.resolve()
      .then(() => state.runtime.closeBrowserSession())
      .catch((error: unknown) => {
        this.deps.logger?.warn?.("Dynamic workflow actor runtime close failed", {
          errorMessage: error instanceof Error ? error.message : String(error),
          event: "dynamic_workflow.actor_runtime.close_failed",
          module: "bootstrap.app",
          sessionId: state.sessionId,
        });
      });
  }

  /**
* Settlement of a parked upgrade question (run service is called in via the registry). realize the body
* workflow-driver-escalation.ts ({@link respondToParkedEscalation}), only delegation is done here.
   */
  respondToEscalation(qid: string, answer: string): boolean {
    return respondToParkedEscalation(this.escalationHost, qid, answer);
  }

/** Remove all parked upgrade questions and answers on a session; implement {@link withdrawSessionEscalations}. */
  private withdrawEscalations(state: SessionState): void {
    withdrawSessionEscalations(this.escalationHost, state);
  }

  /**
* World read: The entire section is delegated to {@link executeWorldRead} (workflow-world-read.ts).
   *
* This is only forwarding because the world read has no state in common with the rest of this file: it does not touch actor sessions,
* No need to touch turn or submit bridge, only three things are needed (two ports plus a cwd). After the separation, this document
* Only the "arrangement of sessions and turns" is left, and the op element, upper limit execution, and git's fixed argv are concentrated in one place.
   */
  async executeWorldRead(op: WorldReadOp, args: unknown[]): Promise<unknown> {
    return await executeWorldRead(this.deps, op, args);
  }

  /**
* Publishing of user interface products: The entire section is entrusted to {@link executeArtifactPublish} (workflow-artifact-publish.ts),
* The reason is the same as world read verbatim above - it has no state in common with this file, just requires two ports, a cwd and
* A session id.
   *
* The method is **optional** on Boundary B (`executeArtifactPublish?`), and this driver always implements it:
* "Is there storage" is an assembly fact, expressed by `artifactStore` in deps and fails loudly on that side, which should not be
* "Is the method present?" This second channel is expressed again (two channels will give the same thing two failure modes).
   */
  async executeArtifactPublish(request: ArtifactPublishRequest): Promise<ArtifactVersionRecord> {
    return await executeArtifactPublish(this.deps, request);
  }

// ————————————————————————————————— Internal: turn arrangement ——————————————————————————————

  private runTurn(
    state: SessionState,
    instance: InstanceRef,
    input: string,
    epilogueStart: number,
  ): void {
    const abortSignal = state.abortController?.signal;
    state.turnGeneration++;
    state.turn = state.runtime
      .executeTurn(input, undefined, {
        ...(abortSignal ? { abortSignal } : {}),
        epilogueStart,
      })
      .then(
        (result) => this.onTurnResolved(state, instance, result),
        (error) => this.onTurnRejected(state, instance, error),
      );
  }

  private onTurnResolved(state: SessionState, instance: InstanceRef, result: TurnResult): void {
// The two reports parsed by a turn (progress precedes usage), the order and load are in reportTurnObservations.
    reportTurnObservations(this.sink, state, instance, result);
    if (this.deps.actorTranscriptStore === undefined) {
// No transcription access plane: the original synchronization path, not a single await is needed (boundary accounting is completely absent, see the deps field comment).
      this.reportTurnOutcome(state, instance, result);
      return;
    }
    void this.settleExchange(state, instance, result);
  }

  /**
* Report the outcome of a turn and answer "Is this the end of the ask exchange?"
   *
* There is only one way beyond accept: give the final text to the engine (typed → nudge or exhaustion failure; untyped → settle accordingly).
* When nudge, the engine will start a new turn in **this call stack** via respondToSubmit, so turnGeneration changes——
* That is the criterion for "the exchange has not yet ended" (repair rounds are not included in this list: they are in the same turn, and this method will not be called at all).
   */
  private reportTurnOutcome(
    state: SessionState,
    instance: InstanceRef,
    result: TurnResult,
  ): boolean {
// Submitted and accepted by the engine: ask has been settled, and the end of turn is just confirmation, and askTurnEnded will no longer be reported.
    if (state.accepted) return true;
    const generation = state.turnGeneration;
    this.sink.askTurnEnded(instance, result.response);
    return state.turnGeneration === generation;
  }

  /**
* The end of an exchange: count messages → report the final result → when the exchange is really over, write the boundary into the journal line of ask.
   *
* **Count first and then report**, the order is payload-based: after reporting, the engine may immediately dispatch the next actor of this actor on the same session.
* ask (per-actor FIFO only guarantees serialization and does not guarantee gaps between them), that round of messages will fall into the same session, and this time
* The count is extended. Count it down first, and what you read is the length at the end of this exchange.
   */
  private async settleExchange(
    state: SessionState,
    instance: InstanceRef,
    result: TurnResult,
  ): Promise<void> {
    const boundary = await countSessionTranscript(this.deps, state, instance);
    const ended = this.reportTurnOutcome(state, instance, result);
    if (!ended || boundary === undefined) return;
    journalAskMessageBoundary(this.deps, state, instance, boundary);
  }

  private onTurnRejected(state: SessionState, instance: InstanceRef, error: unknown): void {
// If turn dies, no one will read the tool results anymore: the parked upgrade questions and answers must be removed together, otherwise they will remain there forever
// In the pendingQuestions of the snapshot, ask the main agent to answer a question that has no audience.
    this.withdrawEscalations(state);
    if (state.cancelled || isTurnCancelled(error)) {
// Engine-initiated cancellation (abort): The engine has settled the ask, and the driver will not report it again.
      return;
    }
// Model side errors are contained in workflow-driver-model-failure.ts (strategy table judgment stop / context_exceeded /
// Transient redrive); it is not a model layer error but a driver side failure.
    if (handleModelTurnFailure(this.modelFailureHost, state, instance, error)) return;
    this.sink.askFailed(instance, toWorkflowError(error));
  }

// ——————————————————————————————— Internal: submit bridge ————————————————————————————

/** Create a session-level submit port: submit_result handler mid-turn calls it and blocks waiting for the decision. */
  private makeSubmitPort(sessionId: SessionId): WorkflowSubmitPort {
    return {
      respond: (request: SubmitResultRequest): Promise<ContractsSubmitVerdict> => {
        const state = this.sessions.get(sessionId);
        const instance = state?.currentInstance;
        if (state === undefined || instance === undefined) {
// There is no request but submit is received: it is not routed to the engine and rejected directly (to avoid hanging).
          return Promise.resolve(rejectWith("no active ask is awaiting a submitted result"));
        }
// Untyped ask guard: "Fully untyped actors do not register submit_result" by design.
//But the driver cannot get the actor's aggregated typed information when creatingActorSession (site graph is required and is not transparently transmitted), so
// Always register. To not rely on the engine's behavior of "submitAttempted retiring early on untyped" (which would make the deferred hang permanently),
// This is intercepted directly inside the driver: untyped ask immediately returns a synthetic rejection when receiving submit, allowing the model to use plain text instead.
        // 绝不上报 askSubmitAttempted。后续版本可据 actor-graph 投影把 per-actor typed 信息透传进来，
        // 真正在 untyped-only actor 上跳过注册（关系到 prompt-cache 的 frozen-tools 不变式）。
        if (!state.currentTyped) {
          return Promise.resolve(
            rejectWith(
              "this ask does not accept submit_result; provide your answer as your final message",
            ),
          );
        }
        // 单前实例不变式：至多一个挂起 deferred。若已有（不应发生），先拒旧的避免泄漏。
        state.pendingSubmit?.reject(
          new WorkflowError("DriverError", "This submit was superseded by a newer submit."),
        );
        const deferred = defer<ContractsSubmitVerdict>();
        state.pendingSubmit = deferred;
        // 同步上报：引擎在本调用栈内校验并经 respondToSubmit 回裁决（同步解开 deferred）。
        this.sink.askSubmitAttempted(instance, request.result);
        return deferred.promise;
      },
    };
  }

  // ——————————————————————————————— 内部：升级问答桥接 ———————————————————————————————

  /**
   * 造一个会话级升级端口；实现体在 workflow-driver-escalation.ts（{@link makeSessionEscalatePort}），
   * 与 {@link makeSubmitPort} 逐条对称的论证也写在那边。
   */
  private makeEscalatePort(
    sessionId: SessionId,
    actor: ActorRef,
    persona: PersonaSpec,
  ): WorkflowEscalatePort {
    return makeSessionEscalatePort(this.escalationHost, sessionId, actor, persona);
  }

  /**
   * 一条 driver 侧事件的双轨落地：durable 进 `dwf_event`，实时经 emit 扇出。
   *
   * 与引擎的 `record()`（engine.ts）逐字节同形，且顺序不可换：launch 侧的 sequence 截取靠
   * 「appendEvent 之后同步紧接着 emit、同一个事件对象引用」这个前提把刚分配到的 sequence 交给
   * emit（dynamic-workflow-run-launch.ts 的 createJournalSequenceCapture）。
   */
  private record(event: RunEvent): void {
    this.journal.appendEvent(this.deps.runId ?? "run", event);
    this.emit(event);
  }
}

/**
 * 造一个绑定了 deps 的 driver 工厂，直接充当 harness 的 makeDriver。journal 与 emit 由 deps 提供，
 * 调用方（测试/生产）持有其引用以做断言与 Boundary C 扇出。
 */
export function createAgentRuntimeWorkflowDriver(
  deps: AgentRuntimeWorkflowDriverDeps,
): (sink: WorkflowReportSink) => WorkflowDriver {
  return (sink) => new AgentRuntimeWorkflowDriver(deps, sink);
}

export { mintActorSessionId } from "./workflow-driver-helpers.js";
export type { ActorRuntimeFactory } from "./workflow-driver-types.js";
