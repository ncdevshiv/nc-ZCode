//! `window-controller` channel — `IWindowControllerService`
//! (`packages/services/src/window-controller/windowController.ts`).
//!
//! The window Controller is a projection + routing layer over the task facade; it owns no
//! task data. Local scopes route into the injected `nc_code_services_agent::TaskService`
//! (`nc-code-task`); remote scopes (`remoteSessionId`) resolve through the injected
//! [`crate::remote_registry::RemoteConnectionRegistry`] (spec §2). Nothing here
//! re-implements the task logic.
//!
//! Subscription state (`subscribeControllerV4`/`resyncControllerV4`/`onDynamicControllerFrame`)
//! is a real projection over [`crate::controller_projection`]: sources are registered lazily
//! when a scope is first listed (mirroring `registerResolvedSource`), refreshed from the task
//! facade's `onDynamicWorkspaceEvent`, and every change is published as a V4 controller frame
//! on the shared `onDynamicControllerFrame` bus.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nc_code_channels::{
    CancelToken, Channel, ChannelContext, ChannelFailure, ClientMode, EventBus, EventSubscription,
};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};
use nc_code_services_agent::TaskService;
use serde_json::{json, Map, Value};

use crate::controller_projection::{
    scope_key_remote, task_key_remote, Projection, TOPIC_TASKS, TOPIC_WORKSPACES,
};
use crate::remote_registry::RemoteConnectionRegistry;

pub struct WindowControllerService {
    handle: Handle,
    ids: Arc<AtomicU64>,
}

/// Shared, cheaply-clonable routing handle. Spawned per-source event listeners capture a
/// clone, so the background refresh path never needs `&self`.
#[derive(Clone)]
struct Handle {
    tasks: Arc<TaskService>,
    remotes: Arc<RemoteConnectionRegistry>,
    ctx: ChannelContext,
    state: Arc<Mutex<Projection>>,
    bus: EventBus,
}

impl WindowControllerService {
    pub fn new(tasks: Arc<TaskService>, remotes: Arc<RemoteConnectionRegistry>) -> Arc<Self> {
        let sequence = now_ms();
        let bus = EventBus::default();
        let projection = Projection::new(
            format!("controller-tasks-{sequence}"),
            format!("controller-workspaces-{sequence}"),
        );
        Arc::new(Self {
            handle: Handle {
                tasks,
                remotes,
                ctx: ChannelContext::new(
                    format!("window-controller-{sequence}"),
                    ClientMode::DesktopContinuous,
                ),
                state: Arc::new(Mutex::new(projection)),
                bus,
            },
            ids: Arc::new(AtomicU64::new(1)),
        })
    }

    fn next_id(&self, prefix: &str) -> String {
        let seq = self.ids.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{}-{seq}", now_ms())
    }

    async fn call_inner(
        &self,
        command: &str,
        args: Vec<WireValue>,
    ) -> Result<WireValue, ServiceError> {
        let args = ServiceArgs::new(args);
        match command {
            "listTaskList" => self.handle.list_task_list(&args).await,
            "mutateTask" => self.handle.mutate_task(&args).await,
            "deleteArchivedTask" => self.handle.delete_archived_task(&args).await,
            "deleteArchivedTasks" => self.handle.delete_archived_tasks(&args).await,
            "subscribeControllerV4" => {
                let subscription_id = self.next_id("controller-sub");
                self.handle.subscribe(&args, &subscription_id).await
            }
            "resyncControllerV4" => self.handle.resync(&args).await,
            "unsubscribeControllerV4" => self.handle.unsubscribe(&args).await,
            other => Err(ServiceError::message(format!("Method not found: {other}"))),
        }
    }
}

impl Handle {
    async fn tasks_call_on(
        &self,
        tasks: &Arc<dyn Channel>,
        command: &str,
        params: Value,
    ) -> Result<Value, ServiceError> {
        match tasks
            .call(
                &self.ctx,
                command,
                vec![WireValue::Json(params)],
                CancelToken::new(),
            )
            .await
        {
            Ok(WireValue::Json(value)) => Ok(value),
            Ok(WireValue::Undefined) => Ok(Value::Null),
            Ok(other) => Ok(Value::String(
                other.as_str().unwrap_or_default().to_string(),
            )),
            Err(failure) => Err(to_service_error(failure)),
        }
    }

