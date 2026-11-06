//! Task snapshot projection: host v4 rows + session config → `ZCodeTaskSnapshot`.
//!
//! The desktop v4 surface reads conversations through topic frames/rowsRange; the
//! legacy task facade still exposes `getTaskSnapshot` for replayable/mobile clients.
//! This module is the single mapping from the host's row projection to the persisted
//! message shape (`ZCodeSessionFile`) so the two read models cannot drift.

use serde_json::{json, Value};

/// The projection of `ZCodePersistedMessage`. The row order is exactly the transcript order:
/// userInput → a user message; reasoning/assistantText/toolCall fold into the following (or current)
/// assistant message and keep their interleaved order inside `parts`.
pub fn rows_to_messages(rows: &[Value]) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    let mut assistant_index: Option<usize> = None;
    let mut pending_thought: Option<String> = None;

    for row in rows {
        let Some(kind) = row.get("kind").and_then(Value::as_str) else {
            continue;
        };
        let created_at = row.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
        match kind {
            "userInput" => {
                let text = row.get("text").and_then(Value::as_str).unwrap_or("");
                if text.trim().is_empty() {
                    continue;
                }
                messages.push(json!({
                    "role": "user",
                    "content": text,
                    "timestamp": created_at,
                }));
                assistant_index = None;
                pending_thought = None;
            }
            "reasoning" => {
                let text = row.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                match assistant_index {
                    Some(index) => {
                        let message = &mut messages[index];
                        let parts = message["parts"].as_array_mut();
                        match parts {
                            Some(parts) => {
                                let last_is_thought = parts
                                    .last()
                                    .map(|part| {
                                        part.get("type").and_then(Value::as_str) == Some("thought")
                                    })
                                    .unwrap_or(false);
                                if last_is_thought {
                                    if let Some(existing) = parts.last_mut() {
                                        let previous = existing
                                            .get("content")
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        existing["content"] = json!(format!("{previous}{text}"));
                                    }
                                } else {
                                    parts.push(json!({"type": "thought", "content": text}));
                                }
                            }
                            None => {
                                message["parts"] = json!([{"type": "thought", "content": text}]);
                            }
                        }
                        let previous = message.get("thought").and_then(Value::as_str).unwrap_or("");
                        message["thought"] = json!(format!("{previous}{text}"));
                    }
                    None => {
                        pending_thought = Some(match pending_thought.take() {
                            Some(previous) => format!("{previous}{text}"),
                            None => text.to_string(),
                        });
                    }
                }
            }
            "assistantText" => {
                let text = row.get("text").and_then(Value::as_str).unwrap_or("");
                if text.trim().is_empty() {
                    continue;
                }
                let model = row.get("model").and_then(Value::as_str);
                let mut message = json!({
                    "role": "assistant",
                    "content": text,
                    "timestamp": created_at,
                    "characterCount": text.chars().count(),
                });
                if let Some(model) = model {
                    message["model"] = json!(model);
                }
                if let Some(response_id) = row.get("assistantResponseId").and_then(Value::as_str) {
                    message["id"] = json!(response_id);
                }
                if let Some(thought) = pending_thought.take() {
                    message["thought"] = json!(thought);
                    message["parts"] = json!([
                        {"type": "thought", "content": thought},
                        {"type": "content", "content": text},
                    ]);
                } else {
                    message["parts"] = json!([{"type": "content", "content": text}]);
                }
                messages.push(message);
                assistant_index = Some(messages.len() - 1);
            }
            "toolCall" => {
                let tool = tool_call_from_row(row);
                let target_index = assistant_index.or_else(|| {
                    if messages
                        .last()
                        .map(|message| {
                            message.get("role").and_then(Value::as_str) == Some("assistant")
                        })
                        .unwrap_or(false)
                    {
                        Some(messages.len() - 1)
                    } else {
                        None
                    }
                });
                match target_index {
                    Some(index) => {
                        let message = &mut messages[index];
                        let tools = message["tools"].as_array_mut();
                        match tools {
                            Some(tools) => tools.push(tool),
                            None => message["tools"] = json!([tool]),
                        }
                        let tool_index = message["tools"]
                            .as_array()
                            .map(|tools| tools.len().saturating_sub(1))
                            .unwrap_or(0);
                        match message["parts"].as_array_mut() {
                            Some(parts) => {
                                parts.push(json!({"type": "tool-call", "toolIndex": tool_index}))
                            }
                            None => {
                                message["parts"] =
                                    json!([{"type": "tool-call", "toolIndex": tool_index}])
                            }
                        }
                        assistant_index = Some(index);
                    }
                    None => {
                        // A tool row with no preceding assistantText (for example a tool at the head of the turn): its own assistant message.
                        messages.push(json!({
                            "role": "assistant",
                            "content": "",
                            "timestamp": created_at,
                            "tools": [tool],
                            "parts": [{"type": "tool-call", "toolIndex": 0}],
                        }));
                        assistant_index = Some(messages.len() - 1);
                    }
                }
            }
            _ => {}
        }
    }
    messages
}

