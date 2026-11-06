//! The protocol tool surface of actor sessions and the escalation Q&A registry.
//!
//! Two parts:
//! 1. **The tool surface** (port of `workflow-actor-tools.ts` + the contracts' `submit-result.ts` /
//!    `escalate.ts`): `submit_result` and `escalate` are registered only into actor child sessions, and the declarations align with TS field by
//!    field (readOnly=false / destructive=false / sideEffectScope=session). The real bridging logic lives in
//!    `ask.rs`; here there is only the tool shell and the contract.
//! 2. **The escalation registry** (port of the parking table in `workflow-driver-escalation.ts` +
//!    `workflow-escalation-registry.ts`): qid → the parked question. The run service sends the main agent's answer back through
//!    [`EscalationRegistry::resolve_question`]; the driver's `cancel_ask` withdraws questions through
//!    [`EscalationRegistry::withdraw`] so that the tool receives `Cancelled`.
//!
//! Escalation happens inside the boundary where the driver executes an ask (the same layer as the repair / nudge rounds), and the engine core is entirely unaware of it:
//! it writes no `dwf_node` row, this is only one blocking Q&A on the driver side.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nc_code_workflow::types::{
    ref_to_string, ActorRef, InstanceRef, RunEvent, WorkflowError, WorkflowErrorCode,
};
use serde_json::{json, Value};

use crate::tools::{AsyncTool, ToolContext, ToolContract, ToolError, ToolErrorCode, SCOPE_SESSION};
use crate::workflow::ask::AskState;
use crate::workflow::prompt;

/// Where the driver's events land (journal + emit); the registry only hands it "already answered".
type EscalationEmitter = Arc<dyn Fn(&RunEvent) + Send + Sync>;

pub const SUBMIT_RESULT_TOOL_NAME: &str = "submit_result";
pub const ESCALATE_TOOL_NAME: &str = "escalate";
const TOOL_JSON_SCHEMA_VERSION: &str = "https://json-schema.org/draft/2020-12/schema";
const MAX_TOOL_MODEL_BYTES: usize = 16_000;
/// TS uses `timeout: {kind:"none"}`: an engine verdict / a main agent's answer may take arbitrarily long. The host's async tools do no
/// wall-clock timeout at this layer (`turn.rs` only reads the contract's budget fields), so this value is a declarative ceiling.
const NO_TIMEOUT_MS: u64 = 86_400_000;

// ——————————————————————————————— the escalation registry ———————————————————————————————

/// One parked escalation question (the read-only view for the run service).
#[derive(Clone, Debug, PartialEq)]
pub struct PendingQuestion {
    pub qid: String,
    pub run_id: String,
    pub actor: ActorRef,
    pub actor_name: Option<String>,
    pub question: String,
    pub context: Option<String>,
    pub asked_at: f64,
    /// The ask that parked it (used by the snapshot projection; not involved in routing).
    pub instance: InstanceRef,
}

struct Parked {
    question: PendingQuestion,
    sender: Option<tokio::sync::oneshot::Sender<Result<String, WorkflowError>>>,
}

/// The table of parked escalation Q&A. Fields are private; services read/answer only through the public methods.
/// The outcome of a qid that has left the stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationRetiredKind {
    Answered,
    Withdrawn,
}

/// A bounded departure log (FIFO eviction; the cap is enough to cover the answering volume of one session).
#[derive(Default)]
struct RetiredLog {
    order: std::collections::VecDeque<String>,
    kinds: HashMap<String, EscalationRetiredKind>,
}

const RETIRED_CAP: usize = 512;

impl RetiredLog {
    fn record(&mut self, qid: &str, kind: EscalationRetiredKind) {
        if self.kinds.insert(qid.to_string(), kind).is_none() {
            self.order.push_back(qid.to_string());
        }
        while self.order.len() > RETIRED_CAP {
            if let Some(evicted) = self.order.pop_front() {
                self.kinds.remove(&evicted);
            }
        }
    }

    fn get(&self, qid: &str) -> Option<EscalationRetiredKind> {
        self.kinds.get(qid).copied()
    }
}

