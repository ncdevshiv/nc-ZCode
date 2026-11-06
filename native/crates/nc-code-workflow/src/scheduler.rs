//! ask scheduler + actor registration (port of `scheduler.ts` / `scheduler-submit.ts` / the
//! `createActor` of `engine.ts`).
//!
//! All of these are synchronous `impl Core` methods, called while the caller holds the outer Mutex. The async session
//! creation and dequeue are performed outside the lock by the outer layer ([`crate::engine::WorkflowEngine`]) executing a
//! [`DispatchIntent`]; the scheduling decisions themselves are made inside the lock, deterministically in FIFO order, matching TS's single-threaded semantics.

use serde_json::Value;
use tokio::sync::oneshot;

use crate::engine_state::{Actor, AskAdmission, AskNode, Core, DispatchIntent, PendingRecorded};
use crate::imported_cache::{imported_ask_record, match_imported_actor};
use crate::types::{
    input_hash, ref_to_string, ActorId, ActorRef, AskMessage, AskSpec, AskStats, InstanceRef,
    NodeKind, NodeOutcome, NodeRecord, NodeRecordStatus, PersonaSpec, RunEvent, SubmitVerdict,
    Violation, WorkflowError, WorkflowErrorCode, INSTRUCTIONS_HEAD_MAX_CHARS, NUDGE_ATTEMPTS,
    REPAIR_ATTEMPTS,
};

pub(crate) type AskReceiver = oneshot::Receiver<Result<Value, WorkflowError>>;

impl Core {
    /// Create an actor (host API). The effective name = spec.name after normalizePersona; when non-empty it must be unique.
    pub fn create_actor(
        &mut self,
        site_id: &str,
        name: Option<String>,
        persona: Option<Value>,
    ) -> Result<ActorId, WorkflowError> {
        if self.run_settled {
            return Err(self.run_error());
        }
        let ordinal = self.next_ordinal(site_id);
        let ref_ = ActorRef {
            site_id: site_id.to_string(),
            ordinal,
        };
        let id = ref_to_string(site_id, ordinal);
        let spec = normalize_persona(name.clone(), persona);
        let effective_name = spec.name.clone();
        if let Some(effective) = effective_name.as_deref() {
            if !effective.is_empty() {
                if let Some(claimed) = self.actor_names.get(effective) {
                    let err = WorkflowError::new(
                        WorkflowErrorCode::DuplicateActorName,
                        format!(
                            "Subagent name \"{effective}\" is used twice in this run ({claimed} and {id}). \
                             A named subagent is the identity an amended re-run matches its cache by, so \
                             names must be unique; give this one its own name or drop the name to make it \
                             anonymous."
                        ),
                    );
                    self.fail_run(err.clone());
                    return Err(err);
                }
                self.actor_names.insert(effective.to_string(), id.clone());
            }
        }
        let imported = match_imported_actor(self.imported_cache.as_ref(), &spec);
        self.register_actor(
            ref_.clone(),
            id.clone(),
            name.clone(),
            spec.clone(),
            imported,
        );
        // putActor replaces the entire record; sessionId / resolvedModel are driver-owned fields, carried through verbatim.
        let existing = self.journal().get_actor(&self.run_id, site_id, ordinal);
        self.journal().put_actor(crate::types::ActorRecord {
            run_id: self.run_id.clone(),
            site_id: site_id.to_string(),
            ordinal,
            name,
            persona: Some(spec.clone()),
            session_id: existing.as_ref().and_then(|e| e.session_id.clone()),
            resolved_model: existing.as_ref().and_then(|e| e.resolved_model.clone()),
        });
        self.record(RunEvent::ActorCreated {
            actor: ref_,
            name: spec.name.clone(),
            persona: Some(spec),
            phase_name: None,
        });
        Ok(id)
    }

    fn register_actor(
        &mut self,
        ref_: ActorRef,
        id: ActorId,
        name: Option<String>,
        persona: PersonaSpec,
        imported: Option<crate::imported_cache::ImportedActorState>,
    ) {
        let recorded_count = self
            .journal()
            .list_nodes(&self.run_id)
            .into_iter()
            .filter(|n| {
                n.kind == NodeKind::Ask
                    && n.actor_site_id.as_deref() == Some(ref_.site_id.as_str())
                    && n.actor_ordinal == Some(ref_.ordinal)
            })
            .count();
        let actor = Actor {
            id: id.clone(),
            ref_,
            persona,
            name,
            recorded_count,
            next_admit_seq: 0,
            pending_recorded: Default::default(),
            pending_live: Default::default(),
            live_queue: Default::default(),
            current: None,
            session: None,
            imported,
        };
        self.actors.insert(id.clone(), actor);
        self.actor_order.push(id);
    }

