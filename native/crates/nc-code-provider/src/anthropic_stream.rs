//! Anthropic messages SSE 事件到 SdkPart 的变换（provider 变换层）。
//!
//! 对齐 `@ai-sdk/anthropic@3.0.81` doStream 的 TextStreamPart 语义：content block
//! 生命周期、thinking/signature、tool_use 参数拼装与 usage 累计。事件归一
//! （`to_model_stream_event`）与 tool 拼装（`StreamingToolCallAssembler`）由 executor
//! 侧统一执行，与 `openai_stream` 分层一致；tool 生命周期固定为
//! start → delta* → end → final tool_call（对齐 runner-normalization「执行参数只取
//! final tool-call」与 assembler 的 end gate）。
//! 已知范围收窄：server_tool_use 仅移植固定五个名称，其余忽略；
//! redacted_thinking/compaction 块不产生正文事件（与原移植范围一致）。

use serde_json::Value;

use crate::assemble::SdkPart;
use crate::{AdapterError, ModelErrorCode, ModelUsage, ServerToolUse};

/// server_tool_use 中被移植的工具名（未知名称忽略，与 TS 白名单一致）。
const SERVER_TOOL_NAMES: [&str; 5] = [
    "web_search",
    "web_fetch",
    "code_execution",
    "bash_20250124",
    "text_editor_20250124",
];

/// content_block_start 中已知的 block 类型集合。
const KNOWN_BLOCK_TYPES: [&str; 6] = [
    "text",
    "thinking",
    "redacted_thinking",
    "tool_use",
    "server_tool_use",
    "compaction",
];

/// content_block_delta 中已知的 delta 类型集合。
const KNOWN_DELTA_TYPES: [&str; 7] = [
    "text_delta",
    "input_json_delta",
    "signature_delta",
    "thinking_delta",
    "citations_delta",
    "web_search_tool_result_delta",
    "bart_tool_result_delta",
];

/// 响应形状非法统一归一到 invalid_model_response（对齐上游 InvalidResponseDataError）。
fn invalid_response(message: impl Into<String>) -> AdapterError {
    AdapterError::new(ModelErrorCode::InvalidModelResponse, message)
}

/// 未决 tool_use block：参数 JSON 在此累积，block stop 时收口为 final tool-call。
struct PendingTool {
    id: String,
    name: String,
    buffer: String,
}

/// 当前活动 content block 种类（anthropic 串行输出，单活动块即可）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveBlock {
    Text,
    Thinking,
    Tool,
    /// redacted_thinking/compaction/未知 server tool：已知但无正文事件。
    Other,
}

/// Anthropic 流解析器：wire 事件 → SdkPart 序列，维护 tool/usage/stop 状态。
pub struct AnthropicStreamParser {
    pending: Option<PendingTool>,
    active_block: Option<ActiveBlock>,
    /// 活动文本块 id（首条 text_delta 处惰性补发 text-start，与 openai 侧一致）。
    text_id: Option<String>,
    /// 活动 thinking 块 id（content_block_start 处发 reasoning-start）。
    reasoning_id: Option<String>,
    stop_reason: Option<String>,
    /// 收到 message_start/message_delta 即视为流已接入，EOF 兜底收尾依据。
    finish_message_id: Option<String>,
    usage: ModelUsage,
}

impl AnthropicStreamParser {
    pub fn new() -> Self {
        Self {
            pending: None,
            active_block: None,
            text_id: None,
            reasoning_id: None,
            stop_reason: None,
            finish_message_id: None,
            usage: ModelUsage::default(),
        }
    }

    /// 事件类型名。
    pub fn event_type(raw: &Value) -> Option<String> {
        raw.get("type").and_then(Value::as_str).map(str::to_owned)
    }

