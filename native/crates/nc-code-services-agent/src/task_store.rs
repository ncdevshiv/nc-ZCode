//! Task membership store: pinned / archived / deleted / unread / title overrides,
//! task groups and grouped-view ordering.
//!
//! Persistence is a single JSON file under the app config dir. The TS backend used
//! `tasks-index.sqlite`; the Rust host keeps the same *facts* (membership rows that
//! sessions-index cannot express) in an atomically replaced document. Session
//! existence/title/activity still comes from the host session store — this file only
//! holds what the user did to a task.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nc_code_service_core::atomic::atomic_write_text;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

pub const TASK_INDEX_FILE: &str = "tasks-index-rust-v1.json";
pub const ROOT_SCOPE_KEY: &str = "__root__";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskIndexFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub tasks: BTreeMap<String, TaskRow>,
    #[serde(default)]
    pub groups: Vec<TaskGroup>,
    /// groupId → members (order-agnostic; sorted by sortOrder/addedAt on read).
    #[serde(default)]
    pub members: BTreeMap<String, Vec<GroupMember>>,
    /// workspaceKey → the ordering of the top-level nodes (a task or a group).
    #[serde(default)]
    pub top_level_orders: BTreeMap<String, Vec<TopLevelNode>>,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaskRow {
    pub task_id: String,
    pub workspace_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_override: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_overridden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_automation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_peak_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selection: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_epoch: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Value>,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGroup {
    pub id: String,
    pub title: String,
    pub color: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMember {
    pub workspace_key: String,
    pub workspace_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_identity: Option<String>,
    pub task_id: String,
    pub sort_order: Option<f64>,
    pub added_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TopLevelNode {
    Group {
        group_id: String,
        sort_order: f64,
    },
    Task {
        workspace_key: String,
        task_id: String,
        sort_order: f64,
    },
}

pub struct TaskIndexStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl TaskIndexStore {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            path: config_dir.join(TASK_INDEX_FILE),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn load(&self) -> Result<TaskIndexFile, std::io::Error> {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(text) => serde_json::from_str(&text).map_err(|error| {
                std::io::Error::other(format!(
                    "task index corrupt at {}: {error}",
                    self.path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TaskIndexFile {
                version: 1,
                ..TaskIndexFile::default()
            }),
            Err(error) => Err(error),
        }
    }

    async fn save(&self, index: &TaskIndexFile) -> Result<(), std::io::Error> {
        let text = serde_json::to_string_pretty(index)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        atomic_write_text(&self.path, &text).await
    }

    /// read-modify-write transaction: this is the only write path for the whole file (single process, single writer).
    pub async fn mutate<T>(
        &self,
        apply: impl FnOnce(&mut TaskIndexFile) -> Result<T, String>,
    ) -> Result<T, String> {
        let _guard = self.lock.lock().await;
        let mut index = self.load().await.map_err(|error| error.to_string())?;
        let value = apply(&mut index)?;
        self.save(&index).await.map_err(|error| error.to_string())?;
        Ok(value)
    }

    pub async fn snapshot(&self) -> Result<TaskIndexFile, String> {
        let _guard = self.lock.lock().await;
        self.load().await.map_err(|error| error.to_string())
    }
}

/// The workspaceKey convention: identity first, otherwise the path (the AGENTS.md Workspace Identity rule).
pub fn workspace_key(workspace_path: &str, workspace_identity: Option<&str>) -> String {
    workspace_identity
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(workspace_path)
        .to_string()
}

pub fn scope_key(workspace_path: &str, workspace_identity: Option<&str>) -> String {
    workspace_key(workspace_path, workspace_identity)
}

pub fn default_group_color() -> &'static str {
    "gray"
}

pub fn serialize_meta(row: &TaskRow, session: &SessionFacts) -> Value {
    let title = row
        .title_override
        .clone()
        .unwrap_or_else(|| session.title.clone());
    let mut meta = json!({
        "taskId": row.task_id,
        "traceId": session.trace_id.clone().unwrap_or_else(|| row.task_id.clone()),
        "title": title,
        "workspacePath": row.workspace_path,
        "createdAt": session.created_at,
        "updatedAt": session.updated_at.max(row.updated_at),
        "mode": row.mode.clone().unwrap_or_else(|| "build".to_string()),
        "provider": row
            .provider
            .clone()
            .or_else(|| session.provider.clone())
            .unwrap_or_else(|| "glm".to_string()),
        "status": session.status.clone().unwrap_or_else(|| "idle".to_string()),
    });
    if let Some(identity) = &row.workspace_identity {
        meta["workspaceIdentity"] = json!(identity);
    }
    if row.title_overridden.unwrap_or(false) {
        meta["titleOverridden"] = json!(true);
    }
    if let Some(model) = &row.model_selection {
        if let Some(model_id) = model.get("modelId").and_then(Value::as_str) {
            meta["model"] = json!(model_id);
        } else if let Some(model_id) = model.get("model").and_then(Value::as_str) {
            meta["model"] = json!(model_id);
        }
    }
    if let Some(thought) = &row.thought_level {
        meta["thoughtLevel"] = json!(thought);
    }
    if let Some(value) = row.runtime_epoch {
        meta["runtimeEpoch"] = json!(value);
    }
    if let Some(value) = &row.cron_automation_id {
        meta["cronAutomationId"] = json!(value);
    }
    if let Some(value) = &row.off_peak_task_id {
        meta["offPeakTaskId"] = json!(value);
    }
    if let Some(value) = &row.forked_from_task_id {
        meta["forkedFromTaskId"] = json!(value);
    }
    if let Some(value) = &row.migration_source {
        meta["migrationSource"] = json!(value);
    }
    if let Some(value) = row.unread_at {
        meta["unreadAt"] = json!(value);
    }
    if let Some(value) = &session.pending_interaction {
        meta["pendingInteraction"] = value.clone();
    }
    if let Some(value) = &session.last_error {
        meta["lastError"] = value.clone();
    }
    if let Some(value) = &row.target {
        meta["target"] = value.clone();
    }
    meta
}

/// Session facts (from the host session / meta.json); the user-side membership has a separate owner.
#[derive(Debug, Clone, Default)]
pub struct SessionFacts {
    pub task_id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub status: Option<String>,
    pub trace_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thought: Option<String>,
    pub pending_interaction: Option<Value>,
    pub last_error: Option<Value>,
}

/// Derive facts from the session view of `workspace_session_facts` (phase → persisted status uses one shared mapping).
pub fn session_facts_from_view(view: &Value, workspace_path: &str) -> SessionFacts {
    let _ = workspace_path;
    let phase = view.get("phase").and_then(Value::as_str).unwrap_or("");
    let status = match phase {
        "running" | "prewarming" | "draft" => Some("running".to_string()),
        "error" => Some("error".to_string()),
        "completedSuccess" | "completedInterrupted" => Some("completed".to_string()),
        _ => None,
    };
    let pending_interaction = view
        .get("pendingInteractions")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find(|item| {
                matches!(
                    item.get("kind").and_then(Value::as_str),
                    Some("permission") | Some("userInput")
                )
            })
        })
        .map(|item| {
            let mut value = json!({
                "interactionId": item.get("interactionId").cloned().unwrap_or(Value::Null),
                "kind": item.get("kind").cloned().unwrap_or(Value::Null),
            });
            if let Some(tool_name) = item
                .get("payload")
                .and_then(|payload| payload.get("toolName"))
                .and_then(Value::as_str)
            {
                value["toolName"] = json!(tool_name);
            }
            value
        });
    SessionFacts {
        task_id: view
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: view
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        created_at: view.get("createdAt").and_then(Value::as_i64).unwrap_or(0),
        updated_at: view.get("updatedAt").and_then(Value::as_i64).unwrap_or(0),
        status,
        trace_id: Some(
            view.get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        provider: view
            .get("provider")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        model: view
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        thought: view
            .get("thought")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        pending_interaction,
        last_error: view.get("lastError").cloned(),
    }
}

pub fn find_group<'a>(index: &'a TaskIndexFile, group_id: &str) -> Option<&'a TaskGroup> {
    index.groups.iter().find(|group| group.id == group_id)
}

pub fn top_level_scope(index: &TaskIndexFile, workspace: &str) -> Vec<TopLevelNode> {
    index
        .top_level_orders
        .get(workspace)
        .cloned()
        .unwrap_or_default()
}

/// A new task enters at the end of the top-level ordering (the dual of TS `initializeGroupedTaskAtTop`).
pub fn push_top_level_task(index: &mut TaskIndexFile, workspace: &str, task_id: &str) {
    let nodes = index
        .top_level_orders
        .entry(workspace.to_string())
        .or_default();
    if nodes.iter().any(|node| match node {
        TopLevelNode::Task { task_id: id, .. } => id == task_id,
        TopLevelNode::Group { .. } => false,
    }) {
        return;
    }
    let next = nodes.iter().map(node_order).fold(0.0_f64, f64::max) + 1.0;
    nodes.push(TopLevelNode::Task {
        workspace_key: workspace.to_string(),
        task_id: task_id.to_string(),
        sort_order: next,
    });
}

pub fn remove_top_level_task(index: &mut TaskIndexFile, workspace: &str, task_id: &str) {
    if let Some(nodes) = index.top_level_orders.get_mut(workspace) {
        nodes.retain(|node| match node {
            TopLevelNode::Task { task_id: id, .. } => id != task_id,
            TopLevelNode::Group { .. } => true,
        });
    }
}

pub fn node_order(node: &TopLevelNode) -> f64 {
    match node {
        TopLevelNode::Group { sort_order, .. } => *sort_order,
        TopLevelNode::Task { sort_order, .. } => *sort_order,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (TaskIndexStore, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-task-index-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        (TaskIndexStore::new(&root), root)
    }

    #[tokio::test]
    async fn membership_round_trips_through_atomic_file() {
        let (store, root) = temp_store("roundtrip");
        store
            .mutate(|index| {
                index.tasks.insert(
                    "t1".to_string(),
                    TaskRow {
                        task_id: "t1".to_string(),
                        workspace_path: "/ws".to_string(),
                        pinned: true,
                        archived: false,
                        updated_at: 7,
                        ..TaskRow::default()
                    },
                );
                Ok(())
            })
            .await
            .expect("mutate");
        let snapshot = store.snapshot().await.expect("snapshot");
        assert!(snapshot.tasks["t1"].pinned);
        assert_eq!(snapshot.tasks["t1"].updated_at, 7);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn corrupt_file_fails_closed() {
        let (store, root) = temp_store("corrupt");
        std::fs::create_dir_all(&root).expect("dir");
        std::fs::write(store.path(), "{ nope").expect("write");
        let error = store.snapshot().await.expect_err("corrupt must fail");
        assert!(error.contains("corrupt"), "{error}");
        assert_eq!(
            std::fs::read_to_string(store.path()).expect("read"),
            "{ nope"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn top_level_order_appends_monotonically() {
        let mut index = TaskIndexFile::default();
        push_top_level_task(&mut index, "/ws", "a");
        push_top_level_task(&mut index, "/ws", "b");
        push_top_level_task(&mut index, "/ws", "a");
        let nodes = index.top_level_orders.get("/ws").expect("nodes");
        assert_eq!(nodes.len(), 2);
        assert!(node_order(&nodes[1]) > node_order(&nodes[0]));
    }
}
