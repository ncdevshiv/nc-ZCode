//! Window Controller projection state and V4 frame builders.
//!
//! Faithful port of `packages/desktop/src/host/windowHostControllerProjection.ts`
//! (`controller.ts` schemas in `packages/shared/src/nc-code-protocol-v4/`). It owns the
//! per-topic log cursor (`logEpoch`/`seq`), the source row maps and the subscriber set,
//! and turns a row diff into `WindowHostControllerFrame` JSON. All frame field names are
//! camelCase and match the zod schemas; `strict()` objects carry exactly their keys.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{json, Map, Value};

pub const TOPIC_TASKS: &str = "controller/tasks-index";
pub const TOPIC_WORKSPACES: &str = "controller/workspaces";

pub fn workspace_key(path: &str, identity: Option<&str>) -> String {
    identity
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(path)
        .to_string()
}

pub fn scope_key(path: &str, identity: Option<&str>) -> String {
    scope_key_remote(None, path, identity)
}

pub fn scope_key_remote(remote: Option<&str>, path: &str, identity: Option<&str>) -> String {
    match remote {
        Some(id) => format!("remote\0{id}\0{}", workspace_key(path, identity)),
        None => format!("local\0{}", workspace_key(path, identity)),
    }
}

/// `taskKey` — `local`/remoteSessionId, workspace key, taskId (`controller.ts` taskKey).
pub fn task_key(task_id: &str, path: &str, identity: Option<&str>) -> String {
    task_key_remote(None, task_id, path, identity)
}

pub fn task_key_remote(
    remote: Option<&str>,
    task_id: &str,
    path: &str,
    identity: Option<&str>,
) -> String {
    match remote {
        Some(id) => format!("remote\0{id}\0{}\0{task_id}", workspace_key(path, identity)),
        None => format!("local\0{}\0{task_id}", workspace_key(path, identity)),
    }
}

/// `addressFor(scope, taskId)`: remote sources carry `remoteSessionId`.
pub fn address_json(task_id: &str, path: &str, identity: Option<&str>) -> Value {
    address_json_remote(None, task_id, path, identity)
}

pub fn address_json_remote(
    remote: Option<&str>,
    task_id: &str,
    path: &str,
    identity: Option<&str>,
) -> Value {
    let mut object = Map::new();
    if let Some(id) = remote {
        object.insert("remoteSessionId".to_string(), json!(id));
    }
    object.insert("workspacePath".to_string(), json!(path));
    if let Some(identity) = identity {
        object.insert("workspaceIdentity".to_string(), json!(identity));
    }
    object.insert("taskId".to_string(), json!(task_id));
    Value::Object(object)
}

/// `defaultLiveStatus(meta)` — no live overlay subsystem in this host, so persisted
/// `status` maps to the idle/completed/error rows exactly as `defaultLiveStatus` does.
pub fn default_live_status(status: Option<&str>) -> &'static str {
    match status {
        Some("completed") => "completed",
        Some("error") => "error",
        _ => "idle",
    }
}

/// Build a `WindowHostControllerTaskRow` from a list item (a `ZCodeTaskMeta`) plus the
/// membership the source-level queries resolved.
pub fn task_row(item: &Value, pinned: bool, archived: bool, active: bool) -> Value {
    task_row_remote(None, item, pinned, archived, active)
}

pub fn task_row_remote(
    remote: Option<&str>,
    item: &Value,
    pinned: bool,
    archived: bool,
    active: bool,
) -> Value {
    let task_id = item.get("taskId").and_then(Value::as_str).unwrap_or("");
    let path = item
        .get("workspacePath")
        .and_then(Value::as_str)
        .unwrap_or("");
    let identity = item.get("workspaceIdentity").and_then(Value::as_str);
    let live_status = default_live_status(item.get("status").and_then(Value::as_str));
    json!({
        "address": address_json_remote(remote, task_id, path, identity),
        "meta": item,
        "membership": { "pinned": pinned, "archived": archived, "active": active },
        "sourceAvailability": "online",
        "liveStatus": live_status,
    })
}

pub fn workspace_fact(path: &str, identity: Option<&str>) -> Value {
    workspace_fact_remote(None, path, identity)
}

