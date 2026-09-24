//! Turn 执行：admission → context 装配 → 模型/工具循环 → settle 的完整生命周期。
//!
//! 与 TS runtime 的关系（specs/rust-native-crates.md S3 验收）：本 increment 的
//! turn 是「多回合 agent 循环」——模型回合与工具回合交替直到模型给出无工具调用
//! 的终态文本。admission 语义完整（串行 gate、幂等重提、cancel 走
//! `zcode-core` CommandInbox）；工具层见 `tools/`（六工具 + 权限闸门）；context
//! 装配见 `turn_context.rs`。
//!
//! fail-closed 纪律：没有 provider 配置时不发起任何模型调用，turn 以
//! `ModelErrorCode::ProviderNotConfigured` 终态投影（turnHeader failed +
//! sessionErrorInfo），命令 settle 为 failed——绝不伪造 provider、绝不吞错误。
//!
//! 防跑飞（不是 tool-count 硬停止）：连续 3 次相同 (tool, input) 签名 → typed
//! fault 终止；单 turn 模型/工具回合上限 64 → typed fault 终止。

use std::sync::Arc;

use serde_json::{json, Value};
use zcode_core::inbox::Admitted;
use zcode_core::protocol::{CommandFinal, CommandStatus};
use zcode_provider::executor::{self, StreamRequest};
use zcode_provider::http_transport::HttpTransport;
use zcode_provider::request::{
    build_anthropic_messages_body, build_openai_compatible_body, Message, MessageRole,
    ProviderKind, RequestBodyOptions, ToolCallRequest, ToolDefinition,
};
use zcode_provider::transport::{RealClock, TransportRequest};
use zcode_provider::{AdapterError, ModelErrorCode, ModelStreamEvent, ModelToolCall, ModelUsage};

use crate::provider_config::ProviderConfig;
use crate::rows;
use crate::session::Session;
use crate::snapshot;
use crate::store::now_ms;
use crate::tools::{ToolContext, ToolError, ToolErrorCode, ToolRegistry};
use crate::turn_context::{self, ContextFacts};

/// host 定义的 fault code（TS 侧 fault 词表的完整移植尚未完成，README 差异表记录）。
pub const FAULT_PROVIDER_NOT_CONFIGURED: &str = "fault.model.providerNotConfigured";
pub const FAULT_TURN_STOPPED: &str = "fault.turn.stopped";
pub const FAULT_MODEL_FAILED: &str = "fault.model.requestFailed";
pub const FAULT_TOOL_ROUND_LIMIT: &str = "fault.turn.roundLimitExceeded";
pub const FAULT_REPEATED_TOOL_CALL: &str = "fault.turn.repeatedToolCall";

const MAX_TOOL_ROUNDS: usize = 64;
const REPEATED_TOOL_CALL_LIMIT: usize = 3;
/// 触发 LLM 摘要压缩的历史消息条数（> 该值时压缩旧部分，保留最近若干条）。
const COMPACT_HISTORY_THRESHOLD_MESSAGES: usize = 24;

/// turn 依赖（host 级只读事实，每 host 构建一次）。
pub struct TurnDeps {
    pub registry: Arc<ToolRegistry>,
    pub context: ContextFacts,
    /// `--full-access`：destructive 工具调用自动允许；缺省 fail-closed。
    pub full_access: bool,
    /// host 级 provider 配置（ListModels 的事实来源）。
    pub provider_config: Option<Arc<ProviderConfig>>,
}

struct RoundResult {
    text: String,
    tool_calls: Vec<ModelToolCall>,
}

