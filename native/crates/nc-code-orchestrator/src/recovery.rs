//! Health monitoring, stuck detection and recovery (spec section 6.2-6.3).
//!
//! A heartbeat is liveness, never progress. Progress is judged from artifacts: new
//! events, new tool calls, changed files, increasing usage.

use serde_json::json;

use crate::db::now_ms;
use crate::error::Result;
use crate::events::EventHub;
use crate::types::{Agent, AgentStatus, MessageKind, ProjectConfig, Task, TaskStatus};
use crate::Store;

#[derive(Debug, Clone, PartialEq)]
pub enum HealthIssue {
    /// No heartbeat / events within the timeout: the agent may be dead.
    Unresponsive { silent_ms: i64 },
    /// Working but no meaningful output for `stall_after`: probably looping.
    Stalled { silent_ms: i64, rounds: usize },
    /// The same tool with the *same input* repeated many times (a real loop, unlike
    /// several different reads of a large repository).
    RepeatedToolCall {
        tool: String,
        input: String,
        count: usize,
    },
    /// Repeated failures of the same command.
    RepeatedFailure { command: String, count: usize },
}

#[derive(Debug, Clone)]
pub struct AgentHealth {
    pub agent_id: String,
    pub task_id: Option<String>,
    pub status: AgentStatus,
    pub last_activity: Option<i64>,
    pub issues: Vec<HealthIssue>,
    pub needs_recovery: bool,
}

/// Snapshot of one agent's observable behavior (defined in `agent`, re-exported here).
pub use crate::agent::ActivityWindow;

pub fn evaluate(
    agent: &Agent,
    window: &ActivityWindow,
    config: &ProjectConfig,
    now: i64,
) -> AgentHealth {
    let mut issues = Vec::new();
    // Liveness and progress are two different facts: the heartbeat proves the process is
    // alive, the last *activity* (a real event) proves it is doing something.
    let heartbeat_at = agent.last_heartbeat.unwrap_or(agent.created_at);
    let activity_at = agent.last_activity.unwrap_or(heartbeat_at);
    let heartbeat_silent = (now - heartbeat_at).max(0);
    let activity_silent = (now - activity_at).max(0);
    let heartbeat_timeout = (config.heartbeat_timeout_secs as i64) * 1000;
    let stall_after = (config.stall_after_secs as i64) * 1000;
    if !agent.status.is_terminal() {
        if heartbeat_silent > heartbeat_timeout {
            issues.push(HealthIssue::Unresponsive {
                silent_ms: heartbeat_silent,
            });
        } else if matches!(agent.status, AgentStatus::Working | AgentStatus::Reviewing)
            && activity_silent > stall_after
            && window.new_files == 0
        {
            issues.push(HealthIssue::Stalled {
                silent_ms: activity_silent,
                rounds: window.rounds,
            });
        }
    }
    // A loop is the same call with the same arguments; different paths are progress.
    for (signature, count) in &window.tool_calls {
        if *count >= 3 {
            let (tool, input) = signature
                .split_once('|')
                .map(|(tool, input)| (tool.to_string(), input.to_string()))
                .unwrap_or_else(|| (signature.clone(), String::new()));
            issues.push(HealthIssue::RepeatedToolCall {
                tool,
                input,
                count: *count,
            });
        }
    }
    for command in &window.failed_commands {
        let count = window
            .failed_commands
            .iter()
            .filter(|other| *other == command)
            .count();
        if count >= 3 {
            issues.push(HealthIssue::RepeatedFailure {
                command: command.clone(),
                count,
            });
        }
    }
    let needs_recovery = !issues.is_empty();
    AgentHealth {
        agent_id: agent.id.clone(),
        task_id: agent.task_id.clone(),
        status: agent.status,
        last_activity: agent.last_activity,
        issues,
        needs_recovery,
    }
}

/// The recovery ladder: nudge -> help -> reassign -> fail. Each step is observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    Nudge,
    RequestHelp,
    Reassign,
    Fail,
}

pub fn recovery_plan(prior_recoveries: u32, task: &Task) -> RecoveryAction {
    match prior_recoveries {
        0 => RecoveryAction::Nudge,
        1 => RecoveryAction::RequestHelp,
        2 if task.attempts <= task.max_retries => RecoveryAction::Reassign,
        _ => RecoveryAction::Fail,
    }
}

