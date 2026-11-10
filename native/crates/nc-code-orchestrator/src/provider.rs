//! Real model client: resolves the provider from the existing personal provider store
//! (`nc-code-services-provider`, the single provider fact source) and runs the existing
//! `nc-code-provider` executor. There is no other HTTP path and no fake provider.

use nc_code_provider::executor::{self, StreamRequest};
use nc_code_provider::http_transport::HttpTransport;
use nc_code_provider::request::{
    build_anthropic_messages_body, build_openai_compatible_body, MessageRole, ProviderKind,
    RequestBodyOptions, ToolDefinition,
};
use nc_code_provider::retry::RetryOptions;
use nc_code_provider::transport::{RealClock, TransportRequest};
use nc_code_provider::{ModelStreamEvent, ModelUsage};
use nc_code_services_provider::store::{
    model_effective_config, provider_effective_config, provider_issues, provider_order,
    ModelRecord, ProviderStore, StoreSnapshot,
};
use serde_json::Value;

use crate::error::{NcagError, Result};

/// One resolved model target. `provider_id`/`model_id` are the store keys, so an
/// administrator can audit which record produced a token.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTarget {
    pub provider_id: String,
    pub model_id: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: String,
    pub auth_style: &'static str,
    pub max_tokens: u32,
    pub context_window: u64,
    /// USD per 1M input tokens / output tokens (0.0 when the operator did not state a price).
    pub input_usd_per_mtoken: f64,
    pub output_usd_per_mtoken: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelReply {
    pub text: String,
    pub tool_calls: Vec<nc_code_provider::ModelToolCall>,
    pub finish_reason: Option<String>,
    pub usage: ModelUsage,
    pub provider: String,
    pub model: String,
    pub cost_usd: f64,
}

const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// Listing of selectable providers/models for the settings surface.
pub fn selection_view(snapshot: &StoreSnapshot) -> Value {
    let providers: Vec<Value> = provider_order(snapshot)
        .into_iter()
        .filter_map(|provider_id| snapshot.provider(&provider_id))
        .map(|provider| {
            let config = provider_effective_config(provider, snapshot);
            let issues = provider_issues(&config);
            let models: Vec<Value> = provider
                .models
                .iter()
                .filter(|model| model.enabled)
                .map(|model| {
                    serde_json::json!({
                        "modelId": model.model_id,
                        "config": model_effective_config(model),
                    })
                })
                .collect();
            serde_json::json!({
                "providerId": provider.provider_id,
                "providerName": provider.provider_name,
                "enabled": provider.enabled,
                "executable": provider.enabled && issues.is_empty(),
                "issues": issues,
                "models": models,
            })
        })
        .collect();
    serde_json::json!({ "revision": snapshot.revision, "providers": providers })
}

/// Resolve a provider/model pair, or the first executable provider when no ids were given.
pub async fn resolve_target(
    store: &ProviderStore,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    context_window_override: Option<u64>,
) -> Result<ModelTarget> {
    let snapshot = store
        .snapshot()
        .await
        .map_err(|error| NcagError::provider(format!("provider store unavailable: {error}")))?;
    resolve_from_snapshot(&snapshot, provider_id, model_id, context_window_override)
}

