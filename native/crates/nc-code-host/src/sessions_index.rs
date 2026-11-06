//! The sessions-index topic: the live data source of the sidebar task list.
//!
//! The shape matches `packages/shared/src/nc-code-protocol-v4/sessions-index.ts`
//! (`SessionsIndexSnapshot` / `SessionsIndexDelta`) and
//! the seq accounting discipline of `apps/nc-code-cli/.../sessions-index-publisher.ts`:
//!
//! - one projection per workspace (`sessionId → SessionSummary`), conflation key = sessionId;
//! - one subscription per connection (a re-subscription replaces the old generation), with `(logEpoch, seq]` increments and bounded replay;
//! - a gap / forced recovery degrades to a snapshot (equivalent under conflated semantics);
//! - frames are encoded into `v4/conversation/frame` notifications via `crate::frame::encode_frame_line`.
//!
//! Difference from TS: TS has the gateway push deltas while ingesting a session snapshot; this host uses an in-process
//! periodic diff (`HostServer`'s index ticker) to derive summaries from `Session`'s authoritative state.
//! Both are "latest-state reduction + conflated delta"; they differ only in the trigger point and do not change client semantics.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::frame::{encode_frame_line, TopicFrameInput};
use crate::rpc::{RpcError, CODE_INVALID_PARAMS};

pub const SESSIONS_INDEX_PREFIX: &str = "sessions-index/";
const MAX_DELTA_LOG: usize = 512;
const MAX_PREVIEW_CHARS: usize = 120;

pub fn topic_for(workspace_id: &str) -> String {
    format!("{SESSIONS_INDEX_PREFIX}{workspace_id}")
}

struct Subscription {
    connection_id: String,
    /// fromSeq of the next frame (the (fromSeq, toSeq] semantics).
    sent_seq: u64,
    next_ordinal: u64,
}

pub struct SessionsIndex {
    workspace_id: String,
    log_epoch: String,
    seq: u64,
    summaries: BTreeMap<String, Value>,
    delta_log: VecDeque<(u64, Value)>,
    subscriptions: HashMap<String, Subscription>,
    subscription_by_connection: HashMap<String, String>,
    next_subscription_serial: u64,
    next_frame_serial: u64,
}

impl SessionsIndex {
    pub fn new(workspace_id: String) -> Self {
        let serial = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        Self {
            workspace_id,
            log_epoch: format!("six-{serial}"),
            seq: 0,
            summaries: BTreeMap::new(),
            delta_log: VecDeque::new(),
            subscriptions: HashMap::new(),
            subscription_by_connection: HashMap::new(),
            next_subscription_serial: 1,
            next_frame_serial: 1,
        }
    }

    fn topic(&self) -> String {
        topic_for(&self.workspace_id)
    }

    fn snapshot(&self) -> Value {
        json!({
            "protocolVersion": 1,
            "workspaceId": self.workspace_id,
            "logEpoch": self.log_epoch,
            "sessions": self.summaries.values().cloned().collect::<Vec<_>>(),
        })
    }

    /// Subscribe: resume when base is valid, otherwise a snapshot. One subscription per connection (replaces the old generation).
    /// Returns `(ack, post_frame_lines)`.
    pub fn subscribe(
        &mut self,
        connection_id: &str,
        base: Option<(&str, u64)>,
    ) -> Result<(Value, Vec<String>), RpcError> {
        if let Some(previous) = self.subscription_by_connection.remove(connection_id) {
            self.subscriptions.remove(&previous);
        }
        let serial = self.next_subscription_serial;
        self.next_subscription_serial += 1;
        let subscription_id = format!("six-{}-{serial}", self.log_epoch);
        self.subscriptions.insert(
            subscription_id.clone(),
            Subscription {
                connection_id: connection_id.to_string(),
                sent_seq: 0,
                next_ordinal: 1,
            },
        );
        self.subscription_by_connection
            .insert(connection_id.to_string(), subscription_id.clone());
        let can_resume = base
            .map(|(epoch, seq)| epoch == self.log_epoch && self.can_resume_from(seq))
            .unwrap_or(false);
        let mut frames = Vec::new();
        let mode;
        if can_resume {
            mode = "resume";
            let seq = base.expect("base checked").1;
            if let Some(subscription) = self.subscriptions.get_mut(&subscription_id) {
                subscription.sent_seq = seq;
            }
            if seq < self.seq {
                self.emit_delta_frame(&subscription_id, "initial", &mut frames);
            }
        } else {
            mode = "snapshot";
            self.emit_snapshot_frame(&subscription_id, "initial", &mut frames);
        }
        Ok((
            json!({
                "ack": {
                    "subscriptionId": subscription_id,
                    "mode": mode,
                    "logEpoch": self.log_epoch,
                }
            }),
            frames,
        ))
    }

