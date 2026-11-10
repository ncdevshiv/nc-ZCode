//! Resource lease manager: shared reads, exclusive writes, fencing tokens, expiry and
//! waiters (spec: specs/nc-ag-orchestration.md section 6).

use std::collections::BTreeMap;

use rusqlite::params;
use serde_json::json;

use crate::db::now_ms;
use crate::error::Result;
use crate::types::{Lease, LeaseMode, Task};
use crate::Store;

/// Normalise a declared resource (a file path or module name) so two spellings of the
/// same file cannot both hold leases.
pub fn normalize_resource(raw: &str) -> String {
    let trimmed = raw.trim();
    let mut value = trimmed.replace('\\', "/");
    while value.starts_with("./") {
        value.drain(..2);
    }
    if value.is_empty() {
        return "workspace".to_string();
    }
    value
}

#[derive(Debug, Clone, PartialEq)]
pub struct LeaseRequest {
    pub resource: String,
    pub mode: LeaseMode,
}

impl LeaseRequest {
    pub fn write(resource: impl Into<String>) -> Self {
        Self {
            resource: normalize_resource(&resource.into()),
            mode: LeaseMode::ExclusiveWrite,
        }
    }
    pub fn read(resource: impl Into<String>) -> Self {
        Self {
            resource: normalize_resource(&resource.into()),
            mode: LeaseMode::SharedRead,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AcquireOutcome {
    Acquired(Vec<Lease>),
    /// Returned with concrete evidence for the agent: who holds it, which task, until when.
    Waiting {
        resource: String,
        held_by_task: String,
        held_by_agent: Option<String>,
        expires_at: i64,
        mode: String,
    },
}

pub struct LeaseManager<'a> {
    store: &'a Store,
    ttl_ms: i64,
}

impl<'a> LeaseManager<'a> {
    pub fn new(store: &'a Store, ttl_ms: i64) -> Self {
        Self { store, ttl_ms }
    }

    /// Acquire all declared resources atomically: either every lease is granted or none is.
    /// Resources are sorted so two tasks declaring the same set cannot deadlock.
    pub fn acquire(
        &self,
        project_id: &str,
        task_id: &str,
        agent_id: Option<&str>,
        requests: &[LeaseRequest],
    ) -> Result<AcquireOutcome> {
        self.store.expire_leases()?;
        let mut requested: BTreeMap<String, LeaseMode> = BTreeMap::new();
        for request in requests {
            let entry = requested
                .entry(request.resource.clone())
                .or_insert(request.mode);
            if request.mode == LeaseMode::ExclusiveWrite {
                *entry = LeaseMode::ExclusiveWrite;
            }
        }
        let now = now_ms();
        let conn = self.store.raw_lock();
        // Check every resource first (no partial acquisition).
        for (resource, mode) in &requested {
            let existing: Option<(String, String, Option<String>, i64, String)> = conn
                .query_row(
                    "SELECT id, owner_task, owner_agent, expires_at, mode FROM leases
                     WHERE project_id = ?1 AND resource = ?2 AND state = 'active'",
                    params![project_id, resource],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .ok();
            if let Some((_, owner_task, owner_agent, expires_at, mode_text)) = existing {
                if owner_task == task_id {
                    continue; // re-entrant acquisition by the same task
                }
                let conflict = *mode == LeaseMode::ExclusiveWrite
                    || mode_text == LeaseMode::ExclusiveWrite.as_str();
                if conflict {
                    return Ok(AcquireOutcome::Waiting {
                        resource: resource.clone(),
                        held_by_task: owner_task,
                        held_by_agent: owner_agent,
                        expires_at,
                        mode: mode_text,
                    });
                }
            }
        }
        let mut granted: Vec<Lease> = Vec::new();
        for (resource, mode) in requested {
            let already: Option<String> = conn
                .query_row(
                    "SELECT id FROM leases WHERE project_id = ?1 AND resource = ?2
                     AND state = 'active' AND owner_task = ?3",
                    params![project_id, resource, task_id],
                    |row| row.get(0),
                )
                .ok();
            if let Some(existing_id) = already {
                let mut lease = lease_by_id(&conn, &existing_id)?;
                conn.execute(
                    "UPDATE leases SET expires_at = ?2 WHERE id = ?1",
                    params![existing_id, now + self.ttl_ms],
                )?;
                lease.expires_at = now + self.ttl_ms;
                granted.push(lease);
                continue;
            }
            let fencing = next_token(&conn)?;
            let lease = Lease {
                id: uuid::Uuid::new_v4().to_string(),
                project_id: project_id.to_string(),
                resource,
                mode,
                owner_agent: agent_id.map(str::to_string),
                owner_task: task_id.to_string(),
                fencing_token: fencing,
                state: "active".to_string(),
                acquired_at: now,
                expires_at: now + self.ttl_ms,
                released_at: None,
                release_reason: None,
            };
            // Written through the held connection: the store lock is not re-entrant, so
            // calling Store::insert_lease here would deadlock on the same connection.
            conn.execute(
                "INSERT INTO leases
                 (id, project_id, resource, mode, owner_agent, owner_task, fencing_token, state,
                  acquired_at, expires_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'active',?8,?9)",
                params![
                    lease.id,
                    lease.project_id,
                    lease.resource,
                    lease.mode.as_str(),
                    lease.owner_agent,
                    lease.owner_task,
                    lease.fencing_token as i64,
                    lease.acquired_at,
                    lease.expires_at
                ],
            )?;
            granted.push(lease);
        }
        Ok(AcquireOutcome::Acquired(granted))
    }

    /// Release every active lease held by a task (integration or cancellation).
    pub fn release_task(&self, task_id: &str, reason: &str) -> Result<usize> {
        let conn = self.store.raw_lock();
        let changed = conn.execute(
            "UPDATE leases SET state = 'released', released_at = ?2, release_reason = ?3
             WHERE owner_task = ?1 AND state = 'active'",
            params![task_id, now_ms(), reason],
        )?;
        Ok(changed)
    }

    /// Waiters: tasks that declared resources and are currently blocked on a lease.
    pub fn waiters(&self, project_id: &str) -> Result<Vec<serde_json::Value>> {
        let leases = self.store.leases_for_project(project_id)?;
        let mut held: BTreeMap<String, Lease> = BTreeMap::new();
        for lease in leases.iter().filter(|lease| lease.state == "active") {
            held.insert(lease.resource.clone(), lease.clone());
        }
        let mut output = Vec::new();
        for task in self.store.tasks_for_project(project_id)? {
            if task.status.is_terminal() {
                continue;
            }
            let requested: Vec<String> = task
                .resources
                .iter()
                .map(|raw| normalize_resource(raw))
                .collect();
            for resource in requested {
                if let Some(lease) = held.get(&resource) {
                    if lease.owner_task != task.id {
                        output.push(json!({
                            "taskId": task.id,
                            "taskTitle": task.title,
                            "resource": resource,
                            "heldByTask": lease.owner_task,
                            "heldByAgent": lease.owner_agent,
                            "expiresAt": lease.expires_at,
                            "fencingToken": lease.fencing_token,
                        }));
                    }
                }
            }
        }
        Ok(output)
    }
}

fn next_token(conn: &rusqlite::Connection) -> Result<u64> {
    let max: i64 = conn.query_row(
        "SELECT COALESCE(MAX(fencing_token), 0) FROM leases",
        [],
        |row| row.get(0),
    )?;
    Ok((max as u64) + 1)
}

fn lease_by_id(conn: &rusqlite::Connection, id: &str) -> Result<Lease> {
    let mut statement = conn.prepare("SELECT * FROM leases WHERE id = ?1")?;
    let lease = statement.query_row([id], |row| {
        let mode: String = row.get("mode")?;
        Ok(Lease {
            id: row.get("id")?,
            project_id: row.get("project_id")?,
            resource: row.get("resource")?,
            mode: LeaseMode::parse(&mode).unwrap_or(LeaseMode::ExclusiveWrite),
            owner_agent: row.get("owner_agent")?,
            owner_task: row.get("owner_task")?,
            fencing_token: row.get::<_, i64>("fencing_token")? as u64,
            state: row.get("state")?,
            acquired_at: row.get("acquired_at")?,
            expires_at: row.get("expires_at")?,
            released_at: row.get("released_at")?,
            release_reason: row.get("release_reason")?,
        })
    })?;
    Ok(lease)
}

/// Declared resources for a task: explicit ones plus the files it expects to touch.
pub fn resources_for(task: &Task) -> Vec<LeaseRequest> {
    let mut requests: Vec<LeaseRequest> = task
        .resources
        .iter()
        .map(|resource| LeaseRequest::write(resource.clone()))
        .collect();
    if requests.is_empty() {
        // Default: exclusive ownership of the task's own output files.
        for output in &task.outputs {
            requests.push(LeaseRequest::write(output.clone()));
        }
    }
    if requests.is_empty() {
        requests.push(LeaseRequest::write(format!("task:{}", task.id)));
    }
    requests
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn setup() -> (Store, String) {
        let store = Store::in_memory().unwrap();
        store
            .insert_project("p", "P", "G:/w", "main", None, &Default::default())
            .unwrap();
        (store, "p".to_string())
    }

    fn task(store: &Store, project: &str, id: &str, resources: &[&str]) {
        let now = now_ms();
        let task = Task {
            id: id.into(),
            project_id: project.into(),
            request_id: "r".into(),
            requirement_id: None,
            title: id.into(),
            description: String::new(),
            capability: "code".into(),
            role: "backend".into(),
            agent_id: None,
            status: crate::types::TaskStatus::Running,
            priority: 1,
            depends_on: vec![],
            inputs: vec![],
            outputs: vec![],
            acceptance: vec![],
            resources: resources.iter().map(|value| value.to_string()).collect(),
            max_retries: 2,
            attempts: 0,
            timeout_ms: 60_000,
            deadline_at: None,
            worktree: None,
            branch: None,
            result: None,
            blocker: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            ended_at: None,
        };
        store.insert_task(&task).unwrap();
    }

    #[test]
    fn exclusive_write_blocks_other_tasks_and_reports_the_holder() {
        let (store, project) = setup();
        task(&store, &project, "t1", &["src/lib.rs"]);
        task(&store, &project, "t2", &["./src/lib.rs"]);
        let manager = LeaseManager::new(&store, 60_000);
        let first = manager
            .acquire(
                &project,
                "t1",
                Some("a1"),
                &resources_for(&store.task("t1").unwrap()),
            )
            .unwrap();
        assert!(matches!(first, AcquireOutcome::Acquired(_)));
        let second = manager
            .acquire(
                &project,
                "t2",
                Some("a2"),
                &resources_for(&store.task("t2").unwrap()),
            )
            .unwrap();
        match second {
            AcquireOutcome::Waiting { held_by_task, .. } => assert_eq!(held_by_task, "t1"),
            other => panic!("expected waiting, got {other:?}"),
        }
    }

    #[test]
    fn shared_reads_coexist_but_a_writer_waits() {
        let (store, project) = setup();
        task(&store, &project, "t1", &["docs/a.md"]);
        task(&store, &project, "t2", &["docs/a.md"]);
        task(&store, &project, "t3", &["docs/a.md"]);
        let manager = LeaseManager::new(&store, 60_000);
        manager
            .acquire(&project, "t1", None, &[LeaseRequest::read("docs/a.md")])
            .unwrap();
        manager
            .acquire(&project, "t2", None, &[LeaseRequest::read("docs/a.md")])
            .unwrap();
        let outcome = manager
            .acquire(&project, "t3", None, &[LeaseRequest::write("docs/a.md")])
            .unwrap();
        assert!(matches!(outcome, AcquireOutcome::Waiting { .. }));
    }

    #[test]
    fn release_and_expiry_free_the_resource() {
        let (store, project) = setup();
        task(&store, &project, "t1", &["a"]);
        task(&store, &project, "t2", &["a"]);
        let manager = LeaseManager::new(&store, 1);
        manager
            .acquire(&project, "t1", None, &[LeaseRequest::write("a")])
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let outcome = manager
            .acquire(&project, "t2", None, &[LeaseRequest::write("a")])
            .unwrap();
        assert!(matches!(outcome, AcquireOutcome::Acquired(_)));
        let released = manager.release_task("t2", "test").unwrap();
        assert_eq!(released, 1);
    }

    #[test]
    fn fencing_tokens_are_monotonic() {
        let (store, project) = setup();
        task(&store, &project, "t1", &["a", "b"]);
        let manager = LeaseManager::new(&store, 60_000);
        let granted = manager
            .acquire(
                &project,
                "t1",
                None,
                &[LeaseRequest::write("a"), LeaseRequest::write("b")],
            )
            .unwrap();
        if let AcquireOutcome::Acquired(leases) = granted {
            assert_eq!(leases.len(), 2);
            assert!(leases[0].fencing_token < leases[1].fencing_token);
        } else {
            panic!("expected acquisition");
        }
    }
}
