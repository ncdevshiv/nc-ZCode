//! Engine facade (port of `engine.ts`): a synchronous state machine inside the lock + asynchronous dispatch outside it.
//!
//! The core ([`Core`]) stays zero I/O, no clock, no randomness; this layer provides:
//! - the constructor's createRun / resume (script hash validation, report counting and artifact state restoration);
//! - Boundary A's host API (the script side);
//! - Boundary B's report sink (the driver side);
//! - the run lifecycle (complete / fail / stop) and the resolving of `settled`;
//! - execution of dispatch intents: session creation (driver, async) → node-dispatched → startAsk.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde_json::Value;
use tokio::sync::oneshot;

use crate::engine_state::{Core, DispatchIntent};
use crate::imported_cache::ImportedWorldQueue;
use crate::types::{
    ActorId, AskMessage, AskProgress, AskSpec, AskStats, AskWaitInfo, Caps, ConcurrencyChange,
    ImportedRunCache, InstanceRef, NodeKind, NodeRecordStatus, RunEvent, RunSettlement,
    RunStallInfo, RunStatus, RunStopReason, SessionRef, SubmitVerdict, ValidateFn, WorkflowDriver,
    WorkflowError, WorkflowErrorCode, WorkflowReportSink,
};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Host metadata that rides along with `run-launched` in the round that launches the run (the engine never reads it, it only transcribes it once).
#[derive(Clone, Debug, Default)]
pub struct RunLaunchConfig {
    pub input_id: String,
    pub phase_names: Option<Vec<String>>,
    pub subagent_model: Option<String>,
    pub script_path: Option<String>,
    pub phase_alongside: Option<Vec<Vec<u32>>>,
}

/// Engine construction configuration.
pub struct EngineConfig {
    pub run_id: String,
    pub driver: Arc<dyn WorkflowDriver>,
    pub caps: Caps,
    /// Must cover every ask site in the script (an absence is a hard MissingAskSpec failure).
    pub ask_specs: HashMap<String, AskSpec>,
    pub validate: ValidateFn,
    pub script_text: Option<String>,
    pub name: Option<String>,
    pub script_hash: Option<String>,
    pub args: Option<Value>,
    pub parent_session_id: Option<String>,
    pub cwd: Option<String>,
    pub tool_call_id: Option<String>,
    pub resumed_from: Option<String>,
    pub launch: Option<RunLaunchConfig>,
    pub inherited_tokens: Option<u64>,
    pub imported_cache: Option<ImportedRunCache>,
}

/// The dynamic workflow engine (deterministic state machine + asynchronous dispatch).
pub struct WorkflowEngine {
    core: Mutex<Core>,
    rt: tokio::runtime::Handle,
    self_weak: OnceLock<Weak<WorkflowEngine>>,
    settled_rx: Mutex<Option<oneshot::Receiver<RunSettlement>>>,
}