    /// same-sub recovery: rebuilt from the client's base; an unknown subscription returns invalid params (a client may only
    /// recover a subscription it got through an ACK, it cannot invent someone else's subscriptionId out of thin air).
    pub fn resync(
        &mut self,
        subscription_id: &str,
        base: Option<(&str, u64)>,
        force_snapshot: bool,
    ) -> Result<(Value, Vec<String>), RpcError> {
        if !self.subscriptions.contains_key(subscription_id) {
            return Err(RpcError::new(
                CODE_INVALID_PARAMS,
                "unknown sessions-index subscriptionId",
            ));
        }
        let can_resume = !force_snapshot
            && base
                .map(|(epoch, seq)| epoch == self.log_epoch && self.can_resume_from(seq))
                .unwrap_or(false);
        let mut frames = Vec::new();
        let mode;
        if can_resume {
            mode = "resume";
            let seq = base.expect("base checked").1;
            if let Some(subscription) = self.subscriptions.get_mut(subscription_id) {
                subscription.sent_seq = seq;
            }
            if seq == self.seq {
                // Watermarks agree: send one empty delta frame so the client can confirm recovery is complete.
                self.emit_empty_delta_frame(subscription_id, "recovery", &mut frames);
            } else {
                self.emit_delta_frame(subscription_id, "recovery", &mut frames);
            }
        } else {
            mode = "snapshot";
            if let Some(subscription) = self.subscriptions.get_mut(subscription_id) {
                subscription.sent_seq = 0;
            }
            self.emit_snapshot_frame(subscription_id, "recovery", &mut frames);
        }
        Ok((
            json!({
                "ack": {
                    "subscriptionId": subscription_id,
                    "mode": mode,
                    "logEpoch": self.log_epoch,
                }
            }),
            frames,
        ))
    }

    pub fn unsubscribe(&mut self, subscription_id: &str, connection_id: &str) {
        let Some(subscription) = self.subscriptions.get(subscription_id) else {
            return;
        };
        if subscription.connection_id != connection_id {
            return;
        }
        self.subscriptions.remove(subscription_id);
        if self
            .subscription_by_connection
            .get(connection_id)
            .map(|current| current == subscription_id)
            .unwrap_or(false)
        {
            self.subscription_by_connection.remove(connection_id);
        }
    }

    fn can_resume_from(&self, seq: u64) -> bool {
        if seq > self.seq {
            return false;
        }
        if seq == self.seq {
            return true;
        }
        self.delta_log
            .iter()
            .find(|(entry, _)| *entry > seq)
            .map(|(entry, _)| *entry == seq + 1)
            .unwrap_or(false)
    }

    fn encode(
        &mut self,
        subscription_id: &str,
        delivery_kind: &str,
        from_seq: u64,
        to_seq: u64,
        payload: Value,
    ) -> Option<String> {
        let (ordinal, topic) = {
            let subscription = self.subscriptions.get_mut(subscription_id)?;
            let ordinal = subscription.next_ordinal;
            subscription.next_ordinal += 1;
            (ordinal, self.topic())
        };
        let frame_id = format!("six-{}-lf-{}", self.log_epoch, self.next_frame_serial);
        self.next_frame_serial += 1;
        let sent_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        match encode_frame_line(&TopicFrameInput {
            topic: &topic,
            subscription_id,
            delivery_kind,
            from_seq,
            to_seq,
            sent_at,
            payload: &payload,
            logical_frame_id: frame_id,
            logical_frame_ordinal: ordinal,
        }) {
            Ok(line) => Some(line),
            Err(error) => {
                // The envelope gate refused the emission (over the limit). Record it honestly; never swallow a frame silently.
                eprintln!("[nc-code-host] sessions-index frame rejected: {error}");
                None
            }
        }
    }

