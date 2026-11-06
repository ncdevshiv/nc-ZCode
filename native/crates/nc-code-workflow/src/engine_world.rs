//! World nodes (world-read / world-run) and the closing / restoring of the import cache gate (port of `engine-world.ts`).
//!
//! The admission decision is made synchronously inside the lock, the driver's execution is awaited outside the
//! lock by [`crate::engine::WorkflowEngine`], and settlement returns inside the lock -- same order as TS's `readWorld` promise chain.

use serde_json::Value;

use crate::engine_state::Core;
use crate::types::{
    world_input_hash, ImportCloseCause, InstanceRef, JournalStorePort, NodeKind, NodeRecord,
    NodeRecordStatus, RunEvent, WorkflowError, WorkflowErrorCode, WorldReadInput,
    WORLD_READ_INPUT_MAX_BYTES,
};

/// Admission outcome: either already completed synchronously (replay / import hit), or carrying the data live execution needs.
#[allow(clippy::large_enum_variant)]
pub(crate) enum WorldAdmission {
    Immediate(Result<Value, WorkflowError>),
    Live {
        instance: InstanceRef,
        kind: NodeKind,
        hash: String,
        input: WorldReadInput,
    },
}

impl Core {
    /// Admission of a world node (synchronous, inside the lock).
    pub(crate) fn world_read_admit(
        &mut self,
        site_id: &str,
        op: &str,
        args: &[Value],
    ) -> WorldAdmission {
        if self.run_settled {
            return WorldAdmission::Immediate(Err(self.run_error()));
        }
        let ordinal = self.next_ordinal(site_id);
        let instance = InstanceRef {
            site_id: site_id.to_string(),
            ordinal,
        };
        let hash = world_input_hash(op, args);
        let input = bound_world_read_input(op, args);

        if let Some(recorded) = self.journal().get_node(&self.run_id, site_id, ordinal) {
            if recorded.input_hash != hash {
                let err = crate::scheduler::hash_mismatch(&instance, &recorded.input_hash, &hash);
                self.fail_run(err.clone());
                return WorldAdmission::Immediate(Err(err));
            }
            if recorded.status == NodeRecordStatus::Completed {
                self.record(RunEvent::NodeSettled {
                    instance,
                    outcome: crate::types::NodeOutcome::Ok,
                    cached: Some(true),
                    error: None,
                    phase_name: None,
                });
                return WorldAdmission::Immediate(Ok(recorded.result.unwrap_or(Value::Null)));
            }
            if recorded.status == NodeRecordStatus::Failed {
                let json = recorded.error.clone();
                self.record(RunEvent::NodeSettled {
                    instance,
                    outcome: crate::types::NodeOutcome::Failed,
                    cached: Some(true),
                    error: json.clone(),
                    phase_name: None,
                });
                let err = json
                    .map(|j| WorkflowError::from_json(&j))
                    .unwrap_or_else(|| {
                        WorkflowError::new(
                            WorkflowErrorCode::DriverError,
                            "Recorded failure is missing its error",
                        )
                    });
                return WorldAdmission::Immediate(Err(err));
            }
            // status === Running: crashed mid-execution, fall through to re-execute live.
        }

        // op "run" is an effect, not a read: the journal keeps world-run as its own kind (an honest audit surface).
        let kind = if op == "run" {
            NodeKind::WorldRun
        } else {
            NodeKind::WorldRead
        };

        // amend-resume: journal replay always wins; once the cache is closed the table is no longer queried.
        if !self.import_closed {
            if let Some(imported) = self.imported_world.take(&hash) {
                self.journal().put_node(NodeRecord {
                    run_id: self.run_id.clone(),
                    site_id: site_id.to_string(),
                    ordinal,
                    kind,
                    actor_site_id: None,
                    actor_ordinal: None,
                    actor_seq: None,
                    input_hash: hash.clone(),
                    status: NodeRecordStatus::Completed,
                    result: Some(imported.result.clone()),
                    error: None,
                    stats: None,
                    artifact_id: None,
                    input: Some(input.clone()),
                    message_boundary: None,
                });
                self.record(RunEvent::NodeSettled {
                    instance,
                    outcome: crate::types::NodeOutcome::Ok,
                    cached: Some(true),
                    error: None,
                    phase_name: None,
                });
                return WorldAdmission::Immediate(Ok(imported.result));
            }
        }

        if self.run_settled {
            return WorldAdmission::Immediate(Err(self.run_error()));
        }
        // A live world.run is a write: close the import cache **before** dispatching (the same rule as the subagent mutating tools).
        if kind == NodeKind::WorldRun {
            self.close_import_cache(&instance, ImportCloseCause::WorldRun, None);
        }
        // Admission writes running immediately: a world read that crashed mid-execution can be re-executed on resume from this.
        self.journal().put_node(NodeRecord {
            run_id: self.run_id.clone(),
            site_id: site_id.to_string(),
            ordinal,
            kind,
            actor_site_id: None,
            actor_ordinal: None,
            actor_seq: None,
            input_hash: hash.clone(),
            status: NodeRecordStatus::Running,
            result: None,
            error: None,
            stats: None,
            artifact_id: None,
            input: Some(input.clone()),
            message_boundary: None,
        });
        self.record(RunEvent::NodeQueued {
            instance: instance.clone(),
            kind,
            actor: None,
            actor_seq: None,
            phase_name: None,
            instructions_head: None,
        });
        self.record(RunEvent::NodeDispatched {
            instance: instance.clone(),
        });
        WorldAdmission::Live {
            instance,
            kind,
            hash,
            input,
        }
    }