    /// Accept one ask: a completed hit short-circuits (following the hold rule); a running hit or no hit dispatches live.
    pub fn admit_ask(
        &mut self,
        site_id: &str,
        actor_id: &str,
        instructions: &str,
        spec: AskSpec,
    ) -> AskReceiver {
        if self.run_settled {
            return rejected(self.run_error());
        }
        if !self.actors.contains_key(actor_id) {
            let err = WorkflowError::new(
                WorkflowErrorCode::UnknownActor,
                format!("Unknown subagent handle: {actor_id}."),
            );
            self.fail_run(err.clone());
            return rejected(err);
        }
        let ordinal = self.next_ordinal(site_id);
        let instance = InstanceRef {
            site_id: site_id.to_string(),
            ordinal,
        };
        let hash = input_hash(&Value::String(instructions.to_string()));
        let (tx, rx) = oneshot::channel();
        let admission = AskAdmission {
            instance: instance.clone(),
            instructions: instructions.to_string(),
            hash: hash.clone(),
            spec,
            sender: tx,
        };
        let recorded = self.journal().get_node(&self.run_id, site_id, ordinal);
        if let Some(recorded) = recorded {
            if recorded.input_hash != hash {
                let err = hash_mismatch(&instance, &recorded.input_hash, &hash);
                self.fail_run(err.clone());
                return rejected(err);
            }
            let seq = recorded.actor_seq.unwrap_or(0);
            let entry = if recorded.status == NodeRecordStatus::Running {
                PendingRecorded::Live(admission)
            } else {
                PendingRecorded::Cached(admission, recorded)
            };
            if let Some(actor) = self.actors.get_mut(actor_id) {
                actor.pending_recorded.insert(seq, entry);
            }
            self.drain_admission(actor_id);
            return rx;
        }
        {
            let actor = self.actors.get_mut(actor_id).expect("actor checked above");
            actor.pending_live.push_back(admission);
        }
        self.drain_admission(actor_id);
        self.pump_all();
        rx
    }

    fn drain_admission(&mut self, actor_id: &str) {
        let Some(mut actor) = self.actors.remove(actor_id) else {
            return;
        };
        loop {
            let seq = actor.next_admit_seq;
            if let Some(entry) = actor.pending_recorded.remove(&seq) {
                actor.next_admit_seq += 1;
                match entry {
                    PendingRecorded::Live(admission) => {
                        let was_live = self.live_ask_instances.contains(&admission.instance.key());
                        if let Some(imported) = actor.imported.as_mut() {
                            imported.reconcile_recorded(seq, &admission.hash, was_live);
                        }
                        self.admit_live(&mut actor, admission, seq);
                    }
                    PendingRecorded::Cached(admission, recorded) => {
                        let was_live = self.live_ask_instances.contains(&admission.instance.key());
                        if let Some(imported) = actor.imported.as_mut() {
                            imported.reconcile_recorded(seq, &recorded.input_hash, was_live);
                        }
                        self.release_cached_ask(admission, &recorded);
                    }
                }
                continue;
            }
            if actor.next_admit_seq >= actor.recorded_count as u64 && !actor.pending_live.is_empty()
            {
                let admission = actor.pending_live.pop_front().expect("non-empty checked");
                actor.next_admit_seq += 1;
                if let Some(admission) = self.try_imported_settle(&mut actor, admission, seq) {
                    self.admit_live(&mut actor, admission, seq);
                }
                continue;
            }
            break;
        }
        let id = actor.id.clone();
        self.actors.insert(id.clone(), actor);
        self.pump_actor(&id);
    }