    fn emit_snapshot_frame(
        &mut self,
        subscription_id: &str,
        delivery_kind: &str,
        out: &mut Vec<String>,
    ) {
        let to_seq = self.seq;
        let payload = json!({"kind": "snapshot", "snapshot": self.snapshot()});
        if let Some(line) = self.encode(subscription_id, delivery_kind, 0, to_seq, payload) {
            if let Some(subscription) = self.subscriptions.get_mut(subscription_id) {
                subscription.sent_seq = to_seq;
            }
            out.push(line);
        }
    }

    fn emit_empty_delta_frame(
        &mut self,
        subscription_id: &str,
        delivery_kind: &str,
        out: &mut Vec<String>,
    ) {
        let seq = self.seq;
        let payload = json!({"kind": "deltas", "deltas": []});
        if let Some(line) = self.encode(subscription_id, delivery_kind, seq, seq, payload) {
            out.push(line);
        }
    }

    fn emit_delta_frame(
        &mut self,
        subscription_id: &str,
        delivery_kind: &str,
        out: &mut Vec<String>,
    ) {
        let sent_seq = match self.subscriptions.get(subscription_id) {
            Some(subscription) => subscription.sent_seq,
            None => return,
        };
        let pending: Vec<&(u64, Value)> = self
            .delta_log
            .iter()
            .filter(|(seq, _)| *seq > sent_seq)
            .collect();
        if pending.is_empty() {
            return;
        }
        let contiguous = pending[0].0 == sent_seq + 1;
        if !contiguous {
            // The replay buffer has a gap: under conflated semantics a snapshot and a resume are equivalent.
            self.emit_snapshot_frame(subscription_id, delivery_kind, out);
            return;
        }
        let deltas: Vec<Value> = pending.iter().map(|(_, delta)| delta.clone()).collect();
        let to_seq = self.seq;
        let payload = json!({"kind": "deltas", "deltas": deltas});
        if let Some(line) = self.encode(subscription_id, delivery_kind, sent_seq, to_seq, payload) {
            if let Some(subscription) = self.subscriptions.get_mut(subscription_id) {
                subscription.sent_seq = to_seq;
            }
            out.push(line);
        }
    }

    /// Refresh the projection with the sessions' current authoritative state; changed sessions produce an upsert delta, vanished sessions produce a remove.
    /// Returns the frame lines for all subscribers (HostServer writes them into the outbox).
    ///
    /// The caller decides whether `summaries`' `lastActivityAt` moves forward based on "did any other field change":
    /// comparison here ignores that field first and only stamps the incoming value after a real change, so no idle frames are emitted.
    pub fn refresh(&mut self, summaries: Vec<(String, Value, i64)>) -> Vec<String> {
        let mut deltas: Vec<(u64, Value)> = Vec::new();
        let mut seen = BTreeMap::new();
        for (session_id, summary, activity_at) in summaries {
            seen.insert(session_id.clone(), ());
            let next = if let Some(previous) = self.summaries.get(&session_id) {
                let mut candidate = summary;
                if same_summary(previous, &candidate) {
                    continue;
                }
                candidate["lastActivityAt"] = json!(activity_at);
                candidate
            } else {
                let mut candidate = summary;
                candidate["lastActivityAt"] = json!(activity_at);
                candidate
            };
            self.summaries.insert(session_id.clone(), next.clone());
            self.seq += 1;
            let delta = json!({"op": "session.upserted", "session": next});
            self.delta_log.push_back((self.seq, delta.clone()));
            deltas.push((self.seq, delta));
        }
        let removed: Vec<String> = self
            .summaries
            .keys()
            .filter(|session_id| !seen.contains_key(*session_id))
            .cloned()
            .collect();
        for session_id in removed {
            self.summaries.remove(&session_id);
            self.seq += 1;
            let delta = json!({"op": "session.removed", "sessionId": session_id});
            self.delta_log.push_back((self.seq, delta.clone()));
            deltas.push((self.seq, delta));
        }
        while self.delta_log.len() > MAX_DELTA_LOG {
            self.delta_log.pop_front();
        }
        if deltas.is_empty() {
            return Vec::new();
        }
        let ids: Vec<String> = self.subscriptions.keys().cloned().collect();
        let mut lines = Vec::new();
        for subscription_id in ids {
            self.emit_delta_frame(&subscription_id, "online", &mut lines);
        }
        lines
    }

