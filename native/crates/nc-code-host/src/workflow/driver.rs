//! Host implementation of `nc_code_workflow::types::WorkflowDriver`: drop the engine's downward side effect port to the real
//! host subsession, world reading and product storage (port of the orchestration part of `workflow-driver.ts`).
//!
//! Ownership and Boundaries:
//! - The engine owns the journal's **decisions** (which events and when to settle); this driver only calls `JournalStorePort`
//! Account for driver-private facts (actor session id / resolved model / ask message boundary) and upgrade Q&A
//! Two events (same shape as TS's `record()`: journal first and then emit).
//! - `emit` only forwards `RunEvent` JSON and does not interpret events (run service is responsible for projection).
//! - This driver does not create the twelve model tools of the workflow, nor does it start run; these belong to the run service.
//! - Session identity: The host's `open_session` casts the id by itself, and there is no API for "creating a session with an id", so
//! `mintActorSessionKey` (deterministic string) is only used as a key in the driver registry; the real session id is written in
//! `dwf_actor.session_id`, when resuming, press that record to resuspend (the record is cross-process authoritative). See `transcript.rs` for details.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use async_trait::async_trait;
use nc_code_workflow::types::{
    ActorRecord, ActorRef, ActorSessionSeed, ArtifactPublishRequest, ArtifactVersionRecord,
    AskMessage, InstanceRef, JournalStorePort, PersonaSpec, RunEvent, SessionRef, SubmitVerdict,
    WorkflowDriver, WorkflowError, WorkflowErrorCode, WorkflowReportSink,
};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::session::Session;
use crate::sessions::SessionSeed;
use crate::subagent::SUBAGENT_TASK_TYPE;
use crate::tools::{AsyncTool, Tool, ToolContext, ToolContract, ToolError, ToolRegistry};
use crate::turn::TurnDeps;
use crate::HostServer;

use crate::workflow::actor_tools::{EscalateTool, EscalationRegistry, SubmitResultTool};
use crate::workflow::artifacts::ArtifactStore;
use crate::workflow::ask::{self, AskState};
use crate::workflow::prompt;
use crate::workflow::transcript::{actor_session_key, copy_history_prefix};
use crate::workflow::world_read::{self, WorldReadScope};

/// Tool surface subtraction table for actor sessions (port of `workflow-actor-tools.ts`).
///
/// The first three categories will be blocked on a non-existent person (headless child no one responds, turn never ends);
/// `Agent`/`Task`/`SendMessage` are recursive/nested arrangements; `ReadSessionContext` reads the parent session out of bounds;
/// `ResolveWorkflowQuestion` is an **identity** question - the answer belongs to the main session only.
const ACTOR_DISALLOWED_TOOLS: &[&str] = &[
    "Agent",
    "Task",
    "SendMessage",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "ReadSessionContext",
    "CreateWorkflow",
    "AmendWorkflow",
    "ResolveWorkflowQuestion",
];

/// driver construction parameters.
#[derive(Clone, Debug)]
pub struct HostWorkflowDriverConfig {
    /// The id of this run (scope of actor session key and event journal).
    pub run_id: String,
    /// The data-dir of the host (the product store falls in `<data_dir>/workflow-artifacts`).
    pub data_dir: PathBuf,
    /// Set of approved commands for `world.run`; `None` denies all world.run (fail-closed).
    pub declared_run_commands: Option<BTreeSet<String>>,
    /// Optional real-time event fanout (one JSON per RunEvent). run service is projected here.
    pub events: Option<UnboundedSender<(u64, Value)>>,
}

/// Driver's private journal + emit combination. The order cannot be changed: first `append_event` and then emit (launch side
/// Sequence interception depends on "append followed by synchronization followed by emit of the same event").
#[derive(Clone)]
struct EventLog {
    journal: Arc<dyn JournalStorePort>,
    run_id: String,
    events: Option<UnboundedSender<(u64, Value)>>,
}

impl EventLog {
    fn record(&self, event: &RunEvent) {
        let stored = self.journal.append_event(&self.run_id, event);
        self.emit_with_sequence(event, stored.sequence, None);
    }

    fn emit(&self, event: &RunEvent) {
        self.emit_with_sequence(event, 0, None);
    }