impl WorkflowEngine {
    /// Construct the engine (createRun or resume). `ScriptHashMismatch` is returned synchronously when a resume is rejected.
    pub fn new(config: EngineConfig) -> Result<Arc<Self>, WorkflowError> {
        let rt = tokio::runtime::Handle::try_current().map_err(|_| {
            WorkflowError::new(
                WorkflowErrorCode::DriverError,
                "WorkflowEngine must be constructed inside a tokio runtime",
            )
        })?;
        let (settled_tx, settled_rx) = oneshot::channel();
        let journal = config.driver.journal();
        let existing = journal.get_run(&config.run_id);
        let created = existing.is_none();

        let spent_tokens: u64;
        let mut report_count = 0usize;
        let mut restored_artifacts: Vec<(Option<String>, Value)> = Vec::new();
        let mut live_ask_instances = Default::default();
        let mut import_closed = false;

        if created {
            // A revised run starts its accounting from the predecessor's accumulated value.
            spent_tokens = config.inherited_tokens.unwrap_or(0);
            journal.create_run(crate::types::RunRecord {
                run_id: config.run_id.clone(),
                parent_session_id: config.parent_session_id.clone(),
                cwd: config.cwd.clone(),
                name: config.name.clone(),
                tool_call_id: config.tool_call_id.clone(),
                script_text: config.script_text.clone(),
                script_hash: config.script_hash.clone(),
                args: config.args.clone(),
                resumed_from: config.resumed_from.clone(),
                caps: config.caps.clone(),
                spent_tokens,
                status: RunStatus::Running,
                stop_reason: None,
                superseded_by: None,
                failure: None,
                result: None,
            });
        } else {
            let existing = existing.expect("checked");
            // resume precondition: the script must be byte-identical (a mismatch rejects this resume, ahead of acceptance).
            if let (Some(recorded), Some(provided)) = (
                existing.script_hash.as_deref(),
                config.script_hash.as_deref(),
            ) {
                if recorded != provided {
                    return Err(WorkflowError::new(
                        WorkflowErrorCode::ScriptHashMismatch,
                        format!(
                            "Cannot resume run {}: its script changed (recorded hash {recorded}, got \
                             {provided}). Resume needs the byte-identical script; to run a revised \
                             script, start a new run with resume_from instead.",
                            config.run_id
                        ),
                    )
                    .with_mismatch(recorded, provided));
                }
            }
            let nodes = journal.list_nodes(&config.run_id);
            report_count = nodes.iter().filter(|n| n.kind == NodeKind::Report).count();
            for node in &nodes {
                if node.kind != NodeKind::Artifact || node.status != NodeRecordStatus::Completed {
                    continue;
                }
                if let Some(result) = node.result.clone() {
                    restored_artifacts.push((node.artifact_id.clone(), result));
                }
            }
            spent_tokens = existing.spent_tokens;
            if config.imported_cache.is_some() {
                let (live, closed) =
                    crate::engine_world::recover_import_closure(journal, &config.run_id);
                live_ask_instances = live;
                import_closed = closed;
            }
            journal.update_run_status(&config.run_id, RunStatus::Running, None);
        }

        let core = Core {
            run_id: config.run_id.clone(),
            driver: Arc::clone(&config.driver),
            caps: config.caps.clone(),
            ask_specs: config.ask_specs,
            validate: config.validate,
            ordinals: HashMap::new(),
            phase_ordinals: HashMap::new(),
            current_phase: None,
            instance_phases: HashMap::new(),
            actor_names: HashMap::new(),
            imported_cache: config.imported_cache.clone(),
            imported_world: ImportedWorldQueue::new(
                config
                    .imported_cache
                    .as_ref()
                    .map(|cache| cache.world.clone())
                    .unwrap_or_default(),
            ),
            import_closed,
            live_ask_instances,
            report_count,
            artifacts: HashMap::new(),
            spent_tokens,
            run_settled: false,
            run_failure: None,
            actors: HashMap::new(),
            actor_order: Vec::new(),
            live_nodes: HashMap::new(),
            active_asks: 0,
            intents: Vec::new(),
            settled_tx: Some(settled_tx),
        };
        let engine = Arc::new(Self {
            core: Mutex::new(core),
            rt,
            self_weak: OnceLock::new(),
            settled_rx: Mutex::new(Some(settled_rx)),
        });
        let _ = engine.self_weak.set(Arc::downgrade(&engine));

        {
            let mut core = engine.core.lock().expect("engine lock");
            // resume restores artifact state (a completed row claims its id).
            for (id, result) in restored_artifacts {
                core.remember_artifact_row(id.as_deref(), &result);
            }
            core.record(RunEvent::RunStarted {
                run_id: config.run_id.clone(),
                caps: config.caps.clone(),
            });
            // Only the run-creating generation records the anchor (it is not recorded again when resume hits existing rows).
            if created {
                if let Some(launch) = &config.launch {
                    core.record(RunEvent::RunLaunched {
                        input_id: launch.input_id.clone(),
                        tool_call_id: config.tool_call_id.clone(),
                        parent_session_id: config.parent_session_id.clone(),
                        phase_names: launch.phase_names.clone(),
                        subagent_model: launch.subagent_model.clone(),
                        script_path: launch.script_path.clone(),
                        phase_alongside: launch.phase_alongside.clone(),
                    });
                }
            }
            // Re-emit one event for inherited/restored usage (emit nothing for zero).
            if core.spent_tokens > 0 {
                let spent = core.spent_tokens;
                core.record(RunEvent::UsageUpdated {
                    spent_tokens: spent,
                });
            }
        }
        Ok(engine)
    }

