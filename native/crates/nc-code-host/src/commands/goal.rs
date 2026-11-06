//! goal 命令组：`sendGoalCommand` / `pauseGoal` / `resumeGoal`。
//!
//! 语义对齐 TS `bootstrap/src/zcode-protocol-v4/commands/handlers/goal-compact.ts`
//! 与 `core/src/runtime/methods/target*.ts`：
//! - `sendGoalCommand`：校验（空目标 → plan 互斥 → 4000 字符上限）→ 写入/替换目标
//!   （replace 语义）→ 启动续跑循环。busy 时由 dispatcher 入队，不在这里处理。
//! - `pauseGoal`：控制命令（不经 inbox，避免排在在途 turn 后面死锁）——CAS 校验后
//!   把 active 目标置 paused 并中止在途 turn/裁判。
//! - `resumeGoal`：paused → active 并立即续跑；运行中拒绝（`activeTurn`），
//!   plan 模式且已有目标时拒绝（`guard.planGoalMutuallyExclusive`），无目标幂等成功。
//!
//! 三条命令与目标状态机的全部接触面都在这里 + `goal_loop`；`goal.rs` 不碰命令面。

use std::sync::Arc;

use nc_code_core::protocol::{CommandEnvelope, CommandFinal, CommandStatus, REASON_STALE_REVISION};
use serde_json::{json, Value};

use crate::commands::{payload_bool, payload_str, settle_admitted};
use crate::goal::{GoalRecord, GoalStatus, MAX_GOAL_OBJECTIVE_CHARS};
use crate::goal_loop::{self, LoopStart};
use crate::session::Session;
use crate::turn::FAULT_PROVIDER_NOT_CONFIGURED;
use crate::HostServer;

/// `sendGoalCommand` 的提交期校验（dispatcher 与执行器共用同一份，避免
/// "入队时放行、执行时拒绝"的两个真相）。
pub(crate) fn validate_goal_submission(payload: &Value) -> Result<String, Box<CommandFinal>> {
    let objective = payload_str(payload, "text", "").trim().to_string();
    if objective.is_empty() {
        return Err(Box::new(CommandFinal {
            message: Some("Usage: /goal <objective>".to_string()),
            ..CommandFinal::with_reason(CommandStatus::Rejected, "emptyObjective")
        }));
    }
    if objective.chars().count() > MAX_GOAL_OBJECTIVE_CHARS {
        return Err(Box::new(CommandFinal {
            message: Some(format!(
                "goal objective must be at most {MAX_GOAL_OBJECTIVE_CHARS} characters"
            )),
            ..CommandFinal::with_reason(CommandStatus::Rejected, "fault.command.executionFailed")
        }));
    }
    if payload_bool(payload, "planEnabled") {
        return Err(Box::new(CommandFinal {
            message: Some("Plan and Goal cannot be active at the same time.".to_string()),
            ..CommandFinal::with_reason(CommandStatus::Rejected, "guard.planGoalMutuallyExclusive")
        }));
    }
    Ok(objective)
}

impl HostServer {
    /// `sendGoalCommand`：写入/替换目标并启动续跑循环。
    pub(crate) async fn run_send_goal_command(
        self: &Arc<Self>,
        session: Arc<Session>,
        admitted: nc_code_core::inbox::Admitted,
    ) {
        let payload = admitted.envelope.payload.clone();
        let objective = match validate_goal_submission(&payload) {
            Ok(objective) => objective,
            Err(final_) => {
                settle_admitted(&session, admitted, *final_, Vec::new(), &self.outbox);
                return;
            }
        };
        // 重复 set = replace（TS 的收敛语义：UI 视为新目标，db 不能留旧目标）。
        let replaced = session.goal().is_some();
        let target_id = self.next_id("goal");
        session.set_goal(Some(GoalRecord::new(target_id.clone(), objective)));
        let mut lines = Vec::new();
        self.persist_meta(&session);
        lines.extend(session.project_goal_update());

        // 模型未配置：目标已落库（TS 也是先 setTarget 再 ensureModelReady），
        // 但续跑起不来 → 命令 fail-closed，不伪造一次"已开始"。
        if self.provider.is_none() {
            let final_ = CommandFinal {
                message: Some(
                    "goal set, but no provider is configured so the continuation cannot run"
                        .to_string(),
                ),
                ..CommandFinal::with_reason(
                    CommandStatus::Failed,
                    FAULT_PROVIDER_NOT_CONFIGURED.to_string(),
                )
            };
            settle_admitted(&session, admitted, final_, lines, &self.outbox);
            return;
        }

        let (loop_lines, end) =
            goal_loop::run_goal_loop(self, Arc::clone(&session), LoopStart::ContinueFirst).await;
        lines.extend(loop_lines);
        let final_ = CommandFinal {
            result: Some(json!({
                "type": "sendGoalCommand",
                "targetId": target_id,
                "replaced": replaced,
                "loopEnd": end.as_str(),
            })),
            ..CommandFinal::accepted()
        };
        settle_admitted(&session, admitted, final_, lines, &self.outbox);
    }