pub fn workspace_fact_remote(remote: Option<&str>, path: &str, identity: Option<&str>) -> Value {
    let mut object = Map::new();
    if let Some(id) = remote {
        object.insert("remoteSessionId".to_string(), json!(id));
    }
    object.insert("workspacePath".to_string(), json!(path));
    if let Some(identity) = identity {
        object.insert("workspaceIdentity".to_string(), json!(identity));
    }
    object.insert("sourceAvailability".to_string(), json!("online"));
    object.insert("connectionState".to_string(), json!("online"));
    Value::Object(object)
}

#[derive(Clone)]
struct Source {
    remote_session_id: Option<String>,
    workspace_path: String,
    workspace_identity: Option<String>,
    rows: BTreeMap<String, Value>,
}

#[derive(Clone)]
struct TopicState {
    log_epoch: String,
    seq: u64,
}

/// All projection state guarded by the service's mutex.
pub struct Projection {
    sources: BTreeMap<String, Source>,
    subscribers: HashMap<String, String>,
    tasks_topic: TopicState,
    workspaces_topic: TopicState,
    pub listening: HashSet<String>,
}

impl Projection {
    pub fn new(tasks_epoch: String, workspaces_epoch: String) -> Self {
        Self {
            sources: BTreeMap::new(),
            subscribers: HashMap::new(),
            tasks_topic: TopicState {
                log_epoch: tasks_epoch,
                seq: 0,
            },
            workspaces_topic: TopicState {
                log_epoch: workspaces_epoch,
                seq: 0,
            },
            listening: HashSet::new(),
        }
    }

    pub fn topic_log_epoch(&self, topic: &str) -> String {
        if topic == TOPIC_WORKSPACES {
            self.workspaces_topic.log_epoch.clone()
        } else {
            self.tasks_topic.log_epoch.clone()
        }
    }

    pub fn has_source(&self, path: &str, identity: Option<&str>) -> bool {
        self.sources.contains_key(&scope_key(path, identity))
    }

    pub fn has_source_remote(
        &self,
        remote: Option<&str>,
        path: &str,
        identity: Option<&str>,
    ) -> bool {
        self.sources
            .contains_key(&scope_key_remote(remote, path, identity))
    }

    pub fn add_subscriber(&mut self, subscription_id: &str, topic: &str) {
        self.subscribers
            .insert(subscription_id.to_string(), topic.to_string());
    }

    pub fn remove_subscriber(&mut self, subscription_id: &str) -> bool {
        self.subscribers.remove(subscription_id).is_some()
    }

    pub fn subscriber_topic(&self, subscription_id: &str) -> Option<&String> {
        self.subscribers.get(subscription_id)
    }

    fn snapshot_frame(&self, subscription_id: &str, topic: &str, now: u64) -> Value {
        let (state, payload_snapshot) = if topic == TOPIC_WORKSPACES {
            (
                &self.workspaces_topic,
                json!({
                    "protocolVersion": 1,
                    "logEpoch": self.workspaces_topic.log_epoch,
                    "workspaces": self.all_workspace_facts(),
                }),
            )
        } else {
            (
                &self.tasks_topic,
                json!({
                    "protocolVersion": 1,
                    "logEpoch": self.tasks_topic.log_epoch,
                    "tasks": self.all_task_rows(),
                }),
            )
        };
        json!({
            "topic": topic,
            "subscriptionId": subscription_id,
            "logEpoch": state.log_epoch,
            "fromSeq": 0,
            "toSeq": state.seq,
            "sentAt": now,
            "payload": { "kind": "snapshot", "snapshot": payload_snapshot },
        })
    }

    /// Emit the snapshot frame for one subscription (the TS subscribe/resync `onFrame`).
    pub fn snapshot_frame_for(&self, subscription_id: &str, topic: &str, now: u64) -> Value {
        self.snapshot_frame(subscription_id, topic, now)
    }

    fn all_task_rows(&self) -> Vec<Value> {
        let mut rows: Vec<(String, Value)> = self
            .sources
            .values()
            .flat_map(|source| source.rows.clone().into_iter())
            .collect();
        rows.sort_by(|left, right| {
            let left_updated = left.1["meta"]["updatedAt"].as_i64().unwrap_or(0);
            let right_updated = right.1["meta"]["updatedAt"].as_i64().unwrap_or(0);
            right_updated
                .cmp(&left_updated)
                .then_with(|| left.0.cmp(&right.0))
        });
        rows.into_iter().map(|(_, row)| row).collect()
    }