/// 执行一个 admitted 的 sendText turn。返回需要广播的帧行（由调用方写入 stdout）。
pub async fn run_turn(
    session: Arc<Session>,
    provider: Option<&ProviderConfig>,
    deps: &TurnDeps,
    admitted: Admitted,
) -> Vec<String> {
    let source_command_id = admitted.envelope.command_id.clone();
    let turn_id = admitted.envelope.command_id.clone();
    let client_id = admitted.envelope.client_id.clone();
    let text = admitted
        .envelope
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut lines = Vec::new();
    session.set_phase("running");
    lines.extend(session.project_turn_started(&turn_id, &source_command_id));
    lines.extend(session.project_user_input(&turn_id, &text, &source_command_id, &client_id));

    let model_label = provider.map(|config| config.model.as_str());
    let response_id = format!("resp-{source_command_id}");
    let row_id = session.project_assistant_row(&turn_id, &response_id, model_label);

    let mut history: Vec<Message> = vec![Message::text(MessageRole::User, text.as_str())];
    let mut streamed_text = String::new();

    let outcome = match provider {
        Some(config) => {
            run_agent_loop(
                &session,
                config,
                deps,
                &turn_id,
                &mut history,
                &mut streamed_text,
                row_id,
                &mut lines,
            )
            .await
        }
        None => Err(AdapterError::new(
            ModelErrorCode::ProviderNotConfigured,
            "no provider configured; pass --provider-config to enable model calls",
        )),
    };

    let stopped = session.take_cancel();
    let (final_text, row_state, failure) = if stopped {
        let info = snapshot::session_error_info(
            FAULT_TURN_STOPPED,
            "turn stopped by user",
            true,
            "runtime",
            now_ms(),
        );
        (streamed_text, "interrupted", Some(info))
    } else {
        match outcome {
            Ok(()) => (streamed_text, "complete", None),
            Err(error) => {
                let code = match error.code {
                    ModelErrorCode::ProviderNotConfigured => FAULT_PROVIDER_NOT_CONFIGURED,
                    _ => FAULT_MODEL_FAILED,
                };
                let info =
                    snapshot::session_error_info(code, &error.message, false, "provider", now_ms());
                (streamed_text, "failed", Some(info))
            }
        }
    };

    session.set_last_error(failure.clone());
    lines.extend(session.project_assistant_final(
        row_id,
        &response_id,
        &final_text,
        row_state,
        model_label,
    ));

    let (header_state, turn_phase, status, reason_code, result) = if stopped {
        (
            "completedInterrupted",
            "completedInterrupted",
            CommandStatus::Failed,
            Some(FAULT_TURN_STOPPED.to_string()),
            None,
        )
    } else if failure.is_some() {
        (
            "failed",
            "error",
            CommandStatus::Failed,
            failure.as_ref().and_then(|error| {
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
            None,
        )
    } else {
        (
            "completedSuccess",
            "completedSuccess",
            CommandStatus::Accepted,
            None,
            Some(input_accepted_result(&source_command_id)),
        )
    };

    finish_turn(
        &session,
        &turn_id,
        &source_command_id,
        header_state,
        turn_phase,
        &mut lines,
    );
    settle(&session, admitted, status, reason_code, result);
    lines
}

fn input_accepted_result(command_id: &str) -> Value {
    json!({
        "type": "inputAccepted",
        "delivery": "startNow",
        "inputId": command_id,
    })
}

/// 生效鉴权样式（配置加载时已校验；这里是纵深防御：坏样式退回 kind 原生并告警）。
fn resolved_auth_style(config: &ProviderConfig, provider_kind: ProviderKind) -> &'static str {
    match config.auth_style() {
        Ok(style) => style,
        Err(error) => {
            eprintln!("[zcode-host] invalid authStyle ({error}); using provider default");
            match provider_kind {
                ProviderKind::Anthropic => crate::provider_config::AUTH_STYLE_X_API_KEY,
                ProviderKind::OpenaiCompatible => crate::provider_config::AUTH_STYLE_BEARER,
            }
        }
    }
}

/// 多回合循环：模型回合 → 工具回合 → 模型回合……直到无工具调用或 typed fault。
#[allow(clippy::too_many_arguments)]
async fn run_agent_loop(
    session: &Arc<Session>,
    config: &ProviderConfig,
    deps: &TurnDeps,
    turn_id: &str,
    history: &mut Vec<Message>,
    streamed_text: &mut String,
    row_id: u64,
    lines: &mut Vec<String>,
) -> Result<(), AdapterError> {
    let tools = deps.registry.tool_definitions();
    let mut last_signature: Option<String> = None;
    let mut repeated = 0usize;

    for _round in 0..MAX_TOOL_ROUNDS {
        if session.take_cancel() {
            return Ok(());
        }
        // 1) 机械 microcompact：阈值内直接裁剪旧工具结果（无需模型）。
        turn_context::microcompact_messages(history);
        // 2) LLM 摘要压缩：历史超阈值时用同一模型摘要旧历史（保留最近若干条）。
        //    摘要失败**不静默**：保留原历史继续本轮（宁可长，不丢上下文）。
        if history.len() > COMPACT_HISTORY_THRESHOLD_MESSAGES {
            let cancel_probe: Arc<dyn Fn() -> bool + Send + Sync> = {
                let session = Arc::clone(session);
                Arc::new(move || session.cancel.load(std::sync::atomic::Ordering::SeqCst))
            };
            if let Ok(compacted) =
                crate::compact::compact_history(config, &deps.context, history, cancel_probe).await
            {
                if compacted.len() < history.len() {
                    *history = compacted;
                }
            }
        }
        let round =
            run_model_roundtrip(session, config, deps, &tools, history, row_id, lines).await?;
        if !round.text.is_empty() {
            streamed_text.push_str(&round.text);
            lines.extend(session.project_assistant_delta(row_id, &round.text));
        }
        if round.tool_calls.is_empty() {
            return Ok(());
        }

        // assistant 回合带 tool_calls 入史（provider 请求体回灌的必需形状）。
        history.push(Message {
            role: MessageRole::Assistant,
            parts: Vec::new(),
            tool_call_id: None,
            tool_calls: to_tool_call_requests(&round.tool_calls),
            reasoning: Vec::new(),
        });

        for call in &round.tool_calls {
            let signature = format!("{}::{}", call.name, call.input);
            if last_signature.as_deref() == Some(signature.as_str()) {
                repeated += 1;
            } else {
                repeated = 1;
                last_signature = Some(signature);
            }
            if repeated >= REPEATED_TOOL_CALL_LIMIT {
                return Err(AdapterError::new(
                    ModelErrorCode::InvalidModelRequest,
                    format!(
                        "{FAULT_REPEATED_TOOL_CALL}: {REPEATED_TOOL_CALL_LIMIT} identical consecutive calls to {}",
                        call.name
                    ),
                ));
            }

            let input_text = call.input.to_string();
            lines.extend(session.project_tool_call(
                turn_id,
                &call.id,
                &call.name,
                "running",
                &input_text,
                None,
            ));
            let output = execute_tool(call, session, deps, turn_id).await;
            let (status, error) = match &output {
                Ok(_) => ("success".to_string(), None),
                Err(error) => (
                    if matches!(error.code, ToolErrorCode::Cancelled) {
                        "cancelled".to_string()
                    } else {
                        "error".to_string()
                    },
                    Some((error.code.as_str().to_string(), error.message.clone())),
                ),
            };
            let error_ref = error
                .as_ref()
                .map(|(code, message)| (code.as_str(), message.as_str()));
            lines.extend(session.project_tool_call(
                turn_id,
                &call.id,
                &call.name,
                &status,
                &input_text,
                error_ref,
            ));

            let result_text = match &output {
                Ok(text) => text.clone(),
                Err(error) => error.to_model_text(),
            };
            let mut tool_message = Message::text(MessageRole::Tool, result_text);
            tool_message.tool_call_id = Some(call.id.clone());
            history.push(tool_message);
        }
    }

    Err(AdapterError::new(
        ModelErrorCode::ModelRequestFailed,
        format!("{FAULT_TOOL_ROUND_LIMIT}: {MAX_TOOL_ROUNDS} model/tool rounds without a terminal answer"),
    ))
}

fn to_tool_call_requests(calls: &[ModelToolCall]) -> Vec<ToolCallRequest> {
    calls
        .iter()
        .map(|call| ToolCallRequest {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
        })
        .collect()
}

/// 权限闸门 + 执行。错误以稳定文本回灌模型（模型需要能据此修正，而不是猜）。
async fn execute_tool(
    call: &ModelToolCall,
    session: &Arc<Session>,
    deps: &TurnDeps,
    turn_id: &str,
) -> Result<String, ToolError> {
    let Some(tool) = deps.registry.get(&call.name) else {
        return Err(ToolError::new(
            ToolErrorCode::InvalidInput,
            format!("no tool named {} in this host increment", call.name),
        ));
    };
    let ctx = ToolContext {
        workspace_root: session.workspace_root.clone(),
        full_access: deps.full_access,
        trace_id: session.log_epoch(),
        turn_id: turn_id.to_string(),
        read_state: Arc::clone(&session.read_state),
        cancel: Arc::clone(&session.cancel),
        todos: Arc::clone(&session.todos),
        provider_config: deps.provider_config.clone(),
    };
    deps.registry.authorize(tool.as_ref(), &call.input, &ctx)?;
    // 阻塞池：工具执行是同步的（Bash 等子进程、nc-tools Kernel、HTTP），
    // 不能占住 turn 的 async 运行时线程——串行语义由调用点（工具回合逐个 await）
    // 保证，不依赖「谁在哪个线程上跑」。
    let tool = Arc::clone(&tool);
    let input = call.input.clone();
    tokio::task::spawn_blocking(move || tool.execute(&input, &ctx))
        .await
        .map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("tool task failed: {error}"),
            )
        })?
}

