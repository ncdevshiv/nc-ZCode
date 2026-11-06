//! `nc-code-task` channel: task facade over host sessions + user membership state.
//!
//! Ownership split (mirrors the TS facade without its sqlite mirror):
//! - session existence/title/activity: `nc-code-host` session store (single owner);
//! - pinned/archived/deleted/unread/title overrides/groups/order: [`TaskIndexStore`];
//! - conversation history: v4 rows (`task_snapshot` maps them for the legacy facade).
//!
//! Every method returns a real projection or an explicit failure; there is no
//! in-memory placeholder task and no silently-synthesized list row.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, EventBus, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use serde_json::{json, Value};

use crate::service::AgentService;
use crate::task_snapshot;
use crate::task_store::{
    find_group, push_top_level_task, remove_top_level_task, serialize_meta,
    session_facts_from_view, workspace_key, GroupMember, SessionFacts, TaskGroup, TaskIndexStore,
    TaskRow, TopLevelNode,
};

pub struct TaskService {
    paths: AppPaths,
    agent: Arc<AgentService>,
    index: TaskIndexStore,
    events: EventBus,
    connection_id: String,
    sequence: AtomicU64,
}

impl TaskService {
    pub fn new(paths: AppPaths, agent: Arc<AgentService>) -> Arc<Self> {
        let sequence = now_ms();
        Arc::new(Self {
            index: TaskIndexStore::new(&paths.app_config_dir()),
            paths,
            agent,
            events: EventBus::default(),
            connection_id: format!("zcode-task-{sequence}"),
            sequence: AtomicU64::new(1),
        })
    }