fn tool_call_from_row(row: &Value) -> Value {
    let input_text = row.get("inputText").and_then(Value::as_str).unwrap_or("");
    let input: Value =
        serde_json::from_str(input_text).unwrap_or_else(|_| Value::String(input_text.to_string()));
    let status = match row.get("status").and_then(Value::as_str) {
        Some("success") | Some("completed") => "completed",
        Some("error") | Some("failed") => "failed",
        Some("cancelled") | Some("stopped") => "stopped",
        Some("denied") => "denied",
        _ => "completed",
    };
    let mut tool = json!({
        "toolName": row.get("toolName").cloned().unwrap_or(Value::Null),
        "status": status,
        "input": input,
    });
    if let Some(error) = row.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("tool call failed");
        tool["error"] = json!(message);
    }
    tool
}

/// `ZCodeTaskSnapshot` (`ZCodeSessionFile` plus the response-state fields).
///
/// `message_limit` takes a tail window; `tool_limit` trims the number of tool rows per message and attaches `toolSlice`
/// metadata (the client can pull the remainder incrementally through `getTaskSnapshotToolCallsSlice`).
pub fn build_snapshot(
    meta: &Value,
    transcript: &Value,
    runtime: &Value,
    message_limit: Option<usize>,
    tool_limit: Option<usize>,
) -> Value {
    let rows = transcript
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut messages = rows_to_messages(&rows);
    let total_messages = messages.len();
    let mut truncated_before = false;
    if let Some(limit) = message_limit {
        if limit > 0 && messages.len() > limit {
            let keep_from = messages.len() - limit;
            messages = messages.split_off(keep_from);
            truncated_before = true;
        }
    }
    if let Some(limit) = tool_limit {
        for (message_index, message) in messages.iter_mut().enumerate() {
            let Some(tools) = message.get("tools").and_then(Value::as_array).cloned() else {
                continue;
            };
            let total = tools.len();
            if total > limit {
                let kept = tools[..limit].to_vec();
                message["tools"] = Value::Array(kept);
                message["toolSlice"] = json!({
                    "messageIndex": message_index,
                    "totalTools": total,
                    "startToolIndex": 0,
                    "endToolIndexExclusive": limit,
                });
            }
        }
    }
    json!({
        "meta": meta,
        "messages": messages,
        "fileChanges": file_changes_by_turn(transcript),
        "runtime": runtime,
        "history": {
            "truncatedBefore": truncated_before,
            "totalMessages": total_messages,
        },
        "slashCommands": [],
    })
}

fn file_changes_by_turn(transcript: &Value) -> Value {
    let Some(records) = transcript.get("fileChanges").and_then(Value::as_array) else {
        return json!([]);
    };
    let mut turn_order: Vec<String> = Vec::new();
    let mut by_turn: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    for record in records {
        let Some(turn_id) = record.get("turnId").and_then(Value::as_str) else {
            continue;
        };
        if !by_turn.contains_key(turn_id) {
            turn_order.push(turn_id.to_string());
        }
        by_turn.entry(turn_id.to_string()).or_default().push(json!({
            "path": record.get("path").cloned().unwrap_or(Value::Null),
            "beforeContent": record.get("beforeContent").cloned().unwrap_or(Value::Null),
            "afterContent": record.get("afterContent").cloned().unwrap_or(Value::Null),
            "writeCount": 1,
        }));
    }
    Value::Array(
        turn_order
            .into_iter()
            .enumerate()
            .map(|(turn_index, turn_id)| {
                json!({
                    "turnIndex": turn_index,
                    "snapshots": by_turn.remove(&turn_id).unwrap_or_default(),
                    "fileState": "applied",
                })
            })
            .collect(),
    )
}

/// Session configuration → `ZCodeConfigOption[]` (the three genuinely switchable surfaces: mode/model/thought).
/// Only values already known for this session are projected; candidate lists are not forged: an absent `options` means the client should use
/// its own model catalog (the v4 configu channel) rather than a hard-coded copy from here.
pub fn config_options(facts: &Value) -> Vec<Value> {
    let mode = facts
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("build")
        .to_string();
    let model = facts
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let thought = facts
        .get("thought")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let mut options = vec![json!({
        "id": "mode",
        "name": "Mode",
        "category": "mode",
        "type": "select",
        "currentValue": mode,
        "options": [
            {"value": "build", "name": "Build"},
            {"value": "plan", "name": "Plan"},
            {"value": "edit", "name": "Edit"},
            {"value": "yolo", "name": "YOLO"},
        ],
    })];
    if let Some(model) = model {
        options.push(json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": model,
        }));
    }
    if let Some(thought) = thought {
        options.push(json!({
            "id": "thought_level",
            "name": "Thinking",
            "category": "thought_level",
            "type": "select",
            "currentValue": thought,
        }));
    }
    options
}

