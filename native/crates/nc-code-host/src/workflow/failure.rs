//! Containment of model-side failures (port of the stop / context_limit branches of `workflow-driver-model-failure.ts`).
//!
//! The host's capability boundary (recorded honestly): `turn::run_subagent_turn` normalizes a model failure into
//! `session.last_error = {code, message, ...}`, and the provider adapter's structured classification (TS's
//! `inspectWorkflowModelFailure` policy table) does not cross that boundary. So this is a conservative text classification
//! for **clearly permanent failures**: authentication / quota / model unavailable / invalid request → `ProviderStop` (the run stops, resumable);
//! context exceeded → node `ContextLimit`; everything else (including transient ones) settles the node as a driver failure.
//!
//! TS's transient backoff re-drive is not implemented: the host's provider executor owns HTTP-level retries itself (`nc-code-provider`),
//! and the driver stacking a second unbounded re-drive would mask the other. This divergence is recorded in the delivery report.

use nc_code_workflow::types::{
    ProviderStopDetails, ProviderStopKind, WorkflowError, WorkflowErrorCode,
};
use serde_json::Value;

/// The cap on the provider's original text inside ProviderStop details (both the notification and the journal carry it, so it cannot be unbounded).
pub const PROVIDER_STOP_RAW_MESSAGE_MAX_CHARS: usize = 2_000;

/// The classification result of one turn failure.
pub(crate) enum TurnFailure {
    /// A deterministic model-side error: the whole run stops (stopped(provider), resumable) and the node is not settled.
    Provider(Box<WorkflowError>),
    /// The ask itself exceeded the context: the node fails with `ContextLimit`, and the script can catch it.
    ContextLimit(String),
    /// Everything else: node-level DriverError.
    Other(String),
}

/// Classify `session.last_error` into the action the driver has to execute.
pub(crate) fn classify_turn_failure(
    last_error: Option<&Value>,
    actor_label: &str,
    provider_id: Option<&str>,
    model_id: Option<&str>,
) -> TurnFailure {
    let Some(error) = last_error else {
        return TurnFailure::Other("subagent turn failed without a recorded cause".to_string());
    };
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let raw_message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no message")
        .to_string();
    if code == "fault.model.providerNotConfigured" {
        return TurnFailure::Provider(Box::new(provider_stop(
            ProviderStopKind::NotConfigured,
            "provider_not_configured",
            actor_label,
            provider_id,
            model_id,
            None,
            &raw_message,
        )));
    }
    if code != "fault.model.requestFailed" {
        return TurnFailure::Other(raw_message);
    }
    let haystack = raw_message.to_ascii_lowercase();
    if mentions_context_limit(&haystack) {
        return TurnFailure::ContextLimit(format!(
            "Subagent {actor_label} exceeded the model's context window even after compaction. \
             Give this ask a smaller input or split the work across subagents."
        ));
    }
    let classified = classify_permanent_provider_error(&haystack);
    match classified {
        Some((kind, reason, provider_code)) => TurnFailure::Provider(Box::new(provider_stop(
            kind,
            reason,
            actor_label,
            provider_id,
            model_id,
            provider_code,
            &raw_message,
        ))),
        None => TurnFailure::Other(raw_message),
    }
}

fn mentions_context_limit(haystack: &str) -> bool {
    (haystack.contains("context") && (haystack.contains("exceed") || haystack.contains("too long")))
        || haystack.contains("context window")
        || haystack.contains("maximum context")
        || haystack.contains("too many tokens")
        || haystack.contains("context_length_exceeded")
}

