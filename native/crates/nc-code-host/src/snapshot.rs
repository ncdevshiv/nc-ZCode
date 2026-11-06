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
    /// `queueStateSchema` 全量值（items/autoDrain/pauseReason?）。
    pub queue: Value,
    /// `pendingInteractionSchema[]` 全量值。
    pub pending_interactions: Value,
    pub stop_requested: bool,
    pub followup_mode: String,
    pub provider: String,
    pub model: String,
    pub thought: String,
    pub collaboration_mode: String,
    /// 当前 `goalStateSchema` 投影（无目标为 null）。
    pub goal: Value,
    /// `control.activeWorks` 的本 host 子集（goalVerifier）。
    pub active_works: Vec<Value>,
    /// `workspaceHookAdmission` 投影（无待审项为 null）。
    pub workspace_hook_admission: Value,
}

fn allowed() -> Value {
    json!({"allowed": true})
}

/// `sessionControlSchema`。stopTargetKind 从 activeWorks 派生（与 TS 同规则）：
/// 无工作 → unknown；只有一种 → 该种类；多种 → mixed。
pub fn control(
    phase: &str,
    can_stop: bool,
    stop_requested: bool,
    last_error: Option<&Value>,
    active_works: &[Value],
) -> Value {
    let stop_state = if !can_stop {
        "idle"
    } else if stop_requested {
        "stopping"
    } else {
        "stoppable"
    };
    let stop_target_kind = match active_works {
        [] => "unknown",
        [only] => only
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
        _ => "mixed",
    };
    json!({
        "phase": phase,
        "sessionEnded": matches!(phase, "completedSuccess" | "completedInterrupted" | "error"),
        "canStop": can_stop,
        "stopState": stop_state,
        "stopTargetKind": stop_target_kind,
        "activeWorks": active_works,
        "lastError": last_error.cloned().unwrap_or(Value::Null),
        "apiRetry": Value::Null,
    })
}

/// 未接线能力：显式 `allowed:false` + 与执行期一致的 reasonCode。
/// 能力面必须诚实—— advertise 一个会 fail-closed 的动作会让 UI 显示死按钮。
fn unavailable(command_type: &str) -> Value {
    json!({
        "allowed": false,
        "reasonCode": format!("fault.command.unsupported:{command_type}"),
    })
}

/// `computeInputRouting` 的 Rust 镜像：goal 裁判在途 → 入队（`goalVerifier` 不是
/// 普通 active turn，guide 模式也不能 steer）；running/prewarming → guide/enqueue；
/// completed + queue>0 + autoDrain=false → choice（客户端呈现 clear/keep）；
/// 其余 startNow。
pub fn input_routing(
    phase: &str,
    followup_mode: &str,
    queue_length: usize,
    auto_drain: bool,
    goal_verifying: bool,
) -> Value {
    if goal_verifying {
        return json!({"mode": "enqueue", "reasonCode": "goalVerifierAcceptsFutureInput"});
    }
    if phase == "running" || phase == "prewarming" {
        let mode = if followup_mode == "guide" {
            "guide"
        } else {
            "enqueue"
        };
        return json!({"mode": mode});
    }
    let completed = phase == "completedSuccess" || phase == "completedInterrupted";
    if completed && queue_length > 0 && !auto_drain {
        return json!({"mode": "choice", "reasonCode": "heldQueueInputRequiresChoice"});
    }
    json!({"mode": "startNow"})
}

/// 能力面按键：接线状态直接读 `commands::route`（唯一路由表），
/// 因此 advertise 与实际执行行为不可能分叉。
fn action(command_type: &str) -> Value {
    if crate::commands::route(command_type) == crate::commands::Route::Unsupported {
        unavailable(command_type)
    } else {
        allowed()
    }
}

/// `sessionActionAvailabilitySchema`：只 advertise 真正接线的动作。
/// pauseGoal/resumeGoal 是状态相关的 guard（`computeAvailability`，projection-state.ts:126），
/// 其余键读路由表。
pub fn availability(goal_status: Option<&str>) -> Value {
    let (pause_goal, resume_goal) = crate::goal::goal_availability(goal_status);
    json!({
        "fork": action("forkAssistant"),
        "compact": action("compact"),
        "switchModelConfig": action("switchModelConfig"),
        "setFollowupMode": action("setFollowupMode"),
        "queueEdit": action("editQueueItem"),
        "sendQueuedNow": action("sendQueuedNow"),
        "pauseGoal": pause_goal,
        "resumeGoal": resume_goal,
    })
}

