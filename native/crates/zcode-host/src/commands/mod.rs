//! admitted 命令的执行面。
//!
//! 划分依据：`lib.rs` 只做**路由**（哪个命令、进哪个桶、是否排队），本目录只做
//! **执行**（状态变更 → 投影 → 收口）。新增一条命令 = 在本目录加一个 spec/函数 +
//! `lib.rs` 一个 match 分支，不再往单个函数里堆第 N 份收口样板。
//!
//! 收口序列（帧投递 → final 合进账本 ack → persist → inbox settle）的唯一所有者是
//! [`settle_admitted`]。任何模块自己写这四步等于制造第二条写入路径，revision 与
//! 账本会随分支顺序漂移。

pub mod child_sessions;
pub mod config;
pub mod feedback;
pub mod file_rewind;
pub mod goal;
pub mod hook_review;
pub mod rerun;
pub mod runs;

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use zcode_core::inbox::Admitted;
use zcode_core::protocol::{merge_final, CommandFinal, CommandStatus};

use crate::session::Session;
use crate::HostServer;

/// 命令路由：一条命令进哪个执行位点。这是 dispatcher 与接线清单的**唯一事实来源**
/// ——`HostServer::command` 只按本枚举分派，测试据此判定"哪些 v4 命令已接线"。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Route {
    /// 会话外操作（TS 的 global bucket）：不经 session inbox。
    CreateSession,
    /// 控制命令：即时生效。不能排 inbox FIFO——排在正在跑的 turn 后面会死锁。
    Stop,
    /// 会话级元数据命令：直接生效，无 turn 副作用。
    RenameSession,
    /// 关闭会话（TS `deleteSession` = `closeSession`，非破坏性）。
    DeleteSession,
    /// 交互应答：同步生效（turn 正在等这个 oneshot，排队会自锁）。
    ResolveInteraction,
    SnoozeInteraction,
    /// 队列变更：同步生效（busy 时排 inbox 会被当前 turn 阻塞）。
    QueueMutation(QueueMutation),
    /// 按 `inputRouting` 裁决：busy/有积压 → 入队，否则走 admission。
    SendText,
    /// sendGoalCommand：与 SendText 同样的入队裁决，但执行器是 goal 循环。
    SendGoalCommand,
    /// pauseGoal：控制命令，必须能打断在途 turn/裁判（同 stop 的理由）。
    PauseGoal,
    /// resumeGoal：运行中直接拒绝（`activeTurn`），否则走 admission 执行续跑。
    ResumeGoal,
    /// 其余命令走 session inbox 串行 admission，执行器由 [`plan`] 决定。
    Admitted,
    /// 协议里有这条命令，但本 host 未接线：仍进 inbox 取得可回放的 ack，
    /// 再 fail-closed 回 `fault.command.unsupported:<type>`（绝不静默成功）。
    Unsupported,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum QueueMutation {
    Edit,
    Reorder,
    Delete,
    SendNow,
}

/// 命令类型 → 路由位点。协议改名或新增命令时，`V4_COMMAND_TYPES` 清单测试会立刻
/// 暴露"有 route 却没登记"或"有命令却既没 route 也没在未接线清单里"。
pub(crate) fn route(command_type: &str) -> Route {
    match command_type {
        "createSession" => Route::CreateSession,
        "stop" => Route::Stop,
        "renameSession" => Route::RenameSession,
        "deleteSession" => Route::DeleteSession,
        "resolveInteraction" => Route::ResolveInteraction,
        "snoozeInteractionAutoResolution" => Route::SnoozeInteraction,
        "editQueueItem" => Route::QueueMutation(QueueMutation::Edit),
        "reorderQueueItem" => Route::QueueMutation(QueueMutation::Reorder),
        "deleteQueueItem" => Route::QueueMutation(QueueMutation::Delete),
        "sendQueuedNow" => Route::QueueMutation(QueueMutation::SendNow),
        "sendText" => Route::SendText,
        "sendGoalCommand" => Route::SendGoalCommand,
        "pauseGoal" => Route::PauseGoal,
        "resumeGoal" => Route::ResumeGoal,
        "compact"
        | "setAutoDrain"
        | "setFollowupMode"
        | "switchModelConfig"
        | "switchCollaborationMode"
        | "setAssistantFeedback"
        | "retryTurn"
        | "editUserQuery"
        | "applyFileRewind"
        | "forkAssistant"
        | "createSelectionSideSession"
        | "respondWorkspaceHookReview"
        | "toggleWorkspaceHookReviewItem"
        | "revokeWorkspaceHookTrust"
        | "requestWorkspaceHookReview" => Route::Admitted,
        _ => Route::Unsupported,
    }
}

