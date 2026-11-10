#![allow(clippy::doc_lazy_continuation)]
//! Handoff skeleton for `nc-code-services-subagents` — pure data/types, no IO.
//!
//! This module follows the `nc-code-services-agent` pattern
//! (`lib.rs` composition + `session.rs` dispatch): a SQLite-backable
//! persistence struct with serde derives, a Send+Sync port trait, and
//! channel dispatch glue where the crate registers a channel.
//!
//! No HTTP, terminal IO, websockets, or SQLite driver live here. The
//! existing service in this crate keeps its real implementation; this
//! module only adds the handoff surface so a future SQLite backing
//! can be dropped in without changing the channel contract.
//!
//! Channel: `no channel (lib/service only)`. Primary service: `SubagentsService`.

use serde::{Deserialize, Serialize};

/// Namespaced handoff envelope version.
pub const HANDOFF_VERSION: u32 = 1;

/// Crate tag, stable across versions.
pub const HANDOFF_CRATE: &str = "nc-code-services-subagents";

/// Persisted record for the handoff. Backed by SQLite when the crate
/// adopts it; serializes as camelCase to match the existing channel
/// payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentRecord {
    /// Stable id (uuid or content hash string).
    pub id: String,
    /// Channel or domain kind.
    pub kind: String,
    /// Opaque domain payload, preserved verbatim.
    #[serde(default)]
    pub payload: serde_json::Value,
    /// Milliseconds since the unix epoch.
    pub created_at: i64,
    /// Milliseconds since the unix epoch.
    pub updated_at: i64,
    /// Optimistic-concurrency revision, bumped per write.
    #[serde(default)]
    pub revision: u64,
}

impl SubagentRecord {
    /// Minimal constructor for tests and fixtures.
    pub fn new(id: impl Into<String>, kind: impl Into<String>) -> Self {
        let now = Self::now_ms();
        Self {
            id: id.into(),
            kind: kind.into(),
            payload: serde_json::Value::Null,
            created_at: now,
            updated_at: now,
            revision: 1,
        }
    }

    /// Current wall-clock milliseconds.
    pub fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Bump revision and touch `updated_at`.
    pub fn touch(&mut self) {
        self.updated_at = Self::now_ms();
        self.revision = self.revision.saturating_add(1);
    }

    /// Serialize to pretty JSON (the on-disk shape).
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserialize from JSON text.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// Generic handoff envelope for round-tripping a batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffEnvelope<T> {
    pub version: u32,
    pub crate_name: String,
    pub records: Vec<T>,
    pub channel: Option<String>,
}

impl<T> HandoffEnvelope<T> {
    /// Wrap records for transport or storage.
    pub fn new(records: Vec<T>) -> Self {
        Self {
            version: HANDOFF_VERSION,
            crate_name: HANDOFF_CRATE.to_string(),
            records,
            channel: None,
        }
    }
}

/// Port trait the SQLite-backed store will implement. Channel dispatch
/// calls through this trait so tests can substitute an in-memory store.
pub trait ServicesSubagentsPort: Send + Sync {
    /// Stable crate tag.
    fn crate_name(&self) -> &'static str {
        HANDOFF_CRATE
    }
    /// Fetch a record by id, if present.
    fn get(&self, id: &str) -> Option<SubagentRecord>;
    /// List all records, oldest first.
    fn list(&self) -> Vec<SubagentRecord>;
    /// Insert or replace a record.
    fn upsert(&self, record: SubagentRecord) -> SubagentRecord;
}

// `Channel` is the existing dispatch contract; this trait is the
// persistence port behind it. Both are Send + Sync by construction.
#[allow(dead_code)]
fn _assert_send_sync<T: Send + Sync>() {}

/// Compile-time proof the primary domain type is Send + Sync.
pub fn assert_domain_is_send_sync() {
    _assert_send_sync::<SubagentRecord>();
    _assert_send_sync::<HandoffEnvelope<SubagentRecord>>();
}

/// In-memory reference store used by the e2e test. A SQLite store
/// with the same semantics can replace it without touching callers.
#[derive(Debug, Default)]
pub struct InMemoryHandoffStore {
    records: std::sync::Mutex<std::collections::BTreeMap<String, SubagentRecord>>,
}

impl InMemoryHandoffStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl ServicesSubagentsPort for InMemoryHandoffStore {
    fn get(&self, id: &str) -> Option<SubagentRecord> {
        self.records
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
    }
    fn list(&self) -> Vec<SubagentRecord> {
        self.records
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
    }
    fn upsert(&self, record: SubagentRecord) -> SubagentRecord {
        let mut map = self.records.lock().unwrap_or_else(|p| p.into_inner());
        map.insert(record.id.clone(), record.clone());
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips_through_json() {
        let record = SubagentRecord::new("id-1", "kind-1");
        let envelope = HandoffEnvelope::new(vec![record.clone()]);
        let text = serde_json::to_string(&envelope).expect("serialize");
        let back: HandoffEnvelope<SubagentRecord> =
            serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, envelope);
        assert_eq!(back.crate_name, HANDOFF_CRATE);
    }

    #[test]
    fn record_touch_bumps_revision() {
        let mut record = SubagentRecord::new("a", "b");
        let rev = record.revision;
        record.touch();
        assert_eq!(record.revision, rev + 1);
        assert!(record.updated_at >= record.created_at);
    }

    #[test]
    fn in_memory_store_is_send_sync_and_usable_as_trait_object() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SubagentRecord>();
        let store = InMemoryHandoffStore::new();
        let record = SubagentRecord::new("x", "y");
        let _boxed: Box<dyn ServicesSubagentsPort> = Box::new(InMemoryHandoffStore::new());
        store.upsert(record.clone());
        assert_eq!(store.get("x"), Some(record));
        assert_eq!(store.list().len(), 1);
    }
}
