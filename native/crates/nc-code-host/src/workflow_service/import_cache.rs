//! Building the amend-resume imported cache (port of
//! `apps/nc-code-cli/packages/bootstrap/src/app/dynamic-workflow-import.ts`).
//!
//! Three call sites share the builder here:
//!   1. the **pre-flight** of `amend` ([`preflight_amend`]): it decides `run_not_found` /
//!      `missing_boundaries` before the in-flight predecessor is stopped — when refused, the
//!      predecessor keeps running and no row is created;
//!   2. `amend` builds the cache after the predecessor has settled ([`build_imported_cache`]);
//!   3. `resume` rebuilds the same table when it sees `resumed_from` ([`rebuild_imported_cache_for_resume`]).
//!
//! Determinism is not a style preference but a correctness precondition: the revised run's journal
//! is self-contained only for "the execution prefix it reached", and unconsumed imports are
//! recovered by rebuilding — so the same predecessor journal must always yield the same table (no
//! timestamps, no randomness, and every traversal order comes from the journal's insertion order).
//!
//! Difference from TS, recorded honestly: TS checks source honesty by counting messages through
//! `ActorTranscriptStore.messages()`; here the host's same source of truth is used
//! (`Session::history_snapshot()` — the very length the driver counted when writing
//! `message_boundary`), so boundary values are mutually readable across the two hosts.

use std::collections::{BTreeMap, HashMap, HashSet};

use nc_code_workflow::types::{
    ActorRecord, ImportedActorCandidate, ImportedAskEntry, ImportedRunCache, ImportedWorldEntry,
    JournalStorePort, NodeKind, NodeRecord, NodeRecordStatus, PersonaSpec, RunRecord, RunStatus,
};

use crate::HostServer;

/// The three reasons an import build is refused. **Discriminants, not prose**: the caller picks
/// its next action from them (pick another run / wait for it to settle / abandon the revision and
/// start a fresh run), so all three must be distinguishable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportRefusal {
    /// The journal has no such predecessor run.
    RunNotFound,
    /// The predecessor is still in flight (non-terminal). Revising **any terminal** run is legal, including completed.
    NotAmendable,
    /// The predecessor has completed asks without a message boundary: a markerless predecessor is refused whole.
    MissingBoundaries,
}

impl ImportRefusal {
    /// TS's discriminant (the port layer uses it and never parses the wording).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RunNotFound => "run_not_found",
            Self::NotAmendable => "not_amendable",
            Self::MissingBoundaries => "missing_boundaries",
        }
    }
}

/// A successful build: the import table + the lineage pointer.
#[derive(Clone, Debug)]
pub(crate) struct ImportedCacheBuild {
    pub(crate) cache: ImportedRunCache,
    pub(crate) resumed_from: String,
}

/// The journal surface one build reads (read-only: the builder writes not a single byte).
fn terminal(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed | RunStatus::Errored | RunStatus::Stopped
    )
}

/// The **pre-flight** of amend: the predecessor exists ∧ every completed ask has a message boundary.
/// It **does not look at status** — both are properties of the predecessor's journal, and stopping
/// cannot change them, so they can be decided before the stop. Only a pre-flight that passes is
/// allowed to stop anything, so a refused amend never leaves behind a run that was stopped for nothing.
pub(crate) fn preflight_amend(
    journal: &dyn JournalStorePort,
    predecessor_run_id: &str,
) -> Result<RunRecord, ImportRefusal> {
    let Some(run) = journal.get_run(predecessor_run_id) else {
        return Err(ImportRefusal::RunNotFound);
    };
    if !completed_asks_have_boundaries(&journal.list_nodes(predecessor_run_id)) {
        return Err(ImportRefusal::MissingBoundaries);
    }
    Ok(run)
}

/// The predicate of gate 3, shared by the pre-flight and the build. A missing marker has only two
/// causes — a journal written before the marker column existed, and an accounting failure on the
/// driver side — and both mean "this run's boundary accounting cannot be trusted", so the rule is
/// strict over the whole table rather than only over the rows that would be imported. A completed
/// ask is required; an unfinished ask having no boundary is **normal** (it never enters the import prefix).
fn completed_asks_have_boundaries(nodes: &[NodeRecord]) -> bool {
    nodes.iter().all(|node| {
        node.kind != NodeKind::Ask
            || node.status != NodeRecordStatus::Completed
            || node.message_boundary.is_some()
    })
}

