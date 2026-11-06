//! Conversation telemetry types (port of `packages/services/src/conversation-telemetry/conversationTelemetry.ts`
//! and related shared protocol facts). This module provides the Rust-side data shapes and a
//! sink abstraction; event wiring is owned by the agent/channel layer.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConversationTelemetryWorkspaceTarget {
    pub workspace_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_identity: Option<String>,
}

/// A generic conversation telemetry fact envelope. The concrete fact schema is defined by
/// `zcode-protocol-v4`; we preserve it as JSON for forward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationTelemetryFact {
    #[serde(flatten)]
    pub fields: Value,
}

/// Sink for conversation telemetry facts (testable, no global state).
pub trait ConversationTelemetrySink: Send + Sync {
    fn publish(
        &self,
        target: &ConversationTelemetryWorkspaceTarget,
        fact: ConversationTelemetryFact,
    );
}

#[derive(Default)]
pub struct VecConversationTelemetrySink {
    pub events: std::sync::Mutex<
        Vec<(
            ConversationTelemetryWorkspaceTarget,
            ConversationTelemetryFact,
        )>,
    >,
}

impl ConversationTelemetrySink for VecConversationTelemetrySink {
    fn publish(
        &self,
        target: &ConversationTelemetryWorkspaceTarget,
        fact: ConversationTelemetryFact,
    ) {
        self.events.lock().unwrap().push((target.clone(), fact));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sink_records_facts() {
        let sink = VecConversationTelemetrySink::default();
        let target = ConversationTelemetryWorkspaceTarget {
            workspace_path: "w".into(),
            workspace_identity: None,
        };
        sink.publish(
            &target,
            ConversationTelemetryFact {
                fields: json!({"kind":"turn"}),
            },
        );
        assert_eq!(sink.events.lock().unwrap().len(), 1);
    }
}
