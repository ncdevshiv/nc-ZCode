//! The read model of the run-observation surface (port of `dynamic-workflow-run-observation.ts`,
//! `dynamic-workflow-run-introspection.ts`, `dynamic-workflow-run-artifact-projection.ts` and
//! the purely synthetic parts of `dynamic-workflow-run-label.ts`).
//!
//! Discipline (item by item the same as TS):
//! - **status truth has exactly one synthesizer** ([`synthesize_run_status`]): in-memory terminal state >
//!   live engine (a terminal journal row may be another instance's orphan convergence, so it cannot end local tracking) > journal > "running but no row yet".
//!   All four read surfaces (snapshot / list / detail / settlement) share it.
//! - The snapshot vocabulary has no `pending`: that step folds into `running` ([`fold_snapshot_status`]),
//!   and the real terminal word is exposed separately through `runStatus`.
//! - Artifact merging **collects completed rows only**, aggregates versions by id, lifts the newest fields to the top level; `primary` stays attached to the id.
//! - Labels derive at read time (`name` → the script's first non-empty line → runId) and are **never written back** to `dwf_run.name`.
//!
//! The journal's enumeration / count / artifact-row queries are not on the engine port (the engine never
//! enumerates), so this module carries its own read-only SQLite connection — same place and same reason as TS's `dwf-journal-introspection.ts`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use nc_code_workflow::types::{
    ArtifactOp, ArtifactVersionRecord, NodeKind, NodeRecord, NodeRecordStatus, RunRecord,
    RunSettlement, RunStatus, RunStopReason, WorkflowErrorJson, WorldReadInput,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::workflow::actor_tools::PendingQuestion;

/// Default/max page sizes for lists: the enumeration surface is bounded and never scans the whole database (same values as TS's two constants).
pub const DEFAULT_LIST_RUNS_LIMIT: usize = 16;
pub const MAX_LIST_RUNS_LIMIT: usize = 64;

/// Default/max page sizes for event pagination (bounded on the v4 gateway side as well).
pub const DEFAULT_LIST_EVENTS_LIMIT: usize = 200;
pub const MAX_LIST_EVENTS_LIMIT: usize = 1000;

// ————————————————————————————————————————————————————————————————
// Read-only introspection connection
// ————————————————————————————————————————————————————————————————

/// The journal's read-only introspection surface (a separate connection: enumeration queries and the engine's write transactions never queue against each other; under WAL reads do not block writes).
pub(crate) struct JournalIntrospection {
    conn: Mutex<Connection>,
}

impl JournalIntrospection {
    /// Opens a **second** connection to the same journal file. DDL belongs to `SqliteJournal::open` (the
    /// host's construction order guarantees it runs first); this connection only reads.
    pub(crate) fn open(path: &Path) -> rusqlite::Result<Self> {
        Ok(Self {
            conn: Mutex::new(Connection::open(path)?),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Enumerate run rows by parent session (`None` = all), most recently updated first.
    ///
    /// `limit` comes from the caller (TS's "fetch one extra to detect truncation" happens here too). Physical
    /// status is an index-friendly pre-filter; the **authority for the terminal decision** remains the decoded
    pub(crate) fn run_rows(
        &self,
        session_id: Option<&str>,
        limit: usize,
    ) -> rusqlite::Result<Vec<RunListRow>> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "select id, time_created, time_updated from dwf_run
             where (?1 is null or parent_session_id = ?1)
             order by time_updated desc
             limit ?2",
        )?;
        let rows = statement.query_map(rusqlite::params![session_id, limit as i64], |row| {
            Ok(RunListRow {
                run_id: row.get::<_, String>(0)?,
                time_created: row.get::<_, i64>(1)? as f64,
                time_updated: row.get::<_, i64>(2)? as f64,
            })
        })?;
        rows.collect()
    }

    /// Non-terminal rows (the input to orphan reconciliation). Physical `pending`/`running` map
    /// one-to-one onto logical non-terminal: `errored`/`stopped` decoded from `failed`/`cancelled` are
    /// terminal, and the historical `failed + Interrupted` is `stopped` — terminal too. Node timestamps of a
    /// single run (`dwf_node.time_created/time_updated`); the v4 workspace listing's `createdAt/updatedAt` are
    /// protocol-required. Time lives only on the SQL columns, never inside an engine record (no clock there).
    pub(crate) fn node_times(&self, run_id: &str) -> rusqlite::Result<Vec<NodeTimeRow>> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "select site_id, ordinal, time_created, time_updated from dwf_node
             where run_id = ?1
             order by id",
        )?;
        let rows = statement.query_map([run_id], |row| {
            Ok(NodeTimeRow {
                site_id: row.get::<_, String>(0)?,
                ordinal: row.get::<_, i64>(1)? as u32,
                time_created: row.get::<_, i64>(2)? as f64,
                time_updated: row.get::<_, i64>(3)? as f64,
            })
        })?;
        rows.collect()
    }

    pub(crate) fn non_terminal_run_ids(
        &self,
        session_id: Option<&str>,
    ) -> rusqlite::Result<Vec<String>> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "select id from dwf_run
             where status in ('pending', 'running')
               and (?1 is null or parent_session_id = ?1)
             order by time_updated desc",
        )?;
        let rows =
            statement.query_map(rusqlite::params![session_id], |row| row.get::<_, String>(0))?;
        rows.collect()
    }
}

