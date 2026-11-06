//! automation storage warehouse: automations (definition + scheduling status) and automation_runs (running history +
//! runId idempotent account), in the same library as task index tasks-index.sqlite (WAL, multi-process security).
//!
//! The warehouse only does storage and atomic state migration; cron analysis / next_run_at calculation is calculated and passed in by the service layer.
//! The warehouse is not aware of cron semantics (same note as automationRepo.ts:221-223). DB connection and schema reuse
//! nc-code-host::storage::open (same table name/same column/same index, ensuring cross-language compatibility).

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::types::{
    Automation, AutomationRun, DispatchStatus, LifecycleStatus, ModelSelection, RunDispatchStatus,
    RunOutcome, ScheduleRule, Trigger,
};

/// Dispatch failure backoff constant (automationRepo.ts:36-40).
pub const DISPATCH_RETRY_BASE_MS: i64 = 30_000;
pub const DISPATCH_RETRY_CAP_MS: i64 = 15 * 60_000;
pub const DISPATCH_MAX_ATTEMPTS: u32 = 5;
/// Claim timeout recovery: running=1 If the settlement is not settled after this time, the holder will be deemed to have crashed.
pub const CLAIM_STALE_MS: i64 = 10 * 60_000;

/// Backoff retry time: now + min(BASE * 2^(attempts-1), CAP) (automationRepo.ts:209-215).
pub fn compute_retry_at(now: i64, attempts: u32) -> i64 {
    let shift = attempts.saturating_sub(1).min(32);
    let backoff = (DISPATCH_RETRY_BASE_MS << shift).min(DISPATCH_RETRY_CAP_MS);
    now + backoff
}

/// automation warehouse. Connections are opened lazily and cached; all methods block (rusqlite synchronization).
pub struct AutomationRepo {
    db_path: PathBuf,
    connection: Mutex<Option<Connection>>,
}

impl AutomationRepo {
    /// Constructed with explicit db path (test injects temporary library; production passes tasks-index.sqlite path).
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            connection: Mutex::new(None),
        }
    }

    fn open_connection(&self) -> Result<Connection, String> {
        nc_code_host::storage::open(&self.db_path).map_err(|error| error.to_string())
    }

    /// Get (or lazily open) the connection. If the opening fails, the error will be thrown upward, leaving no semi-initialized state.
    fn with_connection<T>(
        &self,
        run: impl FnOnce(&Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut guard = self
            .connection
            .lock()
            .map_err(|_| "automation repo lock poisoned".to_string())?;
        if guard.is_none() {
            *guard = Some(self.open_connection()?);
        }
        let connection = guard.as_ref().expect("connection opened");
        run(connection)
    }

    /// Close the connection (idempotent).
    pub fn close(&self) {
        if let Ok(mut guard) = self.connection.lock() {
            *guard = None;
        }
    }

    fn row_to_automation(row: &Row<'_>) -> Result<Automation, rusqlite::Error> {
        Ok(Automation {
            automation_id: row.get("automation_id")?,
            title: row.get("title")?,
            cron_expr: row.get("cron_expr")?,
            prompt: row.get("prompt")?,
            model_selection: row
                .get::<_, Option<String>>("model_selection")?
                .and_then(|value| read_model_selection(&value)),
            mode: row.get("mode")?,
            workspace_key: row.get("workspace_key")?,
            workspace_path: row.get("workspace_path")?,
            workspace_identity: row.get("workspace_identity")?,
            target_task_id: row.get("target_task_id")?,
            location_kind: row.get("location_kind")?,
            recurring: row.get::<_, i64>("recurring")? != 0,
            max_runs: row.get("max_runs")?,
            end_at: row.get("end_at")?,
            schedule_rule: row
                .get::<_, Option<String>>("schedule_rule")?
                .and_then(|value| serde_json::from_str::<ScheduleRule>(&value).ok()),
            schedule_edited_by_user: row
                .get::<_, Option<i64>>("schedule_edited_by_user")?
                .map(|v| v != 0),
            run_count: row.get::<_, i64>("run_count")? as u32,
            enabled: row.get::<_, i64>("enabled")? != 0,
            lifecycle_status: parse_lifecycle(&row.get::<_, String>("lifecycle_status")?),
            next_run_at: row.get("next_run_at")?,
            last_run_at: row.get("last_run_at")?,
            dispatch_status: parse_dispatch(&row.get::<_, String>("dispatch_status")?),
            dispatch_attempts: row.get::<_, i64>("dispatch_attempts")? as u32,
            retry_at: row.get("retry_at")?,
            last_error: row.get("last_error")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn row_to_run(row: &Row<'_>) -> Result<AutomationRun, rusqlite::Error> {
        Ok(AutomationRun {
            run_id: row.get("run_id")?,
            automation_id: row.get("automation_id")?,
            workspace_key: row.get("workspace_key")?,
            scheduled_at: row.get("scheduled_at")?,
            trigger: if row.get::<_, String>("trigger")? == "manual" {
                Trigger::Manual
            } else {
                Trigger::Schedule
            },
            model_selection: row
                .get::<_, Option<String>>("model_selection")?
                .and_then(|value| read_model_selection(&value)),
            dispatch_status: parse_run_dispatch(&row.get::<_, String>("dispatch_status")?),
            outcome: row
                .get::<_, Option<String>>("outcome")?
                .and_then(|value| parse_outcome(&value)),
            session_id: row.get("session_id")?,
            error: row.get("error")?,
            attempts: row.get::<_, i64>("attempts")? as u32,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    /// Single query (with optional workspace ownership filtering); returns None if it does not exist or does not belong to the workspace.
    pub fn get(
        &self,
        automation_id: &str,
        workspace_key: Option<&str>,
    ) -> Result<Option<Automation>, String> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT * FROM automations WHERE automation_id = ?1 AND (?2 IS NULL OR workspace_key = ?2)",
                    params![automation_id, workspace_key],
                    Self::row_to_automation,
                )
                .optional()
                .map_err(|error| error.to_string())
        })
    }

    /// Full list (in descending order by creation time), with optional workspace filtering.
    pub fn list(&self, workspace_key: Option<&str>) -> Result<Vec<Automation>, String> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT * FROM automations WHERE (?1 IS NULL OR workspace_key = ?1) ORDER BY created_at DESC",
                )
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map(params![workspace_key], Self::row_to_automation)
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())
        })
    }

    /// Whether the workspace has automation bound to a certain task (CronCreate ownership criterion).
    pub fn has_task_binding(
        &self,
        workspace_key: &str,
        target_task_id: &str,
    ) -> Result<bool, String> {
        self.with_connection(|connection| {
            let found: Option<i64> = connection
                .query_row(
                    "SELECT 1 FROM automations WHERE workspace_key = ?1 AND target_task_id = ?2 LIMIT 1",params![workspace_key, target_task_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            Ok(found.is_some())
        })
    }

    /// The life cycle caliber of maxRuns only counts scheduled dispatch (run_count includes manual, which is only used for Card display).
    pub fn get_scheduled_run_count(
        &self,
        automation_id: &str,
        workspace_key: Option<&str>,
    ) -> Result<Option<i64>, String> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT scheduled_run_count FROM automations WHERE automation_id = ?1 AND (?2 IS NULL OR workspace_key = ?2)",
                    params![automation_id, workspace_key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())
        })
    }
}

