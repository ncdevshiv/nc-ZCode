//! 出站请求体构造：内部消息模型 → openai-compatible `chat/completions` /
//! anthropic `/messages` 请求体。
//!
//! 对齐 TS `tool-transform.ts`（strict 四重门控：contract 声明 + providerKind
//! anthropic + 首方 modelId + 子集可表达）、`anthropic-stream-compat.ts`
//! （messages[] 内 system 保持 string content、顶层 system 为块数组）与两个
//! `video/*` patch 的字节形状。
//!
//! golden-request 语料（runner-debug 录制）在本 checkout **不可用**，字段级
//! byte-parity 门保持 OPEN：这里只断言 patch/compat 源码已证明的形状，
//! 其余键序与字段假设在 README「divergences」记录。

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::schema::{is_anthropic_first_party_model_id, to_strict_tool_schema};
use crate::video::{
    anthropic_video_part, decoded_base64_len, ensure_video_within_budget,
    openai_compatible_video_part, MEDIA_BUDGET_CURRENT_VIDEO_TOO_LARGE,
};
use crate::{AdapterError, ModelErrorCode};

/// 请求目标 provider：决定 wire 形状，executor 据此选择流解析器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    OpenaiCompatible,
}

// ============================================================
// 内部消息模型（缝上沿 contracts ModelRequest 的收窄子集）
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    // TS 侧 Message.role 默认为 'user'（对齐默认对话消息），derive Default 需保持该语义。
    #[default]
    User,
    Assistant,
    Tool,
}

/// 多模态内容部件；`data` 均为 base64 文本（附件解析层已编码）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentPart {
    Text { text: String },
    Image { media_type: String, data: String },
    Video { media_type: String, data: String },
}

/// assistant 历史中的 tool 调用（回灌请求体）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// assistant 历史中的 reasoning 块（anthropic 往返 signature 的载体）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReasoningBlock {
    pub text: String,
    /// anthropic.signature；缺席时序列化为空串（对齐 providerOptionsForReasoningBlock）。
    pub signature: Option<String>,
    /// anthropic.redactedData；存在时走 redacted_thinking 块。
    pub redacted_data: Option<String>,
}

/// 序列化形状是**持久化契约**，不是内部实现的副产品：会话的模型历史要落进
/// `transcript.jsonl` 并在重启后逐字段还原（specs/rust-native-crates.md S3
/// session memory 规则 2）。因此字段名保持 snake_case 原样落盘、由本模块自己
/// 读回；跨语言读取方只能是 host 的 transcript 回放，不存在第二种解码器。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    /// 纯文本消息用 `text()` 构造；富内容走 parts。
    pub parts: Vec<ContentPart>,
    /// role=Tool 时的 tool_use_id。
    pub tool_call_id: Option<String>,
    /// role=Assistant 时回灌的 tool 调用。
    pub tool_calls: Vec<ToolCallRequest>,
    /// role=Assistant 时回灌的 reasoning 块。
    pub reasoning: Vec<ReasoningBlock>,
}

impl Message {
    pub fn text(role: MessageRole, text: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![ContentPart::Text { text: text.into() }],
            ..Self::default()
        }
    }

    /// 纯文本内容拼接（空则 None）。
    fn plain_text(&self) -> Option<String> {
        let texts: Vec<&str> = self
            .parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if texts.is_empty() {
            return None;
        }
        Some(texts.join("\n\n"))
    }

    fn has_only_text(&self) -> bool {
        self.parts
            .iter()
            .all(|part| matches!(part, ContentPart::Text { .. }))
    }
}

/// 工具定义；`strict` 来自 contract 声明（是否启用还须过 provider/模型门控）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub strict: bool,
}

/// 构造入参（borrow，调用方持有 canonical 数据）。
#[derive(Debug, Clone, Copy)]
pub struct RequestBodyOptions<'a> {
    pub model: &'a str,
    /// 顶层 system 块序列（prompt 段；anthropic 保持块数组形状）。
    pub system: &'a [String],
    pub messages: &'a [Message],
    pub tools: &'a [ToolDefinition],
    pub max_tokens: u32,
    pub stream: bool,
}

fn invalid_request(message: impl Into<String>) -> AdapterError {
    AdapterError::new(ModelErrorCode::InvalidModelRequest, message)
}

/// 视频预算：按解码后字节数判定，超限 fail-closed（稳定 wire 码进 context）。
fn ensure_video_budget(data: &str) -> Result<(), AdapterError> {
    ensure_video_within_budget(decoded_base64_len(data)).map_err(|error| {
        AdapterError::new(ModelErrorCode::InvalidModelRequest, error.to_string())
            .with_context("code", json!(MEDIA_BUDGET_CURRENT_VIDEO_TOO_LARGE))
    })
}

// ============================================================
// openai-compatible：POST /v1/chat/completions
// ============================================================