    /// The run settlement promise: resolved on completion / failure / cancellation.
    pub async fn settled(&self) -> RunSettlement {
        let rx = self.settled_rx.lock().expect("engine lock").take();
        match rx {
            Some(rx) => rx.await.unwrap_or_else(|_| RunSettlement {
                status: RunStatus::Errored,
                artifact: None,
                reason: None,
                superseded_by: None,
                error: Some(WorkflowError::new(
                    WorkflowErrorCode::Interrupted,
                    "The engine settlement channel was dropped",
                )),
            }),
            None => RunSettlement {
                status: RunStatus::Errored,
                artifact: None,
                reason: None,
                superseded_by: None,
                error: Some(WorkflowError::new(
                    WorkflowErrorCode::DriverError,
                    "settled() was already awaited",
                )),
            },
        }
    }

    /// The current run status (no row created yet / unknown always yields running, same as TS).
    pub fn status(&self) -> RunStatus {
        let core = self.core.lock().expect("engine lock");
        core.journal()
            .get_run(&core.run_id)
            .map(|record| record.status)
            .unwrap_or(RunStatus::Running)
    }

    // ——————————————————————— Boundary A ———————————————————————

    pub fn create_actor(
        &self,
        site_id: &str,
        name: Option<String>,
        persona: Option<Value>,
    ) -> Result<ActorId, WorkflowError> {
        let mut core = self.core.lock().expect("engine lock");
        core.create_actor(site_id, name, persona)
    }

    /// One ask (the promise the script awaits).
    pub fn ask(
        &self,
        site_id: &str,
        actor_id: &str,
        instructions: &str,
    ) -> BoxFuture<Result<Value, WorkflowError>> {
        let mut core = self.core.lock().expect("engine lock");
        let Some(spec) = core.ask_specs.get(site_id).cloned() else {
            let err = WorkflowError::new(
                WorkflowErrorCode::MissingAskSpec,
                format!(
                    "Ask site {site_id} has no spec: askSpecs is incomplete (compile output mismatch)."
                ),
            );
            core.fail_run(err.clone());
            return Box::pin(async move { Err(err) });
        };
        let rx = core.admit_ask(site_id, actor_id, instructions, spec);
        let intents = core.take_intents();
        drop(core);
        self.spawn_dispatch(intents);
        Box::pin(async move {
            match rx.await {
                Ok(result) => result,
                Err(_) => Err(WorkflowError::new(
                    WorkflowErrorCode::Cancelled,
                    "Ask was cancelled before it settled",
                )),
            }
        })
    }

    pub fn world_read(
        &self,
        site_id: &str,
        op: &str,
        args: Vec<Value>,
    ) -> BoxFuture<Result<Value, WorkflowError>> {
        let admission = {
            let mut core = self.core.lock().expect("engine lock");
            core.world_read_admit(site_id, op, &args)
        };
        let driver = self.driver_ref();
        let engine = self.self_weak.get().and_then(Weak::upgrade);
        let op = op.to_string();
        Box::pin(async move {
            match admission {
                crate::engine_world::WorldAdmission::Immediate(result) => result,
                crate::engine_world::WorldAdmission::Live {
                    instance,
                    kind,
                    hash,
                    input,
                } => {
                    let outcome = driver.execute_world_read(&op, args).await;
                    let Some(engine) = engine else {
                        return Err(WorkflowError::new(
                            WorkflowErrorCode::DriverError,
                            "Engine dropped while a world read was in flight",
                        ));
                    };
                    let mut core = engine.core.lock().expect("engine lock");
                    core.world_read_settle(&instance, kind, &hash, &input, outcome)
                }
            }
        })
    }

    pub fn report(&self, site_id: &str, item: Value, artifact_id: Option<&str>) {
        let mut core = self.core.lock().expect("engine lock");
        core.publish_report(site_id, item, artifact_id);
    }

