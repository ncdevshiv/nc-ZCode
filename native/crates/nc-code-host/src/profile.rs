//! Delivery profile 参数与 flush 管线。
//!
//! 参数逐字镜像 `packages/shared/src/zcode-protocol-v4/core.ts` 的
//! `DELIVERY_PROFILES`（30ms/150ms 等常量是跨进程契约，改值必须同改 TS 并跑
//! 黄金测试）。过滤/合并是 `profiles.ts` 纯函数的移植：filter → coalesce → 打帧，
//! 恢复回放走同一函数，保证断线重连的确定性重投递。

use serde_json::{json, Value};

/// 与 `streamablePathSchema` 对齐的流式路径。
pub const STREAM_PATH_TEXT: &str = "text";
pub const STREAM_PATH_INPUT_TEXT: &str = "inputText";
pub const STREAM_PATH_OUTPUT_TEXT: &str = "output.text";
pub const STREAM_PATH_SUMMARY_TEXT: &str = "summaryText";

/// `clientMode` 枚举（`v4ConversationSubscribeParamsSchema.clientMode`）。
pub const CLIENT_MODE_CONTINUOUS: &str = "desktop-continuous";
pub const CLIENT_MODE_REPLAYABLE: &str = "web-remote-replayable";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryProfileName {
    Continuous,
    Replayable,
}