// ————————————————————————————————————————————————————————————————
// List rows
// ————————————————————————————————————————————————————————————————

/// One row of an enumeration query (timestamps are read straight off the journal row — `RunRecord`
/// carries no time, and "how long ago" is the introspection surface's only age source).
pub(crate) struct RunListRow {
    pub(crate) run_id: String,
    pub(crate) time_created: f64,
    pub(crate) time_updated: f64,
}

/// One row of the enumeration surface (the shared cross-section of the run list / session summary).
/// A node-timestamp row (only the introspection SQL can read it; engine records have no clock).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NodeTimeRow {
    pub site_id: String,
    pub ordinal: u32,
    pub time_created: f64,
    pub time_updated: f64,
}

/// A list row (the elements of v4 `workflowRuns` map from it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunListItem {
    pub run_id: String,
    pub label: String,
    pub label_source: LabelSource,
    pub status: SnapshotStatus,
    /// The real terminal word, present only in a terminal state (`status` is background-task vocabulary and folds stopped→cancelled).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_status: Option<RunStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<RunStopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub spent_tokens: u64,
    /// The resume gate's predicate result (see [`is_resumable`]): the UI carries this one bit and never derives it from `status` itself.
    pub resumable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    /// The engine for this run is held by this service right now (unsettled).
    pub live: bool,
    pub created_at: f64,
    pub updated_at: f64,
}

// ————————————————————————————————————————————————————————————————
// Status synthesis
// ————————————————————————————————————————————————————————————————

/// Whether a logical status is terminal. This is the **only authority** on what counts as terminal: SQL is only an index-friendly pre-filter.
pub(crate) fn is_terminal(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed | RunStatus::Errored | RunStatus::Stopped
    )
}

/// **The single implementation of status-truth synthesis**, four priority tiers:
///   in-memory terminal state > live engine (journal status is trusted only when non-terminal) > journal status > "running but no row yet".
///
/// While this process's engine is still alive, a terminal journal row may come from another instance's orphan convergence and must not end local tracking;
/// conversely, once settlement happened locally the in-memory terminal state wins (it is the raw value just handed over by the engine).
pub(crate) fn synthesize_run_status(
    terminal: Option<&RunSettlement>,
    live: bool,
    journal_status: Option<RunStatus>,
) -> RunStatus {
    if let Some(settlement) = terminal {
        return settlement.status;
    }
    if let Some(status) = journal_status {
        // A live engine ignores terminal rows (foreign writes); non-terminal rows are trusted as usual (the engine wrote them itself).
        if live && is_terminal(status) {
            return RunStatus::Running;
        }
        return status;
    }
    // Engine/entry present, no journal row: the microtask gap between submit and createRun.
    RunStatus::Pending
}

/// The snapshot / background-task vocabulary: `pending` and `running` are both still running, `stopped` folds to cancelled and `errored`
/// folds to failed. The real terminal word is exposed separately through `runStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

pub(crate) fn fold_snapshot_status(status: RunStatus) -> SnapshotStatus {
    match status {
        RunStatus::Completed => SnapshotStatus::Completed,
        RunStatus::Errored => SnapshotStatus::Failed,
        RunStatus::Stopped => SnapshotStatus::Cancelled,
        RunStatus::Pending | RunStatus::Running => SnapshotStatus::Running,
    }
}