fn read_model_selection(value: &str) -> Option<ModelSelection> {
    // SQL NULL = old default not migrated; "null" = cleared explicitly. Neither resurrects old choices.
    if value == "null" {
        return None;
    }
    serde_json::from_str::<ModelSelection>(value).ok()
}

fn serialize_model_selection(selection: Option<&ModelSelection>) -> String {
    match selection {
        Some(selection) => serde_json::to_string(selection).unwrap_or_else(|_| "null".into()),
        None => "null".to_string(),
    }
}

fn parse_lifecycle(value: &str) -> LifecycleStatus {
    match value {
        "completed" => LifecycleStatus::Completed,
        "failed" => LifecycleStatus::Failed,
        "paused" => LifecycleStatus::Paused,
        _ => LifecycleStatus::Active,
    }
}

fn parse_dispatch(value: &str) -> DispatchStatus {
    match value {
        "claimed" => DispatchStatus::Claimed,
        "dispatched" => DispatchStatus::Dispatched,
        "failed_to_dispatch" => DispatchStatus::FailedToDispatch,
        _ => DispatchStatus::Idle,
    }
}

fn parse_run_dispatch(value: &str) -> RunDispatchStatus {
    match value {
        "dispatched" => RunDispatchStatus::Dispatched,
        "failed_to_dispatch" => RunDispatchStatus::FailedToDispatch,
        "skipped" => RunDispatchStatus::Skipped,
        _ => RunDispatchStatus::Claimed,
    }
}

fn parse_outcome(value: &str) -> Option<RunOutcome> {
    match value {
        "running" => Some(RunOutcome::Running),
        "succeeded" => Some(RunOutcome::Succeeded),
        "failed" => Some(RunOutcome::Failed),
        "stopped" => Some(RunOutcome::Stopped),
        _ => None,
    }
}

/// Edit the participating scheduling recalculation results (passed in after the service layer is calculated).
#[derive(Debug, Clone, Default)]
pub struct UpdateParams {
    pub title: Option<String>,
    pub cron_expr: Option<String>,
    pub prompt: Option<String>,
    /// None=no change; Some(None)=clear; Some(Some(sel))=set value.
    pub model_selection: Option<Option<ModelSelection>>,
    pub mode: Option<Option<String>>,
    pub recurring: Option<bool>,
    pub max_runs: Option<Option<u32>>,
    pub end_at: Option<Option<i64>>,
    /// None=No change; Some(None)=Clear and restore to normal cron; Some(Some(rule))=Set rules.
    pub schedule_rule: Option<Option<ScheduleRule>>,
    pub schedule_edited_by_user: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    pub next_run_at: Option<Option<i64>>,
    pub lifecycle_status: Option<LifecycleStatus>,
    pub reset_retry: bool,
}

/// Create input parameters (the service layer calculates nextRunAt/lifecycle and then passes it in).
#[derive(Debug, Clone, Default)]
pub struct CreateParams {
    pub title: String,
    pub cron_expr: String,
    pub prompt: String,
    pub model_selection: Option<ModelSelection>,
    pub mode: Option<String>,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub target_task_id: Option<String>,
    pub recurring: bool,
    pub max_runs: Option<u32>,
    pub end_at: Option<i64>,
    pub schedule_rule: Option<ScheduleRule>,
}

#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub next_run_at: Option<i64>,
    pub lifecycle_status: Option<LifecycleStatus>,
}

impl AutomationRepo {
    /// Create automation (BEGIN IMMEDIATE serialization "total check + insert", keep the upper limit of 20).
    pub fn create(
        &self,
        params: &CreateParams,
        workspace_key: &str,
        options: &CreateOptions,
    ) -> Result<Automation, String> {
        if let Some(mode) = params.mode.as_deref() {
            assert_valid_mode(mode)?;
        }
        let automation_id = format!("automation-{}", new_uuid());
        let now = now_ms();
        let created = self.with_connection(|connection| {
            let tx = connection
                .unchecked_transaction()
                .map_err(|error| error.to_string())?;
            let count: i64 = tx
                .query_row("SELECT COUNT(*) FROM automations", [], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            if count as usize >= crate::types::AUTOMATION_CREATE_LIMIT {
                return Err(crate::types::create_limit_error());
            }
            let enabled =
                i64::from(options.lifecycle_status != Some(LifecycleStatus::Completed));
            let lifecycle = options
                .lifecycle_status
                .map(lifecycle_str)
                .unwrap_or("active")
                .to_string();
            tx.execute(
                "INSERT INTO automations (
                    automation_id, title, cron_expr, prompt, model, provider, model_selection,                    workspace_key, workspace_path, workspace_identity, target_task_id, location_kind,
                    recurring, max_runs, end_at, schedule_rule, schedule_edited_by_user,
                    run_count, enabled, lifecycle_status, next_run_at, last_run_at, running, claimed_at,
                    dispatch_status, dispatch_attempts, retry_at, last_error, mode, thought_level,
                    created_at, updated_at
                ) VALUES (
                    ?1, ?2, ?3, ?4, NULL, NULL, ?5,
                    ?6, ?7, ?8, ?9, 'local',
                    ?10, ?11, ?12, ?13, 0,
                    0, ?14, ?15, ?16, NULL, 0, NULL,
                    'idle', 0, NULL, NULL, ?17, NULL,
                    ?18, ?18
                )",
                params![
                    automation_id,
                    params.title,
                    params.cron_expr,
                    params.prompt,
                    serialize_model_selection(params.model_selection.as_ref()),
                    workspace_key,
                    params.workspace_path,
                    params.workspace_identity,
                    params.target_task_id,
                    i64::from(params.recurring),
                    params.max_runs.map(i64::from),
                    params.end_at,
                    params
                        .schedule_rule
                        .as_ref()
                        .and_then(|rule| serde_json::to_string(rule).ok()),
                    enabled,
                    lifecycle,
                    options.next_run_at,
                    params.mode,
                    now,
                ],
            )
            .map_err(|error| error.to_string())?;
            tx.commit().map_err(|error| error.to_string())?;
Ok(())
        });
        created?;
        self.get(&automation_id, Some(workspace_key))?
            .ok_or_else(|| "created automation missing after insert".to_string())
    }