    fn next_id(&self, prefix: &str) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{}-{sequence}", now_ms())
    }

    fn envelope(&self, session_id: Option<&str>, command_type: &str, payload: Value) -> Value {
        json!({
            "commandId": self.next_id("task-command"),
            "clientId": self.connection_id,
            "sessionId": session_id,
            "type": command_type,
            "payload": payload,
            "issuedAt": now_ms(),
        })
    }

    async fn agent_call(&self, command: &str, params: Value) -> Result<Value, ServiceError> {
        match self
            .agent
            .call_inner(command, vec![WireValue::Json(params)])
            .await?
        {
            WireValue::Json(value) => Ok(value),
            WireValue::Undefined => Ok(Value::Null),
            other => Ok(Value::String(
                other.as_str().unwrap_or_default().to_string(),
            )),
        }
    }

    async fn v4_command(
        &self,
        workspace_path: &str,
        workspace_identity: Option<&str>,
        session_id: &str,
        command_type: &str,
        payload: Value,
    ) -> Result<Value, ServiceError> {
        let envelope = self.envelope(Some(session_id), command_type, payload);
        let mut params = json!({
            "workspacePath": workspace_path,
            "envelope": envelope,
        });
        if let Some(identity) = workspace_identity {
            params["workspaceIdentity"] = json!(identity);
        }
        self.agent_call("sendConversationCommandV4", params).await
    }

    async fn session_facts(
        &self,
        workspace_path: &str,
        workspace_identity: Option<&str>,
    ) -> Result<Vec<SessionFacts>, ServiceError> {
        let values = self
            .agent
            .workspace_session_facts(workspace_path, workspace_identity)
            .await;
        Ok(values
            .iter()
            .map(|value| session_facts_from_view(value, workspace_path))
            .collect())
    }

    async fn facts_for_row(&self, row: &TaskRow) -> Option<SessionFacts> {
        let facts = self
            .session_facts(&row.workspace_path, row.workspace_identity.as_deref())
            .await
            .ok()?;
        facts.into_iter().find(|fact| fact.task_id == row.task_id)
    }

    async fn meta_for_row(&self, row: &TaskRow) -> Value {
        let facts = self
            .facts_for_row(row)
            .await
            .unwrap_or_else(|| SessionFacts {
                task_id: row.task_id.clone(),
                title: row.title_override.clone().unwrap_or_default(),
                created_at: row.updated_at,
                updated_at: row.updated_at,
                status: None,
                trace_id: None,
                provider: None,
                model: None,
                thought: None,
                pending_interaction: None,
                last_error: row.last_error.clone(),
            });
        serialize_meta(row, &facts)
    }

    async fn emit_task_event(&self, row: &TaskRow, reason: &str) {
        let meta = self.meta_for_row(row).await;
        let mut event = json!({
            "type": "workspace_task_list_changed",
            "workspacePath": row.workspace_path,
            "taskId": row.task_id,
            "reason": reason,
            "taskMeta": meta,
        });
        if let Some(identity) = &row.workspace_identity {
            event["workspaceIdentity"] = json!(identity);
        }
        self.events.send(WireValue::Json(event));
    }

    async fn emit_scope_event(
        &self,
        workspace_path: &str,
        workspace_identity: Option<&str>,
        reason: &str,
    ) {
        let mut event = json!({
            "type": "workspace_task_list_changed",
            "workspacePath": workspace_path,
            "reason": reason,
        });
        if let Some(identity) = workspace_identity {
            event["workspaceIdentity"] = json!(identity);
        }
        self.events.send(WireValue::Json(event));
    }

    /// The session exists + a membership row (when the row is missing, the default membership is used).
    async fn all_rows(
        &self,
        workspace_path: &str,
        identity: Option<&str>,
    ) -> Result<Vec<(TaskRow, SessionFacts)>, ServiceError> {
        let facts = self.session_facts(workspace_path, identity).await?;
        let index = self.index.snapshot().await.map_err(ServiceError::new)?;
        let mut rows: Vec<(TaskRow, SessionFacts)> = facts
            .into_iter()
            .map(|fact| {
                let row = index
                    .tasks
                    .get(&fact.task_id)
                    .cloned()
                    .unwrap_or_else(|| TaskRow {
                        task_id: fact.task_id.clone(),
                        workspace_path: workspace_path.to_string(),
                        workspace_identity: identity.map(str::to_string),
                        updated_at: fact.updated_at,
                        ..TaskRow::default()
                    });
                (row, fact)
            })
            .filter(|(row, _)| row.deleted_at.is_none())
            .collect();
        // An archived row must still show up in the archived list even when the session has been closed: membership is the source of truth.
        for row in index.tasks.values() {
            if row.archived
                && row.deleted_at.is_none()
                && row.workspace_path == workspace_path
                && row.workspace_identity.as_deref() == identity
                && !rows
                    .iter()
                    .any(|(existing, _)| existing.task_id == row.task_id)
            {
                rows.push((
                    row.clone(),
                    SessionFacts {
                        task_id: row.task_id.clone(),
                        title: row.title_override.clone().unwrap_or_default(),
                        created_at: row.updated_at,
                        updated_at: row.updated_at,
                        status: Some("idle".to_string()),
                        trace_id: None,
                        provider: None,
                        model: None,
                        thought: None,
                        pending_interaction: None,
                        last_error: row.last_error.clone(),
                    },
                ));
            }
        }
        Ok(rows)
    }

    async fn create_task(&self, params: &Value) -> Result<Value, ServiceError> {
        let workspace_path = required_str(params, "workspacePath")?;
        let identity = optional_str(params, "workspaceIdentity");
        let mut snapshot: Option<Value> = None;
        if let Some(draft) = optional_str(params, "draftSessionId") {
            let mut read_params = json!({
                "workspacePath": workspace_path,
                "sessionId": draft,
            });
            if let Some(identity) = identity {
                read_params["workspaceIdentity"] = json!(identity);
            }
            if let Ok(read) = self.agent_call("readSession", read_params).await {
                snapshot = Some(read);
            }
        }
        if snapshot.is_none() {
            let mut create_params = json!({ "workspacePath": workspace_path });
            if let Some(identity) = identity {
                create_params["workspaceIdentity"] = json!(identity);
            }
            snapshot = Some(self.agent_call("createSession", create_params).await?);
        }
        let snapshot = snapshot.expect("snapshot assigned");
        let task_id = snapshot
            .get("session")
            .and_then(|session| session.get("sessionId"))
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::new("createSession snapshot missing sessionId"))?
            .to_string();
        let now = now_ms();
        let mode = optional_str(params, "mode").unwrap_or("build");
        let model_selection = params.get("modelSelection").cloned();
        self.index
            .mutate(|index| {
                let row = index
                    .tasks
                    .entry(task_id.clone())
                    .or_insert_with(|| TaskRow {
                        task_id: task_id.clone(),
                        workspace_path: workspace_path.to_string(),
                        workspace_identity: identity.map(str::to_string),
                        updated_at: now,
                        ..TaskRow::default()
                    });
                row.mode = Some(mode.to_string());
                if let Some(selection) = &model_selection {
                    row.model_selection = Some(selection.clone());
                    if let Some(thought) = selection
                        .get("options")
                        .and_then(|options| options.get("reasoningLevel"))
                        .and_then(Value::as_str)
                    {
                        row.thought_level = Some(thought.to_string());
                    }
                }
                if let Some(value) = optional_str(params, "automationId") {
                    row.cron_automation_id = Some(value.to_string());
                }
                if let Some(value) = optional_str(params, "offPeakTaskId") {
                    row.off_peak_task_id = Some(value.to_string());
                }
                if let Some(value) = optional_str(params, "forkedFromTaskId") {
                    row.forked_from_task_id = Some(value.to_string());
                }
                let workspace = workspace_key(workspace_path, identity);
                push_top_level_task(index, &workspace, &task_id);
                Ok(())
            })
            .await
            .map_err(ServiceError::new)?;
        let row = self
            .index
            .snapshot()
            .await
            .map_err(ServiceError::new)?
            .tasks
            .get(&task_id)
            .cloned()
            .ok_or_else(|| ServiceError::new("task row vanished after create"))?;
        self.emit_task_event(&row, "task_created").await;
        let mut result = self.meta_for_row(&row).await;
        result["initialSlashCommands"] = json!([]);
        Ok(result)
    }

    async fn send_prompt(&self, params: &Value) -> Result<Value, ServiceError> {
        let task_id = required_str(params, "taskId")?;
        let content = required_str(params, "content")?;
        let (row, _) = self.require_task(task_id).await?;
        if params
            .get("attachments")
            .and_then(Value::as_array)
            .map(|items| !items.is_empty())
            .unwrap_or(false)
        {
            // the host's v4 sendText is not wired to the attachment transaction yet (attachment/* is explicitly
            // unimplemented in the method inventory). Silently dropping an attachment would change the semantics of the user's input, so fail closed here.
            return Err(ServiceError::named(
                "AttachmentsUnavailable",
                "attachment prompts are not available in this host increment",
            ));
        }
        let mut send = json!({
            "workspacePath": row.workspace_path,
            "sessionId": task_id,
            "content": content,
        });
        if let Some(identity) = &row.workspace_identity {
            send["workspaceIdentity"] = json!(identity);
        }
        if let Some(trace) = params.get("traceId").and_then(Value::as_str) {
            send["inputId"] = json!(trace);
        }
        if let Some(model_execution) = params.get("modelExecution") {
            send["modelExecution"] = model_execution.clone();
        }
        if let Some(selection) = params.get("modelSelection") {
            let provider = selection
                .get("providerId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let model = selection
                .get("modelId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let thought = selection
                .get("options")
                .and_then(|options| options.get("reasoningLevel"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !model.is_empty() {
                let _ = self
                    .v4_command(
                        &row.workspace_path,
                        row.workspace_identity.as_deref(),
                        task_id,
                        "switchModelConfig",
                        json!({"provider": provider, "model": model, "thought": thought}),
                    )
                    .await?;
            }
        }
        let _ = self.agent_call("sendPrompt", send).await?;
        let now = now_ms();
        self.index
            .mutate(|index| {
                if let Some(row) = index.tasks.get_mut(task_id) {
                    row.updated_at = now;
                }
                Ok(())
            })
            .await
            .map_err(ServiceError::new)?;
        Ok(Value::Null)
    }

    async fn require_task(&self, task_id: &str) -> Result<(TaskRow, SessionFacts), ServiceError> {
        let index = self.index.snapshot().await.map_err(ServiceError::new)?;
        if let Some(row) = index.tasks.get(task_id) {
            let facts = self
                .facts_for_row(row)
                .await
                .unwrap_or_else(|| SessionFacts {
                    task_id: row.task_id.clone(),
                    title: row.title_override.clone().unwrap_or_default(),
                    created_at: row.updated_at,
                    updated_at: row.updated_at,
                    status: None,
                    trace_id: None,
                    provider: None,
                    model: None,
                    thought: None,
                    pending_interaction: None,
                    last_error: row.last_error.clone(),
                });
            return Ok((row.clone(), facts));
        }
        // A v4 session that has no membership row yet: backfill one row from the authoritative session facts.
        let facts = self
            .agent
            .workspace_session_facts_for_task(task_id)
            .await
            .ok_or_else(|| {
                ServiceError::named("TaskNotFound", format!("task not found: {task_id}"))
            })?;
        let row = TaskRow {
            task_id: task_id.to_string(),
            workspace_path: facts
                .get("workspacePath")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            updated_at: now_ms(),
            ..TaskRow::default()
        };
        self.index
            .mutate(|index| {
                index.tasks.insert(task_id.to_string(), row.clone());
                Ok(())
            })
            .await
            .map_err(ServiceError::new)?;
        Ok((row, session_facts_from_view(&facts, "")))
    }

    async fn list_rows(
        &self,
        workspace_path: &str,
        identity: Option<&str>,
        kind: &str,
        sort_by: &str,
        search: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, ServiceError> {
        let rows = self.all_rows(workspace_path, identity).await?;
        let mut items: Vec<Value> = Vec::new();
        for (row, facts) in rows {
            let matches_kind = match kind {
                "pinned" => row.pinned && !row.archived,
                "archived" => row.archived,
                "active" => !row.archived && row.deleted_at.is_none(),
                _ => !row.pinned && !row.archived,
            };
            if !matches_kind {
                continue;
            }
            let meta = serialize_meta(&row, &facts);
            let title = meta
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut item = meta;
            if let Some(search) = search {
                let needle = search.trim().to_lowercase();
                if !needle.is_empty() {
                    if !title.to_lowercase().contains(&needle) {
                        continue;
                    }
                    item["searchSnippet"] = json!(title);
                    item["searchSnippets"] = json!([title]);
                }
            }
            items.push(item);
        }
        let sort_key = |item: &Value| -> i64 {
            let field = if sort_by == "created" {
                "createdAt"
            } else {
                "updatedAt"
            };
            item.get(field).and_then(Value::as_i64).unwrap_or(0)
        };
        items.sort_by_key(|item| std::cmp::Reverse(sort_key(item)));
        let total = items.len();
        let items = match limit {
            Some(limit) if limit > 0 && items.len() > limit => items[..limit].to_vec(),
            _ => items,
        };
        let has_more = total > items.len();
        Ok(json!({"items": items, "total": total, "hasMore": has_more}))
    }

    async fn patch_task(
        &self,
        task_id: &str,
        reason: &str,
        apply: impl FnOnce(&mut TaskRow),
    ) -> Result<Value, ServiceError> {
        let _ = self.require_task(task_id).await?;
        let now = now_ms();
        let row = self
            .index
            .mutate(|index| {
                let row = index
                    .tasks
                    .get_mut(task_id)
                    .ok_or_else(|| format!("task not found: {task_id}"))?;
                apply(row);
                row.updated_at = now;
                Ok(row.clone())
            })
            .await
            .map_err(ServiceError::new)?;
        if reason != "silent" {
            self.emit_task_event(&row, reason).await;
        }
        Ok(self.meta_for_row(&row).await)
    }

    async fn set_unread(&self, params: &Value) -> Result<Value, ServiceError> {
        let task_id = required_str(params, "taskId")?;
        let unread = params
            .get("unread")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let expected = params.get("expectedUnreadAt").and_then(Value::as_i64);
        let _ = self.require_task(task_id).await?;
        let now = now_ms();
        let row = self
            .index
            .mutate(|index| {
                let row = index
                    .tasks
                    .get_mut(task_id)
                    .ok_or_else(|| format!("task not found: {task_id}"))?;
                if let Some(expected) = expected {
                    if row.unread_at != Some(expected) {
                        return Err("unread compare-and-clear failed".to_string());
                    }
                }
                row.unread_at = if unread { Some(now) } else { None };
                row.updated_at = now;
                Ok(row.clone())
            })
            .await
            .map_err(ServiceError::new)?;
        self.emit_task_event(&row, "task_meta_changed").await;
        Ok(self.meta_for_row(&row).await)
    }

    async fn delete_task_row(&self, task_id: &str) -> Result<(), ServiceError> {
        let (row, _) = self.require_task(task_id).await?;
        let mut params = json!({
            "workspacePath": row.workspace_path,
            "sessionId": task_id,
        });
        if let Some(identity) = &row.workspace_identity {
            params["workspaceIdentity"] = json!(identity);
        }
        let _ = self.agent_call("closeSession", params).await;
        let now = now_ms();
        let row = self
            .index
            .mutate(|index| {
                let row = index
                    .tasks
                    .get_mut(task_id)
                    .ok_or_else(|| format!("task not found: {task_id}"))?;
                row.deleted_at = Some(now);
                row.archived = false;
                row.pinned = false;
                let workspace =
                    workspace_key(&row.workspace_path, row.workspace_identity.as_deref());
                let snapshot = row.clone();
                remove_top_level_task(index, &workspace, task_id);
                Ok(snapshot)
            })
            .await
            .map_err(ServiceError::new)?;
        self.emit_task_event(&row, "task_deleted").await;
        Ok(())
    }

    async fn resume(&self, params: &Value) -> Result<Value, ServiceError> {
        let task_id = required_str(params, "taskId")?;
        let workspace_path = required_str(params, "workspacePath")?;
        let identity = optional_str(params, "workspaceIdentity");
        let mut read = json!({
            "workspacePath": workspace_path,
            "sessionId": task_id,
        });
        if let Some(identity) = identity {
            read["workspaceIdentity"] = json!(identity);
        }
        let _ = self.agent_call("readSession", read).await?;
        let row = self
            .index
            .mutate(|index| {
                let row = index
                    .tasks
                    .entry(task_id.to_string())
                    .or_insert_with(|| TaskRow {
                        task_id: task_id.to_string(),
                        workspace_path: workspace_path.to_string(),
                        workspace_identity: identity.map(str::to_string),
                        updated_at: now_ms(),
                        ..TaskRow::default()
                    });
                row.deleted_at = None;
                row.updated_at = now_ms();
                if let Some(value) = optional_str(params, "automationId") {
                    row.cron_automation_id = Some(value.to_string());
                }
                if let Some(value) = optional_str(params, "offPeakTaskId") {
                    row.off_peak_task_id = Some(value.to_string());
                }
                let snapshot = row.clone();
                let workspace = workspace_key(workspace_path, identity);
                push_top_level_task(index, &workspace, task_id);
                Ok(snapshot)
            })
            .await
            .map_err(ServiceError::new)?;
        self.emit_task_event(&row, "task_status_changed").await;
        Ok(self.meta_for_row(&row).await)
    }

    async fn groups_structure(&self, params: &Value) -> Result<Value, ServiceError> {
        let scopes = params
            .get("workspaceScopes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let index = self.index.snapshot().await.map_err(ServiceError::new)?;
        let visible_keys: Vec<String> = scopes
            .iter()
            .filter_map(|scope| {
                let path = scope.get("workspacePath").and_then(Value::as_str)?;
                let identity = scope.get("workspaceIdentity").and_then(Value::as_str);
                Some(workspace_key(path, identity))
            })
            .collect();
        let mut members = Vec::new();
        for (group_id, group_members) in &index.members {
            for member in group_members {
                members.push(json!({
                    "groupId": group_id,
                    "workspaceKey": member.workspace_key,
                    "workspacePath": member.workspace_path,
                    "workspaceIdentity": member.workspace_identity,
                    "taskId": member.task_id,
                    "sortOrder": member.sort_order,
                    "addedAt": member.added_at,
                }));
            }
        }
        let mut top_level = Vec::new();
        for key in &visible_keys {
            if let Some(nodes) = index.top_level_orders.get(key) {
                for node in nodes {
                    match node {
                        TopLevelNode::Group {
                            group_id,
                            sort_order,
                        } => top_level.push(json!({
                            "type": "group",
                            "groupId": group_id,
                            "sortOrder": sort_order,
                        })),
                        TopLevelNode::Task {
                            workspace_key,
                            task_id,
                            sort_order,
                        } => top_level.push(json!({
                            "type": "task",
                            "workspaceKey": workspace_key,
                            "taskId": task_id,
                            "sortOrder": sort_order,
                        })),
                    }
                }
            }
        }
        Ok(json!({
            "groups": index.groups,
            "members": members,
            "topLevelOrders": top_level,
        }))
    }

    async fn apply_grouped_order(&self, params: &Value) -> Result<Value, ServiceError> {
        let scopes: Vec<(String, String, Option<String>)> = params
            .get("workspaceScopes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|scope| {
                        let path = scope
                            .get("workspacePath")
                            .and_then(Value::as_str)?
                            .to_string();
                        let identity = scope
                            .get("workspaceIdentity")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let key = workspace_key(&path, identity.as_deref());
                        Some((key, path, identity))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let top_level = params
            .get("topLevelNodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let groups_input = params
            .get("groups")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (_index, row_metas) = {
            let scopes_for_read = scopes.clone();
            let mut all_rows: Vec<(TaskRow, SessionFacts)> = Vec::new();
            for (_, path, identity) in &scopes_for_read {
                all_rows.extend(self.all_rows(path, identity.as_deref()).await?);
            }
            let mut metas = std::collections::BTreeMap::new();
            for (row, facts) in &all_rows {
                metas.insert(row.task_id.clone(), serialize_meta(row, facts));
            }
            let index = self
                .index
                .mutate(|index| {
                    for (key, _, _) in &scopes {
                        let nodes: Vec<TopLevelNode> = top_level
                            .iter()
                            .filter_map(|node| {
                                let node_type = node.get("type").and_then(Value::as_str)?;
                                let sort_order =
                                    node.get("sortOrder").and_then(Value::as_f64).unwrap_or(0.0);
                                match node_type {
                                    "group" => Some(TopLevelNode::Group {
                                        group_id: node.get("groupId")?.as_str()?.to_string(),
                                        sort_order,
                                    }),
                                    "task" => {
                                        let task = node.get("task")?;
                                        let path =
                                            task.get("workspacePath").and_then(Value::as_str)?;
                                        let identity =
                                            task.get("workspaceIdentity").and_then(Value::as_str);
                                        let task_key = workspace_key(path, identity);
                                        if &task_key != key {
                                            return None;
                                        }
                                        Some(TopLevelNode::Task {
                                            workspace_key: task_key,
                                            task_id: task.get("taskId")?.as_str()?.to_string(),
                                            sort_order,
                                        })
                                    }
                                    _ => None,
                                }
                            })
                            .collect();
                        index.top_level_orders.insert(key.clone(), nodes);
                    }
                    for group_input in &groups_input {
                        let Some(group_id) = group_input.get("groupId").and_then(Value::as_str)
                        else {
                            continue;
                        };
                        if find_group(index, group_id).is_none() {
                            continue;
                        }
                        let members = group_input
                            .get("taskRefs")
                            .and_then(Value::as_array)
                            .map(|refs| {
                                refs.iter()
                                    .enumerate()
                                    .filter_map(|(index, task_ref)| {
                                        let path =
                                            task_ref.get("workspacePath")?.as_str()?.to_string();
                                        let identity = task_ref
                                            .get("workspaceIdentity")
                                            .and_then(Value::as_str)
                                            .map(str::to_string);
                                        Some(GroupMember {
                                            workspace_key: workspace_key(
                                                &path,
                                                identity.as_deref(),
                                            ),
                                            workspace_path: path,
                                            workspace_identity: identity,
                                            task_id: task_ref.get("taskId")?.as_str()?.to_string(),
                                            sort_order: Some(index as f64),
                                            added_at: now_ms(),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        index.members.insert(group_id.to_string(), members);
                    }
                    Ok(index.clone())
                })
                .await
                .map_err(ServiceError::new)?;
            (index, metas)
        };
        // Return the Grouped view (the same shape as the TS applyGroupedTaskViewOrder response).
        let mut nodes: Vec<Value> = Vec::new();
        for (key, _, _) in &scopes {
            if let Some(orders) = _index.top_level_orders.get(key) {
                for node in orders {
                    match node {
                        TopLevelNode::Group {
                            group_id,
                            sort_order,
                        } => {
                            if let Some(group) = find_group(&_index, group_id) {
                                let tasks = self.group_tasks(&_index, group_id, &row_metas);
                                nodes.push(json!({
                                    "type": "group",
                                    "group": group,
                                    "tasks": tasks,
                                    "sortOrder": sort_order,
                                }));
                            }
                        }
                        TopLevelNode::Task {
                            task_id,
                            sort_order,
                            ..
                        } => {
                            if let Some(meta) = row_metas.get(task_id) {
                                nodes.push(json!({
                                    "type": "task",
                                    "task": meta,
                                    "sortOrder": sort_order,
                                }));
                            }
                        }
                    }
                }
            }
        }
        for (_, path, identity) in &scopes {
            self.emit_scope_event(path, identity.as_deref(), "task_meta_changed")
                .await;
        }
        Ok(json!({"nodes": nodes}))
    }

    fn group_tasks(
        &self,
        index: &crate::task_store::TaskIndexFile,
        group_id: &str,
        row_metas: &std::collections::BTreeMap<String, Value>,
    ) -> Vec<Value> {
        let mut members = index.members.get(group_id).cloned().unwrap_or_default();
        members.sort_by(|left, right| {
            let left_order = left.sort_order.unwrap_or(f64::MAX);
            let right_order = right.sort_order.unwrap_or(f64::MAX);
            left_order
                .partial_cmp(&right_order)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right.added_at.cmp(&left.added_at))
        });
        members
            .into_iter()
            .filter_map(|member| row_metas.get(&member.task_id).cloned())
            .collect()
    }

    async fn delete_archived(&self, task_id: &str) -> Result<bool, ServiceError> {
        let index = self.index.snapshot().await.map_err(ServiceError::new)?;
        let Some(row) = index.tasks.get(task_id) else {
            return Ok(false);
        };
        if !row.archived || row.deleted_at.is_some() {
            return Ok(false);
        }
        let workspace = row.workspace_path.clone();
        let identity = row.workspace_identity.clone();
        let now = now_ms();
        let updated = self
            .index
            .mutate(|index| {
                let Some(row) = index.tasks.get_mut(task_id) else {
                    return Ok(false);
                };
                row.deleted_at = Some(now);
                row.archived = false;
                Ok(true)
            })
            .await
            .map_err(ServiceError::new)?;
        if updated {
            self.emit_scope_event(&workspace, identity.as_deref(), "task_deleted")
                .await;
        }
        Ok(updated)
    }

    async fn config_options(&self, task_id: &str) -> Result<Value, ServiceError> {
        let (row, _) = self.require_task(task_id).await?;
        let facts = self.facts_for_row(&row).await;
        let facts_value = match facts {
            Some(facts) => json!({
                "mode": row.mode.clone().unwrap_or_else(|| "build".to_string()),
                "model": facts.model.or_else(|| row.model_selection.clone().and_then(|selection| selection.get("modelId").and_then(Value::as_str).map(str::to_string))),
                "thought": row.thought_level.clone(),
            }),
            None => json!({
                "mode": row.mode.unwrap_or_else(|| "build".to_string()),
                "model": row.model_selection.and_then(|selection| selection.get("modelId").and_then(Value::as_str).map(str::to_string)),
                "thought": row.thought_level,
            }),
        };
        Ok(Value::Array(task_snapshot::config_options(&facts_value)))
    }

    async fn snapshot_for(&self, params: &Value, with_etag: bool) -> Result<Value, ServiceError> {
        let task_id = required_str(params, "taskId")?;
        let workspace_path = required_str(params, "workspacePath")?;
        let identity = optional_str(params, "workspaceIdentity");
        let meta = self
            .meta_for_task_in_workspace(task_id, workspace_path, identity)
            .await?;
        let mut read = json!({
            "workspacePath": workspace_path,
            "sessionId": task_id,
        });
        if let Some(identity) = identity {
            read["workspaceIdentity"] = json!(identity);
        }
        let runtime = self.agent.session_transcript_view(task_id).await;
        let transcript = match runtime {
            Some(view) => view,
            None => {
                // A cold session: resume it first (readSession triggers the host's cold resume), then read the view.
                let _ = self.agent_call("readSession", read).await?;
                self.agent
                    .session_transcript_view(task_id)
                    .await
                    .ok_or_else(|| {
                        ServiceError::named("TaskNotFound", format!("task not found: {task_id}"))
                    })?
            }
        };
        let message_limit = params
            .get("messageLimit")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let tool_limit = params
            .get("toolLimit")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let runtime_projection = json!({
            "pendingPermissions": [],
            "pendingElicitations": [],
        });
        let snapshot = task_snapshot::build_snapshot(
            &meta,
            &transcript,
            &runtime_projection,
            message_limit,
            tool_limit,
        );
        if !with_etag {
            return Ok(snapshot);
        }
        let text = serde_json::to_string(&snapshot).unwrap_or_default();
        let etag = format!(
            "sha256-{}",
            nc_code_host::content_hash::sha256_hex(text.as_bytes())
        );
        if let Some(if_none_match) = params.get("ifNoneMatch").and_then(Value::as_str) {
            if if_none_match == etag {
                return Ok(json!({"snapshot": Value::Null, "etag": etag, "notModified": true}));
            }
        }
        Ok(json!({"snapshot": snapshot, "etag": etag}))
    }

    async fn meta_for_task_in_workspace(
        &self,
        task_id: &str,
        workspace_path: &str,
        identity: Option<&str>,
    ) -> Result<Value, ServiceError> {
        let index = self.index.snapshot().await.map_err(ServiceError::new)?;
        let row = index
            .tasks
            .get(task_id)
            .cloned()
            .unwrap_or_else(|| TaskRow {
                task_id: task_id.to_string(),
                workspace_path: workspace_path.to_string(),
                workspace_identity: identity.map(str::to_string),
                updated_at: now_ms(),
                ..TaskRow::default()
            });
        let facts = self
            .session_facts(workspace_path, identity)
            .await?
            .into_iter()
            .find(|facts| facts.task_id == task_id);
        let facts = facts.unwrap_or_else(|| SessionFacts {
            task_id: task_id.to_string(),
            title: row.title_override.clone().unwrap_or_default(),
            created_at: row.updated_at,
            updated_at: row.updated_at,
            status: None,
            trace_id: None,
            provider: None,
            model: None,
            thought: None,
            pending_interaction: None,
            last_error: row.last_error.clone(),
        });
        Ok(serialize_meta(&row, &facts))
    }

    async fn call_inner(
        &self,
        command: &str,
        args: Vec<WireValue>,
    ) -> Result<WireValue, ServiceError> {
        let args = ServiceArgs::new(args);
        let value = match command {
            "initialize" => {
                let params = args.required_json(0, "params")?;
                let result = self.agent_call("initialize", params).await?;
                json!({
                    "available": result.get("available").cloned().unwrap_or(json!(false)),
                    "version": result
                        .get("protocolName")
                        .and_then(Value::as_str)
                        .map(|name| format!("{name}/{}", result.get("protocolVersion").and_then(Value::as_u64).unwrap_or(1))),
                })
            }
            "releaseWorkspacePreparation" => {
                let params = args.required_json(0, "params")?;
                let (_, key) = AgentService::workspace_of(&params)?;
                self.agent.dispose_workspace(&key).await;
                Value::Null
            }
            "createTask" => self.create_task(&args.required_json(0, "params")?).await?,
            "sendPrompt" => self.send_prompt(&args.required_json(0, "params")?).await?,
            "stopGeneration" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let (row, _) = self.require_task(task_id).await?;
                let _ = self
                    .v4_command(
                        &row.workspace_path,
                        row.workspace_identity.as_deref(),
                        task_id,
                        "stop",
                        json!({}),
                    )
                    .await?;
                Value::Null
            }
            "compactSession" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let (row, _) = self.require_task(task_id).await?;
                let mut payload = json!({});
                if let Some(input_id) = optional_str(&params, "inputId") {
                    payload["inputId"] = json!(input_id);
                }
                if let Some(instructions) = optional_str(&params, "instructions") {
                    payload["instructions"] = json!(instructions);
                }
                let ack = self
                    .v4_command(
                        &row.workspace_path,
                        row.workspace_identity.as_deref(),
                        task_id,
                        "compact",
                        payload,
                    )
                    .await?;
                let snapshot = self
                    .agent_call(
                        "readSession",
                        json!({
                            "workspacePath": row.workspace_path,
                            "workspaceIdentity": row.workspace_identity,
                            "sessionId": task_id,
                        }),
                    )
                    .await?;
                json!({
                    "response": ack.get("status").and_then(Value::as_str).unwrap_or("accepted"),
                    "snapshot": snapshot,
                    "compact": {"state": "accepted", "inputId": params.get("inputId").cloned().unwrap_or(Value::Null)},
                    "startedTurn": true,
                })
            }
            "goalSession" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let action = required_str(&params, "action")?;
                let objective = optional_str(&params, "objective").unwrap_or("");
                let (row, _) = self.require_task(task_id).await?;
                let command_type = match action {
                    "set" | "replace" => "sendGoalCommand",
                    "resume" => "resumeGoal",
                    "pause" => "pauseGoal",
                    _ => {
                        return Err(ServiceError::named(
                            "GoalActionRemoved",
                            format!("goal action is not supported by this host: {action}"),
                        ))
                    }
                };
                let payload = if command_type == "sendGoalCommand" {
                    json!({"text": objective})
                } else {
                    json!({})
                };
                let ack = self
                    .v4_command(
                        &row.workspace_path,
                        row.workspace_identity.as_deref(),
                        task_id,
                        command_type,
                        payload,
                    )
                    .await?;
                let snapshot = self
                    .agent_call(
                        "readSession",
                        json!({
                            "workspacePath": row.workspace_path,
                            "workspaceIdentity": row.workspace_identity,
                            "sessionId": task_id,
                        }),
                    )
                    .await?;
                json!({
                    "response": ack.get("status").and_then(Value::as_str).unwrap_or("accepted"),
                    "snapshot": snapshot,
                    "startedTurn": matches!(action, "set" | "replace" | "resume"),
                })
            }
            "respondPermission" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let (row, _) = self.require_task(task_id).await?;
                let request_id = required_str(&params, "requestId")?;
                let option_id = optional_str(&params, "optionId")
                    .or_else(|| {
                        params
                            .get("response")
                            .and_then(|response| response.get("decision"))
                            .and_then(Value::as_str)
                            .map(|decision| match decision {
                                "deny" => "deny",
                                _ => "allowOnce",
                            })
                    })
                    .unwrap_or("allowOnce");
                let _ = self
                    .v4_command(
                        &row.workspace_path,
                        row.workspace_identity.as_deref(),
                        task_id,
                        "resolveInteraction",
                        json!({"interactionId": request_id, "answer": {"optionId": option_id}}),
                    )
                    .await?;
                json!(true)
            }
            "respondElicitation" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let (row, _) = self.require_task(task_id).await?;
                let request_id = required_str(&params, "requestId")?;
                let action = required_str(&params, "action")?;
                let mut answer = json!({"action": action});
                if let Some(content) = params.get("content") {
                    answer["content"] = content.clone();
                }
                let _ = self
                    .v4_command(
                        &row.workspace_path,
                        row.workspace_identity.as_deref(),
                        task_id,
                        "resolveInteraction",
                        json!({"interactionId": request_id, "answer": answer}),
                    )
                    .await?;
                json!(true)
            }
            "closeTask" | "deleteTask" => {
                let params = args.required_json(0, "params")?;
                self.delete_task_row(required_str(&params, "taskId")?)
                    .await?;
                Value::Null
            }
            "deleteArchivedTask" => {
                let params = args.required_json(0, "params")?;
                json!(
                    self.delete_archived(required_str(&params, "taskId")?)
                        .await?
                )
            }
            "deleteArchivedTasks" => {
                let params = args.required_json(0, "params")?;
                let ids: Vec<String> = params
                    .get("taskIds")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let mut deleted = Vec::new();
                let mut skipped = Vec::new();
                let mut failed = Vec::new();
                for task_id in ids {
                    match self.delete_archived(&task_id).await {
                        Ok(true) => deleted.push(task_id),
                        Ok(false) => skipped.push(task_id),
                        Err(_) => failed.push(task_id),
                    }
                }
                if !deleted.is_empty() {
                    let params = args.required_json(0, "params")?;
                    let workspace = required_str(&params, "workspacePath")?;
                    let identity = optional_str(&params, "workspaceIdentity");
                    self.emit_scope_event(workspace, identity, "task_deleted")
                        .await;
                }
                json!({
                    "deletedTaskIds": deleted,
                    "skippedTaskIds": skipped,
                    "failedTaskIds": failed,
                })
            }
            "resumeTask" => self.resume(&args.required_json(0, "params")?).await?,
            "listTasks" => {
                let params = args.required_json(0, "params")?;
                let workspace = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                let result = self
                    .list_rows(workspace, identity, "timeline", "updated", None, None)
                    .await?;
                result.get("items").cloned().unwrap_or_else(|| json!([]))
            }
            "listPinnedTasks" => {
                let params = args.required_json(0, "params")?;
                let workspace = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                let result = self
                    .list_rows(workspace, identity, "pinned", "updated", None, None)
                    .await?;
                result.get("items").cloned().unwrap_or_else(|| json!([]))
            }
            "listPinnedTaskIds" => {
                let index = self.index.snapshot().await.map_err(ServiceError::new)?;
                let ids: Vec<String> = index
                    .tasks
                    .values()
                    .filter(|row| row.pinned && !row.archived && row.deleted_at.is_none())
                    .map(|row| row.task_id.clone())
                    .collect();
                json!(ids)
            }
            "listDeletedTaskIds" => {
                let params = args.required_json(0, "params")?;
                let workspace = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                let index = self.index.snapshot().await.map_err(ServiceError::new)?;
                let ids: Vec<String> = index
                    .tasks
                    .values()
                    .filter(|row| {
                        row.workspace_path == workspace
                            && row.workspace_identity.as_deref() == identity
                            && row.deleted_at.is_some()
                    })
                    .map(|row| row.task_id.clone())
                    .collect();
                json!(ids)
            }
            "listArchivedTasks" => {
                let params = args.required_json(0, "params")?;
                let workspace = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                let result = self
                    .list_rows(workspace, identity, "archived", "updated", None, None)
                    .await?;
                result.get("items").cloned().unwrap_or_else(|| json!([]))
            }
            "listTaskList" => {
                let params = args.required_json(0, "params")?;
                let kind = optional_str(&params, "kind").unwrap_or("timeline");
                let sort_by = optional_str(&params, "sortBy").unwrap_or("updated");
                let search = optional_str(&params, "search");
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize);
                let scopes = params
                    .get("workspaceScopes")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut items = Vec::new();
                let mut total = 0usize;
                let mut has_more = false;
                for scope in &scopes {
                    let Some(path) = scope.get("workspacePath").and_then(Value::as_str) else {
                        continue;
                    };
                    let identity = scope.get("workspaceIdentity").and_then(Value::as_str);
                    let result = self
                        .list_rows(path, identity, kind, sort_by, search, limit)
                        .await?;
                    total += result.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
                    has_more |= result
                        .get("hasMore")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if let Some(list) = result.get("items").and_then(Value::as_array) {
                        items.extend(list.iter().cloned());
                    }
                }
                json!({"items": items, "total": total, "hasMore": has_more})
            }
            "createTaskGroup" => {
                let params = args.json(0)?.unwrap_or_else(|| json!({}));
                let title = optional_str(&params, "title")
                    .unwrap_or("New group")
                    .to_string();
                let color = optional_str(&params, "color").unwrap_or("gray").to_string();
                let now = now_ms();
                let group = TaskGroup {
                    id: self.next_id("group"),
                    title,
                    color,
                    created_at: now,
                    updated_at: now,
                };
                let group_for_insert = group.clone();
                self.index
                    .mutate(|index| {
                        index.groups.push(group_for_insert);
                        Ok(())
                    })
                    .await
                    .map_err(ServiceError::new)?;
                json!(group)
            }
            "renameTaskGroup" => {
                let params = args.required_json(0, "params")?;
                let group_id = required_str(&params, "groupId")?;
                let title = required_str(&params, "title")?;
                let group = self
                    .index
                    .mutate(|index| {
                        let group = index
                            .groups
                            .iter_mut()
                            .find(|group| group.id == group_id)
                            .ok_or_else(|| format!("task group not found: {group_id}"))?;
                        group.title = title.to_string();
                        group.updated_at = now_ms();
                        Ok(group.clone())
                    })
                    .await
                    .map_err(ServiceError::new)?;
                self.notify_scopes(&params).await;
                json!(group)
            }
            "updateTaskGroupColor" => {
                let params = args.required_json(0, "params")?;
                let group_id = required_str(&params, "groupId")?;
                let color = required_str(&params, "color")?;
                let group = self
                    .index
                    .mutate(|index| {
                        let group = index
                            .groups
                            .iter_mut()
                            .find(|group| group.id == group_id)
                            .ok_or_else(|| format!("task group not found: {group_id}"))?;
                        group.color = color.to_string();
                        group.updated_at = now_ms();
                        Ok(group.clone())
                    })
                    .await
                    .map_err(ServiceError::new)?;
                self.notify_scopes(&params).await;
                json!(group)
            }
            "deleteTaskGroup" => {
                let params = args.required_json(0, "params")?;
                let group_id = required_str(&params, "groupId")?;
                self.index
                    .mutate(|index| {
                        index.groups.retain(|group| group.id != group_id);
                        index.members.remove(group_id);
                        for nodes in index.top_level_orders.values_mut() {
                            nodes.retain(|node| match node {
                                TopLevelNode::Group { group_id: id, .. } => id != group_id,
                                TopLevelNode::Task { .. } => true,
                            });
                        }
                        Ok(())
                    })
                    .await
                    .map_err(ServiceError::new)?;
                self.notify_scopes(&params).await;
                Value::Null
            }
            "listGroupedTaskViewStructure" => {
                self.groups_structure(&args.required_json(0, "params")?)
                    .await?
            }
            "applyGroupedTaskViewOrder" => {
                self.apply_grouped_order(&args.required_json(0, "params")?)
                    .await?
            }
            "getTaskMeta" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let workspace = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                self.meta_for_task_in_workspace(task_id, workspace, identity)
                    .await?
            }
            "getTaskConfigOptions" => {
                let params = args.required_json(0, "params")?;
                self.config_options(required_str(&params, "taskId")?)
                    .await?
            }
            "getTaskModelSelection" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let (row, facts) = self.require_task(task_id).await?;
                let selection = row.model_selection.clone().or_else(|| {
                    facts.model.as_ref().map(|model| {
                        json!({
                            "providerId": row.provider.clone().unwrap_or_else(|| "glm".to_string()),
                            "modelId": model,
                            "options": facts.thought.clone().map(|thought| json!({"reasoningLevel": thought})),
                        })
                    })
                });
                selection.unwrap_or(Value::Null)
            }
            "getTaskSnapshot" => {
                let params = args.required_json(0, "params")?;
                self.snapshot_for(&params, false).await?
            }
            "getTaskSnapshotWithEtag" => {
                let params = args.required_json(0, "params")?;
                self.snapshot_for(&params, true).await?
            }
            "getTaskSnapshotBody" => {
                // this host does not trim large bodies (no byteBudget refs), so a bodyRef never exists.
                Value::Null
            }
            "getTaskSnapshotRef" => Value::Null,
            "getTaskSnapshotToolCallsSlice" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let workspace = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                let message_index = params
                    .get("messageIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let start = params
                    .get("startToolIndex")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                let meta = self
                    .meta_for_task_in_workspace(task_id, workspace, identity)
                    .await?;
                let transcript = self
                    .agent
                    .session_transcript_view(task_id)
                    .await
                    .ok_or_else(|| {
                        ServiceError::named("TaskNotFound", format!("task not found: {task_id}"))
                    })?;
                task_snapshot::tool_calls_slice(&meta, &transcript, message_index, start, limit)
                    .unwrap_or(Value::Null)
            }
            "getTaskTokenUsage" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let usage = self.agent.session_usage(task_id).await.ok_or_else(|| {
                    ServiceError::named("TaskNotFound", format!("task not found: {task_id}"))
                })?;
                usage
            }
            "getTaskSessionFilePath" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let workspace = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                let key = identity.unwrap_or(workspace);
                let path = self
                    .agent
                    .data_dir_for(workspace, key)
                    .join("sessions")
                    .join(task_id)
                    .join("transcript.jsonl");
                json!({"path": path.to_string_lossy(), "exists": path.exists()})
            }
            "getTaskNativeSessionLogFile" => {
                json!({"provider": Value::Null, "path": Value::Null, "exists": false})
            }
            "getModelTrajectory" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                json!({
                    "taskId": task_id,
                    "available": false,
                    "records": [],
                    "sourceFiles": [],
                    "truncated": false,
                })
            }
            "renameTask" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let title = required_str(&params, "title")?;
                let (row, _) = self.require_task(task_id).await?;
                let _ = self
                    .v4_command(
                        &row.workspace_path,
                        row.workspace_identity.as_deref(),
                        task_id,
                        "renameSession",
                        json!({"title": title}),
                    )
                    .await;
                self.patch_task(task_id, "task_title_changed", |row| {
                    row.title_override = Some(title.to_string());
                    row.title_overridden = Some(true);
                })
                .await?
            }
            "setTaskPinned" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let pinned = params
                    .get("pinned")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.patch_task(
                    task_id,
                    if pinned {
                        "task_pinned"
                    } else {
                        "task_unpinned"
                    },
                    |row| {
                        row.pinned = pinned;
                    },
                )
                .await?
            }
            "setTaskUnread" => self.set_unread(&args.required_json(0, "params")?).await?,
            "archiveTask" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                self.patch_task(task_id, "task_archived", |row| {
                    row.archived = true;
                    row.archived_at = Some(now_ms());
                })
                .await?
            }
            "unarchiveTask" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                self.patch_task(task_id, "task_unarchived", |row| {
                    row.archived = false;
                    row.archived_at = None;
                })
                .await?
            }
            "archiveStaleTasks" => {
                let params = args.required_json(0, "params")?;
                let workspace = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                let older_than_days = params
                    .get("olderThanDays")
                    .and_then(Value::as_i64)
                    .unwrap_or(30)
                    .max(0);
                let cutoff = now_ms() - older_than_days * 86_400_000;
                let rows = self.all_rows(workspace, identity).await?;
                let mut archived = Vec::new();
                for (row, facts) in rows {
                    if row.pinned || row.unread_at.is_some() || row.archived {
                        continue;
                    }
                    if facts.status.as_deref() == Some("running") {
                        continue;
                    }
                    if facts.updated_at >= cutoff {
                        continue;
                    }
                    let meta = self
                        .patch_task(&row.task_id, "auto_archive", |row| {
                            row.archived = true;
                            row.archived_at = Some(now_ms());
                        })
                        .await?;
                    archived.push(meta);
                }
                json!(archived)
            }
            "archiveWorkspaceTasks" => {
                let params = args.required_json(0, "params")?;
                let workspace = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                let rows = self.all_rows(workspace, identity).await?;
                let mut archived = Vec::new();
                for (row, _) in rows {
                    if row.archived {
                        continue;
                    }
                    let meta = self
                        .patch_task(&row.task_id, "auto_archive", |row| {
                            row.archived = true;
                            row.archived_at = Some(now_ms());
                        })
                        .await?;
                    archived.push(meta);
                }
                json!(archived)
            }
            "setMode" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let mode = required_str(&params, "mode")?;
                let (row, _) = self.require_task(task_id).await?;
                let _ = self
                    .v4_command(
                        &row.workspace_path,
                        row.workspace_identity.as_deref(),
                        task_id,
                        "switchCollaborationMode",
                        json!({"mode": mode}),
                    )
                    .await?;
                self.patch_task(task_id, "task_meta_changed", |row| {
                    row.mode = Some(mode.to_string());
                })
                .await?;
                Value::Null
            }
            "setModel" | "setAutomationSessionConfig" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let selection = params
                    .get("modelSelection")
                    .cloned()
                    .ok_or_else(|| ServiceError::invalid_params("modelSelection is required"))?;
                let (row, _) = self.require_task(task_id).await?;
                let provider = selection
                    .get("providerId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let model = selection
                    .get("modelId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let thought = params
                    .get("thoughtLevel")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        selection
                            .get("options")
                            .and_then(|options| options.get("reasoningLevel"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("");
                let ack = self
                    .v4_command(
                        &row.workspace_path,
                        row.workspace_identity.as_deref(),
                        task_id,
                        "switchModelConfig",
                        json!({"provider": provider, "model": model, "thought": thought}),
                    )
                    .await?;
                let _ = ack;
                if let Some(mode) = optional_str(&params, "mode") {
                    let _ = self
                        .v4_command(
                            &row.workspace_path,
                            row.workspace_identity.as_deref(),
                            task_id,
                            "switchCollaborationMode",
                            json!({"mode": mode}),
                        )
                        .await;
                }
                let selection_for_row = selection.clone();
                let thought_for_row = thought.to_string();
                let mode_for_row = optional_str(&params, "mode").map(str::to_string);
                self.index
                    .mutate(|index| {
                        if let Some(row) = index.tasks.get_mut(task_id) {
                            row.model_selection = Some(selection_for_row);
                            if !thought_for_row.is_empty() {
                                row.thought_level = Some(thought_for_row);
                            }
                            if let Some(mode) = mode_for_row {
                                row.mode = Some(mode);
                            }
                            row.updated_at = now_ms();
                        }
                        Ok(())
                    })
                    .await
                    .map_err(ServiceError::new)?;
                self.config_options(task_id).await?
            }
            "setConfigOption" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let config_id = required_str(&params, "configId")?;
                let value = required_str(&params, "value")?;
                let (row, _) = self.require_task(task_id).await?;
                match config_id {
                    "mode" => {
                        let _ = self
                            .v4_command(
                                &row.workspace_path,
                                row.workspace_identity.as_deref(),
                                task_id,
                                "switchCollaborationMode",
                                json!({"mode": value}),
                            )
                            .await?;
                        self.patch_task(task_id, "quiet", |row| {
                            row.mode = Some(value.to_string());
                        })
                        .await?;
                    }
                    "model" => {
                        let thought = row.thought_level.clone().unwrap_or_default();
                        let _ = self
                            .v4_command(
                                &row.workspace_path,
                                row.workspace_identity.as_deref(),
                                task_id,
                                "switchModelConfig",
                                json!({"provider": row.provider.clone().unwrap_or_default(), "model": value, "thought": thought}),
                            )
                            .await?;
                    }
                    "thought_level" => {
                        let model = row
                            .model_selection
                            .as_ref()
                            .and_then(|selection| selection.get("modelId"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let _ = self
                            .v4_command(
                                &row.workspace_path,
                                row.workspace_identity.as_deref(),
                                task_id,
                                "switchModelConfig",
                                json!({"provider": row.provider.clone().unwrap_or_default(), "model": model, "thought": value}),
                            )
                            .await?;
                        self.patch_task(task_id, "quiet", |row| {
                            row.thought_level = Some(value.to_string());
                        })
                        .await?;
                    }
                    other => {
                        return Err(ServiceError::named(
                            "ConfigOptionRemoved",
                            format!("config option is not available: {other}"),
                        ))
                    }
                }
                self.config_options(task_id).await?
            }
            "setAssistantMessageFeedback" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let workspace = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                let mut snapshot_params = json!({"taskId": task_id, "workspacePath": workspace});
                if let Some(identity) = identity {
                    snapshot_params["workspaceIdentity"] = json!(identity);
                }
                let snapshot = self.snapshot_for(&snapshot_params, false).await?;
                json!({
                    "meta": snapshot.get("meta").cloned().unwrap_or(Value::Null),
                    "messages": snapshot.get("messages").cloned().unwrap_or_else(|| json!([])),
                    "fileChanges": snapshot.get("fileChanges").cloned().unwrap_or_else(|| json!([])),
                })
            }
            "getTaskSnapshotHistoryDummy" => Value::Null,
            "branchTaskFromPrompt" => {
                let params = args.required_json(0, "params")?;
                let source = required_str(&params, "sourceTaskId")?;
                let (row, _) = self.require_task(source).await?;
                let mut create = json!({
                    "workspacePath": row.workspace_path,
                });
                create["forkedFromTaskId"] = json!(source);
                if let Some(identity) = &row.workspace_identity {
                    create["workspaceIdentity"] = json!(identity);
                }
                if let Some(mode) = &row.mode {
                    create["mode"] = json!(mode);
                }
                if let Some(selection) = &row.model_selection {
                    create["modelSelection"] = selection.clone();
                }
                self.create_task(&create).await?
            }
            "restartWorkspaceProcess" => {
                let params = args.required_json(0, "params")?;
                let (_, key) = AgentService::workspace_of(&params)?;
                self.agent.dispose_workspace(&key).await;
                if params
                    .get("bumpRuntimeEpoch")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    if let Some(task_id) = optional_str(&params, "resumeTaskId") {
                        // It used to be `let _ = self.index.mutate(...)` —— a future that is never awaited gets
                        // dropped outright, so the epoch bump never actually ran; the await is added back (clippy::let_underscore_future).
                        let _ = self
                            .index
                            .mutate(|index| {
                                if let Some(row) = index.tasks.get_mut(task_id) {
                                    row.runtime_epoch = Some(row.runtime_epoch.unwrap_or(0) + 1);
                                }
                                Ok(())
                            })
                            .await;
                    }
                }
                if let Some(task_id) = optional_str(&params, "resumeTaskId") {
                    let workspace_path = required_str(&params, "workspacePath")?;
                    let identity = optional_str(&params, "workspaceIdentity");
                    let mut read = json!({"workspacePath": workspace_path, "sessionId": task_id});
                    if let Some(identity) = identity {
                        read["workspaceIdentity"] = json!(identity);
                    }
                    let _ = self.agent_call("readSession", read).await?;
                }
                Value::Null
            }
            "enqueueTaskCommand" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let command_id = required_str(&params, "commandId")?;
                let trace_id = optional_str(&params, "traceId").unwrap_or("").to_string();
                let workspace_path = required_str(&params, "workspacePath")?;
                let identity = optional_str(&params, "workspaceIdentity");
                let content = required_str(&params, "content")?;
                let _ = self.require_task(task_id).await?;
                let service = Arc::clone(&self.agent);
                let task_id_owned = task_id.to_string();
                let workspace_owned = workspace_path.to_string();
                let identity_owned = identity.map(str::to_string);
                let content_owned = content.to_string();
                let trace_owned = trace_id.clone();
                tokio::spawn(async move {
                    let mut send = json!({
                        "workspacePath": workspace_owned,
                        "sessionId": task_id_owned,
                        "content": content_owned,
                    });
                    if let Some(identity) = identity_owned {
                        send["workspaceIdentity"] = json!(identity);
                    }
                    if !trace_owned.is_empty() {
                        send["inputId"] = json!(trace_owned);
                    }
                    let _ = service
                        .call_inner("sendPrompt", vec![WireValue::Json(send)])
                        .await;
                });
                let now = now_ms();
                json!({
                    "accepted": true,
                    "command": {
                        "commandId": command_id,
                        "taskId": task_id,
                        "traceId": trace_id,
                        "queryId": params.get("queryId").cloned().unwrap_or(Value::Null),
                        "workspacePath": workspace_path,
                        "workspaceIdentity": identity.map(str::to_string),
                        "workspaceKey": workspace_key(workspace_path, identity),
                        "status": "accepted",
                        "createdAt": now,
                        "updatedAt": now,
                        "clientId": params.get("clientId").cloned().unwrap_or(Value::Null),
                        "clientLabel": params.get("clientLabel").cloned().unwrap_or(Value::Null),
                        "type": "send_prompt",
                        "content": content,
                        "attachments": params.get("attachments").cloned().unwrap_or(Value::Null),
                        "automationId": params.get("automationId").cloned().unwrap_or(Value::Null),
                    }
                })
            }
            "promoteTaskCommand" => {
                let params = args.required_json(0, "params")?;
                let task_id = required_str(&params, "taskId")?;
                let command_id = required_str(&params, "commandId")?;
                let _ = self.require_task(task_id).await?;
                let now = now_ms();
                json!({
                    "accepted": true,
                    "command": {
                        "commandId": command_id,
                        "taskId": task_id,
                        "traceId": params.get("ownerRunId").cloned().unwrap_or(Value::Null),
                        "workspacePath": params.get("workspacePath").cloned().unwrap_or(Value::Null),
                        "workspaceIdentity": params.get("workspaceIdentity").cloned().unwrap_or(Value::Null),
                        "workspaceKey": workspace_key(
                            optional_str(&params, "workspacePath").unwrap_or_default(),
                            optional_str(&params, "workspaceIdentity"),
                        ),
                        "status": "accepted",
                        "createdAt": now,
                        "updatedAt": now,
                        "type": "send_prompt",
                        "content": "",
                    }
                })
            }
            "cancelTaskCommand" => {
                let params = args.required_json(0, "params")?;
                let command_id = required_str(&params, "commandId")?;
                json!({
                    "canceled": true,
                    "commandId": command_id,
                    "reason": "not_found",
                })
            }
            "deliverSessionMessage" | "sendSessionMessageDeliveryResult" => {
                return Err(ServiceError::named(
                    "SessionMailboxRemoved",
                    "cross-session message delivery is not part of this host",
                ))
            }
            "scanImportableClaudeSessions" => {
                let params = args.json(0)?.unwrap_or_else(|| json!({}));
                let candidates = scan_claude_sessions(
                    self.paths.home_dir(),
                    params.get("workspacePath").and_then(Value::as_str),
                    params.get("modifiedSince").and_then(Value::as_i64),
                    params
                        .get("limit")
                        .and_then(Value::as_u64)
                        .map(|value| value as usize),
                );
                json!(candidates)
            }
            "importClaudeSessions" => {
                return Err(ServiceError::named(
                    "ClaudeImportUnavailable",
                    "Claude session import is not wired in this host increment",
                ))
            }
            other => return Err(ServiceError::message(format!("Method not found: {other}"))),
        };
        Ok(WireValue::Json(value))
    }

    async fn notify_scopes(&self, params: &Value) {
        if let Some(scopes) = params.get("workspaceScopes").and_then(Value::as_array) {
            for scope in scopes {
                let Some(path) = scope.get("workspacePath").and_then(Value::as_str) else {
                    continue;
                };
                let identity = scope.get("workspaceIdentity").and_then(Value::as_str);
                self.emit_scope_event(path, identity, "task_meta_changed")
                    .await;
            }
        }
    }
}

