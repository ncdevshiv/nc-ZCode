//! ConversationSnapshot 构造，形状对齐 `snapshot.ts` 的
//! `conversationSnapshotSchema`（必填字段一个不少；可选字段按本 increment 的
//! 能力面缺省：不投影 interaction/workflow/subagent 运行态）。
//!
//! 语义边界（README 差异表同步记录）：`control`/`config`/`usage` 取本 host 的
//! 真实状态；turn 级事实（activeWorks/lastError）由 turn 生命周期写入。
//! 全部字段级整体替换（state.updated patch），绝不在客户端做深合并。

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct SnapshotInput<'a> {
    pub session_id: &'a str,
    pub log_epoch: &'a str,
    pub seq: u64,
    pub revision: i64,
    pub phase: &'a str,
    pub can_stop: bool,
    pub last_error: Option<&'a Value>,
    pub title: &'a str,
    pub title_source: &'a str,
    pub rows: &'a [Value],
    pub total_row_count: usize,
    pub first_row_id: Option<u64>,
    pub usage: &'a Value,
    pub queue_auto_drain: bool,
}

fn allowed() -> Value {
    json!({"allowed": true})
}

/// `sessionControlSchema`。stopTargetKind 闭集取 "unknown"（无在途工作时无目标）。
pub fn control(phase: &str, can_stop: bool, last_error: Option<&Value>) -> Value {
    json!({
        "phase": phase,
        "sessionEnded": matches!(phase, "completedSuccess" | "completedInterrupted" | "error"),
        "canStop": can_stop,
        "stopState": if can_stop { "stoppable" } else { "idle" },
        "stopTargetKind": "unknown",
        "activeWorks": [],
        "lastError": last_error.cloned().unwrap_or(Value::Null),
        "apiRetry": Value::Null,
    })
}

/// `sessionActionAvailabilitySchema`：本 increment 的能力面。
pub fn availability() -> Value {
    json!({
        "fork": allowed(),
        "compact": allowed(),
        "switchModelConfig": allowed(),
        "setFollowupMode": allowed(),
        "queueEdit": allowed(),
        "sendQueuedNow": allowed(),
        "pauseGoal": allowed(),
        "resumeGoal": allowed(),
    })
}

pub fn conversation_snapshot(input: &SnapshotInput<'_>) -> Value {
    json!({
        "protocolVersion": 1,
        "sessionId": input.session_id,
        "logEpoch": input.log_epoch,
        "seq": input.seq,
        "revision": input.revision,
        "control": control(input.phase, input.can_stop, input.last_error),
        "availability": availability(),
        "inputRouting": {"mode": "startNow"},
        "meta": {"title": input.title, "titleSource": input.title_source},
        "config": {
            "provider": "",
            "model": "",
            "thought": "",
            "thoughtLevels": [],
            "followupMode": "queue",
            "mode": "build",
        },
        "modelTransition": Value::Null,
        "usage": input.usage,
        "queue": {"items": [], "autoDrain": input.queue_auto_drain},
        "pendingInteractions": [],
        "pendingCommands": [],
        "backgroundWorks": [],
        "subagents": {"revision": 0, "childSessionIds": [], "running": [], "endedTotal": 0},
        "goal": Value::Null,
        "plan": Value::Null,
        "workspaceHookAdmission": Value::Null,
        "rows": {
            "window": input.rows,
            "totalCount": input.total_row_count,
            "firstRowId": input.first_row_id,
        },
    })
}

/// usage 初始态（`sessionUsageStateSchema`）：contextWindow 可空，cumulative 必填。
pub fn empty_usage() -> Value {
    json!({
        "contextWindow": Value::Null,
        "cumulative": {
            "inputTokens": 0,
            "outputTokens": 0,
            "cacheReadTokens": 0,
            "cacheWriteTokens": 0,
        },
    })
}

/// state.updated patch 只带变更键（键级整体替换）。本 increment 的投影面：
/// revision / control / usage / queue。
pub fn revision_patch(revision: i64) -> Value {
    json!({"revision": revision})
}

pub fn control_patch(phase: &str, can_stop: bool, last_error: Option<&Value>) -> Value {
    json!({"control": control(phase, can_stop, last_error)})
}

pub fn usage_patch(usage: &Value) -> Value {
    json!({"usage": usage})
}

/// `sessionErrorInfoSchema`：错误投影的权威形状。
pub fn session_error_info(
    code: &str,
    message: &str,
    recoverable: bool,
    source: &str,
    at: i64,
) -> Value {
    json!({
        "code": code,
        "message": message,
        "recoverable": recoverable,
        "at": at,
        "source": source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{assistant_text_row, RowBase};

    #[test]
    fn snapshot_has_all_required_top_level_keys() {
        let usage = empty_usage();
        let rows = vec![assistant_text_row(
            &RowBase {
                row_id: 1,
                turn_id: "t".into(),
                created_at: 1,
                created_at_seq: 1,
            },
            "r",
            "hi",
            "complete",
            None,
        )];
        let snapshot = conversation_snapshot(&SnapshotInput {
            session_id: "s",
            log_epoch: "e",
            seq: 1,
            revision: 1,
            phase: "running",
            can_stop: true,
            last_error: None,
            title: "",
            title_source: "default",
            rows: &rows,
            total_row_count: 1,
            first_row_id: Some(1),
            usage: &usage,
            queue_auto_drain: true,
        });
        for key in [
            "protocolVersion",
            "sessionId",
            "logEpoch",
            "seq",
            "revision",
            "control",
            "availability",
            "inputRouting",
            "meta",
            "config",
            "modelTransition",
            "usage",
            "queue",
            "pendingInteractions",
            "pendingCommands",
            "backgroundWorks",
            "subagents",
            "goal",
            "plan",
            "workspaceHookAdmission",
            "rows",
        ] {
            assert!(snapshot.get(key).is_some(), "missing required key: {key}");
        }
        assert_eq!(snapshot["rows"]["window"][0]["text"], json!("hi"));
        assert_eq!(snapshot["control"]["stopState"], json!("stoppable"));
        assert_eq!(snapshot["control"]["sessionEnded"], json!(false));
    }

    #[test]
    fn session_ended_derives_from_phase() {
        assert_eq!(
            control("completedSuccess", false, None)["sessionEnded"],
            json!(true)
        );
        assert_eq!(control("error", false, None)["sessionEnded"], json!(true));
        assert_eq!(control("draft", false, None)["sessionEnded"], json!(false));
    }
}
