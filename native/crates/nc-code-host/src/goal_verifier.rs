//! goal completion verifierï¼šå•æ¬¡æ— å·¥å…·æ¨¡åž‹è°ƒç”¨çš„**ç¼–æŽ’å±‚**ã€‚
//!
//! è¯­ä¹‰é€æ¡å¯¹é½ TS `core/src/runtime/methods/target-completion-verification.ts`ï¼š
//! - æ¶ˆæ¯ = å®Œæ•´ä¼šè¯åŽ†å² + ä¸€æ¡ user çš„éªŒè¯æç¤ºè¯ï¼ˆ`formatGoalCompletionVerificationPrompt`ï¼‰ï¼›
//! - `tools: []`ï¼Œæœ€å¤§è¾“å‡º tokens å–æ¨¡åž‹é…ç½®ä¸Šé™ï¼ˆä¸é™æ¡£ï¼‰ï¼›
//! - è£åˆ¤è¯•å›¾è°ƒå·¥å…· â†’ fail-openï¼ˆ"attempted to call tools"ï¼‰ï¼›
//! - è¯·æ±‚å¤±è´¥ â†’ fail-openï¼ˆ`Completion verifier request failed: â€¦`ï¼‰ï¼Œä¿ç•™ reason ä¾›æ—¥å¿—/UIï¼›
//! - å–æ¶ˆï¼ˆStop / pauseGoalï¼‰â†’ `Cancelled`ï¼Œç”±å¾ªçŽ¯è´Ÿè´£æŠŠç›®æ ‡æš‚åœâ€”â€”å–æ¶ˆ**ä¸æ˜¯**ç»“è®ºï¼Œ
//!   æ—¢ä¸ fail-open ä¹Ÿä¸å†™å…¥ verificationsã€‚
//!
//! æœ¬æ¨¡å—åªè´Ÿè´£"è¯´ä»€ä¹ˆ/æ€Žä¹ˆåˆ¤"ï¼ŒçŠ¶æ€æœºä¸ŽæŒä¹…åŒ–åœ¨ `goal.rs` / `goal_loop.rs`ã€‚

use std::sync::Arc;

use crate::goal::{
    format_goal_completion_verification_prompt, parse_verification_text, GoalRecord,
    VerificationResult, VERIFIER_FALLBACK_REASON,
};
use crate::provider_config::ProviderConfig;
use crate::session::Session;
use crate::turn_context::ContextFacts;

/// è£åˆ¤è°ƒç”¨çš„ä¸¤ç§ç»“å±€ï¼šç»™å‡ºç»“è®ºï¼Œæˆ–è¢«å–æ¶ˆï¼ˆæ²¡æœ‰ç»“è®ºï¼‰ã€‚
pub(crate) enum VerifierCall {
    Completed(VerificationResult),
    Cancelled,
}

/// è¿è¡Œä¸€æ¬¡å®Œæˆæ€§è£åˆ¤ã€‚`is_cancelled` ç”± executor è½®è¯¢ï¼ŒStop èƒ½è®©è¯·æ±‚å°½å¿«æ”¶æ•›ã€‚
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
            // åŽ†å²ä¸å¯è¯» = è£åˆ¤çœ‹ä¸åˆ°å·¥ä½œè¯æ®ã€‚fail-open ä¸Žå JSON åŒç±»ï¼š
            // è£åˆ¤é“¾è·¯æ•…éšœä¸èƒ½æŠŠå·²äº¤ä»˜çš„ç›®æ ‡æ°¸ä¹…å¡ä½ã€‚
            return VerifierCall::Completed(VerificationResult::process_failure(format!(
                "Completion verifier could not read conversation history: {error}"
            )));
        }
    };
    let mut messages = history;
    messages.push(nc_code_provider::request::Message::text(
        nc_code_provider::request::MessageRole::User,
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

#[cfg(test)]
mod tests {
    #[test]
    fn outcome_classification_distinguishes_process_failure_from_not_satisfied() {
        assert_eq!(
            crate::goal::VerificationResult::process_failure("x").outcome(),
            crate::goal::VerificationOutcome::Failed
        );
        assert_eq!(
            crate::goal::VerificationResult::fail_open("x").outcome(),
            crate::goal::VerificationOutcome::Pass
        );
        let not_satisfied = crate::goal::VerificationResult {
            passed: false,
            reason: "no".into(),
            next_action: Some("do".into()),
            verifier_failed: false,
        };
        assert_eq!(
            not_satisfied.outcome(),
            crate::goal::VerificationOutcome::NotSatisfied
        );
        assert!(not_satisfied.should_continue());
    }

    #[test]
    fn parse_candidates_are_real_tests_for_goal_loop_verifier_predicate() {
        // Verifier predicate path: parse JSON, fallback on invalid.
        let valid = crate::goal::parse_verification_text(r#"{"passed":true,"reason":"done"}"#);
        assert_eq!(valid.outcome(), crate::goal::VerificationOutcome::Pass);
        // Invalid JSON falls back to fail_open with its own reason (not the
        // VERIFIER_FALLBACK_REASON which is for empty text from the model).
        let invalid = crate::goal::parse_verification_text("not json in the end");
        assert_eq!(invalid.outcome(), crate::goal::VerificationOutcome::Pass);
        assert_eq!(
            invalid.reason,
            "The completion verifier did not return valid JSON."
        );
        // Empty text is trimmed and also hits the invalid-JSON fallback.
        // The shared fallback constant is used only for JSON objects that parse but
        // carry an empty reason (pinned in goal::tests::empty_reason_falls_back_to_the_shared_constant).
        let empty = crate::goal::parse_verification_text("");
        assert_eq!(
            empty.reason,
            "The completion verifier did not return valid JSON."
        );
    }
}
