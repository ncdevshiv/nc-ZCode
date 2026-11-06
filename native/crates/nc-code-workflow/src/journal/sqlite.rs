//! `JournalStorePort` over SQLite (`dwf_*` tables).
//!
//! Port of `apps/nc-code-cli/packages/adapters/src/storage/session-store/repositories/dwf-journal.ts`
//! (+ `dwf-journal-codecs.ts`) and of the `0019_dwf_journal` baseline in `migrations.ts`.
//!
//! The physical schema is copied column-for-column, constraint-for-constraint so a journal
//! written by the TypeScript runtime stays readable and resumable here and vice versa:
//!
//! | table       | purpose                                                        |
//! | ----------- | -------------------------------------------------------------- |
//! | `dwf_run`   | one row per run: metadata, accumulated usage, logical status   |
//! | `dwf_actor` | one row per created actor: persona + driver-owned session pin  |
//! | `dwf_node`  | one row per ask / world-read / world-run / report / artifact    |
//! | `dwf_event` | append-only run event log, `sequence` monotonic per run        |
//!
//! Logical↔physical status mapping lives only here (mirrors `dwf-journal-codecs.ts`):
//!
//! | logical                    | `dwf_run.status` | `failure_json`                                    |
//! | -------------------------- | ---------------- | ------------------------------------------------- |
//! | `stopped{reason, error?}`  | `cancelled`      | `{"stopReason": …, "supersededBy"?, "error"?}`     |
//! | `errored{error}`           | `failed`         | the bare `WorkflowErrorJson`                       |
//! | `completed` / `pending` / `running` | same name | unchanged                              |
//!
//! Decoding also covers historical rows: `cancelled` without a reason envelope reads back as
//! `stopped(user)`, and `failed` with code `Interrupted` (orphan convergence) reads back as
//! `stopped(interrupted)`.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::types::{
    ActorRecord, Caps, JournalStorePort, ListEventsOptions, NodeKind, NodeRecord, NodeRecordStatus,
    RunEvent, RunRecord, RunSettlementRecord, RunStatus, RunStopReason, StoredEvent,
    WorkflowErrorCode, WorkflowErrorJson,
};

// ————————————————————————————————————————————————————————————————
// Connection and migration
// ————————————————————————————————————————————————————————————————

/// Connection-level pragmas aligned with the TS session store:
///
/// - `foreign_keys = on` — `migration-runner.ts:137` (among the `dwf_*` tables the only cascading FKs are node/actor/event →
///   run; parent_session_id / session_id are deliberately plain text);
/// - `journal_mode = wal` — `migration-runner.ts:139-148` (`readJournalMode(db, true)`);
///   on a `:memory:` database SQLite returns `memory` instead of erroring, the same outcome as TS's `isMemoryJournalMode`;
/// - `busy_timeout = 5000` — `DEFAULT_SQLITE_STARTUP_LOCK_TIMEOUT_MS` from `migration-runner.ts:13`,
///   restored to that value at `:92` once the migration finishes; when multiple Agents share
///   the same database, this waiting policy must match TS;
/// - `synchronous = normal` — the TS side has no such pragma (node:sqlite uses the default); this is an explicit
///   native-side choice: under WAL, NORMAL already guarantees crash consistency without fsyncing every commit.
const OPEN_PRAGMAS: &str = "\
pragma journal_mode = wal;
pragma synchronous = normal;
pragma foreign_keys = on;
pragma busy_timeout = 5000;
";

/// The DDL of `0019_dwf_journal` (`migrations.ts:830-911`), with column names/types/CHECK/indexes copied verbatim.
///
/// `create … if not exists` keeps `migrate` idempotent: TS's migration runner keeps a ledger so it runs only once,
/// while here the same DDL runs every time the database is opened, at the cost of a few no-ops.
const DWF_JOURNAL_SCHEMA: &str = r#"
create table if not exists dwf_run (
  id text primary key,
  parent_session_id text,
  cwd text,
  name text,
  script_text text,
  script_hash text,
  args_json text,
  tool_call_id text,
  resumed_from text,
  caps_max_concurrency integer not null,
  spent_tokens integer not null default 0,
  status text not null check(status in (
    'pending',
    'running',
    'completed',
    'failed',
    'cancelled'
  )),
  result_json text,
  failure_json text,
  time_created integer not null,
  time_updated integer not null
);

create index if not exists dwf_run_cwd_idx on dwf_run(cwd, time_updated);

create table if not exists dwf_actor (
  id integer primary key autoincrement,
  run_id text not null references dwf_run(id) on delete cascade,
  site_id text not null,
  ordinal integer not null,
  name text,
  persona_json text,
  resolved_model text,
  session_id text,
  time_created integer not null,
  time_updated integer not null,
  unique(run_id, site_id, ordinal)
);

create index if not exists dwf_actor_run_idx on dwf_actor(run_id);

create table if not exists dwf_node (
  id integer primary key autoincrement,
  run_id text not null references dwf_run(id) on delete cascade,
  site_id text not null,
  ordinal integer not null,
  kind text not null check(kind in ('ask', 'world-read', 'world-run', 'report', 'artifact')),
  actor_site_id text,
  actor_ordinal integer,
  actor_seq integer,
  input_hash text not null,
  input_json text,
  status text not null check(status in ('running', 'completed', 'failed')),
  result_json text,
  error_json text,
  stats_json text,
  message_boundary integer,
  artifact_id text,
  time_created integer not null,
  time_updated integer not null,
  unique(run_id, site_id, ordinal)
);

create index if not exists dwf_node_run_idx on dwf_node(run_id);
create index if not exists dwf_node_artifact_idx on dwf_node(run_id, artifact_id);

create table if not exists dwf_event (
  id integer primary key autoincrement,
  run_id text not null references dwf_run(id) on delete cascade,
  sequence integer not null,
  type text not null,
  payload_json text not null,
  time_created integer not null,
  unique(run_id, sequence)
);

create index if not exists dwf_event_artifact_idx
  on dwf_event(run_id, json_extract(payload_json, '$.artifactId'), sequence);
"#;

/// The `JournalStorePort` on top of SQLite.
///
/// The port is synchronous (the engine takes no async seam from persistence), so the connection lives in a `Mutex` and is used by `&self` methods.
pub struct SqliteJournal {
    conn: Mutex<Connection>,
}

impl SqliteJournal {
    /// Open (creating if necessary) a journal database: apply [`OPEN_PRAGMAS`] first, then run [`Self::migrate`].
    ///
    /// Auto-migration matches the TS `SqliteSessionStore` constructor (`sqlite-session-store.ts:259-261`
    /// runs `runSqliteSessionMigrations` right after open); `migrate` stays public so callers that "attach to a
    /// session database already migrated elsewhere" can invoke it explicitly.
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(OPEN_PRAGMAS)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Idempotently create all four `dwf_*` tables and the five indexes (`migrations.ts:830-911`).
    ///
    /// Creates tables only, touches no data, and does not write TS's `schema_migration` ledger: the ledger is the
    /// session store's migration runner's business; here only the shape is guaranteed.
    pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(DWF_JOURNAL_SCHEMA)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("journal lock")
    }
}

/// The port methods have no `Result`: storage errors throw on the TS side, so here we can only panic loudly.
fn fail(context: &str, error: rusqlite::Error) -> ! {
    panic!("dwf journal: {context}: {error}")
}

