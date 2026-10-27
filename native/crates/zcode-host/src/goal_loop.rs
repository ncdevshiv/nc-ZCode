//! goal 自动续跑循环（TS `runActiveTargetContinuationLoop` 的 Rust 版）。
//!
//! 循环的角色：把"一轮工作 → 裁判 → 再一轮"串成串行事实，并在每个边界重新读目标
//! 状态（用户可能在裁判返回前 Stop/pause/替换目标）。
//!
//! 让位规则（与 TS `yieldToPendingCommands` 等价）：
//! - 有排队输入、或有命令在 inbox 里等 gate → 结束本轮循环，留给用户；
//! - 目标不再 active / 会话进入 plan 模式 → 结束；
//! - Stop（cancel 已置位）→ 目标 paused 后结束。
//!
//! 谁投影什么：本模块只编排；目标状态变更经 `Session::project_goal_update`，
//! 裁判时间线经 `Session::project_goal_verify_marker`，续跑轮经 `turn::run_goal_turn`。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::goal::{GoalStatus, VerificationOutcome};
use crate::goal_verifier::{verify_completion, VerifierCall};
use crate::session::Session;
use crate::store::now_ms;
use crate::turn::{self, TurnState};
use crate::HostServer;

/// 循环起点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopStart {
    /// sendGoalCommand：先跑一轮续跑，再验（TS `verifyBeforeFirstContinue: false`）。
    ContinueFirst,
    /// 用户轮结束后 / resumeGoal：先验，再决定是否续跑（`verifyBeforeFirstContinue: true`）。
    VerifyFirst,
}

/// 循环结束原因（供命令 ack 表达"目标是否还在推进"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopEnd {
    /// 目标完成（裁判 pass）。
    Verified,
    /// 裁判给出 notSatisfied 但没有 nextAction，或裁判过程失败 → 等用户。
    AwaitingUser,
    /// 目标不再 active（pause/resume/替换/结束）。
    Inactive,
    /// 让位给排队输入或待执行命令。
    Yielded,
    /// Stop / pauseGoal 取消了在途工作。
    Cancelled,
    /// 模型/历史故障：本轮失败，循环停下（不静默重试）。
    Failed,
    /// plan 模式：不自动续跑（目标仍然落库）。
    PlanMode,
}

impl LoopEnd {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LoopEnd::Verified => "verified",
            LoopEnd::AwaitingUser => "awaitingUser",
            LoopEnd::Inactive => "inactive",
            LoopEnd::Yielded => "yielded",
            LoopEnd::Cancelled => "cancelled",
            LoopEnd::Failed => "failed",
            LoopEnd::PlanMode => "planMode",
        }
    }
}

/// 跑目标续跑循环，返回需要广播的帧行与结束原因。
pub(crate) async fn run_goal_loop(
    server: &Arc<HostServer>,
    session: Arc<Session>,
    start: LoopStart,
) -> (Vec<String>, LoopEnd) {
    let mut lines = Vec::new();
    let mut verify_first = start == LoopStart::VerifyFirst;
    // 循环开始前消费**遗留** cancel 位：pauseGoal 在没有在途工作时也会置位它来
    // 打断（或 stop 恰好落在两轮之间），若不消费，resumeGoal 的第一次循环会立刻
    // 把这个刚恢复的目标再暂停一次。真正的 stop 在循环运行中置位，仍会被下面的
    // 检查或在途 turn/verifier 看到。
    let _ = session.take_cancel();
    loop {
        if session.cancel.load(Ordering::SeqCst) {
            lines.extend(pause_cancelled_goal(server, &session));
            return (lines, LoopEnd::Cancelled);
        }
        let Some(goal) = session.goal() else {
            return (lines, LoopEnd::Inactive);
        };
        if goal.status != GoalStatus::Active {
            return (lines, LoopEnd::Inactive);
        }
        if session.collaboration_mode() == "plan" {
            return (lines, LoopEnd::PlanMode);
        }
        // 让位：排队输入或等待 gate 的命令都比自动续跑优先。
        if session.queue_len() > 0 || session.inbox_pending_commands() > 0 {
            return (lines, LoopEnd::Yielded);
        }
        if verify_first {
            match verify_once(server, &session, &mut lines).await {
                VerifyStep::Pass => return (lines, LoopEnd::Verified),
                VerifyStep::Stop(reason) => return (lines, reason),
                VerifyStep::Continue => {}
            }
        }
        // 一轮续跑：model-only 输入，行上只出现 turnHeader(goalContinuation)。
        let Some(goal) = session.goal() else {
            return (lines, LoopEnd::Inactive);
        };
        let started_at = now_ms();
        session.update_goal(|record| record.start_run(started_at));
        lines.extend(persist_and_project(server, &session));
        let note = latest_continuation_note(&goal);
        let prompt = crate::goal::format_goal_continuation_prompt(
            &goal,
            note.as_ref()
                .map(|(reason, action)| (reason.as_str(), action.as_str())),
        );
        let turn_id = format!("{}#goal-{}-{}", session.id, goal.target_id, goal.iteration);
        let report = turn::run_goal_turn(
            Arc::clone(&session),
            server.provider.as_ref(),
            &server.turn_deps,
            &prompt,
            &turn_id,
            &session.id,
        )
        .await;
        lines.extend(report.lines);
        session.update_goal(|record| record.finish_run(now_ms()));
        lines.extend(persist_and_project(server, &session));
        match report.state {
            TurnState::Interrupted => {
                lines.extend(pause_cancelled_goal(server, &session));
                return (lines, LoopEnd::Cancelled);
            }
            TurnState::Failed => return (lines, LoopEnd::Failed),
            TurnState::Completed => {}
        }
        verify_first = true;
    }
}