/// Read the predecessor journal and build the [`ImportedRunCache`]. **Purely deterministic**: the same
/// journal state always yields the same table.
///
/// The three gates run in order (gates before building: when a gate fails, not one row is read):
///   1. predecessor missing → `run_not_found`;
///   2. predecessor non-terminal → `not_amendable`;
///   3. the predecessor has a completed ask with no `message_boundary` → `missing_boundaries`
///      (refused wholesale, never row by row: a half-mis-accounted predecessor would produce a table
///      that looks complete but truncates at the wrong place).
pub(crate) fn build_imported_cache(
    host: &HostServer,
    journal: &dyn JournalStorePort,
    predecessor_run_id: &str,
) -> Result<ImportedCacheBuild, ImportRefusal> {
    build_imported_cache_with(journal, predecessor_run_id, &|session_id, boundary| {
        honors_boundary(host, session_id, boundary)
    })
}

/// The builder's body. The transcript-honesty check is injected as a parameter: production passes
/// the host's version, so the pure journal rules (gates, prefix, world queues, chain backtracking)
/// can be asserted directly without a host.
fn build_imported_cache_with(
    journal: &dyn JournalStorePort,
    predecessor_run_id: &str,
    honors_boundary: &dyn Fn(&str, u64) -> bool,
) -> Result<ImportedCacheBuild, ImportRefusal> {
    let Some(run) = journal.get_run(predecessor_run_id) else {
        return Err(ImportRefusal::RunNotFound);
    };
    // The amendable set is **any terminal** status; resume's gate (which accepts only stopped) is
    // deliberately not reused here.
    if !terminal(run.status) {
        return Err(ImportRefusal::NotAmendable);
    }
    let nodes = journal.list_nodes(predecessor_run_id);
    if !completed_asks_have_boundaries(&nodes) {
        return Err(ImportRefusal::MissingBoundaries);
    }

    let mut actors: BTreeMap<String, ImportedActorCandidate> = BTreeMap::new();
    for record in named_unique_actors(&journal.list_actors(predecessor_run_id)) {
        // named_unique_actors accepts only rows whose name is non-empty and unique within this run, so
        // this unwrap honors the filter rather than reading optimistically.
        let name = record.name.clone().unwrap_or_default();
        if let Some(candidate) = build_actor_candidate(
            journal,
            &record,
            &nodes,
            predecessor_run_id,
            honors_boundary,
        ) {
            actors.insert(name, candidate);
        }
    }

    Ok(ImportedCacheBuild {
        cache: ImportedRunCache {
            actors,
            world: build_world_queues(&nodes),
        },
        resumed_from: predecessor_run_id.to_string(),
    })
}

/// The resume-side entry point: rebuild the revised run's imported cache. **Any failure only
/// degrades; it never refuses the resume.**
///
/// It shares the very same [`build_imported_cache`] with submit/revise — not for the convenience of
/// reuse but as a correctness precondition: the revised run's journal is self-contained only for the
/// execution prefix it reached, and unconsumed imports are recovered by this rebuild; two different
/// tables on the two sides would turn "rebuild" into "build a different table".
///
/// Three reasons make "rebuild failed" different from "build failed" (each mirrors the file comment of
/// TS `rebuildImportedCacheForResume`):
///   - the revised run already exists; refusing a resume turns a runnable run into a permanently stuck one;
///   - already-consumed hits are **real rows** in this run's journal, and replay needs no table for them;
///   - unconsumed imports degrade to live re-execution: correct results, only spending tokens that could
///     have been saved.
///
/// So this path does not even distinguish the three gate reasons: for a resume, `run_not_found` (the
/// predecessor was cleaned up) and `missing_boundaries` are the same thing — "no cache available this
/// time". **The refusal gate for a markerless predecessor still exists**, inside the builder (both the
/// amend pre-flight and the build refuse wholesale on it); resume merely downgrades its result to "no
/// cache", exactly as TS does.
pub(crate) fn rebuild_imported_cache_for_resume(
    host: &HostServer,
    journal: &dyn JournalStorePort,
    run_id: &str,
    predecessor_run_id: &str,
) -> Option<ImportedRunCache> {
    match build_imported_cache(host, journal, predecessor_run_id) {
        Ok(built) => Some(built.cache),
        Err(reason) => {
            eprintln!(
                "[nc-code-host] workflow amend cache rebuild skipped for run {run_id} \
                 (predecessor {predecessor_run_id}): {}; resuming without it",
                reason.as_str()
            );
            None
        }
    }
}