/// An UPDATE affecting 0 rows means "unknown run" -- SQLite does not raise an error for that, so it must be detected
/// explicitly (`assertRunTouched` in `dwf-journal.ts`).
fn assert_run_touched(changed: usize, run_id: &str) {
    if changed == 0 {
        panic!("dwf journal: unknown run: {run_id}");
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

// ————————————————————————————————————————————————————————————————
// Column codecs (dwf-journal-codecs.ts)
// ————————————————————————————————————————————————————————————————

/// `encodeJson` (`session-store/json.ts`): `None` → SQL NULL, anything else stored as JSON text.
fn encode_json<T: Serialize>(value: Option<&T>, column: &str) -> Option<String> {
    value.map(|value| {
        serde_json::to_string(value)
            .unwrap_or_else(|error| panic!("dwf journal: cannot encode {column}: {error}"))
    })
}

/// `decodeJson`: NULL / empty string → an absent key; malformed JSON fails loudly (TS's `JSON.parse` throws too).
fn decode_json<T: DeserializeOwned>(text: Option<String>, column: &str) -> Option<T> {
    let value = decode_value(text, column)?;
    Some(
        serde_json::from_value(value)
            .unwrap_or_else(|error| panic!("dwf journal: cannot decode {column}: {error}")),
    )
}

fn decode_value(text: Option<String>, column: &str) -> Option<Value> {
    let text = text?;
    if text.is_empty() {
        return None;
    }
    Some(
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("dwf journal: invalid JSON in {column}: {error}")),
    )
}

/// `encodeResultJson` (`dwf-journal-codecs.ts:317-321`): `result: null` is a legal ask result;
/// only **absence** maps to SQL NULL.
fn encode_result_json(value: Option<&Value>) -> Option<String> {
    value.map(Value::to_string)
}

fn decode_result_json(text: Option<String>) -> Option<Value> {
    decode_value(text, "result_json")
}

fn parse_error_json(value: &Value) -> Option<WorkflowErrorJson> {
    serde_json::from_value(value.clone()).ok()
}

/// The envelope inside a stopped row's `failure_json` (told apart from `WorkflowErrorJson` by the `stopReason` key).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoppedEnvelope<'a> {
    stop_reason: RunStopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a WorkflowErrorJson>,
}

struct EncodedSettlement {
    physical: &'static str,
    failure_json: Option<String>,
}

/// Logical status (+ the settlement bag) → physical column value. The single write-side entry point (shared by `createRun` / `updateRunStatus`),
/// corresponding to `encodeRunSettlement` (`dwf-journal-codecs.ts:77-96`).
fn encode_run_settlement(
    status: RunStatus,
    stop_reason: Option<RunStopReason>,
    superseded_by: Option<&str>,
    failure: Option<&WorkflowErrorJson>,
) -> EncodedSettlement {
    match status {
        RunStatus::Stopped => {
            // stopped always writes the envelope: a historical/fallback path with no reason decodes as user, not as "no reason".
            let envelope = StoppedEnvelope {
                stop_reason: stop_reason.unwrap_or(RunStopReason::User),
                superseded_by,
                error: failure,
            };
            EncodedSettlement {
                physical: "cancelled",
                failure_json: Some(serde_json::to_string(&envelope).unwrap_or_else(|error| {
                    panic!("dwf journal: cannot encode envelope: {error}")
                })),
            }
        }
        RunStatus::Errored => EncodedSettlement {
            physical: "failed",
            failure_json: encode_json(failure, "failure_json"),
        },
        RunStatus::Completed => EncodedSettlement {
            physical: "completed",
            failure_json: encode_json(failure, "failure_json"),
        },
        RunStatus::Pending | RunStatus::Running => EncodedSettlement {
            physical: status.as_str(),
            failure_json: None,
        },
    }
}

struct DecodedSettlement {
    status: RunStatus,
    stop_reason: Option<RunStopReason>,
    superseded_by: Option<String>,
    failure: Option<WorkflowErrorJson>,
}

fn parse_stop_reason(value: &str) -> Option<RunStopReason> {
    Some(match value {
        "user" => RunStopReason::User,
        "model" => RunStopReason::Model,
        "provider" => RunStopReason::Provider,
        "interrupted" => RunStopReason::Interrupted,
        "superseded" => RunStopReason::Superseded,
        _ => return None,
    })
}

/// Physical column value → the logical terminal triple, corresponding to `decodeRunSettlement` (`dwf-journal-codecs.ts:99-131`).
/// Historical rows need no backfill: `cancelled` with no envelope ⇒ `stopped(user)`; `failed` + `Interrupted` ⇒
/// `stopped(interrupted)`; any other `failed` ⇒ `errored`.
fn decode_run_settlement(physical: &str, failure_json: Option<&str>) -> DecodedSettlement {
    let raw = failure_json.and_then(|text| {
        if text.is_empty() {
            return None;
        }
        Some(
            serde_json::from_str::<Value>(text)
                .unwrap_or_else(|error| panic!("dwf journal: invalid failure_json: {error}")),
        )
    });
    match physical {
        "cancelled" => {
            let mut decoded = DecodedSettlement {
                status: RunStatus::Stopped,
                stop_reason: Some(RunStopReason::User),
                superseded_by: None,
                failure: None,
            };
            // Envelope sniffing uses a whitelist: not an envelope (an old row, some other failure) degrades to stopped(user).
            if let Some(Value::Object(map)) = &raw {
                if let Some(reason) = map
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .and_then(parse_stop_reason)
                {
                    decoded.stop_reason = Some(reason);
                    if let Some(superseded_by) = map.get("supersededBy").and_then(Value::as_str) {
                        if !superseded_by.is_empty() {
                            decoded.superseded_by = Some(superseded_by.to_owned());
                        }
                    }
                    decoded.failure = map.get("error").and_then(parse_error_json);
                }
            }
            decoded
        }
        "failed" => {
            let interrupted = raw
                .as_ref()
                .and_then(|value| value.get("code"))
                .and_then(Value::as_str)
                == Some(WorkflowErrorCode::Interrupted.as_str());
            let failure = raw.as_ref().and_then(parse_error_json);
            if interrupted {
                DecodedSettlement {
                    status: RunStatus::Stopped,
                    stop_reason: Some(RunStopReason::Interrupted),
                    superseded_by: None,
                    failure,
                }
            } else {
                DecodedSettlement {
                    status: RunStatus::Errored,
                    stop_reason: None,
                    superseded_by: None,
                    failure,
                }
            }
        }
        "completed" => DecodedSettlement {
            status: RunStatus::Completed,
            stop_reason: None,
            superseded_by: None,
            failure: raw.as_ref().and_then(parse_error_json),
        },
        "pending" => DecodedSettlement {
            status: RunStatus::Pending,
            stop_reason: None,
            superseded_by: None,
            failure: None,
        },
        "running" => DecodedSettlement {
            status: RunStatus::Running,
            stop_reason: None,
            superseded_by: None,
            failure: None,
        },
        other => panic!("dwf journal: unknown dwf_run.status: {other}"),
    }
}

fn node_kind_str(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Ask => "ask",
        NodeKind::WorldRead => "world-read",
        NodeKind::WorldRun => "world-run",
        NodeKind::Report => "report",
        NodeKind::Artifact => "artifact",
    }
}

fn parse_node_kind(value: &str) -> NodeKind {
    match value {
        "ask" => NodeKind::Ask,
        "world-read" => NodeKind::WorldRead,
        "world-run" => NodeKind::WorldRun,
        "report" => NodeKind::Report,
        "artifact" => NodeKind::Artifact,
        other => panic!("dwf journal: unknown dwf_node.kind: {other}"),
    }
}

fn node_status_str(status: NodeRecordStatus) -> &'static str {
    match status {
        NodeRecordStatus::Running => "running",
        NodeRecordStatus::Completed => "completed",
        NodeRecordStatus::Failed => "failed",
    }
}

fn parse_node_status(value: &str) -> NodeRecordStatus {
    match value {
        "running" => NodeRecordStatus::Running,
        "completed" => NodeRecordStatus::Completed,
        "failed" => NodeRecordStatus::Failed,
        other => panic!("dwf journal: unknown dwf_node.status: {other}"),
    }
}

// ————————————————————————————————————————————————————————————————
// Statements (corresponding to dwf-journal.ts one by one)
// ————————————————————————————————————————————————————————————————

const CREATE_RUN_SQL: &str = "\
insert into dwf_run (
  id, parent_session_id, cwd, name, script_text, script_hash, tool_call_id,
  args_json, resumed_from, caps_max_concurrency,
  spent_tokens, status, failure_json, result_json, time_created, time_updated
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

