//! Supervisor: the deterministic driver around the orchestrator.
//!
//! Responsibilities (spec: specs/nc-ag-orchestration.md section 3.2, 6.2-6.3):
//! - periodic scheduler tick (dependency-aware admission, concurrency, leases);
//! - health sweep: unresponsive and stalled agents -> recovery ladder;
//! - message dead-letter sweep;
//! - lease expiry sweep;
//! - startup recovery (orphaned work is requeued, worktrees preserved).
//!
//! The supervisor owns no business state: it only calls orchestrator/store commands.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::db::now_ms;
use crate::error::Result;
use crate::events::EventHub;
use crate::orchestrator::Orchestrator;
use crate::recovery::{self, ActivityWindow};
use crate::types::{AgentStatus, ProjectConfig, TaskStatus};
use crate::Store;

pub struct Supervisor {
    orchestrator: Arc<Orchestrator>,
    store: Arc<Store>,
    hub: Arc<EventHub>,
    running: AtomicBool,
}

#[derive(Debug, Clone, Default)]
pub struct SweepReport {
    pub dispatched: usize,
    pub unhealthy_agents: usize,
    pub recovered_actions: usize,
    pub expired_leases: usize,
    pub dead_letters: usize,
}

impl Supervisor {
    pub fn new(orchestrator: Arc<Orchestrator>, store: Arc<Store>, hub: Arc<EventHub>) -> Self {
        Self {
            orchestrator,
            store,
            hub,
            running: AtomicBool::new(false),
        }
    }

    pub fn orchestrator(&self) -> &Arc<Orchestrator> {
        &self.orchestrator
    }

    /// Startup: reconcile orphaned agents/tasks before any new dispatch.
    pub fn startup(&self) -> Result<serde_json::Value> {
        let report = recovery::recover_on_startup(&self.store, &self.hub)?;
        crate::events::emit(
            &self.store,
            &self.hub,
            None,
            None,
            None,
            "supervisor.startup",
            report.clone(),
        )?;
        Ok(report)
    }

    /// One pass of every maintenance loop.
    pub async fn sweep(&self) -> Result<SweepReport> {
        let mut report = SweepReport::default();
        let expired_leases = self.store.expire_leases()?;
        let projects = self.store.projects()?;
        for project in &projects {
            report.dead_letters += crate::messaging::sweep_dead_letters(
                &self.store,
                &project.id,
                (project.config.lease_ttl_secs as i64) * 1000 * 4,
            )?;
        }
        // Health sweep across live agents.
        for agent in self.store.all_agents()? {
            if agent.status.is_terminal() {
                continue;
            }
            let Some(task_id) = agent.task_id.clone() else {
                continue;
            };
            let Ok(task) = self.store.task(&task_id) else {
                continue;
            };
            if task.status.is_terminal() {
                continue;
            }
            let Ok(project) = self.store.project(&agent.project_id) else {
                continue;
            };
            let window = self.activity_window(&agent.id)?;
            let health = recovery::evaluate(&agent, &window, &project.config, now_ms());
            if health.issues.is_empty() {
                continue;
            }
            report.unhealthy_agents += 1;
            // A repeated-command/tool loop is evidence of an unproductive cycle even when
            // the agent is still emitting events; treat it as a stall.
            let prior = self.orchestrator.bump_recovery(&agent.id);
            let action = recovery::apply_recovery(
                &self.store,
                &self.hub,
                &agent,
                Some(&task),
                &health.issues,
                prior.saturating_sub(1),
            )?;
            report.recovered_actions += 1;
            crate::events::emit(
                &self.store,
                &self.hub,
                Some(&project.id),
                Some(&task.id),
                Some(&agent.id),
                "agent.health",
                json!({
                    "issues": health.issues.iter().map(|issue| format!("{issue:?}")).collect::<Vec<_>>(),
                    "action": format!("{action:?}"),
                }),
            )?;
        }
        // Scheduler tick last: recovered work becomes dispatchable in the same pass.
        report.dispatched = self.orchestrator.tick().await?;
        report.expired_leases = expired_leases;
        Ok(report)
    }

