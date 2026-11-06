//! v4 window-controller stdio surface (`v4/controller/subscribe|resync|unsubscribe`).
//!
//! Mirrors TS `windowHostControllerService` + `windowHostControllerProjection`
//! with zero new crates: topics `controller/tasks-index` / `controller/workspaces`,
//! ACK-only responses with post-response snapshot notifications
//! (`v4/controller/frame`, params = controller frame JSON).
//!
//! Source of truth: the host's live sessions (`session_view`). Pinned/archived
//! membership is owned by the channel `window-controller` (TaskIndexStore); the
//! stdio projection reports default membership and is documented as such.
//! Row mapping lives in `controller_row.rs` (same census as the 400-line rule).

use serde_json::{json, Value};
use std::collections::HashMap;

use crate::controller_row::{
    controller_tasks_snapshot_impl, controller_workspaces_snapshot_impl, task_row_for_view,
};
use crate::rpc::{RpcError, RpcNotification, CODE_INVALID_PARAMS, NOTIFICATION_CONTROLLER_FRAME};
use crate::{HostServer, RequestOutcome};

pub const TOPIC_TASKS: &str = "controller/tasks-index";
pub const TOPIC_WORKSPACES: &str = "controller/workspaces";

#[derive(Clone)]
struct TopicState {
    log_epoch: String,
    seq: u64,
}

pub struct ControllerState {
    subscribers: HashMap<String, String>,
    tasks_topic: TopicState,
    workspaces_topic: TopicState,
}

impl ControllerState {
    pub fn new(tasks_epoch: String, workspaces_epoch: String) -> Self {
        Self {
            subscribers: HashMap::new(),
            tasks_topic: TopicState {
                log_epoch: tasks_epoch,
                seq: 0,
            },
            workspaces_topic: TopicState {
                log_epoch: workspaces_epoch,
                seq: 0,
            },
        }
    }
    fn topic_state(&self, topic: &str) -> &TopicState {
        if topic == TOPIC_WORKSPACES {
            &self.workspaces_topic
        } else {
            &self.tasks_topic
        }
    }
    fn topic_state_mut(&mut self, topic: &str) -> &mut TopicState {
        if topic == TOPIC_WORKSPACES {
            &mut self.workspaces_topic
        } else {
            &mut self.tasks_topic
        }
    }
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::new(CODE_INVALID_PARAMS, message)
}

fn frame_line(
    subscription_id: &str,
    topic: &str,
    log_epoch: &str,
    seq: u64,
    payload: Value,
    now: i64,
) -> String {
    let frame = json!({
        "topic": topic, "subscriptionId": subscription_id, "logEpoch": log_epoch,
        "fromSeq": 0, "toSeq": seq, "sentAt": now, "payload": payload,
    });
    RpcNotification {
        jsonrpc: "2.0",
        method: NOTIFICATION_CONTROLLER_FRAME.to_string(),
        params: frame,
    }
    .to_line()
}

fn delta_line(
    subscription_id: &str,
    topic: &str,
    log_epoch: &str,
    from_seq: u64,
    to_seq: u64,
    deltas: Vec<Value>,
    now: i64,
) -> String {
    let frame = json!({
        "topic": topic, "subscriptionId": subscription_id, "logEpoch": log_epoch,
        "fromSeq": from_seq, "toSeq": to_seq, "sentAt": now,
        "payload": { "kind": "deltas", "deltas": deltas },
    });
    RpcNotification {
        jsonrpc: "2.0",
        method: NOTIFICATION_CONTROLLER_FRAME.to_string(),
        params: frame,
    }
    .to_line()
}

pub fn controller_tasks_snapshot(server: &HostServer) -> (Vec<Value>, String, u64) {
    let rows = controller_tasks_snapshot_impl(server);
    let guard = server.controller_state_lock().map(|g| {
        (
            g.topic_state(TOPIC_TASKS).log_epoch.clone(),
            g.topic_state(TOPIC_TASKS).seq,
        )
    });
    let (epoch, seq) = guard.unwrap_or_default();
    (rows, epoch, seq)
}

pub fn controller_workspaces_snapshot(server: &HostServer) -> (Vec<Value>, String, u64) {
    let rows = controller_workspaces_snapshot_impl(server);
    let guard = server.controller_state_lock().map(|g| {
        (
            g.topic_state(TOPIC_WORKSPACES).log_epoch.clone(),
            g.topic_state(TOPIC_WORKSPACES).seq,
        )
    });
    let (epoch, seq) = guard.unwrap_or_default();
    (rows, epoch, seq)
}

