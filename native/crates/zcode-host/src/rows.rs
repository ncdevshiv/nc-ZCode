//! ConversationRow / ConversationDelta 的 JSON 构造，形状逐字段对齐
//! `packages/shared/src/zcode-protocol-v4/rows.ts` 与 `delta.ts`。
//!
//! 本 increment 只投影三种行（turnHeader / userInput / assistantText）与五个
//! delta op 中的四个（row.removed 属 edit/rewind 路径，尚未接线，不构造）。
//! 结构性规则与服务端一致：结构变化换整行（row.upserted），文本增长用
//! row.delta；rowId 单调递增、永不复用。

use serde_json::{json, Value};

pub const OP_ROW_APPENDED: &str = "row.appended";
pub const OP_ROW_UPSERTED: &str = "row.upserted";
pub const OP_ROW_DELTA: &str = "row.delta";
pub const OP_STATE_UPDATED: &str = "state.updated";

/// rowBaseFields 的公共部分（`rows.ts` rowBaseFields）。
#[derive(Debug, Clone)]
pub struct RowBase {
    pub row_id: u64,
    pub turn_id: String,
    pub created_at: i64,
    pub created_at_seq: u64,
}

pub fn row_base(base: &RowBase) -> Value {
    json!({
        "rowId": base.row_id,
        "turnId": base.turn_id,
        "createdAt": base.created_at,
        "createdAtSeq": base.created_at_seq,
    })
}

/// turnHeader 行（`rows.ts` turnHeaderRowSchema 的相关子集）。
pub fn turn_header_row(
    base: &RowBase,
    origin: &str,
    state: &str,
    started_at: i64,
    source_command_id: &str,
) -> Value {
    let mut row = row_base(base);
    if let Some(object) = row.as_object_mut() {
        object.insert("kind".to_string(), json!("turnHeader"));
        object.insert("origin".to_string(), json!(origin));
        object.insert("state".to_string(), json!(state));
        object.insert("startedAt".to_string(), json!(started_at));
        object.insert("sourceCommandId".to_string(), json!(source_command_id));
    }
    row
}

/// userInput 行（`rows.ts` userInputRowSchema 的相关子集）。
pub fn user_input_row(
    base: &RowBase,
    text: &str,
    source_command_id: &str,
    client_id: &str,
) -> Value {
    let mut row = row_base(base);
    if let Some(object) = row.as_object_mut() {
        object.insert("kind".to_string(), json!("userInput"));
        object.insert("text".to_string(), json!(text));
        object.insert("origin".to_string(), json!("realUser"));
        object.insert("sourceCommandId".to_string(), json!(source_command_id));
        object.insert("clientId".to_string(), json!(client_id));
    }
    row
}

/// assistantText 行；`text` 始终为当前全量正文（终态 upsert 的权威载体）。
pub fn assistant_text_row(
    base: &RowBase,
    response_id: &str,
    text: &str,
    state: &str,
    model: Option<&str>,
) -> Value {
    let mut row = row_base(base);
    if let Some(object) = row.as_object_mut() {
        object.insert("kind".to_string(), json!("assistantText"));
        object.insert("assistantResponseId".to_string(), json!(response_id));
        object.insert("text".to_string(), json!(text));
        object.insert("state".to_string(), json!(state));
        if let Some(model) = model {
            object.insert("model".to_string(), json!(model));
        }
    }
    row
}

pub fn row_appended(row: Value) -> Value {
    json!({"op": OP_ROW_APPENDED, "row": row})
}

pub fn row_upserted(row: Value) -> Value {
    json!({"op": OP_ROW_UPSERTED, "row": row})
}

pub fn row_delta(row_id: u64, path: &str, append: &str) -> Value {
    json!({"op": OP_ROW_DELTA, "rowId": row_id, "path": path, "append": append})
}