const GET_RUN_SQL: &str = "select * from dwf_run where id = ?";

/// A non-terminal state = no settlement: when resume flips a run back to running it must clear the previous generation's failure_json /
/// result_json, otherwise the Interrupted failure written by orphan reconciliation would coexist with running (`dwf-journal.ts:129-150`).
const UPDATE_RUN_NON_TERMINAL_SQL: &str = "\
update dwf_run set
  status = ?,
  failure_json = null,
  result_json = null,
  time_updated = ?
where id = ?";

/// The terminal state and the artifact in one write. `failure_json` is rewritten as a whole column (the settlement bag is the entire truth about failure at this moment);
/// `result_json` is preserved with coalesce (absent = untouched, so a repeated settlement carrying no artifact does not erase it).
const UPDATE_RUN_TERMINAL_SQL: &str = "\
update dwf_run set
  status = ?,
  failure_json = ?,
  result_json = coalesce(?, result_json),
  time_updated = ?
where id = ?";

const UPDATE_RUN_USAGE_SQL: &str =
    "update dwf_run set spent_tokens = ?, time_updated = ? where id = ?";

const PUT_ACTOR_SQL: &str = "\
insert into dwf_actor (
  run_id, site_id, ordinal, name, persona_json, session_id, resolved_model,
  time_created, time_updated
) values (?, ?, ?, ?, ?, ?, ?, ?, ?)
on conflict(run_id, site_id, ordinal) do update set
  name = excluded.name,
  persona_json = excluded.persona_json,
  session_id = excluded.session_id,
  resolved_model = excluded.resolved_model,
  time_updated = excluded.time_updated";

const GET_ACTOR_SQL: &str =
    "select * from dwf_actor where run_id = ? and site_id = ? and ordinal = ?";

/// `order by id`: the read order is the persistence order (same in TS). The in-memory implementation sorts by (siteId, ordinal), so
/// the read order across implementations agrees only when "insertion order == key order" (the contract tests construct their scenarios that way).
const LIST_ACTORS_SQL: &str = "select * from dwf_actor where run_id = ? order by id";

const PUT_NODE_SQL: &str = "\
insert into dwf_node (
  run_id, site_id, ordinal, kind, actor_site_id, actor_ordinal, actor_seq,
  input_hash, status, result_json, error_json, stats_json, message_boundary,
  artifact_id, input_json, time_created, time_updated
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
on conflict(run_id, site_id, ordinal) do update set
  kind = excluded.kind,
  actor_site_id = excluded.actor_site_id,
  actor_ordinal = excluded.actor_ordinal,
  actor_seq = excluded.actor_seq,
  input_hash = excluded.input_hash,
  status = excluded.status,
  result_json = excluded.result_json,
  error_json = excluded.error_json,
  stats_json = excluded.stats_json,
  message_boundary = excluded.message_boundary,
  artifact_id = excluded.artifact_id,
  input_json = excluded.input_json,
  time_updated = excluded.time_updated";

const GET_NODE_SQL: &str =
    "select * from dwf_node where run_id = ? and site_id = ? and ordinal = ?";

const LIST_NODES_SQL: &str = "select * from dwf_node where run_id = ? order by id";

/// Sequence allocation and the write are the same statement: reading `MAX(sequence)+1` once on its own and then inserting would race out
/// duplicate sequences between processes (several Agents sharing one database under WAL). The base matches TS: the first event's sequence is 0
/// (`coalesce(max(sequence) + 1, 0)`, `dwf-journal.ts:371-380`).
const APPEND_EVENT_SQL: &str = "\
insert into dwf_event (run_id, sequence, type, payload_json, time_created)
values (
  ?,
  coalesce((select max(sequence) + 1 from dwf_event where run_id = ?), 0),
  ?, ?, ?
)
returning sequence";

const LIST_EVENTS_SQL: &str =
    "select sequence, payload_json, time_created from dwf_event where run_id = ? order by sequence limit ?";

/// The cursor semantics are "strictly greater than", matching the in-memory implementation; `limit -1` is SQLite's spelling for "no limit".
const LIST_EVENTS_AFTER_SQL: &str =
    "select sequence, payload_json, time_created from dwf_event where run_id = ? and sequence > ? order by sequence limit ?";

// ————————————————————————————————————————————————————————————————
// Row decoding
// ————————————————————————————————————————————————————————————————

fn decode_run(row: &Row<'_>) -> rusqlite::Result<RunRecord> {
    let physical: String = row.get("status")?;
    let failure_json: Option<String> = row.get("failure_json")?;
    let settlement = decode_run_settlement(&physical, failure_json.as_deref());
    Ok(RunRecord {
        run_id: row.get("id")?,
        parent_session_id: row.get("parent_session_id")?,
        cwd: row.get("cwd")?,
        name: row.get("name")?,
        tool_call_id: row.get("tool_call_id")?,
        script_text: row.get("script_text")?,
        script_hash: row.get("script_hash")?,
        // NULL means an absent key: keep "no arguments" distinguishable from "arguments are an empty bag" at the record level.
        args: decode_json(row.get("args_json")?, "dwf_run.args_json"),
        resumed_from: row.get("resumed_from")?,
        caps: Caps {
            max_concurrency: row.get::<_, i64>("caps_max_concurrency")? as u32,
        },
        spent_tokens: row.get::<_, i64>("spent_tokens")? as u64,
        status: settlement.status,
        stop_reason: settlement.stop_reason,
        superseded_by: settlement.superseded_by,
        failure: settlement.failure,
        result: decode_result_json(row.get("result_json")?),
    })
}

fn decode_actor(row: &Row<'_>) -> rusqlite::Result<ActorRecord> {
    Ok(ActorRecord {
        run_id: row.get("run_id")?,
        site_id: row.get("site_id")?,
        ordinal: row.get::<_, i64>("ordinal")? as u32,
        name: row.get("name")?,
        persona: decode_json(row.get("persona_json")?, "dwf_actor.persona_json"),
        session_id: row.get("session_id")?,
        resolved_model: row.get("resolved_model")?,
    })
}

fn decode_node(row: &Row<'_>) -> rusqlite::Result<NodeRecord> {
    let kind: String = row.get("kind")?;
    let status: String = row.get("status")?;
    Ok(NodeRecord {
        run_id: row.get("run_id")?,
        site_id: row.get("site_id")?,
        ordinal: row.get::<_, i64>("ordinal")? as u32,
        kind: parse_node_kind(&kind),
        actor_site_id: row.get("actor_site_id")?,
        actor_ordinal: row
            .get::<_, Option<i64>>("actor_ordinal")?
            .map(|value| value as u32),
        actor_seq: row
            .get::<_, Option<i64>>("actor_seq")?
            .map(|value| value as u64),
        input_hash: row.get("input_hash")?,
        status: parse_node_status(&status),
        result: decode_result_json(row.get("result_json")?),
        error: decode_json(row.get("error_json")?, "dwf_node.error_json"),
        stats: decode_json(row.get("stats_json")?, "dwf_node.stats_json"),
        // `0` is a legal boundary (copying zero messages), so the test must be "not NULL" rather than truthiness.
        message_boundary: row
            .get::<_, Option<i64>>("message_boundary")?
            .map(|value| value as u64),
        artifact_id: row.get("artifact_id")?,
        input: decode_json(row.get("input_json")?, "dwf_node.input_json"),
    })
}

fn decode_event(row: &Row<'_>) -> rusqlite::Result<StoredEvent> {
    let payload: String = row.get(1)?;
    Ok(StoredEvent {
        sequence: row.get::<_, i64>(0)? as u64,
        event: serde_json::from_str(&payload)
            .unwrap_or_else(|error| panic!("dwf journal: invalid event payload: {error}")),
        // The only clock in the event log; the column always has a value since 0019, so it is written unconditionally.
        time_created: Some(row.get::<_, i64>(2)? as f64),
    })
}