    fn admit_live(&mut self, actor: &mut Actor, admission: AskAdmission, seq: u64) {
        let key = admission.instance.key();
        let node = AskNode {
            instance: admission.instance.clone(),
            actor: actor.ref_.clone(),
            actor_seq: seq,
            instructions: admission.instructions.clone(),
            hash: admission.hash.clone(),
            spec: admission.spec.clone(),
            sender: Some(admission.sender),
            repairs_remaining: REPAIR_ATTEMPTS,
            nudges_remaining: NUDGE_ATTEMPTS,
            settled: false,
            dispatched: false,
            last_stats: None,
        };
        self.live_nodes.insert(key.clone(), node);
        // Admission writes running immediately (carrying actorSeq + inputHash): a node that crashed mid-execution can be re-dispatched on resume from this.
        self.journal().put_node(NodeRecord {
            run_id: self.run_id.clone(),
            site_id: admission.instance.site_id.clone(),
            ordinal: admission.instance.ordinal,
            kind: NodeKind::Ask,
            actor_site_id: Some(actor.ref_.site_id.clone()),
            actor_ordinal: Some(actor.ref_.ordinal),
            actor_seq: Some(seq),
            input_hash: admission.hash.clone(),
            status: NodeRecordStatus::Running,
            result: None,
            error: None,
            stats: None,
            artifact_id: None,
            input: None,
            message_boundary: None,
        });
        actor.live_queue.push_back(key);
        let instructions_head = head_of_instructions(&admission.instructions);
        self.record(RunEvent::NodeQueued {
            instance: admission.instance,
            kind: NodeKind::Ask,
            actor: Some(actor.ref_.clone()),
            actor_seq: Some(seq),
            phase_name: None,
            instructions_head,
        });
        // Switching to live does not close the import cache: the gate is closed by the first write (askMutating / world.run), not by dispatch itself.
    }

    /// On a fresh ask's admission, query the import cache once; a hit settles cached and returns None.
    fn try_imported_settle(
        &mut self,
        actor: &mut Actor,
        admission: AskAdmission,
        seq: u64,
    ) -> Option<AskAdmission> {
        let entry = match actor.imported.as_mut() {
            Some(imported) => {
                if self.import_closed {
                    imported.take_if_pure(seq, &admission.hash)
                } else {
                    imported.take(seq, &admission.hash)
                }
            }
            None => None,
        };
        let Some(entry) = entry else {
            return Some(admission);
        };
        self.journal().put_node(imported_ask_record(
            &self.run_id,
            &admission.instance,
            &actor.ref_,
            seq,
            &admission.hash,
            &entry,
        ));
        self.record(RunEvent::NodeSettled {
            instance: admission.instance.clone(),
            outcome: NodeOutcome::Ok,
            cached: Some(true),
            error: None,
            phase_name: None,
        });
        let _ = admission.sender.send(Ok(entry.result));
        None
    }

    fn release_cached_ask(&mut self, admission: AskAdmission, recorded: &NodeRecord) {
        if recorded.status == NodeRecordStatus::Completed {
            self.record(RunEvent::NodeSettled {
                instance: admission.instance.clone(),
                outcome: NodeOutcome::Ok,
                cached: Some(true),
                error: None,
                phase_name: None,
            });
            let _ = admission
                .sender
                .send(Ok(recorded.result.clone().unwrap_or(Value::Null)));
        } else {
            // A recorded failure must also short-circuit and be reproduced (the script may already have try/catch'd it).
            let json = recorded.error.clone();
            self.record(RunEvent::NodeSettled {
                instance: admission.instance.clone(),
                outcome: NodeOutcome::Failed,
                cached: Some(true),
                error: json.clone(),
                phase_name: None,
            });
            let err = json
                .map(|j| WorkflowError::from_json(&j))
                .unwrap_or_else(|| {
                    WorkflowError::new(
                        WorkflowErrorCode::DriverError,
                        "Recorded failure is missing its error",
                    )
                });
            let _ = admission.sender.send(Err(err));
        }
    }

    pub(crate) fn pump_all(&mut self) {
        let ids = self.actor_order.clone();
        for id in ids {
            self.pump_actor(&id);
        }
    }

    fn pump_actor(&mut self, actor_id: &str) {
        if self.run_settled {
            return;
        }
        let Some(actor) = self.actors.get_mut(actor_id) else {
            return;
        };
        if actor.current.is_some() || actor.live_queue.is_empty() {
            return;
        }
        if self.active_asks >= self.caps.max_concurrency as usize {
            return;
        }
        let key = actor.live_queue.pop_front().expect("non-empty checked");
        actor.current = Some(key.clone());
        self.active_asks += 1;
        self.intents.push(DispatchIntent {
            actor_id: actor_id.to_string(),
            instance_key: key,
        });
    }

    // ——————————————————————— reporting upward (Boundary B) ———————————————————————

