//! Event stream: every orchestration step is persisted (audit-capable) and broadcast to
//! live subscribers (SSE). The DB row is the source of truth; subscribers are optional.

use serde_json::Value;
use tokio::sync::broadcast;

use crate::error::Result;
use crate::types::Event;
use crate::Store;

pub const EVENT_CAPACITY: usize = 512;

#[derive(Clone)]
pub struct EventHub {
    sender: broadcast::Sender<Event>,
}

impl EventHub {
    pub fn new() -> Self {
        let (sender, _receiver) = broadcast::channel(EVENT_CAPACITY);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    pub fn publish(&self, event: &Event) {
        // A send error only means there are no live subscribers right now.
        let _ = self.sender.send(event.clone());
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

/// Persist and broadcast one event.
pub fn emit(
    store: &Store,
    hub: &EventHub,
    project_id: Option<&str>,
    task_id: Option<&str>,
    agent_id: Option<&str>,
    kind: &str,
    payload: Value,
) -> Result<Event> {
    let event = store.append_event(project_id, task_id, agent_id, kind, &payload)?;
    hub.publish(&event);
    Ok(event)
}

/// Terminal / notable kinds that the dashboard highlights.
pub fn is_attention_kind(kind: &str) -> bool {
    matches!(
        kind,
        "task.failed"
            | "task.rejected"
            | "agent.stuck"
            | "decision.opened"
            | "budget.exceeded"
            | "provider.unavailable"
            | "integration.conflict"
    )
}
