//! Engine private state (port of the seam in `engine-state.ts` + the fields of `engine.ts`).
//!
//! The Rust side has no TS private-field + arrow-closure seam: the state is one [`Core`], whose synchronous methods are
//! implemented module by module as `impl Core` in `scheduler.rs` / `engine_artifacts.rs` / `engine_report.rs` /
//! `engine_world.rs` / `engine_settlement.rs`. Concurrency comes from the outer `WorkflowEngine`'s
//! `Mutex<Core>`; the core itself is still a deterministic state machine with zero I/O, no clock and no randomness.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::oneshot;

use crate::imported_cache::{ImportedActorState, ImportedWorldQueue};
use crate::types::{
    ActorId, ActorRef, AskSpec, AskStats, Caps, ImportedRunCache, InstanceRef, JournalStorePort,
    NodeRecord, PersonaSpec, RunEvent, SessionRef, ValidateFn, WorkflowDriver, WorkflowError,
    WorkflowErrorCode,
};

/// The ask settlement channel: resolved to the script side at settle time.
pub(crate) type AskSender = oneshot::Sender<Result<Value, WorkflowError>>;

/// The state of one user-facing artifact id within this run (all three are derived from journal rows).
#[derive(Clone, Debug)]
pub(crate) struct ArtifactIdState {
    pub kind: crate::types::ArtifactOp,
    pub versions: u32,
    /// The canonical JSON of the preset spec (for the idempotence check on a repeated declaration).
    pub spec: Option<String>,
    pub primary: bool,
}

/// The admission data for one ask (the pending queue carries the sender).
pub(crate) struct AskAdmission {
    pub instance: InstanceRef,
    pub instructions: String,
    pub hash: String,
    pub spec: AskSpec,
    pub sender: AskSender,
}

/// The release action for a recorded node that has arrived but is not yet admitted (TS's pendingRecorded closure replaced by an enum).
#[allow(clippy::large_enum_variant)]
pub(crate) enum PendingRecorded {
    Live(AskAdmission),
    Cached(AskAdmission, NodeRecord),
}

/// One live ask node (one that must actually be dispatched).
pub(crate) struct AskNode {
    pub instance: InstanceRef,
    pub actor: ActorRef,
    pub actor_seq: u64,
    pub instructions: String,
    pub hash: String,
    pub spec: AskSpec,
    pub sender: Option<AskSender>,
    pub repairs_remaining: u32,
    pub nudges_remaining: u32,
    pub settled: bool,
    pub dispatched: bool,
    pub last_stats: Option<AskStats>,
}

/// The actor runtime state maintained by the scheduler.
pub(crate) struct Actor {
    pub id: ActorId,
    pub ref_: ActorRef,
    pub persona: PersonaSpec,
    pub name: Option<String>,
    /// How many ask nodes of this actor are already recorded in the journal -- during replay a live node may only be released once all of them have been admitted.
    pub recorded_count: usize,
    pub next_admit_seq: u64,
    pub pending_recorded: BTreeMap<u64, PendingRecorded>,
    pub pending_live: VecDeque<AskAdmission>,
    /// Admitted live nodes waiting to be dispatched (FIFO = admission order), stored by instance key.
    pub live_queue: VecDeque<String>,
    /// The instance key of the live node currently executing (an actor is serial, so at most one).
    pub current: Option<String>,
    /// The session is created lazily, once per actor.
    pub session: Option<SessionRef>,
    pub imported: Option<ImportedActorState>,
}

/// One dispatch intent: scheduling decides it inside the lock, and the outer layer performs the async session creation / dequeue outside the lock.
pub(crate) struct DispatchIntent {
    pub actor_id: ActorId,
    pub instance_key: String,
}

/// The engine core state.
pub(crate) struct Core {
    pub run_id: String,
    pub driver: Arc<dyn WorkflowDriver>,
    pub caps: Caps,
    pub ask_specs: HashMap<String, AskSpec>,
    pub validate: ValidateFn,

    /// The per-site execution ordinal counter (shared by ask / world-read / actor, whose site ids are mutually distinct).
    pub ordinals: HashMap<String, u32>,
    /// How many times each phase has been entered (keyed by name), in a table separate from the site ordinals.
    pub phase_ordinals: HashMap<String, u64>,
    /// The name of the phase the control flow is currently in.
    pub current_phase: Option<String>,
    /// Instance (`site@ordinal`) → the phase name at its birth.
    pub instance_phases: HashMap<String, String>,
    /// Non-empty effective actor names already claimed in this run → the actor id that claimed them.
    pub actor_names: HashMap<String, String>,

    pub imported_cache: Option<ImportedRunCache>,
    pub imported_world: ImportedWorldQueue,
    pub import_closed: bool,
    /// The "ask instances that ran live before the crash", restored from events on resume.
    pub live_ask_instances: HashSet<String>,

    pub report_count: usize,
    pub artifacts: HashMap<String, ArtifactIdState>,
    pub spent_tokens: u64,

