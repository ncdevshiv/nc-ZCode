//! SQLite store: one connection, WAL, explicit schema. The orchestrator is the only
//! writer of orchestration state (spec: specs/nc-ag-orchestration.md section 2).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{NcagError, Result};

pub const SCHEMA_SQL: &str = include_str!("../migrations/0001_init.sql");
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

pub struct Store {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.pragma_update(None, "foreign_keys", "OFF")?;
        connection.execute_batch(SCHEMA_SQL)?;
        Ok(Self {
            conn: Mutex::new(connection),
            path: path.to_path_buf(),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(SCHEMA_SQL)?;
        Ok(Self {
            conn: Mutex::new(connection),
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Public guard for the repository layer (same single connection, no second pool).
    pub fn raw_lock(&self) -> MutexGuard<'_, Connection> {
        self.lock()
    }

    /// Run `work` inside one transaction. Any error rolls back.
    pub fn tx<T>(&self, work: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let value = work(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    pub fn exists(&self, table: &str, id: &str) -> Result<bool> {
        let conn = self.lock();
        let sql = format!("SELECT 1 FROM {table} WHERE id = ?1");
        let found: Option<i64> = conn.query_row(&sql, [id], |row| row.get(0)).optional()?;
        Ok(found.is_some())
    }

    /// Append one audit row. `hash = sha256(prev_hash | actor | action | detail | at)`
    /// so tampering with any earlier row breaks verification.
    pub fn append_audit(
        &self,
        project_id: Option<&str>,
        actor: &str,
        action: &str,
        detail: &Value,
        at: i64,
    ) -> Result<String> {
        let conn = self.lock();
        let prev: String = conn
            .query_row(
                "SELECT hash FROM audit ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_else(|| GENESIS_HASH.to_string());
        let detail_text = serde_json::to_string(detail)?;
        let hash = audit_hash(&prev, actor, action, &detail_text, at);
        conn.execute(
            "INSERT INTO audit (project_id, actor, action, detail_json, prev_hash, hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![project_id, actor, action, detail_text, prev, hash, at],
        )?;
        Ok(hash)
    }

    /// Verify the whole chain; returns the first broken row id when tampered.
    pub fn verify_audit_chain(&self) -> Result<Option<i64>> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT id, actor, action, detail_json, prev_hash, hash, created_at
             FROM audit ORDER BY id ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        let mut expected_prev = GENESIS_HASH.to_string();
        for row in rows {
            let (id, actor, action, detail, prev, hash, at) = row?;
            if prev != expected_prev {
                return Ok(Some(id));
            }
            let recomputed = audit_hash(&prev, &actor, &action, &detail, at);
            if recomputed != hash {
                return Ok(Some(id));
            }
            expected_prev = hash;
        }
        Ok(None)
    }

    pub fn audit_len(&self) -> Result<i64> {
        let conn = self.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM audit", [], |row| row.get(0))?)
    }
}

pub fn audit_hash(prev: &str, actor: &str, action: &str, detail_text: &str, at: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev.as_bytes());
    hasher.update(b"|");
    hasher.update(actor.as_bytes());
    hasher.update(b"|");
    hasher.update(action.as_bytes());
    hasher.update(b"|");
    hasher.update(detail_text.as_bytes());
    hasher.update(b"|");
    hasher.update(at.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

/// Stable UTC milliseconds. Every timestamp in NC-AG is this clock.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn to_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

pub fn from_text(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

pub fn json_vec(text: &str) -> Vec<String> {
    from_text(text)
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn db_error(message: impl Into<String>) -> NcagError {
    NcagError::new("storage_error", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_chain_detects_tampering() {
        let store = Store::in_memory().expect("store");
        store
            .append_audit(None, "orchestrator", "project.created", &Value::Null, 1)
            .expect("a1");
        store
            .append_audit(None, "orchestrator", "task.created", &Value::Null, 2)
            .expect("a2");
        assert_eq!(store.verify_audit_chain().unwrap(), None);
        {
            let conn = store.lock();
            conn.execute("UPDATE audit SET action = 'task.deleted' WHERE id = 2", [])
                .unwrap();
        }
        assert_eq!(store.verify_audit_chain().unwrap(), Some(2));
    }

    #[test]
    fn now_ms_is_utc_epoch_millis() {
        let now = now_ms();
        assert!(now > 1_600_000_000_000);
    }
}