    /// A submit_result arrived (handleSubmitAttempted of scheduler-submit.ts).
    pub fn submit_attempted(&mut self, instance: &InstanceRef, payload: Value) {
        let key = instance.key();
        let (typed, settled, schema) = match self.live_nodes.get(&key) {
            Some(node) => (node.spec.typed, node.settled, node.spec.schema.clone()),
            None => return,
        };
        if settled || !typed {
            return;
        }
        let (value, violations) = normalize_submit(&self.validate, schema.as_ref(), payload);
        if violations.is_empty() {
            self.driver
                .respond_to_submit(instance, SubmitVerdict::Accept);
            self.settle_ok(&key, value);
            return;
        }
        let repairs_left = self
            .live_nodes
            .get(&key)
            .map(|n| n.repairs_remaining)
            .unwrap_or(0);
        if repairs_left > 0 {
            if let Some(node) = self.live_nodes.get_mut(&key) {
                node.repairs_remaining -= 1;
            }
            let attempt = REPAIR_ATTEMPTS - repairs_left + 1;
            self.driver.respond_to_submit(
                instance,
                SubmitVerdict::Reject {
                    violations: violations.clone(),
                },
            );
            self.record(RunEvent::NodeRepairing {
                instance: instance.clone(),
                attempt,
                violations,
            });
            return;
        }
        self.driver.cancel_ask(instance);
        let err = WorkflowError::new(
            WorkflowErrorCode::ValidationFailed,
            "submit_result failed schema validation repeatedly and the repair budget is exhausted.",
        )
        .with_violations(violations);
        self.settle_failed(&key, err);
    }

    /// The turn ended without submitting (handleTurnEnded of scheduler-submit.ts).
    pub fn turn_ended(&mut self, instance: &InstanceRef, final_text: &str) {
        let key = instance.key();
        let (typed, settled) = match self.live_nodes.get(&key) {
            Some(node) => (node.spec.typed, node.settled),
            None => return,
        };
        if settled {
            return;
        }
        if !typed {
            self.settle_ok(&key, Value::String(final_text.to_string()));
            return;
        }
        let nudges_left = self
            .live_nodes
            .get(&key)
            .map(|n| n.nudges_remaining)
            .unwrap_or(0);
        if nudges_left > 0 {
            if let Some(node) = self.live_nodes.get_mut(&key) {
                node.nudges_remaining -= 1;
            }
            self.driver
                .respond_to_submit(instance, SubmitVerdict::Nudge);
            self.record(RunEvent::NodeNudged {
                instance: instance.clone(),
            });
            return;
        }
        self.driver.cancel_ask(instance);
        let err = WorkflowError::new(
            WorkflowErrorCode::ResultNotSubmitted,
            "The typed ask ended without a submit_result call, so there is no result.",
        )
        .with_final_text(final_text);
        self.settle_failed(&key, err);
    }

    pub fn note_stats(&mut self, instance: &InstanceRef, stats: AskStats) {
        let key = instance.key();
        if let Some(node) = self.live_nodes.get_mut(&key) {
            node.last_stats = Some(stats);
            return;
        }
        // The node has already left liveNodes (the typed-accept-led path): backfill the settled journal record.
        if let Some(recorded) =
            self.journal()
                .get_node(&self.run_id, &instance.site_id, instance.ordinal)
        {
            let mut updated = recorded;
            updated.stats = Some(stats);
            self.journal().put_node(updated);
        }
    }

    pub fn failed_ask(&mut self, instance: &InstanceRef, error: WorkflowError) {
        let key = instance.key();
        let eligible = matches!(self.live_nodes.get(&key), Some(node) if !node.settled);
        if eligible {
            self.settle_failed(&key, error);
        }
    }

    /// Whether this instance is still an in-flight (admitted, not yet settled) live ask.
    pub fn is_live(&self, instance: &InstanceRef) -> bool {
        matches!(self.live_nodes.get(&instance.key()), Some(node) if !node.settled)
    }

    /// The effective name of the subagent that owns an in-flight ask (used to name it in the gate-closing event).
    pub fn live_actor_name(&self, instance: &InstanceRef) -> Option<String> {
        let node = self.live_nodes.get(&instance.key())?;
        if node.settled {
            return None;
        }
        self.actors
            .get(&node.actor.key())
            .and_then(|a| a.persona.name.clone())
    }

    // ——————————————————————— settlement ———————————————————————

    fn settle_ok(&mut self, key: &str, value: Value) {
        let Some(mut node) = self.live_nodes.remove(key) else {
            return;
        };
        if node.settled {
            self.live_nodes.insert(key.to_string(), node);
            return;
        }
        node.settled = true;
        self.journal()
            .put_node(self.node_record_for(&node, SettleOutcome::Completed(value.clone())));
        self.record(RunEvent::NodeSettled {
            instance: node.instance.clone(),
            outcome: NodeOutcome::Ok,
            cached: None,
            error: None,
            phase_name: None,
        });
        self.finish_live_node(&node);
        if let Some(sender) = node.sender.take() {
            let _ = sender.send(Ok(value));
        }
    }