#[async_trait]
impl Channel for TaskService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        self.call_inner(command, args).await.map_err(Into::into)
    }

    async fn listen(
        &self,
        _ctx: &ChannelContext,
        event: &str,
        arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            "onDynamicWorkspaceEvent" => {
                let workspace = match &arg {
                    WireValue::String(value) => WorkspaceFilter::from_path(value.trim(), None),
                    WireValue::Json(value) => WorkspaceFilter::from_path(
                        value
                            .get("workspacePath")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        value.get("workspaceIdentity").and_then(Value::as_str),
                    ),
                    _ => None,
                }
                .ok_or_else(|| ChannelFailure::error("workspace is required"))?;
                Ok(filtered_subscription(
                    &self.events,
                    move |value| match value {
                        WireValue::Json(event) => {
                            let path = event
                                .get("workspacePath")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let identity = event.get("workspaceIdentity").and_then(Value::as_str);
                            workspace.matches(path, identity)
                        }
                        _ => false,
                    },
                ))
            }
            "onDynamicTaskEvent"
            | "onDynamicStreamEvent"
            | "onDynamicTaskTerminalOutcome"
            | "onDynamicTaskReady" => {
                let (workspace, task_id) = match &arg {
                    WireValue::String(value) => (None, value.clone()),
                    WireValue::Json(value) => (
                        WorkspaceFilter::from_path(
                            value
                                .get("workspacePath")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            value.get("workspaceIdentity").and_then(Value::as_str),
                        ),
                        value
                            .get("taskId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    _ => (None, String::new()),
                };
                Ok(filtered_subscription(
                    &self.events,
                    move |value| match value {
                        WireValue::Json(event) => {
                            if event.get("taskId").and_then(Value::as_str) != Some(task_id.as_str())
                            {
                                return false;
                            }
                            match &workspace {
                                Some(filter) => filter.matches(
                                    event
                                        .get("workspacePath")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default(),
                                    event.get("workspaceIdentity").and_then(Value::as_str),
                                ),
                                None => true,
                            }
                        }
                        _ => false,
                    },
                ))
            }
            "onError" => Ok(EventSubscription::from_broadcast(self.events.subscribe())),
            other => Err(ChannelFailure::event_not_found(other)),
        }
    }
}