#[derive(Default)]
pub struct EscalationRegistry {
    pending: Mutex<HashMap<String, Parked>>,
    /// The departure log: qids already answered / withdrawn within this process (bounded). resolve_question uses it to tell
    /// "already answered" and "withdrawn with the ask" apart from "never existed" -- the registry itself does not persist,
    /// so after a process restart old qids uniformly fall back to UnknownQuestion (the copy already explains this).
    retired: Mutex<RetiredLog>,
    /// The per-run monotonic escalation sequence number; the second segment of the qid. Together with runId it forms a collision-free id.
    sequence: AtomicU64,
    /// Where the driver's events land (journal + emit). The registry has no notion of the journal; it only hands over "already answered".
    emitter: Mutex<Option<EscalationEmitter>>,
}

impl EscalationRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Attach the driver's event landing; `resolve_question` uses it to emit `escalation-resolved`.
    pub fn set_emitter(&self, emitter: Arc<dyn Fn(&RunEvent) + Send + Sync>) {
        if let Ok(mut slot) = self.emitter.lock() {
            *slot = Some(emitter);
        }
    }

    /// The questions currently parked (a stable snapshot ordered by when they were raised, without the sender).
    pub fn pending_questions(&self) -> Vec<PendingQuestion> {
        let Ok(pending) = self.pending.lock() else {
            return Vec::new();
        };
        let mut questions: Vec<PendingQuestion> = pending
            .values()
            .map(|parked| parked.question.clone())
            .collect();
        questions.sort_by(|left, right| {
            left.asked_at
                .total_cmp(&right.asked_at)
                .then_with(|| left.qid.cmp(&right.qid))
        });
        questions
    }

    /// The parked questions of one ask (the filter condition when the driver withdraws).
    pub fn pending_for_instance(&self, instance: &InstanceRef) -> Vec<PendingQuestion> {
        self.pending_questions()
            .into_iter()
            .filter(|question| question.instance == *instance)
            .collect()
    }

    /// The departure outcome of a qid (within this process); not departed or already FIFO-evicted → None.
    pub fn retired_kind(&self, qid: &str) -> Option<EscalationRetiredKind> {
        self.retired.lock().ok().and_then(|log| log.get(qid))
    }

    /// Whether a qid is already taken (the collision probe while minting).
    pub fn is_taken(&self, qid: &str) -> bool {
        self.pending
            .lock()
            .map(|pending| pending.contains_key(qid))
            .unwrap_or(false)
    }

    /// Register one parking. Returns false when the same qid already exists (the caller treats it as a wiring error).
    pub(crate) fn park(
        &self,
        question: PendingQuestion,
        sender: tokio::sync::oneshot::Sender<Result<String, WorkflowError>>,
    ) -> bool {
        let Ok(mut pending) = self.pending.lock() else {
            return false;
        };
        if pending.contains_key(&question.qid) {
            return false;
        }
        pending.insert(
            question.qid.clone(),
            Parked {
                question,
                sender: Some(sender),
            },
        );
        true
    }

    /// Settle a parked question (the run service's entry point): emit `escalation-resolved` and release the waiting tool.
    ///
    /// Returning false means this registry has no such qid (wrong run, or it was just withdrawn by cancelAsk). The registry has already retired
    /// the qid before the call; the caller does not need to delete it again.
    pub fn resolve_question(&self, qid: &str, answer: &str) -> bool {
        let parked = match self.pending.lock() {
            Ok(mut pending) => pending.remove(qid),
            Err(_) => None,
        };
        let Some(mut parked) = parked else {
            return false;
        };
        let emitter = self.emitter.lock().ok().and_then(|slot| slot.clone());
        if let Some(emitter) = emitter {
            // The trade-off **opposite** to the raise path: the actor is already waiting for this answer, and keeping it parked for the sake of one observation event is
            // the worse outcome -- deliver the answer even if the event fails (TS likewise only logs a warn).
            emitter(&RunEvent::EscalationResolved {
                qid: qid.to_string(),
                answer: answer.to_string(),
            });
        }
        if let Ok(mut log) = self.retired.lock() {
            log.record(qid, EscalationRetiredKind::Answered);
        }
        if let Some(sender) = parked.sender.take() {
            let _ = sender.send(Ok(answer.to_string()));
        }
        true
    }

    /// Withdraw a question: the waiting tool receives `Cancelled` (not "there is no answer", but that the ask is gone).
    pub fn withdraw(&self, qid: &str) -> bool {
        let parked = match self.pending.lock() {
            Ok(mut pending) => pending.remove(qid),
            Err(_) => None,
        };
        let Some(mut parked) = parked else {
            return false;
        };
        if let Ok(mut log) = self.retired.lock() {
            log.record(qid, EscalationRetiredKind::Withdrawn);
        }
        if let Some(sender) = parked.sender.take() {
            let _ = sender.send(Err(WorkflowError::new(
                WorkflowErrorCode::Cancelled,
                format!("Escalation {qid} was cancelled along with its ask."),
            )));
        }
        true
    }

    /// Withdraw all parked questions of one ask.
    pub fn withdraw_instance(&self, instance: &InstanceRef) -> usize {
        let qids: Vec<String> = self
            .pending_for_instance(instance)
            .into_iter()
            .map(|question| question.qid)
            .collect();
        qids.iter().filter(|qid| self.withdraw(qid)).count()
    }

    /// Mint a globally unique question id: `dwfq-<runId fragment>-<seq>`.
    ///
    /// A short fragment could collide with another run's fragment, so the candidates step up in length; the last candidate is the full runId,
    /// which by construction must be unused, so the loop always terminates. All of them taken can only mean two drivers share the same
    /// runId (an assembly error) -- return None and let the caller fail loudly, instead of issuing an id that would misroute the answer.
    pub(crate) fn mint_qid(&self, run_id: &str) -> Option<String> {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let body = run_id.strip_prefix("dwfrun-").unwrap_or(run_id);
        let full = sanitize_id_segment(body);
        let mut candidates: Vec<String> = Vec::new();
        for fragment in [
            full.chars().take(8).collect::<String>(),
            full.chars().take(16).collect::<String>(),
            full,
        ] {
            if !candidates.contains(&fragment) {
                candidates.push(fragment);
            }
        }
        for fragment in candidates {
            let candidate = format!("dwfq-{fragment}-{sequence}");
            if !self.is_taken(&candidate) {
                return Some(candidate);
            }
        }
        None
    }
}