    /// For tests/diagnostics: does the current projection contain a given session.
    pub fn has_session(&self, session_id: &str) -> bool {
        self.summaries.contains_key(session_id)
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }
}

fn same_summary(left: &Value, right: &Value) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    if let Some(object) = left.as_object_mut() {
        object.remove("lastActivityAt");
    }
    if let Some(object) = right.as_object_mut() {
        object.remove("lastActivityAt");
    }
    // createdAt may transiently be 0 inside the cold-recovery window (while meta has not been read yet); a transient value must not produce a downgrade frame.
    let left_created = left.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
    let right_created = right.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
    if left_created == 0 || right_created == 0 {
        if let Some(object) = left.as_object_mut() {
            object.remove("createdAt");
        }
        if let Some(object) = right.as_object_mut() {
            object.remove("createdAt");
        }
    }
    left == right
}

/// Derive a `SessionSummary` from the session's authoritative state (a pure function, easy to unit-test).
/// The 10 parameters correspond one-by-one to TS's summary-derivation signature; switching to a parameter object would add one more "alignment point" for no gain.
#[allow(clippy::too_many_arguments)]
pub fn derive_summary(
    workspace_id: &str,
    session_id: &str,
    title: &str,
    title_source: &str,
    phase: &str,
    goal_status: Option<&str>,
    last_assistant_preview: Option<String>,
    pending_interactions: &Value,
    created_at: i64,
    parent_session_id: Option<&str>,
) -> Value {
    let mut summary = json!({
        "sessionId": session_id,
        "workspaceId": workspace_id,
        "title": title,
        "titleSource": title_source,
        "phase": phase,
        "sessionEnded": matches!(phase, "completedSuccess" | "completedInterrupted" | "error"),
        "hasBackgroundWork": false,
        "lastActivityAt": created_at,
        "createdAt": created_at,
    });
    if let Some(parent) = parent_session_id {
        summary["parentSessionId"] = json!(parent);
    }
    if let Some(status) = goal_status {
        summary["goalStatus"] = json!(status);
    }
    if let Some(preview) = last_assistant_preview {
        let trimmed = preview.trim();
        if !trimmed.is_empty() {
            let preview: String = trimmed.chars().take(MAX_PREVIEW_CHARS).collect();
            summary["lastAssistantPreview"] = json!(preview);
        }
    }
    if let Some(items) = pending_interactions.as_array() {
        let permission_count = items
            .iter()
            .filter(|item| item.get("kind").and_then(Value::as_str) == Some("permission"))
            .count();
        let user_input_count = items
            .iter()
            .filter(|item| item.get("kind").and_then(Value::as_str) == Some("userInput"))
            .count();
        if let Some(first) = items.iter().find(|item| {
            matches!(
                item.get("kind").and_then(Value::as_str),
                Some("permission") | Some("userInput")
            )
        }) {
            let mut pending = json!({
                "interactionId": first.get("interactionId").cloned().unwrap_or(Value::Null),
                "kind": first.get("kind").cloned().unwrap_or(Value::Null),
            });
            if let Some(tool_name) = first
                .get("payload")
                .and_then(|payload| payload.get("toolName"))
                .and_then(Value::as_str)
            {
                pending["toolName"] = json!(tool_name);
            }
            if let Some(auto) = first.get("autoResolution") {
                pending["autoResolution"] = auto.clone();
            }
            summary["pendingInteraction"] = pending;
        }
        if permission_count > 0 || user_input_count > 0 {
            summary["pendingInteractionSummary"] = json!({
                "permissionCount": permission_count,
                "userInputCount": user_input_count,
            });
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(session_id: &str, title: &str) -> Value {
        derive_summary(
            "/ws",
            session_id,
            title,
            "default",
            "running",
            None,
            None,
            &json!([]),
            10,
            None,
        )
    }

    fn frame_payload(line: &str) -> Value {
        let value: Value = serde_json::from_str(line).expect("frame line");
        assert_eq!(value["method"], json!("v4/conversation/frame"));
        let params = &value["params"];
        assert_eq!(
            params["wireVersion"],
            json!(nc_code_v4_wire::V4_WIRE_PROTOCOL_VERSION)
        );
        assert_eq!(params["kind"], json!("complete"));
        params["frame"].clone()
    }

    #[test]
    fn subscribe_emits_snapshot_then_refresh_emits_deltas() {
        let mut index = SessionsIndex::new("/ws".to_string());
        index.refresh(vec![("s1".to_string(), summary("s1", "One"), 10)]);
        let (ack, frames) = index.subscribe("conn-1", None).expect("subscribe");
        assert_eq!(ack["ack"]["mode"], json!("snapshot"));
        assert_eq!(frames.len(), 1);
        let frame = frame_payload(&frames[0]);
        assert_eq!(frame["topic"], json!("sessions-index//ws"));
        assert_eq!(frame["payload"]["kind"], json!("snapshot"));
        assert_eq!(frame["payload"]["snapshot"]["workspaceId"], json!("/ws"));
        assert_eq!(
            frame["payload"]["snapshot"]["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let lines = index.refresh(vec![
            ("s1".to_string(), summary("s1", "Renamed"), 20),
            ("s2".to_string(), summary("s2", "Two"), 20),
        ]);
        assert_eq!(lines.len(), 1);
        let frame = frame_payload(&lines[0]);
        assert_eq!(frame["payload"]["kind"], json!("deltas"));
        let deltas = frame["payload"]["deltas"].as_array().expect("deltas");
        assert_eq!(deltas.len(), 2);
        assert!(deltas
            .iter()
            .all(|delta| delta["op"] == json!("session.upserted")));

        let lines = index.refresh(vec![
            ("s1".to_string(), summary("s1", "Renamed"), 20),
            ("s2".to_string(), summary("s2", "Two"), 20),
        ]);
        assert!(lines.is_empty(), "unchanged refresh must not emit frames");

        let lines = index.refresh(vec![]);
        let frame = frame_payload(&lines[0]);
        let deltas = frame["payload"]["deltas"].as_array().expect("deltas");
        assert!(deltas
            .iter()
            .all(|delta| delta["op"] == json!("session.removed")));
        assert!(!index.has_session("s2"));
    }

    #[test]
    fn resync_resumes_from_acknowledged_watermark() {
        let mut index = SessionsIndex::new("/ws".to_string());
        index.refresh(vec![("s1".to_string(), summary("s1", "One"), 10)]);
        let (ack, _) = index.subscribe("conn", None).expect("subscribe");
        let subscription_id = ack["ack"]["subscriptionId"].as_str().unwrap().to_string();
        let epoch = index.log_epoch.clone();
        let seq = index.seq();
        let (ack, frames) = index
            .resync(&subscription_id, Some((&epoch, seq)), false)
            .expect("resync");
        assert_eq!(ack["ack"]["mode"], json!("resume"));
        let frame = frame_payload(&frames[0]);
        let deltas = frame["payload"]["deltas"].as_array().expect("deltas");
        assert!(deltas.is_empty());

        let (ack, frames) = index
            .resync(&subscription_id, Some((&epoch, seq)), true)
            .expect("resync force");
        assert_eq!(ack["ack"]["mode"], json!("snapshot"));
        let frame = frame_payload(&frames[0]);
        assert_eq!(frame["payload"]["kind"], json!("snapshot"));
    }

    #[test]
    fn resubscribe_replaces_previous_generation_and_unsubscribe_is_scoped() {
        let mut index = SessionsIndex::new("/ws".to_string());
        let (first, _) = index.subscribe("conn", None).expect("first");
        let first_id = first["ack"]["subscriptionId"].as_str().unwrap().to_string();
        let (second, _) = index.subscribe("conn", None).expect("second");
        let second_id = second["ack"]["subscriptionId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(first_id, second_id);
        index.refresh(vec![("s1".to_string(), summary("s1", "One"), 10)]);
        assert_eq!(index.subscriptions.len(), 1);
        index.unsubscribe(&second_id, "other-conn");
        assert_eq!(index.subscriptions.len(), 1);
        index.unsubscribe(&second_id, "conn");
        assert!(index.subscriptions.is_empty());
    }

    #[test]
    fn cold_summary_created_at_is_not_downgraded_to_zero() {
        let mut summary = summary("s1", "One");
        summary["createdAt"] = json!(0);
        let previous = summary.clone();
        summary["createdAt"] = json!(10);
        assert!(same_summary(&previous, &summary));
    }
}