/// The **only predicate** of the resume gate: stopped is resumable except for `superseded` (the unfinished work now belongs to the successor,
/// so replaying would do the same thing twice against the workspace the successor is editing); errored / completed are not resumable.
///
/// The enumeration summary's `resumable` and `resume()`'s pre-check share it — if the UI re-derived it from status,
/// the two predicates would disagree one day: the button lit but the command refused.
pub(crate) fn is_resumable(status: RunStatus, stop_reason: Option<RunStopReason>) -> bool {
    status == RunStatus::Stopped && stop_reason != Some(RunStopReason::Superseded)
}

/// The stop reason is meaningful only for stopped; every other status returns `None`.
///
/// `live` = this service still holds an unsettled engine: then **the row's stop reason is not trusted** — it could only come from another
/// instance's orphan convergence, and "status says running, reason says interrupted" is harder to debug than either being wrong (same as TS `stopReasonOf`).
pub(crate) fn stop_reason_of(
    terminal: Option<&RunSettlement>,
    live: bool,
    record: Option<&RunRecord>,
) -> Option<RunStopReason> {
    if let Some(settlement) = terminal {
        return (settlement.status == RunStatus::Stopped)
            .then_some(settlement.reason)
            .flatten();
    }
    if live {
        return None;
    }
    let record = record?;
    (record.status == RunStatus::Stopped)
        .then_some(record.stop_reason)
        .flatten()
}

/// The successor pointer: in-memory terminal state first, then the journal row; meaningful only for stopped (on any other status the key reads as absent even if the payload carries it,
/// so a completed run is never drawn as "superseded").
pub(crate) fn superseded_by_of(
    terminal: Option<&RunSettlement>,
    record: Option<&RunRecord>,
) -> Option<String> {
    if let Some(settlement) = terminal {
        return (settlement.status == RunStatus::Stopped)
            .then(|| settlement.superseded_by.clone())
            .flatten();
    }
    let record = record?;
    (record.status == RunStatus::Stopped)
        .then(|| record.superseded_by.clone())
        .flatten()
}

/// errored always carries a failure; stopped carries one only for provider / interrupted (a user / model stop has no failure to report).
/// The journal's `failure_json` is authoritative: it carries a structured code (so `Interrupted` / `ProviderStop` /
/// `DriverError` are distinguishable), which is why it is passed through verbatim instead of folded into a generic failure.
pub(crate) fn failure_of(
    terminal: Option<&RunSettlement>,
    record: Option<&RunRecord>,
) -> Option<WorkflowErrorJson> {
    if let Some(settlement) = terminal {
        return match settlement.status {
            RunStatus::Errored | RunStatus::Stopped => {
                settlement.error.as_ref().map(|e| e.to_json())
            }
            _ => None,
        };
    }
    let record = record?;
    match record.status {
        RunStatus::Errored => record.failure.clone(),
        RunStatus::Stopped => match record.stop_reason {
            Some(RunStopReason::Provider) | Some(RunStopReason::Interrupted) => {
                record.failure.clone()
            }
            _ => None,
        },
        _ => None,
    }
}

// ————————————————————————————————————————————————————————————————
// Labels
// ————————————————————————————————————————————————————————————————

/// The character cap for a script-derived label (what one list row can show in full; a derived value is a heuristic, and the longer it is the less it looks like a label).
const LABEL_MAX_CHARS: usize = 80;

/// The label source. Exactly two: a user-chosen name and the derived first line (adding a third value for a branch that should not happen only makes consumers
/// write one more check that can never fire — same as TS).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelSource {
    Name,
    Script,
}

/// `name` (non-empty after trim) → the script's first non-empty line (cut at 80) → runId. A derived result is **never written back** to the journal.
pub(crate) fn run_label(
    run_id: &str,
    name: Option<&str>,
    script_text: Option<&str>,
) -> (String, LabelSource) {
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        return (name.to_string(), LabelSource::Name);
    }
    let first_line =
        script_text.and_then(|script| script.lines().map(str::trim).find(|line| !line.is_empty()));
    match first_line {
        Some(line) => (truncate_chars(line, LABEL_MAX_CHARS), LabelSource::Script),
        None => (run_id.to_string(), LabelSource::Script),
    }
}

/// Cut to the character cap (never splitting a Unicode scalar; the script is external input written by a model and may contain emoji).
fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

// ————————————————————————————————————————————————————————————————
// Node summaries / artifacts
// ————————————————————————————————————————————————————————————————