    fn emit_with_sequence(&self, event: &RunEvent, sequence: u64, actor_session_id: Option<&str>) {
        if let Some(events) = &self.events {
            if let Ok(mut value) = serde_json::to_value(event) {
                if let Some(session) = actor_session_id {
                    if let Some(object) = value.as_object_mut() {
                        object.insert(
                            "__actorSessionId".to_string(),
                            Value::String(session.to_string()),
                        );
                    }
                }
                let _ = events.send((sequence, value));
            }
        }
    }
}

struct ActorSession {
    child: Arc<Session>,
    actor: ActorRef,
    name: Option<String>,
    persona_system: Option<String>,
}

/// The shared status of the drive. The only public side is [`HostWorkflowDriver`].
pub(crate) struct DriverInner {
    pub(crate) run_id: String,
    /// Weak: the run service owns this driver and the host owns the service, so a strong
    /// host handle here would close host → service → runs → driver → host and leak the
    /// compile kernel (and its Node child) past host shutdown.
    pub(crate) host: std::sync::Weak<HostServer>,
    /// The parent session of this run (the actor session inherits its run configuration; the session seed is also based on its parent id).
    parent: Arc<Session>,
    pub(crate) journal: Arc<dyn JournalStorePort>,
    event_log: EventLog,
    sink: Mutex<Option<Weak<dyn WorkflowReportSink>>>,
    pub(crate) escalations: Arc<EscalationRegistry>,
    /// host session id → actor session.
    sessions: Mutex<HashMap<String, Arc<ActorSession>>>,
    /// Deterministic key (`dwf-<run>-<site@ordinal>`) → host session id.
    actor_sessions: Mutex<HashMap<String, String>>,
    /// instance key → ask.
    pub(crate) asks: Mutex<HashMap<String, Arc<AskState>>>,
    /// host session id → instance key of the latest ask (remove the old docked item when changing ask).
    session_asks: Mutex<HashMap<String, String>>,
    pub(crate) world: WorldReadScope,
    artifacts: ArtifactStore,
    disposed: AtomicBool,
    /// Serialized session creation: Two concurrent dispatches of the same actor cannot each create an orphan session.
    creation: Mutex<()>,
    turn_counter: AtomicU64,
}

impl DriverInner {
    pub(crate) fn sink(&self) -> Option<Arc<dyn WorkflowReportSink>> {
        self.sink
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().and_then(|weak| weak.upgrade()))
    }

    pub(crate) fn report_mutating(&self, instance: &InstanceRef) {
        if let Some(sink) = self.sink() {
            sink.ask_mutating(instance);
        }
    }

    pub(crate) fn record_event(&self, event: &RunEvent) {
        self.event_log.record(event);
    }

    pub(crate) fn provider(&self) -> Option<crate::provider_config::ProviderConfig> {
        let host = self.host.upgrade()?;
        host.turn_deps.provider_config.as_deref().cloned()
    }

    pub(crate) fn provider_id(&self) -> Option<String> {
        let host = self.host.upgrade()?;
        host.turn_deps
            .provider_config
            .as_ref()
            .and_then(|config| config.provider_id.clone())
    }

    pub(crate) fn model_id(&self) -> Option<String> {
        let host = self.host.upgrade()?;
        host.turn_deps
            .provider_config
            .as_ref()
            .map(|config| config.model.clone())
    }

    pub(crate) fn next_turn_id(&self, instance: &InstanceRef, index: u64) -> String {
        let sequence = self.turn_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let site: String = instance
            .site_id
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        format!("wf-{site}-{}-{index}-{sequence}", instance.ordinal)
    }
}

/// The host implementation of `WorkflowDriver`.
pub struct HostWorkflowDriver {
    inner: Arc<DriverInner>,
}

