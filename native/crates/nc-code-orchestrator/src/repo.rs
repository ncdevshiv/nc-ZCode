//! Repository layer for projects, requests and requirements.
//!
//! Every function takes `&Store`; mapping lives here so the orchestrator never builds
//! SQL strings of its own (spec: specs/nc-ag-orchestration.md section 2).

use rusqlite::{params, Row};
use serde_json::Value;

use crate::db::{from_text, json_vec, now_ms, to_text, Store};
use crate::error::{NcagError, Result};
use crate::types::{
    AcceptanceCheck, Project, ProjectConfig, ProjectStatus, Request, RequestStatus, Requirement,
};

pub fn project_from_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    let config_text: String = row.get("config_json")?;
    let status_text: String = row.get("status")?;
    Ok(Project {
        id: row.get("id")?,
        name: row.get("name")?,
        workspace_path: row.get("workspace_path")?,
        base_branch: row.get("base_branch")?,
        base_commit: row.get("base_commit")?,
        status: ProjectStatus::parse(&status_text).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(3, "status".into(), rusqlite::types::Type::Text)
        })?,
        config: serde_json::from_str(&config_text).unwrap_or_default(),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub fn request_from_row(row: &Row<'_>) -> rusqlite::Result<Request> {
    let status_text: String = row.get("status")?;
    let analysis_text: Option<String> = row.get("analysis_json")?;
    let plan_text: Option<String> = row.get("plan_json")?;
    Ok(Request {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        instruction: row.get("instruction")?,
        status: RequestStatus::parse(&status_text).unwrap_or(RequestStatus::Received),
        planner: row.get("planner")?,
        analysis: analysis_text.as_deref().map(from_text),
        plan: plan_text.as_deref().map(from_text),
        trace_id: row.get("trace_id")?,
        error: row.get("error")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub fn requirement_from_row(row: &Row<'_>) -> rusqlite::Result<Requirement> {
    let acceptance_text: String = row.get("acceptance_json")?;
    Ok(Requirement {
        id: row.get("id")?,
        request_id: row.get("request_id")?,
        code: row.get("code")?,
        description: row.get("description")?,
        category: row.get("category")?,
        priority: row.get("priority")?,
        acceptance: serde_json::from_str(&acceptance_text).unwrap_or_default(),
        verification: row.get("verification")?,
        source_excerpt: row.get("source_excerpt")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
    })
}

impl Store {
    pub fn insert_project(
        &self,
        id: &str,
        name: &str,
        workspace_path: &str,
        base_branch: &str,
        base_commit: Option<&str>,
        config: &ProjectConfig,
    ) -> Result<Project> {
        let now = now_ms();
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO projects
             (id, name, workspace_path, base_branch, base_commit, status, config_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7, ?7)",
            params![
                id,
                name,
                workspace_path,
                base_branch,
                base_commit,
                to_text(&serde_json::to_value(config)?),
                now
            ],
        )?;
        drop(conn);
        self.project(id)
    }

    pub fn project(&self, id: &str) -> Result<Project> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM projects WHERE id = ?1")?;
        let row = statement.query_row([id], project_from_row).ok();
        row.ok_or_else(|| NcagError::not_found(format!("project {id} does not exist")))
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM projects ORDER BY created_at ASC")?;
        let rows = statement.query_map([], project_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn update_project_status(&self, id: &str, status: ProjectStatus) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE projects SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, status.as_str(), now_ms()],
        )?;
        Ok(())
    }

    pub fn update_project_config(&self, id: &str, config: &ProjectConfig) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE projects SET config_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, to_text(&serde_json::to_value(config)?), now_ms()],
        )?;
        Ok(())
    }

    pub fn update_project_base(&self, id: &str, base_commit: &str) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE projects SET base_commit = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, base_commit, now_ms()],
        )?;
        Ok(())
    }

    pub fn insert_request(
        &self,
        id: &str,
        project_id: &str,
        instruction: &str,
        trace_id: &str,
    ) -> Result<()> {
        let now = now_ms();
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO requests
             (id, project_id, instruction, status, trace_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'received', ?4, ?5, ?5)",
            params![id, project_id, instruction, trace_id, now],
        )?;
        Ok(())
    }

    pub fn request(&self, id: &str) -> Result<Request> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM requests WHERE id = ?1")?;
        statement
            .query_row([id], request_from_row)
            .ok()
            .ok_or_else(|| NcagError::not_found(format!("request {id} does not exist")))
    }

    pub fn requests_for_project(&self, project_id: &str) -> Result<Vec<Request>> {
        let conn = self.raw_lock();
        let mut statement =
            conn.prepare("SELECT * FROM requests WHERE project_id = ?1 ORDER BY created_at ASC")?;
        let rows = statement.query_map([project_id], request_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn all_requests(&self) -> Result<Vec<Request>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare("SELECT * FROM requests ORDER BY created_at DESC")?;
        let rows = statement.query_map([], request_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn set_request_status(&self, id: &str, status: RequestStatus) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE requests SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, status.as_str(), now_ms()],
        )?;
        Ok(())
    }

    pub fn set_request_status_with_error(
        &self,
        id: &str,
        status: RequestStatus,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE requests SET status = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, status.as_str(), error, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_request_analysis(
        &self,
        id: &str,
        planner: &str,
        analysis: &Value,
        plan: Option<&Value>,
    ) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE requests SET planner = ?2, analysis_json = ?3, plan_json = ?4, updated_at = ?5
             WHERE id = ?1",
            params![id, planner, to_text(analysis), plan.map(to_text), now_ms()],
        )?;
        Ok(())
    }

    pub fn insert_requirement(&self, requirement: &Requirement) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "INSERT INTO requirements
             (id, request_id, code, description, category, priority, acceptance_json, verification,
              source_excerpt, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                requirement.id,
                requirement.request_id,
                requirement.code,
                requirement.description,
                requirement.category,
                requirement.priority,
                to_text(&serde_json::to_value(&requirement.acceptance)?),
                requirement.verification,
                requirement.source_excerpt,
                requirement.status,
                requirement.created_at
            ],
        )?;
        Ok(())
    }

    pub fn requirements_for_request(&self, request_id: &str) -> Result<Vec<Requirement>> {
        let conn = self.raw_lock();
        let mut statement = conn.prepare(
            "SELECT * FROM requirements WHERE request_id = ?1 ORDER BY priority DESC, created_at ASC",
        )?;
        let rows = statement.query_map([request_id], requirement_from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn set_requirement_status(&self, id: &str, status: &str) -> Result<()> {
        let conn = self.raw_lock();
        conn.execute(
            "UPDATE requirements SET status = ?2 WHERE id = ?1",
            params![id, status],
        )?;
        Ok(())
    }
}

/// Parse the acceptance JSON of a plan/task into typed checks.
pub fn acceptance_from(value: &Value) -> Vec<AcceptanceCheck> {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

pub fn strings_from(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn json_value(text: &str) -> Value {
    from_text(text)
}

pub fn joined(values: &[String]) -> String {
    to_text(&Value::Array(
        values.iter().cloned().map(Value::String).collect(),
    ))
}

pub fn split(text: &str) -> Vec<String> {
    json_vec(text)
}