    pub run_settled: bool,
    pub run_failure: Option<WorkflowError>,

    // —— scheduler state ——
    pub actors: HashMap<ActorId, Actor>,
    pub actor_order: Vec<ActorId>,
    pub live_nodes: HashMap<String, AskNode>,
    pub active_asks: usize,

    /// Dispatch intents for outside the lock (the outer layer executes them after releasing the lock and tokio::spawns them).
    pub intents: Vec<DispatchIntent>,
    /// The resolution channel for the run settlement (the outer layer holds the receiver).
    pub settled_tx: Option<oneshot::Sender<crate::types::RunSettlement>>,
}

impl Core {
    pub fn journal(&self) -> &dyn JournalStorePort {
        self.driver.journal()
    }

    pub fn is_run_settled(&self) -> bool {
        self.run_settled
    }

    pub fn run_error(&self) -> WorkflowError {
        self.run_failure.clone().unwrap_or_else(|| {
            WorkflowError::new(WorkflowErrorCode::Cancelled, "Run already settled.")
        })
    }

    pub fn close_import(&mut self) {
        self.import_closed = true;
    }

    pub fn report_count(&self) -> usize {
        self.report_count
    }

    pub fn count_report(&mut self) {
        self.report_count += 1;
    }

    /// Mark the run as settled; when a failure is supplied, also record the run-level failure reason.
    pub fn mark_settled(&mut self, failure: Option<WorkflowError>) {
        self.run_settled = true;
        if failure.is_some() {
            self.run_failure = failure;
        }
    }

    /// Run-level failure (first-wins; implemented in engine_settlement.rs).
    pub fn fail_run(&mut self, error: WorkflowError) {
        crate::engine_settlement::settle_failed(self, error);
    }

    /// Resolve the run settlement promise.
    pub fn resolve_settled(&mut self, settlement: crate::types::RunSettlement) {
        if let Some(tx) = self.settled_tx.take() {
            let _ = tx.send(settlement);
        }
    }

    /// The single minting point for site ordinals (createActor / admitAsk / worldRead / report / artifact all pass through it synchronously),
    /// which also records the instance's birth phase.
    pub fn next_ordinal(&mut self, site_id: &str) -> u32 {
        let next = self.ordinals.get(site_id).copied().unwrap_or(0) + 1;
        self.ordinals.insert(site_id.to_string(), next);
        if let Some(phase) = self.current_phase.clone() {
            self.instance_phases
                .insert(crate::types::ref_to_string(site_id, next), phase);
        }
        next
    }

    /// An event both lands in the journal and fans out through driver.emit; the birth phase is stamped on here.
    pub fn record(&mut self, event: RunEvent) {
        let stamped = self.stamp_birth_phase(event);
        let stored = self.journal().append_event(&self.run_id, &stamped);
        self.driver.emit_recorded(&stamped, stored.sequence);
    }

    fn stamp_birth_phase(&self, event: RunEvent) -> RunEvent {
        match event {
            RunEvent::ActorCreated {
                actor,
                name,
                persona,
                phase_name,
            } => match self.instance_phases.get(&actor.key()) {
                Some(phase) if phase_name.is_none() => RunEvent::ActorCreated {
                    actor,
                    name,
                    persona,
                    phase_name: Some(phase.clone()),
                },
                _ => RunEvent::ActorCreated {
                    actor,
                    name,
                    persona,
                    phase_name,
                },
            },
            RunEvent::NodeQueued {
                instance,
                kind,
                actor,
                actor_seq,
                phase_name,
                instructions_head,
            } => match self.instance_phases.get(&instance.key()) {
                Some(phase) if phase_name.is_none() => RunEvent::NodeQueued {
                    instance,
                    kind,
                    actor,
                    actor_seq,
                    phase_name: Some(phase.clone()),
                    instructions_head,
                },
                _ => RunEvent::NodeQueued {
                    instance,
                    kind,
                    actor,
                    actor_seq,
                    phase_name,
                    instructions_head,
                },
            },
            RunEvent::NodeSettled {
                instance,
                outcome,
                cached,
                error,
                phase_name,
            } => match (cached, self.instance_phases.get(&instance.key())) {
                (Some(true), Some(phase)) if phase_name.is_none() => RunEvent::NodeSettled {
                    instance,
                    outcome,
                    cached,
                    error,
                    phase_name: Some(phase.clone()),
                },
                _ => RunEvent::NodeSettled {
                    instance,
                    outcome,
                    cached,
                    error,
                    phase_name,
                },
            },
            other => other,
        }
    }

    pub fn take_intents(&mut self) -> Vec<DispatchIntent> {
        std::mem::take(&mut self.intents)
    }

    /// Abort in-flight asks (used when the run is cancelled / fails / completes). Implemented in scheduler.rs.
    pub fn abort_in_flight(&mut self, error: WorkflowError, emit_cancelled: bool) {
        crate::scheduler::abort_in_flight(self, error, emit_cancelled);
    }
}