/// The sum of persisted node rows — **not** "total steps": a dynamic workflow has no static total, and queued exists only in the event
/// phase and is never persisted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeCounts {
    pub running: u32,
    pub completed: u32,
    pub failed: u32,
}

impl NodeCounts {
    pub fn observed(&self) -> u32 {
        self.running + self.completed + self.failed
    }
}

pub(crate) fn count_nodes(nodes: &[NodeRecord]) -> NodeCounts {
    let mut counts = NodeCounts::default();
    for node in nodes {
        match node.status {
            NodeRecordStatus::Running => counts.running += 1,
            NodeRecordStatus::Completed => counts.completed += 1,
            NodeRecordStatus::Failed => counts.failed += 1,
        }
    }
    counts
}

/// Node rows in the journal with `kind = "report"` (written once, always completed; the reported item is on `result`
/// itself), in insertion order = report order. Readers only appear after a terminal state; with zero rows the whole field is absent.
pub(crate) fn reported_items(nodes: &[NodeRecord]) -> Vec<Value> {
    nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Report)
        .map(|node| node.result.clone().unwrap_or(Value::Null))
        .collect()
}

/// One artifact version (the read-surface shape of `ArtifactVersionRecord`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunArtifactVersion {
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    pub published_at: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<bool>,
}

/// One artifact (version history aggregated by id plus the newest version's fields).
///
/// ⚠ Terminology: this is what the script publishes for the **user** to see through `artifact.*`, not the engine's internal
/// `RunSettlement.artifact` (the script's top-level return value, called `output` on the read surface).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunArtifact {
    pub id: String,
    pub kind: ArtifactOp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    pub version: u32,
    pub versions: Vec<RunArtifactVersion>,
    /// How many `report` rows carry this id label (the data volume of a preset board). Always 0 for content artifacts.
    pub item_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<bool>,
}

/// Merge rules (item by item the same as TS `artifactsOf`):
/// - Only `completed` rows are collected. A failed publish claims neither an id nor a version number; counting it into the version history
///   would make "version 3" point at bytes that never existed in the UI;
/// - rows with the same id sort by ascending `version`; what is lifted to the top level — `title` / `contentType` / `sourcePath` / `spec` —
///   is the **newest** version's value;
/// - `item_count` = the number of `report` rows carrying this id label;
/// - the deliverable (`primary`) leads, the rest keep their first-publish order.
pub(crate) fn merge_artifacts(nodes: &[NodeRecord]) -> Vec<RunArtifact> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, (ArtifactOp, Vec<RunArtifactVersion>)> = HashMap::new();
    for node in nodes {
        if node.kind != NodeKind::Artifact || node.status != NodeRecordStatus::Completed {
            continue;
        }
        let Some(record) = node.result.as_ref().and_then(artifact_record) else {
            continue;
        };
        let id = node
            .artifact_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| record.id.clone());
        match by_id.entry(id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().1.push(artifact_version(&record));
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                order.push(id);
                entry.insert((record.kind, vec![artifact_version(&record)]));
            }
        }
    }

    // Label counting scans the node table only when a **preset board really exists** (content artifacts always have item_count 0).
    let mut item_counts: HashMap<String, usize> = HashMap::new();
    if by_id.values().any(|(kind, _)| kind.is_preset()) {
        for node in nodes {
            if node.kind != NodeKind::Report {
                continue;
            }
            if let Some(artifact_id) = node.artifact_id.as_deref() {
                *item_counts.entry(artifact_id.to_string()).or_default() += 1;
            }
        }
    }

    let mut artifacts: Vec<RunArtifact> = order
        .into_iter()
        .filter_map(|id| {
            let (kind, mut versions) = by_id.remove(&id)?;
            versions.sort_by_key(|version| version.version);
            let latest = versions.last()?.clone();
            // The flag sticks to the id (the engine guarantees later versions carry it); any version setting it counts — older rows lack this key.
            let primary = versions
                .iter()
                .any(|version| version.primary == Some(true))
                .then_some(true);
            Some(RunArtifact {
                id: id.clone(),
                kind,
                title: latest.title,
                description: latest.description,
                content_type: latest.content_type,
                source_path: latest.source_path,
                spec: latest.spec,
                version: latest.version,
                versions,
                item_count: item_counts.get(&id).copied().unwrap_or(0),
                primary,
            })
        })
        .collect();
    // Stable sort: the relative order among non-primary entries does not move.
    artifacts.sort_by_key(|artifact| std::cmp::Reverse(artifact.primary == Some(true)));
    artifacts
}