    /// Edit defined fields (recalculate as needed nextRunAt/lifecycle/clear retry). Clear the retry state when changing cron_expr.
    pub fn update(
        &self,
        automation_id: &str,
        params: &UpdateParams,
        options: &UpdateOptions,
        workspace_key: Option<&str>,
    ) -> Result<Option<Automation>, String> {
        if let Some(Some(mode)) = params.mode.as_ref() {
            assert_valid_mode(mode)?;
        }
        let existing = match self.get(automation_id, workspace_key)? {
            Some(existing) => existing,
            None => return Ok(None),
        };
        self.with_connection(|connection| {
            let now = now_ms();
            let title = params.title.clone().unwrap_or(existing.title);
            let cron_expr = params.cron_expr.clone().unwrap_or(existing.cron_expr);
            let prompt = params.prompt.clone().unwrap_or(existing.prompt);
            let model_selection = match &params.model_selection {
                None => serialize_model_selection(existing.model_selection.as_ref()),
                Some(selection) => serialize_model_selection(selection.as_ref()),
            };
            let mode = match &params.mode {
                None => existing.mode,
                Some(mode) => mode.clone(),
            };
            let recurring = params.recurring.unwrap_or(existing.recurring);
            let max_runs = match params.max_runs {
                None => existing.max_runs,
                Some(value) => value,
            };
            let end_at = match params.end_at {
                None => existing.end_at,
                Some(value) => value,
            };
            let schedule_rule = match &params.schedule_rule {
                None => existing.schedule_rule,
                Some(rule) => rule.clone(),
            };
            let schedule_edited = params.schedule_edited_by_user.unwrap_or(false);
            let next_run_at = match options.next_run_at {
                None => existing.next_run_at,
                Some(value) => value,
            };
            let lifecycle = options
                .lifecycle_status
                .unwrap_or(existing.lifecycle_status);
            // enabled is completely derived from lifecycle: active=1, completed/failed/paused=0.
            let enabled = i64::from(lifecycle == LifecycleStatus::Active);
            let (attempts, retry_at, dispatch) = if options.reset_retry {
                (0_i64, None, "idle".to_string())
            } else {
                (
                    existing.dispatch_attempts as i64,
                    existing.retry_at,
                    dispatch_str(existing.dispatch_status).to_string(),
                )
            };
            connection
                .execute(
                    "UPDATE automations SET title=?1, cron_expr=?2, prompt=?3, model_selection=?4,
                     mode=?5, recurring=?6, max_runs=?7, end_at=?8, schedule_rule=?9,
                     schedule_edited_by_user=?10, next_run_at=?11, lifecycle_status=?12,
                     enabled=?13, dispatch_attempts=?14, retry_at=?15, dispatch_status=?16,
                     updated_at=?17 WHERE automation_id=?18 AND (?19 IS NULL OR workspace_key=?19)",
                    params![
                        title,
                        cron_expr,
                        prompt,
                        model_selection,
                        mode,
                        i64::from(recurring),
                        max_runs.map(i64::from),
                        end_at,
                        schedule_rule
                            .as_ref()
                            .and_then(|rule| serde_json::to_string(rule).ok()),
                        i64::from(schedule_edited),
                        next_run_at,
                        lifecycle_str(lifecycle),
                        enabled,
                        attempts,
                        retry_at,
                        dispatch,
                        now,
                        automation_id,
                        workspace_key,
                    ],
                )
                .map_err(|error| error.to_string())?;
            // Self.get cannot be called here - it will enter with_connection again (Mutex is not reentrant, deadlock).
            Ok(())
        })?;
        // Read back after the transaction is committed (the guard has been released at this time).
        self.get(automation_id, workspace_key)
    }

    /// Remove automation (optional workspace ownership filtering). Returns whether the row was deleted.
    pub fn delete(&self, automation_id: &str, workspace_key: Option<&str>) -> Result<bool, String> {
        self.with_connection(|connection| {
            let changed = connection
                .execute(
                    "DELETE FROM automations WHERE automation_id=?1 AND (?2 IS NULL OR workspace_key=?2)",
                    params![automation_id, workspace_key],
                )
                .map_err(|error| error.to_string())?;
            Ok(changed > 0)
        })
    }