    pub fn enter_phase(&self, name: &str) {
        let mut core = self.core.lock().expect("engine lock");
        if core.run_settled {
            return;
        }
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        let ordinal = {
            let next = core.phase_ordinals.get(trimmed).copied().unwrap_or(0) + 1;
            core.phase_ordinals.insert(trimmed.to_string(), next);
            next
        };
        core.current_phase = Some(trimmed.to_string());
        core.record(RunEvent::PhaseEntered {
            name: trimmed.to_string(),
            ordinal,
        });
    }

    pub fn publish_artifact(
        &self,
        site_id: &str,
        op: crate::types::ArtifactOp,
        args: Vec<Value>,
    ) -> BoxFuture<Result<crate::types::ArtifactRef, WorkflowError>> {
        let admission = {
            let mut core = self.core.lock().expect("engine lock");
            core.publish_artifact_admit(site_id, op, &args)
        };
        let driver = self.driver_ref();
        let engine = self.self_weak.get().and_then(Weak::upgrade);
        Box::pin(async move {
            match admission {
                crate::engine_artifacts::ArtifactAdmission::Immediate(result) => result,
                crate::engine_artifacts::ArtifactAdmission::Live {
                    instance,
                    id,
                    op,
                    version,
                    primary,
                    hash,
                    request,
                } => {
                    let record = driver.execute_artifact_publish(request).await;
                    let Some(engine) = engine else {
                        return Err(WorkflowError::new(
                            WorkflowErrorCode::DriverError,
                            "Engine dropped while an artifact publish was in flight",
                        ));
                    };
                    let mut core = engine.core.lock().expect("engine lock");
                    core.publish_artifact_settle(
                        crate::engine_artifacts::ArtifactLiveParts {
                            instance,
                            id,
                            op,
                            version,
                            primary,
                            hash,
                        },
                        record,
                    )
                }
            }
        })
    }

    pub fn declare_artifact(&self, site_id: &str, op: crate::types::ArtifactOp, args: Vec<Value>) {
        let mut core = self.core.lock().expect("engine lock");
        core.declare_preset_artifact(site_id, op, &args);
    }

    pub fn log(&self, message: &str) {
        let mut core = self.core.lock().expect("engine lock");
        if core.run_settled {
            return;
        }
        core.record(RunEvent::Log {
            message: message.to_string(),
        });
    }

    // ——————————————————————— run lifecycle ———————————————————————

    pub fn complete(&self, artifact: Value) {
        let mut core = self.core.lock().expect("engine lock");
        crate::engine_settlement::settle_completed(&mut core, artifact);
    }

    pub fn fail(&self, error: WorkflowError) {
        let mut core = self.core.lock().expect("engine lock");
        core.fail_run(error);
    }

    pub fn stop(
        &self,
        reason: RunStopReason,
        error: Option<WorkflowError>,
        superseded_by: Option<String>,
    ) {
        let mut core = self.core.lock().expect("engine lock");
        crate::engine_settlement::settle_stopped(&mut core, reason, error, superseded_by);
    }

    // ——————————————————————— dispatch ———————————————————————

    fn driver_ref(&self) -> Arc<dyn WorkflowDriver> {
        let core = self.core.lock().expect("engine lock");
        Arc::clone(&core.driver)
    }

    fn spawn_dispatch(&self, intents: Vec<DispatchIntent>) {
        if intents.is_empty() {
            return;
        }
        let Some(engine) = self.self_weak.get().and_then(Weak::upgrade) else {
            return;
        };
        for intent in intents {
            let engine = Arc::clone(&engine);
            self.rt.spawn(async move {
                engine.run_dispatch(intent).await;
            });
        }
    }