    /// 处理单个 SSE data 对象，产出零到多条 SdkPart。
    pub fn handle(&mut self, raw: &Value) -> Result<Vec<SdkPart>, AdapterError> {
        let event_type = Self::event_type(raw)
            .ok_or_else(|| invalid_response("anthropic 事件缺少 type 字段"))?;
        match event_type.as_str() {
            "message_start" => self.on_message_start(raw),
            "content_block_start" => self.on_block_start(raw),
            "content_block_delta" => self.on_block_delta(raw),
            "content_block_stop" => self.on_block_stop(),
            "message_delta" => self.on_message_delta(raw),
            "message_stop" => self.finish_inner(),
            "ping" => Ok(Vec::new()),
            "error" => {
                // 流内 error 事件走 error part（对齐 openai 侧），由上层分类与重试决策。
                let message = raw
                    .get("error")
                    .map(|e| {
                        e.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned()
                    })
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "anthropic 流错误".to_owned());
                Ok(vec![SdkPart::Error { message }])
            }
            other => Err(invalid_response(format!(
                "未知的 anthropic 事件类型: {other}"
            ))),
        }
    }

    /// 流结束（SSE 正常收尾）时收口活动块并产出 Finish（若尚未产出）。
    pub fn flush(&mut self) -> Result<Vec<SdkPart>, AdapterError> {
        if self.finish_message_id.is_none() && self.pending.is_none() && self.active_block.is_none()
        {
            return Ok(Vec::new());
        }
        self.finish_inner()
    }

    fn on_message_start(&mut self, raw: &Value) -> Result<Vec<SdkPart>, AdapterError> {
        if let Some(message) = raw.get("message") {
            if let Some(id) = message.get("id").and_then(Value::as_str) {
                if self.finish_message_id.is_none() {
                    self.finish_message_id = Some(id.to_owned());
                }
            }
            if let Some(usage) = message.get("usage") {
                self.apply_usage(usage);
            }
        }
        Ok(Vec::new())
    }

    /// anthropic usage → ModelUsage（对齐 runner-normalization normalizeUsage 的读取路径）。
    fn apply_usage(&mut self, usage: &Value) {
        if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.usage.input_tokens = Some(input);
        }
        if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.usage.output_tokens = Some(output);
        }
        if let Some(cached) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.usage.cache_read_tokens = Some(cached);
        }
        if let Some(cached) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.usage.cache_write_tokens = Some(cached);
        }
        // normalizeUsage：raw.server_tool_use 有任一计数才带上 serverToolUse。
        if let Some(server) = usage
            .get("server_tool_use")
            .filter(|value| value.is_object())
        {
            let web_search = server.get("web_search_requests").and_then(Value::as_u64);
            let web_fetch = server.get("web_fetch_requests").and_then(Value::as_u64);
            if web_search.is_some() || web_fetch.is_some() {
                self.usage.server_tool_use = Some(ServerToolUse {
                    web_search_requests: web_search,
                    web_fetch_requests: web_fetch,
                });
            }
        }
        self.sync_total_tokens();
    }

    /// total = input + output（AI SDK LanguageModelUsage 同构；两者齐备才写入）。
    fn sync_total_tokens(&mut self) {
        if let (Some(input), Some(output)) = (self.usage.input_tokens, self.usage.output_tokens) {
            self.usage.total_tokens = Some(input + output);
        }
    }

    fn on_block_start(&mut self, raw: &Value) -> Result<Vec<SdkPart>, AdapterError> {
        let block = raw
            .get("content_block")
            .ok_or_else(|| invalid_response("content_block_start 缺少 content_block"))?;
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_response("content_block 缺少 type"))?;
        if !KNOWN_BLOCK_TYPES.contains(&block_type) {
            return Err(invalid_response(format!(
                "未知的 content_block 类型: {block_type}"
            )));
        }
        match block_type {
            "text" => {
                self.active_block = Some(ActiveBlock::Text);
                Ok(Vec::new())
            }
            "thinking" => {
                // 有签名 thinking：signature 由 signature_delta 携带（见 on_block_delta）。
                let id = block_index_id(raw, "reasoning");
                self.reasoning_id = Some(id.clone());
                self.active_block = Some(ActiveBlock::Thinking);
                Ok(vec![SdkPart::ReasoningStart {
                    id,
                    provider_metadata: None,
                }])
            }
            "tool_use" => self.on_tool_use_start(block),
            "server_tool_use" => self.on_server_tool_use_start(block),
            _ => {
                // redacted_thinking / compaction：无正文事件，stop 处同样 no-op。
                self.active_block = Some(ActiveBlock::Other);
                Ok(Vec::new())
            }
        }
    }

    fn on_tool_use_start(&mut self, block: &Value) -> Result<Vec<SdkPart>, AdapterError> {
        let id = block
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let start = SdkPart::ToolInputStart {
            id: id.clone(),
            tool_name: name.clone(),
            provider_executed: None,
        };
        self.active_block = Some(ActiveBlock::Tool);
        match block.get("input") {
            // input 缺省/显式 null：等待 input_json_delta 增量拼装。
            None | Some(Value::Null) => {
                self.pending = Some(PendingTool {
                    id,
                    name,
                    buffer: String::new(),
                });
                Ok(vec![start])
            }
            Some(value) => {
                let object = value
                    .as_object()
                    .ok_or_else(|| invalid_response("tool_use input 必须是对象"))?;
                if object.is_empty() {
                    // 流式 tool_use 起始 input 为 {}：真实参数由 input_json_delta 给出。
                    self.pending = Some(PendingTool {
                        id,
                        name,
                        buffer: String::new(),
                    });
                    return Ok(vec![start]);
                }
                // 完整 input 一次性补齐：start + 展示用 argument delta + end + final call。
                self.pending = None;
                let text = serde_json::to_string(value)
                    .map_err(|e| invalid_response(format!("序列化 tool input 失败: {e}")))?;
                let input = value.clone();
                Ok(vec![
                    start,
                    SdkPart::ToolInputDelta {
                        id: id.clone(),
                        delta: text,
                    },
                    SdkPart::ToolInputEnd { id: id.clone() },
                    SdkPart::ToolCall {
                        tool_call_id: id,
                        tool_name: name,
                        input,
                        provider_executed: None,
                    },
                ])
            }
        }
    }

    fn on_server_tool_use_start(&mut self, block: &Value) -> Result<Vec<SdkPart>, AdapterError> {
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !SERVER_TOOL_NAMES.contains(&name) {
            // 未知 server tool：不透传，与 TS ignore 一致。
            self.active_block = Some(ActiveBlock::Other);
            return Ok(Vec::new());
        }
        let id = block
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        // web_search 等以空参数开始并立即结束，随后是 final tool-call。
        self.active_block = Some(ActiveBlock::Other);
        Ok(vec![
            SdkPart::ToolInputStart {
                id: id.clone(),
                tool_name: name.to_owned(),
                provider_executed: None,
            },
            SdkPart::ToolInputEnd { id: id.clone() },
            SdkPart::ToolCall {
                tool_call_id: id,
                tool_name: name.to_owned(),
                input: serde_json::json!({}),
                provider_executed: None,
            },
        ])
    }

    fn on_block_delta(&mut self, raw: &Value) -> Result<Vec<SdkPart>, AdapterError> {
        let delta = raw
            .get("delta")
            .ok_or_else(|| invalid_response("content_block_delta 缺少 delta"))?;
        let delta_type = delta
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_response("delta 缺少 type"))?;
        if !KNOWN_DELTA_TYPES.contains(&delta_type) {
            return Err(invalid_response(format!("未知的 delta 类型: {delta_type}")));
        }
        match delta_type {
            "text_delta" => {
                if self.active_block != Some(ActiveBlock::Text) {
                    return Err(invalid_response("text_delta 不在 text block 内"));
                }
                let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    return Ok(Vec::new());
                }
                // 首条 delta 惰性补发 text-start（对齐 openai 侧 is_active_text 门控）。
                if self.text_id.is_none() {
                    self.text_id = Some(block_index_id(raw, "text"));
                }
                let id = self.text_id.clone();
                Ok(vec![SdkPart::TextDelta {
                    id,
                    text: text.to_owned(),
                }])
            }
            "thinking_delta" => {
                let id = self
                    .reasoning_id
                    .clone()
                    .filter(|_| self.active_block == Some(ActiveBlock::Thinking))
                    .ok_or_else(|| invalid_response("thinking_delta 不在 thinking block 内"))?;
                let thinking = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                Ok(vec![SdkPart::ReasoningDelta {
                    id: Some(id),
                    delta: thinking.to_owned(),
                    provider_metadata: None,
                }])
            }
            "signature_delta" => {
                let id = self
                    .reasoning_id
                    .clone()
                    .filter(|_| self.active_block == Some(ActiveBlock::Thinking))
                    .ok_or_else(|| invalid_response("signature_delta 不在 thinking block 内"))?;
                let signature = delta.get("signature").and_then(Value::as_str).unwrap_or("");
                // core 的 reasoning block.providerOptions 直接取 delta 的 providerMetadata，
                // 历史回写读 providerOptions.anthropic.signature（对齐 reasoning-history）。
                Ok(vec![SdkPart::ReasoningDelta {
                    id: Some(id),
                    delta: String::new(),
                    provider_metadata: Some(serde_json::json!({
                        "anthropic": { "signature": signature }
                    })),
                }])
            }
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let Some(pending) = self.pending.as_mut() else {
                    return Err(invalid_response("input_json_delta 缺少未决 tool_use block"));
                };
                pending.buffer.push_str(partial);
                Ok(vec![SdkPart::ToolInputDelta {
                    id: pending.id.clone(),
                    delta: partial.to_owned(),
                }])
            }
            // citations / server tool result 等已知但无正文事件的 delta。
            _ => Ok(Vec::new()),
        }
    }

    fn on_block_stop(&mut self) -> Result<Vec<SdkPart>, AdapterError> {
        match self.active_block.take() {
            Some(ActiveBlock::Text) => Ok(match self.text_id.take() {
                Some(id) => vec![SdkPart::TextEnd { id }],
                None => Vec::new(),
            }),
            Some(ActiveBlock::Thinking) => Ok(match self.reasoning_id.take() {
                Some(id) => vec![SdkPart::ReasoningEnd {
                    id,
                    provider_metadata: None,
                }],
                None => Vec::new(),
            }),
            Some(ActiveBlock::Tool) => {
                // 完整 input 已在 start 处一次性收口时 pending 为 None：stop 不重复发。
                match self.pending.take() {
                    Some(pending) => self.close_pending_tool(pending),
                    None => Ok(Vec::new()),
                }
            }
            Some(ActiveBlock::Other) | None => Ok(Vec::new()),
        }
    }

    /// 活动 tool pending → tool-input-end + final tool-call（输入在此定型）。
    fn close_pending_tool(&self, pending: PendingTool) -> Result<Vec<SdkPart>, AdapterError> {
        let input = if pending.buffer.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str::<Value>(&pending.buffer)
                .map_err(|e| invalid_response(format!("tool_use 参数 JSON 无法解析: {e}")))?
        };
        Ok(vec![
            SdkPart::ToolInputEnd {
                id: pending.id.clone(),
            },
            SdkPart::ToolCall {
                tool_call_id: pending.id,
                tool_name: pending.name,
                input,
                provider_executed: None,
            },
        ])
    }

    fn on_message_delta(&mut self, raw: &Value) -> Result<Vec<SdkPart>, AdapterError> {
        if let Some(delta) = raw.get("delta") {
            if let Some(stop) = delta.get("stop_reason").and_then(Value::as_str) {
                if !stop.is_empty() {
                    self.stop_reason = Some(stop.to_owned());
                }
            }
            if let Some(id) = delta.get("id").and_then(Value::as_str) {
                self.finish_message_id = Some(id.to_owned());
            }
        }
        if let Some(usage) = raw.get("usage") {
            self.apply_usage(usage);
        }
        Ok(Vec::new())
    }

    /// 收口所有活动状态并发射 Finish（finish_reason 映射见 map_anthropic_stop_reason）。
    fn finish_inner(&mut self) -> Result<Vec<SdkPart>, AdapterError> {
        let mut parts = Vec::new();
        if let Some(pending) = self.pending.take() {
            parts.extend(self.close_pending_tool(pending)?);
        }
        if let Some(id) = self.text_id.take() {
            parts.push(SdkPart::TextEnd { id });
        }
        if let Some(id) = self.reasoning_id.take() {
            parts.push(SdkPart::ReasoningEnd {
                id,
                provider_metadata: None,
            });
        }
        self.active_block = None;
        self.sync_total_tokens();
        let finish_reason = self
            .stop_reason
            .take()
            .map(|stop| map_anthropic_stop_reason(&stop))
            .unwrap_or("unknown");
        parts.push(SdkPart::Finish {
            finish_reason: finish_reason.to_owned(),
            usage: self.usage.clone(),
            provider_metadata: None,
        });
        self.finish_message_id = None;
        Ok(parts)
    }
}

impl Default for AnthropicStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

/// 按 content block 的 index 派生事件 id（text-/reasoning- 前缀，仅用于流内关联）。
fn block_index_id(raw: &Value, prefix: &str) -> String {
    let index = raw.get("index").and_then(Value::as_u64).unwrap_or(0);
    format!("{prefix}-{index}")
}

/// Anthropic stop_reason → SDK finish reason 字面量（mapAnthropicStopReason）。
pub fn map_anthropic_stop_reason(stop: &str) -> &'static str {
    match stop {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool-calls",
        "refusal" => "content-filter",
        _ => "unknown",
    }
}
