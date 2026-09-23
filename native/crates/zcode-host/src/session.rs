//! Session：一个会话的权威投影态 + 串行 admission + 订阅者帧管线。
//!
//! 所有权（specs/rust-native-crates.md S3 与本 increment 的验收）：session 是行、
//! revision、logEpoch、phase 与命令 inbox 的唯一所有者；turn 只通过本模块的
//! 方法变更状态并发射帧；stdio 连接只携带订阅者身份，不持有会话事实。
//!
//! 事件顺序保证：所有投影变更先追加进 session 行集合并落盘，再进入各订阅者的
//! flush 管线；同一订阅者内帧序严格递增；回放缓冲与在线帧走同一条编码路径。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use zcode_core::inbox::{CommandInbox, SharedCommandInbox};
use zcode_core::protocol::{CommandAck, CommandInboxHost, CommandKey};

use crate::frame::{self, TopicFrameInput, DELIVERY_KIND_ONLINE};
use crate::profile::{
    coalesce_deltas, filter_deltas_for_profile, DeliveryProfile, DeliveryProfileName, ReplayBuffer,
};
use crate::rows::{self, RowBase};
use crate::snapshot::{self, SnapshotInput};
use crate::store::{now_ms, TranscriptStore};

/// 订阅者缓冲区上限：镜像 `PROTOCOL_V4_LIMITS.subscriberBufferMaxOps/Bytes`。
const SUBSCRIBER_BUFFER_MAX_OPS: usize = 500;
const SUBSCRIBER_BUFFER_MAX_BYTES: usize = 1024 * 1024;

/// inbox 回调面：session 的 revision/epoch/transcript 回源。
pub struct SessionHost {
    pub session_id: String,
    pub revision: AtomicI64,
    pub log_epoch: String,
    transcript_acks: Mutex<HashMap<String, Value>>,
}

impl CommandInboxHost for SessionHost {
    fn get_revision(&self, _session_id: &str) -> Option<i64> {
        Some(self.revision.load(Ordering::SeqCst))
    }

    fn get_log_epoch(&self, _session_id: &str) -> Option<String> {
        Some(self.log_epoch.clone())
    }

    /// transcript 回源：transcript → timeline → child → discarded 优先级中的第一级。
    /// 终态 ack 在 session 落盘（幂等重提的权威事实）。
    fn lookup_transcript_command(&self, key: &CommandKey) -> Option<CommandAck> {
        let guard = self.transcript_acks.lock().ok()?;
        let raw = guard.get(&key.command_id)?;
        serde_json::from_value(raw.clone()).ok()
    }

    fn now_ms(&self) -> i64 {
        now_ms()
    }
}

struct Subscriber {
    id: String,
    profile: DeliveryProfile,
    last_to_seq: u64,
    pending: Vec<Value>,
    last_flush_at_ms: i128,
    replay: ReplayBuffer,
}

pub struct Session {
    pub id: String,
    pub workspace_root: PathBuf,
    pub workspace_identity: String,
    title: Mutex<(String, String)>,
    pub rows: Mutex<Vec<Value>>,
    next_row_id: AtomicU64,
    /// 投影记账：每个 op 递增；帧的 (fromSeq, toSeq] 区间以它为准。
    op_seq: AtomicU64,
    logical_frame_ordinal: AtomicU64,
    pub phase: Mutex<String>,
    pub last_error: Mutex<Option<Value>>,
    /// turn 取消探针：工具层与 provider executor 共用同一个开关。
    pub cancel: Arc<AtomicBool>,
    queue_auto_drain: AtomicBool,
    subscribers: Mutex<Vec<Subscriber>>,
    /// revision / logEpoch / transcript 回源的唯一所有者是 inbox 的 host
    /// （`CommandInboxHost`）；本结构不持有第二份 revision/epoch。
    pub inbox: SharedCommandInbox<SessionHost>,
    /// read-before-edit 状态：会话级（跨 turn 保持），工具层读写它。
    pub read_state: Arc<Mutex<HashSet<PathBuf>>>,
    store: TranscriptStore,
    usage: Mutex<Value>,
}

/// 构造参数集中到一个结构（调用点两个：create/resume）。
pub struct SessionInit {
    pub id: String,
    pub workspace_root: PathBuf,
    pub workspace_identity: String,
    pub log_epoch: String,
    pub title: String,
    pub title_source: String,
    pub rows: Vec<Value>,
    pub revision: i64,
    pub transcript_acks: HashMap<String, Value>,
    pub next_row_id: u64,
    pub store: TranscriptStore,
}