/// The predecessor's actor rows that **can serve as candidates**: non-empty name, and that name unique within the run.
///
/// Anonymous ones are not taken (the name is the cache identity key, so with no name there is no coordinate to match against);
/// on a duplicate name **neither row is taken** — picking a candidate by name over a duplicated predecessor is a dice roll, and the cost of picking wrong is another actor's session prefix passed off as this actor's context.
fn named_unique_actors(records: &[ActorRecord]) -> Vec<ActorRecord> {
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for record in records {
        if let Some(name) = record.name.as_deref().filter(|name| !name.is_empty()) {
            *by_name.entry(name).or_default() += 1;
        }
    }
    let mut reported: HashSet<&str> = HashSet::new();
    let mut unique = Vec::new();
    for record in records {
        let Some(name) = record.name.as_deref().filter(|name| !name.is_empty()) else {
            continue;
        };
        let count = by_name.get(name).copied().unwrap_or(0);
        if count == 1 {
            unique.push(record.clone());
            continue;
        }
        // A duplicate name (count > 1) is reported once: two rows of the same name must not each report it.
        if count > 1 && reported.insert(name) {
            eprintln!(
                "[nc-code-host] workflow amend: duplicate actor name '{name}' in the \
                 predecessor ({count} rows), skipped"
            );
        }
    }
    unique
}

/// One candidate actor's importable prefix + its transcript source. A missing piece at any step returns `None` (that actor runs fresh).
fn build_actor_candidate(
    journal: &dyn JournalStorePort,
    actor: &ActorRecord,
    nodes: &[NodeRecord],
    predecessor_run_id: &str,
    honors_boundary: &dyn Fn(&str, u64) -> bool,
) -> Option<ImportedActorCandidate> {
    // The persona is the frozen identity the engine records synchronously in createActor, so it is always there normally;
    // its absence can only mean a row rewritten from outside. Runtime comparison cannot discuss persona consistency without something to compare against — drop the candidate rather than substitute `{}`.
    let persona: PersonaSpec = actor.persona.clone()?;

    let entries = completed_ask_prefix(nodes, actor);
    if entries.is_empty() {
        return None;
    }

    let Some((source_session_id, resolved_model)) =
        resolve_transcript_source(journal, actor, predecessor_run_id)
    else {
        // No ancestor on the chain holds a session for this actor (never created, or the session was cleaned up). A full-
        // fidelity transcript is this feature's ruling: with no transcript there is no context to continue into — degrade to a fresh actor.
        eprintln!(
            "[nc-code-host] workflow amend: no transcript source for actor {} in {predecessor_run_id}, import dropped",
            actor.name.as_deref().unwrap_or_default()
        );
        return None;
    };

    let boundary = entries.last()?.message_boundary;
    if !honors_boundary(&source_session_id, boundary) {
        eprintln!(
            "[nc-code-host] workflow amend: transcript source {source_session_id} is shorter than \
             boundary {boundary}, import dropped"
        );
        return None;
    }

    Some(ImportedActorCandidate {
        persona,
        entries,
        transcript_source_session_id: source_session_id,
        resolved_model,
    })
}

/// The actor's **longest all-completed ask prefix** (contiguous over actorSeq 0..k).
///
/// The prefix stops dead at the first non-completed entry, and the three ways of stopping get the same treatment: failed,
/// crashed in flight (running), or a hole in the sequence numbers. A failed ask does not bind the new run (revising is often precisely the way past a failure), so it is not imported itself; but skipping it to import the entries after it would smuggle context —
/// the skipped round's question and answer are still in the source session's transcript while the cache would claim they never happened.
fn completed_ask_prefix(nodes: &[NodeRecord], actor: &ActorRecord) -> Vec<ImportedAskEntry> {
    let mut by_seq: HashMap<u64, &NodeRecord> = HashMap::new();
    for node in nodes {
        if node.kind != NodeKind::Ask {
            continue;
        }
        if node.actor_site_id.as_deref() != Some(actor.site_id.as_str())
            || node.actor_ordinal != Some(actor.ordinal)
        {
            continue;
        }
        let Some(seq) = node.actor_seq else { continue };
        by_seq.insert(seq, node);
    }

    let mut entries = Vec::new();
    let mut seq = 0u64;
    while let Some(node) = by_seq.get(&seq) {
        if node.status != NodeRecordStatus::Completed {
            break;
        }
        // The boundary must be present: gate 3 already vetted the whole predecessor, so this honors an invariant rather than reading optimistically.
        let Some(message_boundary) = node.message_boundary else {
            break;
        };
        entries.push(ImportedAskEntry {
            input_hash: node.input_hash.clone(),
            result: node.result.clone().unwrap_or(serde_json::Value::Null),
            stats: node.stats.clone(),
            message_boundary,
        });
        seq += 1;
    }
    entries
}

