//! Durable messaging: typed messages, dedupe keys, acknowledgment, delivery state and
//! inbox retrieval for agents (spec: specs/nc-ag-orchestration.md section 7).

use serde_json::json;

use crate::db::now_ms;
use crate::error::Result;
use crate::types::{Message, MessageKind};
use crate::Store;

/// Channel names are stable: `project:<id>`, `task:<id>`, `agent:<id>`, `global`.
pub fn project_channel(project_id: &str) -> String {
    format!("project:{project_id}")
}

pub fn task_channel(task_id: &str) -> String {
    format!("task:{task_id}")
}

pub fn agent_channel(agent_id: &str) -> String {
    format!("agent:{agent_id}")
}

pub const GLOBAL_CHANNEL: &str = "global";
pub const ORCHESTRATOR: &str = "orchestrator";
pub const USER: &str = "user";

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub project_id: String,
    pub task_id: Option<String>,
    pub kind: MessageKind,
    pub sender: String,
    pub recipient: String,
    pub channel: String,
    pub subject: String,
    pub body: String,
    pub refs: Vec<String>,
    pub correlation_id: String,
    pub priority: i32,
    pub requires_ack: bool,
    pub dedupe_key: Option<String>,
}

impl NewMessage {
    pub fn to_agent(
        project_id: &str,
        task_id: Option<&str>,
        agent_id: &str,
        kind: MessageKind,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            project_id: project_id.to_string(),
            task_id: task_id.map(str::to_string),
            kind,
            sender: ORCHESTRATOR.to_string(),
            recipient: agent_id.to_string(),
            channel: agent_channel(agent_id),
            subject: subject.into(),
            body: body.into(),
            refs: Vec::new(),
            correlation_id: uuid::Uuid::new_v4().to_string(),
            priority: 2,
            requires_ack: true,
            dedupe_key: None,
        }
    }

    pub fn to_project(
        project_id: &str,
        task_id: Option<&str>,
        kind: MessageKind,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            project_id: project_id.to_string(),
            task_id: task_id.map(str::to_string),
            kind,
            sender: ORCHESTRATOR.to_string(),
            recipient: project_channel(project_id),
            channel: project_channel(project_id),
            subject: subject.into(),
            body: body.into(),
            refs: Vec::new(),
            correlation_id: uuid::Uuid::new_v4().to_string(),
            priority: 2,
            requires_ack: false,
            dedupe_key: None,
        }
    }

    pub fn from_agent(
        project_id: &str,
        task_id: &str,
        agent_id: &str,
        kind: MessageKind,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            project_id: project_id.to_string(),
            task_id: Some(task_id.to_string()),
            kind,
            sender: agent_id.to_string(),
            recipient: ORCHESTRATOR.to_string(),
            channel: project_channel(project_id),
            subject: subject.into(),
            body: body.into(),
            refs: Vec::new(),
            correlation_id: uuid::Uuid::new_v4().to_string(),
            priority: 2,
            requires_ack: false,
            dedupe_key: None,
        }
    }

    pub fn with_dedupe(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }

    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

/// Persist a message and return it. A duplicate dedupe key is a no-op (idempotent send).
pub fn send(store: &Store, message: NewMessage) -> Result<Option<Message>> {
    let now = now_ms();
    let record = Message {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: message.project_id,
        task_id: message.task_id,
        kind: message.kind,
        sender: message.sender,
        recipient: message.recipient,
        channel: message.channel,
        subject: message.subject,
        body: message.body,
        refs: message.refs,
        correlation_id: message.correlation_id,
        priority: message.priority,
        requires_ack: message.requires_ack,
        ack_state: if message.requires_ack {
            "pending"
        } else {
            "not_required"
        }
        .to_string(),
        delivery_state: "queued".to_string(),
        dedupe_key: message.dedupe_key,
        created_at: now,
    };
    if !store.insert_message(&record)? {
        return Ok(None);
    }
    Ok(Some(record))
}

/// Mark a message as delivered when the target agent actually reads it.
pub fn deliver_to_agent(store: &Store, message_id: &str) -> Result<()> {
    store.mark_delivered(message_id, "delivered")
}