    /// Resolve the owning facade for a scope: local always hits `tasks`;
    /// remote hits the window registry (spec §2). Unknown/mismatched remote
    /// fails closed with the registry's named `RemoteSourceUnavailable`.
    fn resolve_tasks(
        &self,
        remote: Option<&str>,
        path: &str,
        identity: Option<&str>,
    ) -> Result<Arc<dyn Channel>, ServiceError> {
        match remote {
            None => {
                let tasks: Arc<dyn Channel> = self.tasks.clone();
                Ok(tasks)
            }
            Some(id) => {
                let identity = identity.ok_or_else(|| {
                    ServiceError::invalid_params(
                        "workspaceIdentity is required for a remote source",
                    )
                })?;
                self.remotes.resolve_scoped(id, path, identity)
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Projection> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Read the source's task index and publish the resulting frames (mirrors
    /// `refreshSource` → `projection.replaceSourceSnapshot`).
    async fn refresh(
        &self,
        remote: Option<&str>,
        path: &str,
        identity: Option<&str>,
    ) -> Result<(), ServiceError> {
        let tasks = self.resolve_tasks(remote, path, identity)?;
        let rows = self.build_rows(&tasks, remote, path, identity).await?;
        let frames = {
            let mut projection = self.lock();
            projection.replace_source_snapshot(remote, path, identity, rows, now_ms())
        };
        for frame in frames {
            self.bus.send(WireValue::Json(frame));
        }
        Ok(())
    }

    /// Register a scope once and start its event listener (mirrors `registerResolvedSource`
    /// subscribing `onDynamicWorkspaceEvent` to trigger refreshes).
    async fn ensure_source(&self, remote: Option<String>, path: &str, identity: Option<&str>) {
        let should_listen = {
            let mut projection = self.lock();
            projection
                .listening
                .insert(scope_key_remote(remote.as_deref(), path, identity))
        };
        // Best-effort first refresh so the projection has rows before frames start.
        let _ = self.refresh(remote.as_deref(), path, identity).await;
        if !should_listen {
            return;
        }
        let tasks = match self.resolve_tasks(remote.as_deref(), path, identity) {
            Ok(tasks) => tasks,
            Err(_) => return,
        };
        let handle = self.clone();
        let path = path.to_string();
        let identity = identity.map(str::to_string);
        let remote_clone = remote.clone();
        let mut arg = Map::new();
        arg.insert("workspacePath".to_string(), json!(path));
        if let Some(identity) = &identity {
            arg.insert("workspaceIdentity".to_string(), json!(identity));
        }
        tokio::spawn(async move {
            let ctx = handle.ctx.clone();
            let subscription = match tasks
                .listen(
                    &ctx,
                    "onDynamicWorkspaceEvent",
                    WireValue::Json(Value::Object(arg)),
                )
                .await
            {
                Ok(subscription) => subscription,
                Err(_) => return,
            };
            let mut subscription = subscription;
            while subscription.next().await.is_some() {
                let _ = handle
                    .refresh(remote_clone.as_deref(), path.as_str(), identity.as_deref())
                    .await;
            }
        });
    }

    async fn build_rows(
        &self,
        tasks: &Arc<dyn Channel>,
        remote: Option<&str>,
        path: &str,
        identity: Option<&str>,
    ) -> Result<BTreeMap<String, Value>, ServiceError> {
        let mut params = Map::new();
        params.insert("workspacePath".to_string(), json!(path));
        if let Some(identity) = identity {
            params.insert("workspaceIdentity".to_string(), json!(identity));
        }
        let params = Value::Object(params);
        // Mirrors `readSourceTaskIndex`: active, then pinned, then archived each overwrite
        // the membership for a task, so a pinned+active task lands as {pinned:true, active:true}.
        let mut rows: BTreeMap<String, Value> = BTreeMap::new();
        for (command, pinned, archived, active) in [
            ("listTasks", false, false, true),
            ("listPinnedTasks", true, false, true),
            ("listArchivedTasks", false, true, false),
        ] {
            let items = self.tasks_call_on(tasks, command, params.clone()).await?;
            let items = items.as_array().cloned().unwrap_or_default();
            for item in items {
                let task_id = item.get("taskId").and_then(Value::as_str).unwrap_or("");
                let row_path = item
                    .get("workspacePath")
                    .and_then(Value::as_str)
                    .unwrap_or(path);
                let row_identity = item
                    .get("workspaceIdentity")
                    .and_then(Value::as_str)
                    .or(identity);
                rows.insert(
                    task_key_remote(remote, task_id, row_path, row_identity),
                    crate::controller_projection::task_row_remote(
                        remote, &item, pinned, archived, active,
                    ),
                );
            }
        }
        Ok(rows)
    }

    async fn list_task_list(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let query = args.required_json(0, "params")?;
        let scopes = query
            .get("workspaceScopes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // Parse scopes into (remote, path, identity) once; each source is
        // refreshed through its owning facade.
        let mut parsed: Vec<(Option<String>, String, Option<String>)> = Vec::new();
        for scope in &scopes {
            let path = required_str(scope, "workspacePath")?;
            let identity = optional_str(scope, "workspaceIdentity");
            let remote = optional_str(scope, "remoteSessionId");
            if let Some(id) = remote.as_deref() {
                if identity.is_none() {
                    return Err(ServiceError::invalid_params(
                        "workspaceIdentity is required for a remote source",
                    ));
                }
                let _ = id;
            }
            parsed.push((remote.clone(), path.clone(), identity.clone()));
            self.ensure_source(remote.clone(), &path, identity.as_deref())
                .await;
        }
        // Delegate per scope through the owning facade (local or remote), then
        // add the projection-only fields (`sourceAvailability`, `liveStatus`)
        // the TS list returns. Per-scope calls preserve the facade's per-scope
        // sort/limit/total semantics while routing remote scopes correctly.
        let mut merged_items: Vec<Value> = Vec::new();
        let mut total = 0usize;
        let mut has_more = false;
        for (remote, path, identity) in &parsed {
            let tasks = self.resolve_tasks(remote.as_deref(), path, identity.as_deref())?;
            let mut single = Map::new();
            if let Some(kind) = query.get("kind") {
                single.insert("kind".to_string(), kind.clone());
            }
            if let Some(sort_by) = query.get("sortBy") {
                single.insert("sortBy".to_string(), sort_by.clone());
            }
            if let Some(search) = query.get("search") {
                single.insert("search".to_string(), search.clone());
            }
            if let Some(limit) = query.get("limit") {
                single.insert("limit".to_string(), limit.clone());
            }
            let mut scope_obj = Map::new();
            scope_obj.insert("workspacePath".to_string(), json!(path));
            if let Some(identity) = identity {
                scope_obj.insert("workspaceIdentity".to_string(), json!(identity));
            }
            single.insert(
                "workspaceScopes".to_string(),
                json!([Value::Object(scope_obj)]),
            );
            let result = self
                .tasks_call_on(&tasks, "listTaskList", Value::Object(single))
                .await?;
            total += result.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
            has_more |= result
                .get("hasMore")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let items = result
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for mut item in items {
                let live_status = crate::controller_projection::default_live_status(
                    item.get("status").and_then(Value::as_str),
                );
                if let Some(object) = item.as_object_mut() {
                    object.insert("sourceAvailability".to_string(), json!("online"));
                    object.insert("liveStatus".to_string(), json!(live_status));
                    if let Some(id) = remote {
                        object.insert("remoteSessionId".to_string(), json!(id));
                    }
                }
                merged_items.push(item);
            }
        }
        Ok(WireValue::Json(json!({
            "items": merged_items,
            "total": total,
            "hasMore": has_more,
        })))
    }

    async fn mutate_task(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let (task_id, path, identity, remote) = parse_address(&params)?;
        let tasks = self.resolve_tasks(remote.as_deref(), &path, identity.as_deref())?;
        let mutation = params
            .get("mutation")
            .cloned()
            .ok_or_else(|| ServiceError::invalid_params("params.mutation is required"))?;
        let kind = required_str(&mutation, "kind")?;
        let base = mutation_params(&task_id, &path, identity.as_deref());
        // Delegates to the same write path the TS runtime uses; the delete kinds leave the
        // projection, so their return is null like the TS `?.meta ?? null`.
        let removed_from_list = match kind.as_str() {
            "pin" => {
                let mut with_flag = base.clone();
                with_flag.insert(
                    "pinned".to_string(),
                    mutation.get("pinned").cloned().unwrap_or(json!(false)),
                );
                self.tasks_call_on(&tasks, "setTaskPinned", Value::Object(with_flag))
                    .await?;
                false
            }
            "archive" => {
                let archived = mutation
                    .get("archived")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let command = if archived {
                    "archiveTask"
                } else {
                    "unarchiveTask"
                };
                self.tasks_call_on(&tasks, command, Value::Object(base.clone()))
                    .await?;
                false
            }
            "delete" => {
                self.tasks_call_on(&tasks, "deleteTask", Value::Object(base.clone()))
                    .await?;
                true
            }
            "delete-archived" => {
                self.tasks_call_on(&tasks, "deleteArchivedTask", Value::Object(base.clone()))
                    .await?;
                true
            }
            "mark-read" => {
                let mut with_flag = base.clone();
                with_flag.insert("unread".to_string(), json!(false));
                if let Some(expected) = mutation.get("expectedUnreadAt") {
                    with_flag.insert("expectedUnreadAt".to_string(), expected.clone());
                }
                self.tasks_call_on(&tasks, "setTaskUnread", Value::Object(with_flag))
                    .await?;
                false
            }
            "mark-unread" => {
                let mut with_flag = base.clone();
                with_flag.insert("unread".to_string(), json!(true));
                self.tasks_call_on(&tasks, "setTaskUnread", Value::Object(with_flag))
                    .await?;
                false
            }
            // open/resume only validate the unique source in the Controller; the conversation
            // write goes through the scoped facade, so there is no state change here.
            "open" | "resume" => false,
            other => {
                return Err(ServiceError::message(format!(
                    "unknown Controller mutation: {other}"
                )))
            }
        };
        let _ = self
            .refresh(remote.as_deref(), &path, identity.as_deref())
            .await;
        if removed_from_list {
            return Ok(WireValue::Json(Value::Null));
        }
        let meta = self
            .tasks_call_on(&tasks, "getTaskMeta", Value::Object(base))
            .await?;
        Ok(WireValue::Json(meta))
    }

    async fn delete_archived_task(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let (task_id, path, identity, remote) = parse_address(&params)?;
        let tasks = self.resolve_tasks(remote.as_deref(), &path, identity.as_deref())?;
        let base = mutation_params(&task_id, &path, identity.as_deref());
        let deleted = self
            .tasks_call_on(&tasks, "deleteArchivedTask", Value::Object(base))
            .await?;
        let _ = self
            .refresh(remote.as_deref(), &path, identity.as_deref())
            .await;
        Ok(WireValue::Json(json!(deleted.as_bool().unwrap_or(false))))
    }

    async fn delete_archived_tasks(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let (task_id, path, identity, remote) = parse_address(&params)?;
        let tasks = self.resolve_tasks(remote.as_deref(), &path, identity.as_deref())?;
        let task_ids: Vec<Value> = params
            .get("taskIds")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if task_ids.is_empty() {
            return Ok(WireValue::Json(json!({
                "deletedTaskIds": [],
                "skippedTaskIds": [],
                "failedTaskIds": [],
            })));
        }
        let mut batch = Map::new();
        batch.insert("workspacePath".to_string(), json!(path));
        if let Some(identity) = &identity {
            batch.insert("workspaceIdentity".to_string(), json!(identity));
        }
        batch.insert("taskIds".to_string(), Value::Array(task_ids));
        let result = self
            .tasks_call_on(&tasks, "deleteArchivedTasks", Value::Object(batch))
            .await?;
        let _ = self
            .refresh(remote.as_deref(), &path, identity.as_deref())
            .await;
        // `task_id` is validated by parse_address but the batch deletes many; keep it read.
        let _ = task_id;
        Ok(WireValue::Json(result))
    }

    async fn subscribe(
        &self,
        args: &ServiceArgs,
        subscription_id: &str,
    ) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let topic = required_str(&params, "topic")?;
        if topic != TOPIC_TASKS && topic != TOPIC_WORKSPACES {
            return Err(ServiceError::invalid_params(format!(
                "params.topic must be one of {TOPIC_TASKS} or {TOPIC_WORKSPACES}"
            )));
        }
        let (frame, log_epoch) = {
            let mut projection = self.lock();
            projection.add_subscriber(subscription_id, &topic);
            let frame = projection.snapshot_frame_for(subscription_id, &topic, now_ms());
            (frame, projection.topic_log_epoch(&topic))
        };
        // The TS subscribe fires the snapshot frame before resolving; the client reads it
        // off onDynamicControllerFrame using the ack's subscriptionId.
        self.bus.send(WireValue::Json(frame));
        Ok(WireValue::Json(json!({
            "ack": { "subscriptionId": subscription_id, "mode": "snapshot", "logEpoch": log_epoch },
        })))
    }

    async fn resync(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let subscription_id = required_str(&params, "subscriptionId")?;
        let (frame, log_epoch) = {
            let projection = self.lock();
            let topic = projection
                .subscriber_topic(&subscription_id)
                .ok_or_else(|| {
                    ServiceError::message(format!(
                        "Controller subscription not found, subscriptionId={subscription_id}"
                    ))
                })?
                .clone();
            let frame = projection.snapshot_frame_for(&subscription_id, &topic, now_ms());
            (frame, projection.topic_log_epoch(&topic))
        };
        self.bus.send(WireValue::Json(frame));
        Ok(WireValue::Json(json!({
            "ack": { "subscriptionId": subscription_id, "mode": "snapshot", "logEpoch": log_epoch },
        })))
    }

    async fn unsubscribe(&self, args: &ServiceArgs) -> Result<WireValue, ServiceError> {
        let params = args.required_json(0, "params")?;
        let subscription_id = required_str(&params, "subscriptionId")?;
        self.lock().remove_subscriber(&subscription_id);
        Ok(WireValue::Undefined)
    }
}

#[async_trait]
impl Channel for WindowControllerService {
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
        _arg: WireValue,
    ) -> Result<EventSubscription, ChannelFailure> {
        match event {
            "onDynamicControllerFrame" => Ok(EventSubscription::from_broadcast(
                self.handle.bus.subscribe(),
            )),
            other => Err(ChannelFailure::event_not_found(other)),
        }
    }
}

fn to_service_error(failure: ChannelFailure) -> ServiceError {
    match failure.payload() {
        Some(payload) => ServiceError::named(payload.name.clone(), payload.message.clone()),
        None => ServiceError::new(failure.to_string()),
    }
}

/// `WindowHostTaskAddress`: taskId + workspacePath always; a `remoteSessionId`
/// additionally requires `workspaceIdentity` (TS schema superRefine) and
/// resolves through the window registry (spec §2). Returns
/// `(task_id, path, identity, remote_session_id)`.
fn parse_address(
    params: &Value,
) -> Result<(String, String, Option<String>, Option<String>), ServiceError> {
    let address = params
        .get("address")
        .ok_or_else(|| ServiceError::invalid_params("params.address is required"))?;
    let task_id = required_str(address, "taskId")?;
    let path = required_str(address, "workspacePath")?;
    let identity = optional_str(address, "workspaceIdentity");
    let remote = optional_str(address, "remoteSessionId");
    if remote.is_some() && identity.is_none() {
        return Err(ServiceError::invalid_params(
            "remote task address requires workspaceIdentity",
        ));
    }
    Ok((task_id, path, identity, remote))
}

fn mutation_params(task_id: &str, path: &str, identity: Option<&str>) -> Map<String, Value> {
    // `mutationParams(address)` in the TS runtime: taskId, workspacePath, optional identity.
    let mut params = Map::new();
    params.insert("taskId".to_string(), json!(task_id));
    params.insert("workspacePath".to_string(), json!(path));
    if let Some(identity) = identity {
        params.insert("workspaceIdentity".to_string(), json!(identity));
    }
    params
}

fn required_str(value: &Value, key: &str) -> Result<String, ServiceError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| ServiceError::invalid_params(format!("{key} is required")))
}