    /// Settlement of a world node (synchronous, inside the lock). Returns the result the script sees.
    pub(crate) fn world_read_settle(
        &mut self,
        instance: &InstanceRef,
        kind: NodeKind,
        hash: &str,
        input: &WorldReadInput,
        outcome: Result<Value, WorkflowError>,
    ) -> Result<Value, WorkflowError> {
        if self.run_settled {
            return Err(self.run_error());
        }
        match outcome {
            Ok(value) => {
                self.journal().put_node(NodeRecord {
                    run_id: self.run_id.clone(),
                    site_id: instance.site_id.clone(),
                    ordinal: instance.ordinal,
                    kind,
                    actor_site_id: None,
                    actor_ordinal: None,
                    actor_seq: None,
                    input_hash: hash.to_string(),
                    status: NodeRecordStatus::Completed,
                    result: Some(value.clone()),
                    error: None,
                    stats: None,
                    artifact_id: None,
                    input: Some(input.clone()),
                    message_boundary: None,
                });
                self.record(RunEvent::NodeSettled {
                    instance: instance.clone(),
                    outcome: crate::types::NodeOutcome::Ok,
                    cached: None,
                    error: None,
                    phase_name: None,
                });
                Ok(value)
            }
            Err(error) => {
                let json = error.to_json();
                self.journal().put_node(NodeRecord {
                    run_id: self.run_id.clone(),
                    site_id: instance.site_id.clone(),
                    ordinal: instance.ordinal,
                    kind,
                    actor_site_id: None,
                    actor_ordinal: None,
                    actor_seq: None,
                    input_hash: hash.to_string(),
                    status: NodeRecordStatus::Failed,
                    result: None,
                    error: Some(json.clone()),
                    stats: None,
                    artifact_id: None,
                    input: Some(input.clone()),
                    message_boundary: None,
                });
                self.record(RunEvent::NodeSettled {
                    instance: instance.clone(),
                    outcome: crate::types::NodeOutcome::Failed,
                    cached: None,
                    error: Some(json),
                    phase_name: None,
                });
                Err(error)
            }
        }
    }

    /// Close the import cache (idempotent; only a revised run emits the event -- it is the single source of truth for resume to restore that "the gate is closed").
    pub fn close_import_cache(
        &mut self,
        instance: &InstanceRef,
        cause: ImportCloseCause,
        actor_name: Option<String>,
    ) {
        if self.import_closed {
            return;
        }
        self.close_import();
        if self.imported_cache.is_none() {
            return;
        }
        self.record(RunEvent::ImportCacheClosed {
            instance: instance.clone(),
            cause,
            actor_name: actor_name.filter(|name| !name.is_empty()),
        });
    }
}

/// On resume, restore the gate-closing decision and the set of "ask instances that ran live" (the source of truth is this run's own events).
pub(crate) fn recover_import_closure(
    journal: &dyn JournalStorePort,
    run_id: &str,
) -> (std::collections::HashSet<String>, bool) {
    let mut live = std::collections::HashSet::new();
    let mut closed = false;
    for stored in journal.list_events(run_id, crate::types::ListEventsOptions::default()) {
        match stored.event {
            RunEvent::ImportCacheClosed { .. } => closed = true,
            RunEvent::NodeQueued {
                instance,
                kind: NodeKind::Ask,
                ..
            } => {
                live.insert(instance.key());
            }
            _ => {}
        }
    }
    (live, closed)
}