/// Dead-letter sweep: acknowledged-required messages that stayed pending past `ttl_ms`.
pub fn sweep_dead_letters(store: &Store, project_id: &str, ttl_ms: i64) -> Result<usize> {
    let now = now_ms();
    let messages = store.messages_for_project(project_id, 500)?;
    let mut swept = 0usize;
    for message in messages {
        if message.requires_ack
            && message.ack_state == "pending"
            && message.delivery_state != "dead_letter"
            && now - message.created_at > ttl_ms
        {
            store.mark_delivered(&message.id, "dead_letter")?;
            swept += 1;
        }
    }
    Ok(swept)
}

/// Context package message list for one agent:
/// direct messages addressed to it, project broadcasts, task channel, system events.
pub fn inbox_for_agent(
    store: &Store,
    project_id: &str,
    agent_id: &str,
    task_id: Option<&str>,
) -> Result<Vec<Message>> {
    let mut inbox = store.messages_for_agent(project_id, agent_id)?;
    for message in store.messages_for_project(project_id, 200)? {
        let matches_project = message.channel == project_channel(project_id);
        let matches_task = task_id.is_some_and(|task| message.task_id.as_deref() == Some(task));
        if (matches_project || matches_task) && !inbox.iter().any(|item| item.id == message.id) {
            inbox.push(message);
        }
    }
    inbox.sort_by_key(|message| message.created_at);
    Ok(inbox)
}

/// Shared awareness snapshot: who exists, who is working, who is blocked.
pub fn awareness(store: &Store, project_id: &str) -> Result<serde_json::Value> {
    let agents = store.agents_for_project(project_id)?;
    let mut working = Vec::new();
    let mut idle = Vec::new();
    let mut blocked = Vec::new();
    let mut failed = Vec::new();
    for agent in &agents {
        match agent.status {
            crate::types::AgentStatus::Working | crate::types::AgentStatus::Reviewing => working
                .push(json!({ "agentId": agent.id, "taskId": agent.task_id, "role": agent.role })),
            crate::types::AgentStatus::Idle | crate::types::AgentStatus::Stopped => {
                idle.push(json!({ "agentId": agent.id, "role": agent.role }))
            }
            crate::types::AgentStatus::Blocked | crate::types::AgentStatus::HelpRequested => {
                blocked.push(
                    json!({ "agentId": agent.id, "taskId": agent.task_id, "role": agent.role }),
                )
            }
            crate::types::AgentStatus::Failed => failed.push(
                json!({ "agentId": agent.id, "taskId": agent.task_id, "error": agent.error }),
            ),
            _ => {}
        }
    }
    Ok(json!({
        "agents": agents.iter().map(|agent| json!({
            "agentId": agent.id,
            "role": agent.role,
            "status": agent.status.as_str(),
            "taskId": agent.task_id,
            "capabilities": agent.capabilities,
        })).collect::<Vec<_>>(),
        "working": working,
        "idle": idle,
        "blocked": blocked,
        "failed": failed,
    }))
}

/// Break a blocking discussion by raising an escalation message with evidence.
pub fn escalate(
    store: &Store,
    project_id: &str,
    task_id: Option<&str>,
    subject: &str,
    evidence: &str,
) -> Result<()> {
    let message = NewMessage::to_project(
        project_id,
        task_id,
        MessageKind::Escalation,
        subject,
        evidence,
    )
    .with_priority(4);
    let message = Message {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: message.project_id,
        task_id: message.task_id,
        kind: message.kind,
        sender: message.sender,
        recipient: message.recipient,
        channel: message.channel,
        subject: message.subject,
        body: message.body,
        refs: message.refs,
        correlation_id: message.correlation_id,
        priority: message.priority,
        requires_ack: false,
        ack_state: "not_required".to_string(),
        delivery_state: "queued".to_string(),
        dedupe_key: message.dedupe_key,
        created_at: now_ms(),
    };
    store.insert_message(&message)?;
    Ok(())
}