impl HostWorkflowDriver {
    /// Assemble the driver. `journal` is passed in by run service (production is SQLite journal; testing available memory implementation);
    /// `parent` is the session that initiated this run (the actor session inherits its provider/model/mode).
    pub fn new(
        config: HostWorkflowDriverConfig,
        host: Arc<HostServer>,
        parent: Arc<Session>,
        journal: Arc<dyn JournalStorePort>,
    ) -> Arc<Self> {
        let event_log = EventLog {
            journal: Arc::clone(&journal),
            run_id: config.run_id.clone(),
            events: config.events.clone(),
        };
        let escalations = EscalationRegistry::new();
        // The dual-track implementation of upgrade events is the same as the engine's record() (journal + emit), but is only triggered by the driver.
        let emitter_log = event_log.clone();
        escalations.set_emitter(Arc::new(move |event: &RunEvent| {
            emitter_log.record(event);
        }));
        let workspace = host.workspace().clone();
        let inner = Arc::new(DriverInner {
            run_id: config.run_id.clone(),
            host: Arc::downgrade(&host),
            parent,
            journal,
            event_log,
            sink: Mutex::new(None),
            escalations,
            sessions: Mutex::new(HashMap::new()),
            actor_sessions: Mutex::new(HashMap::new()),
            asks: Mutex::new(HashMap::new()),
            session_asks: Mutex::new(HashMap::new()),
            world: WorldReadScope::new(workspace, config.declared_run_commands),
            artifacts: ArtifactStore::new(&config.data_dir),
            disposed: AtomicBool::new(false),
            creation: Mutex::new(()),
            turn_counter: AtomicU64::new(1),
        });
        Arc::new(Self { inner })
    }

    /// The engine hangs itself as a sink after construction (driver exists before sink). Use Weak: the engine holds the driver,
    /// The driver holding the engine will form a loop.
    pub fn attach_sink(&self, sink: Arc<dyn WorkflowReportSink>) {
        if let Ok(mut slot) = self.inner.sink.lock() {
            *slot = Some(Arc::downgrade(&sink));
        }
    }

    /// Upgrade the Q&A registry (run service queries/answers through it).
    pub fn escalation_registry(&self) -> Arc<EscalationRegistry> {
        Arc::clone(&self.inner.escalations)
    }

    /// The host session the actor currently falls into (progress projection associates the actor-created event with the session).
    pub fn session_for_actor(&self, actor: &ActorRef) -> Option<SessionRef> {
        let key = actor_session_key(&self.inner.run_id, actor);
        self.inner
            .actor_sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(&key).cloned())
            .map(|id| SessionRef { id })
    }

    /// The host session where the ask is located (run service is used to open the details page/count message boundaries).
    pub fn session_for_instance(&self, instance: &InstanceRef) -> Option<SessionRef> {
        self.inner
            .asks
            .lock()
            .ok()
            .and_then(|asks| asks.get(&instance.key()).cloned())
            .map(|ask| ask.session_ref())
    }

    /// Journal handle of driver private fact (used for testing and read-only assertions of run service).
    pub fn journal(&self) -> &dyn JournalStorePort {
        self.inner.journal.as_ref()
    }
}

#[async_trait]
impl WorkflowDriver for HostWorkflowDriver {
    fn journal(&self) -> &dyn JournalStorePort {
        self.inner.journal.as_ref()
    }

    fn emit(&self, event: &RunEvent) {
        //Only forward, not interpreted: projection is the responsibility of the run service.
        self.inner.event_log.emit(event);
    }

    fn emit_recorded(&self, event: &RunEvent, sequence: u64) {
        let actor_session_id = if let RunEvent::ActorCreated { actor, .. } = event {
            self.session_for_actor(actor).map(|session| session.id)
        } else {
            None
        };
        self.inner
            .event_log
            .emit_with_sequence(event, sequence, actor_session_id.as_deref());
    }