pub fn conversation_snapshot(input: &SnapshotInput<'_>) -> Value {
    let queue_length = input
        .queue
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let auto_drain = input
        .queue
        .get("autoDrain")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let goal_status = input.goal.get("status").and_then(Value::as_str);
    let goal_verifying = input
        .active_works
        .iter()
        .any(|work| work.get("kind").and_then(Value::as_str) == Some("goalVerifier"));
    json!({
        "protocolVersion": 1,
        "sessionId": input.session_id,
        "logEpoch": input.log_epoch,
        "seq": input.seq,
        "revision": input.revision,
        "control": control(input.phase, input.can_stop, input.stop_requested, input.last_error, &input.active_works),
        "availability": availability(goal_status),
        "inputRouting": input_routing(input.phase, &input.followup_mode, queue_length, auto_drain, goal_verifying),
        "meta": {"title": input.title, "titleSource": input.title_source},
        "config": {
            "provider": input.provider,
            "model": input.model,
            "thought": input.thought,
            "thoughtLevels": [],
            "followupMode": input.followup_mode,
            "mode": input.collaboration_mode,
        },
        "modelTransition": Value::Null,
        "usage": input.usage,
        "queue": input.queue,
        "pendingInteractions": input.pending_interactions,
        "pendingCommands": [],
        "backgroundWorks": [],
        "subagents": {"revision": 0, "childSessionIds": [], "running": [], "endedTotal": 0},
        "goal": input.goal,
        "plan": Value::Null,
        "workspaceHookAdmission": input.workspace_hook_admission,
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

pub fn control_patch(
    phase: &str,
    can_stop: bool,
    stop_requested: bool,
    last_error: Option<&Value>,
    active_works: &[Value],
) -> Value {
    json!({"control": control(phase, can_stop, stop_requested, last_error, active_works)})
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
                entity_id: "resp-r".into(),
                created_at: 1,
                created_at_seq: 1,
            },
            "r",
            "hi",
            "complete",
            None,
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
            queue: json!({"items": [], "autoDrain": true}),
            pending_interactions: json!([]),
            stop_requested: false,
            followup_mode: "queue".to_string(),
            provider: String::new(),
            model: String::new(),
            thought: String::new(),
            collaboration_mode: "build".to_string(),
            goal: Value::Null,
            active_works: Vec::new(),
            workspace_hook_admission: Value::Null,
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
    fn snapshot_values_stay_inside_protocol_closed_sets() {
        let usage = empty_usage();
        let snap = conversation_snapshot(&SnapshotInput {
            session_id: "s",
            log_epoch: "e",
            seq: 1,
            revision: 1,
            phase: "running",
            can_stop: true,
            stop_requested: true,
            last_error: None,
            title: "",
            title_source: "default",
            rows: &[],
            total_row_count: 0,
            first_row_id: None,
            usage: &usage,
            queue: json!({"items": [], "autoDrain": true}),
            pending_interactions: json!([]),
            followup_mode: "queue".to_string(),
            provider: String::new(),
            model: String::new(),
            thought: String::new(),
            collaboration_mode: "build".to_string(),
            goal: Value::Null,
            active_works: Vec::new(),
            workspace_hook_admission: Value::Null,
        });
        // control.phase ∈ sessionPhaseSchema 闭集
        assert!(crate::session::Session::VALID_PHASES
            .contains(&snap["control"]["phase"].as_str().unwrap_or("")));
        // stopState / stopTargetKind ∈ 闭集
        assert!(["idle", "stoppable", "stopping"]
            .contains(&snap["control"]["stopState"].as_str().unwrap_or("")));
        assert!([
            "assistant",
            "tool",
            "subagent",
            "compact",
            "goalVerifier",
            "goalContinuation",
            "turnSteer",
            "mixed",
            "unknown"
        ]
        .contains(&snap["control"]["stopTargetKind"].as_str().unwrap_or("")));
        // inputRouting.mode ∈ 闭集
        assert!(["startNow", "enqueue", "guide", "reject", "choice"]
            .contains(&snap["inputRouting"]["mode"].as_str().unwrap_or("")));
        // config.followupMode ∈ 闭集
        assert!(["queue", "guide"].contains(&snap["config"]["followupMode"].as_str().unwrap_or("")));
        // availability：allowed=true，或 allowed=false 且带 reasonCode
        for key in [
            "fork",
            "compact",
            "switchModelConfig",
            "setFollowupMode",
            "queueEdit",
            "sendQueuedNow",
            "pauseGoal",
            "resumeGoal",
        ] {
            let entry = &snap["availability"][key];
            if entry["allowed"] == json!(true) {
                assert!(entry.get("reasonCode").is_none());
            } else {
                assert_eq!(entry["allowed"], json!(false), "availability.{key}");
                assert!(
                    entry["reasonCode"].as_str().is_some_and(|c| !c.is_empty()),
                    "availability.{key} must carry a reasonCode"
                );
            }
        }
    }

    #[test]
    fn input_routing_reflects_busy_phase_and_followup_mode() {
        assert_eq!(
            input_routing("running", "queue", 0, true, false)["mode"],
            json!("enqueue")
        );
        assert_eq!(
            input_routing("running", "guide", 0, true, false)["mode"],
            json!("guide")
        );
        assert_eq!(
            input_routing("prewarming", "queue", 0, true, false)["mode"],
            json!("enqueue")
        );
        assert_eq!(
            input_routing("prewarming", "guide", 0, true, false)["mode"],
            json!("guide")
        );
        // 空闲/草稿 → startNow（followupMode 不影响空闲路由）
        assert_eq!(
            input_routing("draft", "queue", 0, true, false)["mode"],
            json!("startNow")
        );
        assert_eq!(
            input_routing("draft", "guide", 0, true, false)["mode"],
            json!("startNow")
        );
        assert_eq!(
            input_routing("completedSuccess", "guide", 0, true, false)["mode"],
            json!("startNow")
        );
        // completed + queue>0 + autoDrain=false → choice（暂停队列的确认框）
        assert_eq!(
            input_routing("completedSuccess", "queue", 2, false, false)["mode"],
            json!("choice")
        );
    }

    #[test]
    fn availability_only_advertises_wired_actions() {
        let no_goal = availability(None);
        for wired in [
            "compact",
            "switchModelConfig",
            "setFollowupMode",
            "queueEdit",
            "sendQueuedNow",
            // forkAssistant 接线后必须同步：能力面读的是同一张路由表。
            "fork",
        ] {
            assert_eq!(
                no_goal[wired]["allowed"],
                json!(true),
                "{wired} is wired and must be allowed"
            );
        }
        for unwired in ["pauseGoal", "resumeGoal"] {
            // 目标命令已接线。无目标时它们仍必须诚实拒绝（不是 unsupported：
            // 命令会执行，只是 guard 判定没有可操作的目标）。
            assert_eq!(
                no_goal[unwired]["allowed"],
                json!(false),
                "{unwired} with no goal must not be advertised as allowed"
            );
            assert!(
                no_goal[unwired]["reasonCode"]
                    .as_str()
                    .is_some_and(|code| code.starts_with("guard.goal.")),
                "{unwired} must carry the goal guard reasonCode"
            );
        }
        // 目标状态驱动允许位：active 可 pause、paused 可 resume，且两个方向互斥。
        let active = availability(Some("active"));
        assert_eq!(active["pauseGoal"]["allowed"], json!(true));
        assert_eq!(active["resumeGoal"]["allowed"], json!(false));
        assert_eq!(
            active["resumeGoal"]["reasonCode"],
            json!("guard.goal.goalNotPaused")
        );
        let paused = availability(Some("paused"));
        assert_eq!(paused["pauseGoal"]["allowed"], json!(false));
        assert_eq!(
            paused["pauseGoal"]["reasonCode"],
            json!("guard.goal.goalNotActive")
        );
        assert_eq!(paused["resumeGoal"]["allowed"], json!(true));
        // verifying / notSatisfied 也是可暂停的投影态（TS 同一张表）。
        for status in ["verifying", "notSatisfied"] {
            assert_eq!(
                availability(Some(status))["pauseGoal"]["allowed"],
                json!(true),
                "{status} must stay pausable"
            );
        }
    }

    #[test]
    fn session_ended_derives_from_phase() {
        assert_eq!(
            control("completedSuccess", false, false, None, &[])["sessionEnded"],
            json!(true)
        );
        assert_eq!(
            control("error", false, false, None, &[])["sessionEnded"],
            json!(true)
        );
        assert_eq!(
            control("draft", false, false, None, &[])["sessionEnded"],
            json!(false)
        );
    }

    #[test]
    fn stop_state_reports_stopping_after_cancel_requested() {
        // 空闲 → idle；运行中未请求停止 → stoppable；已请求但未收口 → stopping。
        assert_eq!(
            control("draft", false, false, None, &[])["stopState"],
            json!("idle")
        );
        assert_eq!(
            control("running", true, false, None, &[])["stopState"],
            json!("stoppable")
        );
        assert_eq!(
            control("running", true, true, None, &[])["stopState"],
            json!("stopping")
        );
    }
}