    /// `resumeGoal`：paused/（等价幂等）active → 立即续跑。
    pub(crate) async fn run_resume_goal_command(
        self: &Arc<Self>,
        session: Arc<Session>,
        admitted: nc_code_core::inbox::Admitted,
    ) {
        if session.goal().is_none() {
            // 无目标 = 幂等成功（TS："No goal to resume." 不改状态、不抛错）。
            let final_ = CommandFinal {
                result: Some(json!({"type": "resumeGoal", "changed": false})),
                ..CommandFinal::accepted()
            };
            settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
            return;
        }
        if session.collaboration_mode() == "plan" {
            let final_ = CommandFinal {
                message: Some("Plan and Goal cannot be active at the same time.".to_string()),
                ..CommandFinal::with_reason(
                    CommandStatus::Rejected,
                    "guard.planGoalMutuallyExclusive",
                )
            };
            settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
            return;
        }
        // 先检查再写入（TS：不能先写 active 再拒绝，否则留下 active Goal + Plan）。
        if self.provider.is_none() {
            let final_ = CommandFinal {
                message: Some(
                    "goal resumed, but no provider is configured so the continuation cannot run"
                        .to_string(),
                ),
                ..CommandFinal::with_reason(
                    CommandStatus::Failed,
                    FAULT_PROVIDER_NOT_CONFIGURED.to_string(),
                )
            };
            session.update_goal(|record| record.set_status(GoalStatus::Active));
            self.persist_meta(&session);
            let lines = session.project_goal_update();
            settle_admitted(&session, admitted, final_, lines, &self.outbox);
            return;
        }
        session.update_goal(|record| record.set_status(GoalStatus::Active));
        self.persist_meta(&session);
        let mut lines = session.project_goal_update();
        let (loop_lines, end) =
            goal_loop::run_goal_loop(self, Arc::clone(&session), LoopStart::ContinueFirst).await;
        lines.extend(loop_lines);
        let final_ = CommandFinal {
            result: Some(json!({
                "type": "resumeGoal",
                "changed": true,
                "loopEnd": end.as_str(),
            })),
            ..CommandFinal::accepted()
        };
        settle_admitted(&session, admitted, final_, lines, &self.outbox);
    }