    fn settle_failed(&mut self, key: &str, error: WorkflowError) {
        let Some(mut node) = self.live_nodes.remove(key) else {
            return;
        };
        if node.settled {
            self.live_nodes.insert(key.to_string(), node);
            return;
        }
        node.settled = true;
        let json = error.to_json();
        self.journal()
            .put_node(self.node_record_for(&node, SettleOutcome::Failed(json.clone())));
        self.record(RunEvent::NodeSettled {
            instance: node.instance.clone(),
            outcome: NodeOutcome::Failed,
            cached: None,
            error: Some(json),
            phase_name: None,
        });
        self.finish_live_node(&node);
        // A node failure only rejects that ask, it does not fail the whole run (the script can try/catch).
        if let Some(sender) = node.sender.take() {
            let _ = sender.send(Err(error));
        }
    }

    fn finish_live_node(&mut self, node: &AskNode) {
        let actor_id = node.actor.key();
        if let Some(actor) = self.actors.get_mut(&actor_id) {
            if actor.current.as_deref() == Some(node.instance.key().as_str()) {
                actor.current = None;
                self.active_asks = self.active_asks.saturating_sub(1);
            }
        }
        self.pump_all();
    }

    fn node_record_for(&self, node: &AskNode, outcome: SettleOutcome) -> NodeRecord {
        let mut record = NodeRecord {
            run_id: self.run_id.clone(),
            site_id: node.instance.site_id.clone(),
            ordinal: node.instance.ordinal,
            kind: NodeKind::Ask,
            actor_site_id: Some(node.actor.site_id.clone()),
            actor_ordinal: Some(node.actor.ordinal),
            actor_seq: Some(node.actor_seq),
            input_hash: node.hash.clone(),
            status: NodeRecordStatus::Completed,
            result: None,
            error: None,
            stats: node.last_stats.clone(),
            artifact_id: None,
            input: None,
            message_boundary: None,
        };
        match outcome {
            SettleOutcome::Completed(result) => {
                record.status = NodeRecordStatus::Completed;
                record.result = Some(result);
            }
            SettleOutcome::Failed(error) => {
                record.status = NodeRecordStatus::Failed;
                record.error = Some(error);
            }
        }
        record
    }
}

#[allow(clippy::large_enum_variant)]
enum SettleOutcome {
    Completed(Value),
    Failed(crate::types::WorkflowErrorJson),
}

/// Abort all in-flight asks: used when the run is cancelled / fails / completes.
pub(crate) fn abort_in_flight(core: &mut Core, error: WorkflowError, emit_cancelled: bool) {
    let nodes: Vec<AskNode> = core.live_nodes.drain().map(|(_, node)| node).collect();
    for mut node in nodes {
        if node.settled {
            continue;
        }
        node.settled = true;
        if node.dispatched {
            core.driver.cancel_ask(&node.instance);
        }
        if emit_cancelled {
            core.record(RunEvent::NodeSettled {
                instance: node.instance.clone(),
                outcome: NodeOutcome::Cancelled,
                cached: None,
                error: None,
                phase_name: None,
            });
        }
        if let Some(sender) = node.sender.take() {
            let _ = sender.send(Err(error.clone()));
        }
    }
    core.active_asks = 0;
}

/// A replay hit whose inputHash disagrees -- the purity contract is broken, so the run fails loudly.
pub(crate) fn hash_mismatch(instance: &InstanceRef, expected: &str, got: &str) -> WorkflowError {
    WorkflowError::new(
        WorkflowErrorCode::InputHashMismatch,
        format!(
            "Replay hit at {} but inputHash differs (expected {expected}, got {got}): the script is \
             not deterministic, so the journal cannot be replayed.",
            instance.key()
        ),
    )
    .with_mismatch(expected, got)
}

/// Persona normalization: a string is treated as the system prompt; the display name lands in persona.name.
fn normalize_persona(name: Option<String>, persona: Option<Value>) -> PersonaSpec {
    let mut base = match persona {
        Some(Value::String(system)) => PersonaSpec {
            name: None,
            system: Some(system),
        },
        Some(Value::Object(map)) => PersonaSpec {
            name: map.get("name").and_then(Value::as_str).map(str::to_string),
            system: map
                .get("system")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        _ => PersonaSpec::default(),
    };
    if base.name.is_none() {
        base.name = name;
    }
    base
}

/// The head of the author's instructions (trimmed at both ends, no ellipsis appended; an empty instruction is absent).
fn head_of_instructions(instructions: &str) -> Option<String> {
    let trimmed = instructions.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= INSTRUCTIONS_HEAD_MAX_CHARS {
        Some(trimmed.to_string())
    } else {
        Some(trimmed.chars().take(INSTRUCTIONS_HEAD_MAX_CHARS).collect())
    }
}

/// Lenient normalization of a submit payload: validate the raw value first; only when the raw value fails and is a string, do one JSON.parse and validate again.
fn normalize_submit(
    validate: &crate::types::ValidateFn,
    schema: Option<&Value>,
    payload: Value,
) -> (Value, Vec<Violation>) {
    let direct = validate(schema, &payload);
    if direct.is_empty() {
        return (payload, Vec::new());
    }
    if let Value::String(text) = &payload {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            let after = validate(schema, &parsed);
            if after.is_empty() {
                return (parsed, Vec::new());
            }
            // A violation must describe the value the model can change (the parsed object), not the string before parsing.
            return (parsed, after);
        }
    }
    (payload, direct)
}

