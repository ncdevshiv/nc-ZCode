//! The publish path for `report()` (port of `engine-report.ts`).
//!
//! Synchronous, no driver round-trip: write one journal row + emit one event, then done.

use serde_json::Value;

use crate::engine_state::Core;
use crate::types::{
    input_hash, InstanceRef, NodeKind, NodeRecord, NodeRecordStatus, RunEvent, WorkflowError,
    WorkflowErrorCode, REPORT_CAPS,
};

impl Core {
    /// Publish an intermediate result (Boundary A's `report`).
    pub fn publish_report(&mut self, site_id: &str, item: Value, artifact_id: Option<&str>) {
        if self.run_settled {
            return; // Same as log: nothing is accepted after settlement
        }
        let ordinal = self.next_ordinal(site_id);
        let instance = InstanceRef {
            site_id: site_id.to_string(),
            ordinal,
        };

        // Serialization probe first (the byte count is the metric the limit is measured against). A Value is already a JSON value, so a cycle is impossible; keep the limit check anyway.
        let serialized = serde_json::to_string(&item).unwrap_or_else(|_| "null".to_string());
        // inputHash deliberately covers only the item; the tag does not enter the hash.
        let hash = input_hash(&item);

        if let Some(recorded) = self.journal().get_node(&self.run_id, site_id, ordinal) {
            if recorded.input_hash != hash {
                let err = crate::scheduler::hash_mismatch(&instance, &recorded.input_hash, &hash);
                self.fail_run(err);
                return;
            }
            return; // replay dedup: skip silently (no event, no duplicate append)
        }

        // A tag is present: it must already have been declared as a preset artifact (checked before the caps).
        if let Some(artifact_id) = artifact_id {
            if !self.is_declared_preset(artifact_id) {
                let declared = self.declared_preset_ids().join(", ");
                self.fail_run(WorkflowError::new(
                    WorkflowErrorCode::ArtifactUndeclared,
                    format!(
                        "report() tag \"{artifact_id}\" is not a declared preset artifact (at {}). \
                         Declare it once at the top of the script, e.g. artifact.chart(\"{artifact_id}\", …), \
                         before tagging reports with it. Declared presets: {}.",
                        instance.key(),
                        if declared.is_empty() { "(none)".to_string() } else { declared }
                    ),
                ));
                return;
            }
        }

        let bytes = serialized.len();
        if bytes > REPORT_CAPS.max_item_serialized_bytes {
            self.fail_run(WorkflowError::new(
                WorkflowErrorCode::ReportCapExceeded,
                format!(
                    "report() item at {} is {bytes} bytes, over the {}-byte limit. Report a summary instead.",
                    instance.key(),
                    REPORT_CAPS.max_item_serialized_bytes
                ),
            ));
            return;
        }
        if self.report_count() >= REPORT_CAPS.max_items_per_run {
            self.fail_run(WorkflowError::new(
                WorkflowErrorCode::ReportCapExceeded,
                format!(
                    "This run already reported {} items, the maximum. Report findings, not chatter.",
                    REPORT_CAPS.max_items_per_run
                ),
            ));
            return;
        }
        self.count_report();
        self.journal().put_node(NodeRecord {
            run_id: self.run_id.clone(),
            site_id: site_id.to_string(),
            ordinal,
            kind: NodeKind::Report,
            actor_site_id: None,
            actor_ordinal: None,
            actor_seq: None,
            input_hash: hash,
            status: NodeRecordStatus::Completed,
            result: Some(item.clone()),
            error: None,
            stats: None,
            artifact_id: artifact_id.map(str::to_string),
            input: None,
            message_boundary: None,
        });
        self.record(RunEvent::Report {
            instance,
            item,
            artifact_id: artifact_id.map(str::to_string),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_core;
    use crate::journal_memory::InMemoryJournalStore;
    use crate::types::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    struct NullDriver {
        journal: Arc<InMemoryJournalStore>,
        events: Mutex<Vec<RunEvent>>,
    }

    #[async_trait::async_trait]
    impl WorkflowDriver for NullDriver {
        fn journal(&self) -> &dyn JournalStorePort {
            &*self.journal
        }
        fn emit(&self, event: &RunEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
        async fn create_actor_session(
            &self,
            _actor: &ActorRef,
            _persona: &PersonaSpec,
            _seed: Option<ActorSessionSeed>,
        ) -> Result<SessionRef, WorkflowError> {
            unreachable!("report tests never create sessions")
        }
        fn start_ask(&self, _s: &SessionRef, _i: &InstanceRef, _m: AskMessage) {}
        fn respond_to_submit(&self, _i: &InstanceRef, _v: SubmitVerdict) {}
        fn cancel_ask(&self, _i: &InstanceRef) {}
        async fn execute_world_read(
            &self,
            _op: &str,
            _args: Vec<Value>,
        ) -> Result<Value, WorkflowError> {
            unreachable!()
        }
    }

    fn core() -> (Core, Arc<NullDriver>) {
        core_with_journal(Arc::new(InMemoryJournalStore::new()))
    }

    fn core_with_journal(journal: Arc<InMemoryJournalStore>) -> (Core, Arc<NullDriver>) {
        let driver = Arc::new(NullDriver {
            journal: journal.clone(),
            events: Mutex::new(Vec::new()),
        });
        (
            test_core(
                "run-r",
                driver.clone(),
                Caps { max_concurrency: 1 },
                Arc::new(|_s: Option<&Value>, _v: &Value| Vec::new()),
            ),
            driver,
        )
    }

    #[test]
    fn report_writes_node_and_event() {
        let (mut core, driver) = core();
        core.publish_report("report#1", json!({"a": 1}), None);
        let node = core.journal().get_node("run-r", "report#1", 1).unwrap();
        assert_eq!(node.kind, NodeKind::Report);
        assert_eq!(node.status, NodeRecordStatus::Completed);
        assert_eq!(node.result, Some(json!({"a": 1})));
        let events = driver.events.lock().unwrap();
        assert!(matches!(events.last(), Some(RunEvent::Report { .. })));
    }

    #[test]
    fn report_replay_is_silently_skipped() {
        let journal = Arc::new(InMemoryJournalStore::new());
        let (mut first, _driver) = core_with_journal(journal.clone());
        first.publish_report("report#1", json!(1), None);
        // A new engine instance on the same journal replaying the same site -> silently skipped, no re-emitted event.
        let (mut replay, replay_driver) = core_with_journal(journal);
        replay.publish_report("report#1", json!(1), None);
        assert!(
            replay_driver.events.lock().unwrap().is_empty(),
            "no duplicate event on replay"
        );
    }

    #[test]
    fn report_replay_hash_mismatch_fails_the_run() {
        let journal = Arc::new(InMemoryJournalStore::new());
        let (mut first, _driver) = core_with_journal(journal.clone());
        first.publish_report("report#1", json!(1), None);
        let (mut replay, _driver) = core_with_journal(journal);
        replay.publish_report("report#1", json!(2), None);
        assert_eq!(
            replay.run_failure.clone().unwrap().code,
            WorkflowErrorCode::InputHashMismatch
        );
    }

    #[test]
    fn undeclared_tag_fails_the_run() {
        let (mut core, _driver) = core();
        core.publish_report("report#1", json!(1), Some("board"));
        assert_eq!(
            core.run_failure.clone().unwrap().code,
            WorkflowErrorCode::ArtifactUndeclared
        );
    }

    #[test]
    fn oversized_item_fails_the_run() {
        let (mut core, _driver) = core();
        let big = "x".repeat(REPORT_CAPS.max_item_serialized_bytes + 1);
        core.publish_report("report#1", json!(big), None);
        assert_eq!(
            core.run_failure.clone().unwrap().code,
            WorkflowErrorCode::ReportCapExceeded
        );
    }
}
