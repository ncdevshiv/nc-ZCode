//! 子会话命令：`forkAssistant`（带内容的对话分叉）与
//! `createSelectionSideSession`（继承运行配置的空副屏，可带首条输入）。
//!
//! 两条命令共用 [`crate::sessions::SessionSeed`] 这一条创建路径，差别只有 seed 内容：
//! - fork 复制父会话到目标行为止的行集与模型历史，**不动父会话、不 stop 父 turn、
//!   不接管工作区**（TS 注释原文：conversation-only fork；legacy `forkSession` 的
//!   ensureNoActiveTurn + workspace rewind 不在这条路径上）。
//! - side session 只继承父的运行配置，内容为空；`firstInput` 存在时把**同一条命令
//!   身份**指向 child 去 admission，父的 inbox/队列因此不被污染。

use std::sync::Arc;

use serde_json::{json, Value};
use zcode_core::inbox::{Admitted, Outcome};
use zcode_core::protocol::{CommandFinal, CommandStatus};

use crate::branch::{self, ForkFault};
use crate::commands::settle_admitted;
use crate::session::Session;
use crate::sessions::SessionSeed;
use crate::turn;
use crate::HostServer;

fn target_of(payload: &Value) -> (Option<u64>, Option<String>) {
    let target = payload.get("target");
    (
        target
            .and_then(|target| target.get("rowId"))
            .and_then(Value::as_u64),
        target
            .and_then(|target| target.get("entityId"))
            .and_then(Value::as_str)
            .map(str::to_string),
    )
}

fn rejected(fault: ForkFault, row_id: u64) -> CommandFinal {
    CommandFinal {
        message: Some(format!(
            "forkAssistant 目标 rowId {row_id} 不是可用的稳定分叉点：{}",
            fault.explain()
        )),
        ..CommandFinal::with_reason(CommandStatus::Rejected, fault.reason_code())
    }
}

impl HostServer {
    pub(crate) fn run_fork_assistant(self: &Arc<Self>, parent: Arc<Session>, admitted: Admitted) {
        let payload = admitted.envelope.payload.clone();
        let (Some(row_id), entity) = target_of(&payload) else {
            settle_admitted(
                &parent,
                admitted,
                rejected(ForkFault::TargetNotStable, 0),
                Vec::new(),
                &self.outbox,
            );
            return;
        };
        let history = parent.history_snapshot().unwrap_or_default();
        let boundary = branch::fork_target(
            &parent.rows_snapshot(),
            &parent.turn_history_snapshot(),
            history.len(),
            row_id,
            entity.as_deref(),
        );
        let boundary = match boundary {
            Ok(boundary) => boundary,
            Err(fault) => {
                settle_admitted(
                    &parent,
                    admitted,
                    rejected(fault, row_id),
                    Vec::new(),
                    &self.outbox,
                );
                return;
            }
        };
        let (rows, copied_history) =
            parent.conversation_copy(boundary.row_count, boundary.history_end);
        // 台账**不**跟着复制：fork 只接管对话，父会话在共享 workspace 里写下的文件
        // 仍归父会话回滚。child 带着父的文件记录去 rewind 就会删掉父正在用的文件。
        // 目标（goal）**跟随 fork**（TS `cloneTargetForFork`）：子会话继承目标身份与
        // 裁判账本，运行态（在途裁判/计时）由 `clone_for_fork` 清掉。
        let seed = SessionSeed {
            rows,
            history: copied_history,
            goal: parent.goal().map(|goal| goal.clone_for_fork()),
            ..SessionSeed::from_parent(&parent)
        };
        let child = match self.open_session(seed) {
            Ok(child) => child,
            Err(error) => {
                let final_ = CommandFinal {
                    message: Some(format!("forkAssistant 创建子会话失败：{}", error.message)),
                    ..CommandFinal::with_reason(
                        CommandStatus::Failed,
                        "fault.command.executionFailed",
                    )
                };
                settle_admitted(&parent, admitted, final_, Vec::new(), &self.outbox);
                return;
            }
        };
        let final_ = CommandFinal {
            result: Some(json!({
                "type": "forkAssistant",
                "sessionId": child.id,
                "parentSessionId": parent.id,
                "boundaryTurnId": boundary.turn_id,
                "copiedRows": boundary.row_count,
                "copiedHistory": boundary.history_end,
            })),
            ..CommandFinal::accepted()
        };
        // fork 不改父会话的任何投影，因此不推进父 revision（客户端拿到的父快照
        // 与 fork 之前完全一致，这是 TS 的"不 stop parent"的投影侧对应）。
        settle_admitted(&parent, admitted, final_, Vec::new(), &self.outbox);
    }