impl DeliveryProfileName {
    pub fn from_client_mode(mode: &str) -> Option<Self> {
        match mode {
            CLIENT_MODE_CONTINUOUS => Some(Self::Continuous),
            CLIENT_MODE_REPLAYABLE => Some(Self::Replayable),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryProfile {
    pub desktop_only_rows: bool,
    pub flush_window_ms: u64,
    pub stream_input_text: bool,
    pub stream_output_text: bool,
    pub stream_summary_text: bool,
    pub stream_output_cap_bytes: usize,
    pub tool_progress: bool,
}

/// 常量值镜像 core.ts；两侧不一致时由 RUST-PORT-LOG 的差异表暴露，不允许静默分叉。
pub const PROFILE_CONTINUOUS: DeliveryProfile = DeliveryProfile {
    desktop_only_rows: true,
    flush_window_ms: 30,
    stream_input_text: true,
    stream_output_text: true,
    stream_summary_text: true,
    stream_output_cap_bytes: 262_144,
    tool_progress: false,
};

pub const PROFILE_REPLAYABLE: DeliveryProfile = DeliveryProfile {
    desktop_only_rows: false,
    flush_window_ms: 150,
    stream_input_text: false,
    stream_output_text: false,
    stream_summary_text: false,
    stream_output_cap_bytes: 0,
    tool_progress: true,
};

impl DeliveryProfile {
    pub fn named(name: DeliveryProfileName) -> Self {
        match name {
            DeliveryProfileName::Continuous => PROFILE_CONTINUOUS,
            DeliveryProfileName::Replayable => PROFILE_REPLAYABLE,
        }
    }

    /// `streamPaths` 查询：row.delta 按流式能力过滤。
    pub fn streams_path(&self, path: &str) -> bool {
        match path {
            STREAM_PATH_TEXT => true,
            STREAM_PATH_INPUT_TEXT => self.stream_input_text,
            STREAM_PATH_OUTPUT_TEXT => self.stream_output_text,
            STREAM_PATH_SUMMARY_TEXT => self.stream_summary_text,
            _ => false,
        }
    }
}

/// 按 profile 的 streamPaths 开关过滤 delta 序列（`filterConversationDeltasForProfile`
/// 移植）。纯函数，输出保持权威日志序。
pub fn filter_deltas_for_profile(deltas: &[Value], profile: DeliveryProfile) -> Vec<Value> {
    deltas
        .iter()
        .filter(|delta| {
            if delta.get("op").and_then(Value::as_str) == Some("row.delta") {
                return match delta.get("path").and_then(Value::as_str) {
                    Some(path) => profile.streams_path(path),
                    None => false,
                };
            }
            true
        })
        .cloned()
        .collect()
}

/// 合并一个 flush 窗口内的 delta：同一 (rowId, path) 的 row.delta 拼接 append，
/// 其余 op 原序保留。合并不改变语义（客户端按序 apply 后终态一致）。
/// 与 TS `coalesceConversationDeltas` 同一套规则（语义要求：coalesce 后逐条 apply
/// 的终态必须与未合并一致）：
///   1. **相邻**同 (rowId,path) 的 row.delta → append 拼接；
///   2. **相邻** state.updated → patch 键浅合并（后键覆盖前键）；
///   3. row.upserted 吞掉同 rowId 更早的 row.delta（整行替换蕴含追加），但只回溯
///      到最近的屏障，且不越过同 rowId 的 upserted/appended（那属于上一代行）；
///   4. row.removed 是屏障，任何规则不得跨越。
///
/// 旧实现用 `find` 全局搜索同 (rowId,path)，会把**被 upsert 隔开的**两段追加合并，
/// 客户端 apply 后正文重复（如 "a" + "b" 变成 "aab"）。
pub fn coalesce_deltas(deltas: &[Value]) -> Vec<Value> {
    fn op(delta: &Value) -> Option<&str> {
        delta.get("op").and_then(Value::as_str)
    }
    fn row_id(delta: &Value) -> Option<u64> {
        delta
            .get("row")
            .and_then(|row| row.get("rowId"))
            .and_then(Value::as_u64)
            .or_else(|| delta.get("rowId").and_then(Value::as_u64))
    }
    fn is_barrier(delta: &Value) -> bool {
        op(delta) == Some("row.removed")
    }

    let mut out: Vec<Value> = Vec::with_capacity(deltas.len());
    for delta in deltas {
        // 规则 3：row.upserted 吞掉同 rowId 更早的 row.delta。
        if op(delta) == Some("row.upserted") {
            let target = row_id(delta);
            let mut i = out.len();
            while i > 0 {
                i -= 1;
                let prev = &out[i];
                if is_barrier(prev) {
                    break;
                }
                if op(prev) == Some("row.delta") && row_id(prev) == target {
                    out.remove(i);
                    continue;
                }
                if (op(prev) == Some("row.upserted") || op(prev) == Some("row.appended"))
                    && row_id(prev) == target
                {
                    break;
                }
            }
        }

        // 规则 1：相邻同 (rowId,path) 的 row.delta 拼接。
        let merge_stream = out.last().is_some_and(|last| {
            op(delta) == Some("row.delta")
                && op(last) == Some("row.delta")
                && row_id(last) == row_id(delta)
                && last.get("path") == delta.get("path")
        });
        if merge_stream {
            if let Some(last) = out.last_mut() {
                let merged = format!(
                    "{}{}",
                    last.get("append").and_then(Value::as_str).unwrap_or(""),
                    delta.get("append").and_then(Value::as_str).unwrap_or("")
                );
                if let Some(object) = last.as_object_mut() {
                    object.insert("append".to_string(), json!(merged));
                }
            }
            continue;
        }

        // 规则 2：相邻 state.updated 浅合并。
        let merge_state = out.last().is_some_and(|last| {
            op(delta) == Some("state.updated") && op(last) == Some("state.updated")
        });
        if merge_state {
            if let Some(last) = out.last_mut() {
                let mut merged = last.get("patch").cloned().unwrap_or_else(|| json!({}));
                if let (Some(target), Some(source)) = (
                    merged.as_object_mut(),
                    delta.get("patch").and_then(Value::as_object),
                ) {
                    for (key, value) in source {
                        target.insert(key.clone(), value.clone());
                    }
                }
                *last = json!({"op": "state.updated", "patch": merged});
            }
            continue;
        }

        // 相邻同 rowId 的 row.upserted：留最后一条（整行替换的传递性）。
        let replace_upsert = out.last().is_some_and(|last| {
            op(delta) == Some("row.upserted")
                && op(last) == Some("row.upserted")
                && row_id(last) == row_id(delta)
        });
        if replace_upsert {
            if let Some(last) = out.last_mut() {
                *last = delta.clone();
            }
            continue;
        }

        out.push(delta.clone());
    }
    out
}

/// 有界回放缓冲（`PROTOCOL_V4_LIMITS.subscriberBufferMaxOps/Bytes` 镜像）：
/// resync 之前累积的、尚未被任何订阅者确认的帧，按 ops/bytes 双上限淘汰。
#[derive(Debug)]
pub struct ReplayBuffer {
    max_ops: usize,
    max_bytes: usize,
    frames: std::collections::VecDeque<(String, usize)>,
    bytes: usize,
}

impl ReplayBuffer {
    pub fn new(max_ops: usize, max_bytes: usize) -> Self {
        Self {
            max_ops,
            max_bytes,
            frames: std::collections::VecDeque::new(),
            bytes: 0,
        }
    }

    pub fn push(&mut self, line: String) {
        let size = line.len();
        self.frames.push_back((line, size));
        self.bytes += size;
        while self.frames.len() > self.max_ops
            || (self.bytes > self.max_bytes && self.frames.len() > 1)
        {
            match self.frames.pop_front() {
                Some((_, evicted)) => self.bytes -= evicted,
                None => break,
            }
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = &String> {
        self.frames.iter().map(|(line, _)| line)
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta(row_id: u64, path: &str, append: &str) -> Value {
        json!({"op":"row.delta","rowId":row_id,"path":path,"append":append})
    }

    #[test]
    fn replayable_filters_non_stream_paths_but_keeps_structural_ops() {
        let deltas = vec![
            delta(1, "text", "a"),
            delta(2, "inputText", "b"),
            json!({"op":"row.appended","row":{"kind":"userInput"}}),
        ];
        let filtered = filter_deltas_for_profile(&deltas, PROFILE_REPLAYABLE);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0]["path"], json!("text"));
        assert_eq!(filtered[1]["op"], json!("row.appended"));
    }

    #[test]
    fn continuous_keeps_all_paths() {
        let deltas = vec![
            delta(1, "text", "a"),
            delta(2, "inputText", "b"),
            delta(3, "output.text", "c"),
            delta(4, "summaryText", "d"),
        ];
        assert_eq!(
            filter_deltas_for_profile(&deltas, PROFILE_CONTINUOUS).len(),
            4
        );
    }

    #[test]
    fn coalesce_merges_same_row_path_and_preserves_order() {
        let deltas = vec![
            delta(1, "text", "he"),
            delta(1, "text", "llo"),
            delta(1, "inputText", "x"),
            delta(2, "text", "world"),
        ];
        let merged = coalesce_deltas(&deltas);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0]["append"], json!("hello"));
        assert_eq!(merged[1]["append"], json!("x"));
        assert_eq!(merged[2]["append"], json!("world"));
    }

    #[test]
    fn coalesce_upsert_swallows_earlier_delta_for_same_row() {
        let deltas = vec![
            delta(1, "text", "a"),
            json!({"op":"row.upserted","row":{"rowId":1,"kind":"assistantText","text":"a"}}),
            delta(1, "text", "b"),
        ];
        let merged = coalesce_deltas(&deltas);
        // 被 upsert 吞掉的 "a" 不得再与 "b" 合并（否则 apply 后变 "aab"）。
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["op"], json!("row.upserted"));
        assert_eq!(merged[1]["append"], json!("b"));
    }

    #[test]
    fn coalesce_does_not_merge_across_intervening_row() {
        let deltas = vec![
            delta(1, "text", "a"),
            delta(2, "text", "x"),
            delta(1, "text", "b"),
        ];
        let merged = coalesce_deltas(&deltas);
        assert_eq!(merged.len(), 3, "only adjacent same-row deltas may merge");
        assert_eq!(merged[0]["append"], json!("a"));
        assert_eq!(merged[2]["append"], json!("b"));
    }

    #[test]
    fn coalesce_stops_at_row_removed_barrier() {
        let deltas = vec![
            delta(1, "text", "a"),
            json!({"op":"row.removed","fromRowId":1}),
            delta(1, "text", "b"),
        ];
        let merged = coalesce_deltas(&deltas);
        assert_eq!(merged.len(), 3, "row.removed is a barrier");
        assert_eq!(merged[0]["append"], json!("a"));
        assert_eq!(merged[2]["append"], json!("b"));
    }

    #[test]
    fn coalesce_merges_adjacent_state_updates_shallowly() {
        let deltas = vec![
            json!({"op":"state.updated","patch":{"revision":1,"control":{"phase":"running"}}}),
            json!({"op":"state.updated","patch":{"revision":2}}),
        ];
        let merged = coalesce_deltas(&deltas);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["patch"]["revision"], json!(2));
        assert_eq!(merged[0]["patch"]["control"]["phase"], json!("running"));
    }

    #[test]
    fn coalesce_upsert_does_not_swallow_previous_generation() {
        let deltas = vec![
            json!({"op":"row.upserted","row":{"rowId":1,"kind":"assistantText","text":"a"}}),
            delta(1, "text", "b"),
            json!({"op":"row.upserted","row":{"rowId":1,"kind":"assistantText","text":"ab"}}),
        ];
        let merged = coalesce_deltas(&deltas);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["row"]["text"], json!("ab"));
    }

    #[test]
    fn replay_buffer_evicts_oldest_within_bounds() {
        let mut buffer = ReplayBuffer::new(2, usize::MAX);
        buffer.push("a".to_string());
        buffer.push("bb".to_string());
        buffer.push("ccc".to_string());
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.lines().next().map(String::as_str), Some("bb"));
    }
}
