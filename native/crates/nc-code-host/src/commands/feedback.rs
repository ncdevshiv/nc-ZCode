//! `setAssistantFeedback`：对稳定 assistant row 的 like / dislike / 清除。
//!
//! 单独成模块而不是塞进配置 spec 表：它是唯一会**执行失败**的 admitted 小命令，
//! 行不存在时必须 fail-closed 回 `fault.command.executionFailed`，而不是假装 accepted。

use std::sync::Arc;

use nc_code_core::inbox::Admitted;
use nc_code_core::protocol::{CommandFinal, CommandStatus};

use crate::commands::settle_admitted;
use crate::session::Session;
use crate::HostServer;

impl HostServer {
    pub(crate) fn run_assistant_feedback(
        self: &Arc<Self>,
        session: Arc<Session>,
        admitted: Admitted,
    ) {
        let payload = admitted.envelope.payload.clone();
        // target 形状 = conversationRowTargetSchema：只认 rowId（logEpoch 由 session 自己校验）。
        let row_id = payload
            .get("target")
            .and_then(|target| target.get("rowId"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        // payload.feedback 是 nullable enum：null → 清除既有反馈。
        let feedback = payload
            .get("feedback")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let (final_, lines) = match session.apply_assistant_feedback(row_id, feedback.as_deref()) {
            Ok(lines) => (CommandFinal::accepted(), lines),
            Err(_) => (
                CommandFinal::with_reason(CommandStatus::Failed, "fault.command.executionFailed"),
                Vec::new(),
            ),
        };
        settle_admitted(&session, admitted, final_, lines, &self.outbox);
    }
}