// ————————————————————————————————————————————————————————————————
// JournalStorePort
// ————————————————————————————————————————————————————————————————

impl JournalStorePort for SqliteJournal {
    fn create_run(&self, record: RunRecord) {
        let encoded = encode_run_settlement(
            record.status,
            record.stop_reason,
            record.superseded_by.as_deref(),
            record.failure.as_ref(),
        );
        let now = now_ms();
        let conn = self.lock();
        let result = conn.execute(
            CREATE_RUN_SQL,
            params![
                record.run_id,
                record.parent_session_id,
                record.cwd,
                record.name,
                record.script_text,
                record.script_hash,
                record.tool_call_id,
                encode_json(record.args.as_ref(), "dwf_run.args_json"),
                record.resumed_from,
                record.caps.max_concurrency as i64,
                record.spent_tokens as i64,
                encoded.physical,
                encoded.failure_json,
                encode_result_json(record.result.as_ref()),
                now,
                now,
            ],
        );
        if let Err(error) = result {
            // A duplicate run is the caller's contract error and deserves a message you can read directly; other failures (FK, disk,
            // constraints) propagate unchanged.
            let exists: i64 = conn
                .query_row(
                    "select exists(select 1 from dwf_run where id = ?)",
                    [&record.run_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            if exists != 0 {
                panic!("dwf journal: run already exists: {}", record.run_id);
            }
            fail("create_run", error);
        }
    }

    fn get_run(&self, run_id: &str) -> Option<RunRecord> {
        let conn = self.lock();
        conn.query_row(GET_RUN_SQL, [run_id], decode_run)
            .optional()
            .unwrap_or_else(|error| fail("get_run", error))
    }

    fn update_run_status(
        &self,
        run_id: &str,
        status: RunStatus,
        settlement: Option<&RunSettlementRecord>,
    ) {
        let now = now_ms();
        let conn = self.lock();
        if matches!(status, RunStatus::Pending | RunStatus::Running) {
            let changed = conn
                .execute(
                    UPDATE_RUN_NON_TERMINAL_SQL,
                    params![status.as_str(), now, run_id],
                )
                .unwrap_or_else(|error| fail("update_run_status", error));
            assert_run_touched(changed, run_id);
            return;
        }
        // The terminal state and the artifact in one write: two separate UPDATEs would open a crash window producing a run that is
        // "completed but with an empty result_json".
        let encoded = encode_run_settlement(
            status,
            settlement.and_then(|settlement| settlement.stop_reason),
            settlement.and_then(|settlement| settlement.superseded_by.as_deref()),
            settlement.and_then(|settlement| settlement.failure.as_ref()),
        );
        let changed = conn
            .execute(
                UPDATE_RUN_TERMINAL_SQL,
                params![
                    encoded.physical,
                    encoded.failure_json,
                    encode_result_json(
                        settlement.and_then(|settlement| settlement.result.as_ref())
                    ),
                    now,
                    run_id,
                ],
            )
            .unwrap_or_else(|error| fail("update_run_status", error));
        assert_run_touched(changed, run_id);
    }

    /// A high-frequency small write: touches only `spent_tokens` / `time_updated` and never status / failure / result.
    /// An unknown runId is a broken contract and must fail loudly (the in-memory implementation panics the same way).
    fn update_run_usage(&self, run_id: &str, spent_tokens: u64) {
        let now = now_ms();
        let conn = self.lock();
        let changed = conn
            .execute(
                UPDATE_RUN_USAGE_SQL,
                params![spent_tokens as i64, now, run_id],
            )
            .unwrap_or_else(|error| fail("update_run_usage", error));
        assert_run_touched(changed, run_id);
    }

    fn put_actor(&self, record: ActorRecord) {
        let now = now_ms();
        let conn = self.lock();
        conn.execute(
            PUT_ACTOR_SQL,
            params![
                record.run_id,
                record.site_id,
                record.ordinal as i64,
                record.name,
                encode_json(record.persona.as_ref(), "dwf_actor.persona_json"),
                record.session_id,
                record.resolved_model,
                now,
                now,
            ],
        )
        .unwrap_or_else(|error| fail("put_actor", error));
    }

    fn get_actor(&self, run_id: &str, site_id: &str, ordinal: u32) -> Option<ActorRecord> {
        let conn = self.lock();
        conn.query_row(
            GET_ACTOR_SQL,
            params![run_id, site_id, ordinal as i64],
            decode_actor,
        )
        .optional()
        .unwrap_or_else(|error| fail("get_actor", error))
    }

    fn list_actors(&self, run_id: &str) -> Vec<ActorRecord> {
        let conn = self.lock();
        let mut statement = conn
            .prepare(LIST_ACTORS_SQL)
            .unwrap_or_else(|error| fail("list_actors", error));
        let rows = statement
            .query_map([run_id], decode_actor)
            .unwrap_or_else(|error| fail("list_actors", error));
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .unwrap_or_else(|error| fail("list_actors", error))
    }

    fn put_node(&self, record: NodeRecord) {
        // Admission (running) → settlement → stats backfill all go through the same upsert, keyed by (run_id, site_id, ordinal);
        // the upsert replaces the whole row, so a backfiller must getNode first and then spread and rewrite.
        let now = now_ms();
        let conn = self.lock();
        conn.execute(
            PUT_NODE_SQL,
            params![
                record.run_id,
                record.site_id,
                record.ordinal as i64,
                node_kind_str(record.kind),
                record.actor_site_id,
                record.actor_ordinal.map(i64::from),
                record.actor_seq.map(|value| value as i64),
                record.input_hash,
                node_status_str(record.status),
                encode_result_json(record.result.as_ref()),
                encode_json(record.error.as_ref(), "dwf_node.error_json"),
                encode_json(record.stats.as_ref(), "dwf_node.stats_json"),
                record.message_boundary.map(|value| value as i64),
                record.artifact_id,
                encode_json(record.input.as_ref(), "dwf_node.input_json"),
                now,
                now,
            ],
        )
        .unwrap_or_else(|error| fail("put_node", error));
    }

    fn get_node(&self, run_id: &str, site_id: &str, ordinal: u32) -> Option<NodeRecord> {
        let conn = self.lock();
        conn.query_row(
            GET_NODE_SQL,
            params![run_id, site_id, ordinal as i64],
            decode_node,
        )
        .optional()
        .unwrap_or_else(|error| fail("get_node", error))
    }

    fn list_nodes(&self, run_id: &str) -> Vec<NodeRecord> {
        let conn = self.lock();
        let mut statement = conn
            .prepare(LIST_NODES_SQL)
            .unwrap_or_else(|error| fail("list_nodes", error));
        let rows = statement
            .query_map([run_id], decode_node)
            .unwrap_or_else(|error| fail("list_nodes", error));
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .unwrap_or_else(|error| fail("list_nodes", error))
    }

    fn append_event(&self, run_id: &str, event: &RunEvent) -> StoredEvent {
        // The instant written into time_created must be handed back to the caller verbatim: decode_event gives the same
        // number when the row is read back, so if the append path could not obtain it we would get "a just-written event has no timestamp, only the read-back one does".
        let time_created = now_ms();
        let payload = serde_json::to_string(event)
            .unwrap_or_else(|error| panic!("dwf journal: cannot encode event: {error}"));
        let conn = self.lock();
        let sequence: i64 = conn
            .query_row(
                APPEND_EVENT_SQL,
                params![run_id, run_id, event.type_name(), payload, time_created],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| fail("append_event", error));
        StoredEvent {
            sequence: sequence as u64,
            event: event.clone(),
            time_created: Some(time_created as f64),
        }
    }

    /// Push both cursor and limit down into SQL: fetching everything and then slicing would read the whole journal into memory once per page.
    /// An unknown run / out-of-range cursor naturally returns empty.
    fn list_events(&self, run_id: &str, opts: ListEventsOptions) -> Vec<StoredEvent> {
        let limit = opts.limit.map(|limit| limit as i64).unwrap_or(-1);
        let conn = self.lock();
        let sql = match opts.after_sequence {
            Some(_) => LIST_EVENTS_AFTER_SQL,
            None => LIST_EVENTS_SQL,
        };
        let mut statement = conn
            .prepare(sql)
            .unwrap_or_else(|error| fail("list_events", error));
        let rows = match opts.after_sequence {
            Some(after) => statement.query_map(params![run_id, after as i64, limit], decode_event),
            None => statement.query_map(params![run_id, limit], decode_event),
        }
        .unwrap_or_else(|error| fail("list_events", error));
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .unwrap_or_else(|error| fail("list_events", error))
    }
}

// ————————————————————————————————————————————————————————————————
// Tests
// ————————————————————————————————————————————————————————————————

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal_memory::InMemoryJournalStore;
    use crate::types::{
        ActorRef, AskStats, InstanceRef, PersonaSpec, ProviderStopDetails, ProviderStopKind,
        Violation, WorkflowErrorMismatch, WorldReadInput,
    };
    use serde_json::json;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn open_temp() -> (tempfile::TempDir, SqliteJournal) {
        let dir = tempfile::tempdir().expect("temp dir");
        let journal = SqliteJournal::open(dir.path().join("dwf.sqlite")).expect("open journal");
        (dir, journal)
    }

    fn pending_run(run_id: &str) -> RunRecord {
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
            status: RunStatus::Pending,
            stop_reason: None,
            superseded_by: None,
            failure: None,
            result: None,
        }
    }

