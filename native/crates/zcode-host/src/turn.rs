//! Turn 执行：admission → 投影 → 模型回合 → settle 的完整生命周期。
//!
//! 与 TS runtime 的关系（specs/rust-native-crates.md S3 验收）：本 increment 的
//! turn 是「单模型回合」——没有工具调度、没有 compact、没有 context 装配（这些
//! 在 README 差异表逐条列示为 OPEN）。admission 语义完整：串行 gate、幂等重提、
//! cancel 全部走 `zcode-core` 的 CommandInbox。
//!
//! fail-closed 纪律：没有 provider 配置时不发起任何模型调用，turn 以
//! `ModelErrorCode::ProviderNotConfigured` 终态投影（sessionErrorInfo +
//! turnHeader failed），命令 settle 为 failed——绝不伪造 provider、绝不吞掉错误。

use std::sync::Arc;

use serde_json::{json, Value};
use zcode_core::inbox::Admitted;
use zcode_core::protocol::{CommandFinal, CommandStatus};
use zcode_provider::executor::{self, StreamRequest};
use zcode_provider::http_transport::HttpTransport;
use zcode_provider::request::{
    build_anthropic_messages_body, build_openai_compatible_body, Message, MessageRole,
    ProviderKind, RequestBodyOptions,
};
use zcode_provider::transport::{RealClock, TransportRequest};
use zcode_provider::{AdapterError, ModelErrorCode, ModelStreamEvent, ModelUsage};

use crate::provider_config::ProviderConfig;
use crate::session::Session;
use crate::snapshot;
use crate::store::now_ms;

/// host 定义的 fault code（TS 侧 fault 词表的完整移植尚未完成，README 差异表记录）。
/// 前缀与 TS `fault.*` 命名空间对齐，值本身是本 host 的稳定契约。
pub const FAULT_PROVIDER_NOT_CONFIGURED: &str = "fault.model.providerNotConfigured";
pub const FAULT_TURN_STOPPED: &str = "fault.turn.stopped";
pub const FAULT_MODEL_FAILED: &str = "fault.model.requestFailed";

/// 系统提示：本 increment 的最小 context builder（完整 context 系统是 OPEN 项）。
const SYSTEM_PROMPT: &str = "You are zcode-host, the native Rust agent host. \
Answer concisely. Tool execution, MCP, memory and skills are not yet wired in this host; \
if the user asks for actions, explain what the TS host would do instead.";

/// 执行一个 admitted 的 sendText turn。返回需要广播的帧行（由调用方写入 stdout）。
pub async fn run_turn(
    session: Arc<Session>,
    provider: Option<&ProviderConfig>,
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

    let response_id = format!("resp-{source_command_id}");
    let model_label = provider.map(|config| config.model.as_str());
    let row_id = session.project_assistant_row(&turn_id, &response_id, model_label);

    let roundtrip = match provider {
        Some(config) => run_model_roundtrip(&session, config, &text, row_id, &mut lines).await,
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
        (roundtrip.unwrap_or_default(), "interrupted", Some(info))
    } else {
        match roundtrip {
            Ok(text) => (text, "complete", None),
            Err(error) => {
                let code = match error.code {
                    ModelErrorCode::ProviderNotConfigured => FAULT_PROVIDER_NOT_CONFIGURED,
                    _ => FAULT_MODEL_FAILED,
                };
                let info =
                    snapshot::session_error_info(code, &error.message, false, "provider", now_ms());
                (error.message.clone(), "failed", Some(info))
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

/// 单模型回合：构造请求体 → zcode-provider executor（含重试/分类）→ 流式投影。
/// usage 由 Finish 事件直接写入 session 投影（`set_usage`）。
async fn run_model_roundtrip(
    session: &Arc<Session>,
    config: &ProviderConfig,
    text: &str,
    row_id: u64,
    lines: &mut Vec<String>,
) -> Result<String, AdapterError> {
    let provider_kind = config.provider_kind().map_err(|error| {
        AdapterError::new(ModelErrorCode::ProviderNotConfigured, error.to_string())
    })?;
    let messages = vec![
        Message::text(MessageRole::System, SYSTEM_PROMPT),
        Message::text(MessageRole::User, text),
    ];
    let options = RequestBodyOptions {
        model: &config.model,
        system: &[SYSTEM_PROMPT.to_string()],
        messages: &messages,
        tools: &[],
        max_tokens: config.max_tokens,
        stream: true,
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
        headers: vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-api-key".to_string(), config.api_key.clone()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ],
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

    let mut text_out = String::new();
    for event in &execution.events {
        match event {
            ModelStreamEvent::TextDelta { text, .. } => {
                text_out.push_str(text);
                lines.extend(session.project_assistant_delta(row_id, text));
            }
            ModelStreamEvent::Finish {
                usage: event_usage, ..
            } => {
                session.set_usage(json!({
                    "contextWindow": Value::Null,
                    "cumulative": cumulative_usage(event_usage),
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
    Ok(text_out)
}

/// `ModelUsage` → `sessionUsageState.cumulative` 的四个权威键。
fn cumulative_usage(usage: &ModelUsage) -> Value {
    json!({
        "inputTokens": usage.input_tokens.unwrap_or(0),
        "outputTokens": usage.output_tokens.unwrap_or(0),
        "cacheReadTokens": usage.cache_read_tokens.unwrap_or(0),
        "cacheWriteTokens": usage.cache_write_tokens.unwrap_or(0),
    })
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
    session.project_deltas(vec![crate::rows::state_updated(patch)], false)
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
