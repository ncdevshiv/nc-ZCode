//! Consumption state for the amend-resume import cache (port of `imported-cache.ts`).
//!
//! The injected [`ImportedRunCache`] is read-only data built by the caller; "how far consumption has gone" is the engine's own state.

use std::collections::{BTreeMap, HashMap};

use crate::types::{
    canonical_json, ActorRef, ActorSessionSeed, ImportedActorCandidate, ImportedAskEntry,
    ImportedRunCache, ImportedWorldEntry, InstanceRef, NodeKind, NodeRecord, NodeRecordStatus,
    PersonaSpec,
};

/// Import consumption state for an actor whose candidate has attached: consume cursor + monotonic divergence flag.
#[derive(Debug)]
pub(crate) struct ImportedActorState {
    candidate: ImportedActorCandidate,
    consumed: usize,
    diverged: bool,
}

impl ImportedActorState {
    pub fn new(candidate: ImportedActorCandidate) -> Self {
        Self {
            candidate,
            consumed: 0,
            diverged: false,
        }
    }

    /// A fresh ask queries the cache at seq: on a hit return the entry (and advance the cursor), otherwise set divergence and return None.
    pub fn take(&mut self, seq: u64, hash: &str) -> Option<ImportedAskEntry> {
        if self.diverged {
            return None;
        }
        let entry = self.candidate.entries.get(seq as usize);
        match entry {
            Some(entry) if entry.input_hash == hash => {
                self.consumed = seq as usize + 1;
                Some(entry.clone())
            }
            _ => {
                self.diverged = true;
                None
            }
        }
    }

    /// The query used after the cache is closed: only pure entries (worldToolCalls === 0) can still hit.
    pub fn take_if_pure(&mut self, seq: u64, hash: &str) -> Option<ImportedAskEntry> {
        if self.diverged {
            return None;
        }
        let entry = self.candidate.entries.get(seq as usize);
        match entry {
            Some(entry)
                if entry.input_hash == hash
                    && entry.stats.as_ref().and_then(|s| s.world_tool_calls) == Some(0) =>
            {
                self.consumed = seq as usize + 1;
                Some(entry.clone())
            }
            _ => {
                self.diverged = true;
                None
            }
        }
    }

    /// Re-derive the divergence state from a recorded ask row (used when resuming after a revised run crashed).
    pub fn reconcile_recorded(&mut self, seq: u64, recorded_hash: &str, was_live: bool) {
        if self.diverged {
            return;
        }
        if was_live {
            self.diverged = true;
            return;
        }
        match self.candidate.entries.get(seq as usize) {
            Some(entry) if entry.input_hash == recorded_hash => {
                self.consumed = self.consumed.max(seq as usize + 1);
            }
            _ => self.diverged = true,
        }
    }

    /// Session seed for a diverged actor: source session + how many messages to copy + the inherited model pin.
    pub fn seed(&self) -> Option<ActorSessionSeed> {
        if self.consumed == 0 {
            return None;
        }
        let last = self.candidate.entries.get(self.consumed - 1)?;
        Some(ActorSessionSeed {
            source_session_id: self.candidate.transcript_source_session_id.clone(),
            message_count: last.message_boundary,
            resolved_model: self.candidate.resolved_model.clone(),
        })
    }
}

/// Import queue consumption state for world nodes: one cursor per content hash (the n-th occurrence matches the n-th record).
#[derive(Debug, Default)]
pub(crate) struct ImportedWorldQueue {
    world: BTreeMap<String, Vec<ImportedWorldEntry>>,
    cursors: HashMap<String, usize>,
}

impl ImportedWorldQueue {
    pub fn new(world: BTreeMap<String, Vec<ImportedWorldEntry>>) -> Self {
        Self {
            world,
            cursors: HashMap::new(),
        }
    }

    /// Take the next record for this content hash; None when exhausted or never recorded (the caller goes live).
    pub fn take(&mut self, hash: &str) -> Option<ImportedWorldEntry> {
        let queue = self.world.get(hash)?;
        let cursor = *self.cursors.get(hash).unwrap_or(&0);
        let entry = queue.get(cursor)?;
        self.cursors.insert(hash.to_string(), cursor + 1);
        Some(entry.clone())
    }
}

/// Find an import candidate for a freshly created actor: look the table up by effective name, and accept only when the canonicalized persona matches.
pub(crate) fn match_imported_actor(
    cache: Option<&ImportedRunCache>,
    spec: &PersonaSpec,
) -> Option<ImportedActorState> {
    let cache = cache?;
    let name = spec.name.as_deref()?;
    if name.is_empty() {
        return None;
    }
    let candidate = cache.actors.get(name)?;
    if canonical_json(&serde_json::to_value(spec).ok()?)
        != canonical_json(&serde_json::to_value(&candidate.persona).ok()?)
    {
        return None;
    }
    Some(ImportedActorState::new(candidate.clone()))
}