/// Walk the `resumed_from` chain back to the **nearest** ancestor run that holds a session for the actor of this name.
///
/// Why the chain must be walked: if an actor in run B hits the cache throughout, B never created a session for it (creation is
/// lazy), so when the B→C revision wants to continue that actor, the transcript exists only in A. A count boundary does not
/// change across a prefix copy, so at any chain ancestor holding a session the boundary values B copied are directly usable — that is the foundation on which chained revisions stand.
///
/// `resolved_model` and the session are taken from **the same row**: a pin means "do not change the model while continuing this
/// transcript", and taking it from another row would make a promise about a transcript that is not its own. The cycle guard
/// (`seen`) is purely defensive: supersede can only point at an earlier settled run, so no cycle can be built, but on corrupted data this while loop would never end.
fn resolve_transcript_source(
    journal: &dyn JournalStorePort,
    actor: &ActorRecord,
    start_run_id: &str,
) -> Option<(String, Option<String>)> {
    let name = actor.name.as_deref()?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut run_id = Some(start_run_id.to_string());
    while let Some(current) = run_id {
        if !seen.insert(current.clone()) {
            break;
        }
        let matches: Vec<ActorRecord> = journal
            .list_actors(&current)
            .into_iter()
            .filter(|candidate| candidate.name.as_deref() == Some(name))
            .collect();
        // 0 = this generation has no such actor at all (the chain breaks for that name); >1 = duplicate name, and taking a
        // session by name is a dice roll. Both stop here instead of going further up: an older generation's session is not the source of **this** transcript.
        if matches.len() != 1 {
            return None;
        }
        let record = &matches[0];
        if let Some(session_id) = record.session_id.clone() {
            return Some((session_id, record.resolved_model.clone()));
        }
        run_id = journal.get_run(&current)?.resumed_from;
    }
    None
}

/// Whether the source session can really honor the boundary (message count >= boundary).
///
/// This check joins the driver's **fail-loudly** semantics with the build's **degrade** semantics: a foreseeable missing piece
/// (session cleaned up or truncated) should drop the candidate at build time, so the driver side becomes a backstop only for what genuinely should not happen.
/// A read failure counts as "cannot honor" as well — a source that cannot be read is not a usable source.
fn honors_boundary(host: &HostServer, session_id: &str, boundary: u64) -> bool {
    let Some(session) = host.load_session(session_id) else {
        return false;
    };
    match session.history_snapshot() {
        Ok(history) => history.len() as u64 >= boundary,
        Err(_) => false,
    }
}