    fn sample_error(code: WorkflowErrorCode, message: &str) -> WorkflowErrorJson {
        WorkflowErrorJson {
            code,
            message: message.into(),
            violations: Some(vec![Violation {
                path: "$.answer".into(),
                expected: "string".into(),
                got: "null".into(),
            }]),
            final_text: Some("partial answer".into()),
            mismatch: Some(WorkflowErrorMismatch {
                expected: "a".into(),
                got: "b".into(),
            }),
            provider_stop: Some(ProviderStopDetails {
                kind: ProviderStopKind::Quota,
                reason: "quota exhausted".into(),
                provider_id: Some("vendor".into()),
                provider_label: Some("Vendor".into()),
                model_id: Some("model-a".into()),
                provider_code: Some("429".into()),
                subagent: None,
                subagent_name: None,
                phase: Some("dispatch".into()),
                raw_message: Some("slow down".into()),
                reset_at: Some(1_700_000_000_000.0),
            }),
        }
    }

    fn node(run_id: &str, site_id: &str, ordinal: u32, kind: NodeKind) -> NodeRecord {
        NodeRecord {
            run_id: run_id.into(),
            site_id: site_id.into(),
            ordinal,
            kind,
            actor_site_id: None,
            actor_ordinal: None,
            actor_seq: None,
            input_hash: "input-hash".into(),
            status: NodeRecordStatus::Running,
            result: None,
            error: None,
            stats: None,
            artifact_id: None,
            input: None,
            message_boundary: None,
        }
    }

    fn actor(run_id: &str, site_id: &str, ordinal: u32) -> ActorRecord {
        ActorRecord {
            run_id: run_id.into(),
            site_id: site_id.into(),
            ordinal,
            name: None,
            persona: None,
            session_id: None,
            resolved_model: None,
        }
    }

    fn column(journal: &SqliteJournal, run_id: &str, column: &str) -> Option<String> {
        journal
            .lock()
            .query_row(
                &format!("select {column} from dwf_run where id = ?"),
                [run_id],
                |row| row.get(0),
            )
            .optional()
            .expect("read column")
    }

    fn physical_status(journal: &SqliteJournal, run_id: &str) -> String {
        column(journal, run_id, "status").expect("status column")
    }

    fn raw_failure_json(journal: &SqliteJournal, run_id: &str) -> Option<Value> {
        column(journal, run_id, "failure_json").map(|text| serde_json::from_str(&text).unwrap())
    }

