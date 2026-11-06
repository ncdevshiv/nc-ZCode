//! The runtime state of one ask and its turn orchestration (port of runTurn /
//! onTurnResolved / onTurnRejected / the submit bridge in `workflow-driver.ts`).
//!
//! Three sequences (the core invariants of this module):
//! 1. **accept**: the model calls `submit_result` → the tool puts the sender into pending → reports
//!    `ask_submit_attempted` → the engine returns its verdict through `respond_to_submit` in the same call stack → the sender unwraps →
//!    the tool returns success and sets the session cancel flag (a host turn has no `turnControl`; the cancel flag is the existing mechanism
//!    for "do not start another round") → the turn closes as completed → only stats are reported, `ask_turn_ended` is not reported again.
//! 2. **reject (repair within the same turn)**: the sender receives `Reject{violations}` → the tool returns an error
//!    tool_result (the model can repair item by item) → the same turn continues → the model retries.
//! 3. **nudge (turn ended without submitting)**: the turn closes as completed → reports `ask_turn_ended` → the engine, in the
//!    **same call stack**, calls `respond_to_submit(Nudge)` → this module sets the nudge flag → the driver loop starts one more round
//!    on the same persistent session (the nudge prompt).
//!
//! The fourth sequence (the escalation Q&A) lives in `escalate`: the tool parks in the registry waiting for the main agent to answer, and the ask's round continues in place.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use nc_code_workflow::types::{
    ActorRef, AskLastTool, AskProgress, AskStats, InstanceRef, SessionRef, SubmitVerdict,
    WorkflowError, WorkflowErrorCode,
};
use serde_json::{json, Value};

use crate::provider_config::ProviderConfig;
use crate::session::Session;
use crate::tools::{ToolError, ToolErrorCode};
use crate::turn::{self, TurnDeps, TurnState};

use crate::workflow::actor_tools::{actor_label, EscalationRegistry, PendingQuestion};
use crate::workflow::driver::DriverInner;
use crate::workflow::failure::{classify_turn_failure, TurnFailure};
use crate::workflow::prompt::{
    self, ESCALATE_CANCELLED, ESCALATION_BUDGET_EXHAUSTED, ESCALATION_NO_ACTIVE_ASK,
    MAX_ESCALATIONS_PER_ASK, NUDGE_PROMPT, SUBMIT_CANCELLED, SUBMIT_NO_ACTIVE_ASK,
    SUBMIT_UNTYPED_ASK,
};

type SubmitSender = tokio::sync::oneshot::Sender<Result<SubmitVerdict, WorkflowError>>;

/// All observable state of one ask. per-actor FIFO (guaranteed by the engine) → at most one in-flight ask per session,
/// so accepted / cancelled / pending_submit all have "single prior instance" semantics and need no per-instance breakdown.
pub(crate) struct AskState {
    pub(crate) inner: Weak<DriverInner>,
    pub(crate) instance: InstanceRef,
    pub(crate) session_id: String,
    pub(crate) actor: ActorRef,
    pub(crate) actor_name: Option<String>,
    typed: bool,
    child: Option<Arc<Session>>,
    escalations: Arc<EscalationRegistry>,
    accepted: AtomicBool,
    cancelled: AtomicBool,
    nudged: AtomicBool,
    mutating_reported: AtomicBool,
    escalations_used: AtomicU32,
    pending_submit: Mutex<Option<SubmitSender>>,
    tool_calls: AtomicU64,
    world_tool_calls: AtomicU64,
    turns: AtomicU64,
    last_tool: Mutex<Option<AskLastTool>>,
}