fn rejected(error: WorkflowError) -> AskReceiver {
    let (tx, rx) = oneshot::channel();
    let _ = tx.send(Err(error));
    rx
}

impl Core {
    /// The dispatch finalization data for one ask (used to build the AskMessage outside the lock).
    pub(crate) fn take_dispatch(&mut self, intent: &DispatchIntent) -> Option<DispatchData> {
        let node = self.live_nodes.get(&intent.instance_key)?;
        if node.settled || self.run_settled {
            return None;
        }
        let actor = self.actors.get(&intent.actor_id)?;
        Some(DispatchData {
            instance: node.instance.clone(),
            actor_ref: actor.ref_.clone(),
            persona: actor.persona.clone(),
            session: actor.session.clone(),
            message: AskMessage {
                instructions: node.instructions.clone(),
                typed: node.spec.typed,
                schema: node.spec.schema.clone(),
            },
        })
    }

    /// Once the session is ready: record node-dispatched, mark dispatched, and write the session back to the actor (also preserving resolvedModel).
    pub(crate) fn mark_dispatched(
        &mut self,
        intent: &DispatchIntent,
        session: &crate::types::SessionRef,
    ) {
        if self.run_settled {
            return;
        }
        let Some(actor) = self.actors.get_mut(&intent.actor_id) else {
            return;
        };
        actor.session = Some(session.clone());
        let actor_ref = actor.ref_.clone();
        let name = actor.name.clone();
        let persona = actor.persona.clone();
        let existing =
            self.journal()
                .get_actor(&self.run_id, &actor_ref.site_id, actor_ref.ordinal);
        self.journal().put_actor(crate::types::ActorRecord {
            run_id: self.run_id.clone(),
            site_id: actor_ref.site_id.clone(),
            ordinal: actor_ref.ordinal,
            name,
            persona: Some(persona),
            session_id: Some(session.id.clone()),
            resolved_model: existing.as_ref().and_then(|e| e.resolved_model.clone()),
        });
        let Some(node) = self.live_nodes.get_mut(&intent.instance_key) else {
            return;
        };
        if node.settled {
            return;
        }
        let instance = node.instance.clone();
        self.record(RunEvent::NodeDispatched { instance });
        if let Some(node) = self.live_nodes.get_mut(&intent.instance_key) {
            node.dispatched = true;
        }
    }

    /// Session creation failed: carry the cause text into message (the journal persists only code/message).
    pub(crate) fn dispatch_session_failed(&mut self, intent: &DispatchIntent, message: String) {
        if self.run_settled {
            return;
        }
        let key = intent.instance_key.clone();
        if self.live_nodes.get(&key).map(|n| n.settled).unwrap_or(true) {
            return;
        }
        let err = WorkflowError::new(
            WorkflowErrorCode::DriverError,
            format!("Failed to create the subagent session: {message}"),
        );
        self.settle_failed_public(&key, err);
    }

    pub(crate) fn settle_failed_public(&mut self, key: &str, error: WorkflowError) {
        self.settle_failed(key, error);
    }

    /// The node lookup before dispatch: the node must still be live and unsettled.
    pub(crate) fn node_live(&self, key: &str) -> bool {
        self.live_nodes
            .get(key)
            .map(|n| !n.settled)
            .unwrap_or(false)
    }

    /// The session seed for an ask (a diverged actor); the outer layer fetches it before create_actor_session.
    pub(crate) fn actor_seed(&self, actor_id: &str) -> Option<crate::types::ActorSessionSeed> {
        self.actors
            .get(actor_id)
            .and_then(|a| a.imported.as_ref())
            .and_then(|i| i.seed())
    }
}