/// Pulling the rest of a tool slice: the real projection of the tool range addressed by message index.
pub fn tool_calls_slice(
    meta: &Value,
    transcript: &Value,
    message_index: usize,
    start_tool_index: usize,
    limit: usize,
) -> Option<Value> {
    let rows = transcript.get("rows").and_then(Value::as_array)?;
    let messages = rows_to_messages(rows);
    let message = messages.get(message_index)?;
    let tools = message.get("tools").and_then(Value::as_array)?;
    let total = tools.len();
    let end = (start_tool_index + limit).min(total);
    let slice: Vec<Value> = if start_tool_index >= total {
        Vec::new()
    } else {
        tools[start_tool_index..end].to_vec()
    };
    Some(json!({
        "taskId": meta.get("taskId").cloned().unwrap_or(Value::Null),
        "messageIndex": message_index,
        "totalTools": total,
        "startToolIndex": start_tool_index,
        "endToolIndexExclusive": end,
        "tools": slice,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(value: Value) -> Value {
        value
    }

    #[test]
    fn rows_project_into_interleaved_assistant_message() {
        let rows = vec![
            row(json!({"kind":"userInput","text":"hello","createdAt":1,"rowId":1})),
            row(json!({"kind":"reasoning","text":"thinking","createdAt":2,"rowId":2})),
            row(
                json!({"kind":"assistantText","text":"hi","createdAt":3,"rowId":3,"assistantResponseId":"resp-1","model":"m"}),
            ),
            row(
                json!({"kind":"toolCall","toolCallId":"c1","toolName":"Read","status":"success","inputText":"{\"path\":\"a\"}","createdAt":4,"rowId":4}),
            ),
        ];
        let messages = rows_to_messages(&rows);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], json!("user"));
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(messages[1]["content"], json!("hi"));
        assert_eq!(messages[1]["thought"], json!("thinking"));
        assert_eq!(messages[1]["tools"][0]["toolName"], json!("Read"));
        assert_eq!(messages[1]["tools"][0]["input"]["path"], json!("a"));
        assert_eq!(
            messages[1]["parts"],
            json!([
                {"type":"thought","content":"thinking"},
                {"type":"content","content":"hi"},
                {"type":"tool-call","toolIndex":0}
            ])
        );
    }

    #[test]
    fn message_limit_keeps_tail_and_reports_history() {
        let rows = vec![
            row(json!({"kind":"userInput","text":"one","createdAt":1,"rowId":1})),
            row(json!({"kind":"userInput","text":"two","createdAt":2,"rowId":2})),
        ];
        let transcript = json!({"rows": rows, "fileChanges": []});
        let snapshot = build_snapshot(
            &json!({"taskId":"t"}),
            &transcript,
            &json!({}),
            Some(1),
            None,
        );
        assert_eq!(snapshot["messages"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["messages"][0]["content"], json!("two"));
        assert_eq!(snapshot["history"]["truncatedBefore"], json!(true));
        assert_eq!(snapshot["history"]["totalMessages"], json!(2));
    }

    #[test]
    fn tool_limit_marks_slice_metadata() {
        let rows = vec![
            row(json!({"kind":"assistantText","text":"ok","createdAt":1,"rowId":1})),
            row(
                json!({"kind":"toolCall","toolCallId":"a","toolName":"A","status":"success","inputText":"{}","createdAt":2,"rowId":2}),
            ),
            row(
                json!({"kind":"toolCall","toolCallId":"b","toolName":"B","status":"success","inputText":"{}","createdAt":3,"rowId":3}),
            ),
        ];
        let transcript = json!({"rows": rows, "fileChanges": []});
        let snapshot = build_snapshot(
            &json!({"taskId":"t"}),
            &transcript,
            &json!({}),
            None,
            Some(1),
        );
        let message = &snapshot["messages"][0];
        assert_eq!(message["tools"].as_array().unwrap().len(), 1);
        assert_eq!(message["toolSlice"]["totalTools"], json!(2));
        let slice = tool_calls_slice(&json!({"taskId":"t"}), &transcript, 0, 1, 5).expect("slice");
        assert_eq!(slice["tools"][0]["toolName"], json!("B"));
        assert_eq!(slice["endToolIndexExclusive"], json!(2));
    }

    #[test]
    fn file_changes_group_by_turn() {
        let transcript = json!({
            "rows": [],
            "fileChanges": [
                {"turnId":"t1","path":"a.txt","afterContent":"x","additions":1,"deletions":0},
                {"turnId":"t1","path":"b.txt","afterContent":"y","additions":1,"deletions":0},
            ]
        });
        let changes = file_changes_by_turn(&transcript);
        assert_eq!(changes.as_array().unwrap().len(), 1);
        assert_eq!(changes[0]["snapshots"].as_array().unwrap().len(), 2);
    }
}
