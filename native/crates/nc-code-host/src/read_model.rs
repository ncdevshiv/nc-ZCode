//! Tier 3 read model: materializes the conversation projection rows **at write time**, so opening a session only takes a window.
//!
//! Why it is needed: the JSONL transcript is a write-ahead log, so reading it must replay the whole file;
//! the open cost grows with the **number of writes**, not with the **size of the session**. This module lands the rows
//! into `conversation_row`, and `window()` uses the `(session_id, row_id desc)` index, so the
//! cost is always O(limit).
//!
//! Division of labor with the transcript (migration order in `specs/large-session-scaling.md`):
//! 1. **Dual write**: the transcript is still the authority and this table is only a side copy → use [`Self::verify_against`]
//! to check the two are byte-identical;
//! 2. once the check passes, switch the read paths over to this table;
//! 3. only last do we weaken the transcript's read role; it keeps carrying the append-only crash safety.
//!
//! Before step 1 is finished, **do not** let any read path depend on this table.

use std::path::Path;

use rusqlite::{params, Connection};
use serde_json::Value;

const SCHEMA: &str = r#"
      create table if not exists conversation_row (
        session_id text not null,
        row_id integer not null,
        data text not null,
        updated_at integer not null,
        primary key (session_id, row_id)
      );

      create index if not exists conversation_row_tail
      on conversation_row (session_id, row_id desc);

      -- Tier 3 Phase 2: Snapshot of the entire transcript.
      -- Row table can only replace `rows`, and `load_session` also needs history / acks / wheel anchor point /
      -- File Ledger - These are also from replays. Therefore, the cut and read must be "the entire snapshot" rather than "only the rows".
      -- transcript_len is the number of transcript bytes when the snapshot is downloaded: if the file length changes, it means
      -- If it is appended later, the snapshot becomes invalid and the caller must roll back and replay. Use length instead of timestamp,
      -- Avoid false freshness caused by writes in the same millisecond.
      create table if not exists conversation_turn_starts (
        session_id text primary key,
        payload    text not null,
        updated_at integer not null
      );

      create table if not exists conversation_history (
        session_id text primary key,
        payload    text not null,
        updated_at integer not null
      );

      create table if not exists conversation_snapshot (
        session_id     text primary key,
        payload        text not null,
        transcript_len integer not null,
        row_count      integer not null default 0,
        updated_at     integer not null
      );
"#;

pub struct ConversationRows {
    conn: Connection,
}

// TranscriptStore derives Debug, so the read model, as its optional field, has to be Debug too;
// the connection itself has no meaningful debug output, so only the type name is reported.
impl std::fmt::Debug for ConversationRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversationRows").finish_non_exhaustive()
    }
}