    /// `pauseGoal`（控制命令）：手动 CAS → 目标置 paused → 中止在途 turn/裁判。
    ///
    /// WHY 不经 inbox：它必须能打断正在跑的 turn/裁判；排队等 gate 会把"暂停"变成
    /// "等这轮跑完再暂停"，与 TS `activeController?.abort` 的语义相反（同 stop）。
    pub(crate) fn pause_goal_command(
        self: &Arc<Self>,
        session: &Arc<Session>,
        envelope: &CommandEnvelope,
    ) -> crate::RequestOutcome {
        if let Some(ack) = control_cas_ack(session, envelope) {
            self.persist_command_ack(&session.id, &envelope.command_id, &ack);
            return crate::RequestOutcome::empty(ack);
        }
        let revision_at_decision = session.revision();
        let paused = session
            .update_goal(|record| match record.status {
                GoalStatus::Active => {
                    record.set_status(GoalStatus::Paused);
                    true
                }
                _ => false,
            })
            .unwrap_or(false);
        // 目标不存在或不是 active → 幂等成功（TS handler 直接 return undefined）。
        let mut post = Vec::new();
        if paused {
            // 只有在途工作才需要中止；空闲时置 cancel 会留下一个没有消费者的
            // latched stop，把下一次 resume 的续跑立刻掐掉。
            if session.is_running() {
                session.request_cancel();
            }
            self.persist_meta(session);
            // project_goal_update 自己会把 revision 与投影一起前移，不要再手动 bump
            // （双跳不会错，但会让客户端看到一次没有内容的 revision 空洞）。
            post.extend(session.project_goal_update());
            post.extend(session.project_control_update());
        }
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "accepted",
            "revisionAtDecision": revision_at_decision,
        });
        self.persist_command_ack(&session.id, &envelope.command_id, &ack);
        crate::RequestOutcome {
            response: ack,
            post_frames: post,
        }
    }

    /// `resumeGoal` 的提交期检查：运行中拒绝 `activeTurn`（TS 同）。
    pub(crate) fn resume_goal_submit_ack(
        &self,
        session: &Arc<Session>,
        envelope: &CommandEnvelope,
    ) -> Option<crate::RequestOutcome> {
        if !session.is_running() {
            return None;
        }
        let ack = json!({
            "commandId": envelope.command_id,
            "status": "rejected",
            "revisionAtDecision": session.revision(),
            "reasonCode": "activeTurn",
            "message": "Cannot manage goals while a prompt is running",
        });
        self.persist_command_ack(&session.id, &envelope.command_id, &ack);
        Some(crate::RequestOutcome::empty(ack))
    }
}

/// 控制命令的 CAS 校验（inbox 不在这条路径上，所以规则必须与 inbox 一致：
/// 缺字段 → rejected `proto.missingBaseRevision`；不匹配 → stale `proto.staleRevision`）。
fn control_cas_ack(session: &Session, envelope: &CommandEnvelope) -> Option<Value> {
    let Some(base_revision) = envelope.base_revision else {
        return Some(json!({
            "commandId": envelope.command_id,
            "status": "rejected",
            "revisionAtDecision": session.revision(),
            "reasonCode": nc_code_core::protocol::REASON_MISSING_BASE_REVISION,
        }));
    };
    if base_revision != session.revision() {
        return Some(json!({
            "commandId": envelope.command_id,
            "status": "stale",
            "revisionAtDecision": session.revision(),
            "reasonCode": REASON_STALE_REVISION,
        }));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason_of(final_: &CommandFinal) -> &str {
        final_.reason_code.as_deref().unwrap_or("")
    }

    #[test]
    fn submission_validation_rejects_empty_oversized_and_planaligned_goals() {
        let empty = validate_goal_submission(&json!({"text": "  
 "}))
        .expect_err("empty objective must be rejected");
        assert_eq!(reason_of(&empty), "emptyObjective");
        assert_eq!(empty.message.as_deref(), Some("Usage: /goal <objective>"));

        // 4000 是**字符**上限：中文按字符计，不按字节（否则 1334 个汉字就被拒）。
        let at_limit = "字".repeat(MAX_GOAL_OBJECTIVE_CHARS);
        assert!(
            validate_goal_submission(&json!({"text": at_limit})).is_ok(),
            "exactly 4000 chars is allowed"
        );
        let over = "字".repeat(MAX_GOAL_OBJECTIVE_CHARS + 1);
        let rejected =
            validate_goal_submission(&json!({"text": over})).expect_err("oversized rejected");
        assert_eq!(reason_of(&rejected), "fault.command.executionFailed");
        assert!(rejected
            .message
            .as_deref()
            .is_some_and(|message| message.contains("at most 4000 characters")));

        let plan = validate_goal_submission(&json!({"text": "ship", "planEnabled": true}))
            .expect_err("plan+goal is mutually exclusive");
        assert_eq!(reason_of(&plan), "guard.planGoalMutuallyExclusive");

        let ok = validate_goal_submission(&json!({"text": "  ship it  "}))
            .expect("valid objective is trimmed and accepted");
        assert_eq!(ok, "ship it");
    }
}