/// v4 命令全集，逐项对齐 `packages/shared/src/zcode-protocol-v4/command.ts`
/// 的 `commandPayloadSchemas` keys。清单存在是为了让"还剩多少没接线"成为可测试
/// 的事实（见 `command_inventory_is_the_exact_complement_of_the_v4_surface`），
/// 而不是 README 里的一句口头承诺。
#[cfg(test)]
pub(crate) const V4_COMMAND_TYPES: &[&str] = &[
    "createSession",
    "createSelectionSideSession",
    "sendText",
    "sendGoalCommand",
    "stop",
    "compact",
    "forkAssistant",
    "applyFileRewind",
    "editUserQuery",
    "retryTurn",
    "setAssistantFeedback",
    "sendQueuedNow",
    "editQueueItem",
    "reorderQueueItem",
    "deleteQueueItem",
    "setAutoDrain",
    "resolveInteraction",
    "respondWorkspaceHookReview",
    "toggleWorkspaceHookReviewItem",
    "revokeWorkspaceHookTrust",
    "requestWorkspaceHookReview",
    "snoozeInteractionAutoResolution",
    "switchModelConfig",
    "switchCollaborationMode",
    "setFollowupMode",
    "pauseGoal",
    "resumeGoal",
    "cancelBackgroundWork",
    "resumeWorkflowRun",
    "startSavedWorkflow",
    "amendWorkflowRunSettings",
    "renameSession",
    "deleteSession",
    "discardSharedContext",
];

/// 尚未接线的 v4 命令 —— 每条必须写明**缺哪个子系统**。这张表不是注释：
/// `unsupported_final` 会把它作为 ack `message` 回给客户端，所以清单过期会直接
/// 变成用户可见的错误文案，而不是无人读的死数据。
/// 接线一条命令时同步删除对应条目。
pub(crate) const UNROUTED_COMMANDS: &[(&str, &str)] = &[
    (
        "cancelBackgroundWork",
        "dynamic workflow run 生命周期（specs/workflow-capability-boundary.md）",
    ),
    (
        "resumeWorkflowRun",
        "dynamic workflow run 生命周期（specs/workflow-capability-boundary.md）",
    ),
    (
        "startSavedWorkflow",
        "saved workflow 解析与编译（specs/workflow-capability-boundary.md）",
    ),
    (
        "amendWorkflowRunSettings",
        "dynamic workflow run 配置修订端口",
    ),
    ("discardSharedContext", "shared context provenance 存储"),
];

/// 子会话命令的两种形态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ChildSessionKind {
    Fork,
    SideSession,
}