    async fn create_actor_session(
        &self,
        actor: &ActorRef,
        persona: &PersonaSpec,
        seed: Option<ActorSessionSeed>,
    ) -> Result<SessionRef, WorkflowError> {
        if self.inner.disposed.load(Ordering::SeqCst) {
            return Err(WorkflowError::new(
                WorkflowErrorCode::DriverError,
                "The workflow driver was disposed before this actor session could be created.",
            ));
        }
        // There is no await in the entire method body: it is safe to hold this lock until return, and make "the same actor only creates one session" become
        // Structural properties (journal reading, modification and writing are also within the lock).
        let _creating = self.inner.creation.lock().map_err(|_| {
            WorkflowError::new(
                WorkflowErrorCode::DriverError,
                "The workflow driver creation lock is poisoned.",
            )
        })?;
        let key = actor_session_key(&self.inner.run_id, actor);
        let Some(host) = self.inner.host.upgrade() else {
            return Err(WorkflowError::new(
                WorkflowErrorCode::DriverError,
                "The host was dropped before this workflow actor session could be created.",
            ));
        };
        if let Some(existing) = self
            .inner
            .actor_sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(&key).cloned())
        {
            return Ok(SessionRef { id: existing });
        }
        //resume mutual identity verification: journal's dwf_actor.session_id is a **record**. host cannot cast the same id,
        // So press it to rehang when the record exists; the rehang failure is structural (session is cleaned / data-dir is wrong), fails loudly
        // Instead of silently creating a new session without transcription.
        let journaled =
            self.inner
                .journal
                .get_actor(&self.inner.run_id, &actor.site_id, actor.ordinal);
        if let Some(journaled_id) = journaled
            .as_ref()
            .and_then(|record| record.session_id.clone())
        {
            let Some(child) = host.load_session(&journaled_id) else {
                return Err(WorkflowError::new(
                    WorkflowErrorCode::DriverError,
                    format!(
                        "Subagent session {} from the journal could not be loaded for {} on resume. \
                         The data directory may not contain this run's actor sessions.",journaled_id,
                        nc_code_workflow::types::ref_to_string(&actor.site_id, actor.ordinal)
                    ),
                )
                .with_mismatch(&journaled_id, "unloadable"));
            };
            // cold resume does not inherit the authorization bit when created; the actor sub-session does not have an interactive client, and non-read-only tools must
            // Run as authorized (same as the new path).
            child.set_full_access_flag(true);
            if let Some(seed) = &seed {
                if let Some(pin) = &seed.resolved_model {
                    apply_model_pin(&child, pin);
                }
                // Idempotent: replication will be skipped if the target session already has a history.
                copy_history_prefix(&host, &child, seed)?;
            }
            self.register_actor_session(&key, actor, persona, child.clone(), &journaled);
            return Ok(SessionRef {
                id: child.id.clone(),
            });
        }

