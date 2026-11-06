//! In-memory journal (for tests and pure replay; contract tests pin it to the same shape as the SQLite implementation).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::{
    ActorRecord, JournalStorePort, ListEventsOptions, NodeRecord, RunEvent, RunRecord,
    RunSettlementRecord, RunStatus, StoredEvent,
};

#[derive(Default)]
struct Inner {
    runs: HashMap<String, RunRecord>,
    actors: HashMap<(String, String, u32), ActorRecord>,
    nodes: HashMap<(String, String, u32), NodeRecord>,
    actor_order: HashMap<(String, String, u32), u64>,
    node_order: HashMap<(String, String, u32), u64>,
    events: HashMap<String, Vec<StoredEvent>>,
    last_sequence: HashMap<String, u64>,
    next_order: u64,
}

/// In-memory implementation: shares [`JournalStorePort`] with the SQLite implementation.
#[derive(Default)]
pub struct InMemoryJournalStore {
    inner: Mutex<Inner>,
}

impl InMemoryJournalStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

impl JournalStorePort for InMemoryJournalStore {
    fn create_run(&self, record: RunRecord) {
        let mut inner = self.inner.lock().expect("journal lock");
        inner.runs.insert(record.run_id.clone(), record);
    }

    fn get_run(&self, run_id: &str) -> Option<RunRecord> {
        let inner = self.inner.lock().expect("journal lock");
        inner.runs.get(run_id).cloned()
    }

    fn update_run_status(
        &self,
        run_id: &str,
        status: RunStatus,
        settlement: Option<&RunSettlementRecord>,
    ) {
        let mut inner = self.inner.lock().expect("journal lock");
        let Some(record) = inner.runs.get_mut(run_id) else {
            return;
        };
        record.status = status;
        if let Some(settlement) = settlement {
            record.stop_reason = settlement.stop_reason;
            record.superseded_by = settlement.superseded_by.clone();
            record.failure = settlement.failure.clone();
            if settlement.result.is_some() {
                record.result = settlement.result.clone();
            }
        }
    }

    fn update_run_usage(&self, run_id: &str, spent_tokens: u64) {
        let mut inner = self.inner.lock().expect("journal lock");
        let record = inner
            .runs
            .get_mut(run_id)
            .unwrap_or_else(|| panic!("updateRunUsage: unknown run {run_id}"));
        record.spent_tokens = spent_tokens;
    }

    fn put_actor(&self, record: ActorRecord) {
        let mut inner = self.inner.lock().expect("journal lock");
        let key = (
            record.run_id.clone(),
            record.site_id.clone(),
            record.ordinal,
        );
        if !inner.actor_order.contains_key(&key) {
            let order = inner.next_order;
            inner.next_order += 1;
            inner.actor_order.insert(key.clone(), order);
        }
        inner.actors.insert(key, record);
    }

    fn get_actor(&self, run_id: &str, site_id: &str, ordinal: u32) -> Option<ActorRecord> {
        let inner = self.inner.lock().expect("journal lock");
        inner
            .actors
            .get(&(run_id.to_string(), site_id.to_string(), ordinal))
            .cloned()
    }

    fn list_actors(&self, run_id: &str) -> Vec<ActorRecord> {
        let inner = self.inner.lock().expect("journal lock");
        let mut rows: Vec<ActorRecord> = inner
            .actors
            .values()
            .filter(|r| r.run_id == run_id)
            .cloned()
            .collect();
        // Insertion order (matches the SQLite repository's `order by id`).
        rows.sort_by_key(|r| {
            inner
                .actor_order
                .get(&(r.run_id.clone(), r.site_id.clone(), r.ordinal))
                .copied()
                .unwrap_or(u64::MAX)
        });
        rows
    }

    fn put_node(&self, record: NodeRecord) {
        let mut inner = self.inner.lock().expect("journal lock");
        let key = (
            record.run_id.clone(),
            record.site_id.clone(),
            record.ordinal,
        );
        if !inner.node_order.contains_key(&key) {
            let order = inner.next_order;
            inner.next_order += 1;
            inner.node_order.insert(key.clone(), order);
        }
        inner.nodes.insert(key, record);
    }

    fn get_node(&self, run_id: &str, site_id: &str, ordinal: u32) -> Option<NodeRecord> {
        let inner = self.inner.lock().expect("journal lock");
        inner
            .nodes
            .get(&(run_id.to_string(), site_id.to_string(), ordinal))
            .cloned()
    }

