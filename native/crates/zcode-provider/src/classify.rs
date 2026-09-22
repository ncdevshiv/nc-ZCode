//! 失败分类链（对齐 `failure-classifier.ts` 的判定顺序 + `failure-inspection.ts` 的探针）。
//!
//! TS 侧输入是任意 `unknown` 错误对象；本移植收敛为结构化 `FailureInput`，
//! 分支顺序、文案与 retryable 语义逐条对齐，不引入新分支。

use crate::business;
use crate::{ModelErrorCode, ModelFailureReason, ModelRetryReason};
use serde_json::Value;

/// 分类结果（对齐 ClassifiedModelFailure）。
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifiedFailure {
    pub code: ModelErrorCode,
    pub message: String,
    pub reason: ModelFailureReason,
    pub retry_reason: ModelRetryReason,
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
    pub status_code: Option<u16>,
}

/// 可判定的失败事实（替代 TS 的 unknown 错误对象）。
#[derive(Debug, Clone)]
pub enum FailureKind {
    InvalidModelResponse {
        message: Option<String>,
    },
    InvalidModelRequest {
        message: Option<String>,
    },
    Cancelled,
    StreamIdleTimeout {
        message: String,
    },
    /// provider 业务错误（HTTP JSON body / SSE error frame 归一）。
    Business(business::ProviderBusinessFailure),
    /// 非业务 HTTP 状态码（网关原样状态）。
    HttpStatus(u16),
    /// 传输层失败；code 为 Node 错误码（ETIMEDOUT/ECONNRESET/…）。
    Transport {
        code: Option<String>,
        message: String,
    },
    Timeout,
    ProviderMarkedRetryable(bool),
    Unknown,
}

#[derive(Debug, Clone)]
pub struct FailureInput {
    pub kind: FailureKind,
    /// 兜底文案（部分分支使用固定文案，见各分支）。
    pub message: Option<String>,
    pub retry_after_ms: Option<u64>,
    /// HTTP 响应状态（无响应时 None）。
    pub status_code: Option<u16>,
    /// 超窗信号（由 body code/message 预先探测，见 `is_context_exceeded`）。
    pub context_exceeded: bool,
}