fn sanitize_id_segment(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '_' => escaped.push_str("__"),
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' => escaped.push(character),
            _ => escaped.push('_'),
        }
    }
    escaped
}

// ——————————————————————————————— submit_result ———————————————————————————————

/// `submit_result`: the actor submits the structured terminal result of this ask.
///
/// The input is deliberately kept generic (`result`'s shape is unconstrained): the per-ask schema travels with the instruction epilogue and the engine validates it on
/// the submit port side. This way the tool declaration can be frozen across asks and hit the prompt cache (the same as the TS generic profile).
pub(crate) struct SubmitResultTool {
    ask: Arc<AskState>,
}

impl SubmitResultTool {
    pub(crate) fn new(ask: Arc<AskState>) -> Self {
        Self { ask }
    }
}

#[async_trait]
impl AsyncTool for SubmitResultTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: SUBMIT_RESULT_TOOL_NAME.to_string(),
            description: "Submit the structured result for the current ask. The required JSON \
                          shape is described in the ask instructions."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "result": {
                        "description": "The structured result for this ask, matching the JSON schema given in the ask instructions."
                    }
                },
                "required": ["result"],
                "additionalProperties": false,
                "$schema": TOOL_JSON_SCHEMA_VERSION,
            }),
            read_only: false,
            destructive: false,
            // TS declares false (putting submit in its own serial group, relying on the "success ends the turn" scheduling semantics to keep sibling
            // tools from running after it). A host's turn already executes tools serially in model order, with no parallel grouping;
            // this field is declarative here, taking TS's value to keep the reconciliation table consistent.
            concurrent_safe: false,
            side_effect_scope: SCOPE_SESSION,
            timeout_ms: NO_TIMEOUT_MS,
            max_output_bytes: MAX_TOOL_MODEL_BYTES,
            max_model_bytes: MAX_TOOL_MODEL_BYTES,
            preview_direction: "head",
        }
    }

    async fn execute(&self, input: &Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        self.ask
            .note_protocol_tool_call(SUBMIT_RESULT_TOOL_NAME, input);
        let Some(payload) = input.get("result") else {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                "result: required (any JSON value). Pass the conforming JSON as the `result` argument.",
            ));
        };
        self.ask.submit(payload.clone()).await
    }
}

// ——————————————————————————————— escalate ———————————————————————————————

/// The tool description of `escalate` = the usage discipline on the actor side (TS's original wording, verbatim).
const ESCALATE_DESCRIPTION: &str = "Escalates a question that is BLOCKING you to the main agent that created this workflow, and waits here for the answer.