impl ConversationRows {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000i64)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Write/overwrite one row. Same semantics as the transcript's `row`/`upsert`: overwrite by rowId.
    pub fn upsert(&self, session_id: &str, row: &Value, now_ms: i64) -> rusqlite::Result<()> {
        let Some(row_id) = row.get("rowId").and_then(Value::as_u64) else {
            return Ok(());
        };
        self.conn.execute(
            "insert into conversation_row (session_id, row_id, data, updated_at)
             values (?1, ?2, ?3, ?4)
             on conflict(session_id, row_id) do update set data = excluded.data,
                                                          updated_at = excluded.updated_at",
            params![session_id, row_id as i64, row.to_string(), now_ms],
        )?;
        Ok(())
    }

    /// Branch truncation: drops `from_row_id` and every row after it (same meaning as `row.removed{fromRowId}`).
    pub fn truncate_from(&self, session_id: &str, from_row_id: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "delete from conversation_row where session_id = ?1 and row_id >= ?2",
            params![session_id, from_row_id as i64],
        )?;
        Ok(())
    }

    /// The tail window, returned in ascending order (matching the order of `session.rows`). Cost O(limit).
    pub fn window(&self, session_id: &str, limit: usize) -> rusqlite::Result<Vec<Value>> {
        let mut statement = self.conn.prepare(
            "select data from conversation_row
             where session_id = ?1
             order by row_id desc
             limit ?2",
        )?;
        let rows = statement.query_map(params![session_id, limit as i64], |row| {
            let data: String = row.get(0)?;
            Ok(serde_json::from_str::<Value>(&data).unwrap_or(Value::Null))
        })?;
        let mut collected: Vec<Value> = rows.filter_map(std::result::Result::ok).collect();
        collected.reverse();
        Ok(collected)
    }

    /// Write the session snapshot. `transcript_len` is the transcript's byte count at the moment of writing;
    /// on read-back it is used to decide whether the snapshot is still fresh.
    ///
    /// **Does not contain rows**: the rows already live in `conversation_row`, so the snapshot stores only history / acks /
    /// turn anchors / file ledger. That lets opening a session be a two-part read of "snapshot meta + tail window",
    /// instead of parsing the whole row array again just for 60 rows. Leaving `rows` empty is intentional —
    /// when the full rows are needed use [`Self::all_rows`].
    pub fn write_snapshot(
        &self,
        session_id: &str,
        transcript: &crate::store::Transcript,
        transcript_len: u64,
        now_ms: i64,
    ) -> rusqlite::Result<()> {
        let mut meta = transcript.clone();
        let row_count = meta.rows.len() as i64;
        meta.rows = Vec::new();
        // History is stored separately: it is the model's memory, needed only when a turn actually runs,
        // and must not be parsed while "opening a session".
        meta.history = Vec::new();
        // Turn anchors scale linearly with session length (one entry per turnHeader), so they are not parsed on the open path either.
        let turn_starts_payload =
            serde_json::to_string(&transcript.turn_history_starts).unwrap_or_default();
        meta.turn_history_starts = Vec::new();
        self.conn.execute(
            "insert into conversation_turn_starts (session_id, payload, updated_at)
             values (?1, ?2, ?3)
             on conflict(session_id) do update set payload = excluded.payload,
                                                   updated_at = excluded.updated_at",
            params![session_id, turn_starts_payload, now_ms],
        )?;
        let history_payload = serde_json::to_string(&transcript.history).unwrap_or_default();
        self.conn.execute(
            "insert into conversation_history (session_id, payload, updated_at)
             values (?1, ?2, ?3)
             on conflict(session_id) do update set payload = excluded.payload,
                                                   updated_at = excluded.updated_at",
            params![session_id, history_payload, now_ms],
        )?;
        let payload = serde_json::to_string(&meta).unwrap_or_default();
        self.conn.execute(
            "insert into conversation_snapshot (session_id, payload, transcript_len, row_count, updated_at)
             values (?1, ?2, ?3, ?4, ?5)
             on conflict(session_id) do update set payload = excluded.payload,
                                                   transcript_len = excluded.transcript_len,
                                                   row_count = excluded.row_count,
                                                   updated_at = excluded.updated_at",
            params![session_id, payload, transcript_len as i64, row_count, now_ms],
        )?;
        Ok(())
    }

    /// Read the snapshot: returned only when "the transcript length matches" and "the row table's row count equals the row count recorded in the snapshot".
    /// The second condition is necessary: the read model may lag because writes happened while the switch was off, and looking only at the length would
    /// return a truncated row table as a complete session.
    pub fn read_snapshot(
        &self,
        session_id: &str,
        transcript_len: u64,
    ) -> Option<crate::store::Transcript> {
        let mut statement = self
            .conn
            .prepare(
                "select payload, transcript_len, row_count from conversation_snapshot where session_id = ?1",
            )
            .ok()?;
        let (payload, stored_len, stored_rows): (String, i64, i64) = statement
            .query_row(params![session_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .ok()?;
        if stored_len.max(0) as u64 != transcript_len {
            return None;
        }
        if stored_rows.max(0) as u64 != self.count(session_id).ok()? {
            return None;
        }
        serde_json::from_str(&payload).ok()
    }

    /// The full rows (ascending). For lazy source fallback: only reached when the caller genuinely needs historical rows.
    pub fn all_rows(&self, session_id: &str) -> rusqlite::Result<Vec<Value>> {
        let mut statement = self.conn.prepare(
            "select data from conversation_row where session_id = ?1 order by row_id asc",
        )?;
        let rows = statement.query_map(params![session_id], |row| {
            let data: String = row.get(0)?;
            Ok(serde_json::from_str::<Value>(&data).unwrap_or(Value::Null))
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Read the snapshot without the row-count check, also returning the row count recorded when it was written.
    ///
    /// For narrow opens: the total row count comes straight from this `row_count`, avoiding a full-table
    /// `count(*)` for one open. Completeness is instead checked by the caller comparing `max_row_id` with the window's last row (both indexed),
    /// which still detects a "row table lagging behind".
    pub fn read_snapshot_raw(
        &self,
        session_id: &str,
        transcript_len: u64,
    ) -> Option<(crate::store::Transcript, u64)> {
        let mut statement = self
            .conn
            .prepare(
                "select payload, transcript_len, row_count from conversation_snapshot where session_id = ?1",
            )
            .ok()?;
        let (payload, stored_len, stored_rows): (String, i64, i64) = statement
            .query_row(params![session_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .ok()?;
        if stored_len.max(0) as u64 != transcript_len {
            return None;
        }
        let transcript = serde_json::from_str(&payload).ok()?;
        Some((transcript, stored_rows.max(0) as u64))
    }

    /// Lazy load of the turn anchors (used by branch truncation).
    pub fn read_turn_starts(&self, session_id: &str) -> Option<Vec<(String, usize)>> {
        let mut statement = self
            .conn
            .prepare("select payload from conversation_turn_starts where session_id = ?1")
            .ok()?;
        let payload: String = statement
            .query_row(params![session_id], |row| row.get(0))
            .ok()?;
        serde_json::from_str(&payload).ok()
    }

    /// Lazy load of the model history. Same positioning as `all_rows`: only reached when it is really needed.
    pub fn read_history(
        &self,
        session_id: &str,
    ) -> Option<Vec<nc_code_provider::request::Message>> {
        let mut statement = self
            .conn
            .prepare("select payload from conversation_history where session_id = ?1")
            .ok()?;
        let payload: String = statement
            .query_row(params![session_id], |row| row.get(0))
            .ok()?;
        serde_json::from_str(&payload).ok()
    }

    /// The largest rowId (uses the `(session_id, row_id desc)` index, O(1)).
    pub fn max_row_id(&self, session_id: &str) -> rusqlite::Result<Option<u64>> {
        self.conn
            .query_row(
                "select max(row_id) from conversation_row where session_id = ?1",
                params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map(|value| value.map(|raw| raw.max(0) as u64))
    }

    /// The smallest rowId: this is what decides whether earlier rows still exist outside the window.
    pub fn first_row_id(&self, session_id: &str) -> rusqlite::Result<Option<u64>> {
        self.conn
            .query_row(
                "select min(row_id) from conversation_row where session_id = ?1",
                params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map(|value| value.map(|raw| raw.max(0) as u64))
    }

    pub fn count(&self, session_id: &str) -> rusqlite::Result<u64> {
        self.conn
            .query_row(
                "select count(*) from conversation_row where session_id = ?1",
                params![session_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count.max(0) as u64)
    }

    /// The consistency gate of the dual-write phase: this table's content must be byte-identical to the authoritative transcript's rows.
    /// Returns the description of the first divergence, which makes it easy to locate which delta missed the mirror.
    pub fn verify_against(
        &self,
        session_id: &str,
        authoritative: &[Value],
    ) -> rusqlite::Result<Option<String>> {
        let mut statement = self.conn.prepare(
            "select row_id, data from conversation_row where session_id = ?1 order by row_id",
        )?;
        let mirrored: Vec<(u64, String)> = statement
            .query_map(params![session_id], |row| {
                Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?))
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        let expected: Vec<(u64, String)> = authoritative
            .iter()
            .filter_map(|row| {
                let row_id = row.get("rowId").and_then(Value::as_u64)?;
                Some((row_id, row.to_string()))
            })
            .collect();

        for (index, expected_row) in expected.iter().enumerate() {
            match mirrored.get(index) {
                None => {
                    return Ok(Some(format!(
                        "missing mirrored row at index {index} (rowId {})",
                        expected_row.0
                    )))
                }
                Some(actual) if actual != expected_row => {
                    return Ok(Some(format!(
                        "mismatch at index {index}: expected rowId {}, got {}",
                        expected_row.0, actual.0
                    )))
                }
                Some(_) => {}
            }
        }
        if mirrored.len() > expected.len() {
            return Ok(Some(format!(
                "mirror has {} extra row(s)",
                mirrored.len() - expected.len()
            )));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(count: u64) -> Vec<Value> {
        (0..count)
            .map(|id| json!({"rowId": id, "kind": "turnHeader"}))
            .collect()
    }

    #[test]
    fn window_matches_tail_of_full_set() {
        let model = ConversationRows::open_in_memory().unwrap();
        let all = rows(5_000);
        for row in &all {
            model.upsert("ses", row, 1).unwrap();
        }
        assert_eq!(model.count("ses").unwrap(), 5_000);
        assert_eq!(model.window("ses", 60).unwrap(), all[4_940..].to_vec());
    }

    #[test]
    fn upsert_replaces_in_place() {
        let model = ConversationRows::open_in_memory().unwrap();
        model
            .upsert("ses", &json!({"rowId": 1, "state": "running"}), 1)
            .unwrap();
        model
            .upsert("ses", &json!({"rowId": 1, "state": "completed"}), 2)
            .unwrap();
        assert_eq!(model.count("ses").unwrap(), 1);
        assert_eq!(
            model.window("ses", 10).unwrap(),
            vec![json!({"rowId": 1, "state": "completed"})]
        );
    }

    #[test]
    fn truncate_drops_from_row_id_and_later() {
        let model = ConversationRows::open_in_memory().unwrap();
        for row in rows(10) {
            model.upsert("ses", &row, 1).unwrap();
        }
        model.truncate_from("ses", 4).unwrap();
        assert_eq!(model.window("ses", 100).unwrap(), rows(4));
    }

    #[test]
    fn verify_reports_divergence() {
        let model = ConversationRows::open_in_memory().unwrap();
        let authoritative = rows(3);
        assert!(model
            .verify_against("ses", &authoritative)
            .unwrap()
            .is_some());

        for row in &authoritative {
            model.upsert("ses", row, 1).unwrap();
        }
        assert_eq!(model.verify_against("ses", &authoritative).unwrap(), None);

        model
            .upsert("ses", &json!({"rowId": 9, "kind": "extra"}), 1)
            .unwrap();
        assert!(model
            .verify_against("ses", &authoritative)
            .unwrap()
            .is_some());
    }

    #[test]
    fn sessions_are_isolated() {
        let model = ConversationRows::open_in_memory().unwrap();
        model.upsert("a", &json!({"rowId": 1}), 1).unwrap();
        model.upsert("b", &json!({"rowId": 1}), 1).unwrap();
        assert_eq!(model.count("a").unwrap(), 1);
        assert_eq!(model.count("b").unwrap(), 1);
        model.truncate_from("a", 0).unwrap();
        assert_eq!(model.count("a").unwrap(), 0);
        assert_eq!(model.count("b").unwrap(), 1);
    }
}

#[cfg(test)]
mod dual_write_tests {
    use super::*;
    use crate::store::{SessionMeta, TranscriptStore};
    use serde_json::json;
    use std::sync::Arc;

    fn meta(session_id: &str) -> SessionMeta {
        SessionMeta {
            session_id: session_id.to_string(),
            title: "dual".to_string(),
            title_source: "default".to_string(),
            log_epoch: "epoch".to_string(),
            revision: 0,
            workspace_identity: "/tmp/ws".to_string(),
            created_at: 0,
            updated_at: 0,
            followup_mode: "queue".to_string(),
            collaboration_mode: "build".to_string(),
            provider: String::new(),
            model: String::new(),
            thought: String::new(),
            goal: None,
            plan_previous_mode: String::new(),
            parent_session_id: None,
            task_type: None,
        }
    }

    /// The core gate of the dual-write phase: whatever the store writes, the read model must have;
    /// append_row / append_upsert / append_rows_truncated must none of them miss the mirror.
    #[test]
    fn store_writes_mirror_into_read_model() {
        let root = std::env::temp_dir().join(format!("zcode-dual-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let model = Arc::new(std::sync::Mutex::new(
            ConversationRows::open_in_memory().unwrap(),
        ));
        let store = TranscriptStore::with_read_model(&root, model.clone());
        store.create_session(&meta("s1")).unwrap();

        for row_id in 0..5 {
            store
                .append_row("s1", &json!({"rowId": row_id, "state": "running"}))
                .unwrap();
        }
        store
            .append_upsert("s1", &json!({"rowId": 2, "state": "completed"}))
            .unwrap();
        store.append_rows_truncated("s1", 4).unwrap();

        let authoritative = store.load_transcript("s1").unwrap().rows;
        assert_eq!(authoritative.len(), 4);
        assert_eq!(
            model
                .lock()
                .unwrap()
                .verify_against("s1", &authoritative)
                .unwrap(),
            None,
            "read model must match transcript after row/upsert/truncate"
        );
        assert_eq!(
            model.lock().unwrap().window("s1", 60).unwrap(),
            authoritative
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Behaviour is unchanged when the read model is not enabled (the default path).
    #[test]
    fn without_read_model_store_still_works() {
        let root = std::env::temp_dir().join(format!("zcode-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let store = TranscriptStore::new(&root);
        store.create_session(&meta("s2")).unwrap();
        store.append_row("s2", &json!({"rowId": 0})).unwrap();
        assert_eq!(store.load_transcript("s2").unwrap().rows.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::store::{SessionMeta, TranscriptStore};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn meta(session_id: &str) -> SessionMeta {
        SessionMeta {
            session_id: session_id.to_string(),
            title: "snap".to_string(),
            title_source: "default".to_string(),
            log_epoch: "epoch".to_string(),
            revision: 0,
            workspace_identity: "/tmp/ws".to_string(),
            created_at: 0,
            updated_at: 0,
            followup_mode: "queue".to_string(),
            collaboration_mode: "build".to_string(),
            provider: String::new(),
            model: String::new(),
            thought: String::new(),
            goal: None,
            plan_previous_mode: String::new(),
            parent_session_id: None,
            task_type: None,
        }
    }

    /// The snapshot must be field-by-field equivalent to the replay — that is the admission condition for switching reads.
    #[test]
    fn snapshot_round_trips_full_transcript() {
        let root = std::env::temp_dir().join(format!("zcode-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let model = Arc::new(Mutex::new(ConversationRows::open_in_memory().unwrap()));
        let store = TranscriptStore::with_read_model(&root, model.clone());
        store.create_session(&meta("s1")).unwrap();
        for row_id in 0..6 {
            store
                .append_row(
                    "s1",
                    &json!({"rowId": row_id, "kind": "turnHeader", "turnId": format!("t{row_id}")}),
                )
                .unwrap();
        }
        store
            .append_upsert("s1", &json!({"rowId": 3, "state": "completed"}))
            .unwrap();
        store
            .append_history(
                "s1",
                &nc_code_provider::request::Message::text(
                    nc_code_provider::request::MessageRole::User,
                    "hello",
                ),
            )
            .unwrap();
        store
            .append_ack("s1", "cmd-1", &json!({"ok": true}))
            .unwrap();

        let replayed = store.load_transcript("s1").unwrap();
        let path = root.join("sessions").join("s1").join("transcript.jsonl");
        let len = std::fs::metadata(&path).unwrap().len();
        model
            .lock()
            .unwrap()
            .write_snapshot("s1", &replayed, len, 1)
            .unwrap();

        let from_snapshot = model
            .lock()
            .unwrap()
            .read_snapshot("s1", len)
            .expect("fresh snapshot");
        // The snapshot stores only meta (history/acks/anchors/ledger); the rows come from conversation_row.
        assert!(
            from_snapshot.rows.is_empty(),
            "snapshot must not carry rows"
        );
        let mirrored_rows = model.lock().unwrap().all_rows("s1").unwrap();
        assert_eq!(mirrored_rows, replayed.rows);
        assert!(
            from_snapshot.history.is_empty(),
            "snapshot must not carry history"
        );
        assert!(
            from_snapshot.turn_history_starts.is_empty(),
            "snapshot must not carry turn anchors"
        );
        let mirrored_history = model
            .lock()
            .unwrap()
            .read_history("s1")
            .expect("history blob");
        assert_eq!(mirrored_history.len(), replayed.history.len());
        let mirrored_starts = model
            .lock()
            .unwrap()
            .read_turn_starts("s1")
            .expect("anchor blob");
        assert_eq!(mirrored_starts, replayed.turn_history_starts);
        assert_eq!(from_snapshot.acks.len(), replayed.acks.len());
    }

    /// The file length changed (new appends) → the snapshot must be judged invalid, and must never return stale state.
    #[test]
    fn stale_snapshot_is_rejected_by_length() {
        let model = ConversationRows::open_in_memory().unwrap();
        let transcript = crate::store::Transcript::default();
        model.write_snapshot("s1", &transcript, 100, 1).unwrap();
        assert!(model.read_snapshot("s1", 100).is_some());
        assert!(model.read_snapshot("s1", 101).is_none());
    }

    /// End to end: the second open goes through the snapshot, and the result matches the first replay.
    #[test]
    fn cached_load_matches_replay_and_warms_snapshot() {
        let root = std::env::temp_dir().join(format!("zcode-cached-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let model = Arc::new(Mutex::new(ConversationRows::open_in_memory().unwrap()));
        let store = TranscriptStore::with_read_model(&root, model.clone());
        store.create_session(&meta("s3")).unwrap();
        for row_id in 0..4 {
            store.append_row("s3", &json!({"rowId": row_id})).unwrap();
        }

        let cold = store.load_transcript_cached("s3").unwrap();
        let warm = store.load_transcript_cached("s3").unwrap();
        assert_eq!(cold.rows, warm.rows);
        assert_eq!(cold.acks.len(), warm.acks.len());

        // After an append the length changes → the snapshot is invalid → replay again and refresh the snapshot.
        store.append_row("s3", &json!({"rowId": 4})).unwrap();
        let after_append = store.load_transcript_cached("s3").unwrap();
        assert_eq!(after_append.rows.len(), 5);
        let again = store.load_transcript_cached("s3").unwrap();
        assert_eq!(again.rows.len(), 5);
        let _ = std::fs::remove_dir_all(&root);
    }
}