    /// Health window from real events (tool calls, failure signals) and the worktree.
    fn activity_window(&self, agent_id: &str) -> Result<ActivityWindow> {
        let events = self.store.events_after(0, 2000)?;
        let mut window = ActivityWindow::default();
        // Recent window only: the last 60 events for this agent, not its whole lifetime.
        let recent: Vec<&crate::types::Event> = events
            .iter()
            .filter(|event| event.agent_id.as_deref() == Some(agent_id))
            .rev()
            .take(60)
            .collect();
        for event in recent.into_iter() {
            match event.kind.as_str() {
                "agent.tool" => {
                    let tool = event
                        .payload
                        .get("tool")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    // Signature = tool + the exact input, so five different reads are not
                    // mistaken for five repetitions of one call.
                    let input = event
                        .payload
                        .get("input")
                        .map(|value| value.to_string())
                        .unwrap_or_default();
                    let signature = format!("{tool}|{input}");
                    if let Some(entry) = window
                        .tool_calls
                        .iter_mut()
                        .find(|(name, _)| *name == signature)
                    {
                        entry.1 += 1;
                    } else {
                        window.tool_calls.push((signature, 1));
                    }
                    if event.payload.get("ok").and_then(|value| value.as_bool()) == Some(false) {
                        let command = event
                            .payload
                            .get("input")
                            .and_then(|value| value.get("command"))
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string();
                        if !command.is_empty() {
                            window.failed_commands.push(command);
                        }
                    }
                }
                "agent.round" => window.rounds += 1,
                _ => {}
            }
        }
        if let Ok(agent) = self.store.agent(agent_id) {
            if let Some(worktree) = agent.worktree.as_ref() {
                window.new_files = std::fs::read_dir(worktree)
                    .map(|entries| entries.flatten().count())
                    .unwrap_or(0);
            }
        }
        Ok(window)
    }

    /// Mark a task as blocked when no agent can take it (used by the API on demand).
    pub fn block_task(&self, task_id: &str, reason: &str) -> Result<()> {
        let task = self.store.task(task_id)?;
        self.store.set_task_status(task_id, TaskStatus::Blocked)?;
        self.store.set_task_blocker(task_id, Some(reason))?;
        crate::events::emit(
            &self.store,
            &self.hub,
            Some(&task.project_id),
            Some(task_id),
            None,
            "task.blocked",
            json!({ "reason": crate::security::redact(reason) }),
        )?;
        Ok(())
    }

    /// Run the background loop until `shutdown` flips. Each pass is independent: a
    /// failure is logged and the loop continues (the orchestrator must survive).
    pub async fn run(self: Arc<Self>, interval_ms: u64, shutdown: Arc<AtomicBool>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut consecutive_errors = 0u32;
        while !shutdown.load(Ordering::SeqCst) {
            match self.sweep().await {
                Ok(report) => {
                    consecutive_errors = 0;
                    if report.dispatched > 0 || report.recovered_actions > 0 {
                        crate::events::emit(
                            &self.store,
                            &self.hub,
                            None,
                            None,
                            None,
                            "supervisor.sweep",
                            json!({
                                "dispatched": report.dispatched,
                                "unhealthyAgents": report.unhealthy_agents,
                                "recoveredActions": report.recovered_actions,
                                "expiredLeases": report.expired_leases,
                                "deadLetters": report.dead_letters,
                            }),
                        )
                        .ok();
                    }
                }
                Err(error) => {
                    consecutive_errors += 1;
                    crate::events::emit(
                        &self.store,
                        &self.hub,
                        None,
                        None,
                        None,
                        "supervisor.error",
                        json!({ "code": error.code, "message": error.message, "consecutive": consecutive_errors }),
                    )
                    .ok();
                }
            }
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
        self.running.store(false, Ordering::SeqCst);
    }
}

/// The default sweep cadence: fast enough to feel live, cheap enough to idle.
pub const SWEEP_INTERVAL_MS: u64 = 1_000;

/// Capabilities exposed to the dashboard so a user can see what work is possible.
pub fn capability_matrix(profiles: &[crate::types::AgentProfile]) -> serde_json::Value {
    let mut capabilities: std::collections::BTreeSet<String> = Default::default();
    for profile in profiles {
        for capability in &profile.capabilities {
            capabilities.insert(capability.clone());
        }
    }
    json!({
        "capabilities": capabilities.into_iter().collect::<Vec<_>>(),
        "profiles": profiles.len(),
    })
}

/// A project's effective config (for the UI and for tests).
pub fn effective_config(project: &crate::types::Project) -> &ProjectConfig {
    &project.config
}

/// Whether an agent status counts as "occupying a concurrency slot".
pub fn occupies_slot(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Working
            | AgentStatus::Assigned
            | AgentStatus::Starting
            | AgentStatus::Reviewing
            | AgentStatus::Recovering
    )
}