/// `dwf_node.result_json` → a version record. The shape comes from the engine and the driver, but after a JSON round trip and possibly from
/// an older CLI, so it goes through deserialization narrowing: a row whose `version` is not a positive integer is dropped entirely
/// (a version without a version number cannot be located in the UI, nor can its bytes be read).
fn artifact_record(value: &Value) -> Option<ArtifactVersionRecord> {
    let record: ArtifactVersionRecord = serde_json::from_value(value.clone()).ok()?;
    (record.version >= 1).then_some(record)
}

fn artifact_version(record: &ArtifactVersionRecord) -> RunArtifactVersion {
    RunArtifactVersion {
        version: record.version,
        title: record.title.clone(),
        description: record.description.clone(),
        content_type: record.content_type.clone(),
        bytes: record.bytes,
        uri: record.uri.clone(),
        source_path: record.source_path.clone(),
        spec: record.spec.clone(),
        published_at: record.published_at.unwrap_or(0.0),
        primary: record.primary,
    }
}

/// Find the **completed** artifact row for `(artifact_id, version)` in the journal and take its `uri` and
/// `contentType`. The id from the renderer is used only to **look up the row** and never becomes a path directly (the first step of the authorization chain).
///
/// Completed only: a failed publish claims no id / kind / version; a preset board's record has no `uri` (it has no bytes),
/// and is rejected here for the same reason.
pub(crate) fn locate_artifact_version(
    nodes: &[NodeRecord],
    artifact_id: &str,
    version: u32,
) -> Option<(String, Option<String>)> {
    for node in nodes {
        if node.kind != NodeKind::Artifact || node.status != NodeRecordStatus::Completed {
            continue;
        }
        // One unreadable row must not make the whole listing unreadable: skip it and keep looking for other versions of the same id.
        let Some(record) = node.result.as_ref().and_then(artifact_record) else {
            continue;
        };
        let row_id = node
            .artifact_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| record.id.clone());
        if row_id != artifact_id || record.version != version {
            continue;
        }
        let uri = record.uri.clone().filter(|uri| !uri.is_empty())?;
        return Some((uri, record.content_type.clone()));
    }
    None
}

// ————————————————————————————————————————————————————————————————
// Workspace transcript listing
// ————————————————————————————————————————————————————————————————

/// The readable shape of one world read in the journal (a listing row; reading the body back is not in this deliverable).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceNode {
    pub site_id: String,
    pub ordinal: u32,
    pub kind: NodeKind,
    pub status: NodeRecordStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<WorldReadInput>,
}

/// world-read / world-run node rows → the workspace listing (journal insertion order = execution order). The row's `input` is exactly
/// the op + arguments the script gave (above 4KB the engine has already cut it to a preview and set `truncated`), so this read surface never touches bytes.
pub(crate) fn workspace_nodes(nodes: &[NodeRecord]) -> Vec<WorkspaceNode> {
    nodes
        .iter()
        .filter(|node| matches!(node.kind, NodeKind::WorldRead | NodeKind::WorldRun))
        .map(|node| WorkspaceNode {
            site_id: node.site_id.clone(),
            ordinal: node.ordinal,
            kind: node.kind,
            status: node.status,
            input: node.input.clone(),
        })
        .collect()
}

// ————————————————————————————————————————————————————————————————
// Pending questions
// ————————————————————————————————————————————————————————————————

/// A pending question on the snapshot / detail (port of `DynamicWorkflowRunPendingQuestion`). `actor` is the
/// `site@ordinal` rendering form (same as TS `refToString`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingQuestionView {
    pub qid: String,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_name: Option<String>,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub asked_at: f64,
}

