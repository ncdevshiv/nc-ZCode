//! 产品输入队列：busy 时的排队事实与 UI 投影（`queueItemSchema` / `queueStateSchema`）。
//!
//! 职责边界（单一所有者）：
//! - 本模块只拥有**排队事实**（顺序、文本、来源命令、admission 序号）与其协议投影；
//! - 不触碰 CommandInbox / turn 执行 / 订阅者 —— 提升（drain）与投影发送由 host 编排；
//! - 因此可独立单测，也可被其它编排层复用（pluggable）。
//!
//! 语义对齐 TS `queue.ts`：
//! - `queueItemId ≡ queue_{sourceCommandId}`（与 `zcode_core::inbox` 同一等式）；
//! - compact 是 typed maintenance intent，**不可编辑**（会把维护意图伪装成普通输入）；
//! - `reorderQueueItem(before=None)` = 移到队尾；未命中 = noop（不报错）；
//! - `deleteQueueItem` 未命中 → 调用方按 `queue.itemMissing` 处理（本模块返回 None）。

use serde_json::{json, Value};
use zcode_core::protocol::CommandEnvelope;

/// 队列项不可变身份 + 可变文本。持有原始 envelope：提升时按原命令事实重新 admission，
/// 不重建字段（禁止 text-only fallback 丢来源/客户端/工具面）。
#[derive(Debug, Clone)]
pub struct QueueItem {
    pub item_id: String,
    pub envelope: CommandEnvelope,
    pub text: String,
    pub client_id: String,
    pub admission_seq: u64,
    pub admitted_at: i64,
    pub tool_disallowlist: Option<Vec<String>>,
}

impl QueueItem {
    pub fn source_command_id(&self) -> &str {
        &self.envelope.command_id
    }

    pub fn kind(&self) -> &str {
        &self.envelope.command_type
    }

    pub fn editable(&self) -> bool {
        self.kind() != "compact"
    }

    /// `queueItemSchema` 投影（strict：字段与 schema 一一对应）。
    pub fn projection(&self, position: usize) -> Value {
        let mut item = json!({
            "sourceCommandId": self.envelope.command_id,
            "queueItemId": self.item_id,
            "clientId": self.client_id,
            "kind": self.kind(),
            "text": self.text,
            "attachments": [],
            "delivery": {"requested": "auto", "admitted": "queue"},
            "order": {"admissionSeq": self.admission_seq, "queuePosition": position},
            "steer": {"state": "notRequested"},
            "dispatch": {"state": "queued"},
            "admittedAt": self.admitted_at,
        });
        if let Some(list) = &self.tool_disallowlist {
            item["toolDisallowlist"] = json!(list);
        }
        item
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// 编辑目标不存在（noop 语义，与 TS warn-and-continue 一致）。
    ItemMissing,
    /// typed maintenance intent 不可编辑。
    NotEditable,
}

impl QueueError {
    /// 协议 reasonCode（gateway noop/rejected 的稳定契约）。
    pub fn reason_code(self) -> &'static str {
        match self {
            Self::ItemMissing => "queue.itemMissing",
            Self::NotEditable => "queue.itemNotEditable",
        }
    }
}

/// 队列本体：保持 FIFO 顺序，所有变更都在锁内完成（由调用方提供同步）。
#[derive(Debug, Default)]
pub struct InputQueue {
    items: Vec<QueueItem>,
    next_admission_seq: u64,
}