/// 一条 admitted 命令用哪个执行器跑。路由表只在这里声明一次；
/// `lib.rs` 不再关心命令语义，未列出的命令类型由 [`Plan::Unsupported`] 兜住。
pub(crate) enum Plan {
    /// 需要跑模型轮次的命令。
    Turn(runs::TurnKind),
    /// 配置类命令的声明式 spec。
    Config(&'static config::ConfigCommand),
    /// 行级反馈命令（唯一会执行失败的 admitted 小命令）。
    Feedback,
    /// 分支重放命令（editUserQuery / retryTurn）：截断最后一轮再重跑。
    Rerun(rerun::RerunKind),
    /// 工作区文件回滚（applyFileRewind）：撤回本轮文件改动，不动聊天历史。
    FileRewind,
    /// 子会话命令（forkAssistant / createSelectionSideSession）。
    ChildSession(ChildSessionKind),
    /// sendGoalCommand：写入/替换目标并启动续跑循环。
    GoalSet,
    /// resumeGoal：paused → active 并立即续跑。
    GoalResume,
    /// workspace hook 评审命令组。
    HookReview(hook_review::HookReviewKind),
    /// 本 host 尚未接线的命令：fail-closed，绝不静默执行。
    Unsupported,
}

/// 命令类型 → 执行器。配置族按注册表 `name` 查表，其余各族显式列出；
/// 未列出的命令类型只能落到 [`Plan::Unsupported`]，因此不存在"忘了登记却静默成功"。
pub(crate) fn plan(command_type: &str) -> Plan {
    match command_type {
        "sendText" => Plan::Turn(runs::TurnKind::Text),
        "compact" => Plan::Turn(runs::TurnKind::Compact),
        "setAssistantFeedback" => Plan::Feedback,
        "retryTurn" => Plan::Rerun(rerun::RerunKind::Retry),
        "editUserQuery" => Plan::Rerun(rerun::RerunKind::Edit),
        "applyFileRewind" => Plan::FileRewind,
        "forkAssistant" => Plan::ChildSession(ChildSessionKind::Fork),
        "createSelectionSideSession" => Plan::ChildSession(ChildSessionKind::SideSession),
        "sendGoalCommand" => Plan::GoalSet,
        "resumeGoal" => Plan::GoalResume,
        "respondWorkspaceHookReview" => Plan::HookReview(hook_review::HookReviewKind::Respond),
        "toggleWorkspaceHookReviewItem" => Plan::HookReview(hook_review::HookReviewKind::Toggle),
        "revokeWorkspaceHookTrust" => Plan::HookReview(hook_review::HookReviewKind::Revoke),
        "requestWorkspaceHookReview" => Plan::HookReview(hook_review::HookReviewKind::Request),
        other => match config::spec_for(other) {
            Some(spec) => Plan::Config(spec),
            None => Plan::Unsupported,
        },
    }
}

/// admitted 命令的唯一执行入口（由 session inbox admission 之后派生的任务调用）。
/// `drain_after=false` 只给队列 drain 用：它自己就是 drain 循环，turn 结束后再 drain
/// 会递归成"每排一项多一层调用栈"。
pub(crate) async fn execute(server: &Arc<HostServer>, session: Arc<Session>, admitted: Admitted) {
    execute_with(server, session, admitted, true).await
}

pub(crate) async fn execute_with(
    server: &Arc<HostServer>,
    session: Arc<Session>,
    admitted: Admitted,
    drain_after: bool,
) {
    let command_type = admitted.envelope.command_type.clone();
    match plan(&command_type) {
        Plan::Turn(kind) => {
            server
                .run_turn_command(session, admitted, kind, drain_after)
                .await
        }
        Plan::Config(spec) => server.run_config_command(spec, session, admitted).await,
        Plan::Feedback => server.run_assistant_feedback(session, admitted),
        Plan::Rerun(kind) => server.run_turn_rerun(session, admitted, kind).await,
        Plan::FileRewind => server.run_apply_file_rewind(session, admitted),
        Plan::ChildSession(ChildSessionKind::Fork) => server.run_fork_assistant(session, admitted),
        Plan::ChildSession(ChildSessionKind::SideSession) => {
            server
                .run_create_selection_side_session(session, admitted)
                .await;
        }
        Plan::GoalSet => server.run_send_goal_command(session, admitted).await,
        Plan::GoalResume => server.run_resume_goal_command(session, admitted).await,
        Plan::HookReview(kind) => server.run_hook_review_command(&session, admitted, kind),
        Plan::Unsupported => {
            let final_ = unsupported_final(&command_type);
            settle_admitted(&session, admitted, final_, Vec::new(), &server.outbox);
        }
    }
}

/// admitted 命令的统一收口：投递投影帧，把 final 合入命令账本，再释放 inbox 槽位。
///
/// WHY 顺序固定：帧必须在 settle 之前投递——settle 会唤醒排在同一个桶后面的下一条
/// 命令，反过来让后一条的帧先落地，客户端就会看到乱序投影。
pub(crate) fn settle_admitted(
    session: &Arc<Session>,
    admitted: Admitted,
    final_: CommandFinal,
    lines: Vec<String>,
    outbox: &UnboundedSender<String>,
) {
    for line in lines {
        let _ = outbox.send(line);
    }
    let merged = merge_final(&admitted.ack, &final_);
    let merged_value = serde_json::to_value(&merged).unwrap_or(Value::Null);
    if let Err(error) = session.persist_ack(&admitted.envelope.command_id, &merged_value) {
        eprintln!(
            "[zcode-host] persist_ack failed for {}: {error}",
            admitted.envelope.command_id
        );
    }
    let _ = session.inbox.settle(admitted.ticket, final_);
}

/// 未接线命令缺的子系统（来自 [`UNROUTED_COMMANDS`]）。
pub(crate) fn missing_subsystem(command_type: &str) -> &'static str {
    UNROUTED_COMMANDS
        .iter()
        .find(|(name, _)| *name == command_type)
        .map(|(_, reason)| *reason)
        .unwrap_or("本 host 未实现该命令")
}

/// 未接线命令的 fail-closed final：绝不静默成功，也不猜测语义。
/// reasonCode 带命令名（客户端按前缀反查文案），message 带缺失的子系统，
/// 让"为什么不支持"成为可诊断的事实。
pub(crate) fn unsupported_final(command_type: &str) -> CommandFinal {
    CommandFinal {
        message: Some(format!(
            "zcode-host 未接线 {command_type}：缺少{}。",
            missing_subsystem(command_type)
        )),
        ..CommandFinal::with_reason(
            CommandStatus::Failed,
            format!("fault.command.unsupported:{command_type}"),
        )
    }
}