impl AskState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        inner: Weak<DriverInner>,
        instance: InstanceRef,
        session_id: String,
        actor: ActorRef,
        actor_name: Option<String>,
        typed: bool,
        child: Arc<Session>,
        escalations: Arc<EscalationRegistry>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            instance,
            session_id,
            actor,
            actor_name,
            typed,
            child: Some(child),
            escalations,
            accepted: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            nudged: AtomicBool::new(false),
            mutating_reported: AtomicBool::new(false),
            escalations_used: AtomicU32::new(0),
            pending_submit: Mutex::new(None),
            tool_calls: AtomicU64::new(0),
            world_tool_calls: AtomicU64::new(0),
            turns: AtomicU64::new(0),
            last_tool: Mutex::new(None),
        })
    }

    pub(crate) fn session_ref(&self) -> SessionRef {
        SessionRef {
            id: self.session_id.clone(),
        }
    }

    pub(crate) fn typed(&self) -> bool {
        self.typed
    }

    pub(crate) fn accepted(&self) -> bool {
        self.accepted.load(Ordering::SeqCst)
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn take_submit(&self) -> Option<SubmitSender> {
        self.pending_submit
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// The engine's verdict lands. Accept marks the ask as settled; Nudge only sets the flag (the turn has ended; the driver loop starts a new round).
    pub(crate) fn apply_verdict(&self, verdict: SubmitVerdict) {
        match verdict {
            SubmitVerdict::Accept => {
                // Mark first, then release the tool: after the tool returns the turn keeps running the remaining tool calls, and accepted is what makes the driver
                // stop reporting ask_turn_ended.
                self.accepted.store(true, Ordering::SeqCst);
                if let Some(sender) = self.take_submit() {
                    let _ = sender.send(Ok(SubmitVerdict::Accept));
                }
            }
            SubmitVerdict::Reject { violations } => {
                if let Some(sender) = self.take_submit() {
                    let _ = sender.send(Ok(SubmitVerdict::Reject { violations }));
                }
            }
            SubmitVerdict::Nudge => {
                if !self.accepted() && !self.cancelled() {
                    self.nudged.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    pub(crate) fn take_nudge(&self) -> bool {
        self.nudged.swap(false, Ordering::SeqCst)
    }

    /// An active cancellation from the engine: abort the in-flight turn, release the parked submit and the parked escalation Q&A (otherwise the tool blocks forever),
    /// and prevent later rounds. A settled ask reports nothing further (the engine has already settled).
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(sender) = self.take_submit() {
            let _ = sender.send(Err(WorkflowError::new(
                WorkflowErrorCode::Cancelled,
                "The ask was cancelled by the engine.",
            )));
        }
        self.withdraw_escalations();
        if let Some(child) = &self.child {
            child.request_cancel();
        }
    }

    pub(crate) fn withdraw_escalations(&self) {
        self.escalations.withdraw_instance(&self.instance);
    }

    /// A tool call that really starts running: the counter, the latest target, and the gate-closing signal of the first workspace write.
    ///
    /// `mutating` and `world_touching` are decided by the caller from the **resolved** contract (the same meaning as TS's
    /// `ToolCallStarted` payload); `submit_result` / `escalate` do not come through here.
    pub(crate) fn note_tool_call(
        &self,
        name: &str,
        input: &Value,
        world_touching: bool,
        mutating: bool,
    ) {
        self.tool_calls.fetch_add(1, Ordering::SeqCst);
        if world_touching {
            self.world_tool_calls.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(summary) = prompt::summarize_tool_call(name, input) {
            if let Ok(mut last) = self.last_tool.lock() {
                *last = Some(summary);
            }
        }
        if mutating && !self.mutating_reported.swap(true, Ordering::SeqCst) {
            // The import-cache closing signal is reported only once (the engine side is already idempotent; reporting fewer times just saves event traffic).
            if let Some(inner) = self.inner.upgrade() {
                inner.report_mutating(&self.instance);
            }
        }
    }

    pub(crate) fn note_turn_started(&self) -> u64 {
        self.turns.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Protocol tools (`submit_result` / `escalate`) also count toward `toolCalls` (an honest audit surface), but not as
    /// "having touched the external world" (otherwise every typed ask would look impure just for delivering its result).
    pub(crate) fn note_protocol_tool_call(&self, name: &str, input: &Value) {
        self.tool_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(summary) = prompt::summarize_tool_call(name, input) {
            if let Ok(mut last) = self.last_tool.lock() {
                *last = Some(summary);
            }
        }
    }

    /// The two facts reported to the engine after the turn parsed; the order is load-bearing: progress before usage.
    pub(crate) fn report_observations(&self, before: UsageMark, after: UsageMark) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let Some(sink) = inner.sink() else {
            return;
        };
        let last_tool = self.last_tool.lock().ok().and_then(|last| last.clone());
        sink.ask_progress(
            &self.instance,
            AskProgress {
                turn: self.turns.load(Ordering::SeqCst),
                tool_calls: self.tool_calls.load(Ordering::SeqCst),
                last_tool,
            },
        );
        sink.ask_stats(
            &self.instance,
            AskStats {
                // The engine accumulates spentTokens from "the delta of each report", so this must be **this round's** delta
                // (TS's TurnResult.usage is also per-turn usage), not the session's cumulative value.
                tokens: after.tokens.saturating_sub(before.tokens),
                tool_calls: self.tool_calls.load(Ordering::SeqCst),
                turns: after.requests.saturating_sub(before.requests).max(1),
                world_tool_calls: Some(self.world_tool_calls.load(Ordering::SeqCst)),
            },
        );
    }

    pub(crate) fn report_turn_ended(&self, final_text: &str) {
        if self.accepted() {
            return;
        }
        if let Some(inner) = self.inner.upgrade() {
            if let Some(sink) = inner.sink() {
                sink.ask_turn_ended(&self.instance, final_text);
            }
        }
    }

    /// Write the session message count at the end of this ask's exchange into this journal row (the truncation boundary for amend-resume).
    ///
    /// The host's "message count" equals the number of model history messages (`Session::history_snapshot`); the row projection is not
    /// what is counted here. If the record is gone (the run was cleaned up) there is nowhere to write -- a missed bookkeeping entry, not an error.
    pub(crate) fn journal_message_boundary(&self, child: &Session) {
        let Ok(history) = child.history_snapshot() else {
            return;
        };
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if let Some(mut node) =
            inner
                .journal
                .get_node(&inner.run_id, &self.instance.site_id, self.instance.ordinal)
        {
            node.message_boundary = Some(history.len() as u64);
            inner.journal.put_node(node);
        }
    }

    pub(crate) fn report_failure(&self, failure: TurnFailure) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let Some(sink) = inner.sink() else {
            return;
        };
        match failure {
            TurnFailure::Provider(error) => sink.stop_run(*error),
            TurnFailure::ContextLimit(message) => {
                sink.ask_failed(
                    &self.instance,
                    WorkflowError::new(WorkflowErrorCode::ContextLimit, message),
                );
            }
            TurnFailure::Other(message) => {
                sink.ask_failed(
                    &self.instance,
                    WorkflowError::new(
                        WorkflowErrorCode::DriverError,
                        format!("Subagent turn failed: {message}"),
                    ),
                );
            }
        }
    }

    // ——————————————————————— the submit_result bridge ———————————————————————

    pub(crate) async fn submit(&self, payload: Value) -> Result<String, ToolError> {
        // The untyped-ask guard: by design "an all-untyped actor registers no submit_result", but the host cannot see the actor's aggregate typed
        // information when it creates the session (that needs the site graph), so it registers unconditionally. Intercept right here: when an untyped ask
        // receives submit, immediately answer with a synthetic rejection telling the model to use plain text, and never report it to the engine.
        if !self.typed() {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                SUBMIT_UNTYPED_ASK,
            ));
        }
        if self.accepted() || self.cancelled() {
            // A submit arrived with no in-flight ask: not routed to the engine, rejected outright (to avoid hanging).
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                SUBMIT_NO_ACTIVE_ASK,
            ));
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        {
            let Ok(mut slot) = self.pending_submit.lock() else {
                return Err(ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    "submit bridge lock is poisoned",
                ));
            };
            // The single-prior-instance invariant: at most one suspended deferred. If one already exists (should not happen), reject the old one first to avoid a leak.
            if let Some(previous) = slot.replace(sender) {
                let _ = previous.send(Err(WorkflowError::new(
                    WorkflowErrorCode::DriverError,
                    "This submit was superseded by a newer submit.",
                )));
            }
        }
        let Some(inner) = self.inner.upgrade() else {
            self.take_submit();
            return Err(ToolError::new(
                ToolErrorCode::Cancelled,
                "The workflow run was disposed before the submit reached the engine.",
            ));
        };
        let Some(sink) = inner.sink() else {
            self.take_submit();
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "The workflow submit sink is not attached to this driver (wiring error).",
            ));
        };
        // **Synchronous reporting**: the engine validates in the same call stack and returns its verdict through respond_to_submit. The pending lock must already be
        // released here (respond_to_submit takes that same lock).
        sink.ask_submit_attempted(&self.instance, payload);
        match receiver.await {
            Ok(Ok(SubmitVerdict::Accept)) => {
                self.accepted.store(true, Ordering::SeqCst);
                // A host turn has no per-tool turnControl: set the session cancel flag so the tool loop stops at the top of the next round
                // (the tool itself returns success, not Cancelled). The engine has already settled this ask.
                if let Some(child) = &self.child {
                    child.request_cancel();
                }
                Ok(json!({"status": "accepted"}).to_string())
            }
            Ok(Ok(SubmitVerdict::Reject { violations })) => Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                prompt::format_submit_violations(&violations),
            )),
            Ok(Ok(SubmitVerdict::Nudge)) => Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "The engine asked for a new turn while submit_result was waiting for a verdict.",
            )),
            Ok(Err(error)) if error.code == WorkflowErrorCode::Cancelled => {
                Err(ToolError::new(ToolErrorCode::Cancelled, SUBMIT_CANCELLED))
            }
            Ok(Err(error)) => Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("submit_result was rejected before reaching the engine: {error}"),
            )),
            Err(_) => Err(ToolError::new(ToolErrorCode::Cancelled, SUBMIT_CANCELLED)),
        }
    }

    // ——————————————————————— the escalate bridge ———————————————————————

    /// Park one escalation Q&A and wait for the main agent to answer.
    ///
    /// Both early returns hand back an **ordinary tool result** instead of throwing: rendering budget-exhausted / no-in-flight-ask as an error only makes the
    /// model treat it as a retryable fault and keep hitting the same wall -- and eliminating exactly that churn is why this feature exists.
    pub(crate) async fn escalate(
        &self,
        question: String,
        context: Option<String>,
    ) -> Result<String, ToolError> {
        if self.accepted() || self.cancelled() {
            return Ok(refused("no_active_ask", ESCALATION_NO_ACTIVE_ASK));
        }
        if !reserve_escalation(&self.escalations_used) {
            return Ok(refused("budget_exhausted", ESCALATION_BUDGET_EXHAUSTED));
        }
        let Some(inner) = self.inner.upgrade() else {
            return Err(ToolError::new(ToolErrorCode::Cancelled, ESCALATE_CANCELLED));
        };
        let Some(qid) = self.escalations.mint_qid(&inner.run_id) else {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                "Cannot mint a free escalation question id (two drivers may be sharing one runId).",
            ));
        };
        let context = normalize_escalation_context(context);
        let asked_at = crate::store::now_ms() as f64;
        let view = PendingQuestion {
            qid: qid.clone(),
            run_id: inner.run_id.clone(),
            actor: self.actor.clone(),
            actor_name: self.actor_name.clone(),
            question: question.clone(),
            context: context.clone(),
            asked_at,
            instance: self.instance.clone(),
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        if !self.escalations.park(view, sender) {
            return Err(ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("Escalation {qid} is already parked (question-id collision)."),
            ));
        }
        inner.record_event(&nc_code_workflow::types::RunEvent::EscalationRaised {
            qid: qid.clone(),
            actor: self.actor.clone(),
            actor_name: self.actor_name.clone(),
            question,
            context,
            asked_at,
        });
        // When the tool future is dropped (the turn was withdrawn wholesale) the registry must still be cleaned, otherwise the snapshot would forever hold
        // a question with no listener.
        let mut guard = ParkedQuestionGuard {
            registry: Arc::clone(&self.escalations),
            qid: qid.clone(),
            armed: true,
        };
        let outcome = receiver.await;
        guard.armed = false;
        match outcome {
            Ok(Ok(answer)) => Ok(json!({
                "status": "answered",
                "message": answer,
                "qid": qid,
            })
            .to_string()),
            // Withdrawal (cancelAsk / session close) and a lost sender are the same outcome: the ask is gone.
            Ok(Err(_)) | Err(_) => {
                Err(ToolError::new(ToolErrorCode::Cancelled, ESCALATE_CANCELLED))
            }
        }
    }
}