impl Session {
    pub fn new(init: SessionInit) -> Arc<Self> {
        let max_row_id = init
            .rows
            .iter()
            .filter_map(|row| row.get("rowId").and_then(Value::as_u64))
            .max()
            .unwrap_or(0);
        let host = SessionHost {
            session_id: init.id.clone(),
            revision: AtomicI64::new(init.revision),
            log_epoch: init.log_epoch,
            transcript_acks: Mutex::new(init.transcript_acks),
        };
        Arc::new(Self {
            id: init.id,
            workspace_root: init.workspace_root,
            workspace_identity: init.workspace_identity,
            title: Mutex::new((init.title, init.title_source)),
            rows: Mutex::new(init.rows),
            next_row_id: AtomicU64::new(init.next_row_id.max(max_row_id)),
            op_seq: AtomicU64::new(max_row_id),
            logical_frame_ordinal: AtomicU64::new(1),
            phase: Mutex::new("draft".to_string()),
            last_error: Mutex::new(None),
            cancel: Arc::new(AtomicBool::new(false)),
            queue_auto_drain: AtomicBool::new(true),
            subscribers: Mutex::new(Vec::new()),
            inbox: SharedCommandInbox::new(CommandInbox::new(host)),
            read_state: Arc::new(Mutex::new(HashSet::new())),
            store: init.store,
            usage: Mutex::new(snapshot::empty_usage()),
        })
    }

    pub fn log_epoch(&self) -> String {
        self.inbox.host().log_epoch.clone()
    }

    pub fn revision(&self) -> i64 {
        self.inbox.host().revision.load(Ordering::SeqCst)
    }

    pub fn set_revision(&self, revision: i64) {
        self.inbox.host().revision.store(revision, Ordering::SeqCst);
    }