/// Reconcile state at startup: running/assigned work whose process is gone must be
/// recovered, not silently trusted (section 3.2 and Scenario K/M).
pub fn recover_on_startup(store: &Store, hub: &EventHub) -> Result<serde_json::Value> {
    let now = now_ms();
    let mut recovered_tasks = Vec::new();
    let mut failed_agents = Vec::new();
    let mut requeued_agents = Vec::new();
    for agent in store.all_agents()? {
        if agent.status.is_terminal() {
            continue;
        }
        let silent = now - agent.last_heartbeat.unwrap_or(agent.created_at);
        let threshold = 10_000; // process restarted: any non-terminal agent is suspect
        if silent > threshold {
            store.set_agent_error(
                &agent.id,
                "agent heartbeat was lost (orchestrator restart)",
                AgentStatus::Recovering,
            )?;
            failed_agents.push(agent.id.clone());
        } else {
            requeued_agents.push(agent.id.clone());
            continue;
        }
        let Some(task_id) = agent.task_id.clone() else {
            continue;
        };
        let task = store.task(&task_id)?;
        if task.status.is_terminal() {
            continue;
        }
        // Task goes back to ready so the scheduler re-dispatches it, preserving the worktree.
        store.set_task_status(&task_id, TaskStatus::Ready)?;
        store.set_task_blocker(&task_id, Some("recovered after orchestrator restart"))?;
        recovered_tasks
            .push(json!({ "taskId": task_id, "agentId": agent.id, "worktree": task.worktree }));
        crate::events::emit(
            store,
            hub,
            Some(&task.project_id),
            Some(&task_id),
            Some(&agent.id),
            "recovery.startup",
            json!({ "worktreePreserved": task.worktree.is_some() }),
        )?;
    }
    // Integer generation leases survive; expired ones are swept.
    let expired = store.expire_leases()?;
    // A pending approval does not block restart recovery.
    let report = json!({
        "recoveredTasks": recovered_tasks,
        "failedAgents": failed_agents,
        "requeuedAgents": requeued_agents,
        "expiredLeases": expired,
    });
    if !recovered_tasks.is_empty() || !failed_agents.is_empty() {
        store.append_audit(None, "orchestrator", "recovery.startup", &report, now)?;
    }
    Ok(report)
}