pub fn resolve_from_snapshot(
    snapshot: &StoreSnapshot,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    context_window_override: Option<u64>,
) -> Result<ModelTarget> {
    // An explicit selection wins; otherwise the user's stored default selection is used
    // before falling back to the first executable provider (never a guess).
    let default_selection = snapshot.default_model_selection.as_ref();
    let selected_provider = provider_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            default_selection
                .and_then(|value| value.get("providerId"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        });
    let selected_model = model_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            default_selection
                .and_then(|value| value.get("modelId"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        });
    let ordered = provider_order(snapshot);
    let candidates: Vec<&nc_code_services_provider::store::ProviderRecord> = ordered
        .iter()
        .filter(|id| selected_provider.is_none() || selected_provider == Some(id.as_str()))
        .filter_map(|id| snapshot.provider(id))
        .filter(|provider| provider.enabled)
        .collect();
    if candidates.is_empty() {
        return Err(NcagError::provider(match selected_provider {
            Some(id) => format!("provider {id} is not configured and enabled"),
            None => "no provider is configured: add one in Settings > Providers (api.baseUrl + access.apiKey) and select a model".to_string(),
        }));
    }
    let mut first_issue: Option<String> = None;
    for provider in candidates {
        let config = provider_effective_config(provider, snapshot);
        let issues = provider_issues(&config);
        if !issues.is_empty() {
            first_issue.get_or_insert(
                issues
                    .first()
                    .and_then(|issue| issue.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("provider is not executable")
                    .to_string(),
            );
            continue;
        }
        let record = match selected_model {
            Some(wanted) => provider
                .models
                .iter()
                .find(|model| model.model_id == wanted && model.enabled),
            None => provider.models.iter().find(|model| model.enabled),
        };
        let Some(record) = record else {
            first_issue.get_or_insert(format!(
                "provider {} has no enabled model{}",
                provider.provider_id,
                selected_model
                    .map(|id| format!(" {id}"))
                    .unwrap_or_default()
            ));
            continue;
        };
        return build_target(provider, record, &config, context_window_override);
    }
    Err(NcagError::provider(first_issue.unwrap_or_else(|| {
        "no executable provider/model pair was found".to_string()
    })))
}

fn build_target(
    provider: &nc_code_services_provider::store::ProviderRecord,
    record: &ModelRecord,
    config: &Value,
    context_window_override: Option<u64>,
) -> Result<ModelTarget> {
    let base_url = config
        .pointer("/api/baseUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| NcagError::provider("provider api.baseUrl is empty"))?;
    let api_key = config
        .pointer("/access/apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| NcagError::provider("provider access.apiKey is empty"))?;
    let api_type = config
        .pointer("/api/type")
        .and_then(Value::as_str)
        .unwrap_or("openai-compatible");
    // The provider store's wire vocabulary: `anthropic-messages` and
    // `openai-chat-completions` are the two transports the executor implements.
    // `openai-responses` is a different protocol; it fails closed by name instead of
    // being silently sent as a chat-completions request.
    let kind = match api_type {
        "anthropic" | "anthropic-messages" | "anthropic-compatible" => ProviderKind::Anthropic,
        "openai-chat-completions" | "openai" | "openai-compatible" | "openai-completions" => {
            ProviderKind::OpenaiCompatible
        }
        other => {
            return Err(NcagError::provider(format!(
                "provider api.type {other} is not a supported model transport; NC-AG supports anthropic-messages and openai-chat-completions"
            )))
        }
    };
    let auth_style = match kind {
        ProviderKind::Anthropic => "x-api-key",
        ProviderKind::OpenaiCompatible => "bearer",
    };
    let effective_model = model_effective_config(record);
    let max_tokens = effective_model
        .pointer("/optionSpecs/maxOutputTokens/max")
        .and_then(Value::as_u64)
        .map(|value| value.min(8192) as u32)
        .unwrap_or(4096);
    let context_window = context_window_override
        .or_else(|| {
            effective_model
                .pointer("/properties/contextWindow")
                .and_then(Value::as_u64)
        })
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let pricing = config.get("pricing").unwrap_or(&Value::Null);
    let input_price = pricing
        .get("inputUsdPerMillionTokens")
        .and_then(Value::as_f64)
        .or_else(|| {
            model_effective_config(record)
                .get("inputUsdPerMillionTokens")
                .and_then(Value::as_f64)
        })
        .unwrap_or(0.0);
    let output_price = pricing
        .get("outputUsdPerMillionTokens")
        .and_then(Value::as_f64)
        .or_else(|| {
            model_effective_config(record)
                .get("outputUsdPerMillionTokens")
                .and_then(Value::as_f64)
        })
        .unwrap_or(0.0);
    Ok(ModelTarget {
        provider_id: provider.provider_id.clone(),
        model_id: record.model_id.clone(),
        kind,
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        auth_style,
        max_tokens: max_tokens.max(256),
        context_window,
        input_usd_per_mtoken: input_price,
        output_usd_per_mtoken: output_price,
    })
}

pub fn endpoint_url(target: &ModelTarget) -> String {
    let base = target.base_url.trim_end_matches('/');
    match target.kind {
        ProviderKind::OpenaiCompatible => {
            if base.ends_with("/chat/completions") {
                base.to_string()
            } else {
                format!("{base}/chat/completions")
            }
        }
        ProviderKind::Anthropic => {
            if base.ends_with("/messages") {
                base.to_string()
            } else {
                format!("{base}/messages")
            }
        }
    }
}

fn auth_headers(target: &ModelTarget) -> Vec<(String, String)> {
    match target.auth_style {
        "bearer" => vec![
            ("content-type".to_string(), "application/json".to_string()),
            (
                "authorization".to_string(),
                format!("Bearer {}", target.api_key),
            ),
        ],
        _ => vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-api-key".to_string(), target.api_key.clone()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ],
    }
}

/// A conversation message in the provider's own vocabulary.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub text: String,
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<nc_code_provider::request::ToolCallRequest>,
    pub reasoning: Vec<nc_code_provider::request::ReasoningBlock>,
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            text: text.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            reasoning: Vec::new(),
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            text: text.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            reasoning: Vec::new(),
        }
    }

    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            text: text.into(),
            tool_call_id: Some(call_id.into()),
            tool_calls: Vec::new(),
            reasoning: Vec::new(),
        }
    }

    fn to_provider(&self) -> nc_code_provider::request::Message {
        let mut message = nc_code_provider::request::Message::text(self.role, self.text.clone());
        message.tool_call_id = self.tool_call_id.clone();
        message.tool_calls = self.tool_calls.clone();
        message.reasoning = self.reasoning.clone();
        message
    }
}