/// Compress `{op, args}` down to within 4 KB (port of `world-read-input.ts`).
pub(crate) fn bound_world_read_input(op: &str, args: &[Value]) -> WorldReadInput {
    const TRUNCATED_ARG_MAX_CHARS: usize = 400;
    const MAX_ARGS_KEPT: usize = 8;
    let plain = WorldReadInput {
        op: op.to_string(),
        args: args.to_vec(),
        truncated: None,
    };
    if let Ok(text) = serde_json::to_string(&plain) {
        if text.len() <= WORLD_READ_INPUT_MAX_BYTES {
            return plain;
        }
    }
    let preview = |value: &Value| -> String {
        let text = match value {
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
        };
        if text.chars().count() > TRUNCATED_ARG_MAX_CHARS {
            format!(
                "{}…",
                text.chars()
                    .take(TRUNCATED_ARG_MAX_CHARS)
                    .collect::<String>()
            )
        } else {
            text
        }
    };
    WorldReadInput {
        op: op.to_string(),
        args: args
            .iter()
            .take(MAX_ARGS_KEPT)
            .map(preview)
            .map(Value::String)
            .collect(),
        truncated: Some(true),
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

    struct WorldDriver {
        journal: Arc<InMemoryJournalStore>,
        events: Mutex<Vec<RunEvent>>,
        calls: Mutex<Vec<(String, Vec<Value>)>>,
        result: Result<Value, WorkflowError>,
    }

    #[async_trait::async_trait]
    impl WorkflowDriver for WorldDriver {
        fn journal(&self) -> &dyn JournalStorePort {
            &*self.journal
        }
        fn emit(&self, event: &RunEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
        async fn create_actor_session(
            &self,
            _a: &ActorRef,
            _p: &PersonaSpec,
            _s: Option<ActorSessionSeed>,
        ) -> Result<SessionRef, WorkflowError> {
            unreachable!()
        }
        fn start_ask(&self, _s: &SessionRef, _i: &InstanceRef, _m: AskMessage) {}
        fn respond_to_submit(&self, _i: &InstanceRef, _v: SubmitVerdict) {}
        fn cancel_ask(&self, _i: &InstanceRef) {}
        async fn execute_world_read(
            &self,
            op: &str,
            args: Vec<Value>,
        ) -> Result<Value, WorkflowError> {
            self.calls.lock().unwrap().push((op.to_string(), args));
            self.result.clone()
        }
    }

    fn core(result: Result<Value, WorkflowError>) -> (Core, Arc<WorldDriver>) {
        core_with_journal(result, Arc::new(InMemoryJournalStore::new()))
    }

    fn core_with_journal(
        result: Result<Value, WorkflowError>,
        journal: Arc<InMemoryJournalStore>,
    ) -> (Core, Arc<WorldDriver>) {
        let driver = Arc::new(WorldDriver {
            journal: journal.clone(),
            events: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            result,
        });
        (
            test_core(
                "run-w",
                driver.clone(),
                Caps { max_concurrency: 1 },
                Arc::new(|_s: Option<&Value>, _v: &Value| Vec::new()),
            ),
            driver,
        )
    }

    #[tokio::test]
    async fn live_world_read_round_trips() {
        let (mut core, driver) = core(Ok(json!({"files": 3})));
        let admission = core.world_read_admit("world-read#1", "glob", &[json!("**/*.ts")]);
        let WorldAdmission::Live {
            instance,
            kind,
            hash,
            input,
        } = admission
        else {
            panic!("must be live")
        };
        let outcome = driver.result.clone();
        let value = core
            .world_read_settle(&instance, kind, &hash, &input, outcome)
            .unwrap();
        assert_eq!(value, json!({"files": 3}));
        let node = core.journal().get_node("run-w", "world-read#1", 1).unwrap();
        assert_eq!(node.status, NodeRecordStatus::Completed);
        assert_eq!(node.input.as_ref().unwrap().op, "glob");
    }

    #[test]
    fn completed_world_read_replays_without_driver() {
        let journal = Arc::new(InMemoryJournalStore::new());
        let (mut core, _driver) = core_with_journal(Ok(Value::Null), journal.clone());
        let admission = core.world_read_admit("world-read#1", "read", &[json!("a.txt")]);
        let WorldAdmission::Live {
            instance,
            kind,
            hash,
            input,
        } = admission
        else {
            panic!()
        };
        core.world_read_settle(&instance, kind, &hash, &input, Ok(json!("cached")))
            .unwrap();
        // A new engine instance on the same journal (replay): ordinal restarts at 1 and the hit short-circuits.
        let (mut replay, replay_driver) = core_with_journal(Ok(Value::Null), journal);
        let admission = replay.world_read_admit("world-read#1", "read", &[json!("a.txt")]);
        match admission {
            WorldAdmission::Immediate(Ok(value)) => assert_eq!(value, json!("cached")),
            _ => panic!("replay must short-circuit"),
        }
        assert!(replay_driver.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn world_run_closes_the_import_cache_only_when_revised() {
        let (mut core, _driver) = core(Ok(Value::Null));
        // Not a revised run: no cache is injected, and closing the gate emits no event.
        core.close_import_cache(
            &InstanceRef {
                site_id: "world-read#1".into(),
                ordinal: 1,
            },
            ImportCloseCause::WorldRun,
            None,
        );
        assert!(core.import_closed);
        assert!(core
            .journal()
            .list_events("run-w", ListEventsOptions::default())
            .is_empty());
    }

    #[test]
    fn bound_input_truncates_large_args() {
        let big = "x".repeat(WORLD_READ_INPUT_MAX_BYTES + 100);
        let input = bound_world_read_input("run", &[json!("node"), json!("-e"), json!(big)]);
        assert_eq!(input.truncated, Some(true));
        assert_eq!(input.args.len(), 3);
        let preview = input.args[2].as_str().unwrap();
        assert!(preview.len() < WORLD_READ_INPUT_MAX_BYTES);
        assert!(preview.ends_with('…'));
        // A small input keeps its original types.
        let small = bound_world_read_input("read", &[json!("a.txt")]);
        assert!(small.truncated.is_none());
    }
}