impl FailureInput {
    pub fn new(kind: FailureKind) -> Self {
        Self {
            kind,
            message: None,
            retry_after_ms: None,
            status_code: None,
            context_exceeded: false,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_status(mut self, status: Option<u16>) -> Self {
        self.status_code = status;
        self
    }

    pub fn with_retry_after(mut self, retry_after_ms: Option<u64>) -> Self {
        self.retry_after_ms = retry_after_ms;
        self
    }

    pub fn with_context_exceeded(mut self, context_exceeded: bool) -> Self {
        self.context_exceeded = context_exceeded;
        self
    }
}

/// 主分类（顺序对齐 classifyModelFailure：invalid → cancel → idle → business → status 链 → 传输链）。
pub fn classify(input: &FailureInput) -> ClassifiedFailure {
    let fallback = |text: &str| input.message.clone().unwrap_or_else(|| text.to_string());
    let retry_after_ms = input.retry_after_ms;
    let status_code = input.status_code;
    let plain = |code: ModelErrorCode,
                 message: String,
                 reason: ModelFailureReason,
                 retry_reason: ModelRetryReason,
                 retryable: bool| {
        ClassifiedFailure {
            code,
            message,
            reason,
            retry_reason,
            retryable,
            retry_after_ms,
            status_code,
        }
    };

    match &input.kind {
        FailureKind::InvalidModelResponse { message } => {
            let message = message
                .clone()
                .unwrap_or_else(|| fallback("Model returned an invalid response."));
            return plain(
                ModelErrorCode::InvalidModelResponse,
                message,
                ModelFailureReason::InvalidRequest,
                ModelRetryReason::NetworkError,
                false,
            );
        }
        FailureKind::InvalidModelRequest { message } => {
            let message = message
                .clone()
                .unwrap_or_else(|| fallback("Model request configuration is invalid."));
            return plain(
                ModelErrorCode::InvalidModelRequest,
                message,
                ModelFailureReason::InvalidRequest,
                ModelRetryReason::NetworkError,
                false,
            );
        }
        FailureKind::Cancelled => {
            return plain(
                ModelErrorCode::ModelRequestCancelled,
                fallback("Model request was cancelled."),
                ModelFailureReason::Cancelled,
                ModelRetryReason::NetworkError,
                false,
            );
        }
        FailureKind::StreamIdleTimeout { message } => {
            return plain(
                ModelErrorCode::ModelRequestTimeout,
                message.clone(),
                ModelFailureReason::StreamIdleTimeout,
                ModelRetryReason::StreamIdleTimeout,
                true,
            );
        }
        FailureKind::Business(failure) => {
            return business::classify_provider_business(failure, input);
        }
        _ => {}
    }

    // ---- 非业务链 ----
    let transport_code = match &input.kind {
        FailureKind::Transport { code, .. } => code.as_deref(),
        _ => None,
    };
    if is_timeout(status_code, transport_code) {
        return plain(
            ModelErrorCode::ModelRequestTimeout,
            fallback("Model request timed out."),
            ModelFailureReason::Timeout,
            ModelRetryReason::Timeout,
            true,
        );
    }

    let status = status_code;
    if status == Some(429) {
        return plain(
            ModelErrorCode::ModelRateLimited,
            fallback("Provider rate limited the model request."),
            ModelFailureReason::RateLimited,
            ModelRetryReason::RateLimited,
            true,
        );
    }
    if status == Some(529) {
        return plain(
            ModelErrorCode::ModelRequestFailed,
            fallback("Provider is overloaded."),
            ModelFailureReason::ProviderOverloaded,
            ModelRetryReason::ProviderOverloaded,
            true,
        );
    }
    if status == Some(401) || status == Some(403) {
        return plain(
            ModelErrorCode::ProviderNotConfigured,
            fallback("Provider authentication failed."),
            ModelFailureReason::AuthFailed,
            ModelRetryReason::AuthRefresh,
            false,
        );
    }
    if status == Some(400) || status == Some(422) {
        if input.context_exceeded {
            return plain(
                ModelErrorCode::ModelContextExceeded,
                "Model request exceeded the provider context window.".to_string(),
                ModelFailureReason::ContextExceeded,
                ModelRetryReason::NetworkError,
                false,
            );
        }
        return plain(
            ModelErrorCode::InvalidModelRequest,
            fallback("Provider rejected the model request."),
            ModelFailureReason::InvalidRequest,
            ModelRetryReason::NetworkError,
            false,
        );
    }
    if input.context_exceeded {
        return plain(
            ModelErrorCode::ModelContextExceeded,
            "Model request exceeded the provider context window.".to_string(),
            ModelFailureReason::ContextExceeded,
            ModelRetryReason::NetworkError,
            false,
        );
    }
    if status.is_some_and(|status| status >= 500) {
        return plain(
            ModelErrorCode::ModelRequestFailed,
            fallback("Provider returned a server error."),
            ModelFailureReason::ServerError,
            ModelRetryReason::ServerError,
            true,
        );
    }

    if is_tls_code(transport_code) {
        return plain(
            ModelErrorCode::ModelRequestFailed,
            fallback("TLS validation failed for the provider request."),
            ModelFailureReason::TlsError,
            ModelRetryReason::NetworkError,
            false,
        );
    }
    if is_proxy_code(transport_code) {
        return plain(
            ModelErrorCode::ModelRequestFailed,
            fallback("Proxy connection failed for the provider request."),
            ModelFailureReason::ProxyError,
            ModelRetryReason::NetworkError,
            true,
        );
    }
    if is_network_code(transport_code) {
        return plain(
            ModelErrorCode::ModelRequestFailed,
            fallback("Network connection failed for the provider request."),
            ModelFailureReason::NetworkError,
            ModelRetryReason::NetworkError,
            true,
        );
    }

    if let FailureKind::ProviderMarkedRetryable(retryable) = &input.kind {
        let retryable = *retryable;
        return if retryable {
            plain(
                ModelErrorCode::ModelRequestFailed,
                fallback("Provider marked the model request as retryable."),
                ModelFailureReason::ServerError,
                ModelRetryReason::ServerError,
                true,
            )
        } else {
            plain(
                ModelErrorCode::ModelRequestFailed,
                fallback("Model request failed."),
                ModelFailureReason::Unknown,
                ModelRetryReason::NetworkError,
                false,
            )
        };
    }

    plain(
        ModelErrorCode::ModelRequestFailed,
        fallback("Model request failed."),
        ModelFailureReason::Unknown,
        ModelRetryReason::NetworkError,
        false,
    )
}

/// 对齐 isRetryableFailure：取消永不自动重试。
pub fn is_retryable_failure(failure: &ClassifiedFailure) -> bool {
    failure.retryable && failure.reason != ModelFailureReason::Cancelled
}

// ============================================================
// 探针对齐（failure-inspection.ts / failure-tls.ts）
// ============================================================

const TIMEOUT_CODES: [&str; 5] = [
    "ETIMEDOUT",
    "ETIMEOUT",
    "UND_ERR_CONNECT_TIMEOUT",
    "UND_ERR_HEADERS_TIMEOUT",
    "UND_ERR_BODY_TIMEOUT",
];

/// status 408 或传输码为超时族（isTimeoutFailure）。
pub fn is_timeout(status: Option<u16>, code: Option<&str>) -> bool {
    if status == Some(408) {
        return true;
    }
    is_timeout_code(code)
}

pub fn is_timeout_code(code: Option<&str>) -> bool {
    let Some(code) = code else {
        return false;
    };
    let normalized = code.to_ascii_uppercase();
    TIMEOUT_CODES.contains(&normalized.as_str())
}

const NETWORK_CODES: [&str; 8] = [
    "ECONNRESET",
    "ECONNREFUSED",
    "EAI_AGAIN",
    "ENOTFOUND",
    "ENETUNREACH",
    "EHOSTUNREACH",
    "UND_ERR_SOCKET",
    "UND_ERR_CONNECT_TIMEOUT",
];

/// isNetworkFailure：大写精确匹配网络错误码族。
pub fn is_network_code(code: Option<&str>) -> bool {
    let Some(code) = code else {
        return false;
    };
    let normalized = code.to_ascii_uppercase();
    NETWORK_CODES.contains(&normalized.as_str())
}

/// isTlsFailure：大写包含 CERT/TLS 或叶子签名无法验证。
pub fn is_tls_code(code: Option<&str>) -> bool {
    let Some(code) = code else {
        return false;
    };
    let normalized = code.to_ascii_uppercase();
    normalized.contains("CERT")
        || normalized.contains("TLS")
        || normalized == "UNABLE_TO_VERIFY_LEAF_SIGNATURE"
}

/// isProxyFailure：大写包含 PROXY。
pub fn is_proxy_code(code: Option<&str>) -> bool {
    code.is_some_and(|code| code.to_ascii_uppercase().contains("PROXY"))
}

const CONTEXT_EXCEEDED_CODES: [&str; 4] = [
    "context_length_exceeded",
    "context_window_exceeded",
    "model_context_exceeded",
    "model_context_window_exceeded",
];

/// isContextExceededFailure 的 code/message 探针（按短语级匹配，避免误伤普通 400）。
pub fn is_context_exceeded(code: Option<&str>, message: Option<&str>) -> bool {
    if let Some(code) = code {
        let normalized = code.to_lowercase();
        if CONTEXT_EXCEEDED_CODES.contains(&normalized.as_str()) {
            return true;
        }
    }
    let Some(message) = message.map(str::to_lowercase) else {
        return false;
    };
    message == "model_context_window_exceeded"
        || (message.contains("context") && message.contains("exceed"))
        || (message.contains("maximum context length")
            && message.contains("tokens")
            && (message.contains("requested") || message.contains("resulted")))
        || message.contains("prompt is too long")
        || (message.contains("input token count")
            && message.contains("exceed")
            && message.contains("maximum number of tokens allowed"))
        || message.contains("range of input length should be")
        || (message.contains("total message token length")
            && message.contains("exceed")
            && message.contains("model limit"))
}

/// parseRetryAfterMs：`x-should-retry=false/0` 直接屏蔽；先 `retry-after-ms`，再秒值。
/// HTTP-date 形态不解析（见报告 scope 项）。
pub fn parse_retry_after_ms(headers: &[(String, String)]) -> Option<u64> {
    let read = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.to_ascii_lowercase() == name)
            .map(|(_, value)| value.as_str())
    };
    if let Some(flag) = read("x-should-retry") {
        let normalized = flag.trim().to_ascii_lowercase();
        if normalized == "false" || normalized == "0" {
            return None;
        }
    }
    if let Some(value) = read("retry-after-ms") {
        if let Some(ms) = parse_numeric_ms(value) {
            return Some(ms);
        }
    }
    let value = read("retry-after")?.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(seconds) = value.parse::<f64>() {
        if seconds.is_finite() {
            return Some((seconds * 1000.0).round().max(0.0) as u64);
        }
    }
    // HTTP-date 形态：TS 用 Date.parse；本移植不带日历库，留待 live HTTP 阶段补齐。
    None
}

fn parse_numeric_ms(value: &str) -> Option<u64> {
    // Option 上下文：解析失败按缺省处理（丢弃错误信息）。
    let parsed = value.trim().parse::<f64>().ok()?;
    if !parsed.is_finite() || parsed < 0.0 {
        return None;
    }
    Some(parsed.round() as u64)
}

/// 供 executor 从 JSON 值快速探测超窗信号。
pub fn context_exceeded_from_value(body: &Value) -> bool {
    let code = body
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| body.get("providerCode").and_then(Value::as_str))
        .or_else(|| {
            body.get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
        });
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| body.get("msg").and_then(Value::as_str))
        .or_else(|| {
            body.get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        });
    is_context_exceeded(code, message)
}