    pub(crate) async fn run_create_selection_side_session(
        self: &Arc<Self>,
        parent: Arc<Session>,
        admitted: Admitted,
    ) {
        let payload = admitted.envelope.payload.clone();
        let first_text = payload
            .get("firstInput")
            .and_then(|first| first.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let child = match self.open_session(SessionSeed::from_parent(&parent)) {
            Ok(child) => child,
            Err(error) => {
                let final_ = CommandFinal {
                    message: Some(format!(
                        "createSelectionSideSession 创建子会话失败：{}",
                        error.message
                    )),
                    ..CommandFinal::with_reason(
                        CommandStatus::Failed,
                        "fault.command.executionFailed",
                    )
                };
                settle_admitted(&parent, admitted, final_, Vec::new(), &self.outbox);
                return;
            }
        };
        let Some(text) = first_text else {
            let final_ = CommandFinal {
                result: Some(json!({
                    "type": "createSelectionSideSession",
                    "sessionId": child.id,
                    "parentSessionId": parent.id,
                })),
                ..CommandFinal::accepted()
            };
            settle_admitted(&parent, admitted, final_, Vec::new(), &self.outbox);
            return;
        };
        // 首条输入进 **child 自己的 session_input 账本**（TS 的 admitInputCommand
        // 指向 childSessionId），所以 child 的 commands/query 能查到这条命令的终态；
        // 父会话的队列与运行态完全不受影响。
        // CAS 字段在这里清掉：child 是刚创建的会话，revision 从 0 起，带着父会话的
        // baseRevision 过去必然 stale-by-construction。
        let admitted_envelope_command_id = admitted.envelope.command_id.clone();
        let mut child_envelope = admitted.envelope.clone();
        child_envelope.session_id = Some(child.id.clone());
        child_envelope.base_revision = None;
        child_envelope.base_log_epoch = None;
        if let Value::Object(map) = &mut child_envelope.payload {
            map.insert("text".to_string(), json!(text));
        }
        let child_inbox = child.inbox.clone();
        // admission 可能阻塞在会话 gate 上（与 stdin 路径同一个理由），放 blocking 池。
        let outcome =
            tokio::task::spawn_blocking(move || child_inbox.handle_envelope(child_envelope))
                .await
                .map_err(|error| error.to_string());
        let (result_input, parent_final) = match outcome.unwrap_or_else(|error| {
            // admission 任务本身失败（运行时关闭等）：按拒绝处理，父命令仍拿到确定的终态。
            Outcome::Ack(zcode_core::protocol::CommandAck {
                command_id: admitted_envelope_command_id.clone(),
                status: CommandStatus::Failed,
                reason_code: Some("fault.command.executionFailed".to_string()),
                message: Some(error),
                revision_at_decision: 0,
                result: None,
                memory_enabled: None,
            })
        }) {
            Outcome::Execute(child_admitted) => {
                let server = Arc::clone(self);
                let child_for_turn = Arc::clone(&child);
                let provider = self.provider.clone();
                let outbox = self.outbox.clone();
                tokio::spawn(async move {
                    let lines = turn::run_turn(
                        child_for_turn,
                        provider.as_ref(),
                        &server.turn_deps,
                        *child_admitted,
                    )
                    .await;
                    for line in lines {
                        let _ = outbox.send(line);
                    }
                });
                (
                    json!({"delivery": "startNow", "inputId": admitted.envelope.command_id}),
                    CommandStatus::Accepted,
                )
            }
            Outcome::Ack(ack) => (
                json!({"delivery": "queue", "inputId": admitted.envelope.command_id}),
                ack.status,
            ),
        };
        let final_ = CommandFinal {
            result: Some(json!({
                "type": "createSelectionSideSession",
                "sessionId": child.id,
                "parentSessionId": parent.id,
                "input": result_input,
            })),
            ..match parent_final {
                CommandStatus::Accepted => CommandFinal::accepted(),
                status => CommandFinal::with_reason(status, "fault.command.inputRejected"),
            }
        };
        settle_admitted(&parent, admitted, final_, Vec::new(), &self.outbox);
    }
}