/// chat/completions body（键序稳定：tests 以序列化串断言快照）。
pub fn build_openai_compatible_body(
    options: &RequestBodyOptions<'_>,
) -> Result<Value, AdapterError> {
    let mut messages = Vec::new();
    // 顶层 system 先于会话消息（openai 无顶层 system 参数）。
    for prompt in options.system {
        messages.push(json!({"role": "system", "content": prompt}));
    }
    for message in options.messages {
        messages.push(openai_message(message)?);
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(options.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("max_tokens".into(), json!(options.max_tokens));
    if !options.tools.is_empty() {
        let tools: Vec<Value> = options
            .tools
            .iter()
            .map(openai_tool)
            .collect::<Result<_, _>>()?;
        body.insert("tools".into(), Value::Array(tools));
    }
    body.insert("stream".into(), json!(options.stream));
    Ok(Value::Object(body))
}

fn openai_message(message: &Message) -> Result<Value, AdapterError> {
    match message.role {
        MessageRole::Tool => {
            // tool 结果：文本拼接为字符串；非文本部件本层无法表达 → fail-closed。
            let text = message
                .plain_text()
                .ok_or_else(|| invalid_request("openai-compatible tool 消息只支持文本内容"))?;
            Ok(json!({
                "role": "tool",
                "tool_call_id": message.tool_call_id.clone().unwrap_or_default(),
                "content": text,
            }))
        }
        MessageRole::Assistant => {
            let mut out = Map::new();
            out.insert("role".into(), json!("assistant"));
            // 文本仅在存在时写入；带 tool_calls 时 content 为 null（openai wire 惯例）。
            match (message.plain_text(), message.has_only_text()) {
                (Some(text), true)
                    if message.tool_calls.is_empty() && message.reasoning.is_empty() =>
                {
                    out.insert("content".into(), json!(text));
                }
                (Some(_), _) | (None, _) => {
                    if !message.tool_calls.is_empty() {
                        out.insert("content".into(), Value::Null);
                    } else if let Some(text) = message.plain_text() {
                        out.insert("content".into(), json!(text));
                    }
                }
            }
            // reasoning → reasoning_content（openai-compatible 流侧同名字段读取）。
            if !message.reasoning.is_empty() {
                let joined = message
                    .reasoning
                    .iter()
                    .map(|block| block.text.as_str())
                    .collect::<Vec<_>>()
                    .join("");
                out.insert("reasoning_content".into(), json!(joined));
            }
            if !message.tool_calls.is_empty() {
                let calls: Vec<Value> = message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                // arguments 是 JSON 字符串（openai wire 固定形状）。
                                "arguments": serde_json::to_string(&call.input)
                                    .unwrap_or_else(|_| String::from("{}")),
                            }
                        })
                    })
                    .collect();
                out.insert("tool_calls".into(), Value::Array(calls));
            }
            Ok(Value::Object(out))
        }
        MessageRole::System | MessageRole::User => {
            let role = match message.role {
                MessageRole::System => "system",
                _ => "user",
            };
            let content = if message.has_only_text() {
                // 纯文本压成 string（openai 侧惯例）。
                match message.plain_text() {
                    Some(text) => json!(text),
                    None => json!(""),
                }
            } else {
                let mut parts = Vec::new();
                for part in &message.parts {
                    parts.push(openai_content_part(part)?);
                }
                Value::Array(parts)
            };
            let mut out = Map::new();
            out.insert("role".into(), json!(role));
            out.insert("content".into(), content);
            Ok(Value::Object(out))
        }
    }
}

fn openai_content_part(part: &ContentPart) -> Result<Value, AdapterError> {
    match part {
        ContentPart::Text { text } => Ok(json!({"type": "text", "text": text})),
        ContentPart::Image { media_type, data } => Ok(json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{media_type};base64,{data}")},
        })),
        ContentPart::Video { media_type, data } => {
            ensure_video_budget(data)?;
            // patch：video/* → {type:"video_url", video_url:{url: data URL}}。
            Ok(openai_compatible_video_part(media_type, data, None))
        }
    }
}

/// openai-compatible 永不启用 strict（门控要求 providerKind=anthropic）。
fn openai_tool(tool: &ToolDefinition) -> Result<Value, AdapterError> {
    let mut function = Map::new();
    function.insert("name".into(), json!(tool.name));
    if let Some(description) = &tool.description {
        function.insert("description".into(), json!(description));
    }
    function.insert("parameters".into(), tool.input_schema.clone());
    Ok(json!({
        "type": "function",
        "function": Value::Object(function),
    }))
}

// ============================================================
// anthropic：POST /v1/messages
// ============================================================