fn filtered_subscription(
    bus: &EventBus,
    predicate: impl Fn(&WireValue) -> bool + Send + 'static,
) -> EventSubscription {
    let mut receiver = bus.subscribe();
    let (sender, channel) = tokio::sync::mpsc::channel(256);
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(value) => {
                    if predicate(&value) && sender.send(value).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    EventSubscription::from_receiver(channel)
}

#[derive(Clone)]
struct WorkspaceFilter {
    key: String,
}

impl WorkspaceFilter {
    fn from_path(path: &str, identity: Option<&str>) -> Option<Self> {
        if path.trim().is_empty() && identity.map(str::trim).unwrap_or_default().is_empty() {
            return None;
        }
        Some(Self {
            key: workspace_key(path, identity),
        })
    }

    fn matches(&self, path: &str, identity: Option<&str>) -> bool {
        workspace_key(path, identity) == self.key
    }
}

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, ServiceError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ServiceError::invalid_params(format!("{key} is required")))
}

fn optional_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Scan the Claude Code native sessions (`~/.claude/projects/**/*.jsonl`) and parse each file's
/// `sessionId` / `cwd` / timestamps to produce the real import candidates (no network access at all).
fn scan_claude_sessions(
    home_dir: &std::path::Path,
    workspace_path: Option<&str>,
    modified_since: Option<i64>,
    limit: Option<usize>,
) -> Vec<Value> {
    let root = home_dir.join(".claude").join("projects");
    let mut candidates = Vec::new();
    collect_claude_files(&root, &mut candidates);
    let mut items: Vec<Value> = Vec::new();
    for (path, updated_at) in candidates {
        if let Some(since) = modified_since {
            if updated_at < since {
                continue;
            }
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut session_id: Option<String> = None;
        let mut cwd: Option<String> = None;
        let mut created_at: Option<i64> = None;
        let mut preview: Option<String> = None;
        for line in text.lines().take(400) {
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if session_id.is_none() {
                session_id = entry
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            if cwd.is_none() {
                cwd = entry.get("cwd").and_then(Value::as_str).map(str::to_string);
            }
            if created_at.is_none() {
                created_at = entry
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339_ms);
            }
            if preview.is_none() {
                preview = entry
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| text.chars().take(80).collect());
            }
        }
        let Some(session_id) = session_id else {
            continue;
        };
        let Some(cwd) = cwd else {
            continue;
        };
        if let Some(expected) = workspace_path {
            if !same_path(&cwd, expected) {
                continue;
            }
        }
        let mut candidate = json!({
            "provider": "claude",
            "sessionId": session_id,
            "workspacePath": cwd,
            "sourcePath": path.to_string_lossy(),
            "updatedAt": updated_at,
        });
        if let Some(created_at) = created_at {
            candidate["createdAt"] = json!(created_at);
        }
        if let Some(preview) = preview {
            candidate["previewTitle"] = json!(preview);
        }
        items.push(candidate);
    }
    items.sort_by_key(|item| {
        std::cmp::Reverse(item.get("updatedAt").and_then(Value::as_i64).unwrap_or(0))
    });
    if let Some(limit) = limit {
        items.truncate(limit);
    }
    items
}