impl InputQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn position_of(&self, item_id: &str) -> Option<usize> {
        self.items.iter().position(|item| item.item_id == item_id)
    }

    pub fn get(&self, item_id: &str) -> Option<&QueueItem> {
        self.items.iter().find(|item| item.item_id == item_id)
    }

    pub fn contains_command(&self, command_id: &str) -> bool {
        self.items
            .iter()
            .any(|item| item.source_command_id() == command_id)
    }

    /// 入队：admission_seq 单调递增（首条 = 1），itemId 由 sourceCommandId 派生。
    pub fn enqueue(
        &mut self,
        envelope: CommandEnvelope,
        text: String,
        client_id: String,
        admitted_at: i64,
        tool_disallowlist: Option<Vec<String>>,
    ) -> &QueueItem {
        self.next_admission_seq += 1;
        let item_id = zcode_core::inbox::queue_item_id_for_command(&envelope.command_id);
        self.items.push(QueueItem {
            item_id,
            envelope,
            text,
            client_id,
            admission_seq: self.next_admission_seq,
            admitted_at,
            tool_disallowlist,
        });
        self.items.last().expect("just pushed")
    }

    /// 原地改文本（保位）。compact 不可编辑；未命中 → ItemMissing。
    pub fn edit(&mut self, item_id: &str, new_text: &str) -> Result<(), QueueError> {
        let position = self.position_of(item_id).ok_or(QueueError::ItemMissing)?;
        if !self.items[position].editable() {
            return Err(QueueError::NotEditable);
        }
        let item = &mut self.items[position];
        item.text = new_text.to_string();
        if let Some(object) = item.envelope.payload.as_object_mut() {
            object.insert("text".to_string(), json!(new_text));
        }
        Ok(())
    }

    /// 移到 `before_item_id` 之前；None = 队尾。目标/自身未命中 → false（noop）。
    pub fn reorder(&mut self, item_id: &str, before_item_id: Option<&str>) -> bool {
        let Some(from) = self.position_of(item_id) else {
            return false;
        };
        let item = self.items.remove(from);
        // 目标位置在移除后重新定位（协议按当前队列顺序解释）。
        let to = match before_item_id {
            Some(before) => match self.position_of(before) {
                Some(position) => position,
                None => {
                    // before 不存在：恢复原位并 noop。
                    self.items.insert(from, item);
                    return false;
                }
            },
            None => self.items.len(),
        };
        // admissionSeq 是「进入 admission 的序号」这一不变事实；reorder 只改展示顺序，
        // 不得重写它（否则 order 字段会伪造新的 admission 事实）。
        self.items.insert(to, item);
        true
    }

    /// 删除并返回该项；未命中 → None（调用方按 `queue.itemMissing` 处理）。
    pub fn remove(&mut self, item_id: &str) -> Option<QueueItem> {
        let position = self.position_of(item_id)?;
        Some(self.items.remove(position))
    }

    /// 提升队首（drain 用）：FIFO。
    pub fn pop_front(&mut self) -> Option<QueueItem> {
        if self.items.is_empty() {
            None
        } else {
            Some(self.items.remove(0))
        }
    }

    /// `queueStateSchema` 投影：items（按当前位置）+ autoDrain + pauseReason?。
    pub fn projection(&self, auto_drain: bool, pause_reason: Option<&str>) -> Value {
        let items: Vec<Value> = self
            .items
            .iter()
            .enumerate()
            .map(|(position, item)| item.projection(position))
            .collect();
        let mut state = json!({"items": items, "autoDrain": auto_drain});
        if let Some(reason) = pause_reason {
            state["pauseReason"] = json!(reason);
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(command_id: &str, command_type: &str, text: &str) -> CommandEnvelope {
        CommandEnvelope {
            command_type: command_type.to_string(),
            command_id: command_id.to_string(),
            client_id: "c".to_string(),
            session_id: Some("s".to_string()),
            base_revision: None,
            base_log_epoch: None,
            payload: json!({"text": text}),
            issued_at: 1.0,
            ttft: None,
        }
    }

    fn queued(id: &str, text: &str) -> (CommandEnvelope, String, String, i64, Option<Vec<String>>) {
        (
            envelope(id, "sendText", text),
            text.to_string(),
            "c".to_string(),
            1_700_000_000_000,
            None,
        )
    }

    #[test]
    fn enqueue_assigns_monotonic_seq_and_derived_item_id() {
        let mut queue = InputQueue::new();
        let (env, text, client, at, disallow) = queued("cmd-a", "hello");
        queue.enqueue(env, text, client, at, disallow);
        queue.enqueue(
            envelope("cmd-b", "sendText", "x"),
            "x".into(),
            "c".into(),
            1,
            None,
        );
        let items = queue.items();
        assert_eq!(items[0].item_id, "queue_cmd-a");
        assert_eq!(items[0].admission_seq, 1);
        assert_eq!(items[1].item_id, "queue_cmd-b");
        assert_eq!(items[1].admission_seq, 2);
        assert!(queue.contains_command("cmd-a"));
        assert!(!queue.contains_command("cmd-z"));
    }

    #[test]
    fn projection_matches_queue_item_schema_shape() {
        let mut queue = InputQueue::new();
        let (env, text, client, at, disallow) = queued("cmd-a", "hello");
        queue.enqueue(env, text, client, at, disallow);
        let state = queue.projection(true, None);
        assert_eq!(state["autoDrain"], json!(true));
        assert!(state.get("pauseReason").is_none());
        let item = &state["items"][0];
        for key in [
            "sourceCommandId",
            "queueItemId",
            "clientId",
            "kind",
            "text",
            "attachments",
            "delivery",
            "order",
            "steer",
            "dispatch",
            "admittedAt",
        ] {
            assert!(item.get(key).is_some(), "missing {key}");
        }
        assert_eq!(item["sourceCommandId"], json!("cmd-a"));
        assert_eq!(item["queueItemId"], json!("queue_cmd-a"));
        assert_eq!(item["kind"], json!("sendText"));
        assert_eq!(item["delivery"]["admitted"], json!("queue"));
        assert_eq!(item["order"]["queuePosition"], json!(0));
        assert_eq!(item["order"]["admissionSeq"], json!(1));
        assert_eq!(item["steer"]["state"], json!("notRequested"));
        assert_eq!(item["dispatch"]["state"], json!("queued"));

        let paused = queue.projection(false, Some("stopped"));
        assert_eq!(paused["pauseReason"], json!("stopped"));
    }

    #[test]
    fn edit_updates_text_in_place_and_rejects_compact_and_missing() {
        let mut queue = InputQueue::new();
        let (env, text, client, at, disallow) = queued("cmd-a", "old");
        queue.enqueue(env, text, client, at, disallow);
        queue.enqueue(
            envelope("cmd-m", "compact", ""),
            String::new(),
            "c".into(),
            1,
            None,
        );
        queue.edit("queue_cmd-a", "new").expect("editable");
        assert_eq!(queue.get("queue_cmd-a").unwrap().text, "new");
        assert_eq!(
            queue.get("queue_cmd-a").unwrap().envelope.payload["text"],
            json!("new")
        );
        // 保位：编辑不改变顺序
        assert_eq!(queue.position_of("queue_cmd-a"), Some(0));
        assert_eq!(queue.edit("queue_cmd-m", "x"), Err(QueueError::NotEditable));
        assert_eq!(
            queue.edit("queue_missing", "x"),
            Err(QueueError::ItemMissing)
        );
        assert_eq!(QueueError::ItemMissing.reason_code(), "queue.itemMissing");
        assert_eq!(
            QueueError::NotEditable.reason_code(),
            "queue.itemNotEditable"
        );
    }

    #[test]
    fn reorder_moves_before_target_or_tail_and_noops_on_miss() {
        let mut queue = InputQueue::new();
        for id in ["a", "b", "c"] {
            queue.enqueue(
                envelope(&format!("cmd-{id}"), "sendText", id),
                id.into(),
                "c".into(),
                1,
                None,
            );
        }
        // c → 队首（before a）
        assert!(queue.reorder("queue_cmd-c", Some("queue_cmd-a")));
        let order: Vec<&str> = queue.items().iter().map(|i| i.text.as_str()).collect();
        assert_eq!(order, vec!["c", "a", "b"]);

        // a → 队尾
        assert!(queue.reorder("queue_cmd-a", None));
        let order: Vec<&str> = queue.items().iter().map(|i| i.text.as_str()).collect();
        assert_eq!(order, vec!["c", "b", "a"]);

        // 自身缺失 / before 缺失 → noop 且顺序不变
        assert!(!queue.reorder("queue_missing", None));
        assert!(!queue.reorder("queue_cmd-c", Some("queue_missing")));
        let order: Vec<&str> = queue.items().iter().map(|i| i.text.as_str()).collect();
        assert_eq!(order, vec!["c", "b", "a"]);
    }

    #[test]
    fn remove_and_pop_front_follow_fifo() {
        let mut queue = InputQueue::new();
        for id in ["a", "b", "c"] {
            queue.enqueue(
                envelope(&format!("cmd-{id}"), "sendText", id),
                id.into(),
                "c".into(),
                1,
                None,
            );
        }
        assert_eq!(queue.remove("queue_cmd-b").unwrap().text, "b");
        assert!(queue.remove("queue_cmd-b").is_none());
        assert_eq!(queue.pop_front().unwrap().text, "a");
        assert_eq!(queue.pop_front().unwrap().text, "c");
        assert!(queue.pop_front().is_none());
        assert!(queue.is_empty());
    }
}