    fn all_workspace_facts(&self) -> Vec<Value> {
        let mut facts: Vec<(String, Value)> = self
            .sources
            .values()
            .map(|source| {
                let key = match &source.remote_session_id {
                    Some(id) => format!(
                        "remote\0{id}\0{}",
                        source
                            .workspace_identity
                            .clone()
                            .unwrap_or_else(|| source.workspace_path.clone())
                    ),
                    None => format!(
                        "local\0{}",
                        source
                            .workspace_identity
                            .clone()
                            .unwrap_or_else(|| source.workspace_path.clone())
                    ),
                };
                (
                    key,
                    workspace_fact_remote(
                        source.remote_session_id.as_deref(),
                        &source.workspace_path,
                        source.workspace_identity.as_deref(),
                    ),
                )
            })
            .collect();
        facts.sort_by(|left, right| left.0.cmp(&right.0));
        facts.into_iter().map(|(_, fact)| fact).collect()
    }

    /// Replace a source's rows and return the frame JSON to publish (task deltas first,
    /// then any workspace upsert), advancing the per-topic cursors like the TS projection.
    pub fn replace_source_snapshot(
        &mut self,
        remote: Option<&str>,
        path: &str,
        identity: Option<&str>,
        next_rows: BTreeMap<String, Value>,
        now: u64,
    ) -> Vec<Value> {
        let key = scope_key_remote(remote, path, identity);
        let is_new = !self.sources.contains_key(&key);
        let previous_rows = self
            .sources
            .get(&key)
            .map(|source| source.rows.clone())
            .unwrap_or_default();
        let previous_fact = self.sources.get(&key).map(|source| {
            workspace_fact_remote(
                source.remote_session_id.as_deref(),
                &source.workspace_path,
                source.workspace_identity.as_deref(),
            )
        });

        let source = self.sources.entry(key).or_insert_with(|| Source {
            remote_session_id: remote.map(str::to_string),
            workspace_path: path.to_string(),
            workspace_identity: identity.map(str::to_string),
            rows: BTreeMap::new(),
        });
        source.remote_session_id = remote.map(str::to_string);
        source.rows = next_rows;
        let next_fact = workspace_fact_remote(
            source.remote_session_id.as_deref(),
            &source.workspace_path,
            source.workspace_identity.as_deref(),
        );

        let mut task_deltas: Vec<Value> = Vec::new();
        for (row_key, previous) in &previous_rows {
            if !source.rows.contains_key(row_key) {
                task_deltas.push(json!({
                    "op": "task.removed",
                    "address": previous["address"].clone(),
                }));
            }
        }
        for (row_key, row) in &source.rows {
            if previous_rows.get(row_key) != Some(row) {
                task_deltas.push(json!({ "op": "task.upserted", "task": row.clone() }));
            }
        }

        let mut frames = self.delta_frames(TOPIC_TASKS, task_deltas, now);
        let workspace_deltas: Vec<Value> = if is_new || previous_fact.as_ref() != Some(&next_fact) {
            vec![json!({ "op": "workspace.upserted", "workspace": next_fact })]
        } else {
            Vec::new()
        };
        frames.extend(self.delta_frames(TOPIC_WORKSPACES, workspace_deltas, now));
        frames
    }

