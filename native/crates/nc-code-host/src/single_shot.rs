//! 无工具单次模型调用：compact 摘要与 goal completion verifier 共用的唯一实现。
//!
//! WHY 抽出：两条调用都是"一次请求、只收文本、不带工具"的辅助模型调用；请求体
//! 构造、鉴权头、重试预算与取消探针若各写一份，就会出现"verifier 认 bearer、turn
//! 认 x-api-key"这类只在一种 provider 上复现的分叉。这里只做一件事：发出请求并
//! 收集文本/工具调用/用量，**不**写历史、不投影行——调用方拥有自己的语义。

use std::sync::Arc;

use nc_code_provider::executor::{self, StreamRequest};
use nc_code_provider::http_transport::HttpTransport;
use nc_code_provider::request::{
    build_anthropic_messages_body, build_openai_compatible_body, Message, RequestBodyOptions,
    ToolDefinition,
};
use nc_code_provider::transport::{RealClock, TransportRequest};
use nc_code_provider::{AdapterError, ModelErrorCode, ModelStreamEvent, ModelUsage};

use crate::provider_config::ProviderConfig;
use crate::turn_context::ContextFacts;

/// 一次无工具调用的结果。`tool_calls` 是事实而不是错误：TS goal verifier 用它
/// 判定"裁判试图调工具"并 fail-open；调用方自行决定怎么处理。
#[derive(Debug, Clone, Default)]
pub struct SingleShotResult {
    pub text: String,
    pub tool_calls: usize,
    /// Tool calls the model emitted (workspace text generation may request tools).
    pub tool_call_list: Vec<nc_code_provider::ModelToolCall>,
    pub finish_reason: Option<String>,
    pub usage: ModelUsage,
}

/// 发起一次无工具模型调用（`tools: []`）。
///
/// `is_cancelled` 每轮由 executor 轮询：Stop 会让在途请求尽快收敛为
/// `ModelErrorCode::TurnAborted` 一类失败，而不是等 provider 自然结束。
pub async fn generate_text(
    config: &ProviderConfig,
    context: &ContextFacts,
    messages: &[Message],
    max_tokens: u32,
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<SingleShotResult, AdapterError> {
    generate(config, context, messages, &[], max_tokens, is_cancelled).await
}

/// One-shot generation with an optional tool surface (workspace text generation).
pub async fn generate(
    config: &ProviderConfig,
    context: &ContextFacts,
    messages: &[Message],
    tools: &[ToolDefinition],
    max_tokens: u32,
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<SingleShotResult, AdapterError> {
    let provider_kind = config.provider_kind().map_err(|error| {
        AdapterError::new(ModelErrorCode::ProviderNotConfigured, error.to_string())
    })?;
    let options = RequestBodyOptions {
        model: &config.model,
        system: &context.system_blocks,
        messages,
        tools,
        max_tokens,
        stream: config.stream,
    };
    let body = match provider_kind {
        nc_code_provider::request::ProviderKind::Anthropic => {
            build_anthropic_messages_body(&options)
        }
        nc_code_provider::request::ProviderKind::OpenaiCompatible => {
            build_openai_compatible_body(&options)
        }
    }?;
    let body_json = serde_json::to_string(&body).map_err(|error| {
        AdapterError::new(ModelErrorCode::InvalidModelRequest, error.to_string())
    })?;
    let auth_style = crate::turn::resolved_auth_style(config, provider_kind);
    let transport_request = TransportRequest {
        url: config.endpoint_url(),
        headers: crate::turn::auth_headers(&config.api_key, auth_style),
        body: body_json,
    };
    let stream_request = StreamRequest {
        transport_request,
        provider_kind,
        retry: nc_code_provider::retry::RetryOptions::default(),
        retry_budget: None,
        off_peak: false,
        preserve_provider_stream_boundaries: false,
        stream_idle_timeout_base_ms: None,
        stream_idle_timeout_retry_number: None,
        is_cancelled,
    };
    let mut transport = HttpTransport::new();
    let execution = executor::execute(&mut transport, &RealClock, &stream_request).await;
    if let Some(failure) = execution.failure {
        return Err(failure);
    }
    let mut result = SingleShotResult::default();
    for event in &execution.events {
        match event {
            ModelStreamEvent::TextDelta { text, .. } => result.text.push_str(text),
            ModelStreamEvent::ToolCall { tool_call } => {
                result.tool_calls += 1;
                result.tool_call_list.push(tool_call.clone());
            }
            ModelStreamEvent::Finish {
                finish_reason,
                usage,
                ..
            } => {
                result.finish_reason = Some(finish_reason.clone());
                result.usage = usage.clone();
            }
            ModelStreamEvent::Error { error } => {
                return Err(AdapterError::new(
                    ModelErrorCode::InvalidModelResponse,
                    error.to_string(),
                ));
            }
            _ => {}
        }
    }
    Ok(result)
}