    /// Pause / resume (paused ↔ active, reserved next_run_at / run_count).
    pub fn set_enabled(
        &self,
        automation_id: &str,
        enabled: bool,
        workspace_key: Option<&str>,
    ) -> Result<(), String> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE automations SET enabled=?1, lifecycle_status=?2, updated_at=?3
                     WHERE automation_id=?4 AND (?5 IS NULL OR workspace_key=?5)",
                    params![
                        i64::from(enabled),
                        if enabled { "active" } else { "paused" },
                        now_ms(),
                        automation_id,
                        workspace_key,
                    ],
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    /// Manual rerun of final state task: return to active, clear count and retry state, nextRunAt is recalculated and passed in by the caller.
    pub fn restart(
        &self,
        automation_id: &str,
        next_run_at: Option<i64>,
        workspace_key: Option<&str>,
    ) -> Result<(), String> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE automations SET lifecycle_status='active', enabled=1, run_count=0,
                     scheduled_run_count=0, dispatch_attempts=0, retry_at=NULL,
                     dispatch_status='idle', running=0, claimed_at=NULL, next_run_at=?1,
                     last_error=NULL, updated_at=?2 WHERE automation_id=?3
                     AND (?4 IS NULL OR workspace_key=?4)",
                    params![next_run_at, now_ms(), automation_id, workspace_key],
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    /// Run immediately: write to the manual run held directly by the current host, occupying the single-flight lock.
    pub fn run_now(
        &self,
        automation_id: &str,
        now: i64,
        workspace_key: Option<&str>,
    ) -> Result<Option<(Automation, AutomationRun)>, String> {
        self.with_connection(|connection| {let tx = connection
                .unchecked_transaction()
                .map_err(|error| error.to_string())?;
            // Recycle zombies to claim.
            tx.execute(
                "UPDATE automations SET running=0, claimed_at=NULL
                 WHERE running=1 AND claimed_at IS NOT NULL AND claimed_at<=?1",
                params![now - CLAIM_STALE_MS],
            )
            .map_err(|error| error.to_string())?;
            let row = tx
                .query_row(
                    "SELECT * FROM automations WHERE automation_id=?1
                     AND (?2 IS NULL OR workspace_key=?2)",
                    params![automation_id, workspace_key],
                    Self::row_to_automation,
                )
                .optional()
                .map_err(|error| error.to_string())?;
            let Some(existing) = row else {
                tx.commit().map_err(|error| error.to_string())?;
                return Ok(None);
            };
            // single-flight: running 0→1.
            let claimed = tx
                .execute(
                    "UPDATE automations SET running=1, claimed_at=?1, updated_at=?1
                     WHERE automation_id=?2 AND running=0",
                    params![now, automation_id],
                )
                .map_err(|error| error.to_string())?;
            if claimed != 1 {
                tx.commit().map_err(|error| error.to_string())?;
                return Ok(None);
            }
            let run_id = format!("{automation_id}:manual:{}", new_uuid());
            tx.execute(
                "INSERT INTO automation_runs (
                    run_id, automation_id, workspace_key, scheduled_at, trigger,
                    model_selection, dispatch_status, attempts, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'manual', NULL, 'claimed', 1, ?5, ?5)",
                params![run_id, automation_id, existing.workspace_key, now, now,],
            )
            .map_err(|error| error.to_string())?;
            tx.commit().map_err(|error| error.to_string())?;
            // running/claimed_at is an internal scheduling column, not in the field JSON; the returned automation
            // Just updated_at reflects this write (consistent with TS rowToAutomation - without running).
            let mut claimed_automation = existing.clone();
            claimed_automation.updated_at = now;
            // Directly construct the run just inserted (you cannot call self.get_run here - it will enter with_connection again,
            // Mutex is not reentrant, leading to deadlock). The fields correspond to the INSERT above.
            let run = AutomationRun {
                run_id,
                automation_id: automation_id.to_string(),
                workspace_key: existing.workspace_key,
                scheduled_at: Some(now),
                trigger: Trigger::Manual,
                model_selection: None,
                dispatch_status: RunDispatchStatus::Claimed,
                outcome: None,
                session_id: None,
                error: None,
                attempts: 1,
                created_at: now,
                updated_at: now,
            };
            Ok(Some((claimed_automation, run)))
        })
    }

    /// Running history (descending order by creation time).
    pub fn list_runs(
        &self,
        automation_id: &str,
        workspace_key: Option<&str>,
    ) -> Result<Vec<AutomationRun>, String> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT * FROM automation_runs WHERE automation_id=?1
                     AND (?2 IS NULL OR workspace_key=?2) ORDER BY created_at DESC",
                )
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map(params![automation_id, workspace_key], Self::row_to_run)
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())
        })
    }

    pub fn get_run(&self, run_id: &str) -> Result<Option<AutomationRun>, String> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT * FROM automation_runs WHERE run_id=?1",
                    params![run_id],
                    Self::row_to_run,
                )
                .optional()
                .map_err(|error| error.to_string())
        })
    }

    pub fn delete_run(&self, run_id: &str, workspace_key: Option<&str>) -> Result<(), String> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "DELETE FROM automation_runs WHERE run_id=?1 AND (?2 IS NULL OR workspace_key=?2)",
                    params![run_id, workspace_key],
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }
}

fn lifecycle_str(status: LifecycleStatus) -> &'static str {
    match status {
        LifecycleStatus::Active => "active",
        LifecycleStatus::Completed => "completed",
        LifecycleStatus::Failed => "failed",
        LifecycleStatus::Paused => "paused",
    }
}

fn dispatch_str(status: DispatchStatus) -> &'static str {
    match status {
        DispatchStatus::Idle => "idle",
        DispatchStatus::Claimed => "claimed",
        DispatchStatus::Dispatched => "dispatched",
        DispatchStatus::FailedToDispatch => "failed_to_dispatch",
    }
}
fn assert_valid_mode(mode: &str) -> Result<(), String> {
    // Reading compatible historical dirty data does not mean that you are allowed to continue writing dirty data (repo is the final persistence boundary).
    const VALID: [&str; 3] = ["ask", "acceptEdits", "plan"];
    if VALID.contains(&mode) {
        Ok(())
    } else {
        Err(format!("Invalid automation mode: {mode}"))
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    Transient,
    Permanent,
}

/// Compensation parameters for missed trigger windows (Rust form of automationRepo.ts:1070-1078).
#[derive(Debug, Clone)]
pub struct SkipAndRescheduleParams<'a> {
    pub automation_id: &'a str,
    pub run_id: &'a str,
    pub workspace_key: &'a str,
    pub scheduled_at: Option<i64>,
    /// The skip reason that falls into run.error (scheduler passes computer_asleep_or_app_not_running).
    pub reason: &'a str,
    pub next_run_at: Option<i64>,
    /// true=The pure one-time task misses the target moment and is the final state; false=Moves forward to the next future trigger point.
    pub finalize: bool,
}

