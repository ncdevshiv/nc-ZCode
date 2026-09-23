//! ZCode provider runtime 的 Rust 移植（S2）。
//!
//! 行为对齐 TS `apps/zcode-cli/packages/adapters/src/model/**` 与
//! `patches/@ai-sdk__*` 两个 video/* 补丁；验收见 `specs/rust-native-crates.md`。
//! 模块边界：`sse`（帧解析）→ `openai_stream`/`anthropic_stream`（provider 变换）→
//! `assemble`（事件归一 + tool 拼装 + 重试边界缓冲）→ `executor`（重试循环 + 传输）；
//! `classify`/`retry` 为失败分类与退避决策，`schema`/`video`/`request` 为出站请求侧。

pub mod anthropic_stream;
pub mod assemble;
pub mod business;
pub mod business_finish;
pub mod classify;
pub mod complete_body;
pub mod executor;
pub mod frame_pipeline;
pub mod http_transport;
pub mod openai_stream;
pub mod request;
pub mod retry;
pub mod schema;
pub mod sse;
pub mod transport;
pub mod video;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

// ============================================================
// 契约枚举（对齐 packages/contracts/src/model/index.ts 的字面量值）
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorCode {
    InvalidModelSelection,
    ModelConfigMissing,
    ProviderNotFound,
    ProviderNotConfigured,
    ModelNotFound,
    InvalidModelRequest,
    InvalidModelResponse,
    ModelRequestFailed,
    ModelRequestAuthMissing,
    ModelRequestCancelled,
    ModelRequestTimeout,
    ModelRateLimited,
    ModelContextExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRetryReason {
    RateLimited,
    ProviderOverloaded,
    ServerError,
    NetworkError,
    Timeout,
    StreamIdleTimeout,
    StaleConnection,
    AuthRefresh,
    ReasoningSignatureRepair,
    OffpeakQueued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFailureReason {
    RateLimited,
    ProviderOverloaded,
    ServerError,
    NetworkError,
    Timeout,
    StreamIdleTimeout,
    StaleConnection,
    AuthRefresh,
    ReasoningSignatureRepair,
    OffpeakQueued,
    AuthFailed,
    Cancelled,
    ContextExceeded,
    InvalidRequest,
    ProviderNotConfigured,
    ProxyError,
    TlsError,
    Unknown,
}

/// 重试预算档位：`Unbounded` 仅放宽瞬态失败的放弃条件（workflow 流量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRetryBudget {
    Default,
    Unbounded,
}

// ============================================================
// 流事件与用量（对齐 contracts ModelStreamEvent / ModelUsage）
// ============================================================

/// compact-only 边界的语义键；正常请求不发射。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamBoundary {
    ProviderResponseStart,
    InferredContentBlockStop,
    ProviderContentBlockStart,
    ProviderContentBlockDelta,
    ProviderContentBlockStop,
    ProviderStopReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ModelStreamEvent {
    Start,
    CompactStreamBoundary {
        boundary: StreamBoundary,
        #[serde(skip_serializing_if = "Option::is_none")]
        block_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        index: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delta_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        present: Option<bool>,
    },
    TextStart {
        id: String,
    },
    TextDelta {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        text: String,
    },
    TextEnd {
        id: String,
    },
    ReasoningStart {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },
    ReasoningDelta {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },
    ReasoningEnd {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
    },
    ToolInputStart {
        id: String,
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
    },
    ToolInputDelta {
        id: String,
        delta: String,
    },
    ToolInputEnd {
        id: String,
    },
    ToolCall {
        tool_call: ModelToolCall,
    },
    Finish {
        finish_reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<Value>,
        usage: ModelUsage,
    },
    Error {
        error: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelToolCall {
    pub id: String,
    pub name: String,
    /// 执行参数；流式来源可能是 JSON 字符串，assembler 归一成对象。
    pub input: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_executed: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub server_tool_use: Option<ServerToolUse>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerToolUse {
    pub web_search_requests: Option<u64>,
    pub web_fetch_requests: Option<u64>,
}

// ============================================================
// 归一化错误（对齐 AiSdkModelAdapterError 的 code/message/context 形状）
// ============================================================

#[derive(Debug, Clone)]
pub struct AdapterError {
    pub code: ModelErrorCode,
    pub message: String,
    pub context: serde_json::Map<String, Value>,
}

impl AdapterError {
    pub fn new(code: ModelErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            context: serde_json::Map::new(),
        }
    }

    pub fn with_context(mut self, key: &str, value: Value) -> Self {
        self.context.insert(key.to_string(), value);
        self
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}", self.code, self.message)
    }
}

impl std::error::Error for AdapterError {}

// ============================================================
// 空补全判定（对齐 runner-diagnostics.ts isZeroOutputModelCompletion）
// ============================================================

/// finish 已给出、reasoning 为 0，且文本/工具/用量全空 → 疑似空补全。
pub fn is_zero_output_model_completion(
    finish_reason: Option<&str>,
    reasoning_length: usize,
    text_length: usize,
    tool_call_count: usize,
    usage: Option<&ModelUsage>,
) -> bool {
    finish_reason.is_some()
        && reasoning_length == 0
        && is_suspicious_model_completion(finish_reason, text_length, tool_call_count, usage)
}

fn is_suspicious_model_completion(
    finish_reason: Option<&str>,
    text_length: usize,
    tool_call_count: usize,
    usage: Option<&ModelUsage>,
) -> bool {
    text_length == 0
        && tool_call_count == 0
        && is_non_stop_finish(finish_reason)
        && is_zero_usage(usage)
}

/// 非 stop/tool-calls 的 finish 才可疑；缺省 finish 同样视为非 stop。
fn is_non_stop_finish(finish_reason: Option<&str>) -> bool {
    let normalized = finish_reason.unwrap_or("").trim().to_ascii_lowercase();
    normalized != "stop" && normalized != "tool-calls" && normalized != "tool_calls"
}

/// 用量全空（server tool use 计数非零即视为有用量）。
pub fn is_zero_usage(usage: Option<&ModelUsage>) -> bool {
    let Some(usage) = usage else {
        return true;
    };
    if let Some(server) = &usage.server_tool_use {
        let server_tool_use =
            server.web_search_requests.unwrap_or(0) + server.web_fetch_requests.unwrap_or(0);
        if server_tool_use > 0 {
            return false;
        }
    }
    let total = usage.total_tokens.unwrap_or_else(|| {
        usage.input_tokens.unwrap_or(0)
            + usage.output_tokens.unwrap_or(0)
            + usage.cache_read_tokens.unwrap_or(0)
            + usage.cache_write_tokens.unwrap_or(0)
            + usage.reasoning_tokens.unwrap_or(0)
    });
    total == 0
}