/// The real dwf_node row that an ask cache hit must write (new siteId, copying result / stats / boundary).
pub(crate) fn imported_ask_record(
    run_id: &str,
    instance: &InstanceRef,
    actor: &ActorRef,
    seq: u64,
    hash: &str,
    entry: &ImportedAskEntry,
) -> NodeRecord {
    NodeRecord {
        run_id: run_id.to_string(),
        site_id: instance.site_id.clone(),
        ordinal: instance.ordinal,
        kind: NodeKind::Ask,
        actor_site_id: Some(actor.site_id.clone()),
        actor_ordinal: Some(actor.ordinal),
        actor_seq: Some(seq),
        input_hash: hash.to_string(),
        status: NodeRecordStatus::Completed,
        result: Some(entry.result.clone()),
        error: None,
        stats: entry.stats.clone(),
        artifact_id: None,
        input: None,
        message_boundary: Some(entry.message_boundary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AskStats, NodeKind};
    use serde_json::json;

    fn candidate(entries: Vec<ImportedAskEntry>) -> ImportedActorCandidate {
        ImportedActorCandidate {
            persona: PersonaSpec {
                name: Some("planner".into()),
                system: None,
            },
            entries,
            transcript_source_session_id: "src-session".into(),
            resolved_model: Some("p/m".into()),
        }
    }

    fn entry(hash: &str, result: serde_json::Value, boundary: u64) -> ImportedAskEntry {
        ImportedAskEntry {
            input_hash: hash.to_string(),
            result,
            stats: None,
            message_boundary: boundary,
        }
    }

    #[test]
    fn take_hits_then_diverges_forever() {
        let mut state = ImportedActorState::new(candidate(vec![
            entry("h1", json!(1), 10),
            entry("h2", json!(2), 20),
        ]));
        assert!(state.take(0, "h1").is_some());
        assert_eq!(state.seed().unwrap().message_count, 10);
        // Hash mismatch at seq 1 diverges; later asks never hit again.
        assert!(state.take(1, "wrong").is_none());
        assert!(state.take(1, "h2").is_none());
        assert!(state.take(2, "h2").is_none());
    }

    #[test]
    fn take_if_pure_only_accepts_pure_entries() {
        let mut impure = entry("h1", json!(1), 10);
        impure.stats = Some(AskStats {
            tokens: 0,
            tool_calls: 1,
            turns: 1,
            world_tool_calls: Some(1),
        });
        let mut state = ImportedActorState::new(candidate(vec![impure]));
        assert!(state.take_if_pure(0, "h1").is_none());

        let mut pure = entry("h1", json!(1), 10);
        pure.stats = Some(AskStats {
            tokens: 0,
            tool_calls: 0,
            turns: 1,
            world_tool_calls: Some(0),
        });
        let mut state = ImportedActorState::new(candidate(vec![pure]));
        assert!(state.take_if_pure(0, "h1").is_some());

        // Missing stats counts as impure (conservative).
        let mut state = ImportedActorState::new(candidate(vec![entry("h1", json!(1), 10)]));
        assert!(state.take_if_pure(0, "h1").is_none());
    }

    #[test]
    fn reconcile_recorded_rebuilds_divergence() {
        let mut state = ImportedActorState::new(candidate(vec![
            entry("h1", json!(1), 10),
            entry("h2", json!(2), 20),
        ]));
        state.reconcile_recorded(0, "h1", false);
        assert_eq!(state.seed().unwrap().message_count, 10);
        // A live ask (cache closed path) diverges even when the hash matches.
        state.reconcile_recorded(1, "h2", true);
        assert!(state.take(1, "h2").is_none());
    }

    #[test]
    fn world_queue_consumes_in_order() {
        let mut world = BTreeMap::new();
        world.insert(
            "h".to_string(),
            vec![
                ImportedWorldEntry {
                    input_hash: "h".into(),
                    kind: NodeKind::WorldRead,
                    result: json!("a"),
                },
                ImportedWorldEntry {
                    input_hash: "h".into(),
                    kind: NodeKind::WorldRead,
                    result: json!("b"),
                },
            ],
        );
        let mut queue = ImportedWorldQueue::new(world);
        assert_eq!(queue.take("h").unwrap().result, json!("a"));
        assert_eq!(queue.take("h").unwrap().result, json!("b"));
        assert!(queue.take("h").is_none());
        assert!(queue.take("missing").is_none());
    }

    #[test]
    fn match_requires_name_and_equal_persona() {
        let mut cache = ImportedRunCache::default();
        cache.actors.insert(
            "planner".to_string(),
            candidate(vec![entry("h1", json!(1), 10)]),
        );
        let hit = match_imported_actor(
            Some(&cache),
            &PersonaSpec {
                name: Some("planner".into()),
                system: Some("x".into()),
            },
        );
        assert!(hit.is_none(), "persona mismatch must drop the candidate");
        let hit = match_imported_actor(
            Some(&cache),
            &PersonaSpec {
                name: Some("planner".into()),
                system: None,
            },
        );
        assert!(hit.is_some());
        let anon = match_imported_actor(Some(&cache), &PersonaSpec::default());
        assert!(anon.is_none(), "anonymous actors never attach");
    }
}