/// The data snapshot needed for dispatch outside the lock.
pub(crate) struct DispatchData {
    pub instance: InstanceRef,
    pub actor_ref: ActorRef,
    pub persona: PersonaSpec,
    pub session: Option<crate::types::SessionRef>,
    pub message: AskMessage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal_memory::InMemoryJournalStore;
    use crate::types::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct RecorderDriver {
        journal: Arc<InMemoryJournalStore>,
        events: Mutex<Vec<RunEvent>>,
        started: Mutex<Vec<(String, u32)>>,
    }

    impl RecorderDriver {
        fn new() -> Self {
            Self {
                journal: Arc::new(InMemoryJournalStore::new()),
                events: Mutex::new(Vec::new()),
                started: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl WorkflowDriver for RecorderDriver {
        fn journal(&self) -> &dyn JournalStorePort {
            &*self.journal
        }
        fn emit(&self, event: &RunEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
        async fn create_actor_session(
            &self,
            actor: &ActorRef,
            _persona: &PersonaSpec,
            _seed: Option<ActorSessionSeed>,
        ) -> Result<SessionRef, WorkflowError> {
            Ok(SessionRef {
                id: format!("s-{}", actor.key()),
            })
        }
        fn start_ask(&self, _session: &SessionRef, instance: &InstanceRef, _message: AskMessage) {
            self.started
                .lock()
                .unwrap()
                .push((instance.site_id.clone(), instance.ordinal));
        }
        fn respond_to_submit(&self, _instance: &InstanceRef, _verdict: SubmitVerdict) {}
        fn cancel_ask(&self, _instance: &InstanceRef) {}
        async fn execute_world_read(
            &self,
            _op: &str,
            _args: Vec<Value>,
        ) -> Result<Value, WorkflowError> {
            Ok(Value::Null)
        }
    }

    fn core_with(driver: Arc<RecorderDriver>) -> Core {
        crate::engine::test_core(
            "run-1",
            driver,
            Caps { max_concurrency: 2 },
            Arc::new(|_schema: Option<&Value>, _value: &Value| Vec::new()),
        )
    }

    #[test]
    fn ask_queues_and_dispatches_after_session() {
        let driver = Arc::new(RecorderDriver::new());
        let mut core = core_with(driver.clone());
        core.create_actor("actor#1", Some("planner".into()), None)
            .unwrap();
        let mut asks = HashMap::new();
        asks.insert(
            "ask#1".to_string(),
            AskSpec {
                typed: false,
                schema: None,
            },
        );
        core.ask_specs = asks;
        let _rx = core.admit_ask(
            "ask#1",
            "actor#1@1",
            "do it",
            AskSpec {
                typed: false,
                schema: None,
            },
        );
        // The session is not created yet: not dispatched, but the node is already in running.
        let record = core.journal().get_node("run-1", "ask#1", 1).unwrap();
        assert_eq!(record.status, NodeRecordStatus::Running);
        let intents = core.take_intents();
        assert_eq!(intents.len(), 1);
        // Simulate the outer layer completing dispatch
        let session = SessionRef {
            id: "s-actor#1@1".into(),
        };
        core.mark_dispatched(&intents[0], &session);
        let Some(data) = core.take_dispatch(&intents[0]) else {
            panic!("node should be live")
        };
        core.driver
            .start_ask(&session, &data.instance, data.message);
        assert_eq!(driver.started.lock().unwrap().len(), 1);
        // turn ends -> settled as untyped finalText
        core.turn_ended(
            &InstanceRef {
                site_id: "ask#1".into(),
                ordinal: 1,
            },
            "hello",
        );
        let record = core.journal().get_node("run-1", "ask#1", 1).unwrap();
        assert_eq!(record.status, NodeRecordStatus::Completed);
        assert_eq!(record.result, Some(json!("hello")));
    }

    #[test]
    fn duplicate_actor_name_fails_the_run() {
        let driver = Arc::new(RecorderDriver::new());
        let mut core = core_with(driver);
        core.create_actor("actor#1", Some("dup".into()), None)
            .unwrap();
        let err = core
            .create_actor("actor#2", Some("dup".into()), None)
            .expect_err("duplicate name must fail");
        assert_eq!(err.code, WorkflowErrorCode::DuplicateActorName);
        assert!(core.run_settled);
    }

    #[test]
    fn replay_rejects_input_hash_mismatch() {
        let driver = Arc::new(RecorderDriver::new());
        let mut core = core_with(driver.clone());
        // Pre-write one running record (simulating the state before a crash)
        driver.journal.put_node(NodeRecord {
            run_id: "run-1".into(),
            site_id: "ask#1".into(),
            ordinal: 1,
            kind: NodeKind::Ask,
            actor_site_id: Some("actor#1".into()),
            actor_ordinal: Some(1),
            actor_seq: Some(0),
            input_hash: "deadbeef".into(),
            status: NodeRecordStatus::Running,
            result: None,
            error: None,
            stats: None,
            artifact_id: None,
            input: None,
            message_boundary: None,
        });
        core.create_actor("actor#1", None, None).unwrap();
        let _rx = core.admit_ask(
            "ask#1",
            "actor#1@1",
            "different",
            AskSpec {
                typed: false,
                schema: None,
            },
        );
        let failure = core.run_failure.clone().expect("must fail the run");
        assert_eq!(failure.code, WorkflowErrorCode::InputHashMismatch);
    }

    #[test]
    fn completed_replay_short_circuits_without_dispatch() {
        let driver = Arc::new(RecorderDriver::new());
        let mut core = core_with(driver.clone());
        let hash = input_hash(&json!("same"));
        driver.journal.put_node(NodeRecord {
            run_id: "run-1".into(),
            site_id: "ask#1".into(),
            ordinal: 1,
            kind: NodeKind::Ask,
            actor_site_id: Some("actor#1".into()),
            actor_ordinal: Some(1),
            actor_seq: Some(0),
            input_hash: hash,
            status: NodeRecordStatus::Completed,
            result: Some(json!("cached answer")),
            error: None,
            stats: None,
            artifact_id: None,
            input: None,
            message_boundary: None,
        });
        core.create_actor("actor#1", None, None).unwrap();
        let mut rx = core.admit_ask(
            "ask#1",
            "actor#1@1",
            "same",
            AskSpec {
                typed: false,
                schema: None,
            },
        );
        let received = rx.try_recv().expect("must resolve immediately");
        assert_eq!(received.unwrap(), json!("cached answer"));
        assert_eq!(core.take_intents().len(), 0, "no dispatch on replay hit");
        assert!(driver.started.lock().unwrap().is_empty());
    }

    #[test]
    fn typed_submit_repairs_then_settles() {
        let driver = Arc::new(RecorderDriver::new());
        let mut core = core_with(driver.clone());
        // Validator: accepts only {ok:true}
        core.validate = Arc::new(|_schema: Option<&Value>, value: &Value| {
            if value.get("ok") == Some(&json!(true)) {
                Vec::new()
            } else {
                vec![Violation {
                    path: "$.ok".into(),
                    expected: "true".into(),
                    got: "false".into(),
                }]
            }
        });
        core.create_actor("actor#1", None, None).unwrap();
        let spec = AskSpec {
            typed: true,
            schema: Some(json!({"type": "object"})),
        };
        let mut rx = core.admit_ask("ask#1", "actor#1@1", "give ok", spec);
        let intents = core.take_intents();
        core.mark_dispatched(&intents[0], &SessionRef { id: "s".into() });
        let instance = InstanceRef {
            site_id: "ask#1".into(),
            ordinal: 1,
        };
        core.submit_attempted(&instance, json!({"ok": false}));
        let repairing = core
            .journal()
            .list_events("run-1", ListEventsOptions::default())
            .into_iter()
            .any(|e| matches!(e.event, RunEvent::NodeRepairing { .. }));
        assert!(repairing, "first invalid submit must emit node-repairing");
        core.submit_attempted(&instance, json!({"ok": true}));
        let settled = rx
            .try_recv()
            .expect("must settle after valid submit")
            .unwrap();
        assert_eq!(settled, json!({"ok": true}));
    }

    #[test]
    fn typed_turn_end_nudges_once_then_fails() {
        let driver = Arc::new(RecorderDriver::new());
        let mut core = core_with(driver);
        core.create_actor("actor#1", None, None).unwrap();
        let spec = AskSpec {
            typed: true,
            schema: None,
        };
        let mut rx = core.admit_ask("ask#1", "actor#1@1", "typed", spec);
        let intents = core.take_intents();
        core.mark_dispatched(&intents[0], &SessionRef { id: "s".into() });
        let instance = InstanceRef {
            site_id: "ask#1".into(),
            ordinal: 1,
        };
        core.turn_ended(&instance, "no submit yet");
        let nudged = core
            .journal()
            .list_events("run-1", ListEventsOptions::default())
            .into_iter()
            .any(|e| matches!(e.event, RunEvent::NodeNudged { .. }));
        assert!(nudged);
        core.turn_ended(&instance, "still nothing");
        let err = rx.try_recv().expect("must reject").unwrap_err();
        assert_eq!(err.code, WorkflowErrorCode::ResultNotSubmitted);
        assert_eq!(err.final_text.as_deref(), Some("still nothing"));
    }
}