        let persona_name = persona
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        let mut session_seed = SessionSeed::from_parent(&self.inner.parent);
        session_seed.title = persona_name.clone().unwrap_or_else(|| {
            format!(
                "workflow subagent {}",
                nc_code_workflow::types::ref_to_string(&actor.site_id, actor.ordinal)
            )
        });
        session_seed.title_source = "custom".to_string();
        session_seed.parent_session_id = Some(self.inner.parent.id.clone());
        session_seed.task_type = Some(SUBAGENT_TASK_TYPE.to_string());
        let child = host.open_session(session_seed).map_err(|error| {
            WorkflowError::new(
                WorkflowErrorCode::DriverError,
                format!(
                    "Cannot create a subagent session for the workflow actor: {}",
                    error.message
                ),
            )
        })?;
        //The actor child session inherits the authorization of this run (reaching this point means that the parent session has allowed the execution of this workflow);
        // It does not have its own interactive client, non-read-only tools must be run as authorized.
        child.set_full_access_flag(true);
        // Model pin: When the provider/model parsed by the precursor is used in the seed, it overrides the gear and is re-parsed (the model is changed silently under transcription continuation)
        // Exactly the identity mutation that pin is designed to guard against). The host's executor currently only recognizes the host-level provider configuration, here
        // The pin falls into the session meta and journal (see delivery report for actual limits).
        if let Some(seed) = &seed {
            if let Some(pin) = &seed.resolved_model {
                apply_model_pin(&child, pin);
            }
        }
        if let Some(seed) = &seed {
            copy_history_prefix(&host, &child, seed)?;
        }
        self.register_actor_session(&key, actor, persona, Arc::clone(&child), &journaled);
        Ok(SessionRef {
            id: child.id.clone(),
        })
    }

    fn start_ask(&self, session: &SessionRef, instance: &InstanceRef, message: AskMessage) {
        let inner = &self.inner;
        if inner.disposed.load(Ordering::SeqCst) {
            // Late distribution after settlement: there is no audience and there should not be another turn.
            return;
        }
        let Some(sink) = inner.sink() else {
            // Without a sink, there is no engine to report back to - this ask cannot be resolved. Write stderr loudly instead of faking it.
            eprintln!(
                "[nc-code-host] workflow ask {} dropped: the driver has no report sink attached (wiring error)",
                instance.key()
            );
            return;
        };
        let Some(actor_session) = inner
            .sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(&session.id).cloned())
        else {
            sink.ask_failed(
                instance,
                WorkflowError::new(
                    WorkflowErrorCode::DriverError,
                    format!("Unknown subagent session: {}", session.id),
                ),
            );
            return;
        };
        // The cancellation bit of the previous ask cannot be brought into the new ask (equivalent to the per-ask status of TS startAsk being reset to zero):
        // cancelAsk may occur between rounds, when the driver loop just sees the flag and exits, without consuming the session bit.
        let _ = actor_session.child.take_cancel();
        // If the previous ask left stops on the abnormal path, remove them here: as soon as the new ask starts, those problems will no longer exist.
        // No one will read the answer.
        if let Some(previous_key) = inner
            .session_asks
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(&session.id).cloned())
        {
            if let Some(previous) = inner
                .asks
                .lock()
                .ok()
                .and_then(|asks| asks.get(&previous_key).cloned())
            {
                if !previous.accepted() && !previous.cancelled() {
                    previous.withdraw_escalations();
                }
            }
        }
        let ask = AskState::new(
            Arc::downgrade(inner),
            instance.clone(),
            session.id.clone(),
            actor_session.actor.clone(),
            actor_session.name.clone(),
            message.typed,
            Arc::clone(&actor_session.child),
            Arc::clone(&inner.escalations),
        );
        if let Ok(mut asks) = inner.asks.lock() {
            asks.insert(instance.key(), Arc::clone(&ask));
        }
        if let Ok(mut sessions) = inner.session_asks.lock() {
            sessions.insert(session.id.clone(), instance.key());
        }
        let Some(host) = inner.host.upgrade() else {
            sink.ask_failed(
                instance,
                WorkflowError::new(
                    WorkflowErrorCode::DriverError,
                    "The host was dropped before this workflow ask could start.",
                ),
            );
            return;
        };
        let deps = build_turn_deps(inner, &host, &actor_session, &ask);
        let instructions = prompt::ask_prompt(
            &message.instructions,
            message.typed,
            message.schema.as_ref(),
        );
        let provider = inner.provider();
        let child = Arc::clone(&actor_session.child);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(ask::drive_ask(ask, child, provider, deps, instructions));
            }
            Err(_) => sink.ask_failed(
                instance,
                WorkflowError::new(
                    WorkflowErrorCode::DriverError,
                    "Cannot start a workflow ask outside a tokio runtime.",
                ),
            ),
        }
    }

    fn respond_to_submit(&self, instance: &InstanceRef, verdict: SubmitVerdict) {
        let ask = self
            .inner
            .asks
            .lock()
            .ok()
            .and_then(|asks| asks.get(&instance.key()).cloned());
        if let Some(ask) = ask {
            ask.apply_verdict(verdict);
        }
    }

    fn cancel_ask(&self, instance: &InstanceRef) {
        let ask = self
            .inner
            .asks
            .lock()
            .ok()
            .and_then(|asks| asks.get(&instance.key()).cloned());
        if let Some(ask) = ask {
            ask.cancel();
        }
    }

    async fn execute_world_read(&self, op: &str, args: Vec<Value>) -> Result<Value, WorkflowError> {
        world_read::execute(&self.inner.world, op, &args).await
    }

    async fn execute_artifact_publish(
        &self,
        request: ArtifactPublishRequest,
    ) -> Result<ArtifactVersionRecord, WorkflowError> {
        let workspace = self
            .inner
            .host
            .upgrade()
            .map(|host| host.workspace().clone())
            .ok_or_else(|| {
                WorkflowError::new(
                    WorkflowErrorCode::DriverError,
                    "The host was dropped before this artifact could be published.",
                )
            })?;
        self.inner.artifacts.publish(&workspace, request).await
    }

    fn dispose(&self) {
        if self.inner.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let asks: Vec<Arc<AskState>> = self
            .inner
            .asks
            .lock()
            .map(|asks| asks.values().cloned().collect())
            .unwrap_or_default();
        for ask in asks {
            ask.cancel();
        }
        if let Ok(mut asks) = self.inner.asks.lock() {
            asks.clear();
        }
        if let Ok(mut sessions) = self.inner.session_asks.lock() {
            sessions.clear();
        }
        if let Ok(mut sessions) = self.inner.sessions.lock() {
            sessions.clear();
        }
        if let Ok(mut sessions) = self.inner.actor_sessions.lock() {
            sessions.clear();
        }
        // Do not delete the host session: they are durable actor transcriptions (resume is also used), just drop the reference.
    }
}