    fn insert_raw_run(
        journal: &SqliteJournal,
        run_id: &str,
        physical: &str,
        failure_json: Option<&str>,
    ) {
        journal
            .lock()
            .execute(
                "insert into dwf_run (
                   id, caps_max_concurrency, spent_tokens, status, failure_json,
                   time_created, time_updated
                 ) values (?, 1, 0, ?, ?, 1, 1)",
                params![run_id, physical, failure_json],
            )
            .expect("raw run insert");
    }

    #[test]
    fn migrate_is_idempotent_and_creates_the_ts_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dwf.sqlite");
        drop(SqliteJournal::open(&path).unwrap());
        // The second open re-runs the same DDL on the same file: no error, no lost data.
        let journal = SqliteJournal::open(&path).unwrap();
        journal.create_run(pending_run("run-after-reopen"));
        drop(journal);
        let journal = SqliteJournal::open(&path).unwrap();
        assert!(journal.get_run("run-after-reopen").is_some());
        let tables: Vec<String> = {
            let conn = journal.lock();
            let mut statement = conn
                .prepare(
                    "select name from sqlite_master where type = 'table' and name like 'dwf_%' order by name",
                )
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            tables,
            vec!["dwf_actor", "dwf_event", "dwf_node", "dwf_run"]
        );
        // The unique constraints and indexes are part of the physical contract.
        let unique_indexes: i64 = journal
            .lock()
            .query_row(
                "select count(*) from sqlite_master where type = 'index' and name in
                   ('dwf_run_cwd_idx', 'dwf_actor_run_idx', 'dwf_node_run_idx',
                    'dwf_node_artifact_idx', 'dwf_event_artifact_idx')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unique_indexes, 5);

        // The connection pragmas really took effect (a mistyped pragma fails silently).
        let conn = journal.lock();
        let journal_mode: String = conn
            .query_row("pragma journal_mode", [], |row| row.get(0))
            .unwrap();
        let synchronous: i64 = conn
            .query_row("pragma synchronous", [], |row| row.get(0))
            .unwrap();
        let foreign_keys: i64 = conn
            .query_row("pragma foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        assert_eq!(synchronous, 1, "synchronous = NORMAL (1)");
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn run_round_trip_covers_every_optional_column_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dwf.sqlite");
        let record = RunRecord {
            run_id: "run-full".into(),
            parent_session_id: Some("session-1".into()),
            cwd: Some("G:\\work".into()),
            name: Some("nightly-report".into()),
            tool_call_id: Some("call-7".into()),
            script_text: Some("export default async function main() { return 1; }".into()),
            script_hash: Some("3d1c0f9a".into()),
            args: Some(json!({"topic": "release", "deep": true})),
            resumed_from: Some("run-prev".into()),
            caps: Caps { max_concurrency: 4 },
            spent_tokens: 1234,
            status: RunStatus::Stopped,
            stop_reason: Some(RunStopReason::Superseded),
            superseded_by: Some("run-next".into()),
            failure: Some(sample_error(
                WorkflowErrorCode::ProviderStop,
                "provider quota exhausted",
            )),
            result: Some(json!({"nested": [1, null, "x"]})),
        };
        // `result: null` is a legal artifact: only absence maps to SQL NULL.
        let null_result = RunRecord {
            run_id: "run-null-result".into(),
            status: RunStatus::Pending,
            result: Some(Value::Null),
            ..pending_run("run-null-result")
        };
        {
            let journal = SqliteJournal::open(&path).unwrap();
            journal.create_run(record.clone());
            assert_eq!(journal.get_run(&record.run_id), Some(record.clone()));
            journal.create_run(null_result.clone());
            assert_eq!(
                journal.get_run("run-null-result"),
                Some(null_result.clone())
            );
            assert_eq!(journal.get_run("run-missing"), None);
        }
        let reopened = SqliteJournal::open(&path).unwrap();
        assert_eq!(reopened.get_run(&record.run_id), Some(record));
        assert_eq!(reopened.get_run("run-null-result"), Some(null_result));
    }

    #[test]
    fn status_codec_maps_all_five_logical_values_both_ways() {
        let (_dir, journal) = open_temp();

        journal.create_run(pending_run("s-pending"));
        assert_eq!(physical_status(&journal, "s-pending"), "pending");
        assert_eq!(
            journal.get_run("s-pending").unwrap().status,
            RunStatus::Pending
        );

        journal.update_run_status("s-pending", RunStatus::Running, None);
        assert_eq!(physical_status(&journal, "s-pending"), "running");
        let running = journal.get_run("s-pending").unwrap();
        assert_eq!(running.status, RunStatus::Running);
        assert!(running.stop_reason.is_none());

        let completed_settlement = RunSettlementRecord {
            result: Some(json!({"answer": 42})),
            ..Default::default()
        };
        journal.update_run_status(
            "s-pending",
            RunStatus::Completed,
            Some(&completed_settlement),
        );
        assert_eq!(physical_status(&journal, "s-pending"), "completed");
        let completed = journal.get_run("s-pending").unwrap();
        assert_eq!(completed.status, RunStatus::Completed);
        assert_eq!(completed.result, Some(json!({"answer": 42})));

        // errored ↔ failed: what is written is a bare WorkflowErrorJson (no stopReason envelope).
        let failure = sample_error(WorkflowErrorCode::DriverError, "driver exploded");
        journal.create_run(pending_run("s-errored"));
        let errored_settlement = RunSettlementRecord {
            failure: Some(failure.clone()),
            ..Default::default()
        };
        journal.update_run_status("s-errored", RunStatus::Errored, Some(&errored_settlement));
        assert_eq!(physical_status(&journal, "s-errored"), "failed");
        let raw = raw_failure_json(&journal, "s-errored").unwrap();
        assert_eq!(raw["code"], json!("DriverError"));
        assert!(
            raw.get("stopReason").is_none(),
            "errored writes the bare error"
        );
        let errored = journal.get_run("s-errored").unwrap();
        assert_eq!(errored.status, RunStatus::Errored);
        assert_eq!(errored.failure, Some(failure));

        // stopped ↔ cancelled: the envelope carries stopReason / supersededBy / error.
        journal.create_run(pending_run("s-stopped"));
        let provider_stop = sample_error(WorkflowErrorCode::ProviderStop, "quota");
        let stopped_settlement = RunSettlementRecord {
            stop_reason: Some(RunStopReason::Provider),
            superseded_by: Some("run-2".into()),
            failure: Some(provider_stop.clone()),
            result: None,
        };
        journal.update_run_status("s-stopped", RunStatus::Stopped, Some(&stopped_settlement));
        assert_eq!(physical_status(&journal, "s-stopped"), "cancelled");
        let raw = raw_failure_json(&journal, "s-stopped").unwrap();
        assert_eq!(raw["stopReason"], json!("provider"));
        assert_eq!(raw["supersededBy"], json!("run-2"));
        assert_eq!(raw["error"]["code"], json!("ProviderStop"));
        let stopped = journal.get_run("s-stopped").unwrap();
        assert_eq!(stopped.status, RunStatus::Stopped);
        assert_eq!(stopped.stop_reason, Some(RunStopReason::Provider));
        assert_eq!(stopped.superseded_by.as_deref(), Some("run-2"));
        assert_eq!(stopped.failure, Some(provider_stop));

        // When the settlement bag is absent the envelope still writes user (the terminal-state encoding on the TS SQLite side).
        journal.create_run(pending_run("s-stopped-default"));
        journal.update_run_status("s-stopped-default", RunStatus::Stopped, None);
        assert_eq!(physical_status(&journal, "s-stopped-default"), "cancelled");
        let raw = raw_failure_json(&journal, "s-stopped-default").unwrap();
        assert_eq!(raw["stopReason"], json!("user"));
        let decoded = journal.get_run("s-stopped-default").unwrap();
        assert_eq!(decoded.status, RunStatus::Stopped);
        assert_eq!(decoded.stop_reason, Some(RunStopReason::User));
        assert!(decoded.failure.is_none());
    }

    #[test]
    fn legacy_physical_rows_decode_like_the_ts_codec() {
        let (_dir, journal) = open_temp();

        // An old orphan-reconciliation row: failed + Interrupted ⇒ stopped(interrupted).
        insert_raw_run(
            &journal,
            "legacy-interrupted",
            "failed",
            Some(r#"{"code":"Interrupted","message":"killed by orphan sweep"}"#),
        );
        let back = journal.get_run("legacy-interrupted").unwrap();
        assert_eq!(back.status, RunStatus::Stopped);
        assert_eq!(back.stop_reason, Some(RunStopReason::Interrupted));
        assert_eq!(
            back.failure.as_ref().map(|failure| failure.code),
            Some(WorkflowErrorCode::Interrupted)
        );

        // A historical cancelled row has no envelope (including those stopped by TaskStop): always decoded as user.
        insert_raw_run(&journal, "legacy-cancelled", "cancelled", None);
        let back = journal.get_run("legacy-cancelled").unwrap();
        assert_eq!(back.status, RunStatus::Stopped);
        assert_eq!(back.stop_reason, Some(RunStopReason::User));
        assert!(back.failure.is_none());

        // A failure_json that is not an envelope must not masquerade as a reason.
        insert_raw_run(
            &journal,
            "legacy-cancelled-other",
            "cancelled",
            Some(r#"{"code":"DriverError","message":"x"}"#),
        );
        let back = journal.get_run("legacy-cancelled-other").unwrap();
        assert_eq!(back.status, RunStatus::Stopped);
        assert_eq!(back.stop_reason, Some(RunStopReason::User));
        assert!(back.failure.is_none());

        // When supersededBy is missing from the envelope, that key is not written out.
        insert_raw_run(
            &journal,
            "legacy-cancelled-envelope",
            "cancelled",
            Some(r#"{"stopReason":"model"}"#),
        );
        let back = journal.get_run("legacy-cancelled-envelope").unwrap();
        assert_eq!(back.stop_reason, Some(RunStopReason::Model));
        assert!(back.superseded_by.is_none());
    }

    #[test]
    fn events_get_monotonic_sequences_and_sql_pagination() {
        let (_dir, journal) = open_temp();
        journal.create_run(pending_run("run-events"));

        let stored: Vec<StoredEvent> = ["a", "b", "c", "d"]
            .iter()
            .map(|message| {
                journal.append_event(
                    "run-events",
                    &RunEvent::Log {
                        message: (*message).into(),
                    },
                )
            })
            .collect();
        // Same base as TS: coalesce(max(sequence) + 1, 0).
        assert_eq!(
            stored
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert!(stored.iter().all(|event| event
            .time_created
            .is_some_and(|time| time > 1_600_000_000_000.0)));

        let all = journal.list_events("run-events", ListEventsOptions::default());
        assert_eq!(all, stored, "append return value matches the read-back row");

        let page = journal.list_events(
            "run-events",
            ListEventsOptions {
                after_sequence: Some(1),
                limit: Some(2),
            },
        );
        assert_eq!(page, all[2..4].to_vec(), "cursor is strictly greater");

        assert!(journal
            .list_events(
                "run-events",
                ListEventsOptions {
                    after_sequence: None,
                    limit: Some(0),
                },
            )
            .is_empty());
        // Callers use limit + 1 as the hasMore probe: the storage layer must not add its own ceiling.
        let probe = journal.list_events(
            "run-events",
            ListEventsOptions {
                after_sequence: None,
                limit: Some(5),
            },
        );
        assert_eq!(probe.len(), 4);
        assert!(journal
            .list_events(
                "run-events",
                ListEventsOptions {
                    after_sequence: Some(99),
                    limit: None,
                },
            )
            .is_empty());
        assert!(journal
            .list_events("run-missing", ListEventsOptions::default())
            .is_empty());
    }

    #[test]
    fn node_upsert_covers_every_kind_and_json_columns() {
        let (_dir, journal) = open_temp();
        journal.create_run(pending_run("run-nodes"));

        let mut artifact = node("run-nodes", "artifact#1", 1, NodeKind::Artifact);
        artifact.status = NodeRecordStatus::Completed;
        artifact.artifact_id = Some("chart-1".into());
        artifact.result = Some(json!({"id": "chart-1", "kind": "chart", "version": 1}));
        journal.put_node(artifact.clone());

        let mut ask = node("run-nodes", "ask#1", 1, NodeKind::Ask);
        ask.actor_site_id = Some("actor#1".into());
        ask.actor_ordinal = Some(2);
        ask.actor_seq = Some(0);
        journal.put_node(ask.clone());

        let mut report = node("run-nodes", "report#1", 1, NodeKind::Report);
        report.status = NodeRecordStatus::Completed;
        report.artifact_id = Some("chart-1".into());
        report.result = Some(json!({"item": "row-1"}));
        journal.put_node(report.clone());

        let mut world = node("run-nodes", "world#1", 1, NodeKind::WorldRead);
        world.input = Some(WorldReadInput {
            op: "grep".into(),
            args: vec![json!("needle"), json!("src")],
            truncated: Some(true),
        });
        journal.put_node(world.clone());

        let mut run_world = node("run-nodes", "world#2", 1, NodeKind::WorldRun);
        run_world.status = NodeRecordStatus::Completed;
        run_world.result = Some(json!({"exitCode": 0, "stdout": "ok", "stderr": ""}));
        journal.put_node(run_world.clone());

        for record in [&artifact, &ask, &report, &world, &run_world] {
            assert_eq!(
                journal.get_node("run-nodes", &record.site_id, record.ordinal),
                Some(record.clone())
            );
        }
        // The read order is the persistence order (order by id).
        assert_eq!(
            journal.list_nodes("run-nodes"),
            vec![artifact, ask.clone(), report, world, run_world]
        );

        // Admission → settlement → stats backfill go through the same upsert, replacing the whole row (including message_boundary: 0).
        ask.status = NodeRecordStatus::Completed;
        ask.result = Some(json!({"answer": 42}));
        ask.stats = Some(AskStats {
            tokens: 120,
            tool_calls: 3,
            turns: 2,
            world_tool_calls: Some(1),
        });
        ask.message_boundary = Some(0);
        journal.put_node(ask.clone());
        assert_eq!(journal.get_node("run-nodes", "ask#1", 1), Some(ask));

        // A failed node: error_json goes through the WorkflowErrorJson codec.
        let mut failed = node("run-nodes", "ask#2", 1, NodeKind::Ask);
        failed.status = NodeRecordStatus::Failed;
        failed.error = Some(sample_error(
            WorkflowErrorCode::ValidationFailed,
            "answer did not match",
        ));
        journal.put_node(failed.clone());
        assert_eq!(journal.get_node("run-nodes", "ask#2", 1), Some(failed));

        assert_eq!(journal.get_node("run-nodes", "ask#9", 1), None);
        assert!(journal.list_nodes("run-missing").is_empty());
    }

    #[test]
    fn actor_upsert_keeps_session_and_resolved_model() {
        let (_dir, journal) = open_temp();
        journal.create_run(pending_run("run-actors"));

        let mut planner = actor("run-actors", "actor#1", 1);
        planner.name = Some("planner".into());
        planner.persona = Some(PersonaSpec {
            name: Some("Planner".into()),
            system: Some("be terse".into()),
        });
        planner.session_id = Some("session-abc".into());
        planner.resolved_model = Some("vendor/model-a".into());
        journal.put_actor(planner.clone());
        assert_eq!(
            journal.get_actor("run-actors", "actor#1", 1),
            Some(planner.clone())
        );

        let mut writer = actor("run-actors", "actor#2", 1);
        writer.name = Some("writer".into());
        journal.put_actor(writer.clone());

        // The engine's repeated putActor spreads the original values through: driver-owned columns are preserved verbatim.
        planner.name = Some("planner-renamed".into());
        journal.put_actor(planner.clone());
        let back = journal.get_actor("run-actors", "actor#1", 1).unwrap();
        assert_eq!(back.name.as_deref(), Some("planner-renamed"));
        assert_eq!(back.session_id.as_deref(), Some("session-abc"));
        assert_eq!(back.resolved_model.as_deref(), Some("vendor/model-a"));
        assert_eq!(back.persona, planner.persona);

        assert_eq!(journal.list_actors("run-actors"), vec![planner, writer]);
        assert_eq!(journal.get_actor("run-actors", "actor#1", 2), None);
        assert!(journal.list_actors("run-missing").is_empty());
    }

    #[test]
    fn settlement_result_survives_later_status_writes() {
        let (_dir, journal) = open_temp();
        journal.create_run(pending_run("run-settle"));

        let settlement = RunSettlementRecord {
            result: Some(json!({"answer": 42})),
            ..Default::default()
        };
        journal.update_run_status("run-settle", RunStatus::Completed, Some(&settlement));
        assert_eq!(
            journal.get_run("run-settle").unwrap().result,
            Some(json!({"answer": 42}))
        );

        // A repeated settlement carrying no artifact does not erase it (coalesce).
        journal.update_run_status("run-settle", RunStatus::Completed, None);
        assert_eq!(
            journal.get_run("run-settle").unwrap().result,
            Some(json!({"answer": 42}))
        );

        // resume flips the run back to running: the previous generation's failure / result must be cleared.
        journal.update_run_status("run-settle", RunStatus::Running, None);
        let running = journal.get_run("run-settle").unwrap();
        assert_eq!(running.status, RunStatus::Running);
        assert!(running.result.is_none());
        assert!(running.failure.is_none());

        // The new terminal state writes the artifact and failure again.
        let failure = sample_error(WorkflowErrorCode::DriverError, "boom");
        let settlement = RunSettlementRecord {
            failure: Some(failure.clone()),
            result: Some(json!({"answer": 1})),
            ..Default::default()
        };
        journal.update_run_status("run-settle", RunStatus::Errored, Some(&settlement));
        let errored = journal.get_run("run-settle").unwrap();
        assert_eq!(errored.status, RunStatus::Errored);
        assert_eq!(errored.failure, Some(failure));
        assert_eq!(errored.result, Some(json!({"answer": 1})));

        // The terminal state's failure is a whole-column rewrite: when the same run settles as completed again, the previous generation's failure is not kept.
        journal.update_run_status("run-settle", RunStatus::Completed, None);
        let completed = journal.get_run("run-settle").unwrap();
        assert_eq!(completed.status, RunStatus::Completed);
        assert!(completed.failure.is_none());
    }

    #[test]
    fn usage_update_requires_a_known_run_and_touches_nothing_else() {
        let (_dir, journal) = open_temp();
        let failure = sample_error(WorkflowErrorCode::ProviderStop, "quota");
        let mut record = pending_run("run-usage");
        record.status = RunStatus::Stopped;
        record.stop_reason = Some(RunStopReason::Provider);
        record.failure = Some(failure.clone());
        record.result = Some(json!({"kept": true}));
        journal.create_run(record);

        journal.update_run_usage("run-usage", 4096);
        let back = journal.get_run("run-usage").unwrap();
        assert_eq!(back.spent_tokens, 4096);
        assert_eq!(back.status, RunStatus::Stopped);
        assert_eq!(back.stop_reason, Some(RunStopReason::Provider));
        assert_eq!(back.failure, Some(failure));
        assert_eq!(back.result, Some(json!({"kept": true})));

        let unknown_usage = catch_unwind(AssertUnwindSafe(|| journal.update_run_usage("nope", 1)));
        assert!(unknown_usage.is_err(), "unknown run must fail loudly");
    }

    #[test]
    fn writes_on_unknown_runs_fail_loudly() {
        let (_dir_a, journal_a) = open_temp();
        let unknown_status = catch_unwind(AssertUnwindSafe(|| {
            journal_a.update_run_status("missing", RunStatus::Running, None);
        }));
        assert!(unknown_status.is_err());

        // foreign_keys = ON: an orphan actor is blocked by the foreign key (TS's runner enables it the same way).
        let (_dir_b, journal_b) = open_temp();
        let orphan_actor = catch_unwind(AssertUnwindSafe(|| {
            journal_b.put_actor(actor("missing", "actor#1", 1))
        }));
        assert!(orphan_actor.is_err(), "orphan actor must be rejected");
    }

    #[test]
    fn create_run_rejects_a_duplicate_run_id() {
        let (_dir, journal) = open_temp();
        journal.create_run(pending_run("run-dup"));
        let duplicate = catch_unwind(AssertUnwindSafe(|| {
            journal.create_run(pending_run("run-dup"))
        }));
        assert!(duplicate.is_err(), "duplicate run ids are a contract error");
    }

    // ————————————————————————————————————————————————————————————————
    // Cross-implementation contract: the same scenario must decode to the same records on the in-memory and the SQLite implementation
    // ————————————————————————————————————————————————————————————————

    struct ContractSnapshot {
        run: RunRecord,
        actors: Vec<ActorRecord>,
        nodes: Vec<NodeRecord>,
        events: Vec<StoredEvent>,
        page: Vec<StoredEvent>,
    }

    fn contract_scenario(journal: &dyn JournalStorePort) -> ContractSnapshot {
        let run_id = "run-contract";
        journal.create_run(RunRecord {
            run_id: run_id.into(),
            parent_session_id: Some("session-9".into()),
            cwd: Some("/repo".into()),
            name: Some("contract".into()),
            tool_call_id: Some("call-1".into()),
            script_text: Some("return 42;".into()),
            script_hash: Some("deadbeef".into()),
            args: Some(json!({"mode": "fast"})),
            resumed_from: None,
            caps: Caps { max_concurrency: 4 },
            spent_tokens: 0,
            status: RunStatus::Running,
            stop_reason: None,
            superseded_by: None,
            failure: None,
            result: None,
        });

        let mut planner = actor(run_id, "actor#1", 1);
        planner.name = Some("planner".into());
        planner.persona = Some(PersonaSpec {
            name: Some("Planner".into()),
            system: Some("think first".into()),
        });
        planner.session_id = Some("session-abc".into());
        planner.resolved_model = Some("vendor/model-a".into());
        journal.put_actor(planner);
        let mut writer = actor(run_id, "actor#2", 1);
        writer.name = Some("writer".into());
        journal.put_actor(writer);

        // Insertion order == (siteId, ordinal) order: the in-memory implementation sorts by key and SQLite by persistence order,
        // so both read surfaces are comparable row by row on this scenario.
        let mut artifact = node(run_id, "artifact#1", 1, NodeKind::Artifact);
        artifact.status = NodeRecordStatus::Completed;
        artifact.artifact_id = Some("chart-1".into());
        artifact.result = Some(json!({"id": "chart-1", "kind": "chart", "version": 1}));
        journal.put_node(artifact);

        let mut ask = node(run_id, "ask#1", 1, NodeKind::Ask);
        ask.actor_site_id = Some("actor#1".into());
        ask.actor_ordinal = Some(1);
        ask.actor_seq = Some(0);
        journal.put_node(ask.clone());

        let mut report = node(run_id, "report#1", 1, NodeKind::Report);
        report.status = NodeRecordStatus::Completed;
        report.artifact_id = Some("chart-1".into());
        report.result = Some(json!({"item": "row-1"}));
        journal.put_node(report);

        let mut world = node(run_id, "world#1", 1, NodeKind::WorldRead);
        world.input = Some(WorldReadInput {
            op: "read".into(),
            args: vec![json!("README.md")],
            truncated: None,
        });
        journal.put_node(world);

        ask.status = NodeRecordStatus::Completed;
        ask.result = Some(json!({"answer": 42}));
        ask.stats = Some(AskStats {
            tokens: 64,
            tool_calls: 2,
            turns: 1,
            world_tool_calls: Some(1),
        });
        ask.message_boundary = Some(0);
        journal.put_node(ask);

        journal.update_run_usage(run_id, 777);

        journal.append_event(
            run_id,
            &RunEvent::RunStarted {
                run_id: run_id.into(),
                caps: Caps { max_concurrency: 4 },
            },
        );
        journal.append_event(
            run_id,
            &RunEvent::NodeQueued {
                instance: InstanceRef {
                    site_id: "ask#1".into(),
                    ordinal: 1,
                },
                kind: NodeKind::Ask,
                actor: Some(ActorRef {
                    site_id: "actor#1".into(),
                    ordinal: 1,
                }),
                actor_seq: Some(0),
                phase_name: Some("solve".into()),
                instructions_head: Some("answer the question".into()),
            },
        );
        journal.append_event(run_id, &RunEvent::UsageUpdated { spent_tokens: 777 });
        journal.append_event(
            run_id,
            &RunEvent::Log {
                message: "settling".into(),
            },
        );
        journal.append_event(
            run_id,
            &RunEvent::RunSettled {
                status: RunStatus::Completed,
                stop_reason: None,
                superseded_by: None,
                error: None,
            },
        );

        let settlement = RunSettlementRecord {
            result: Some(json!({"answer": 42})),
            ..Default::default()
        };
        journal.update_run_status(run_id, RunStatus::Completed, Some(&settlement));

        let events = journal.list_events(run_id, ListEventsOptions::default());
        let cursor = events.first().map(|event| event.sequence).unwrap_or(0) + 1;
        let page = journal.list_events(
            run_id,
            ListEventsOptions {
                after_sequence: Some(cursor),
                limit: Some(2),
            },
        );
        ContractSnapshot {
            run: journal.get_run(run_id).expect("run present"),
            actors: journal.list_actors(run_id),
            nodes: journal.list_nodes(run_id),
            events,
            page,
        }
    }

    #[test]
    fn sqlite_matches_the_in_memory_journal() {
        let (_dir, sqlite) = open_temp();
        let memory = InMemoryJournalStore::new();
        let from_sqlite = contract_scenario(&sqlite);
        let from_memory = contract_scenario(&memory);

        assert_eq!(from_sqlite.run, from_memory.run);
        assert_eq!(from_sqlite.run.status, RunStatus::Completed);
        assert_eq!(from_sqlite.run.result, Some(json!({"answer": 42})));
        assert_eq!(from_sqlite.run.spent_tokens, 777);
        assert_eq!(from_sqlite.actors, from_memory.actors);
        assert_eq!(from_sqlite.nodes, from_memory.nodes);
        assert_eq!(from_sqlite.events.len(), from_memory.events.len());
        assert_eq!(from_sqlite.page.len(), from_memory.page.len());
        assert_eq!(from_sqlite.page.len(), 2);

        // Events identical row by row. The only known and quarantined difference is the sequence base: the TS SQLite side allocates from 0
        // (coalesce(max(sequence) + 1, 0)), while this crate's in-memory implementation starts at 1; both are monotonic, so
        // cross-runtime reads are unaffected. The base difference is reported separately (a fix would belong to journal_memory.rs).
        let sqlite_base = from_sqlite.events.first().expect("events").sequence;
        let memory_base = from_memory.events.first().expect("events").sequence;
        for (sqlite_event, memory_event) in from_sqlite.events.iter().zip(&from_memory.events) {
            assert_eq!(
                sqlite_event.sequence - sqlite_base,
                memory_event.sequence - memory_base
            );
            assert_eq!(sqlite_event.event, memory_event.event);
            assert!(sqlite_event.time_created.is_some());
            assert!(memory_event.time_created.is_some());
        }
        for (sqlite_event, memory_event) in from_sqlite.page.iter().zip(&from_memory.page) {
            assert_eq!(
                sqlite_event.sequence - sqlite_base,
                memory_event.sequence - memory_base
            );
            assert_eq!(sqlite_event.event, memory_event.event);
        }
    }
}
