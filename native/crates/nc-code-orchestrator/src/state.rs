//! Explicit state machines. Illegals transitions are errors, never coerced:
//! the orchestrator records the attempt and keeps the prior state.

use crate::types::{AgentStatus, RequestStatus, TaskStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub from: String,
    pub to: String,
    pub entity: &'static str,
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal {} transition: {} -> {}",
            self.entity, self.from, self.to
        )
    }
}

impl std::error::Error for TransitionError {}

pub fn task_can_transition(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    match from {
        Created => matches!(to, Planned | Cancelled | Blocked),
        Planned => matches!(to, Ready | Cancelled | Blocked),
        Ready => matches!(to, Assigned | Cancelled | Blocked),
        Assigned => matches!(to, Running | Ready | Cancelled | Failed),
        Running => matches!(
            to,
            Review | Verification | Completed | Failed | Cancelled | Blocked | Ready
        ),
        Review => matches!(to, Verification | Running | Rejected | Failed | Cancelled),
        Verification => matches!(to, Completed | Review | Rejected | Failed | Cancelled),
        Rejected => matches!(to, Ready | Failed | Cancelled),
        Completed => matches!(to, Ready),
        Failed => matches!(to, Ready | Cancelled),
        Cancelled => false,
        Blocked => matches!(to, Ready | Cancelled | Failed),
    }
}

pub fn agent_can_transition(from: AgentStatus, to: AgentStatus) -> bool {
    use AgentStatus::*;
    match from {
        Created => matches!(to, Initializing | Stopping | Failed),
        Initializing => matches!(to, Starting | Failed | Stopping),
        Starting => matches!(to, Idle | Working | Assigned | Failed | Stopping),
        Idle => matches!(
            to,
            Assigned | Working | Stopping | Failed | Stopped | Recovering
        ),
        Assigned => matches!(to, Starting | Working | Stopping | Failed | Recovering),
        Working => matches!(
            to,
            Waiting
                | Blocked
                | HelpRequested
                | Reviewing
                | HandoffPending
                | Idle
                | Stopping
                | Failed
                | Recovering
                | Stopped
        ),
        Waiting => matches!(to, Working | Stopping | Failed | Recovering | Blocked),
        Blocked => matches!(to, Working | Recovering | Failed | Stopping),
        HelpRequested => matches!(to, Working | Waiting | Failed | Recovering),
        Reviewing => matches!(to, Working | Idle | Failed | Stopping),
        HandoffPending => matches!(to, Working | Failed | Stopping | Stopped),
        Recovering => matches!(to, Working | Idle | Failed | Stopping),
        Stopping => matches!(to, Stopped | Failed),
        Stopped => false,
        Failed => matches!(to, Recovering | Stopped),
    }
}

pub fn request_can_transition(from: RequestStatus, to: RequestStatus) -> bool {
    use RequestStatus::*;
    match from {
        Received => matches!(to, Analyzing | Failed | Cancelled),
        Analyzing => matches!(
            to,
            Planned | AwaitingApproval | WaitingProvider | Failed | Cancelled
        ),
        Planned => matches!(
            to,
            Executing | AwaitingApproval | WaitingProvider | Failed | Cancelled
        ),
        AwaitingApproval => matches!(
            to,
            Planned | Executing | WaitingProvider | Failed | Cancelled
        ),
        WaitingProvider => matches!(to, Planned | Executing | Failed | Cancelled),
        Executing => matches!(to, Completed | Failed | Cancelled | WaitingProvider),
        Completed => false,
        Failed => matches!(to, Analyzing | Planned),
        Cancelled => false,
    }
}

pub fn ensure_task(from: TaskStatus, to: TaskStatus) -> Result<(), TransitionError> {
    if task_can_transition(from, to) {
        Ok(())
    } else {
        Err(TransitionError {
            from: from.as_str().to_string(),
            to: to.as_str().to_string(),
            entity: "task",
        })
    }
}

pub fn ensure_agent(from: AgentStatus, to: AgentStatus) -> Result<(), TransitionError> {
    if agent_can_transition(from, to) {
        Ok(())
    } else {
        Err(TransitionError {
            from: from.as_str().to_string(),
            to: to.as_str().to_string(),
            entity: "agent",
        })
    }
}

pub fn ensure_request(from: RequestStatus, to: RequestStatus) -> Result<(), TransitionError> {
    if request_can_transition(from, to) {
        Ok(())
    } else {
        Err(TransitionError {
            from: from.as_str().to_string(),
            to: to.as_str().to_string(),
            entity: "request",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_tasks_are_terminal_except_explicit_reopen() {
        assert!(task_can_transition(
            TaskStatus::Completed,
            TaskStatus::Ready
        ));
        assert!(!task_can_transition(
            TaskStatus::Completed,
            TaskStatus::Running
        ));
        assert!(!task_can_transition(
            TaskStatus::Cancelled,
            TaskStatus::Ready
        ));
    }

    #[test]
    fn tasks_must_be_assigned_before_running() {
        assert!(!task_can_transition(TaskStatus::Ready, TaskStatus::Running));
        assert!(task_can_transition(TaskStatus::Ready, TaskStatus::Assigned));
        assert!(task_can_transition(
            TaskStatus::Assigned,
            TaskStatus::Running
        ));
    }

    #[test]
    fn stopped_agents_do_not_resume() {
        assert!(!agent_can_transition(
            AgentStatus::Stopped,
            AgentStatus::Working
        ));
        assert!(agent_can_transition(
            AgentStatus::Working,
            AgentStatus::Recovering
        ));
    }
}