    async fn run_dispatch(self: Arc<Self>, intent: DispatchIntent) {
        let (session, actor_ref, persona, message, instance, seed) = {
            let mut core = self.core.lock().expect("engine lock");
            let Some(data) = core.take_dispatch(&intent) else {
                return;
            };
            let seed = core.actor_seed(&intent.actor_id);
            (
                data.session,
                data.actor_ref,
                data.persona,
                data.message,
                data.instance,
                seed,
            )
        };
        let session = match session {
            Some(session) => session,
            None => {
                match self
                    .driver_ref()
                    .create_actor_session(&actor_ref, &persona, seed)
                    .await
                {
                    Ok(session) => {
                        let mut core = self.core.lock().expect("engine lock");
                        core.mark_dispatched(&intent, &session);
                        let intents = core.take_intents();
                        drop(core);
                        self.spawn_dispatch(intents);
                        session
                    }
                    Err(error) => {
                        let mut core = self.core.lock().expect("engine lock");
                        core.dispatch_session_failed(&intent, error.message.clone());
                        let intents = core.take_intents();
                        drop(core);
                        self.spawn_dispatch(intents);
                        return;
                    }
                }
            }
        };
        let live = {
            let core = self.core.lock().expect("engine lock");
            core.node_live(&intent.instance_key) && !core.run_settled
        };
        if !live {
            return;
        }
        self.driver_ref().start_ask(&session, &instance, message);
    }
}

/// Provided so module tests can construct a [`Core`] directly (without going through the tokio runtime).
#[cfg(test)]
pub(crate) fn test_core(
    run_id: &str,
    driver: Arc<dyn WorkflowDriver>,
    caps: Caps,
    validate: ValidateFn,
) -> Core {
    let (settled_tx, _rx) = oneshot::channel();
    Core {
        run_id: run_id.to_string(),
        driver,
        caps,
        ask_specs: HashMap::new(),
        validate,
        ordinals: HashMap::new(),
        phase_ordinals: HashMap::new(),
        current_phase: None,
        instance_phases: HashMap::new(),
        actor_names: HashMap::new(),
        imported_cache: None,
        imported_world: ImportedWorldQueue::new(Default::default()),
        import_closed: false,
        live_ask_instances: Default::default(),
        report_count: 0,
        artifacts: HashMap::new(),
        spent_tokens: 0,
        run_settled: false,
        run_failure: None,
        actors: HashMap::new(),
        actor_order: Vec::new(),
        live_nodes: HashMap::new(),
        active_asks: 0,
        intents: Vec::new(),
        settled_tx: Some(settled_tx),
    }
}

// ——————————————————————— Boundary B：sink ———————————————————————

impl WorkflowReportSink for WorkflowEngine {
    fn ask_submit_attempted(&self, instance: &InstanceRef, payload: Value) {
        let mut core = self.core.lock().expect("engine lock");
        core.submit_attempted(instance, payload);
        let intents = core.take_intents();
        drop(core);
        self.spawn_dispatch(intents);
    }

    fn ask_turn_ended(&self, instance: &InstanceRef, final_text: &str) {
        let mut core = self.core.lock().expect("engine lock");
        core.turn_ended(instance, final_text);
        let intents = core.take_intents();
        drop(core);
        self.spawn_dispatch(intents);
    }

    fn ask_progress(&self, instance: &InstanceRef, progress: AskProgress) {
        let mut core = self.core.lock().expect("engine lock");
        if core.run_settled {
            return;
        }
        core.record(RunEvent::NodeProgress {
            instance: instance.clone(),
            turn: progress.turn,
            tool_calls: progress.tool_calls,
            last_tool: progress.last_tool,
        });
    }

    fn ask_stats(&self, instance: &InstanceRef, stats: AskStats) {
        let mut core = self.core.lock().expect("engine lock");
        core.note_stats(instance, stats.clone());
        core.spent_tokens += stats.tokens;
        let spent = core.spent_tokens;
        core.journal().update_run_usage(&core.run_id, spent);
        // A straggler that arrives after settlement only updates the books (the journal row); it emits no more events.
        if core.run_settled {
            return;
        }
        core.record(RunEvent::UsageUpdated {
            spent_tokens: spent,
        });
    }

    fn ask_failed(&self, instance: &InstanceRef, error: WorkflowError) {
        let mut core = self.core.lock().expect("engine lock");
        core.failed_ask(instance, error);
        let intents = core.take_intents();
        drop(core);
        self.spawn_dispatch(intents);
    }