pub(crate) fn pending_question_views(questions: &[PendingQuestion]) -> Vec<PendingQuestionView> {
    questions
        .iter()
        .map(|question| PendingQuestionView {
            qid: question.qid.clone(),
            actor: nc_code_workflow::types::ref_to_string(
                &question.actor.site_id,
                question.actor.ordinal,
            ),
            actor_name: question.actor_name.clone(),
            question: question.question.clone(),
            context: question.context.clone(),
            asked_at: question.asked_at,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_workflow::types::{ActorRef, ArtifactVersionRecord, Caps, InstanceRef};
    use serde_json::json;

    fn run_record(status: RunStatus, stop_reason: Option<RunStopReason>) -> RunRecord {
        RunRecord {
            run_id: "run-1".into(),
            parent_session_id: Some("session-1".into()),
            cwd: Some("/work".into()),
            name: None,
            tool_call_id: None,
            script_text: None,
            script_hash: None,
            args: None,
            resumed_from: None,
            caps: Caps { max_concurrency: 4 },
            spent_tokens: 0,
            status,
            stop_reason,
            superseded_by: None,
            failure: None,
            result: None,
        }
    }

    fn artifact_node(
        id: &str,
        version: u32,
        kind: ArtifactOp,
        primary: Option<bool>,
    ) -> NodeRecord {
        let record = ArtifactVersionRecord {
            id: id.to_string(),
            kind,
            version,
            title: Some(format!("title v{version}")),
            description: None,
            content_type: Some("text/markdown".into()),
            bytes: Some(3),
            uri: Some(format!("zcode-artifact://run-1/{id}")),
            source_path: None,
            spec: None,
            published_at: Some(version as f64),
            primary,
        };
        NodeRecord {
            run_id: "run-1".into(),
            site_id: "artifact#1".into(),
            ordinal: 1,
            kind: NodeKind::Artifact,
            actor_site_id: None,
            actor_ordinal: None,
            actor_seq: None,
            input_hash: "h".into(),
            status: NodeRecordStatus::Completed,
            result: Some(serde_json::to_value(record).unwrap()),
            error: None,
            stats: None,
            artifact_id: Some(id.to_string()),
            input: None,
            message_boundary: None,
        }
    }

    #[test]
    fn snapshot_status_folds_pending_to_running_and_terminal_words() {
        assert_eq!(
            fold_snapshot_status(RunStatus::Pending),
            SnapshotStatus::Running
        );
        assert_eq!(
            fold_snapshot_status(RunStatus::Running),
            SnapshotStatus::Running
        );
        assert_eq!(
            fold_snapshot_status(RunStatus::Completed),
            SnapshotStatus::Completed
        );
        assert_eq!(
            fold_snapshot_status(RunStatus::Errored),
            SnapshotStatus::Failed
        );
        assert_eq!(
            fold_snapshot_status(RunStatus::Stopped),
            SnapshotStatus::Cancelled
        );
    }

    #[test]
    fn status_truth_prefers_memory_then_ignores_foreign_terminal_rows() {
        let settlement = RunSettlement {
            status: RunStatus::Stopped,
            artifact: None,
            reason: Some(RunStopReason::User),
            superseded_by: None,
            error: None,
        };
        // In-memory terminal state beats the journal row (the row may not be written yet).
        assert_eq!(
            synthesize_run_status(Some(&settlement), true, Some(RunStatus::Running)),
            RunStatus::Stopped
        );
        // Live engine + terminal journal row = a foreign write: report it as running, honestly.
        assert_eq!(
            synthesize_run_status(None, true, Some(RunStatus::Stopped)),
            RunStatus::Running
        );
        // Live engine + non-terminal row is trusted as usual.
        assert_eq!(
            synthesize_run_status(None, true, Some(RunStatus::Running)),
            RunStatus::Running
        );
        // Entry only, no row: the submit → createRun gap; pending is the only honest answer.
        assert_eq!(synthesize_run_status(None, true, None), RunStatus::Pending);
    }

    #[test]
    fn resumable_is_stopped_except_superseded() {
        assert!(is_resumable(RunStatus::Stopped, Some(RunStopReason::User)));
        assert!(is_resumable(
            RunStatus::Stopped,
            Some(RunStopReason::Interrupted)
        ));
        assert!(is_resumable(RunStatus::Stopped, None));
        assert!(!is_resumable(
            RunStatus::Stopped,
            Some(RunStopReason::Superseded)
        ));
        assert!(!is_resumable(RunStatus::Errored, None));
        assert!(!is_resumable(RunStatus::Completed, None));
    }

    #[test]
    fn stop_reason_and_failure_only_apply_to_stopped_rows() {
        let stopped = run_record(RunStatus::Stopped, Some(RunStopReason::Interrupted));
        assert_eq!(
            stop_reason_of(None, false, Some(&stopped)),
            Some(RunStopReason::Interrupted)
        );
        // Under a live engine the row's stop reason is a foreign write and is not trusted.
        assert_eq!(stop_reason_of(None, true, Some(&stopped)), None);
        let completed = run_record(RunStatus::Completed, None);
        assert_eq!(stop_reason_of(None, false, Some(&completed)), None);
        let failed = failure_of(None, Some(&run_record(RunStatus::Errored, None)));
        assert!(
            failed.is_none(),
            "a row without failure_json has no failure"
        );
        let mut interrupted = run_record(RunStatus::Stopped, Some(RunStopReason::Interrupted));
        interrupted.failure = Some(
            nc_code_workflow::types::WorkflowError::new(
                nc_code_workflow::types::WorkflowErrorCode::Interrupted,
                "process died",
            )
            .to_json(),
        );
        assert_eq!(
            failure_of(None, Some(&interrupted))
                .expect("interrupted carries its failure")
                .code,
            nc_code_workflow::types::WorkflowErrorCode::Interrupted
        );
        // A user stop has no failure to report, even if the row carries one (defensive: do not report it as an error).
        let mut user_stop = run_record(RunStatus::Stopped, Some(RunStopReason::User));
        user_stop.failure = interrupted.failure.clone();
        assert!(failure_of(None, Some(&user_stop)).is_none());
    }

    #[test]
    fn artifacts_merge_versions_and_put_the_primary_first() {
        let mut nodes = vec![
            artifact_node("secondary", 1, ArtifactOp::Markdown, None),
            artifact_node("primary", 1, ArtifactOp::Markdown, Some(true)),
            artifact_node("primary", 2, ArtifactOp::Markdown, Some(true)),
            {
                let mut failed = artifact_node("secondary", 2, ArtifactOp::Markdown, None);
                failed.status = NodeRecordStatus::Failed;
                failed.result = None;
                failed
            },
        ];
        // One report row feeding a board (item_count).
        nodes.push(NodeRecord {
            run_id: "run-1".into(),
            site_id: "report#1".into(),
            ordinal: 1,
            kind: NodeKind::Report,
            actor_site_id: None,
            actor_ordinal: None,
            actor_seq: None,
            input_hash: "h".into(),
            status: NodeRecordStatus::Completed,
            result: Some(json!({"x": 1})),
            error: None,
            stats: None,
            artifact_id: Some("primary".into()),
            input: None,
            message_boundary: None,
        });

        let artifacts = merge_artifacts(&nodes);
        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].id, "primary", "primary leads");
        assert_eq!(artifacts[0].version, 2, "latest version fields are hoisted");
        assert_eq!(artifacts[0].title.as_deref(), Some("title v2"));
        assert_eq!(artifacts[0].versions.len(), 2);
        assert_eq!(artifacts[0].primary, Some(true));
        // A failed publish claims no version.
        assert_eq!(artifacts[1].versions.len(), 1);

        let located = locate_artifact_version(&nodes, "primary", 1).expect("row");
        assert_eq!(located.0, "zcode-artifact://run-1/primary");
        assert_eq!(located.1.as_deref(), Some("text/markdown"));
        assert!(locate_artifact_version(&nodes, "secondary", 2).is_none());
    }

    #[test]
    fn labels_prefer_the_name_then_the_first_script_line() {
        assert_eq!(
            run_label("run-1", Some(" nightly "), Some("return 1;")),
            ("nightly".to_string(), LabelSource::Name)
        );
        assert_eq!(
            run_label(
                "run-1",
                Some("  "),
                Some("\n  // audit the repo  \nreturn 1;")
            ),
            ("// audit the repo".to_string(), LabelSource::Script)
        );
        assert_eq!(
            run_label("run-1", None, None),
            ("run-1".to_string(), LabelSource::Script)
        );
    }

    #[test]
    fn pending_questions_render_the_actor_reference() {
        let views = pending_question_views(&[PendingQuestion {
            qid: "q1".into(),
            run_id: "run-1".into(),
            actor: ActorRef {
                site_id: "actor#1".into(),
                ordinal: 2,
            },
            actor_name: Some("critic".into()),
            question: "which gate?".into(),
            context: None,
            asked_at: 1.0,
            instance: InstanceRef {
                site_id: "ask#1".into(),
                ordinal: 1,
            },
        }]);
        assert_eq!(views[0].actor, "actor#1@2");
        assert_eq!(views[0].actor_name.as_deref(), Some("critic"));
    }
}