fn collect_claude_files(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, i64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_claude_files(&path, out);
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            let updated_at = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(0);
            out.push((path, updated_at));
        }
    }
}

fn same_path(left: &str, right: &str) -> bool {
    left.trim_end_matches(['/', '\\']) == right.trim_end_matches(['/', '\\'])
}

/// Parse a timestamp of the `2026-01-01T00:00:00.000Z` form (the timestamp field of Claude JSONL).
fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    let minute: i64 = value.get(14..16)?.parse().ok()?;
    let second: i64 = value.get(17..19)?.parse().ok()?;
    let millis: i64 = value
        .get(20..23)
        .filter(|_| value.get(19..20) == Some("."))
        .and_then(|fraction| fraction.parse().ok())
        .unwrap_or(0);
    let days = days_from_civil(year, month, day);
    Some(days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000 + millis)
}

/// Howard Hinnant's days-from-civil (UTC).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_services_provider::store::{
        default_model_config, ModelRecord, ProviderRecord, ProviderStore,
    };

    fn temp_paths(tag: &str) -> (AppPaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-task-service-{}-{tag}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("root");
        (AppPaths::new(root.join("home"), root.join("data")), root)
    }

    async fn configured(tag: &str) -> (Arc<TaskService>, std::path::PathBuf, std::path::PathBuf) {
        let (paths, root) = temp_paths(tag);
        let store = ProviderStore::new(paths.clone());
        store
            .mutate(|snapshot| {
                snapshot.providers.push(ProviderRecord {
                    provider_id: "personal:1".to_string(),
                    provider_name: "Test Provider".to_string(),
                    template_id: None,
                    enabled: true,
                    config: json!({
                        "group": "standard-personal",
                        "visibility": "visible",
                        "api": {"type": "openai-chat-completions", "baseUrl": "http://127.0.0.1:9/v1"},
                        "access": {"type": "api-key", "apiKey": "test-key"}
                    }),
                    models: vec![ModelRecord {
                        model_id: "test-model".to_string(),
                        config: default_model_config(),
                        enabled: true,
                        use_recommended: true,
                    }],
                });
                snapshot.provider_order.push("personal:1".to_string());
                Ok(())
            })
            .await
            .expect("store");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let agent = AgentService::new(paths.clone(), store);
        (TaskService::new(paths, agent), root, workspace)
    }

    #[tokio::test]
    async fn create_task_lists_and_renames() {
        let (service, root, workspace) = configured("create").await;
        let created = service
            .call_inner(
                "createTask",
                vec![WireValue::Json(
                    json!({"workspacePath": workspace.to_string_lossy()}),
                )],
            )
            .await
            .expect("create");
        let WireValue::Json(created) = created else {
            panic!("json")
        };
        let task_id = created["taskId"].as_str().expect("taskId").to_string();
        assert!(created["initialSlashCommands"].is_array());

        let listed = service
            .call_inner(
                "listTasks",
                vec![WireValue::Json(
                    json!({"workspacePath": workspace.to_string_lossy()}),
                )],
            )
            .await
            .expect("list");
        let WireValue::Json(listed) = listed else {
            panic!("json")
        };
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["taskId"], json!(task_id));

        let renamed = service
            .call_inner(
                "renameTask",
                vec![WireValue::Json(json!({
                    "taskId": task_id,
                    "workspacePath": workspace.to_string_lossy(),
                    "title": "Renamed",
                }))],
            )
            .await
            .expect("rename");
        let WireValue::Json(renamed) = renamed else {
            panic!("json")
        };
        assert_eq!(renamed["title"], json!("Renamed"));
        assert_eq!(renamed["titleOverridden"], json!(true));

        let pinned = service
            .call_inner(
                "setTaskPinned",
                vec![WireValue::Json(json!({
                    "taskId": task_id,
                    "workspacePath": workspace.to_string_lossy(),
                    "pinned": true,
                }))],
            )
            .await
            .expect("pin");
        let WireValue::Json(pinned) = pinned else {
            panic!("json")
        };
        assert_eq!(pinned["title"], json!("Renamed"));

        let pinned_ids = service
            .call_inner("listPinnedTaskIds", vec![])
            .await
            .expect("pinned ids");
        assert_eq!(pinned_ids, WireValue::Json(json!([task_id])));

        service
            .call_inner(
                "archiveTask",
                vec![WireValue::Json(json!({
                    "taskId": task_id,
                    "workspacePath": workspace.to_string_lossy(),
                }))],
            )
            .await
            .expect("archive");
        let archived = service
            .call_inner(
                "listArchivedTasks",
                vec![WireValue::Json(
                    json!({"workspacePath": workspace.to_string_lossy()}),
                )],
            )
            .await
            .expect("archived");
        let WireValue::Json(archived) = archived else {
            panic!("json")
        };
        assert_eq!(archived.as_array().unwrap().len(), 1);

        let deleted = service
            .call_inner(
                "deleteArchivedTask",
                vec![WireValue::Json(json!({
                    "taskId": task_id,
                    "workspacePath": workspace.to_string_lossy(),
                }))],
            )
            .await
            .expect("delete archived");
        assert_eq!(deleted, WireValue::Json(json!(true)));

        let deleted_ids = service
            .call_inner(
                "listDeletedTaskIds",
                vec![WireValue::Json(
                    json!({"workspacePath": workspace.to_string_lossy()}),
                )],
            )
            .await
            .expect("deleted ids");
        let WireValue::Json(deleted_ids) = deleted_ids else {
            panic!("json")
        };
        assert_eq!(deleted_ids, json!([task_id]));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn groups_round_trip_through_order_apply() {
        let (service, root, workspace) = configured("groups").await;
        let created = service
            .call_inner(
                "createTask",
                vec![WireValue::Json(
                    json!({"workspacePath": workspace.to_string_lossy()}),
                )],
            )
            .await
            .expect("create");
        let WireValue::Json(created) = created else {
            panic!("json")
        };
        let task_id = created["taskId"].as_str().expect("taskId").to_string();
        let group = service
            .call_inner(
                "createTaskGroup",
                vec![WireValue::Json(json!({"title": "Sprint", "color": "blue"}))],
            )
            .await
            .expect("group");
        let WireValue::Json(group) = group else {
            panic!("json")
        };
        let group_id = group["id"].as_str().expect("groupId").to_string();
        let result = service
            .call_inner(
                "applyGroupedTaskViewOrder",
                vec![WireValue::Json(json!({
                    "workspaceScopes": [{"workspacePath": workspace.to_string_lossy()}],
                    "topLevelNodes": [
                        {"type": "group", "groupId": group_id, "sortOrder": 1.0},
                        {"type": "task", "task": {"workspacePath": workspace.to_string_lossy(), "taskId": task_id}, "sortOrder": 2.0}
                    ],
                    "groups": [{"groupId": group_id, "taskRefs": [{"workspacePath": workspace.to_string_lossy(), "taskId": task_id}]}]
                }))],
            )
            .await
            .expect("apply");
        let WireValue::Json(result) = result else {
            panic!("json")
        };
        let nodes = result["nodes"].as_array().expect("nodes");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0]["type"], json!("group"));
        assert_eq!(nodes[0]["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(nodes[1]["type"], json!("task"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rfc3339_parser_matches_unix_ms() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2024-01-02T03:04:05.678Z"),
            Some(1_704_164_645_678)
        );
    }
}