    fn stop_run(&self, error: WorkflowError) {
        let error = self.enrich_provider_stop_phase(error);
        let mut core = self.core.lock().expect("engine lock");
        crate::engine_settlement::settle_stopped(
            &mut core,
            RunStopReason::Provider,
            Some(error),
            None,
        );
    }

    fn run_stalled(&self, info: RunStallInfo) {
        let mut core = self.core.lock().expect("engine lock");
        if core.run_settled {
            return;
        }
        core.record(RunEvent::RunStalled {
            since_ms: info.since_ms,
            reason: info.reason,
            cap: info.cap,
        });
    }

    fn ask_waiting(&self, instance: &InstanceRef, info: AskWaitInfo) {
        let mut core = self.core.lock().expect("engine lock");
        if core.run_settled || !core.is_live(instance) {
            return;
        }
        core.record(RunEvent::NodeWaiting {
            instance: instance.clone(),
            cause: info.cause,
            reason: info.reason,
            attempt: info.attempt,
            delay_ms: info.delay_ms,
            retry_after_ms: info.retry_after_ms,
        });
    }

    fn ask_executing(&self, instance: &InstanceRef) {
        let mut core = self.core.lock().expect("engine lock");
        if core.run_settled || !core.is_live(instance) {
            return;
        }
        core.record(RunEvent::NodeExecuting {
            instance: instance.clone(),
        });
    }

    fn ask_mutating(&self, instance: &InstanceRef) {
        let mut core = self.core.lock().expect("engine lock");
        if core.run_settled || !core.is_live(instance) {
            return;
        }
        let name = core.live_actor_name(instance);
        core.close_import_cache(instance, crate::types::ImportCloseCause::MutatingTool, name);
    }

    fn concurrency_changed(&self, change: ConcurrencyChange) {
        let mut core = self.core.lock().expect("engine lock");
        if core.run_settled {
            return;
        }
        core.record(RunEvent::ConcurrencyChanged {
            key: change.key,
            previous: change.previous,
            next: change.next,
            reason: change.reason,
            last_good: change.last_good,
            last_bad: change.last_bad,
            cooldown_ms: change.cooldown_ms,
        });
    }
}

impl WorkflowEngine {
    /// ProviderStop fills in the birth phase of the subagent that triggered the stop.
    fn enrich_provider_stop_phase(&self, error: WorkflowError) -> WorkflowError {
        let phase = {
            let core = self.core.lock().expect("engine lock");
            let Some(details) = error.provider_stop.as_ref() else {
                return error;
            };
            if details.phase.is_some() {
                return error;
            }
            let Some(subagent) = details.subagent.as_deref() else {
                return error;
            };
            core.instance_phases.get(subagent).cloned()
        };
        let Some(phase) = phase else { return error };
        let mut enriched = error;
        if let Some(details) = enriched.provider_stop.take() {
            enriched.provider_stop = Some(crate::types::ProviderStopDetails {
                phase: Some(phase),
                ..details
            });
        }
        enriched
    }
}

#[async_trait::async_trait]
impl crate::types::WorkflowHostApi for WorkflowEngine {
    fn create_actor(
        &self,
        site_id: &str,
        name: Option<String>,
        persona: Option<Value>,
    ) -> Result<ActorId, WorkflowError> {
        WorkflowEngine::create_actor(self, site_id, name, persona)
    }

    async fn ask(
        &self,
        site_id: &str,
        actor: &ActorId,
        instructions: &str,
    ) -> Result<Value, WorkflowError> {
        WorkflowEngine::ask(self, site_id, actor, instructions).await
    }

    async fn world_read(
        &self,
        site_id: &str,
        op: &str,
        args: Vec<Value>,
    ) -> Result<Value, WorkflowError> {
        WorkflowEngine::world_read(self, site_id, op, args).await
    }

    fn report(&self, site_id: &str, item: Value, artifact_id: Option<&str>) {
        WorkflowEngine::report(self, site_id, item, artifact_id);
    }

    fn enter_phase(&self, name: &str) {
        WorkflowEngine::enter_phase(self, name);
    }

