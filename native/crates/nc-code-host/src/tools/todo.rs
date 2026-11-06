//! Task 工具：Todo（读写任务列表）。
//!
//! 契约对齐 TS `core/src/tool/handlers/todo.ts` + `contracts/tools/todo.ts`：
//!   TodoWrite {todos:[{content,status,priority}]} —— 整体替换；**至多一项
//!   in_progress**（TS 同一校验），status/priority 闭集与字段长度逐字一致。
//!   TodoRead  {} → 完整列表。
//!
//! 状态归属：todo 列表是**会话**事实（每 session 一份），随 transcript 落盘后
//! 可恢复——挂在 Session 上，不放全局。

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::tools::{Tool, ToolContext, ToolContract, ToolError, ToolErrorCode};

const MAX_TODOS: usize = 100;
const MAX_CONTENT_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TodoPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    pub priority: TodoPriority,
}

/// 会话级 todo 存储：Session 持有它（随 meta 恢复），工具经 ToolContext 访问。
pub type TodoStore = Arc<Mutex<Vec<TodoItem>>>;

pub fn new_todo_store() -> TodoStore {
    Arc::new(Mutex::new(Vec::new()))
}

fn parse_items(input: &Value) -> Result<Vec<TodoItem>, ToolError> {
    let todos = input
        .get("todos")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::new(ToolErrorCode::InvalidInput, "todos: required array"))?;
    if todos.len() > MAX_TODOS {
        return Err(ToolError::new(
            ToolErrorCode::InvalidInput,
            format!("todos: at most {MAX_TODOS} items"),
        ));
    }
    let mut items = Vec::with_capacity(todos.len());
    for todo in todos {
        let content = todo
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new(ToolErrorCode::InvalidInput, "content: required string"))?
            .trim()
            .to_string();
        if content.is_empty() {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                "content must be non-empty",
            ));
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                format!("content exceeds {MAX_CONTENT_CHARS} characters"),
            ));
        }
        let status = match todo.get("status").and_then(Value::as_str) {
            Some("pending") => TodoStatus::Pending,
            Some("in_progress") => TodoStatus::InProgress,
            Some("completed") => TodoStatus::Completed,
            other => {
                return Err(ToolError::new(
                    ToolErrorCode::InvalidInput,
                    format!(
                        "status: invalid value {other:?}; expected pending|in_progress|completed"
                    ),
                ))
            }
        };
        let priority = match todo.get("priority").and_then(Value::as_str) {
            Some("high") => TodoPriority::High,
            Some("medium") => TodoPriority::Medium,
            Some("low") => TodoPriority::Low,
            other => {
                return Err(ToolError::new(
                    ToolErrorCode::InvalidInput,
                    format!("priority: invalid value {other:?}; expected high|medium|low"),
                ))
            }
        };
        items.push(TodoItem {
            content,
            status,
            priority,
        });
    }
    // TS removed the old "at most one in_progress" refusal: parallel sub-agents may
    // each hold one in-progress item, so the host must not reject that input.
    Ok(items)
}

pub struct TodoWriteTool;

impl Tool for TodoWriteTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "TodoWrite".to_string(),
            description: "Create or update the structured task list for the current session. Replaces the whole list.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The complete updated todo list.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string", "description": "Brief description of the task"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                                "priority": {"type": "string", "enum": ["high", "medium", "low"]},
                            },
                            "required": ["content", "status", "priority"],
                        },
                    },
                },
                "required": ["todos"],
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: crate::tools::SCOPE_NONE,
            timeout_ms: 5_000,
            max_output_bytes: 8 * 1024,
            max_model_bytes: 8 * 1024,
            preview_direction: "head",
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let items = parse_items(input)?;
        let old = ctx
            .todos
            .lock()
            .map(|store| store.clone())
            .unwrap_or_default();
        if let Ok(mut store) = ctx.todos.lock() {
            *store = items.clone();
        }
        let count = |status: TodoStatus| items.iter().filter(|item| item.status == status).count();
        Ok(json!({
            "summary": {
                "total": items.len(),
                "pending": count(TodoStatus::Pending),
                "inProgress": count(TodoStatus::InProgress),
                "completed": count(TodoStatus::Completed),
            },
            "oldTodos": old,
            "todos": items,
        })
        .to_string())
    }
}

pub struct TodoReadTool;