fn optional_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn address_accepts_remote_with_identity_and_requires_fields() {
        // Remote with identity parses (routing is the registry's job, not the parser's).
        let parsed = parse_address(&json!({ "address": {
            "workspacePath": "/p", "taskId": "t", "remoteSessionId": "r", "workspaceIdentity": "id"
        } }))
        .expect("remote with identity must parse");
        assert_eq!(parsed.3, Some("r".to_string()));
        // Remote without identity is a TS-schema validation failure (superRefine),
        // not a routing failure.
        let error = parse_address(&json!({ "address": {
            "workspacePath": "/p", "taskId": "t", "remoteSessionId": "r"
        } }))
        .expect_err("remote without identity");
        assert_eq!(error.name, "ZodError");

        let error = parse_address(&json!({ "address": { "workspacePath": "/p" } }))
            .expect_err("missing taskId");
        assert_eq!(error.name, "ZodError");
    }

    #[test]
    fn mutation_params_matches_the_typescript_shape() {
        // TS mutationParams(address) passes address.workspaceIdentity through verbatim and does not trim it
        // (packages/desktop/src/host/windowHostControllerService.ts:70-76);
        // trimming only happens on keys like taskKey/scopeKey (see keys_prefer_identity_and_are_stable).
        let params = mutation_params("t", "/p", Some(" id "));
        assert_eq!(
            params,
            Map::from_iter([
                ("taskId".to_string(), json!("t")),
                ("workspacePath".to_string(), json!("/p")),
                ("workspaceIdentity".to_string(), json!(" id ")),
            ])
        );
        let without = mutation_params("t", "/p", None);
        assert!(!without.contains_key("workspaceIdentity"));
    }

    #[test]
    fn topic_constants_match_the_v4_protocol() {
        assert_eq!(TOPIC_TASKS, "controller/tasks-index");
        assert_eq!(TOPIC_WORKSPACES, "controller/workspaces");
    }
}