/// Apply the recovery action for a detected issue.
pub fn apply_recovery(
    store: &Store,
    hub: &EventHub,
    agent: &Agent,
    task: Option<&Task>,
    issues: &[HealthIssue],
    prior_recoveries: u32,
) -> Result<RecoveryAction> {
    let Some(task) = task else {
        store.set_agent_error(&agent.id, "stuck without a task", AgentStatus::Failed)?;
        return Ok(RecoveryAction::Fail);
    };
    let action = recovery_plan(prior_recoveries, task);
    let evidence = json!({
        "issues": issues.iter().map(|issue| format!("{issue:?}")).collect::<Vec<_>>(),
        "agentId": agent.id,
        "taskId": task.id,
        "priorRecoveries": prior_recoveries,
    });
    match action {
        RecoveryAction::Nudge => {
            let message = crate::messaging::NewMessage::to_agent(
                &task.project_id,
                Some(&task.id),
                &agent.id,
                MessageKind::TaskUpdate,
                "Progress check",
                "Your last activity produced no artifact. Report the current blocker or finish the task with submit_result.",
            )
            .with_priority(3);
            crate::messaging::send(store, message)?;
            store.heartbeat(&agent.id, Some("nudged"))?;
        }
        RecoveryAction::RequestHelp => {
            store.set_agent_status(&agent.id, AgentStatus::HelpRequested)?;
            crate::messaging::send(
                store,
                crate::messaging::NewMessage::to_project(
                    &task.project_id,
                    Some(&task.id),
                    MessageKind::HelpRequest,
                    "Agent needs help",
                    evidence.to_string(),
                )
                .with_priority(4),
            )?;
        }
        RecoveryAction::Reassign => {
            store.set_agent_status(&agent.id, AgentStatus::Failed)?;
            store.set_task_status(&task.id, TaskStatus::Ready)?;
            store.set_task_blocker(&task.id, Some("reassigned after stall"))?;
        }
        RecoveryAction::Fail => {
            store.set_agent_error(&agent.id, "recovery exhausted", AgentStatus::Failed)?;
            store.set_task_status(&task.id, TaskStatus::Failed)?;
            store.set_task_blocker(&task.id, Some("recovery exhausted"))?;
        }
    }
    crate::events::emit(
        store,
        hub,
        Some(&task.project_id),
        Some(&task.id),
        Some(&agent.id),
        "agent.recovery",
        json!({ "action": format!("{action:?}"), "issues": evidence["issues"] }),
    )?;
    store.append_audit(
        Some(&task.project_id),
        "orchestrator",
        "agent.recovery",
        &json!({ "action": format!("{action:?}"), "agentId": agent.id, "taskId": task.id }),
        now_ms(),
    )?;
    Ok(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(status: AgentStatus, last_activity: i64) -> Agent {
        Agent {
            id: "a1".into(),
            project_id: "p".into(),
            task_id: Some("t1".into()),
            profile_id: "backend".into(),
            role: "backend".into(),
            status,
            persona: String::new(),
            capabilities: vec![],
            permissions: vec![],
            model: None,
            provider: None,
            workspace: None,
            worktree: None,
            last_heartbeat: Some(last_activity),
            last_activity: Some(last_activity),
            progress_marker: None,
            usage_input: 0,
            usage_output: 0,
            cost_usd: 0.0,
            error: None,
            created_at: last_activity,
            updated_at: last_activity,
            ended_at: None,
        }
    }

    #[test]
    fn silent_agents_are_unresponsive_and_working_agents_stall() {
        let config = ProjectConfig {
            heartbeat_timeout_secs: 90,
            stall_after_secs: 300,
            ..Default::default()
        };
        let now = 1_000_000;
        // Both clocks are stale: the agent is unresponsive.
        let health = evaluate(
            &agent(AgentStatus::Working, now - 100_000),
            &ActivityWindow::default(),
            &config,
            now,
        );
        assert!(matches!(health.issues[0], HealthIssue::Unresponsive { .. }));
        // Heartbeat is fresh (last_heartbeat = now) but activity is stale: a stall.
        let mut stalled = agent(AgentStatus::Working, now);
        stalled.last_activity = Some(now - 301_000);
        let health = evaluate(&stalled, &ActivityWindow::default(), &config, now);
        assert!(matches!(health.issues[0], HealthIssue::Stalled { .. }));
        // A heartbeat alone is not progress, but new artifacts suppress the stall finding.
        let window = ActivityWindow {
            new_files: 2,
            ..Default::default()
        };
        stalled.last_activity = Some(now - 400_000);
        let health = evaluate(&stalled, &window, &config, now);
        assert!(health.issues.is_empty(), "{:?}", health.issues);
    }

    #[test]
    fn repeated_tools_and_failures_are_detected() {
        let config = ProjectConfig::default();
        let now = 10_000_000;
        let window = ActivityWindow {
            tool_calls: vec![("read_file|{\"path\": \"src/lib.rs\"}".into(), 6)],
            failed_commands: vec![
                "cargo test".into(),
                "cargo test".into(),
                "cargo test".into(),
            ],
            ..Default::default()
        };
        let health = evaluate(&agent(AgentStatus::Working, now), &window, &config, now);
        assert!(health
            .issues
            .iter()
            .any(|issue| matches!(issue, HealthIssue::RepeatedToolCall { .. })));
        assert!(health
            .issues
            .iter()
            .any(|issue| matches!(issue, HealthIssue::RepeatedFailure { .. })));
    }

    #[test]
    fn recovery_ladder_is_bounded() {
        let task = Task {
            id: "t1".into(),
            project_id: "p".into(),
            request_id: "r".into(),
            requirement_id: None,
            title: String::new(),
            description: String::new(),
            capability: "backend".into(),
            role: "backend".into(),
            agent_id: None,
            status: TaskStatus::Running,
            priority: 1,
            depends_on: vec![],
            inputs: vec![],
            outputs: vec![],
            acceptance: vec![],
            resources: vec![],
            max_retries: 2,
            attempts: 0,
            timeout_ms: 1000,
            deadline_at: None,
            worktree: None,
            branch: None,
            result: None,
            blocker: None,
            created_at: 0,
            updated_at: 0,
            started_at: None,
            ended_at: None,
        };
        assert_eq!(recovery_plan(0, &task), RecoveryAction::Nudge);
        assert_eq!(recovery_plan(1, &task), RecoveryAction::RequestHelp);
        assert_eq!(recovery_plan(2, &task), RecoveryAction::Reassign);
        assert_eq!(recovery_plan(3, &task), RecoveryAction::Fail);
    }

    #[test]
    fn startup_recovery_requeues_orphaned_work_and_keeps_the_worktree() {
        let store = Store::in_memory().unwrap();
        store
            .insert_project("p", "P", "G:/w", "main", None, &Default::default())
            .unwrap();
        let hub = EventHub::new();
        // A live agent and task.
        store
            .insert_agent("a1", "p", "t1", "backend", "backend", "", &[], &[], None)
            .unwrap();
        let task = Task {
            id: "t1".into(),
            project_id: "p".into(),
            request_id: "r".into(),
            requirement_id: None,
            title: "T".into(),
            description: String::new(),
            capability: "backend".into(),
            role: "backend".into(),
            agent_id: Some("a1".into()),
            status: TaskStatus::Running,
            priority: 1,
            depends_on: vec![],
            inputs: vec![],
            outputs: vec![],
            acceptance: vec![],
            resources: vec![],
            max_retries: 2,
            attempts: 1,
            timeout_ms: 1000,
            deadline_at: None,
            worktree: Some("G:/wt".into()),
            branch: Some("ncag/t1".into()),
            result: None,
            blocker: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(0),
            ended_at: None,
        };
        store.insert_task(&task).unwrap();
        store.set_agent_status("a1", AgentStatus::Working).unwrap();
        store.heartbeat("a1", None).unwrap();
        let report = recover_on_startup(&store, &hub).unwrap();
        // Fresh heartbeat (now) -> not orphaned; nothing is lost.
        assert!(report["recoveredTasks"].as_array().unwrap().is_empty());
        // Simulate a stale heartbeat by resetting the clock fields.
        {
            let conn = store.raw_lock();
            conn.execute(
                "UPDATE agents SET last_heartbeat = 1, last_activity = 1 WHERE id = 'a1'",
                [],
            )
            .unwrap();
        }
        let report = recover_on_startup(&store, &hub).unwrap();
        let recovered = report["recoveredTasks"].as_array().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0]["worktree"], "G:/wt");
        assert_eq!(store.task("t1").unwrap().status, TaskStatus::Ready);
    }
}