impl Tool for TodoReadTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "TodoRead".to_string(),
            description: "Read the current session's structured task list.".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: crate::tools::SCOPE_NONE,
            timeout_ms: 5_000,
            max_output_bytes: 8 * 1024,
            max_model_bytes: 8 * 1024,
            preview_direction: "head",
        }
    }

    fn execute(&self, _input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let todos = ctx
            .todos
            .lock()
            .map(|store| store.clone())
            .unwrap_or_default();
        Ok(json!({"todos": todos}).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn test_ctx() -> ToolContext {
        ToolContext {
            workspace_root: PathBuf::from("."),
            full_access: true,
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: new_todo_store(),
            provider_config: None,
            session_id: "test-session".to_string(),
            model_ref: None,
            tool_call_id: None,
            plan_mode: false,
            ask_user: None,
            plan: None,
            subagent: None,
            background: None,
            sessions: None,
            workflow: None,
            automation: None,
            browser: None,
            computer: None,
            offpeak: None,
        }
    }

    #[test]
    fn accepts_two_in_progress_items() {
        let input = json!({"todos": [
            {"content": "a", "status": "in_progress", "priority": "high"},
            {"content": "b", "status": "in_progress", "priority": "low"},
        ]});
        let items =
            parse_items(&input).expect("parallel sub-agents may each hold one in-progress item");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn accepts_one_in_progress_and_closes_over_fields() {
        let input = json!({"todos": [
            {"content": " a ", "status": "in_progress", "priority": "high"},
            {"content": "b", "status": "pending", "priority": "low"},
        ]});
        let items = parse_items(&input).expect("valid list");
        assert_eq!(items[0].content, "a", "content must be trimmed");
        assert_eq!(items[0].status, TodoStatus::InProgress);
    }

    #[test]
    fn rejects_invalid_closures() {
        let bad_status = json!({"todos": [{"content": "a", "status": "done", "priority": "high"}]});
        assert!(parse_items(&bad_status).is_err());
        let bad_priority =
            json!({"todos": [{"content": "a", "status": "pending", "priority": "urgent"}]});
        assert!(parse_items(&bad_priority).is_err());
        let empty = json!({"todos": [{"content": "  ", "status": "pending", "priority": "high"}]});
        assert!(parse_items(&empty).is_err());
    }

    #[test]
    fn rejects_too_many_todos_and_long_content() {
        let too_many: Vec<Value> = (0..101)
            .map(|i| json!({"content": format!("t{i}"), "status": "pending", "priority": "low"}))
            .collect();
        assert!(parse_items(&json!({"todos": too_many})).is_err());

        let long = "x".repeat(201);
        let err = parse_items(
            &json!({"todos": [{"content": long, "status": "pending", "priority": "low"}]}),
        );
        assert!(err.is_err());
    }

    #[test]
    fn execute_write_and_read_round_trip() {
        let ctx = test_ctx();
        let write = TodoWriteTool;
        let read = TodoReadTool;
        let input = json!({"todos": [
            {"content": "a", "status": "in_progress", "priority": "high"},
            {"content": "b", "status": "completed", "priority": "low"},
        ]});
        let out = write.execute(&input, &ctx).expect("write");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["summary"]["total"], 2);
        assert_eq!(v["summary"]["inProgress"], 1);
        assert_eq!(v["summary"]["completed"], 1);
        assert_eq!(v["oldTodos"].as_array().unwrap().len(), 0);
        assert_eq!(v["todos"].as_array().unwrap().len(), 2);

        let read_out = read.execute(&json!({}), &ctx).expect("read");
        let rv: Value = serde_json::from_str(&read_out).expect("json");
        assert_eq!(rv["todos"].as_array().unwrap().len(), 2);
        assert_eq!(rv["todos"][0]["content"], "a");
    }

    #[test]
    fn execute_write_replaces_and_tracks_old_todos() {
        let ctx = test_ctx();
        let write = TodoWriteTool;
        write
            .execute(
                &json!({"todos":[{"content":"old","status":"pending","priority":"high"}]}),
                &ctx,
            )
            .expect("first");
        let out = write
            .execute(
                &json!({"todos":[{"content":"new","status":"completed","priority":"low"}]}),
                &ctx,
            )
            .expect("second");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["oldTodos"][0]["content"], "old");
        assert_eq!(v["todos"][0]["content"], "new");
        assert_eq!(v["todos"][0]["status"], "completed");
    }
}