impl HostServer {
    pub(crate) fn controller_state_lock(
        &self,
    ) -> Option<std::sync::MutexGuard<'_, ControllerState>> {
        self.controller.lock().ok()
    }

    fn snapshot_post_frame(&self, subscription_id: &str, topic: &str) -> String {
        let now = crate::store::now_ms();
        if topic == TOPIC_WORKSPACES {
            let (workspaces, epoch, seq) = controller_workspaces_snapshot(self);
            frame_line(
                subscription_id,
                topic,
                &epoch,
                seq,
                json!({
                    "kind": "snapshot",
                    "snapshot": { "protocolVersion": 1, "logEpoch": epoch, "workspaces": workspaces },
                }),
                now,
            )
        } else {
            let (tasks, epoch, seq) = controller_tasks_snapshot(self);
            frame_line(
                subscription_id,
                topic,
                &epoch,
                seq,
                json!({
                    "kind": "snapshot",
                    "snapshot": { "protocolVersion": 1, "logEpoch": epoch, "tasks": tasks },
                }),
                now,
            )
        }
    }

    pub(crate) fn controller_subscribe(
        self: &std::sync::Arc<Self>,
        params: &Value,
    ) -> Result<RequestOutcome, RpcError> {
        let topic = params
            .get("topic")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| invalid("params.topic: required string"))?;
        if topic != TOPIC_TASKS && topic != TOPIC_WORKSPACES {
            return Err(invalid(format!(
                "params.topic must be one of {TOPIC_TASKS} or {TOPIC_WORKSPACES}"
            )));
        }
        let subscription_id = self.next_controller_id("controller");
        {
            let mut state = self
                .controller_state_lock()
                .ok_or_else(|| invalid("controller state locked"))?;
            state
                .subscribers
                .insert(subscription_id.clone(), topic.to_string());
        }
        let post_frame = self.snapshot_post_frame(&subscription_id, topic);
        Ok(RequestOutcome {
            response: json!({ "ack": { "subscriptionId": subscription_id, "mode": "snapshot", "logEpoch": self.controller_log_epoch(topic) } }),
            post_frames: vec![post_frame],
        })
    }

    pub(crate) fn controller_resync(
        self: &std::sync::Arc<Self>,
        params: &Value,
    ) -> Result<RequestOutcome, RpcError> {
        let subscription_id = params
            .get("subscriptionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("params.subscriptionId: required string"))?;
        let topic = {
            let state = self
                .controller_state_lock()
                .ok_or_else(|| invalid("controller state locked"))?;
            state
                .subscribers
                .get(subscription_id)
                .cloned()
                .ok_or_else(|| {
                    invalid(format!(
                        "Controller subscription not found, subscriptionId={subscription_id}"
                    ))
                })?
        };
        let post_frame = self.snapshot_post_frame(subscription_id, &topic);
        Ok(RequestOutcome {
            response: json!({ "ack": { "subscriptionId": subscription_id, "mode": "snapshot", "logEpoch": self.controller_log_epoch(&topic) } }),
            post_frames: vec![post_frame],
        })
    }

    pub(crate) fn controller_unsubscribe(
        &self,
        params: &Value,
    ) -> Result<RequestOutcome, RpcError> {
        let subscription_id = params
            .get("subscriptionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("params.subscriptionId: required string"))?;
        if let Some(mut state) = self.controller_state_lock() {
            state.subscribers.remove(subscription_id);
        }
        Ok(RequestOutcome {
            response: json!({}),
            post_frames: Vec::new(),
        })
    }

    fn controller_log_epoch(&self, topic: &str) -> String {
        self.controller_state_lock()
            .map(|g| g.topic_state(topic).log_epoch.clone())
            .unwrap_or_default()
    }

    pub(crate) fn next_controller_id(&self, prefix: &str) -> String {
        let seq = self
            .id_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{prefix}-{}-{seq}", crate::store::now_ms())
    }

    /// Emit a tasks delta for a newly registered session (real event source:
    /// session creation). No subscribers → no-op.
    pub(crate) fn controller_notify_session(&self, view: &Value) {
        let row = task_row_for_view(view, &self.workspace.to_string_lossy(), None);
        let frames: Vec<String> = {
            let Some(mut state) = self.controller_state_lock() else {
                return;
            };
            let from_seq = state.topic_state(TOPIC_TASKS).seq;
            let targets: Vec<String> = state
                .subscribers
                .iter()
                .filter(|(_, t)| *t == TOPIC_TASKS)
                .map(|(id, _)| id.clone())
                .collect();
            if targets.is_empty() {
                return;
            }
            state.topic_state_mut(TOPIC_TASKS).seq += 1;
            let epoch = state.topic_state(TOPIC_TASKS).log_epoch.clone();
            let to_seq = state.topic_state(TOPIC_TASKS).seq;
            let deltas = vec![json!({ "op": "task.upserted", "task": row })];
            let now = crate::store::now_ms();
            targets
                .iter()
                .map(|id| {
                    delta_line(
                        id,
                        TOPIC_TASKS,
                        &epoch,
                        from_seq,
                        to_seq,
                        deltas.clone(),
                        now,
                    )
                })
                .collect()
        };
        for line in frames {
            let _ = self.outbox.send(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn topic_constants_match_the_v4_protocol() {
        assert_eq!(TOPIC_TASKS, "controller/tasks-index");
        assert_eq!(TOPIC_WORKSPACES, "controller/workspaces");
    }
    #[test]
    fn task_row_reexport_matches() {
        let view = json!({ "sessionId": "s", "workspacePath": "/ws", "createdAt": 1, "updatedAt": 2, "phase": "idle", "mode": "build" });
        let row = task_row_for_view(&view, "/ws", None);
        assert_eq!(row["meta"]["taskId"], json!("s"));
    }
}