impl HostWorkflowDriver {
    /// Register the new session into two tables, and write the driver's private facts back to `dwf_actor` (read, modify, and retain what the engine writes
    /// name / persona).
    fn register_actor_session(
        &self,
        key: &str,
        actor: &ActorRef,
        persona: &PersonaSpec,
        child: Arc<Session>,
        journaled: &Option<ActorRecord>,
    ) {
        let name = persona.name.clone().filter(|name| !name.is_empty());
        let actor_session = Arc::new(ActorSession {
            child: Arc::clone(&child),
            actor: actor.clone(),
            name: name.clone(),
            persona_system: persona.system.clone(),
        });
        if let Ok(mut sessions) = self.inner.sessions.lock() {
            sessions.insert(child.id.clone(), actor_session);
        }
        if let Ok(mut sessions) = self.inner.actor_sessions.lock() {
            sessions.insert(key.to_string(), child.id.clone());
        }
        let mut record = journaled.clone().unwrap_or(ActorRecord {
            run_id: self.inner.run_id.clone(),
            site_id: actor.site_id.clone(),
            ordinal: actor.ordinal,
            name: None,
            persona: None,
            session_id: None,
            resolved_model: None,
        });
        if record.name.is_none() {
            record.name = name;
        }
        if record.persona.is_none() {
            record.persona = Some(persona.clone());
        }
        record.session_id = Some(child.id.clone());
        // Empty provider/model does not write fake pin: the host fails-closed when no provider is configured, and the pin should also be absent.
        let provider = child.provider();
        let model = child.model();
        if !provider.is_empty() && !model.is_empty() {
            record.resolved_model = Some(format!("{provider}/{model}"));
        }
        self.inner.journal.put_actor(record);
    }
}

/// The `provider/model` pin in the seed falls into the session meta (the writing and reading of `providerId/modelId` are reversed).
fn apply_model_pin(child: &Arc<Session>, pin: &str) {
    let (provider, model) = match pin.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (provider.to_string(), model.to_string())
        }
        // Pins without a provider can only come from old records: as the model id, the provider inherits the inherited value.
        _ => (child.provider(), pin.to_string()),
    };
    if model.is_empty() {
        return;
    }
    child.set_model_config(&provider, &model, &child.thought());
}

/// Create a turn dependency of ask: the context of the parent session + the registry after removing the interactive/recursive tools + two protocol tools
/// + Observation packaging (counting and import cache closing signals).
fn build_turn_deps(
    inner: &DriverInner,
    host: &Arc<HostServer>,
    actor_session: &ActorSession,
    ask: &Arc<AskState>,
) -> TurnDeps {
    let base = host.turn_deps.registry.without(ACTOR_DISALLOWED_TOOLS);
    let registry = wrap_actor_registry(base, ask);
    let mut context = host.turn_deps.context.clone();
    if let Some(system) = &actor_session.persona_system {
        if !system.trim().is_empty() {
            // Persona's system prompt goes through the host's system block channel (there is no second system prompt channel).
            context.system_blocks.push(system.clone());
        }
    }
    let deps = TurnDeps {
        registry: Arc::new(registry),
        context,
        full_access: true,
        provider_config: host.turn_deps.provider_config.clone(),
        host: OnceLock::new(),
        max_tool_rounds: None,
    };
    let _ = deps.host.set(Arc::downgrade(host));
    deps
}

/// Add an observation wrapper to each tool in the registry, and add two protocol tools.
///
/// Why include read-only tools: `AskStats.worldToolCalls` counts calls that have "seen or touched the external world",
/// And the host's tool execution path does not have a session event stream to subscribe to - the wrapper point is the only place where each call is seen.
fn wrap_actor_registry(base: ToolRegistry, ask: &Arc<AskState>) -> ToolRegistry {
    // Workflow actors get the real submit_result/escalate implementations bound to this ask.
    // Remove the unconfigured base stubs first so they cannot shadow the async workflow tools
    // (turn execution prefers a sync tool when both names are present).
    let mut registry = base.without(&["submit_result", "escalate"]);
    for name in registry.names() {
        if let Some(tool) = registry.get(&name) {
            registry = registry.with_tool(Arc::new(ObservedTool {
                inner: tool,
                ask: Arc::clone(ask),
            }));
        } else if let Some(tool) = registry.get_async(&name) {
            registry = registry.with_async_tool(Arc::new(ObservedAsyncTool {
                inner: tool,
                ask: Arc::clone(ask),
            }));
        }
    }
    registry = registry.with_async_tool(Arc::new(SubmitResultTool::new(Arc::clone(ask))));
    registry.with_async_tool(Arc::new(EscalateTool::new(Arc::clone(ask))))
}