    /// Bump the topic cursor once (only when there are deltas) and emit one frame per
    /// subscriber on that topic, mirroring `publishTaskDeltas`/`publishWorkspaceDeltas`.
    pub fn delta_frames(&mut self, topic: &str, deltas: Vec<Value>, now: u64) -> Vec<Value> {
        if deltas.is_empty() {
            return Vec::new();
        }
        let state = if topic == TOPIC_WORKSPACES {
            &mut self.workspaces_topic
        } else {
            &mut self.tasks_topic
        };
        let from_seq = state.seq;
        state.seq += 1;
        let log_epoch = state.log_epoch.clone();
        self.subscribers
            .iter()
            .filter(|(_, subscriber_topic)| *subscriber_topic == topic)
            .map(|(subscription_id, _)| {
                json!({
                    "topic": topic,
                    "subscriptionId": subscription_id,
                    "logEpoch": log_epoch,
                    "fromSeq": from_seq,
                    "toSeq": state.seq,
                    "sentAt": now,
                    "payload": { "kind": "deltas", "deltas": deltas },
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(task_id: &str, updated: i64, status: &str) -> Value {
        let item = json!({
            "taskId": task_id,
            "workspacePath": "/ws",
            "updatedAt": updated,
            "status": status,
        });
        task_row(&item, false, false, true)
    }

    #[test]
    fn keys_prefer_identity_and_are_stable() {
        assert_eq!(task_key("t", "/p", Some(" id ")), "local\0id\0t");
        assert_eq!(task_key("t", "/p", None), "local\0/p\0t");
        assert_eq!(scope_key("/p", Some("")), "local\0/p");
        assert_eq!(
            scope_key_remote(Some("r"), "/p", Some("id")),
            "remote\0r\0id"
        );
        assert_eq!(
            task_key_remote(Some("r"), "t", "/p", Some("id")),
            "remote\0r\0id\0t"
        );
    }

    #[test]
    fn snapshot_frame_matches_the_v4_shape() {
        let mut projection = Projection::new("epoch-t".into(), "epoch-w".into());
        let mut rows = BTreeMap::new();
        rows.insert(task_key("t", "/ws", None), row("t", 5, "completed"));
        projection.replace_source_snapshot(None, "/ws", None, rows, 111);
        let frame = projection.snapshot_frame_for("sub-1", TOPIC_TASKS, 222);
        assert_eq!(frame["topic"], json!(TOPIC_TASKS));
        assert_eq!(frame["logEpoch"], json!("epoch-t"));
        assert_eq!(frame["fromSeq"], json!(0));
        assert_eq!(frame["payload"]["kind"], json!("snapshot"));
        let tasks = frame["payload"]["snapshot"]["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["liveStatus"], json!("completed"));
        assert_eq!(tasks[0]["sourceAvailability"], json!("online"));
        assert!(tasks[0]["address"].get("remoteSessionId").is_none());
    }

    #[test]
    fn remote_snapshot_carries_remote_session_id() {
        let mut projection = Projection::new("epoch-t".into(), "epoch-w".into());
        let mut rows = BTreeMap::new();
        let item = json!({ "taskId": "t", "workspacePath": "/ws", "workspaceIdentity": "id", "updatedAt": 5, "status": "idle" });
        let remote_row = task_row_remote(Some("r1"), &item, false, false, true);
        rows.insert(
            task_key_remote(Some("r1"), "t", "/ws", Some("id")),
            remote_row,
        );
        projection.replace_source_snapshot(Some("r1"), "/ws", Some("id"), rows, 111);
        let frame = projection.snapshot_frame_for("sub", TOPIC_TASKS, 222);
        let tasks = frame["payload"]["snapshot"]["tasks"].as_array().unwrap();
        assert_eq!(tasks[0]["address"]["remoteSessionId"], json!("r1"));
        let wf = projection.snapshot_frame_for("sub", TOPIC_WORKSPACES, 222);
        let w = wf["payload"]["snapshot"]["workspaces"].as_array().unwrap();
        assert_eq!(w[0]["remoteSessionId"], json!("r1"));
    }

    #[test]
    fn deltas_bump_cursor_only_when_nonempty_and_target_subscribers() {
        let mut projection = Projection::new("e".into(), "w".into());
        projection.add_subscriber("s1", TOPIC_TASKS);
        // no deltas -> no seq bump
        assert!(projection.delta_frames(TOPIC_TASKS, vec![], 1).is_empty());
        let mut rows = BTreeMap::new();
        rows.insert(task_key("a", "/ws", None), row("a", 1, "idle"));
        let frames = projection.replace_source_snapshot(None, "/ws", None, rows, 2);
        // Only a TOPIC_TASKS subscriber: workspace frames are filtered out by topic (in the TS
        // publishWorkspaceDeltas, it continues when subscriber.topic !== CONTROLLER_WORKSPACES_TOPIC,
        // windowHostControllerProjection.ts:291-293), so there is only one frame here—
        // but the workspace topic's cursor still advances and never replays the old sequence.
        assert_eq!(frames.len(), 1);
        let task_frame = &frames[0];
        assert_eq!(task_frame["subscriptionId"], json!("s1"));
        assert_eq!(task_frame["fromSeq"], json!(0));
        assert_eq!(task_frame["toSeq"], json!(1));
        assert_eq!(task_frame["payload"]["kind"], json!("deltas"));
    }
}