This is a LAST RESORT, for when you are genuinely stuck on something outside your reach:
- a gate that is broken or impossible to pass (a check capped at 95 when the threshold is 96);
- instructions that contradict each other, so no output can satisfy both;
- a fact you cannot obtain (a tradeoff, an external convention, what 'good' means here) that only whoever started this run knows.

Do NOT use it for curiosity, progress reports, asking permission, confirming a conclusion you could verify yourself, or thinking out loud. None of those are blocked — keep working.

Ask ONE focused question that can be answered in a sentence, and put your evidence in `context`: what you already tried, and exactly where you are stuck. The quality of the answer depends on it.

The cost: this call BLOCKS until the main agent answers, which may take a long time. You get at most 3 escalations per ask; the 4th tells you the budget is spent and to proceed on your own best judgement. Do not spend them on questions not worth waiting for.";

/// `escalate`: the actor escalates a genuine blocker to the main agent that created this workflow, and parks inside the ask waiting for an answer.
pub(crate) struct EscalateTool {
    ask: Arc<AskState>,
}

impl EscalateTool {
    pub(crate) fn new(ask: Arc<AskState>) -> Self {
        Self { ask }
    }
}

#[async_trait]
impl AsyncTool for EscalateTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: ESCALATE_TOOL_NAME.to_string(),
            description: ESCALATE_DESCRIPTION.to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The single focused question that unblocks you. One question per call, answerable in a sentence."
                    },
                    "context": {
                        "type": "string",
                        "description": "What you already tried and where exactly you are stuck — the evidence the answerer needs."
                    }
                },
                "required": ["question"],
                "additionalProperties": false,
                "$schema": TOOL_JSON_SCHEMA_VERSION,
            }),
            read_only: false,
            destructive: false,
            // Deliberately unlike submit_result: an escalation is **not** a terminal tool (it neither ends the turn nor excludes sibling tools).
            concurrent_safe: true,
            side_effect_scope: SCOPE_SESSION,
            timeout_ms: NO_TIMEOUT_MS,
            max_output_bytes: MAX_TOOL_MODEL_BYTES,
            max_model_bytes: MAX_TOOL_MODEL_BYTES,
            preview_direction: "head",
        }
    }

    async fn execute(&self, input: &Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        self.ask.note_protocol_tool_call(ESCALATE_TOOL_NAME, input);
        let question = input
            .get("question")
            .and_then(Value::as_str)
            .filter(|question| !question.is_empty())
            .ok_or_else(|| {
                ToolError::new(
                    ToolErrorCode::InvalidInput,
                    "question: required non-empty string (one focused question per call)",
                )
            })?
            .to_string();
        let context = match input.get("context") {
            None | Some(Value::Null) => None,
            Some(Value::String(context)) => Some(context.clone()),
            Some(_) => {
                return Err(ToolError::new(
                    ToolErrorCode::InvalidInput,
                    "context: must be a string when present",
                ))
            }
        };
        self.ask.escalate(question, context).await
    }
}

/// For tests/diagnostics: the escalation budget of one ask (the constant is exported here so two places do not each write a 3).
pub const MAX_ESCALATIONS_PER_ASK: u32 = prompt::MAX_ESCALATIONS_PER_ASK;

/// A stable key order (qid → question) for the run service to assemble a snapshot; empty when nothing is parked.
pub fn pending_questions_by_id(registry: &EscalationRegistry) -> BTreeMap<String, PendingQuestion> {
    registry
        .pending_questions()
        .into_iter()
        .map(|question| (question.qid.clone(), question))
        .collect()
}