/// The content table of world nodes: `input_hash` → a queue of records in journal insertion order (the n-th occurrence matches the n-th entry).
///
/// The key is **the predecessor record's inputHash**, never recomputed: the engine's hashing of `{op,args}` is exactly the value
/// written into that column, and recomputing here would copy the hash contract to a second place — once the two drift, the surface symptom is "the cache misses for no reason".
/// Only completed rows are taken: a failed world read is re-executed (a failure does not bind the new run).
/// world-run shares the table with world-read — importing a world-run is a **safety property**, not an optimization: an amended
/// re-run must never silently replay an effect that is already journaled (a deployment script running twice).
fn build_world_queues(nodes: &[NodeRecord]) -> BTreeMap<String, Vec<ImportedWorldEntry>> {
    let mut world: BTreeMap<String, Vec<ImportedWorldEntry>> = BTreeMap::new();
    for node in nodes {
        if !matches!(node.kind, NodeKind::WorldRead | NodeKind::WorldRun) {
            continue;
        }
        if node.status != NodeRecordStatus::Completed {
            continue;
        }
        world
            .entry(node.input_hash.clone())
            .or_default()
            .push(ImportedWorldEntry {
                input_hash: node.input_hash.clone(),
                kind: node.kind,
                result: node.result.clone().unwrap_or(serde_json::Value::Null),
            });
    }
    world
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_workflow::journal_memory::InMemoryJournalStore;

    /// These unit tests cover only the **pure journal read** rules (boundary gate, prefix, world queues, chain backtracking);
    /// the segment with the transcript-honesty check needs a real host — see the e2e tests in `workflow_service::tests`.
    fn seed_predecessor(
        journal: &InMemoryJournalStore,
        run_id: &str,
        status: RunStatus,
        resumed_from: Option<&str>,
    ) {
        journal.create_run(RunRecord {
            run_id: run_id.to_string(),
            parent_session_id: Some("session-1".to_string()),
            cwd: Some("/work".to_string()),
            name: None,
            tool_call_id: None,
            script_text: Some("return 1;".to_string()),
            script_hash: Some("hash".to_string()),
            args: None,
            resumed_from: resumed_from.map(str::to_string),
            caps: nc_code_workflow::types::Caps { max_concurrency: 1 },
            spent_tokens: 0,
            status,
            stop_reason: None,
            superseded_by: None,
            failure: None,
            result: None,
        });
    }

    fn node(
        run_id: &str,
        site_id: &str,
        ordinal: u32,
        kind: NodeKind,
        actor: Option<(&str, u32, u64)>,
        status: NodeRecordStatus,
    ) -> NodeRecord {
        NodeRecord {
            run_id: run_id.to_string(),
            site_id: site_id.to_string(),
            ordinal,
            kind,
            actor_site_id: actor.map(|(site, _, _)| site.to_string()),
            actor_ordinal: actor.map(|(_, ordinal, _)| ordinal),
            actor_seq: actor.map(|(_, _, seq)| seq),
            // A world node's hash depends only on content (several occurrences of the same {op,args} share one key), while ask
            // nodes are distinguished by site + ordinal (each ask carries different instructions).
            input_hash: match actor {
                Some(_) => format!("{site_id}@{ordinal}-input"),
                None => format!("{site_id}-input"),
            },
            status,
            result: Some(serde_json::json!({"site": site_id})),
            error: None,
            stats: None,
            artifact_id: None,
            input: None,
            message_boundary: actor.map(|(_, _, seq)| seq + 10),
        }
    }

    /// The pure journal rules (gates, prefix, world queues, chain backtracking) never touch a host: the honesty check is
    /// injected here as an always-true stub, and its own semantics are covered by the e2e in `workflow_service::tests` against real sessions.
    fn build_pure(
        journal: &InMemoryJournalStore,
        predecessor: &str,
    ) -> Result<ImportedCacheBuild, ImportRefusal> {
        build_imported_cache_with(journal, predecessor, &|_, _| true)
    }

    #[test]
    fn the_boundary_gate_rejects_the_whole_predecessor() {
        let journal = InMemoryJournalStore::new();
        seed_predecessor(&journal, "run-a", RunStatus::Completed, None);
        let mut ask = node(
            "run-a",
            "ask#1",
            1,
            NodeKind::Ask,
            Some(("actor#1", 1, 0)),
            NodeRecordStatus::Completed,
        );
        ask.message_boundary = None;
        journal.put_node(ask);

        assert_eq!(
            preflight_amend(&journal, "run-a").unwrap_err(),
            ImportRefusal::MissingBoundaries
        );
        // A markerless predecessor is refused wholesale: not one row is imported, and a boundary is never synthesized.
        assert_eq!(
            build_pure(&journal, "run-a").unwrap_err(),
            ImportRefusal::MissingBoundaries
        );
        assert_eq!(
            preflight_amend(&journal, "absent").unwrap_err(),
            ImportRefusal::RunNotFound
        );
        assert_eq!(
            build_pure(&journal, "absent").unwrap_err(),
            ImportRefusal::RunNotFound
        );
    }

    #[test]
    fn an_in_flight_predecessor_is_not_amendable_but_passes_preflight() {
        let journal = InMemoryJournalStore::new();
        seed_predecessor(&journal, "run-live", RunStatus::Running, None);
        // The pre-flight does not look at status: a completed ask of an in-flight predecessor was stored with its boundary already.
        assert!(preflight_amend(&journal, "run-live").is_ok());
        assert_eq!(
            build_pure(&journal, "run-live").unwrap_err(),
            ImportRefusal::NotAmendable
        );
    }

    #[test]
    fn completed_prefix_stops_at_the_first_failure_and_world_queues_keep_order() {
        let journal = InMemoryJournalStore::new();
        seed_predecessor(&journal, "run-p", RunStatus::Errored, None);
        journal.put_actor(ActorRecord {
            run_id: "run-p".to_string(),
            site_id: "actor#1".to_string(),
            ordinal: 1,
            name: Some("planner".to_string()),
            persona: Some(PersonaSpec {
                name: Some("planner".to_string()),
                system: Some("you plan".to_string()),
            }),
            session_id: Some("session-actor".to_string()),
            resolved_model: Some("vendor/model".to_string()),
        });
        journal.put_node(node(
            "run-p",
            "ask#1",
            1,
            NodeKind::Ask,
            Some(("actor#1", 1, 0)),
            NodeRecordStatus::Completed,
        ));
        journal.put_node(node(
            "run-p",
            "ask#2",
            1,
            NodeKind::Ask,
            Some(("actor#1", 1, 1)),
            NodeRecordStatus::Failed,
        ));
        // Entries after a failure must never enter the prefix (context smuggling).
        journal.put_node(node(
            "run-p",
            "ask#3",
            1,
            NodeKind::Ask,
            Some(("actor#1", 1, 2)),
            NodeRecordStatus::Completed,
        ));
        journal.put_node(node(
            "run-p",
            "world-read#1",
            1,
            NodeKind::WorldRead,
            None,
            NodeRecordStatus::Completed,
        ));
        journal.put_node(node(
            "run-p",
            "world-read#1",
            2,
            NodeKind::WorldRead,
            None,
            NodeRecordStatus::Completed,
        ));
        journal.put_node(node(
            "run-p",
            "world-read#1",
            3,
            NodeKind::WorldRead,
            None,
            NodeRecordStatus::Failed,
        ));

        let nodes = journal.list_nodes("run-p");
        let records = journal.list_actors("run-p");
        let entries = completed_ask_prefix(&nodes, &records[0]);
        assert_eq!(entries.len(), 1, "prefix dies at the failed ask");
        assert_eq!(entries[0].message_boundary, 10);

        let world = build_world_queues(&nodes);
        let queue = world.get("world-read#1-input").expect("hash-keyed queue");
        assert_eq!(queue.len(), 2, "failed world rows are not imported");
        assert_eq!(queue[0].kind, NodeKind::WorldRead);

        let (source, model) =
            resolve_transcript_source(&journal, &records[0], "run-p").expect("source");
        assert_eq!(source, "session-actor");
        assert_eq!(model.as_deref(), Some("vendor/model"));

        // The whole build: one actor candidate (one prefix entry) + one world table.
        let built = build_pure(&journal, "run-p").expect("amendable errored predecessor");
        assert_eq!(built.resumed_from, "run-p");
        let candidate = built
            .cache
            .actors
            .get("planner")
            .expect("named actor candidate");
        assert_eq!(candidate.entries.len(), 1);
        assert_eq!(candidate.transcript_source_session_id, "session-actor");
        assert_eq!(built.cache.world.len(), 1);
    }

    #[test]
    fn transcript_source_walks_the_resumed_from_chain() {
        let journal = InMemoryJournalStore::new();
        // A holds the actor's session; B hit the cache throughout and so never created one; C is a revision of B.
        seed_predecessor(&journal, "run-a", RunStatus::Completed, None);
        journal.put_actor(ActorRecord {
            run_id: "run-a".to_string(),
            site_id: "actor#1".to_string(),
            ordinal: 1,
            name: Some("critic".to_string()),
            persona: None,
            session_id: Some("session-a".to_string()),
            resolved_model: None,
        });
        seed_predecessor(&journal, "run-b", RunStatus::Completed, Some("run-a"));
        journal.put_actor(ActorRecord {
            run_id: "run-b".to_string(),
            site_id: "actor#1".to_string(),
            ordinal: 1,
            name: Some("critic".to_string()),
            persona: None,
            session_id: None,
            resolved_model: None,
        });
        let records = journal.list_actors("run-b");
        let (source, _) = resolve_transcript_source(&journal, &records[0], "run-b").expect("chain");
        assert_eq!(source, "session-a");

        // A broken name on the chain (that generation holds two rows of it) stops here instead of walking further up.
        journal.put_actor(ActorRecord {
            run_id: "run-b".to_string(),
            site_id: "actor#2".to_string(),
            ordinal: 1,
            name: Some("critic".to_string()),
            persona: None,
            session_id: None,
            resolved_model: None,
        });
        let duplicated = journal
            .list_actors("run-b")
            .into_iter()
            .find(|record| record.site_id == "actor#1")
            .expect("actor#1 row");
        assert!(resolve_transcript_source(&journal, &duplicated, "run-b").is_none());
    }
}
