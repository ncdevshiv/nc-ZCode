//! Task and agent repositories (the two operands the scheduler mutates).

use rusqlite::{params, Row};
use serde_json::Value;

use crate::db::{from_text, json_vec, now_ms, to_text, Store};
use crate::error::{NcagError, Result};
use crate::repo::acceptance_from;
use crate::types::{Agent, AgentStatus, Task, TaskStatus};

fn task_from_row(row: &Row<'_>) -> rusqlite::Result<Task> {
    let status_text: String = row.get("status")?;
    let acceptance_text: String = row.get("acceptance_json")?;
    let result_text: Option<String> = row.get("result_json")?;
    Ok(Task {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        request_id: row.get("request_id")?,
        requirement_id: row.get("requirement_id")?,
        title: row.get("title")?,
        description: row.get("description")?,
        capability: row.get("capability")?,
        role: row.get("role")?,
        agent_id: row.get("agent_id")?,
        status: TaskStatus::parse(&status_text).unwrap_or(TaskStatus::Created),
        priority: row.get("priority")?,
        depends_on: json_vec(&row.get::<_, String>("depends_on_json")?),
        inputs: json_vec(&row.get::<_, String>("inputs_json")?),
        outputs: json_vec(&row.get::<_, String>("outputs_json")?),
        acceptance: acceptance_from(&from_text(&acceptance_text)),
        resources: json_vec(&row.get::<_, String>("resources_json")?),
        max_retries: row.get::<_, i64>("max_retries")? as u32,
        attempts: row.get::<_, i64>("attempts")? as u32,
        timeout_ms: row.get::<_, i64>("timeout_ms")? as u64,
        deadline_at: row.get("deadline_at")?,
        worktree: row.get("worktree")?,
        branch: row.get("branch")?,
        result: result_text.as_deref().map(from_text),
        blocker: row.get("blocker")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
    })
}

