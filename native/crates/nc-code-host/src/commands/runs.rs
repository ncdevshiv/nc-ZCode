//! turn 型 admitted 命令：需要跑模型轮次的命令族。
//!
//! Plug 点：`TurnKind` 的一个 variant = 一个 `turn::run_*` 入口。命令的收口、
//! 帧投递、turn 结束后提升队首这些共性只写一次；不同命令只声明"用哪个 runner"。

use std::sync::Arc;

use nc_code_core::inbox::Admitted;

use crate::goal::GoalStatus;
use crate::goal_loop::{self, LoopStart};
use crate::session::Session;
use crate::store::now_ms;
use crate::turn;
use crate::HostServer;

/// turn 型命令的执行入口选择。
pub(crate) enum TurnKind {
    /// `sendText`：一次完整用户输入 → 模型轮 → 工具循环。
    Text,
    /// `compact`：输入型维护命令，压缩历史但不改会话生命周期语义。
    Compact,
}

impl TurnKind {
    fn is_user_prompt(&self) -> bool {
        matches!(self, TurnKind::Text)
    }
}

impl HostServer {
    /// turn 型命令的统一执行序列：跑 runner → 投递帧 → 收口 → 提升队首。
    ///
    /// WHY drain 在最后：runner 内部已经 settle（释放 inbox 槽位并把 phase 收回
    /// idle），此时才允许队首以 `sendQueuedNow` 语义进入下一条 turn；提前 drain 会
    /// 让两条 turn 并行，破坏 session 串行 admission 的不变式。
    pub(crate) async fn run_turn_command(
        self: &Arc<Self>,
        session: Arc<Session>,
        admitted: Admitted,
        kind: TurnKind,
        drain_after: bool,
    ) {
        let session_for_drain = Arc::clone(&session);
        let provider = self.provider.clone();
        let ttft_command_id = admitted.envelope.command_id.clone();
        self.ttft_mark_execution(&ttft_command_id, &admitted.envelope.command_id);
        // 目标记账（TS startTargetTurnAccounting）：目标 active 时把本轮计入其运行时长。
        let accounted = self.start_goal_turn_accounting(&session);
        let lines = match kind {
            TurnKind::Text => {
                turn::run_turn(
                    Arc::clone(&session),
                    provider.as_ref(),
                    &self.turn_deps,
                    admitted,
                )
                .await
            }
            TurnKind::Compact => {
                turn::run_compact(
                    Arc::clone(&session),
                    provider.as_ref(),
                    &self.turn_deps,
                    admitted,
                )
                .await
            }
        };
        for line in lines {
            let _ = self.outbox.send(line);
        }
        let is_user_prompt = kind.is_user_prompt();
        // Stop hook 的续跑发生在 turn 内部（`run_agent_loop` 的模型终态边界），
        // 与 TS `finishModelStepWithoutToolCalls` 同位置；这里不需要额外处理。
        // 轮尾：先收口本轮记账，再按 `continueActiveTargetAfterTurn` 决定是否自动续跑。
        for line in self
            .finish_goal_turn(&session, accounted, is_user_prompt)
            .await
        {
            let _ = self.outbox.send(line);
        }
        self.ttft_mark_terminal(&ttft_command_id, "completed");
        if drain_after {
            self.drain_queue(Arc::clone(&session_for_drain)).await;
        }
        // A background task may have settled during this turn: deliver its
        // `backgroundResult` turn now that the session is idle again.
        self.schedule_background_result_drain(session_for_drain);
    }

    /// 用户轮开始时的目标记账（只有 active 目标才计时；TS `where status='active'`）。
    pub(crate) fn start_goal_turn_accounting(&self, session: &Arc<Session>) -> bool {
        if !session
            .goal()
            .is_some_and(|goal| goal.status == GoalStatus::Active)
        {
            return false;
        }
        let started_at = now_ms();
        session.update_goal(|record| record.start_run(started_at));
        true
    }

    /// 轮尾：累计运行时长、落盘，并在用户轮成功后按 goal 规则自动续跑。
    ///
    /// `continue_goal` = TS `continueActiveTargetAfterTurn`（用户提交的 prompt 才有；
    /// compact 之类的维护命令不触发续跑）。
    pub(crate) async fn finish_goal_turn(
        self: &Arc<Self>,
        session: &Arc<Session>,
        accounted: bool,
        continue_goal: bool,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        if accounted {
            session.update_goal(|record| record.finish_run(now_ms()));
            self.persist_meta(session);
            lines.extend(session.project_goal_update());
        }
        let goal_active = session
            .goal()
            .is_some_and(|goal| goal.status == GoalStatus::Active);
        if continue_goal && goal_active && session.phase() == "completedSuccess" {
            let (goal_lines, _) =
                goal_loop::run_goal_loop(self, Arc::clone(session), LoopStart::VerifyFirst).await;
            lines.extend(goal_lines);
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn basic_test() {
        assert!(true);
    }
}
