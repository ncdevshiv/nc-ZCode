//! `editUserQuery` / `retryTurn`：分支截断 + 以给定文本重放一轮。
//!
//! 两条命令是同一件事的两种文本来源（TS 注释原文：editUserQuery = 换文本的
//! retryTurn），所以执行序列只有一份：定位目标轮 → 守卫 → 解析文本 → 截断
//! （磁盘先、内存后）→ 投 `row.removed` → 重放一轮。
//!
//! 与 TS 的一处**记录在案的差异**：TS 在 turn 运行中会 preempt 当前轮再截断；
//! 本 host 的这两条命令走 session inbox 串行 admission，busy 时排在当前 turn 之后
//! 执行。差异是安全的（截断永远发生在已收口的状态上，守卫判定的"最后一轮"不会与
//! 在途投影竞争），但确实与桌面的"立即打断"手感不同，记在 crate README 差异清单。

use std::sync::Arc;

use nc_code_core::inbox::Admitted;
use nc_code_core::protocol::{CommandFinal, CommandStatus};
use serde_json::{json, Value};

use crate::branch::{self, BranchFault};
use crate::commands::settle_admitted;
use crate::session::Session;
use crate::turn;
use crate::HostServer;

/// 分支重放的文本来源。
pub(crate) enum RerunKind {
    /// `retryTurn`：重放该轮**原** user prompt（截断后行集里已经没有它）。
    Retry,
    /// `editUserQuery`：重放 payload.newText。
    Edit,
}

/// 把 admitted 的输入文本换成解析出来的 prompt，其余身份字段（commandId、
/// ack、ticket、admissionSeq）原样保留：重放轮仍由**这条命令**负责收口，
/// 不能伪造一个新的 commandId 让账本对不上。
fn with_text(mut admitted: Admitted, text: &str) -> Admitted {
    if let Value::Object(map) = &mut admitted.envelope.payload {
        map.insert("text".to_string(), json!(text));
    }
    admitted
}

fn target_entity_id(payload: &Value) -> Option<&str> {
    payload
        .get("target")
        .and_then(|target| target.get("entityId"))
        .and_then(Value::as_str)
}

fn target_row_id(payload: &Value) -> Option<u64> {
    payload
        .get("target")
        .and_then(|target| target.get("rowId"))
        .and_then(Value::as_u64)
}

impl HostServer {
    pub(crate) async fn run_turn_rerun(
        self: &Arc<Self>,
        session: Arc<Session>,
        admitted: Admitted,
        kind: RerunKind,
    ) {
        let command_type = admitted.envelope.command_type.clone();
        let payload = admitted.envelope.payload.clone();
        let Some(row_id) = target_row_id(&payload) else {
            let final_ = CommandFinal::with_reason(
                CommandStatus::Failed,
                BranchFault::TargetUnresolvable.reason_code(),
            );
            settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
            return;
        };
        let rows = session.rows_snapshot();
        let history_starts = session.turn_history_snapshot();
        let entity_id = target_entity_id(&payload);
        let anchor = match kind {
            RerunKind::Retry => branch::retry_target(&rows, &history_starts, row_id, entity_id),
            RerunKind::Edit => branch::edit_target(&rows, &history_starts, row_id, entity_id),
        };
        let anchor = match anchor {
            Ok(anchor) => anchor,
            Err(fault) => {
                let final_ = CommandFinal {
                    message: Some(format!(
                        "{command_type} 目标 rowId {row_id} 被分支守卫拒绝：{}",
                        fault_reason(fault)
                    )),
                    ..CommandFinal::with_reason(CommandStatus::Failed, fault.reason_code())
                };
                settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
                return;
            }
        };
        // 文本必须在截断之前解析完：截断后 userInput 行已经不在了。
        let text = match kind {
            RerunKind::Retry => anchor.user_text.clone().unwrap_or_default(),
            RerunKind::Edit => payload
                .get("newText")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
        let has_attachments = payload
            .get("attachments")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty());
        if let Err(fault) = branch::require_prompt_input(&text, has_attachments) {
            let final_ = CommandFinal {
                message: Some(format!("{command_type} 输入为空：正文与附件都没有内容")),
                ..CommandFinal::with_reason(CommandStatus::Failed, fault.reason_code())
            };
            settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
            return;
        }
        // 附件本体仍未接线（host 不解析 attachmentRef → TurnAttachment）：既然
        // 正文可以独立成立，就按"附件被忽略"出声，而不是静默丢。
        if has_attachments {
            eprintln!(
                "[zcode-host] {command_type}: attachments are not rendered by this host (ignored)"
            );
        }
        let history_keep = anchor.history_start.unwrap_or_default();
        if let Err(error) = session.truncate_branch(anchor.header_row_id, history_keep) {
            // 截断写失败 = 行集与历史都可能已经分叉，不能继续重放。
            let final_ = CommandFinal {
                message: Some(format!("{command_type} 分支截断落盘失败：{error}")),
                ..CommandFinal::with_reason(CommandStatus::Failed, "fault.command.executionFailed")
            };
            settle_admitted(&session, admitted, final_, Vec::new(), &self.outbox);
            return;
        }
        session.bump_revision();
        let removed_lines = session.project_rows_removed(anchor.header_row_id);
        for line in removed_lines {
            let _ = self.outbox.send(line);
        }
        // 重放：turn::run_turn 负责投影、模型轮与最终 settle（origin 标成 editRerun）。
        // 重放轮也是用户轮：目标记账与轮后自动续跑与 sendText 同规则。
        let accounted = self.start_goal_turn_accounting(&session);
        let provider = self.provider.clone();
        let lines = turn::run_turn(
            Arc::clone(&session),
            provider.as_ref(),
            &self.turn_deps,
            with_text(admitted, &text),
        )
        .await;
        for line in lines {
            let _ = self.outbox.send(line);
        }
        for line in self.finish_goal_turn(&session, accounted, true).await {
            let _ = self.outbox.send(line);
        }
        self.drain_queue(session).await;
    }
}

/// 守卫失败的可读原因（进 ack.message，客户端按 reasonCode 反查 i18n 文案）。
fn fault_reason(fault: BranchFault) -> &'static str {
    match fault {
        BranchFault::TargetUnresolvable => "目标行无法解析到某一轮",
        BranchFault::RetryTargetNotLatest => "retryTurn 只能作用于最后一轮的 assistant 回复",
        BranchFault::EditTargetNotLatest => "editUserQuery 只能作用于最后一轮的 real user query",
        BranchFault::HistoryAnchorMissing => "该轮缺少历史锚点（旧 transcript），无法安全截断",
        BranchFault::EmptyInput => "正文与附件都为空",
    }
}