    pub fn phase(&self) -> String {
        self.phase
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    pub fn set_phase(&self, phase: &str) {
        if let Ok(mut guard) = self.phase.lock() {
            *guard = phase.to_string();
        }
    }

    pub fn title(&self) -> (String, String) {
        self.title
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    pub fn set_title(&self, title: String, source: &str) {
        if let Ok(mut guard) = self.title.lock() {
            *guard = (title, source.to_string());
        }
    }

    pub fn usage(&self) -> Value {
        self.usage
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    pub fn set_usage(&self, usage: Value) {
        if let Ok(mut guard) = self.usage.lock() {
            *guard = usage;
        }
    }

    pub fn last_error(&self) -> Option<Value> {
        self.last_error.lock().ok().and_then(|guard| guard.clone())
    }

    pub fn set_last_error(&self, error: Option<Value>) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = error;
        }
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn take_cancel(&self) -> bool {
        self.cancel.swap(false, Ordering::SeqCst)
    }

    fn allocate_row_id(&self) -> u64 {
        self.next_row_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn next_op_seq(&self) -> u64 {
        self.op_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn op_seq(&self) -> u64 {
        self.op_seq.load(Ordering::SeqCst)
    }

    fn row_base(&self, turn_id: &str) -> RowBase {
        let row_id = self.allocate_row_id();
        let created_at = now_ms();
        RowBase {
            row_id,
            turn_id: turn_id.to_string(),
            created_at,
            created_at_seq: self.next_op_seq(),
        }
    }

    /// 追加一行并返回其 rowId；落盘优先于投影。
    pub fn append_row(&self, row: Value) -> u64 {
        let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
        if let Ok(mut guard) = self.rows.lock() {
            guard.push(row.clone());
        }
        let _ = self.store.append_row(&self.id, &row);
        row_id
    }

    /// 就地更新一行（结构变化，整行替换）；transcript 记 Upsert，重放不重复。
    pub fn upsert_row(&self, row: Value) -> u64 {
        let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
        if let Ok(mut guard) = self.rows.lock() {
            if let Some(slot) = guard
                .iter_mut()
                .find(|candidate| candidate.get("rowId").and_then(Value::as_u64) == Some(row_id))
            {
                *slot = row.clone();
            }
        }
        let _ = self.store.append_upsert(&self.id, &row);
        row_id
    }

    pub fn rows_snapshot(&self) -> Vec<Value> {
        self.rows
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// 窗口投影：尾部 60 行（`PROTOCOL_V4_LIMITS.snapshotTailWindowRows` 镜像）。
    pub fn rows_window(&self) -> Vec<Value> {
        const TAIL_WINDOW_ROWS: usize = 60;
        let rows = self.rows_snapshot();
        if rows.len() > TAIL_WINDOW_ROWS {
            rows[rows.len() - TAIL_WINDOW_ROWS..].to_vec()
        } else {
            rows
        }
    }

    pub fn total_row_count(&self) -> usize {
        self.rows.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    pub fn first_row_id(&self) -> Option<u64> {
        self.rows_snapshot()
            .first()
            .and_then(|row| row.get("rowId").and_then(Value::as_u64))
    }

    pub fn persist_ack(&self, command_id: &str, ack: &Value) {
        let _ = self.store.append_ack(&self.id, command_id, ack);
        if let Ok(mut guard) = self.inbox.host().transcript_acks.lock() {
            guard.insert(command_id.to_string(), ack.clone());
        }
    }

    // ── 订阅者与帧发射 ──

    pub fn add_subscriber(&self, id: String, profile: DeliveryProfileName) {
        let subscriber = Subscriber {
            id,
            profile: DeliveryProfile::named(profile),
            last_to_seq: self.op_seq(),
            pending: Vec::new(),
            last_flush_at_ms: now_ms() as i128,
            replay: ReplayBuffer::new(SUBSCRIBER_BUFFER_MAX_OPS, SUBSCRIBER_BUFFER_MAX_BYTES),
        };
        if let Ok(mut guard) = self.subscribers.lock() {
            guard.push(subscriber);
        }
    }

    pub fn remove_subscriber(&self, id: &str) {
        if let Ok(mut guard) = self.subscribers.lock() {
            guard.retain(|subscriber| subscriber.id != id);
        }
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers
            .lock()
            .map(|guard| guard.len())
            .unwrap_or(0)
    }

    /// 投影一批 delta：入各订阅者待发队列；到期或 force 时打帧。
    /// 返回需要写入 stdout 的完整 NDJSON 行（每订阅者一至多帧）。
    pub fn project_deltas(&self, deltas: Vec<Value>, force: bool) -> Vec<String> {
        let now = now_ms() as i128;
        let mut out = Vec::new();
        let mut guard = match self.subscribers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for subscriber in guard.iter_mut() {
            let filtered = filter_deltas_for_profile(&deltas, subscriber.profile);
            if !filtered.is_empty() {
                subscriber.pending.extend(filtered);
            }
            let window_elapsed =
                now - subscriber.last_flush_at_ms >= subscriber.profile.flush_window_ms as i128;
            if !force && !window_elapsed {
                continue;
            }
            if let Some(line) = self.flush_subscriber(subscriber, now) {
                out.push(line);
            }
        }
        out
    }

    /// 定时 flush：flushWindowMs 到期后把滞留 delta 发出。turn 结束后的尾帧与
    /// 静默期的窗口推进都靠它，不依赖新事件到达（TS 侧同为由定时器驱动）。
    pub fn flush_expired(&self) -> Vec<String> {
        let now = now_ms() as i128;
        let mut out = Vec::new();
        let mut guard = match self.subscribers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for subscriber in guard.iter_mut() {
            if subscriber.pending.is_empty() {
                continue;
            }
            if now - subscriber.last_flush_at_ms < subscriber.profile.flush_window_ms as i128 {
                continue;
            }
            if let Some(line) = self.flush_subscriber(subscriber, now) {
                out.push(line);
            }
        }
        out
    }

    fn flush_subscriber(&self, subscriber: &mut Subscriber, now: i128) -> Option<String> {
        if subscriber.pending.is_empty() {
            subscriber.last_flush_at_ms = now;
            return None;
        }
        let to_seq = self.op_seq();
        let line = self.encode_frame(
            subscriber,
            json!({"kind": "deltas", "deltas": coalesce_deltas(&subscriber.pending)}),
            DELIVERY_KIND_ONLINE,
            to_seq,
        );
        subscriber.pending.clear();
        subscriber.last_to_seq = to_seq;
        subscriber.last_flush_at_ms = now;
        line.inspect(|line| subscriber.replay.push(line.clone()))
    }

    /// 全量 snapshot 帧（initial / recovery）。
    pub fn project_snapshot(&self, subscriber_id: &str, delivery_kind: &str) -> Vec<String> {
        let (title, title_source) = self.title();
        let usage = self.usage();
        let rows = self.rows_window();
        let snapshot = snapshot::conversation_snapshot(&SnapshotInput {
            session_id: &self.id,
            log_epoch: &self.log_epoch(),
            seq: self.op_seq(),
            revision: self.revision(),
            phase: &self.phase(),
            can_stop: self.is_running(),
            last_error: self.last_error().as_ref(),
            title: &title,
            title_source: &title_source,
            rows: &rows,
            total_row_count: self.total_row_count(),
            first_row_id: self.first_row_id(),
            usage: &usage,
            queue_auto_drain: self.queue_auto_drain.load(Ordering::SeqCst),
        });
        let payload = json!({"kind": "snapshot", "snapshot": snapshot});
        let mut guard = match self.subscribers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut out = Vec::new();
        if let Some(subscriber) = guard
            .iter_mut()
            .find(|subscriber| subscriber.id == subscriber_id)
        {
            let to_seq = self.op_seq();
            if let Some(line) = self.encode_frame(subscriber, payload, delivery_kind, to_seq) {
                subscriber.replay.push(line.clone());
                out.push(line);
            }
        }
        out
    }

    fn encode_frame(
        &self,
        subscriber: &Subscriber,
        payload: Value,
        delivery_kind: &str,
        to_seq: u64,
    ) -> Option<String> {
        let topic = format!("conversation/{}", self.id);
        let ordinal = self.logical_frame_ordinal.fetch_add(1, Ordering::SeqCst);
        let input = TopicFrameInput {
            topic: &topic,
            subscription_id: &subscriber.id,
            delivery_kind,
            from_seq: subscriber.last_to_seq,
            to_seq,
            sent_at: now_ms(),
            payload: &payload,
            logical_frame_id: format!("{}-{ordinal}", self.id),
            logical_frame_ordinal: ordinal,
        };
        match frame::encode_frame_line(&input) {
            Ok(line) => Some(line),
            Err(error) => {
                // 信封超限是 typed error：投影侧记录并可观测，绝不静默丢帧后假装成功。
                if let Ok(mut guard) = self.last_error.lock() {
                    *guard = Some(snapshot::session_error_info(
                        "fault.frame.envelopeTooLarge",
                        &error.to_string(),
                        true,
                        "runtime",
                        now_ms(),
                    ));
                }
                None
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.phase() == "running"
    }

    /// rowsRange：按 [fromRowId, toRowId] 读行（只读查询，无状态）。
    pub fn rows_range(
        &self,
        from_row_id: Option<u64>,
        to_row_id: Option<u64>,
        limit: Option<usize>,
    ) -> Vec<Value> {
        const DEFAULT_LIMIT: usize = 60;
        let rows = self.rows_snapshot();
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(200);
        rows.into_iter()
            .filter(|row| {
                let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
                from_row_id.is_none_or(|from| row_id >= from)
                    && to_row_id.is_none_or(|to| row_id <= to)
            })
            .take(limit)
            .collect()
    }

    // ── turn 投影助手（turn.rs 调用）──

    pub fn project_turn_started(&self, turn_id: &str, source_command_id: &str) -> Vec<String> {
        let base = self.row_base(turn_id);
        let header = rows::turn_header_row(
            &base,
            "userInput",
            "running",
            base.created_at,
            source_command_id,
        );
        self.append_row(header.clone());
        self.project_deltas(vec![rows::row_appended(header)], false)
    }

    pub fn project_turn_finished(
        &self,
        turn_id: &str,
        source_command_id: &str,
        state: &str,
    ) -> Vec<String> {
        let started_at = self
            .rows_snapshot()
            .iter()
            .find(|row| {
                row.get("kind").and_then(Value::as_str) == Some("turnHeader")
                    && row.get("sourceCommandId").and_then(Value::as_str) == Some(source_command_id)
            })
            .and_then(|row| row.get("rowId").and_then(Value::as_u64))
            .unwrap_or(0);
        let base = RowBase {
            row_id: started_at,
            turn_id: turn_id.to_string(),
            created_at: now_ms(),
            created_at_seq: self.next_op_seq(),
        };
        let header = rows::turn_header_row(
            &base,
            "userInput",
            state,
            base.created_at,
            source_command_id,
        );
        self.upsert_row(header.clone());
        self.project_deltas(vec![rows::row_upserted(header)], false)
    }

    pub fn project_user_input(
        &self,
        turn_id: &str,
        text: &str,
        source_command_id: &str,
        client_id: &str,
    ) -> Vec<String> {
        let base = self.row_base(turn_id);
        let row = rows::user_input_row(&base, text, source_command_id, client_id);
        self.append_row(row.clone());
        self.project_deltas(vec![rows::row_appended(row)], false)
    }

    /// assistant 文本行：创建（streaming）→ delta 追加 → 终态 upsert。
    pub fn project_assistant_row(
        &self,
        turn_id: &str,
        response_id: &str,
        model: Option<&str>,
    ) -> u64 {
        let base = self.row_base(turn_id);
        let row = rows::assistant_text_row(&base, response_id, "", "streaming", model);
        let row_id = base.row_id;
        self.append_row(row.clone());
        self.project_deltas(vec![rows::row_appended(row)], false);
        row_id
    }

    pub fn project_assistant_delta(&self, row_id: u64, append: &str) -> Vec<String> {
        self.project_deltas(vec![rows::row_delta(row_id, "text", append)], false)
    }

    /// toolCall 行：running → 终态（success/error/cancelled）两次投影；
    /// `inputText` 是流式路径，continuous 档随窗口 flush，终态 upsert 携全量。
    pub fn project_tool_call(
        &self,
        turn_id: &str,
        tool_call_id: &str,
        tool_name: &str,
        status: &str,
        input_text: &str,
        error: Option<(&str, &str)>,
    ) -> Vec<String> {
        let existing_row_id = self
            .rows_snapshot()
            .iter()
            .find(|row| {
                row.get("kind").and_then(Value::as_str) == Some("toolCall")
                    && row.get("toolCallId").and_then(Value::as_str) == Some(tool_call_id)
            })
            .and_then(|row| row.get("rowId").and_then(Value::as_u64));
        let base = match existing_row_id {
            Some(row_id) => RowBase {
                row_id,
                turn_id: turn_id.to_string(),
                created_at: now_ms(),
                created_at_seq: self.next_op_seq(),
            },
            None => self.row_base(turn_id),
        };
        let row = rows::tool_call_row(&base, tool_call_id, tool_name, status, input_text, error);
        if existing_row_id.is_some() {
            self.upsert_row(row.clone());
            self.project_deltas(vec![rows::row_upserted(row)], false)
        } else {
            self.append_row(row.clone());
            self.project_deltas(vec![rows::row_appended(row)], false)
        }
    }

    pub fn project_assistant_final(
        &self,
        row_id: u64,
        response_id: &str,
        text: &str,
        state: &str,
        model: Option<&str>,
    ) -> Vec<String> {
        let turn_id = self
            .rows_snapshot()
            .iter()
            .find(|row| row.get("rowId").and_then(Value::as_u64) == Some(row_id))
            .and_then(|row| {
                row.get("turnId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let base = RowBase {
            row_id,
            turn_id,
            created_at: now_ms(),
            created_at_seq: self.next_op_seq(),
        };
        let row = rows::assistant_text_row(&base, response_id, text, state, model);
        self.upsert_row(row.clone());
        self.project_deltas(vec![rows::row_upserted(row)], true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TranscriptStore;

    fn test_session(id: &str) -> Arc<Session> {
        let store =
            TranscriptStore::new(std::env::temp_dir().join(format!("zcode-host-test-{id}")));
        let _ = std::fs::remove_dir_all(store.session_dir(id));
        Session::new(SessionInit {
            id: id.to_string(),
            workspace_root: std::env::temp_dir().join(format!("ws-{id}")),
            workspace_identity: format!("/ws/{id}"),
            log_epoch: "epoch-1".to_string(),
            title: String::new(),
            title_source: "default".to_string(),
            rows: Vec::new(),
            revision: 0,
            transcript_acks: HashMap::new(),
            next_row_id: 0,
            store,
        })
    }

    #[test]
    fn rows_assigned_to_no_subscriber_are_buffered_not_emitted() {
        let session = test_session("monotonic");
        let lines = session.project_user_input("t1", "a", "cmd-1", "c1");
        assert!(
            lines.is_empty(),
            "no subscriber: projection is buffered, not emitted"
        );
        let rows = session.rows_snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["rowId"], json!(1));
    }

    #[test]
    fn assistant_stream_then_final_is_idempotent() {
        let session = test_session("stream");
        session.project_user_input("t1", "hi", "cmd-1", "c1");
        let row_id = session.project_assistant_row("t1", "resp-1", Some("m"));
        session.project_assistant_delta(row_id, "he");
        session.project_assistant_delta(row_id, "llo");
        session.project_assistant_final(row_id, "resp-1", "hello", "complete", Some("m"));
        let rows = session.rows_snapshot();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["text"], json!("hello"));
        assert_eq!(rows[1]["state"], json!("complete"));
    }

    #[test]
    fn rows_range_is_bounded() {
        let session = test_session("range");
        for index in 0..5 {
            session.project_user_input("t1", &format!("m{index}"), "cmd", "c1");
        }
        assert_eq!(session.rows_range(Some(2), None, None).len(), 4);
        assert_eq!(session.rows_range(None, Some(2), None).len(), 2);
    }
}