    async fn publish_artifact(
        &self,
        site_id: &str,
        op: &str,
        args: Vec<Value>,
    ) -> Result<crate::types::ArtifactRef, WorkflowError> {
        let op = crate::types::ArtifactOp::parse(op).ok_or_else(|| {
            WorkflowError::new(
                WorkflowErrorCode::DriverError,
                format!("Unknown artifact op: {op}"),
            )
        })?;
        WorkflowEngine::publish_artifact(self, site_id, op, args).await
    }

    fn declare_artifact(&self, site_id: &str, op: &str, args: Vec<Value>) {
        if let Some(op) = crate::types::ArtifactOp::parse(op) {
            WorkflowEngine::declare_artifact(self, site_id, op, args);
        }
    }

    fn log(&self, message: &str) {
        WorkflowEngine::log(self, message);
    }
}

// Aliases for the unused-import check (keeps readability aligned with the TS vocabulary).
#[allow(dead_code)]
fn _type_anchors(_: Option<SessionRef>, _: Option<SubmitVerdict>, _: Option<AskMessage>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal_memory::InMemoryJournalStore;
    use crate::types::{AskProgress, AskStats, JournalStorePort, SubmitVerdict};
    use serde_json::json;
    use std::sync::Arc;

    struct ChannelDriver {
        journal: Arc<InMemoryJournalStore>,
        started: tokio::sync::mpsc::UnboundedSender<(InstanceRef, AskMessage)>,
    }

    #[async_trait::async_trait]
    impl WorkflowDriver for ChannelDriver {
        fn journal(&self) -> &dyn JournalStorePort {
            &*self.journal
        }
        fn emit(&self, _event: &RunEvent) {}
        async fn create_actor_session(
            &self,
            actor: &crate::types::ActorRef,
            _persona: &crate::types::PersonaSpec,
            _seed: Option<crate::types::ActorSessionSeed>,
        ) -> Result<SessionRef, WorkflowError> {
            Ok(SessionRef {
                id: format!("session-{}", actor.key()),
            })
        }
        fn start_ask(&self, _session: &SessionRef, instance: &InstanceRef, message: AskMessage) {
            let _ = self.started.send((instance.clone(), message));
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

    fn config(
        run_id: &str,
        driver: Arc<dyn WorkflowDriver>,
        ask_specs: HashMap<String, AskSpec>,
    ) -> EngineConfig {
        EngineConfig {
            run_id: run_id.to_string(),
            driver,
            caps: Caps { max_concurrency: 2 },
            ask_specs,
            validate: Arc::new(|_schema: Option<&Value>, _value: &Value| Vec::new()),
            script_text: None,
            name: None,
            script_hash: Some("hash".into()),
            args: None,
            parent_session_id: None,
            cwd: None,
            tool_call_id: None,
            resumed_from: None,
            launch: None,
            inherited_tokens: None,
            imported_cache: None,
        }
    }

    #[tokio::test]
    async fn end_to_end_typed_ask_completes_the_run() {
        let journal = Arc::new(InMemoryJournalStore::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = Arc::new(ChannelDriver {
            journal: journal.clone(),
            started: tx,
        });
        let mut specs = HashMap::new();
        specs.insert(
            "ask#1".to_string(),
            AskSpec {
                typed: true,
                schema: Some(json!({"type": "object"})),
            },
        );
        let engine = WorkflowEngine::new(config("run-e2e", driver, specs)).expect("engine");
        let actor = engine
            .create_actor("actor#1", Some("planner".into()), None)
            .unwrap();
        let ask = engine.ask("ask#1", &actor, "do the thing");
        let (instance, message) = rx.recv().await.expect("start_ask arrives");
        assert!(message.typed);
        assert_eq!(instance.site_id, "ask#1");
        engine.ask_submit_attempted(&instance, json!({"ok": true}));
        let value = ask.await.expect("ask settles with submitted value");
        assert_eq!(value, json!({"ok": true}));
        engine.complete(json!({"done": true}));
        let settlement = engine.settled().await;
        assert_eq!(settlement.status, RunStatus::Completed);
        assert_eq!(settlement.artifact, Some(json!({"done": true})));
        let row = journal.get_run("run-e2e").unwrap();
        assert_eq!(row.status, RunStatus::Completed);
        assert_eq!(row.result, Some(json!({"done": true})));

        let types: Vec<&str> = journal
            .list_events("run-e2e", crate::types::ListEventsOptions::default())
            .iter()
            .map(|e| e.event.type_name())
            .collect();
        let expected = [
            "run-started",
            "actor-created",
            "node-queued",
            "node-dispatched",
            "node-settled",
            "run-settled",
        ];
        assert_eq!(types, expected, "journal event order is the contract");

        let node = journal.get_node("run-e2e", "ask#1", 1).unwrap();
        assert_eq!(node.status, NodeRecordStatus::Completed);
        assert_eq!(node.result, Some(json!({"ok": true})));
        let actor_row = journal.get_actor("run-e2e", "actor#1", 1).unwrap();
        assert_eq!(actor_row.session_id.as_deref(), Some("session-actor#1@1"));
    }

    #[tokio::test]
    async fn usage_and_progress_are_recorded_in_order() {
        let journal = Arc::new(InMemoryJournalStore::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = Arc::new(ChannelDriver {
            journal: journal.clone(),
            started: tx,
        });
        let engine =
            WorkflowEngine::new(config("run-usage", driver, HashMap::new())).expect("engine");
        // One direct report besides world, then simulated stats.
        engine.report("report#1", json!({"p": 1}), None);
        engine.ask_progress(
            &InstanceRef {
                site_id: "ask#9".into(),
                ordinal: 1,
            },
            AskProgress {
                turn: 1,
                tool_calls: 2,
                last_tool: None,
            },
        );
        engine.ask_stats(
            &InstanceRef {
                site_id: "ask#9".into(),
                ordinal: 1,
            },
            AskStats {
                tokens: 17,
                tool_calls: 2,
                turns: 1,
                world_tool_calls: Some(1),
            },
        );
        let types: Vec<&str> = journal
            .list_events("run-usage", crate::types::ListEventsOptions::default())
            .iter()
            .map(|e| e.event.type_name())
            .collect();
        assert_eq!(
            types,
            ["run-started", "report", "node-progress", "usage-updated"]
        );
        assert_eq!(journal.get_run("run-usage").unwrap().spent_tokens, 17);
        let _ = &mut rx;
    }

    #[tokio::test]
    async fn script_hash_mismatch_rejects_resume() {
        let journal = Arc::new(InMemoryJournalStore::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let driver: Arc<dyn WorkflowDriver> = Arc::new(ChannelDriver {
            journal: journal.clone(),
            started: tx,
        });
        let _first =
            WorkflowEngine::new(config("run-resume", driver.clone(), HashMap::new())).unwrap();
        // Same runId, different scriptHash: resume must reject synchronously and must not rewrite existing rows.
        let mut second_config = config("run-resume", driver, HashMap::new());
        second_config.script_hash = Some("different".into());
        let err = match WorkflowEngine::new(second_config) {
            Ok(_) => panic!("must reject"),
            Err(err) => err,
        };
        assert_eq!(err.code, WorkflowErrorCode::ScriptHashMismatch);
    }

    #[tokio::test]
    async fn stop_and_fail_settle_with_reasons() {
        let journal = Arc::new(InMemoryJournalStore::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let driver: Arc<dyn WorkflowDriver> = Arc::new(ChannelDriver {
            journal: journal.clone(),
            started: tx,
        });
        let engine = WorkflowEngine::new(config("run-stop", driver, HashMap::new())).unwrap();
        engine.stop(RunStopReason::User, None, None);
        let settlement = engine.settled().await;
        assert_eq!(settlement.status, RunStatus::Stopped);
        assert_eq!(settlement.reason, Some(RunStopReason::User));
        let row = journal.get_run("run-stop").unwrap();
        assert_eq!(row.status, RunStatus::Stopped);
        assert_eq!(row.stop_reason, Some(RunStopReason::User));
    }
}