/// /messages body（键序稳定：tests 以序列化串断言快照）。
pub fn build_anthropic_messages_body(
    options: &RequestBodyOptions<'_>,
) -> Result<Value, AdapterError> {
    let mut messages = Vec::new();
    for message in options.messages {
        messages.push(anthropic_message(message)?);
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(options.model));
    body.insert("messages".into(), Value::Array(messages));
    if !options.system.is_empty() {
        // 顶层 system 保持块数组（anthropic-stream-compat「顶层 system[] 保持原样」）。
        let blocks: Vec<Value> = options
            .system
            .iter()
            .map(|prompt| json!({"type": "text", "text": prompt}))
            .collect();
        body.insert("system".into(), Value::Array(blocks));
    }
    body.insert("max_tokens".into(), json!(options.max_tokens));
    if !options.tools.is_empty() {
        let tools: Vec<Value> = options
            .tools
            .iter()
            .map(|tool| anthropic_tool(tool, options.model))
            .collect::<Result<_, _>>()?;
        body.insert("tools".into(), Value::Array(tools));
    }
    body.insert("stream".into(), json!(options.stream));
    Ok(Value::Object(body))
}

fn anthropic_message(message: &Message) -> Result<Value, AdapterError> {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User | MessageRole::Tool => "user",
        MessageRole::Assistant => "assistant",
    };
    let mut out = Map::new();
    out.insert("role".into(), json!(role));

    if message.role == MessageRole::Tool {
        let text = message
            .plain_text()
            .ok_or_else(|| invalid_request("anthropic tool 消息只支持文本内容"))?;
        // tool_result 在 user 消息内容里；tool_call_id 缺失时用空串（上游已校验）。
        out.insert(
            "content".into(),
            json!([{
                "type": "tool_result",
                "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                "content": text,
            }]),
        );
        return Ok(Value::Object(out));
    }

    if message.role == MessageRole::Assistant {
        // 文本 only → string content；带 thinking/tool_use → 块数组。
        if message.has_only_text() && message.tool_calls.is_empty() && message.reasoning.is_empty()
        {
            out.insert(
                "content".into(),
                json!(message.plain_text().unwrap_or_default()),
            );
            return Ok(Value::Object(out));
        }
        let mut blocks: Vec<Value> = Vec::new();
        for block in &message.reasoning {
            if let Some(data) = &block.redacted_data {
                blocks.push(json!({"type": "redacted_thinking", "data": data}));
                continue;
            }
            blocks.push(json!({
                "type": "thinking",
                "thinking": block.text,
                // 缺席签名用空串（serializer 支持的 unsigned 表示）。
                "signature": block.signature.clone().unwrap_or_default(),
            }));
        }
        for part in &message.parts {
            blocks.push(anthropic_content_part(part)?);
        }
        for call in &message.tool_calls {
            blocks.push(json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": call.input,
            }));
        }
        out.insert("content".into(), Value::Array(blocks));
        return Ok(Value::Object(out));
    }

    // system / user：system 保持 string content（compat 规则）；user 用块数组
    // （AI SDK 侧最终形态；string→单元素 block 的展开在上游发生）。
    if message.role == MessageRole::System {
        let text = message
            .plain_text()
            .ok_or_else(|| invalid_request("anthropic system 消息只支持文本内容"))?;
        out.insert("content".into(), json!(text));
        return Ok(Value::Object(out));
    }
    let mut blocks = Vec::new();
    for part in &message.parts {
        blocks.push(anthropic_content_part(part)?);
    }
    out.insert("content".into(), Value::Array(blocks));
    Ok(Value::Object(out))
}

fn anthropic_content_part(part: &ContentPart) -> Result<Value, AdapterError> {
    match part {
        ContentPart::Text { text } => Ok(json!({"type": "text", "text": text})),
        ContentPart::Image { media_type, data } => Ok(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data},
        })),
        ContentPart::Video { media_type, data } => {
            ensure_video_budget(data)?;
            // patch：video/* → {type:"video", source:{type:"base64",...}}；无 cache 不写键。
            Ok(anthropic_video_part(media_type, data, None))
        }
    }
}

/// strict 四重门控：contract.strict + providerKind=anthropic + 首方 modelId
/// + 子集可表达；全过才折叠 schema 并带 `strict:true`，否则原样发送、无 strict 键。
fn anthropic_tool(tool: &ToolDefinition, model_id: &str) -> Result<Value, AdapterError> {
    let eligible = tool.strict
        && is_anthropic_first_party_model_id(Some(model_id))
        && !is_anthropic_first_party_model_id(None); // 保持与 TS 相同的显式判定顺序
    let folded = if eligible {
        to_strict_tool_schema(&tool.input_schema)
    } else {
        None
    };

    let mut out = Map::new();
    out.insert("name".into(), json!(tool.name));
    if let Some(description) = &tool.description {
        out.insert("description".into(), json!(description));
    }
    match folded {
        Some(schema) => {
            out.insert("input_schema".into(), schema);
            out.insert("strict".into(), json!(true));
        }
        None => {
            out.insert("input_schema".into(), tool.input_schema.clone());
        }
    }
    Ok(Value::Object(out))
}