/// The actor's display name: the name when it has one, otherwise `site@ordinal` (the same as TS's `subagentLabel`).
pub(crate) fn actor_label(actor: &ActorRef, name: Option<&str>) -> String {
    name.map(str::to_string)
        .unwrap_or_else(|| ref_to_string(&actor.site_id, actor.ordinal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_workflow::types::Violation;

    fn question(qid: &str) -> PendingQuestion {
        PendingQuestion {
            qid: qid.to_string(),
            run_id: "run".to_string(),
            actor: ActorRef {
                site_id: "actor#1".to_string(),
                ordinal: 1,
            },
            actor_name: Some("critic".to_string()),
            question: "Which gate?".to_string(),
            context: None,
            asked_at: 1.0,
            instance: InstanceRef {
                site_id: "ask#1".to_string(),
                ordinal: 1,
            },
        }
    }

    #[tokio::test]
    async fn resolve_delivers_the_answer_and_emits() {
        let registry = EscalationRegistry::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_sink = Arc::clone(&events);
        registry.set_emitter(Arc::new(move |event: &RunEvent| {
            events_for_sink.lock().unwrap().push(event.clone());
        }));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        assert!(registry.park(question("dwfq-a-1"), sender));
        assert_eq!(registry.pending_questions().len(), 1);
        assert!(registry.resolve_question("dwfq-a-1", "use gate B"));
        assert_eq!(receiver.await.unwrap().unwrap(), "use gate B");
        assert!(registry.pending_questions().is_empty());
        assert!(matches!(
            &events.lock().unwrap()[0],
            RunEvent::EscalationResolved { qid, answer }
                if qid == "dwfq-a-1" && answer == "use gate B"
        ));
        assert!(!registry.resolve_question("dwfq-a-1", "again"));
    }

    #[tokio::test]
    async fn withdraw_cancels_the_waiting_tool() {
        let registry = EscalationRegistry::new();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        assert!(registry.park(question("dwfq-a-2"), sender));
        assert_eq!(
            registry.withdraw_instance(&InstanceRef {
                site_id: "ask#1".to_string(),
                ordinal: 1
            }),
            1
        );
        let error = receiver.await.unwrap().expect_err("cancelled");
        assert_eq!(error.code, WorkflowErrorCode::Cancelled);
        assert!(!registry.is_taken("dwfq-a-2"));
    }

    #[test]
    fn retired_questions_keep_their_outcome() {
        let registry = EscalationRegistry::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        assert!(registry.park(question("dwfq-a-1"), tx));
        assert!(registry.resolve_question("dwfq-a-1", "use gate B"));
        assert_eq!(
            registry.retired_kind("dwfq-a-1"),
            Some(EscalationRetiredKind::Answered)
        );
        let (tx, _rx) = tokio::sync::oneshot::channel();
        assert!(registry.park(question("dwfq-b-2"), tx));
        assert!(registry.withdraw("dwfq-b-2"));
        assert_eq!(
            registry.retired_kind("dwfq-b-2"),
            Some(EscalationRetiredKind::Withdrawn)
        );
        assert_eq!(registry.retired_kind("dwfq-zzz-9"), None);
    }

    #[test]
    fn qids_are_run_scoped_and_never_collide() {
        let registry = EscalationRegistry::new();
        let first = registry.mint_qid("dwfrun-abcdefgh1234").unwrap();
        let second = registry.mint_qid("dwfrun-abcdefgh1234").unwrap();
        assert!(first.starts_with("dwfq-abcdefgh-"), "{first}");
        assert_ne!(first, second);
    }

    #[test]
    fn contracts_pin_the_ts_metadata() {
        let ask = ask_state_stub();
        let submit = SubmitResultTool::new(ask.clone()).contract();
        assert_eq!(submit.name, "submit_result");
        assert!(!submit.read_only && !submit.destructive);
        assert_eq!(submit.side_effect_scope, "session");
        assert_eq!(submit.input_schema["required"], json!(["result"]));
        assert_eq!(submit.input_schema["additionalProperties"], json!(false));

        let escalate = EscalateTool::new(ask).contract();
        assert_eq!(escalate.name, "escalate");
        assert!(!escalate.read_only && !escalate.destructive);
        assert!(escalate.concurrent_safe);
        assert_eq!(escalate.side_effect_scope, "session");
        assert_eq!(escalate.input_schema["required"], json!(["question"]));
        assert!(
            escalate.description.contains("LAST RESORT")
                && escalate
                    .description
                    .contains("at most 3 escalations per ask")
        );
    }

    /// Contract tests need no real session; the construction of AskState is covered by the tests in `ask.rs`.
    fn ask_state_stub() -> Arc<AskState> {
        crate::workflow::ask::test_ask_state()
    }

    #[test]
    fn violation_message_is_repairable() {
        let text = crate::workflow::prompt::format_submit_violations(&[Violation {
            path: "$.x".to_string(),
            expected: "integer".to_string(),
            got: "string".to_string(),
        }]);
        assert!(text.contains("$.x: expected integer, got string"));
    }
}