    fn list_nodes(&self, run_id: &str) -> Vec<NodeRecord> {
        let inner = self.inner.lock().expect("journal lock");
        let mut rows: Vec<NodeRecord> = inner
            .nodes
            .values()
            .filter(|r| r.run_id == run_id)
            .cloned()
            .collect();
        // Insertion order (same as the SQLite repository; resume's artifact restoration relies on this same order).
        rows.sort_by_key(|r| {
            inner
                .node_order
                .get(&(r.run_id.clone(), r.site_id.clone(), r.ordinal))
                .copied()
                .unwrap_or(u64::MAX)
        });
        rows
    }

    fn append_event(&self, run_id: &str, event: &RunEvent) -> StoredEvent {
        let mut inner = self.inner.lock().expect("journal lock");
        // The TS repository starts sequence at 0 (`coalesce(max(sequence)+1, 0)`), value-identical across implementations.
        let next = inner.last_sequence.get(run_id).map(|n| n + 1).unwrap_or(0);
        inner.last_sequence.insert(run_id.to_string(), next);
        let stored = StoredEvent {
            sequence: next,
            event: event.clone(),
            time_created: Some(now_ms()),
        };
        inner
            .events
            .entry(run_id.to_string())
            .or_default()
            .push(stored.clone());
        stored
    }

    fn list_events(&self, run_id: &str, opts: ListEventsOptions) -> Vec<StoredEvent> {
        let inner = self.inner.lock().expect("journal lock");
        let Some(events) = inner.events.get(run_id) else {
            return Vec::new();
        };
        events
            .iter()
            .filter(|e| {
                opts.after_sequence
                    .map(|after| e.sequence > after)
                    .unwrap_or(true)
            })
            .take(opts.limit.unwrap_or(usize::MAX))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Caps, RunEvent};
    use serde_json::json;

    fn run(run_id: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.into(),
            parent_session_id: None,
            cwd: None,
            name: None,
            tool_call_id: None,
            script_text: None,
            script_hash: None,
            args: None,
            resumed_from: None,
            caps: Caps { max_concurrency: 2 },
            spent_tokens: 0,
            status: RunStatus::Running,
            stop_reason: None,
            superseded_by: None,
            failure: None,
            result: None,
        }
    }

    #[test]
    fn run_status_and_settlement_are_one_write() {
        let store = InMemoryJournalStore::new();
        store.create_run(run("r1"));
        let settlement = RunSettlementRecord {
            stop_reason: None,
            superseded_by: None,
            failure: None,
            result: Some(json!({"ok": true})),
        };
        store.update_run_status("r1", RunStatus::Completed, Some(&settlement));
        let back = store.get_run("r1").unwrap();
        assert_eq!(back.status, RunStatus::Completed);
        assert_eq!(back.result, Some(json!({"ok": true})));
    }

    #[test]
    fn events_have_monotonic_sequences_and_pagination() {
        let store = InMemoryJournalStore::new();
        for message in ["a", "b", "c", "d"] {
            store.append_event(
                "r1",
                &RunEvent::Log {
                    message: message.to_string(),
                },
            );
        }
        let all = store.list_events("r1", ListEventsOptions::default());
        assert_eq!(
            all.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert!(all.iter().all(|e| e.time_created.is_some()));
        let page = store.list_events(
            "r1",
            ListEventsOptions {
                after_sequence: Some(2),
                limit: Some(1),
            },
        );
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].sequence, 3);
        assert!(store
            .list_events("missing", ListEventsOptions::default())
            .is_empty());
    }

    #[test]
    fn node_upsert_replaces_whole_record() {
        let store = InMemoryJournalStore::new();
        let mut node = NodeRecord {
            run_id: "r1".into(),
            site_id: "ask#1".into(),
            ordinal: 1,
            kind: crate::types::NodeKind::Ask,
            actor_site_id: Some("actor#1".into()),
            actor_ordinal: Some(1),
            actor_seq: Some(0),
            input_hash: "h".into(),
            status: crate::types::NodeRecordStatus::Running,
            result: None,
            error: None,
            stats: None,
            artifact_id: None,
            input: None,
            message_boundary: None,
        };
        store.put_node(node.clone());
        node.status = crate::types::NodeRecordStatus::Completed;
        node.result = Some(json!({"a": 1}));
        store.put_node(node);
        let back = store.get_node("r1", "ask#1", 1).unwrap();
        assert_eq!(back.status, crate::types::NodeRecordStatus::Completed);
        assert_eq!(back.result, Some(json!({"a": 1})));
    }

    #[test]
    fn usage_update_requires_known_run() {
        let store = InMemoryJournalStore::new();
        store.create_run(run("r1"));
        store.update_run_usage("r1", 42);
        assert_eq!(store.get_run("r1").unwrap().spent_tokens, 42);
        let result = std::panic::catch_unwind(|| store.update_run_usage("missing", 1));
        assert!(result.is_err(), "unknown run must fail loudly");
    }
}