/// payload 字符串字段：缺省回落到调用方给出的默认值（与 TS 的解构默认一致）。
pub(crate) fn payload_str<'a>(payload: &'a Value, key: &str, default: &'a str) -> &'a str {
    payload.get(key).and_then(Value::as_str).unwrap_or(default)
}

pub(crate) fn payload_bool(payload: &Value, key: &str) -> bool {
    payload.get(key).and_then(Value::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;

    /// 接线清单必须是 v4 命令全集的**精确补集**：
    /// - 全集里的每条命令要么有 route，要么在 `UNROUTED_COMMANDS` 里写明缺哪个子系统；
    /// - 已接线的命令不能再留在未接线清单里（接线后必须删条目）；
    /// - 两边都不许出现清单外的名字（协议改名要立刻暴露，而不是悄悄变成死键）。
    #[test]
    fn command_inventory_is_the_exact_complement_of_the_v4_surface() {
        let unrouted: BTreeSet<&str> = UNROUTED_COMMANDS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            unrouted.len(),
            UNROUTED_COMMANDS.len(),
            "duplicate entry in UNROUTED_COMMANDS"
        );
        for &name in V4_COMMAND_TYPES {
            let documented = unrouted.contains(name);
            let routed = route(name) != Route::Unsupported;
            assert_eq!(
                routed, !documented,
                "{name}: routed={routed} listed_unrouted={documented} — the inventory must be an exact complement"
            );
            if documented {
                let reason = UNROUTED_COMMANDS
                    .iter()
                    .find(|(entry, _)| *entry == name)
                    .map(|(_, reason)| *reason)
                    .unwrap_or_default();
                assert!(
                    !reason.is_empty(),
                    "{name} is unrouted without a missing-subsystem reason"
                );
            }
        }
        for (name, _) in UNROUTED_COMMANDS {
            assert!(
                V4_COMMAND_TYPES.contains(name),
                "UNROUTED_COMMANDS lists {name}, which is not in the v4 command surface"
            );
        }
    }

    /// `Route::Admitted` 的承诺必须兑现：路由说"进 inbox 执行"的命令，`plan` 必须有
    /// 真执行器。否则执行期会落到 `Plan::Unsupported`，同一个命令在能力面显示 allowed、
    /// 在执行面却 fail-closed —— 那正是能力面撒谎的那类 bug。
    #[test]
    fn every_admitted_route_has_an_executor() {
        for &name in V4_COMMAND_TYPES {
            if route(name) == Route::Admitted {
                assert!(
                    !matches!(plan(name), Plan::Unsupported),
                    "{name} is routed Admitted but has no executor"
                );
            }
        }
        // 未接线与拼错的命令类型只能落到 Unsupported；不存在"未知即执行"。
        // pauseGoal 是控制命令（不经 execute），执行器表里刻意没有它。
        assert!(matches!(plan("pauseGoal"), Plan::Unsupported));
        assert!(matches!(plan("cancelBackgroundWork"), Plan::Unsupported));
        assert!(matches!(plan("definitelyNotACommand"), Plan::Unsupported));
    }

    /// 未接线命令的 reasonCode 必须带命令名（客户端按前缀反查文案），message 必须
    /// 带上缺失的子系统（否则"不支持"是个无法诊断的黑箱）。
    #[test]
    fn unsupported_final_names_the_command_and_its_missing_subsystem() {
        let final_ = unsupported_final("cancelBackgroundWork");
        assert_eq!(final_.status, CommandStatus::Failed);
        assert_eq!(
            final_.reason_code.as_deref(),
            Some("fault.command.unsupported:cancelBackgroundWork")
        );
        let message = final_.message.expect("unsupported ack must explain why");
        assert!(
            message.contains("dynamic workflow"),
            "message should surface the documented gap, got: {message}"
        );
        // 清单外的命令名也要有兜底文案，不能 panic 或空 message。
        assert!(unsupported_final("notACommand")
            .message
            .is_some_and(|m| m.contains("notACommand")));
    }

    #[test]
    fn payload_helpers_fall_back_to_the_caller_default() {
        let payload = json!({"mode": "guide", "autoDrain": true});
        assert_eq!(payload_str(&payload, "mode", "queue"), "guide");
        assert_eq!(payload_str(&payload, "provider", ""), "");
        assert!(payload_bool(&payload, "autoDrain"));
        assert!(!payload_bool(&payload, "planEnabled"));
    }
}