/// 续跑轮的提示词需要上一轮裁判的 (reason, nextAction)。
fn latest_continuation_note(goal: &crate::goal::GoalRecord) -> Option<(String, String)> {
    let last = goal.verifications.last()?;
    let action = last.next_action.clone().unwrap_or_default();
    if action.trim().is_empty() {
        return None;
    }
    Some((last.reason.clone().unwrap_or_default(), action))
}

enum VerifyStep {
    Pass,
    Continue,
    Stop(LoopEnd),
}

/// 一次裁判：开始投影（goal=verifying + activeWorks）→ 调用 → 落结论 + 时间线 marker。
async fn verify_once(
    server: &Arc<HostServer>,
    session: &Arc<Session>,
    lines: &mut Vec<String>,
) -> VerifyStep {
    let Some(goal) = session.goal() else {
        return VerifyStep::Stop(LoopEnd::Inactive);
    };
    let started_at = now_ms();
    session.update_goal(|record| record.begin_verification(started_at));
    let iteration = session.goal().map(|record| record.iteration).unwrap_or(0);
    let target_id = session
        .goal()
        .map(|record| record.target_id)
        .unwrap_or_default();
    // 裁判是 completion-blocking active work：phase=running + activeWorks.goalVerifier，
    // 这样 Stop 可停、inputRouting 变 enqueue（goalVerifierAcceptsFutureInput）。
    let previous_phase = session.phase();
    session.set_phase("running");
    lines.extend(persist_and_project(server, session));
    lines.extend(session.project_control_update());
    // started marker：与 TS 同一条生命周期行（targetId_iteration），终态再 upsert。
    let (_, marker_lines) =
        session.project_goal_verify_marker(&target_id, iteration, "running", None, None);
    lines.extend(marker_lines);

    let Some(provider) = server.provider.as_ref() else {
        let result = crate::goal::VerificationResult::process_failure(
            "Completion verifier request failed: no provider configured",
        );
        return finish_verification(server, session, lines, iteration, result, previous_phase);
    };
    let cancel_probe: Arc<dyn Fn() -> bool + Send + Sync> = {
        let session = Arc::clone(session);
        Arc::new(move || session.cancel.load(Ordering::SeqCst))
    };
    let call = verify_completion(
        session,
        provider,
        &server.turn_deps.context,
        &goal,
        cancel_probe,
    )
    .await;
    match call {
        VerifierCall::Cancelled => {
            // 消费 cancel 位，避免泄漏到下一条 turn。
            let _ = session.take_cancel();
            session.update_goal(|record| record.pause_for_cancel());
            session.set_phase(&previous_phase);
            lines.extend(persist_and_project(server, session));
            lines.extend(session.project_control_update());
            VerifyStep::Stop(LoopEnd::Cancelled)
        }
        VerifierCall::Completed(result) => {
            finish_verification(server, session, lines, iteration, result, previous_phase)
        }
    }
}

fn finish_verification(
    server: &Arc<HostServer>,
    session: &Arc<Session>,
    lines: &mut Vec<String>,
    iteration: u32,
    result: crate::goal::VerificationResult,
    previous_phase: String,
) -> VerifyStep {
    let outcome = result.outcome();
    let target_id = session
        .goal()
        .map(|record| record.target_id)
        .unwrap_or_default();
    let at = now_ms();
    session.update_goal(|record| {
        record.record_verification(
            outcome,
            at,
            None,
            Some(result.reason.clone()),
            result.next_action.clone(),
        );
    });
    // 时间线 marker：同一 targetId+iteration 的行从 running upsert 成终态
    // （含 outcome 与 detail），行本身是 verifications 的 anchor。
    let detail = match outcome {
        VerificationOutcome::Pass => Some(result.reason.as_str()),
        VerificationOutcome::NotSatisfied => result
            .next_action
            .as_deref()
            .or(Some(result.reason.as_str())),
        VerificationOutcome::Failed => Some(result.reason.as_str()),
    };
    let (anchor_row_id, marker_lines) =
        session.project_goal_verify_marker(&target_id, iteration, outcome.as_str(), detail, None);
    lines.extend(marker_lines);
    // 把 anchor 回填到 just-recorded entry（TS 的 anchorRowId 指向 marker 行）。
    session.update_goal(|record| {
        if let Some(entry) = record.verifications.last_mut() {
            entry.anchor_row_id = Some(anchor_row_id);
        }
    });
    session.set_phase(&previous_phase);
    lines.extend(persist_and_project(server, session));
    lines.extend(session.project_control_update());
    match outcome {
        VerificationOutcome::Pass => VerifyStep::Pass,
        VerificationOutcome::NotSatisfied if result.should_continue() => VerifyStep::Continue,
        _ => VerifyStep::Stop(LoopEnd::AwaitingUser),
    }
}

/// Stop/pauseGoal：目标 active → paused（`stopPausesActiveGoalTarget`）。
fn pause_cancelled_goal(server: &Arc<HostServer>, session: &Arc<Session>) -> Vec<String> {
    let paused = session
        .update_goal(|record| record.pause_for_cancel())
        .unwrap_or(false);
    if !paused {
        return Vec::new();
    }
    let mut lines = persist_and_project(server, session);
    lines.extend(session.project_control_update());
    lines
}

/// 目标状态变更的唯一持久化点：meta.json 先落盘，再投影。
fn persist_and_project(server: &Arc<HostServer>, session: &Arc<Session>) -> Vec<String> {
    server.persist_meta(session);
    session.project_goal_update()
}