impl AutomationRepo {
    /// Single-flight claims due items: atomic running=0→1. At the same time, the final state of end_at and the zombie claim were recycled.
    /// The due judgment looks at next_run_at and retry_at at the same time, and any expiration is due.
    pub fn claim_due(&self, now: i64) -> Result<Vec<Automation>, String> {
        self.with_connection(|connection| {
            let tx = connection
                .unchecked_transaction()
                .map_err(|error| error.to_string())?;
            // The deadline is the plan boundary; when it expires, it will be converted to final state first.
            tx.execute(
                "UPDATE automations SET lifecycle_status='completed', enabled=0, next_run_at=NULL,
                 retry_at=NULL, running=0, claimed_at=NULL, updated_at=?1
                 WHERE enabled=1 AND end_at IS NOT NULL AND end_at < ?1",
                params![now],
            )
            .map_err(|error| error.to_string())?;
            // Recycle zombie claims (holder collapses).
            tx.execute(
                "UPDATE automations SET running=0, claimed_at=NULL
                 WHERE running=1 AND claimed_at IS NOT NULL AND claimed_at <= ?1",
                params![now - CLAIM_STALE_MS],
            )
            .map_err(|error| error.to_string())?;
            // Claim conditions: enabled and not in transit. If retry_at is pressed, only retry_at expires; otherwise, next_run_at is pressed.
            let due = {
                let mut statement = tx
                    .prepare(
                        "SELECT * FROM automations WHERE enabled=1 AND running=0 AND (
                         (retry_at IS NOT NULL AND retry_at <= ?1)
                         OR (retry_at IS NULL AND next_run_at IS NOT NULL AND next_run_at <= ?1))",
                    )
                    .map_err(|error| error.to_string())?;
                let rows = statement
                    .query_map(params![now], Self::row_to_automation)
                    .map_err(|error| error.to_string())?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.to_string())?
            };
            let mut claimed = Vec::new();
            for row in due {
                let changed = tx
                    .execute(
                        "UPDATE automations SET running=1, claimed_at=?1, dispatch_status='claimed',
                         updated_at=?1 WHERE automation_id=?2 AND running=0",
                        params![now, row.automation_id],
                    )
                    .map_err(|error| error.to_string())?;
                if changed == 1 {
                    let mut claimed_row = row.clone();
                    claimed_row.dispatch_status = DispatchStatus::Claimed;
                    claimed_row.updated_at = now;
                    claimed.push(claimed_row);
                }
            }
            tx.commit().map_err(|error| error.to_string())?;
            Ok(claimed)
        })
    }

    /// Successful settlement of distribution: count +1, write last_run_at, clear and retry, reset running; loop back to active,
    /// Limited number of max_runs to completed (enabled=0, next_run_at=NULL).
    pub fn mark_dispatched(
        &self,
        automation_id: &str,
        dispatched_at: i64,
        next_run_at: Option<i64>,
    ) -> Result<(), String> {
        let Some(row) = self.get(automation_id, None)? else {
            return Ok(()); // Deleted, discard the writeback to avoid resurrection
        };
        let run_count = row.run_count + 1;
        let scheduled = self
            .get_scheduled_run_count(automation_id, None)?
            .unwrap_or(0)
            + 1;
        //The limited task (recurring=0) is completed when it reaches the upper limit; if max_runs is not explicit, press it once (upper limit 1).
        let reached_max = !row.recurring && scheduled >= row.max_runs.unwrap_or(1) as i64;
        let reached_end = row
            .end_at
            .is_some_and(|end| next_run_at.unwrap_or(i64::MAX) > end);
        let terminal = reached_max || reached_end;
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE automations SET run_count=?1, scheduled_run_count=?2, last_run_at=?3,
                     dispatch_status='dispatched', dispatch_attempts=0, retry_at=NULL,
                     last_error=NULL, running=0, claimed_at=NULL, lifecycle_status=?4,
                     enabled=?5, next_run_at=?6, updated_at=?3 WHERE automation_id=?7",
                    params![
                        run_count,
                        scheduled,
                        dispatched_at,
                        if terminal { "completed" } else { "active" },
                        i64::from(!terminal),
                        if terminal { None } else { next_run_at },
                        automation_id,
                    ],
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    /// Distribution failure: transient accumulates attempts and retreats from writing retry_at; when the upper limit is reached, the loop jumps to next_run_at,
    /// Limited transfer failed. permanent direct failed final state deactivation.
    pub fn mark_dispatch_failed(
        &self,
        automation_id: &str,
        failed_at: i64,
        error: &str,
        kind: FailKind,
        next_run_at: Option<i64>,
    ) -> Result<(), String> {
        let Some(row) = self.get(automation_id, None)? else {
            return Ok(());
        };
        if kind == FailKind::Permanent {
            return self.with_connection(|connection| {
                connection
                    .execute(
                        "UPDATE automations SET dispatch_status='failed_to_dispatch',
                         lifecycle_status='failed', enabled=0, running=0, claimed_at=NULL,
                         last_error=?1, updated_at=?2 WHERE automation_id=?3",
                        params![error, failed_at, automation_id],
                    )
                    .map_err(|error| error.to_string())?;
                Ok(())
            });
        }
        let attempts = row.dispatch_attempts + 1;
        if attempts >= DISPATCH_MAX_ATTEMPTS {
            return self.with_connection(|connection| {
                if row.recurring {
                    // Loop task: give up this round, jump to the next normal next_run_at, clear and try again and return to idle.
                    connection
                        .execute(
                            "UPDATE automations SET dispatch_status='idle', dispatch_attempts=0,retry_at=NULL, running=0, claimed_at=NULL, next_run_at=?1,
                             last_error=?2, updated_at=?3 WHERE automation_id=?4",
                            params![next_run_at, error, failed_at, automation_id],
                        )
                        .map_err(|error| error.to_string())?;
                } else {
                    connection
                        .execute(
                            "UPDATE automations SET dispatch_status='failed_to_dispatch',
                             lifecycle_status='failed', enabled=0, running=0, claimed_at=NULL,
                             last_error=?1, updated_at=?2 WHERE automation_id=?3",
                            params![error, failed_at, automation_id],
                        )
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            });
        }
        let retry_at = compute_retry_at(failed_at, attempts);
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE automations SET dispatch_status='failed_to_dispatch', dispatch_attempts=?1,
                     retry_at=?2, running=0, claimed_at=NULL, last_error=?3, updated_at=?4
                     WHERE automation_id=?5",
                    params![attempts, retry_at, error, failed_at, automation_id],
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    /// Shut down/exit release claim: clear running, retain next_run_at, do not remember failure and do not advance.
    pub fn release_claim(&self, automation_id: &str) -> Result<(), String> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE automations SET running=0, claimed_at=NULL, dispatch_status='idle',
                     updated_at=?1 WHERE automation_id=?2 AND running=1",
                    params![now_ms(), automation_id],
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    /// After the manual run ends, only the single-flight lock is released and the scheduling status of the automation is not modified.
    /// (automationRepo.ts:1036-1048 releaseManualClaim). manual does not advance cron /
    /// max_runs / lifecycle, dispatch_status must maintain their original values.
    pub fn release_manual_claim(
        &self,
        automation_id: &str,
        workspace_key: &str,
    ) -> Result<(), String> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE automations SET running=0, claimed_at=NULL, updated_at=?1
                     WHERE automation_id=?2 AND workspace_key=?3 AND running=1",
                    params![now_ms(), automation_id, workspace_key],
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    /// Missed trigger window: atomically record a skipped run + move forward next_run_at (or one-time task transfer to final state)
    /// + Reset claim. Ignore run_count (automationRepo.ts:1064-1122 skipAndReschedule).
    ///
    /// `finalize=true` is used for purely one-time tasks: the target time has been missed, that is, the final state (completed + disabled +
    /// Clear schedule), subsequent cycles must no longer be deduced from the compatibility scheduleRule to continue execution.
    pub fn skip_and_reschedule(&self, params: &SkipAndRescheduleParams<'_>) -> Result<(), String> {
        let now = now_ms();
        self.with_connection(|connection| {
            let tx = connection
                .unchecked_transaction()
                .map_err(|error| error.to_string())?;
            tx.execute(
                "INSERT INTO automation_runs (                    run_id, automation_id, workspace_key, scheduled_at, trigger,
                    dispatch_status, error, attempts, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, 'schedule', 'skipped', ?5, 0, ?6, ?6)
                ON CONFLICT(run_id) DO UPDATE SET
                    dispatch_status='skipped', error=excluded.error, updated_at=excluded.updated_at",
                params![
                    params.run_id,
                    params.automation_id,
                    params.workspace_key,
                    params.scheduled_at,
                    params.reason,
                    now,
                ],
            )
            .map_err(|error| error.to_string())?;
            if params.finalize {
                tx.execute(
                    "UPDATE automations
                     SET lifecycle_status='completed', enabled=0, next_run_at=NULL,
                         running=0, claimed_at=NULL, dispatch_status='idle',
                         dispatch_attempts=0, retry_at=NULL, updated_at=?1
                     WHERE automation_id=?2",
                    params![now, params.automation_id],
                )
                .map_err(|error| error.to_string())?;
            } else {
                tx.execute(
                    "UPDATE automations
                     SET next_run_at=?1, running=0, claimed_at=NULL, dispatch_status='idle',
                         dispatch_attempts=0, retry_at=NULL, updated_at=?2
                     WHERE automation_id=?3",
                    params![params.next_run_at, now, params.automation_id],
                )
                .map_err(|error| error.to_string())?;
            }
            tx.commit().map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    /// Upsert a line of run when claiming (run_id conflict means hitting retry in this round, no new creation).
    pub fn upsert_run_claimed(
        &self,
        run_id: &str,
        automation_id: &str,
        workspace_key: &str,
        scheduled_at: Option<i64>,
        trigger: Trigger,
        model_selection: Option<&ModelSelection>,
    ) -> Result<(), String> {
        let now = now_ms();
        let trigger = match trigger {
            Trigger::Manual => "manual",
            Trigger::Schedule => "schedule",
        };
        self.with_connection(|connection| {
            connection
                .execute(
                    "INSERT INTO automation_runs (
                        run_id, automation_id, workspace_key, scheduled_at, trigger,
                        model_selection, dispatch_status, attempts, created_at, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'claimed', 0, ?7, ?7)
                    ON CONFLICT(run_id) DO UPDATE SET
                        dispatch_status='claimed',
                        model_selection=COALESCE(automation_runs.model_selection, excluded.model_selection),
                        outcome=NULL, error=NULL, attempts=attempts+1, updated_at=excluded.updated_at",
                    params![
                        run_id,
                        automation_id,
                        workspace_key,
                        scheduled_at,
                        trigger,
                        serialize_model_selection(model_selection),
                        now,
                    ],
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    /// The dispatch result is written back to run (dispatched backfills session_id / failed and error is recorded).
    pub fn mark_run_dispatch(
        &self,
        run_id: &str,
        dispatch_status: RunDispatchStatus,
        session_id: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), String> {
        let status = match dispatch_status {
            RunDispatchStatus::Dispatched => "dispatched",
            RunDispatchStatus::FailedToDispatch => "failed_to_dispatch",
            RunDispatchStatus::Skipped => "skipped",
            RunDispatchStatus::Claimed => "claimed",
        };
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE automation_runs SET dispatch_status=?1,
                     session_id=COALESCE(?2, session_id), error=?3, updated_at=?4 WHERE run_id=?5",
                    params![status, session_id, error, now_ms(), run_id],
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    /// Select atomically fixes the run modelSelection when Submission is first formed; then calls read-only
    /// Original value (automationRepo.ts:1190-1212 fixRunModelSelection). old distribution in every
    /// Transient retry re-reads Host preferred, which will cause the same run to change models.
    /// Run returns an error when it does not exist (the missing ledger cannot be regarded as "no selection").
    pub fn fix_run_model_selection(
        &self,
        run_id: &str,
        selection: &ModelSelection,
    ) -> Result<Option<ModelSelection>, String> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE automation_runs SET model_selection=COALESCE(model_selection, ?1),
                     updated_at=?2 WHERE run_id=?3",
                    params![serialize_model_selection(Some(selection)), now_ms(), run_id],
                )
                .map_err(|error| error.to_string())?;
            let stored = connection
                .query_row(
                    "SELECT model_selection FROM automation_runs WHERE run_id=?1",
                    params![run_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            stored
                .map(|value| value.and_then(|value| read_model_selection(&value)))
                .ok_or_else(|| format!("Automation run does not exist: {run_id}"))
        })
    }

    /// Manual run successfully settles the first distribution: atomically updates the run ledger and the cumulative number of runs /// (automationRepo.ts:1240-1302 markManualRunDispatched).
    ///
    /// The same runId may be directly dispatched and settled repeatedly with late returns, so dispatch_status is used first
    /// Enter dispatched as idempotent boundary; manual does not advance cron / maxRuns / lifecycle, nor
    /// Release single-flight claim (claim is still closed by the real turn final state). Returns whether it is hit and
    /// The automation count is written (false if dispatched or run is not manual).
    pub fn mark_manual_run_dispatched(
        &self,
        run_id: &str,
        session_id: Option<&str>,
        dispatched_at: i64,
    ) -> Result<bool, String> {
        self.with_connection(|connection| {
            let tx = connection
                .unchecked_transaction()
                .map_err(|error| error.to_string())?;
            let found: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT automation_id, workspace_key, dispatch_status
                     FROM automation_runs WHERE run_id=?1 AND trigger='manual'",
                    params![run_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            let Some((automation_id, workspace_key, dispatch_status)) = found else {
                tx.commit().map_err(|error| error.to_string())?;
                return Ok(false);
            };
            if dispatch_status == "dispatched" {
                tx.commit().map_err(|error| error.to_string())?;
                return Ok(false);
            }
            tx.execute(
                "UPDATE automation_runs
                 SET dispatch_status='dispatched',
                     session_id=COALESCE(?1, session_id), error=NULL, updated_at=?2
                 WHERE run_id=?3 AND trigger='manual' AND dispatch_status<>'dispatched'",
                params![session_id, dispatched_at, run_id],
            )
            .map_err(|error| error.to_string())?;
            let changed = tx
                .execute(
                    "UPDATE automations
                     SET run_count=run_count+1, last_run_at=?1, updated_at=?1
                     WHERE automation_id=?2 AND workspace_key=?3",
                    params![dispatched_at, automation_id, workspace_key],
                )
                .map_err(|error| error.to_string())?;
            tx.commit().map_err(|error| error.to_string())?;
            Ok(changed > 0)
        })
    }

    /// Session runtime writes back the running results (running does not overwrite the existing final state - idempotent).
    pub fn mark_run_outcome(
        &self,
        run_id: &str,
        outcome: RunOutcome,
        error: Option<&str>,
    ) -> Result<(), String> {
        let outcome_str = match outcome {
            RunOutcome::Running => "running",
            RunOutcome::Succeeded => "succeeded",
            RunOutcome::Failed => "failed",
            RunOutcome::Stopped => "stopped",
        };
        self.with_connection(|connection| {
            // Consistent with the CASE of automationRepo.ts:1304-1326: running is an intermediate state and must not
            // Overwrite the settled final state; factual rewriting is allowed between final states (such as late replay).
            connection
                .execute(
                    "UPDATE automation_runs SET
                        outcome=CASE
                            WHEN ?1='running' AND outcome IS NOT NULL AND outcome<>'running'
                                THEN outcome
                            ELSE ?1
                        END,
                        error=CASE
                            WHEN ?1='running' AND outcome IS NOT NULL AND outcome<>'running'
                                THEN error
                            ELSE COALESCE(?2, error)
                        END,
                        updated_at=?3
                     WHERE run_id=?4",
                    params![outcome_str, error, now_ms(), run_id],
                )
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    /// Retention policy: Delete historical runs that exceed max_age_ms (to prevent unlimited growth).
    pub fn prune_runs(&self, max_age_ms: i64) -> Result<usize, String> {
        self.with_connection(|connection| {
            let changed = connection
                .execute(
                    "DELETE FROM automation_runs WHERE created_at < ?1",
                    params![now_ms() - max_age_ms],
                )
                .map_err(|error| error.to_string())?;
            Ok(changed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DispatchStatus, LifecycleStatus, RunOutcome};
    use std::path::PathBuf;

    fn temp_db(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zcode-auto-repo-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir.join("tasks-index.sqlite")
    }
    fn create(repo: &AutomationRepo, recurring: bool, next_run_at: Option<i64>) -> Automation {
        repo.create(
            &CreateParams {
                title: "scheduled report".to_string(),
                cron_expr: "30 9 * * *".to_string(),
                prompt: "run the thing".to_string(),
                workspace_path: "/ws/alpha".to_string(),
                recurring,
                ..Default::default()
            },
            "/ws/alpha",
            &CreateOptions {
                next_run_at,
                ..Default::default()
            },
        )
        .expect("create")
    }

    #[test]
    fn skip_and_reschedule_records_skipped_run_and_advances_next_run_at() {
        let repo = AutomationRepo::new(temp_db("skip-recurring"));
        let automation = create(&repo, true, Some(1_000));
        let claimed = repo.claim_due(2_000).expect("claim");
        assert_eq!(claimed.len(), 1, "Expired items must be claimed");
        let run_id = format!("{}:1000", automation.automation_id);

        repo.skip_and_reschedule(&SkipAndRescheduleParams {
            automation_id: &automation.automation_id,
            run_id: &run_id,
            workspace_key: "/ws/alpha",
            scheduled_at: Some(1_000),
            reason: "computer_asleep_or_app_not_running",
            next_run_at: Some(600_000),
            finalize: false,
        })
        .expect("skip");

        let after = repo
            .get(&automation.automation_id, Some("/ws/alpha"))
            .expect("get")
            .expect("exists");
        assert_eq!(
            after.next_run_at,
            Some(600_000),
            "Move forward to the next future trigger point"
        );
        assert_eq!(after.dispatch_status, DispatchStatus::Idle);
        assert_eq!(
            after.dispatch_attempts, 0,
            "Skip does not count the number of dispatch failures"
        );
        assert_eq!(after.run_count, 0, "Skip does not count run_count");
        assert_eq!(after.lifecycle_status, LifecycleStatus::Active);
        assert!(after.enabled);

        let run = repo.get_run(&run_id).expect("run").expect("exists");
        assert_eq!(run.dispatch_status, RunDispatchStatus::Skipped);
        assert_eq!(
            run.error.as_deref(),
            Some("computer_asleep_or_app_not_running")
        );
        assert_eq!(run.attempts, 0);
        assert_eq!(run.trigger, Trigger::Schedule);

        // claim has been reset: no more due before 600_000, and can be claimed again at that point.
        assert!(repo.claim_due(599_999).expect("early").is_empty());
        assert_eq!(repo.claim_due(600_000).expect("due").len(), 1);
    }

    #[test]
    fn skip_and_reschedule_finalizes_missed_one_shot() {
        let repo = AutomationRepo::new(temp_db("skip-one-shot"));
        let automation = create(&repo, false, Some(1_000));
        assert_eq!(repo.claim_due(2_000).expect("claim").len(), 1);
        let run_id = format!("{}:1000", automation.automation_id);

        repo.skip_and_reschedule(&SkipAndRescheduleParams {
            automation_id: &automation.automation_id,
            run_id: &run_id,
            workspace_key: "/ws/alpha",
            scheduled_at: Some(1_000),
            reason: "computer_asleep_or_app_not_running",
            next_run_at: None,
            finalize: true,
        })
        .expect("skip");

        let after = repo
            .get(&automation.automation_id, Some("/ws/alpha"))
            .expect("get")
            .expect("exists");
        assert_eq!(after.lifecycle_status, LifecycleStatus::Completed);
        assert!(
            !after.enabled,
            "One-time task misses the window and is finalized and deactivated"
        );
        assert_eq!(after.next_run_at, None);
        assert_eq!(after.dispatch_status, DispatchStatus::Idle);
        assert_eq!(after.run_count, 0);
        let run = repo.get_run(&run_id).expect("run").expect("exists");
        assert_eq!(run.dispatch_status, RunDispatchStatus::Skipped);
        assert!(repo.claim_due(i64::MAX / 2).expect("no claim").is_empty());
    }

    #[test]
    fn mark_manual_run_dispatched_counts_once_and_keeps_first_session() {
        let repo = AutomationRepo::new(temp_db("manual-dispatched"));
        let automation = create(&repo, true, Some(9_000_000));
        let (_, run) = repo
            .run_now(&automation.automation_id, 5_000, None)
            .expect("run_now")
            .expect("claimed");

        assert!(
            repo.mark_manual_run_dispatched(&run.run_id, Some("session-1"), 6_000)
                .expect("mark"),
            "Must hit when entering dispatched for the first time"
        );
        let after = repo
            .get(&automation.automation_id, None)
            .expect("get")
            .expect("exists");
        assert_eq!(
            after.run_count, 1,
            "manual is included in the cumulative number of runs"
        );
        assert_eq!(after.last_run_at, Some(6_000));
        assert_eq!(
            after.next_run_at,
            Some(9_000_000),
            "manual does not change the cron plan"
        );
        assert_eq!(after.lifecycle_status, LifecycleStatus::Active);

        let stored = repo.get_run(&run.run_id).expect("run").expect("exists");
        assert_eq!(stored.dispatch_status, RunDispatchStatus::Dispatched);
        assert_eq!(stored.session_id.as_deref(), Some("session-1"));

        // Repeated settlements (late returns/crash recovery) are idempotent.
        assert!(
            !repo
                .mark_manual_run_dispatched(&run.run_id, Some("session-2"), 7_000)
                .expect("second mark"),
            "A dispatched run must not be counted again"
        );
        let again = repo
            .get(&automation.automation_id, None)
            .expect("get")
            .expect("exists");
        assert_eq!(again.run_count, 1);
        assert_eq!(again.last_run_at, Some(6_000));
        assert_eq!(
            repo.get_run(&run.run_id)
                .expect("run")
                .expect("exists")
                .session_id
                .as_deref(),
            Some("session-1"),
            "sessionId keeps the first backfill value"
        );
    }

    #[test]
    fn release_manual_claim_allows_the_next_run_now_claim() {
        let repo = AutomationRepo::new(temp_db("manual-release"));
        let automation = create(&repo, true, Some(9_000_000));
        let (_, first) = repo
            .run_now(&automation.automation_id, 5_000, None)
            .expect("run_now")
            .expect("claimed");
        assert!(
            repo.run_now(&automation.automation_id, 5_001, None)
                .expect("second run_now")
                .is_none(),
            "single-flight: claim in transit refuses to claim again"
        );

        repo.release_manual_claim(&automation.automation_id, "/ws/alpha")
            .expect("release");
        let (_, second) = repo
            .run_now(&automation.automation_id, 5_002, None)
            .expect("run_now after release")
            .expect("claim released");
        assert_ne!(first.run_id, second.run_id);

        let after = repo
            .get(&automation.automation_id, None)
            .expect("get")
            .expect("exists");
        assert_eq!(
            after.dispatch_status,
            DispatchStatus::Idle,
            "Release manual claim without writing automation dispatch state"
        );
        assert_eq!(after.lifecycle_status, LifecycleStatus::Active);
    }

    #[test]
    fn fix_run_model_selection_keeps_the_first_selection() {
        let repo = AutomationRepo::new(temp_db("fix-selection"));
        let automation = create(&repo, true, Some(9_000_000));
        let (_, run) = repo
            .run_now(&automation.automation_id, 5_000, None)
            .expect("run_now")
            .expect("claimed");
        let first = ModelSelection {
            provider_id: "personal:1".to_string(),
            model_id: "model-a".to_string(),
            options: None,
        };
        let second = ModelSelection {
            provider_id: "personal:1".to_string(),
            model_id: "model-b".to_string(),
            options: None,
        };
        assert_eq!(
            repo.fix_run_model_selection(&run.run_id, &first)
                .expect("fix"),
            Some(first.clone())
        );
        // Back off and retry calling again: only read back the first fixed value, without changing the model.
        assert_eq!(
            repo.fix_run_model_selection(&run.run_id, &second)
                .expect("fix again"),
            Some(first.clone())
        );
        assert_eq!(
            repo.get_run(&run.run_id)
                .expect("run")
                .expect("exists")
                .model_selection,
            Some(first)
        );
        assert!(
            repo.fix_run_model_selection("missing-run", &second)
                .is_err(),
            "An error must be reported if the ledger is missing, and an empty selection cannot be returned"
        );
    }

    #[test]
    fn mark_run_outcome_running_never_downgrades_a_terminal_state() {
        let repo = AutomationRepo::new(temp_db("outcome-idempotent"));
        let automation = create(&repo, true, Some(9_000_000));
        let (_, run) = repo
            .run_now(&automation.automation_id, 5_000, None)
            .expect("run_now")
            .expect("claimed");

        repo.mark_run_outcome(&run.run_id, RunOutcome::Succeeded, None)
            .expect("succeeded");
        repo.mark_run_outcome(&run.run_id, RunOutcome::Running, None)
            .expect("late running");
        let stored = repo.get_run(&run.run_id).expect("run").expect("exists");
        assert_eq!(
            stored.outcome,
            Some(RunOutcome::Succeeded),
            "running is an intermediate state and must not overwrite the final state"
        );

        //Rewrite according to the real facts between final states (late final state return of the same run).
        repo.mark_run_outcome(&run.run_id, RunOutcome::Failed, Some("provider error"))
            .expect("failed");
        let stored = repo.get_run(&run.run_id).expect("run").expect("exists");
        assert_eq!(stored.outcome, Some(RunOutcome::Failed));
        assert_eq!(stored.error.as_deref(), Some("provider error"));
    }
}
