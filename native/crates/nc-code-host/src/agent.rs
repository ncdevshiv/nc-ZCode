//! Agent orchestration primitives (ported concepts from TS core/src/agent).
//!
//! Provides turn state, message history management, and lightweight
//! orchestration helpers used by host runtimes. The full model/tool turn loop
//! lives in `turn.rs`; this module holds portable agent state logic.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

/// High-level agent turn state (complementary to runtime TurnState).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTurnPhase {
    Idle,
    Preparing,
    Streaming,
    AwaitingTool,
    ExecutingTools,
    Completing,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub max_turns: Option<usize>,
    pub max_history_messages: usize,
    pub preserve_system_messages: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: None,
            max_history_messages: 400,
            preserve_system_messages: true,
        }
    }
}

/// Minimal message envelope for history management.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentMessage {
    pub role: String,
    pub content: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl AgentMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Value::String(text.into()),
            name: None,
            tool_call_id: None,
            is_error: None,
        }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Value::String(text.into()),
            name: None,
            tool_call_id: None,
            is_error: None,
        }
    }
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: Value::String(text.into()),
            name: None,
            tool_call_id: None,
            is_error: None,
        }
    }
}

/// Bounded conversation history with deterministic trimming.
#[derive(Debug, Clone, Default)]
pub struct MessageHistory {
    messages: VecDeque<AgentMessage>,
}

impl MessageHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, message: AgentMessage) {
        self.messages.push_back(message);
    }

    pub fn extend<I: IntoIterator<Item = AgentMessage>>(&mut self, items: I) {
        self.messages.extend(items);
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &AgentMessage> {
        self.messages.iter()
    }

    pub fn to_vec(&self) -> Vec<AgentMessage> {
        self.messages.iter().cloned().collect()
    }

    /// Trim history to `max_messages`, preserving leading system messages when requested.
    pub fn trim(&mut self, max_messages: usize, preserve_system: bool) {
        if max_messages == 0 {
            self.messages.clear();
            return;
        }
        while self.messages.len() > max_messages {
            let remove_idx = if preserve_system {
                self.messages
                    .iter()
                    .position(|m| m.role != "system")
                    .unwrap_or(0)
            } else {
                0
            };
            self.messages.remove(remove_idx);
        }
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }
}

#[derive(Debug, Clone)]
pub struct AgentOrchestrator {
    config: AgentConfig,
    phase: AgentTurnPhase,
    turns: usize,
    history: MessageHistory,
}

impl Default for AgentOrchestrator {
    fn default() -> Self {
        Self::new(AgentConfig::default())
    }
}

impl AgentOrchestrator {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            phase: AgentTurnPhase::Idle,
            turns: 0,
            history: MessageHistory::new(),
        }
    }

    pub fn max_turns(&self) -> Option<usize> {
        self.config.max_turns
    }

    pub fn phase(&self) -> AgentTurnPhase {
        self.phase
    }

    pub fn turns(&self) -> usize {
        self.turns
    }

    pub fn history(&self) -> &MessageHistory {
        &self.history
    }

    pub fn history_mut(&mut self) -> &mut MessageHistory {
        &mut self.history
    }

    pub fn can_continue(&self) -> bool {
        match self.config.max_turns {
            Some(max) => self.turns < max,
            None => true,
        }
    }

    pub fn transition(&mut self, phase: AgentTurnPhase) {
        self.phase = phase;
    }

    pub fn begin_turn(&mut self) -> bool {
        if !self.can_continue() {
            self.phase = AgentTurnPhase::Failed;
            return false;
        }
        self.turns = self.turns.saturating_add(1);
        self.phase = AgentTurnPhase::Preparing;
        true
    }

    pub fn complete(&mut self) {
        self.phase = AgentTurnPhase::Completed;
    }

    pub fn fail(&mut self) {
        self.phase = AgentTurnPhase::Failed;
    }

    pub fn cancel(&mut self) {
        self.phase = AgentTurnPhase::Cancelled;
    }

    pub fn push_message(&mut self, message: AgentMessage) {
        self.history.push(message);
        self.history.trim(
            self.config.max_history_messages,
            self.config.preserve_system_messages,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_orchestrator_turn_limits() {
        let mut orch = AgentOrchestrator::new(AgentConfig {
            max_turns: Some(2),
            ..Default::default()
        });
        assert!(orch.begin_turn());
        assert_eq!(orch.turns(), 1);
        assert!(orch.begin_turn());
        assert_eq!(orch.turns(), 2);
        assert!(!orch.begin_turn());
        assert_eq!(orch.phase(), AgentTurnPhase::Failed);
    }

    #[test]
    fn history_trim_preserves_system() {
        let mut history = MessageHistory::new();
        history.push(AgentMessage::system("sys"));
        history.push(AgentMessage::user("u1"));
        history.push(AgentMessage::assistant_text("a1"));
        history.push(AgentMessage::user("u2"));
        history.trim(3, true);
        let msgs = history.to_vec();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[2].content, Value::String("u2".to_string()));
    }

    #[test]
    fn push_message_applies_bounded_history() {
        let mut orch = AgentOrchestrator::new(AgentConfig {
            max_history_messages: 2,
            preserve_system_messages: false,
            ..Default::default()
        });
        orch.push_message(AgentMessage::user("1"));
        orch.push_message(AgentMessage::user("2"));
        orch.push_message(AgentMessage::user("3"));
        let msgs = orch.history().to_vec();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, Value::String("2".to_string()));
    }

    #[test]
    fn phase_transitions_are_explicit() {
        let mut orch = AgentOrchestrator::default();
        assert_eq!(orch.phase(), AgentTurnPhase::Idle);
        orch.begin_turn();
        assert_eq!(orch.phase(), AgentTurnPhase::Preparing);
        orch.transition(AgentTurnPhase::Streaming);
        orch.complete();
        assert_eq!(orch.phase(), AgentTurnPhase::Completed);
    }
}