/// toolCall 行（`rows.ts` toolCallRowSchema 的相关子集）：必填 toolCallId/
/// toolName/status/inputText；`inputText` 是流式路径（continuous 档可增量，
/// 本 increment 整条一次给出，终态 upsert 携带全量）。error 仅 status=error 携带。
pub fn tool_call_row(
    base: &RowBase,
    tool_call_id: &str,
    tool_name: &str,
    status: &str,
    input_text: &str,
    error: Option<(&str, &str)>,
) -> Value {
    let mut row = row_base(base);
    if let Some(object) = row.as_object_mut() {
        object.insert("kind".to_string(), json!("toolCall"));
        object.insert("toolCallId".to_string(), json!(tool_call_id));
        object.insert("toolName".to_string(), json!(tool_name));
        object.insert("status".to_string(), json!(status));
        object.insert("inputText".to_string(), json!(input_text));
        if let Some((code, message)) = error {
            object.insert(
                "error".to_string(),
                json!({"code": code, "message": message}),
            );
        }
    }
    row
}

pub fn state_updated(patch: Value) -> Value {
    json!({"op": OP_STATE_UPDATED, "patch": patch})
}

/// 终态一致性断言（黄金不变量的 Rust 侧表现）：过滤+合并后的 delta 序列 apply 到
/// 初始行集合上，必须得到与终态 snapshot 完全一致的行集合。e2e 与单元测试共用。
pub fn apply_deltas(rows: &mut Vec<Value>, deltas: &[Value]) {
    for delta in deltas {
        match delta.get("op").and_then(Value::as_str) {
            Some(OP_ROW_APPENDED) => {
                if let Some(row) = delta.get("row") {
                    rows.push(row.clone());
                }
            }
            Some(OP_ROW_UPSERTED) => {
                if let Some(row) = delta.get("row") {
                    let row_id = row.get("rowId").and_then(Value::as_u64);
                    if let Some(row_id) = row_id {
                        if let Some(slot) = rows.iter_mut().find(|candidate| {
                            candidate.get("rowId").and_then(Value::as_u64) == Some(row_id)
                        }) {
                            *slot = row.clone();
                            continue;
                        }
                    }
                    rows.push(row.clone());
                }
            }
            Some(OP_ROW_DELTA) => {
                let Some(row_id) = delta.get("rowId").and_then(Value::as_u64) else {
                    continue;
                };
                let path = delta.get("path").and_then(Value::as_str).unwrap_or("");
                let append = delta.get("append").and_then(Value::as_str).unwrap_or("");
                if let Some(row) = rows.iter_mut().find(|candidate| {
                    candidate.get("rowId").and_then(Value::as_u64) == Some(row_id)
                }) {
                    let current = row.get(path).and_then(Value::as_str).unwrap_or("");
                    let merged = format!("{current}{append}");
                    if let Some(object) = row.as_object_mut() {
                        object.insert(path.to_string(), json!(merged));
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(row_id: u64) -> RowBase {
        RowBase {
            row_id,
            turn_id: "turn-1".to_string(),
            created_at: 1_700_000_000_000,
            created_at_seq: row_id,
        }
    }

    #[test]
    fn delta_then_upsert_round_trip_is_stable() {
        let mut rows = vec![user_input_row(&base(1), "hi", "cmd-1", "client-1")];
        let mut streamed = vec![row_appended(assistant_text_row(
            &base(2),
            "resp-1",
            "",
            "streaming",
            Some("m"),
        ))];
        streamed.push(row_delta(2, "text", "he"));
        streamed.push(row_delta(2, "text", "llo"));
        apply_deltas(&mut rows, &streamed);
        assert_eq!(rows[1]["text"], json!("hello"));

        // 终态 upsert 携带全量正文；再次 apply 不改变结果（幂等）。
        let final_row = assistant_text_row(&base(2), "resp-1", "hello", "complete", Some("m"));
        apply_deltas(&mut rows, &[row_upserted(final_row.clone())]);
        assert_eq!(rows[1], final_row);
    }

    #[test]
    fn upsert_replaces_by_row_id_without_duplicating() {
        let mut rows = vec![assistant_text_row(&base(1), "r", "a", "streaming", None)];
        apply_deltas(
            &mut rows,
            &[row_upserted(assistant_text_row(
                &base(1),
                "r",
                "ab",
                "complete",
                None,
            ))],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["text"], json!("ab"));
        assert_eq!(rows[0]["state"], json!("complete"));
    }
}