fn agent_from_row(row: &Row<'_>) -> rusqlite::Result<Agent> {
    let status_text: String = row.get("status")?;
    Ok(Agent {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        task_id: row.get("task_id")?,
        profile_id: row.get("profile_id")?,
        role: row.get("role")?,
        status: AgentStatus::parse(&status_text).unwrap_or(AgentStatus::Created),
        persona: row.get("persona")?,
        capabilities: json_vec(&row.get::<_, String>("capabilities_json")?),
        permissions: json_vec(&row.get::<_, String>("permissions_json")?),
        model: row.get("model")?,
        provider: row.get("provider")?,
        workspace: row.get("workspace")?,
        worktree: row.get("worktree")?,
        last_heartbeat: row.get("last_heartbeat")?,
        last_activity: row.get("last_activity")?,
        progress_marker: row.get("progress_marker")?,
        usage_input: row.get::<_, i64>("usage_input")? as u64,
        usage_output: row.get::<_, i64>("usage_output")? as u64,
        cost_usd: row.get("cost_usd")?,
        error: row.get("error")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        ended_at: row.get("ended_at")?,
    })
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn insert_task(&self, task: &Task) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO tasks
             (id, project_id, request_id, requirement_id, title, description, capability, role,
              agent_id, status, priority, depends_on_json, inputs_json, outputs_json,
              acceptance_json, resources_json, max_retries, attempts, timeout_ms, deadline_at,
              worktree, branch, result_json, blocker, created_at, updated_at, started_at, ended_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,
                     ?21,?22,?23,?24,?25,?26,?27,?28)",
            params![
                task.id,
                task.project_id,
                task.request_id,
                task.requirement_id,
                task.title,
                task.description,
                task.capability,
                task.role,
                task.agent_id,
                task.status.as_str(),
                task.priority,
                to_text(&serde_json::json!(task.depends_on)),
                to_text(&serde_json::json!(task.inputs)),
                to_text(&serde_json::json!(task.outputs)),
                to_text(&serde_json::to_value(&task.acceptance)?),
                to_text(&serde_json::json!(task.resources)),
                task.max_retries as i64,
                task.attempts as i64,
                task.timeout_ms as i64,
                task.deadline_at,
                task.worktree,
                task.branch,
                task.result.as_ref().map(to_text),
                task.blocker,
                task.created_at,
                task.updated_at,
                task.started_at,
                task.ended_at
            ],
        )?;
        Ok(())
    }

    pub fn task(&self, id: &str) -> Result<Task> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM tasks WHERE id = ?1")?;
        statement
            .query_row([id], task_from_row)
            .ok()
            .ok_or_else(|| NcagError::not_found(format!("task {id} does not exist")))
    }

    pub fn tasks_for_project(&self, project_id: &str) -> Result<Vec<Task>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare(
            "SELECT * FROM tasks WHERE project_id = ?1 ORDER BY priority DESC, created_at ASC",
        )?;
        let rows = statement.query_map([project_id], task_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn tasks_for_request(&self, request_id: &str) -> Result<Vec<Task>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare(
            "SELECT * FROM tasks WHERE request_id = ?1 ORDER BY priority DESC, created_at ASC",
        )?;
        let rows = statement.query_map([request_id], task_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn all_tasks(&self) -> Result<Vec<Task>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM tasks ORDER BY created_at ASC")?;
        let rows = statement.query_map([], task_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn set_task_status(&self, id: &str, status: TaskStatus) -> Result<()> {
        let conn = self.raw_lock();
        let mut started = String::new();
        let mut ended = String::new();
        if status == TaskStatus::Running {
            started = format!(", started_at = COALESCE(started_at, {})", now_ms());
        }
        if status.is_terminal() {
            ended = format!(", ended_at = {}", now_ms());
        }
        conn.execute(
            &format!("UPDATE tasks SET status = ?2, updated_at = ?3{started}{ended} WHERE id = ?1"),
            params![id, status.as_str(), now_ms()],
        )?;
        Ok(())
    }

    pub fn assign_task(
        &self,
        id: &str,
        agent_id: &str,
        worktree: &str,
        branch: &str,
    ) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE tasks SET agent_id = ?2, worktree = ?3, branch = ?4, status = 'assigned',
             updated_at = ?5, started_at = COALESCE(started_at, ?5) WHERE id = ?1",
            params![id, agent_id, worktree, branch, now_ms()],
        )?;
        Ok(())
    }

    pub fn bump_task_attempt(&self, id: &str) -> Result<u32> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE tasks SET attempts = attempts + 1, updated_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        let attempts: i64 =
            conn.query_row("SELECT attempts FROM tasks WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;
        Ok(attempts as u32)
    }

    pub fn set_task_result(&self, id: &str, result: &Value) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE tasks SET result_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, to_text(result), now_ms()],
        )?;
        Ok(())
    }

    pub fn set_task_blocker(&self, id: &str, blocker: Option<&str>) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE tasks SET blocker = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, blocker, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_task_dependencies(&self, id: &str, depends_on: &[String]) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE tasks SET depends_on_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, to_text(&serde_json::json!(depends_on)), now_ms()],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_agent(
        &self,
        id: &str,
        project_id: &str,
        task_id: &str,
        profile_id: &str,
        role: &str,
        persona: &str,
        capabilities: &[String],
        permissions: &[String],
        model: Option<&str>,
    ) -> Result<()> {
        let now = now_ms();
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO agents
             (id, project_id, task_id, profile_id, role, status, persona, capabilities_json,
              permissions_json, model, last_heartbeat, last_activity, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,'created',?6,?7,?8,?9,?10,?10,?10,?10)",
            params![
                id,
                project_id,
                task_id,
                profile_id,
                role,
                persona,
                to_text(&serde_json::json!(capabilities)),
                to_text(&serde_json::json!(permissions)),
                model,
                now
            ],
        )?;
        Ok(())
    }

    pub fn agent(&self, id: &str) -> Result<Agent> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM agents WHERE id = ?1")?;
        statement
            .query_row([id], agent_from_row)
            .ok()
            .ok_or_else(|| NcagError::not_found(format!("agent {id} does not exist")))
    }

    pub fn agents_for_project(&self, project_id: &str) -> Result<Vec<Agent>> {
        let conn = self.raw_lock();
        let mut statement =
            conn.prepare("SELECT * FROM agents WHERE project_id = ?1 ORDER BY created_at ASC")?;
        let rows = statement.query_map([project_id], agent_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn all_agents(&self) -> Result<Vec<Agent>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM agents ORDER BY created_at ASC")?;
        let rows = statement.query_map([], agent_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn set_agent_status(&self, id: &str, status: AgentStatus) -> Result<()> {
        let conn = self.raw_lock();
        let ended = if status.is_terminal() {
            format!(", ended_at = {}", now_ms())
        } else {
            String::new()
        };
        conn.execute(
            &format!("UPDATE agents SET status = ?2, updated_at = ?3{ended} WHERE id = ?1"),
            params![id, status.as_str(), now_ms()],
        )?;
        Ok(())
    }

    pub fn heartbeat(&self, id: &str, marker: Option<&str>) -> Result<()> {
        let now = now_ms();
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE agents SET last_heartbeat = ?2, last_activity = ?2,
             progress_marker = COALESCE(?3, progress_marker), updated_at = ?2 WHERE id = ?1",
            params![id, now, marker],
        )?;
        Ok(())
    }

    pub fn set_agent_worktree(
        &self,
        id: &str,
        worktree: &str,
        provider: &str,
        model: &str,
    ) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE agents SET worktree = ?2, provider = ?3, model = ?4, updated_at = ?5
             WHERE id = ?1",
            params![id, worktree, provider, model, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_agent_error(&self, id: &str, error: &str, status: AgentStatus) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE agents SET error = ?2, status = ?3, updated_at = ?4, ended_at = ?4 WHERE id = ?1",
            params![id, error, status.as_str(), now_ms()],
        )?;
        Ok(())
    }

    /// Clear a recovered agent's last error. The error column is the *current* failure
    /// fact: an agent that went through recovery and then finished its task must not keep
    /// advertising the old one (the UI read it as "idle · recovery exhausted").
    pub fn clear_agent_error(&self, id: &str) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE agents SET error = NULL, updated_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn add_agent_usage(&self, id: &str, input: u64, output: u64, cost: f64) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE agents SET usage_input = usage_input + ?2, usage_output = usage_output + ?3,
             cost_usd = cost_usd + ?4, updated_at = ?5 WHERE id = ?1",
            params![id, input as i64, output as i64, cost, now_ms()],
        )?;
        Ok(())
    }
}

pub fn task_json(task: &Task) -> Value {
    serde_json::to_value(task).unwrap_or(Value::Null)
}