/// Run one model request through the existing executor (streaming, retries, classification).
pub async fn generate(
    target: &ModelTarget,
    system: &[String],
    messages: &[ChatMessage],
    tools: &[ToolDefinition],
    timeout_ms: u64,
) -> Result<ModelReply> {
    let provider_messages: Vec<nc_code_provider::request::Message> =
        messages.iter().map(ChatMessage::to_provider).collect();
    let options = RequestBodyOptions {
        model: &target.model_id,
        system,
        messages: &provider_messages,
        tools,
        max_tokens: target.max_tokens,
        stream: true,
    };
    let body = match target.kind {
        ProviderKind::Anthropic => build_anthropic_messages_body(&options),
        ProviderKind::OpenaiCompatible => build_openai_compatible_body(&options),
    }
    .map_err(|error| NcagError::provider(error.to_string()))?;
    let body_text = serde_json::to_string(&body)?;
    let transport_request = TransportRequest {
        url: endpoint_url(target),
        headers: auth_headers(target),
        body: body_text,
    };
    let cancelled = std::sync::Arc::new(|| false);
    let stream_request = StreamRequest {
        transport_request,
        provider_kind: target.kind,
        retry: RetryOptions::default(),
        retry_budget: None,
        off_peak: false,
        preserve_provider_stream_boundaries: false,
        stream_idle_timeout_base_ms: Some(60_000),
        stream_idle_timeout_retry_number: None,
        is_cancelled: cancelled,
    };
    let mut transport = HttpTransport::new();
    let execution = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        executor::execute(&mut transport, &RealClock, &stream_request),
    )
    .await
    .map_err(|_| {
        NcagError::provider(format!(
            "model request exceeded the {timeout_ms}ms deadline"
        ))
    })?;
    if let Some(failure) = execution.failure {
        return Err(NcagError::provider(failure.to_string()));
    }
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut finish_reason = None;
    let mut usage = ModelUsage::default();
    for event in &execution.events {
        match event {
            ModelStreamEvent::TextDelta { text: delta, .. } => text.push_str(delta),
            ModelStreamEvent::ToolCall { tool_call } => tool_calls.push(tool_call.clone()),
            ModelStreamEvent::Finish {
                finish_reason: reason,
                usage: round_usage,
                ..
            } => {
                finish_reason = Some(reason.clone());
                usage = round_usage.clone();
            }
            ModelStreamEvent::Error { error } => {
                return Err(NcagError::provider(error.to_string()));
            }
            _ => {}
        }
    }
    let cost_usd = cost_of(target, &usage);
    Ok(ModelReply {
        text,
        tool_calls,
        finish_reason,
        usage,
        provider: target.provider_id.clone(),
        model: target.model_id.clone(),
        cost_usd,
    })
}

/// USD for one usage record from the configured per-million prices.
pub fn cost_of(target: &ModelTarget, usage: &ModelUsage) -> f64 {
    let input = usage.input_tokens.unwrap_or(0) as f64;
    let output = usage.output_tokens.unwrap_or(0) as f64;
    (input * target.input_usd_per_mtoken + output * target.output_usd_per_mtoken) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_services_provider::store::{ModelRecord, ProviderRecord, StoreSnapshot};

    fn snapshot_with(api_key: &str, model: &str) -> StoreSnapshot {
        StoreSnapshot {
            version: 1,
            revision: 3,
            providers: vec![ProviderRecord {
                provider_id: "p1".into(),
                provider_name: "Test".into(),
                template_id: None,
                enabled: true,
                config: serde_json::json!({
                    "api": { "type": "openai-compatible", "baseUrl": "http://127.0.0.1:9/v1" },
                    "access": { "apiKey": api_key },
                    "pricing": { "inputUsdPerMillionTokens": 1.0, "outputUsdPerMillionTokens": 2.0 }
                }),
                models: vec![ModelRecord {
                    model_id: model.into(),
                    config: serde_json::json!({}),
                    enabled: true,
                    use_recommended: true,
                }],
            }],
            provider_order: vec!["p1".into()],
            default_model_selection: None,
        }
    }

    #[test]
    fn resolves_the_first_executable_model() {
        let snapshot = snapshot_with("sk-test", "m-1");
        let target = resolve_from_snapshot(&snapshot, None, None, None).expect("target");
        assert_eq!(target.kind, ProviderKind::OpenaiCompatible);
        assert_eq!(target.auth_style, "bearer");
        assert_eq!(target.model_id, "m-1");
        assert_eq!(
            endpoint_url(&target),
            "http://127.0.0.1:9/v1/chat/completions"
        );
    }

    #[test]
    fn missing_credentials_fail_closed_and_name_the_field() {
        let snapshot = snapshot_with("", "m-1");
        let error = resolve_from_snapshot(&snapshot, None, None, None).expect_err("must fail");
        assert_eq!(error.code, "provider_unavailable");
        assert!(
            !error.message.is_empty(),
            "the provider failure must name a reason"
        );
        // The store-level issue list names the exact missing field.
        assert!(
            error.message.contains("API key") || error.message.contains("apiKey"),
            "{}",
            error.message
        );
    }

    #[test]
    fn cost_uses_configured_prices() {
        let snapshot = snapshot_with("k", "m");
        let target = resolve_from_snapshot(&snapshot, None, None, None).unwrap();
        let usage = ModelUsage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            ..ModelUsage::default()
        };
        assert!((cost_of(&target, &usage) - 2.0).abs() < 1e-9);
    }
}