/// The observation of a tool call on the contract (synonymous with the TS `ToolCallStarted` payload).
fn observe_tool_call(ask: &AskState, contract: &ToolContract, input: &Value, read_only: bool) {
    ask.note_tool_call(
        &contract.name,
        input,
        world_touching_scope(contract.side_effect_scope),
        mutating_scope(contract.side_effect_scope, read_only),
    );
}

/// "Looking at or touching the external world": Only the protocol files (session / userInteraction) are excluded, and the rest are counted as touches.
fn world_touching_scope(scope: &str) -> bool {
    !matches!(
        scope,
        crate::tools::SCOPE_SESSION | crate::tools::SCOPE_USER_INTERACTION
    )
}

/// TS `isWorkspaceMutatingToolCall`: non-read-only and scoped to workspace/git/system.
fn mutating_scope(scope: &str, read_only: bool) -> bool {
    !read_only
        && matches!(
            scope,
            crate::tools::SCOPE_WORKSPACE | crate::tools::SCOPE_SYSTEM | "git"
        )
}

struct ObservedTool {
    inner: Arc<dyn Tool>,
    ask: Arc<AskState>,
}

impl Tool for ObservedTool {
    fn contract(&self) -> ToolContract {
        self.inner.contract()
    }

    fn requires_user_interaction(&self) -> bool {
        self.inner.requires_user_interaction()
    }

    fn is_read_only_invocation(&self, input: &Value) -> bool {
        self.inner.is_read_only_invocation(input)
    }

    fn result_display(&self) -> Option<Value> {
        self.inner.result_display()
    }

    fn result_display_for(&self, input: &Value, output_text: &str) -> Option<Value> {
        self.inner.result_display_for(input, output_text)
    }

    fn always_allowed_by_name(&self) -> bool {
        self.inner.always_allowed_by_name()
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let contract = self.inner.contract();
        let read_only = contract.read_only || self.inner.is_read_only_invocation(input);
        observe_tool_call(&self.ask, &contract, input, read_only);
        self.inner.execute(input, ctx)
    }
}

struct ObservedAsyncTool {
    inner: Arc<dyn AsyncTool>,
    ask: Arc<AskState>,
}

#[async_trait]
impl AsyncTool for ObservedAsyncTool {
    fn contract(&self) -> ToolContract {
        self.inner.contract()
    }

    fn requires_user_interaction(&self) -> bool {
        self.inner.requires_user_interaction()
    }

    fn result_display(&self) -> Option<Value> {
        self.inner.result_display()
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let contract = self.inner.contract();
        let read_only = contract.read_only;
        observe_tool_call(&self.ask, &contract, input, read_only);
        self.inner.execute(input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        SCOPE_NETWORK, SCOPE_SESSION, SCOPE_SYSTEM, SCOPE_USER_INTERACTION, SCOPE_WORKSPACE,
    };

    #[test]
    fn observation_rules_mirror_the_contracts_helpers() {
        // Reading files (scope none, readOnly true): counts as "touching the world", not "writing".
        assert!(world_touching_scope("none"));
        assert!(!mutating_scope("none", true));

        //Write files: both touch the world and write.
        assert!(world_touching_scope(SCOPE_WORKSPACE));
        assert!(mutating_scope(SCOPE_WORKSPACE, false));
        assert!(mutating_scope(SCOPE_SYSTEM, false));
        assert!(mutating_scope("git", false));

        // Network: touch the world, do not write to the workspace.
        assert!(world_touching_scope(SCOPE_NETWORK));
        assert!(!mutating_scope(SCOPE_NETWORK, false));

        //Protocol file (session/userInteraction): neither touching the world nor writing.
        assert!(!world_touching_scope(SCOPE_SESSION));
        assert!(!world_touching_scope(SCOPE_USER_INTERACTION));
        assert!(!mutating_scope(SCOPE_SESSION, false));

        // Bash's read-only call (readOnly=true) does not trigger the door closing signal.
        assert!(!mutating_scope(SCOPE_SYSTEM, true));
    }
}
