//! goal completion verifier：单次无工具模型调用的**编排层**。
//!
//! 语义逐条对齐 TS `core/src/runtime/methods/target-completion-verification.ts`：
//! - 消息 = 完整会话历史 + 一条 user 的验证提示词（`formatGoalCompletionVerificationPrompt`）；
//! - `tools: []`，最大输出 tokens 取模型配置上限（不降档）；
//! - 裁判试图调工具 → fail-open（"attempted to call tools"）；
//! - 请求失败 → fail-open（`Completion verifier request failed: …`），保留 reason 供日志/UI；
//! - 取消（Stop / pauseGoal）→ `Cancelled`，由循环负责把目标暂停——取消**不是**结论，
//!   既不 fail-open 也不写入 verifications。
//!
//! 本模块只负责"说什么/怎么判"，状态机与持久化在 `goal.rs` / `goal_loop.rs`。

use std::sync::Arc;

use crate::goal::{
    format_goal_completion_verification_prompt, parse_verification_text, GoalRecord,
    VerificationResult, VERIFIER_FALLBACK_REASON,
};
use crate::provider_config::ProviderConfig;
use crate::session::Session;
use crate::turn_context::ContextFacts;

/// 裁判调用的两种结局：给出结论，或被取消（没有结论）。
pub(crate) enum VerifierCall {
    Completed(VerificationResult),
    Cancelled,
}

/// 运行一次完成性裁判。`is_cancelled` 由 executor 轮询，Stop 能让请求尽快收敛。
pub(crate) async fn verify_completion(
    session: &Arc<Session>,
    config: &ProviderConfig,
    context: &ContextFacts,
    goal: &GoalRecord,
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> VerifierCall {
    let history = match session.history_snapshot() {
        Ok(history) => history,
        Err(error) => {
            // 历史不可读 = 裁判看不到工作证据。fail-open 与坏 JSON 同类：
            // 裁判链路故障不能把已交付的目标永久卡住。
            return VerifierCall::Completed(VerificationResult::process_failure(format!(
                "Completion verifier could not read conversation history: {error}"
            )));
        }
    };
    let mut messages = history;
    messages.push(zcode_provider::request::Message::text(
        zcode_provider::request::MessageRole::User,
        format_goal_completion_verification_prompt(goal),
    ));
    match crate::single_shot::generate_text(
        config,
        context,
        &messages,
        config.max_tokens,
        Arc::clone(&is_cancelled),
    )
    .await
    {
        Ok(result) => {
            if is_cancelled() {
                return VerifierCall::Cancelled;
            }
            let verification = if result.tool_calls > 0 {
                VerificationResult::fail_open(
                    "The completion verifier attempted to call tools instead of returning a verification result.",
                )
            } else if result.text.trim().is_empty() {
                VerificationResult::fail_open(VERIFIER_FALLBACK_REASON)
            } else {
                parse_verification_text(&result.text)
            };
            VerifierCall::Completed(verification)
        }
        Err(error) => {
            if is_cancelled() {
                VerifierCall::Cancelled
            } else {
                VerifierCall::Completed(VerificationResult::process_failure(format!(
                    "Completion verifier request failed: {}",
                    error.message
                )))
            }
        }
    }
}
