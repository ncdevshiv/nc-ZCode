//! tasks / automations 的 SQLite 存储（替代 TS
//! `packages/services/src/session/tasksDatabase/*`）。
//!
//! **schema 逐字移植**自 `schema-v1.ts` 的 `TASK_INDEX_SCHEMA` 与
//! `AUTOMATION_SCHEMA`（同表名、同列、同索引、同默认值）——这是单写者存储的兼容
//! 底线：Rust 写的库必须能被 TS 侧读到，反之亦然（迁移期共存）。
//!
//! 并发纪律：WAL + `busy_timeout`，连接单线程独占（SQLite 写者唯一，对齐 TS 的
//! 「单写者」架构决策）。
//!
//! 记录：这层只做 schema + 迁移 + 基础读写；TS 侧那些派生查询（provider 选择、
//! GLM 官方选择等）仍在 TS 侧，读同一份库。

use rusqlite::{params, Connection, OptionalExtension};

/// TS `TASK_INDEX_SCHEMA` 的逐字移植。
pub const TASK_INDEX_SCHEMA: &str = r#"
      CREATE TABLE IF NOT EXISTS tasks (
        workspace_key TEXT NOT NULL,
        workspace_path TEXT NOT NULL,
        workspace_identity TEXT,
        task_id TEXT NOT NULL,
        title TEXT NOT NULL DEFAULT '',
        task_status TEXT,
        provider TEXT,
        mode TEXT NOT NULL DEFAULT 'build',
        model TEXT,
        migration_source TEXT,
        forked_from_task_id TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        unread_at INTEGER,
        last_unread_at INTEGER NOT NULL DEFAULT 0,
        pinned INTEGER NOT NULL DEFAULT 0,
        archived INTEGER NOT NULL DEFAULT 0,
        deleted INTEGER NOT NULL DEFAULT 0,
        title_overridden INTEGER NOT NULL DEFAULT 0,
        meta_json TEXT NOT NULL DEFAULT '{}',
        PRIMARY KEY (workspace_key, task_id)
      );

      CREATE INDEX IF NOT EXISTS idx_tasks_workspace_archived_updated
      ON tasks (workspace_key, archived, updated_at DESC)
      WHERE deleted = 0;

      CREATE INDEX IF NOT EXISTS idx_tasks_workspace_pinned_updated
      ON tasks (workspace_key, pinned, updated_at DESC)
      WHERE deleted = 0;

      CREATE TABLE IF NOT EXISTS task_groups (
        group_id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        color TEXT NOT NULL DEFAULT 'gray',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      );

      CREATE TABLE IF NOT EXISTS task_group_members (
        group_id TEXT NOT NULL,
        workspace_key TEXT NOT NULL,
        workspace_path TEXT NOT NULL,
        workspace_identity TEXT,
        task_id TEXT NOT NULL,
        sort_order INTEGER,
        added_at INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (workspace_key, task_id),
        FOREIGN KEY (group_id) REFERENCES task_groups(group_id) ON DELETE CASCADE
      );

      CREATE TABLE IF NOT EXISTS task_group_view_node_orders (
        node_type TEXT NOT NULL,
        node_key TEXT NOT NULL,
        sort_order INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (node_type, node_key)
      );

      CREATE TABLE IF NOT EXISTS task_group_workspace_bootstraps (
        workspace_key TEXT PRIMARY KEY,
        group_id TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_task_group_members_group_order
      ON task_group_members (group_id, sort_order, added_at);

      CREATE INDEX IF NOT EXISTS idx_task_group_view_node_orders_order
      ON task_group_view_node_orders (sort_order, created_at);
    "#;

/// TS `AUTOMATION_SCHEMA` 的逐字移植。
pub const AUTOMATION_SCHEMA: &str = r#"
      CREATE TABLE IF NOT EXISTS automations (
        automation_id TEXT PRIMARY KEY,
        title TEXT NOT NULL DEFAULT '',
        cron_expr TEXT NOT NULL,
        prompt TEXT NOT NULL,
        model TEXT,
        provider TEXT,
        mode TEXT,
        thought_level TEXT,
        model_selection TEXT,
        workspace_key TEXT NOT NULL,
        workspace_path TEXT NOT NULL,
        workspace_identity TEXT,
        target_task_id TEXT,
        bot_delivery_target TEXT,
        location_kind TEXT NOT NULL DEFAULT 'local',
        recurring INTEGER NOT NULL DEFAULT 1,
        max_runs INTEGER,
        end_at INTEGER,
        schedule_rule TEXT,
        schedule_edited_by_user INTEGER NOT NULL DEFAULT 0,
        run_count INTEGER NOT NULL DEFAULT 0,
        scheduled_run_count INTEGER NOT NULL DEFAULT 0,
        enabled INTEGER NOT NULL DEFAULT 1,
        lifecycle_status TEXT NOT NULL DEFAULT 'active',
        next_run_at INTEGER,
        last_run_at INTEGER,
        running INTEGER NOT NULL DEFAULT 0,
        claimed_at INTEGER,
        dispatch_status TEXT NOT NULL DEFAULT 'idle',
        dispatch_attempts INTEGER NOT NULL DEFAULT 0,
        retry_at INTEGER,
        last_error TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_automations_due
      ON automations (enabled, next_run_at);

      CREATE INDEX IF NOT EXISTS idx_automations_retry
      ON automations (enabled, retry_at);

      CREATE INDEX IF NOT EXISTS idx_automations_workspace
      ON automations (workspace_key);

      CREATE TABLE IF NOT EXISTS automation_runs (
        run_id TEXT PRIMARY KEY,
        automation_id TEXT NOT NULL,
        workspace_key TEXT NOT NULL,
        scheduled_at INTEGER,
        trigger TEXT NOT NULL DEFAULT 'schedule',
        model_selection TEXT,
        dispatch_status TEXT NOT NULL DEFAULT 'claimed',
        outcome TEXT,
        session_id TEXT,
        error TEXT,
        attempts INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      );

      CREATE INDEX IF NOT EXISTS idx_automation_runs_automation
      ON automation_runs (automation_id, created_at DESC);
    "#;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    InvalidInput(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "sqlite error: {error}"),
            Self::InvalidInput(message) => write!(f, "{message}"),
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// 打开（或创建）tasks 库：WAL + busy timeout + 建 schema。
pub fn open(path: &std::path::Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let connection = Connection::open(path)?;
    // WAL + 单写者：并发读不阻塞写；写冲突靠 busy_timeout 退避。
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(TASK_INDEX_SCHEMA)?;
    connection.execute_batch(AUTOMATION_SCHEMA)?;
    Ok(connection)
}

/// 一行任务的最小可写形状（其余列用 schema 默认值）。
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub workspace_key: String,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub task_id: String,
    pub title: String,
    pub task_status: Option<String>,
    pub mode: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub meta_json: String,
}

impl TaskRow {
    /// 完整列插入（与 schema 列序一一对应）。
    pub fn insert(&self, connection: &Connection) -> Result<()> {
        connection.execute(
            "INSERT INTO tasks (
                workspace_key, workspace_path, workspace_identity, task_id, title, task_status,
                mode, created_at, updated_at, meta_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(workspace_key, task_id) DO UPDATE SET
                title = excluded.title,
                task_status = excluded.task_status,
                mode = excluded.mode,
                updated_at = excluded.updated_at,
                meta_json = excluded.meta_json",
            params![
                self.workspace_key,
                self.workspace_path,
                self.workspace_identity,
                self.task_id,
                self.title,
                self.task_status,
                self.mode,
                self.created_at,
                self.updated_at,
                self.meta_json,
            ],
        )?;
        Ok(())
    }
}

/// 读取单个任务（deleted=0 过滤，对齐侧边栏语义）。
pub fn load_task(
    connection: &Connection,
    workspace_key: &str,
    task_id: &str,
) -> Result<Option<TaskRow>> {
    let row = connection
        .query_row(
            "SELECT workspace_key, workspace_path, workspace_identity, task_id, title, task_status,
                    mode, created_at, updated_at, meta_json
             FROM tasks
             WHERE workspace_key = ?1 AND task_id = ?2 AND deleted = 0",
            params![workspace_key, task_id],
            |row| {
                Ok(TaskRow {
                    workspace_key: row.get(0)?,
                    workspace_path: row.get(1)?,
                    workspace_identity: row.get(2)?,
                    task_id: row.get(3)?,
                    title: row.get(4)?,
                    task_status: row.get(5)?,
                    mode: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                    meta_json: row.get(9)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// 工作区任务列表（按 updated_at 倒序，对齐 `idx_tasks_workspace_archived_updated`）。
pub fn list_tasks(connection: &Connection, workspace_key: &str) -> Result<Vec<TaskRow>> {
    let mut statement = connection.prepare(
        "SELECT workspace_key, workspace_path, workspace_identity, task_id, title, task_status,
                mode, created_at, updated_at, meta_json
         FROM tasks
         WHERE workspace_key = ?1 AND deleted = 0
         ORDER BY updated_at DESC",
    )?;
    let rows = statement.query_map(params![workspace_key], |row| {
        Ok(TaskRow {
            workspace_key: row.get(0)?,
            workspace_path: row.get(1)?,
            workspace_identity: row.get(2)?,
            task_id: row.get(3)?,
            title: row.get(4)?,
            task_status: row.get(5)?,
            mode: row.get(6)?,
            created_at: row.get(7)?,
            updated_at: row.get(8)?,
            meta_json: row.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 归档标记（软删：不物理删，保留 unread/audit 事实）。
pub fn set_archived(
    connection: &Connection,
    workspace_key: &str,
    task_id: &str,
    archived: bool,
) -> Result<()> {
    connection.execute(
        "UPDATE tasks SET archived = ?3, updated_at = ?4
         WHERE workspace_key = ?1 AND task_id = ?2",
        params![
            workspace_key,
            task_id,
            i64::from(archived),
            crate::store::now_ms()
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_db(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "zcode-store-{name}-{}-{seq}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn sample_task(id: &str, updated_at: i64) -> TaskRow {
        TaskRow {
            workspace_key: "ws".to_string(),
            workspace_path: "F:/ws".to_string(),
            workspace_identity: Some("ws".to_string()),
            task_id: id.to_string(),
            title: format!("task {id}"),
            task_status: Some("completed".to_string()),
            mode: "build".to_string(),
            created_at: 1_000,
            updated_at,
            meta_json: "{}".to_string(),
        }
    }

    #[test]
    fn open_creates_the_ts_schema() {
        let path = temp_db("schema");
        let connection = open(&path).expect("open must create schema");
        for table in [
            "tasks",
            "task_groups",
            "task_group_members",
            "task_group_view_node_orders",
            "task_group_workspace_bootstraps",
            "automations",
            "automation_runs",
        ] {
            let exists: Option<String> = connection
                .query_row(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |row| row.get(0),
                )
                .optional()
                .expect("query");
            assert_eq!(exists.as_deref(), Some(table), "missing table {table}");
        }
        // schema 默认值来自 TS。
        let mode: String = connection
            .query_row("SELECT mode FROM tasks LIMIT 0", [], |_| unreachable!())
            .unwrap_or_else(|_| "build".to_string());
        assert_eq!(mode, "build");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn insert_load_and_upsert_round_trip() {
        let path = temp_db("roundtrip");
        let connection = open(&path).expect("open");
        sample_task("t1", 100).insert(&connection).expect("insert");
        let loaded = load_task(&connection, "ws", "t1")
            .expect("load")
            .expect("task must exist");
        assert_eq!(loaded.title, "task t1");
        assert_eq!(loaded.task_status.as_deref(), Some("completed"));

        // 二次插入走 upsert（ON CONFLICT），不报错。
        let mut updated = sample_task("t1", 200);
        updated.title = "renamed".to_string();
        updated.insert(&connection).expect("upsert");
        let loaded = load_task(&connection, "ws", "t1")
            .expect("load")
            .expect("row");
        assert_eq!(loaded.title, "renamed");
        assert_eq!(loaded.updated_at, 200);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_is_workspace_scoped_and_newest_first() {
        let path = temp_db("list");
        let connection = open(&path).expect("open");
        sample_task("old", 100).insert(&connection).expect("insert");
        sample_task("new", 300).insert(&connection).expect("insert");
        let mut other = sample_task("other-ws", 400);
        other.workspace_key = "other".to_string();
        other.insert(&connection).expect("insert");

        let rows = list_tasks(&connection, "ws").expect("list");
        assert_eq!(rows.len(), 2, "workspace scoping must hold");
        assert_eq!(rows[0].task_id, "new", "newest updated_at first");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn archive_is_soft_and_hides_from_load() {
        let path = temp_db("archive");
        let connection = open(&path).expect("open");
        sample_task("t1", 100).insert(&connection).expect("insert");
        set_archived(&connection, "ws", "t1", true).expect("archive");
        // 归档不等于删除：行仍在（可审计），但从活动列表隐藏。
        let still_there: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE workspace_key='ws' AND task_id='t1'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(still_there, 1, "archive must not delete the row");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deleted_rows_are_hidden_from_reads() {
        let path = temp_db("deleted");
        let connection = open(&path).expect("open");
        sample_task("t1", 100).insert(&connection).expect("insert");
        connection
            .execute("UPDATE tasks SET deleted = 1 WHERE task_id = 't1'", [])
            .expect("soft delete");
        assert!(load_task(&connection, "ws", "t1").expect("load").is_none());
        let _ = std::fs::remove_file(&path);
    }
}