/// 单模型回合（含 provider executor 的重试/分类/流式拼装）。
async fn run_model_roundtrip(
    session: &Arc<Session>,
    config: &ProviderConfig,
    deps: &TurnDeps,
    tools: &[ToolDefinition],
    history: &[Message],
    row_id: u64,
    lines: &mut Vec<String>,
) -> Result<RoundResult, AdapterError> {
    let provider_kind = config.provider_kind().map_err(|error| {
        AdapterError::new(ModelErrorCode::ProviderNotConfigured, error.to_string())
    })?;
    let options = RequestBodyOptions {
        model: &config.model,
        system: &deps.context.system_blocks,
        messages: history,
        tools,
        max_tokens: config.max_tokens,
        stream: config.stream,
    };
    let body = match provider_kind {
        ProviderKind::Anthropic => build_anthropic_messages_body(&options),
        ProviderKind::OpenaiCompatible => build_openai_compatible_body(&options),
    }?;
    let body_json = serde_json::to_string(&body).map_err(|error| {
        AdapterError::new(ModelErrorCode::InvalidModelRequest, error.to_string())
    })?;

    let transport_request = TransportRequest {
        url: config.endpoint_url(),
        // 鉴权头按 config 的 authStyle 选择（缺省 kind 原生：anthropic → x-api-key +
        // anthropic-version，openai-compatible → Authorization: Bearer）。
        headers: auth_headers(&config.api_key, resolved_auth_style(config, provider_kind)),
        body: body_json,
    };
    let stream_request = StreamRequest {
        transport_request,
        provider_kind,
        retry: zcode_provider::retry::RetryOptions::default(),
        retry_budget: None,
        off_peak: false,
        preserve_provider_stream_boundaries: false,
        stream_idle_timeout_base_ms: None,
        stream_idle_timeout_retry_number: None,
        is_cancelled: Arc::new({
            let session = Arc::clone(session);
            move || session.cancel.load(std::sync::atomic::Ordering::SeqCst)
        }),
    };

    let mut transport = HttpTransport::new();
    let clock = RealClock;
    let execution = executor::execute(&mut transport, &clock, &stream_request).await;
    if let Some(failure) = execution.failure {
        return Err(failure);
    }

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for event in &execution.events {
        match event {
            ModelStreamEvent::TextDelta { text: chunk, .. } => text.push_str(chunk),
            ModelStreamEvent::ToolCall { tool_call } => tool_calls.push(tool_call.clone()),
            ModelStreamEvent::Finish { usage, .. } => {
                session.set_usage(json!({
                    "contextWindow": Value::Null,
                    "cumulative": cumulative_usage(usage),
                }));
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
    if !text.is_empty() {
        lines.extend(session.project_assistant_delta(row_id, &text));
    }
    Ok(RoundResult { text, tool_calls })
}

pub fn cumulative_usage(usage: &ModelUsage) -> Value {
    json!({
        "inputTokens": usage.input_tokens.unwrap_or(0),
        "outputTokens": usage.output_tokens.unwrap_or(0),
        "cacheReadTokens": usage.cache_read_tokens.unwrap_or(0),
        "cacheWriteTokens": usage.cache_write_tokens.unwrap_or(0),
    })
}

/// 出站鉴权头：由 `authStyle` 决定（`bearer` → Authorization；否则 x-api-key +
/// anthropic-version）。kind 原生缺省在 `ProviderConfig::auth_style` 里解析，
/// 这里只认最终样式——live endpoint 实测抓出的形状错误，回归锁在此。
pub fn auth_headers(api_key: &str, auth_style: &str) -> Vec<(String, String)> {
    match auth_style {
        crate::provider_config::AUTH_STYLE_BEARER => vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("authorization".to_string(), format!("Bearer {api_key}")),
        ],
        _ => vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-api-key".to_string(), api_key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ],
    }
}

fn finish_turn(
    session: &Arc<Session>,
    turn_id: &str,
    source_command_id: &str,
    header_state: &str,
    turn_phase: &str,
    lines: &mut Vec<String>,
) {
    lines.extend(session.project_turn_finished(turn_id, source_command_id, header_state));
    session.set_phase(turn_phase);
    lines.extend(emit_state_update(Arc::clone(session)));
    // 终态强制 flush：turn 的最后一帧不依赖 ticker 时序，协议上确定性收口。
    lines.extend(session.project_deltas(Vec::new(), true));
}

/// control + revision + usage 的 state.updated 投影（字段级整体替换）。
fn emit_state_update(session: Arc<Session>) -> Vec<String> {
    let phase = session.phase();
    let control = snapshot::control(&phase, session.is_running(), session.last_error().as_ref());
    let patch = json!({
        "control": control,
        "revision": session.revision(),
        "usage": session.usage(),
    });
    session.project_deltas(vec![rows::state_updated(patch)], false)
}

/// settle：把终态写回 inbox 并持久化（幂等重提的权威事实）。
fn settle(
    session: &Arc<Session>,
    admitted: Admitted,
    status: CommandStatus,
    reason_code: Option<String>,
    result: Option<Value>,
) {
    let final_ = match (&status, &reason_code) {
        (CommandStatus::Accepted, _) => CommandFinal::accepted(),
        (_, Some(code)) => CommandFinal::with_reason(status, code.clone()),
        (_, None) => CommandFinal::accepted(),
    };
    let final_ = CommandFinal { result, ..final_ };
    let merged = zcode_core::protocol::merge_final(&admitted.ack, &final_);
    let merged_value = serde_json::to_value(&merged).unwrap_or(Value::Null);
    session.persist_ack(&admitted.envelope.command_id, &merged_value);
    let _ = session.inbox.settle(admitted.ticket, final_);
    // 终态命令推进会话 revision（TS 同语义：revision 随权威投影前移）。
    session.set_revision(session.revision().saturating_add(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_headers_match_provider_shape() {
        let openai = auth_headers("k", crate::provider_config::AUTH_STYLE_BEARER);
        assert!(openai
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer k"));
        assert!(!openai.iter().any(|(name, _)| name == "x-api-key"));

        let anthropic = auth_headers("k", crate::provider_config::AUTH_STYLE_X_API_KEY);
        assert!(anthropic
            .iter()
            .any(|(name, value)| name == "x-api-key" && value == "k"));
        assert!(anthropic
            .iter()
            .any(|(name, value)| name == "anthropic-version" && value == "2023-06-01"));
        assert!(!anthropic.iter().any(|(name, _)| name == "authorization"));
    }

    #[test]
    fn auth_style_override_beats_kind_default() {
        let config = ProviderConfig {
            kind: "anthropic".to_string(),
            base_url: "https://gw.example/v1".to_string(),
            api_key: "k".to_string(),
            model: "m".to_string(),
            max_tokens: 1024,
            auth_style: Some("bearer".to_string()),
            stream: true,
            extra_models: Vec::new(),
            reasoning_levels: Vec::new(),
        };
        assert_eq!(config.auth_style().expect("style"), "bearer");
        let headers = auth_headers(&config.api_key, config.auth_style().expect("style"));
        assert!(headers.iter().any(|(name, _)| name == "authorization"));
        // 缺省时 anthropic 用原生 x-api-key。
        let mut native = config.clone();
        native.auth_style = None;
        assert_eq!(native.auth_style().expect("style"), "x-api-key");
    }
}