fn refused(reason: &str, message: &str) -> String {
    json!({"status": "refused", "message": message, "reason": reason}).to_string()
}

fn reserve_escalation(used: &AtomicU32) -> bool {
    used.try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
        (used < MAX_ESCALATIONS_PER_ASK).then_some(used + 1)
    })
    .is_ok()
}

/// The optional context of `escalate`: whitespace equals absence (models often pass an empty string, which is just noise inside an event).
fn normalize_escalation_context(context: Option<String>) -> Option<String> {
    context
        .map(|context| context.trim().to_string())
        .filter(|context| !context.is_empty())
}

struct ParkedQuestionGuard {
    registry: Arc<EscalationRegistry>,
    qid: String,
    armed: bool,
}

impl Drop for ParkedQuestionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.registry.withdraw(&self.qid);
        }
    }
}

// ——————————————————————————————— the turn driver loop ———————————————————————————————

#[derive(Clone, Copy)]
pub(crate) struct UsageMark {
    tokens: u64,
    requests: u64,
}

pub(crate) fn usage_mark(session: &Session) -> UsageMark {
    let report = session.usage_report();
    UsageMark {
        tokens: report
            .get("totalTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        requests: report
            .get("modelRequestCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

/// Drive the complete exchange of one ask: the initial turn + (when the engine requires it) one nudge turn.
///
/// After every turn, report progress / stats (the stats are **this round's delta**); once accepted or cancelled by the engine
/// do not report `ask_turn_ended` again. Every exit path withdraws any parked escalation Q&A.
pub(crate) async fn drive_ask(
    ask: Arc<AskState>,
    child: Arc<Session>,
    provider: Option<ProviderConfig>,
    deps: TurnDeps,
    instructions: String,
) {
    let _ = drive_inner(&ask, &child, provider.as_ref(), &deps, instructions).await;
    // The normal path does not need it (a parked escalation blocks the turn, so an ask cannot settle while it still owes an answer); this cleanup
    // covers abnormal exits (turn failure, lost sender, the future being dropped).
    ask.withdraw_escalations();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DriveEnd {
    Settled,
    Failed,
    Cancelled,
}

async fn drive_inner(
    ask: &Arc<AskState>,
    child: &Arc<Session>,
    provider: Option<&ProviderConfig>,
    deps: &TurnDeps,
    instructions: String,
) -> DriveEnd {
    let mut prompt = instructions;
    loop {
        if ask.cancelled() {
            return DriveEnd::Cancelled;
        }
        let before = usage_mark(child);
        let turn_index = ask.note_turn_started();
        let (turn_id, source_command_id) = {
            let Some(inner) = ask.inner.upgrade() else {
                return DriveEnd::Cancelled;
            };
            let turn_id = inner.next_turn_id(&ask.instance, turn_index);
            (turn_id.clone(), format!("workflow-{turn_id}"))
        };
        let report = turn::run_subagent_turn(
            Arc::clone(child),
            provider,
            deps,
            &prompt,
            &turn_id,
            &source_command_id,
        )
        .await;
        let after = usage_mark(child);
        ask.report_observations(before, after);

        if ask.cancelled() {
            return DriveEnd::Cancelled;
        }
        if ask.accepted() {
            // Already settled: the turn ending is only a confirmation (the engine already did settleOk at accept time).
            //
            // Clear the session cancel bit we set in order to "stop the next round": on the normal path the tool loop has already taken it,
            // but if the loop exits early because of another typed fault, a leftover bit would make the next ask of the same actor stop on
            // its very first round. Only our own bit is cleared here -- accepted is its only precondition.
            let _ = child.take_cancel();
            ask.journal_message_boundary(child);
            return DriveEnd::Settled;
        }
        match report.state {
            TurnState::Failed => {
                let last_error = child.last_error();
                let label = actor_label(&ask.actor, ask.actor_name.as_deref());
                let (provider_id, model_id) = ask
                    .inner
                    .upgrade()
                    .map(|inner| (inner.provider_id(), inner.model_id()))
                    .unwrap_or((None, None));
                let failure = classify_turn_failure(
                    last_error.as_ref(),
                    &label,
                    provider_id.as_deref(),
                    model_id.as_deref(),
                );
                ask.report_failure(failure);
                return DriveEnd::Failed;
            }
            TurnState::Interrupted => {
                // The session was cancelled by Stop / dispose: the engine has already (or is about to) settle, so do not report twice.
                return DriveEnd::Cancelled;
            }
            TurnState::Completed => {}
        }
        ask.report_turn_ended(&report.final_text);
        if ask.take_nudge() {
            prompt = NUDGE_PROMPT.to_string();
            continue;
        }
        ask.journal_message_boundary(child);
        return DriveEnd::Settled;
    }
}

/// Provided so actor_tools' contract tests can construct an ask that is not attached to a driver (the Weak is empty).
#[cfg(test)]
pub(crate) fn test_ask_state() -> Arc<AskState> {
    Arc::new(AskState {
        inner: Weak::new(),
        instance: InstanceRef {
            site_id: "ask#1".to_string(),
            ordinal: 1,
        },
        session_id: "session-test".to_string(),
        actor: ActorRef {
            site_id: "actor#1".to_string(),
            ordinal: 1,
        },
        actor_name: Some("critic".to_string()),
        typed: true,
        child: None,
        escalations: EscalationRegistry::new(),
        accepted: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        nudged: AtomicBool::new(false),
        mutating_reported: AtomicBool::new(false),
        escalations_used: AtomicU32::new(0),
        pending_submit: Mutex::new(None),
        tool_calls: AtomicU64::new(0),
        world_tool_calls: AtomicU64::new(0),
        turns: AtomicU64::new(0),
        last_tool: Mutex::new(None),
    })
}