/// Conservative permanent-failure recognition: only explicit evidence stops the whole run; anything not recognized just fails the node.
fn classify_permanent_provider_error(
    haystack: &str,
) -> Option<(ProviderStopKind, &'static str, Option<String>)> {
    let status = ["401", "402", "403", "404", "429"]
        .into_iter()
        .find(|status| haystack.contains(status));
    let status_code = status.map(str::to_string);
    if haystack.contains("invalid api key")
        || haystack.contains("invalid_api_key")
        || haystack.contains("unauthorized")
        || haystack.contains("authentication")
        || haystack.contains("permission denied")
        || matches!(status, Some("401") | Some("403"))
    {
        return Some((ProviderStopKind::Auth, "auth", status_code));
    }
    if haystack.contains("quota")
        || haystack.contains("insufficient_quota")
        || haystack.contains("billing")
        || haystack.contains("rate limit")
        || haystack.contains("rate_limit")
        || matches!(status, Some("402") | Some("429"))
    {
        return Some((ProviderStopKind::Quota, "quota", status_code));
    }
    if haystack.contains("model not found")
        || haystack.contains("model_not_found")
        || haystack.contains("no such model")
        || haystack.contains("does not exist")
        || matches!(status, Some("404"))
    {
        return Some((
            ProviderStopKind::ModelUnavailable,
            "model_unavailable",
            status_code,
        ));
    }
    if haystack.contains("invalid request")
        || haystack.contains("invalid_request")
        || haystack.contains("bad request")
        || haystack.contains("invalid_model_request")
        || matches!(status, Some("400"))
    {
        return Some((
            ProviderStopKind::InvalidRequest,
            "invalid_request",
            status_code,
        ));
    }
    None
}

fn provider_stop(
    kind: ProviderStopKind,
    reason: &str,
    actor_label: &str,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    provider_code: Option<String>,
    raw_message: &str,
) -> WorkflowError {
    let raw_message: String = raw_message
        .chars()
        .take(PROVIDER_STOP_RAW_MESSAGE_MAX_CHARS)
        .collect();
    let details = ProviderStopDetails {
        kind,
        reason: reason.to_string(),
        provider_id: provider_id.map(str::to_string),
        provider_label: None,
        model_id: model_id.map(str::to_string),
        provider_code: provider_code.clone(),
        subagent: Some(actor_label.to_string()),
        subagent_name: None,
        phase: None,
        raw_message: if raw_message.is_empty() {
            None
        } else {
            Some(raw_message.clone())
        },
        reset_at: None,
    };
    let code = provider_code
        .map(|code| format!(" [{code}]"))
        .unwrap_or_default();
    WorkflowError::new(
        WorkflowErrorCode::ProviderStop,
        format!(
            "Subagent {actor_label} hit a permanent model-side error ({reason}{code}): {raw_message}"
        ),
    )
    .with_provider_stop(details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auth_and_quota_stop_the_run() {
        let auth = classify_turn_failure(
            Some(
                &json!({"code": "fault.model.requestFailed", "message": "HTTP 401 invalid api key"}),
            ),
            "actor#1@1",
            Some("p"),
            Some("m"),
        );
        match auth {
            TurnFailure::Provider(error) => {
                assert_eq!(error.code, WorkflowErrorCode::ProviderStop);
                let details = error.provider_stop.expect("details");
                assert_eq!(details.kind, ProviderStopKind::Auth);
                assert_eq!(details.provider_code.as_deref(), Some("401"));
            }
            _ => panic!("auth must stop the run"),
        }
        let quota = classify_turn_failure(
            Some(&json!({"code": "fault.model.requestFailed", "message": "429 quota exceeded"})),
            "a",
            None,
            None,
        );
        match quota {
            TurnFailure::Provider(error) => {
                assert_eq!(
                    error.provider_stop.expect("quota details").kind,
                    ProviderStopKind::Quota
                );
            }
            _ => panic!("quota must stop the run"),
        }
    }

    #[test]
    fn context_limit_fails_the_node_not_the_run() {
        let failure = classify_turn_failure(
            Some(&json!({
                "code": "fault.model.requestFailed",
                "message": "maximum context length exceeded: 200000 tokens"
            })),
            "actor",
            None,
            None,
        );
        match failure {
            TurnFailure::ContextLimit(message) => assert!(message.contains("context window")),
            _ => panic!("context limit must be a node failure"),
        }
    }

    #[test]
    fn unknown_failures_stay_driver_errors() {
        assert!(matches!(
            classify_turn_failure(
                Some(&json!({"code": "fault.turn.roundLimitExceeded", "message": "64 rounds"})),
                "a",
                None,
                None
            ),
            TurnFailure::Other(_)
        ));
        assert!(matches!(
            classify_turn_failure(
                Some(&json!({"code": "fault.model.requestFailed", "message": "connection reset"})),
                "a",
                None,
                None
            ),
            TurnFailure::Other(_)
        ));
        assert!(matches!(
            classify_turn_failure(None, "a", None, None),
            TurnFailure::Other(_)
        ));
    }
}
