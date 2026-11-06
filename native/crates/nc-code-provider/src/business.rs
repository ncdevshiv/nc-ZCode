//! provider 业务码映射与业务失败归一。
//!
//! 对齐 `failure-provider-business-codes.ts`（全表 + 终止/可重试族）、
//! `model-execution.ts`（body/SSE frame 业务码读取器与文案归一）与
//! `failure-classifier.ts` 的业务分支顺序。

use crate::classify::{classify, is_network_code, is_timeout, FailureInput};
use crate::sse::{parse_sse_frame, SseParser};
use crate::{ModelErrorCode, ModelFailureReason, ModelRetryReason};
use serde_json::Value;

/// ZCode 自己的包装码：出现在 body.code 时跳过，取真实上游码。
pub const PROVIDER_BUSINESS_ERROR_WRAPPER_CODE: &str = "PROVIDER_BUSINESS_ERROR";
/// provider 文案归一上限（对齐 MAX_PROVIDER_MESSAGE_CHARS）。
pub const MAX_PROVIDER_MESSAGE_CHARS: usize = 1_000;

/// 部分 OpenAI-compatible provider 包装的可恢复上游网络故障码。
const PROVIDER_NETWORK_BUSINESS_CODES: [&str; 1] = ["1234"];
const PROVIDER_INTERNAL_NETWORK_MESSAGES: [&str; 3] = [
    "500 internal network error",
    "internal network error",
    "internal network failure",
];

/// 从 HTTP JSON body / SSE frame 归一出的业务失败。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderBusinessFailure {
    pub provider_code: Option<String>,
    pub provider_message: Option<String>,
    pub provider_request_id: Option<String>,
    /// `providerCodeToStatusCode` 结果（纯数字且 100..=599）。
    pub status_code: Option<u16>,
    /// body `error.type`（用于 internal network 族判定）。
    pub body_error_type: Option<String>,
}

impl ProviderBusinessFailure {
    pub fn message(&self) -> String {
        self.provider_message
            .clone()
            .unwrap_or_else(|| "Provider returned a business error.".to_string())
    }
}

/// 业务码映射的一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusinessMapping {
    pub code: ModelErrorCode,
    pub reason: ModelFailureReason,
    pub retry_reason: ModelRetryReason,
    pub retryable: bool,
}

const TERMINAL_RATE_LIMIT: BusinessMapping = BusinessMapping {
    code: ModelErrorCode::ModelRateLimited,
    reason: ModelFailureReason::RateLimited,
    retry_reason: ModelRetryReason::RateLimited,
    retryable: false,
};
const TERMINAL_BUSINESS: BusinessMapping = BusinessMapping {
    code: ModelErrorCode::ModelRequestFailed,
    reason: ModelFailureReason::Unknown,
    retry_reason: ModelRetryReason::NetworkError,
    retryable: false,
};
const RETRYABLE_RATE_LIMIT: BusinessMapping = BusinessMapping {
    code: ModelErrorCode::ModelRateLimited,
    reason: ModelFailureReason::RateLimited,
    retry_reason: ModelRetryReason::RateLimited,
    retryable: true,
};
const RETRYABLE_OVERLOAD: BusinessMapping = BusinessMapping {
    code: ModelErrorCode::ModelRequestFailed,
    reason: ModelFailureReason::ProviderOverloaded,
    retry_reason: ModelRetryReason::ProviderOverloaded,
    retryable: true,
};
const SERVER_RETRYABLE: BusinessMapping = BusinessMapping {
    code: ModelErrorCode::ModelRequestFailed,
    reason: ModelFailureReason::ServerError,
    retry_reason: ModelRetryReason::ServerError,
    retryable: true,
};
const NETWORK_RETRYABLE: BusinessMapping = BusinessMapping {
    code: ModelErrorCode::ModelRequestFailed,
    reason: ModelFailureReason::NetworkError,
    retry_reason: ModelRetryReason::NetworkError,
    retryable: true,
};
const CONTEXT_EXCEEDED: BusinessMapping = BusinessMapping {
    code: ModelErrorCode::ModelContextExceeded,
    reason: ModelFailureReason::ContextExceeded,
    retry_reason: ModelRetryReason::NetworkError,
    retryable: false,
};
const RATE_LIMITED_TERMINAL_FAMILY: BusinessMapping = TERMINAL_RATE_LIMIT;
const RATE_RETRYABLE: BusinessMapping = RETRYABLE_RATE_LIMIT;

/// 业务码 → 分类映射（表内顺序无冲突；表外返回 None 走通用兜底链）。
pub fn provider_business_code_mapping(code: &str) -> Option<BusinessMapping> {
    let mapping = match code {
        "500" | "1120" | "1230" | "2007" => SERVER_RETRYABLE,
        "1006" => BusinessMapping {
            code: ModelErrorCode::ProviderNotConfigured,
            reason: ModelFailureReason::AuthFailed,
            retry_reason: ModelRetryReason::AuthRefresh,
            retryable: false,
        },
        "1005" => BusinessMapping {
            code: ModelErrorCode::ModelRequestFailed,
            reason: ModelFailureReason::InvalidRequest,
            retry_reason: ModelRetryReason::NetworkError,
            retryable: false,
        },
        "3006" => BusinessMapping {
            code: ModelErrorCode::ModelNotFound,
            reason: ModelFailureReason::InvalidRequest,
            retry_reason: ModelRetryReason::NetworkError,
            retryable: false,
        },
        "3001" => BusinessMapping {
            code: ModelErrorCode::InvalidModelRequest,
            reason: ModelFailureReason::InvalidRequest,
            retry_reason: ModelRetryReason::NetworkError,
            retryable: false,
        },
        "3007" => BusinessMapping {
            code: ModelErrorCode::InvalidModelRequest,
            reason: ModelFailureReason::AuthFailed,
            retry_reason: ModelRetryReason::AuthRefresh,
            retryable: false,
        },
        // 并发上限族：3008/3009/3010 + 1304/1308/1310/1313 终止型限流。
        "3008" | "3009" | "3010" | "1304" | "1308" | "1310" | "1313" => {
            RATE_LIMITED_TERMINAL_FAMILY
        }
        "1234" => NETWORK_RETRYABLE,
        "1261" => CONTEXT_EXCEEDED,
        // 终止型业务失败（未知业务码也显式终止，避免 429 兜底误重试）。
        "1113" | "1309" | "1311" | "1008" | "1314" | "1315" => TERMINAL_BUSINESS,
        "1302" | "1303" | "1305" | "3002" => RATE_RETRYABLE,
        "1312" => RETRYABLE_OVERLOAD,
        "insufficient_quota"
        | "credit_balance_exhausted"
        | "organization_spend_limit_exceeded"
        | "project_spend_limit_exceeded"
        | "organization_usage_limit_exceeded"
        | "exceeded_current_quota_error"
        | "2056"
        | "20097"
        | "1316"
        | "1317"
        | "1318"
        | "1319"
        | "1320"
        | "1321" => TERMINAL_RATE_LIMIT,
        "rate_limit_reached_error" | "rate_limit_error" => RETRYABLE_RATE_LIMIT,
        "engine_overloaded_error" | "overloaded_error" => RETRYABLE_OVERLOAD,
        _ => return None,
    };
    Some(mapping)
}

/// 业务失败分类（顺序对齐 classifyProviderBusinessFailure：表 → 超窗 → 超时 → 状态 → 网络族 → ≥500 → 未知）。
pub fn classify_provider_business(
    failure: &ProviderBusinessFailure,
    input: &FailureInput,
) -> crate::classify::ClassifiedFailure {
    let message = failure.message();
    let retry_after_ms = input.retry_after_ms;
    let provider_code = failure.provider_code.as_deref();
    // HTTP 状态优先，业务码推导状态兜底；仅 4xx/5xx 参与判定。
    let effective_status = normalize_http_status(input.status_code)
        .or_else(|| normalize_http_status(failure.status_code));

    let plain = |code: ModelErrorCode,
                 message: String,
                 reason: ModelFailureReason,
                 retry_reason: ModelRetryReason,
                 retryable: bool| {
        crate::classify::ClassifiedFailure {
            code,
            message,
            reason,
            retry_reason,
            retryable,
            retry_after_ms,
            status_code: effective_status,
        }
    };

    if let Some(mapped) = provider_code.and_then(provider_business_code_mapping) {
        return plain(
            mapped.code,
            message,
            mapped.reason,
            mapped.retry_reason,
            mapped.retryable,
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
    if is_timeout(effective_status, provider_code) {
        return plain(
            ModelErrorCode::ModelRequestTimeout,
            message,
            ModelFailureReason::Timeout,
            ModelRetryReason::Timeout,
            true,
        );
    }
    let retry_reason = if effective_status.is_some_and(|status| status >= 500) {
        ModelRetryReason::ServerError
    } else {
        ModelRetryReason::NetworkError
    };

    match effective_status {
        Some(401) | Some(403) => {
            return plain(
                ModelErrorCode::ProviderNotConfigured,
                message,
                ModelFailureReason::AuthFailed,
                ModelRetryReason::AuthRefresh,
                false,
            );
        }
        Some(404) => {
            return plain(
                ModelErrorCode::ModelNotFound,
                message,
                ModelFailureReason::InvalidRequest,
                ModelRetryReason::NetworkError,
                false,
            );
        }
        Some(400) | Some(422) => {
            return plain(
                ModelErrorCode::InvalidModelRequest,
                message,
                ModelFailureReason::InvalidRequest,
                ModelRetryReason::NetworkError,
                false,
            );
        }
        Some(429) => {
            return plain(
                ModelErrorCode::ModelRateLimited,
                message,
                ModelFailureReason::RateLimited,
                ModelRetryReason::RateLimited,
                true,
            );
        }
        _ => {}
    }

    if is_retryable_business_network_failure(failure) {
        return plain(
            ModelErrorCode::ModelRequestFailed,
            message,
            ModelFailureReason::NetworkError,
            ModelRetryReason::NetworkError,
            true,
        );
    }
    if effective_status.is_some_and(|status| status >= 500) {
        return plain(
            ModelErrorCode::ModelRequestFailed,
            message,
            ModelFailureReason::ServerError,
            retry_reason,
            true,
        );
    }
    plain(
        ModelErrorCode::ModelRequestFailed,
        message,
        ModelFailureReason::Unknown,
        retry_reason,
        false,
    )
}

/// isRetryableProviderBusinessNetworkFailure：传输码被包进业务码 / 网络族码 / internal api_error 文案。
fn is_retryable_business_network_failure(failure: &ProviderBusinessFailure) -> bool {
    if let Some(code) = &failure.provider_code {
        if is_network_code(Some(code)) {
            return true;
        }
        if PROVIDER_NETWORK_BUSINESS_CODES.contains(&code.as_str()) {
            return true;
        }
        let normalized = code.to_lowercase();
        if normalized == "network_error" || normalized == "network_error_retryable" {
            return true;
        }
    }
    if failure.body_error_type.as_deref() != Some("api_error") {
        return false;
    }
    let Some(message) = &failure.provider_message else {
        return false;
    };
    let normalized = message.trim().to_lowercase();
    PROVIDER_INTERNAL_NETWORK_MESSAGES.contains(&normalized.as_str())
}

// ============================================================
// body / SSE frame 业务失败读取（对齐 model-execution.ts）
// ============================================================

/// 从 HTTP JSON body 读取业务失败；无失败信号（success!=false 且业务码为零/缺失）→ None。
pub fn read_provider_business_failure_from_body(body: &Value) -> Option<ProviderBusinessFailure> {
    let record = body.as_object()?;
    let error_record = record.get("error");
    let provider_code = record
        .get("providerCode")
        .and_then(read_code)
        .or_else(|| {
            error_record
                .and_then(|error| error.get("providerCode"))
                .and_then(read_code)
        })
        .or_else(|| record.get("error_code").and_then(read_code))
        .or_else(|| {
            error_record
                .and_then(|error| error.get("error_code"))
                .and_then(read_code)
        })
        .or_else(|| record.get("code").and_then(read_code))
        .or_else(|| {
            error_record
                .and_then(|error| error.get("code"))
                .and_then(read_code)
        });

    let provider_message = read_message(record);
    let failed_by_success = record.get("success").and_then(Value::as_bool) == Some(false);
    let failed_by_code = is_non_zero_code(provider_code.as_deref());
    if !failed_by_success && !failed_by_code {
        return None;
    }
    Some(ProviderBusinessFailure {
        status_code: provider_code.as_deref().and_then(provider_code_to_status),
        provider_code,
        provider_message,
        provider_request_id: read_request_id(record),
        body_error_type: error_record
            .and_then(|error| error.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// 整段 SSE 文本的帧级业务失败扫描（对齐 detectProviderBusinessSseFrameFailure）：
/// data trim 后为空或 `[DONE]` 跳过；JSON 命中业务表直接返回；否则 `event: error` 兜底。
pub fn detect_business_sse_frames(text: &str) -> Option<ProviderBusinessFailure> {
    let mut parser = SseParser::new();
    let mut events = parser.feed(text.as_bytes());
    events.extend(parser.finish());
    for event in events {
        let Some(data) = event.data.as_deref() else {
            continue;
        };
        let trimmed = data.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            continue;
        }
        let Ok(body) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(failure) = read_provider_business_failure_from_body(&body) {
            return Some(failure);
        }
        let is_error_event = event
            .event
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("error"));
        if !is_error_event {
            continue;
        }
        // event:error 帧即使没有业务码也构成失败（码可能缺省，走未知兜底链）。
        let record = body.as_object();
        let error_record = record.and_then(|record| record.get("error"));
        let provider_code = record
            .and_then(|record| record.get("providerCode").and_then(read_code))
            .or_else(|| record.and_then(|record| record.get("error_code").and_then(read_code)))
            .or_else(|| record.and_then(|record| record.get("code").and_then(read_code)))
            .or_else(|| error_record.and_then(|error| error.get("code").and_then(read_code)));
        return Some(ProviderBusinessFailure {
            status_code: provider_code.as_deref().and_then(provider_code_to_status),
            provider_message: record.and_then(read_message),
            provider_request_id: record.and_then(read_request_id),
            provider_code,
            body_error_type: error_record
                .and_then(|error| error.get("type"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    None
}

/// 流内 JSON 帧的业务失败探测（对齐 SSE 帧包装器：先业务表，后 event:error 兜底）。
pub fn business_failure_from_json_frame(
    event: Option<&str>,
    value: &Value,
) -> Option<ProviderBusinessFailure> {
    if let Some(failure) = read_provider_business_failure_from_body(value) {
        return Some(failure);
    }
    if !event.is_some_and(|name| name.eq_ignore_ascii_case("error")) {
        return None;
    }
    let record = value.as_object()?;
    let error_record = record.get("error");
    let provider_code = record
        .get("providerCode")
        .and_then(read_code)
        .or_else(|| record.get("error_code").and_then(read_code))
        .or_else(|| record.get("code").and_then(read_code))
        .or_else(|| {
            error_record
                .and_then(|error| error.get("code"))
                .and_then(read_code)
        });
    Some(ProviderBusinessFailure {
        status_code: provider_code.as_deref().and_then(provider_code_to_status),
        provider_message: read_message(record),
        provider_request_id: read_request_id(record),
        provider_code,
        body_error_type: error_record
            .and_then(|error| error.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// toProviderCode：数字原样、非空字符串 trim、包装码丢弃。
fn read_code(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => {
            if number.is_f64() && number.as_f64().is_some_and(|v| !v.is_finite()) {
                return None;
            }
            Some(number.to_string())
        }
        Value::String(text) => {
            let normalized = text.trim();
            if normalized.is_empty() {
                return None;
            }
            if normalized.eq_ignore_ascii_case(PROVIDER_BUSINESS_ERROR_WRAPPER_CODE) {
                return None;
            }
            Some(normalized.to_string())
        }
        _ => None,
    }
}

/// isNonZeroProviderCode：数字须有限且非零；字符串按 JS Number 判定（非数字串视为真）。
fn is_non_zero_code(code: Option<&str>) -> bool {
    let Some(code) = code else {
        return false;
    };
    match code.parse::<f64>() {
        Ok(number) => number.is_finite() && number != 0.0,
        Err(_) => true,
    }
}

/// providerCodeToStatusCode：纯数字且落在 100..=599。
fn provider_code_to_status(code: &str) -> Option<u16> {
    if !code.bytes().all(|byte| byte.is_ascii_digit()) || code.is_empty() {
        return None;
    }
    let status: u16 = code.parse().ok()?;
    (100..=599).contains(&status).then_some(status)
}

/// 分类链只消费 4xx/5xx。
fn normalize_http_status(value: Option<u16>) -> Option<u16> {
    value.filter(|status| (400..=599).contains(status))
}

/// readProviderMessage：msg/message 优先级 + 空白折叠 + 1000 字符截断。
fn read_message(record: &serde_json::Map<String, Value>) -> Option<String> {
    let error_record = record.get("error");
    let candidates = [
        record.get("msg"),
        record.get("message"),
        error_record.and_then(|error| error.get("msg")),
        error_record.and_then(|error| error.get("message")),
        error_record.filter(|error| error.is_string()),
    ];
    for candidate in candidates.into_iter().flatten() {
        let Value::String(text) = candidate else {
            continue;
        };
        let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            continue;
        }
        if normalized.chars().count() > MAX_PROVIDER_MESSAGE_CHARS {
            let truncated: String = normalized
                .chars()
                .take(MAX_PROVIDER_MESSAGE_CHARS)
                .collect();
            return Some(format!("{truncated}..."));
        }
        return Some(normalized);
    }
    None
}

/// readProviderRequestId：request_id/requestId/id（顶层与 error 内）。
fn read_request_id(record: &serde_json::Map<String, Value>) -> Option<String> {
    let error_record = record.get("error");
    let candidates = [
        record.get("request_id"),
        record.get("requestId"),
        record.get("id"),
        error_record.and_then(|error| error.get("request_id")),
        error_record.and_then(|error| error.get("requestId")),
        error_record.and_then(|error| error.get("id")),
    ];
    for candidate in candidates.into_iter().flatten() {
        let Value::String(text) = candidate else {
            continue;
        };
        let normalized = text.trim();
        if normalized.is_empty() {
            continue;
        }
        if normalized.chars().count() > MAX_PROVIDER_MESSAGE_CHARS {
            let truncated: String = normalized
                .chars()
                .take(MAX_PROVIDER_MESSAGE_CHARS)
                .collect();
            return Some(format!("{truncated}..."));
        }
        return Some(normalized.to_string());
    }
    None
}

/// 供 executor 复用的帧解析入口（保持 parse_sse_frame 公开可测）。
pub fn frame_event_for_diagnostics(frame: &str) -> String {
    let event = parse_sse_frame(frame);
    event.event.unwrap_or_default()
}

/// 单位测试/诊断用：业务码是否可重试（表语义直读）。
pub fn business_code_retryable(code: &str) -> Option<bool> {
    provider_business_code_mapping(code).map(|mapping| mapping.retryable)
}

/// 分类便捷入口（测试与 executor 共用，避免重复拼装）。
pub fn classify_business(
    failure: &ProviderBusinessFailure,
    retry_after_ms: Option<u64>,
    status_code: Option<u16>,
    context_exceeded: bool,
) -> crate::classify::ClassifiedFailure {
    let input = FailureInput::new(crate::classify::FailureKind::Business(failure.clone()))
        .with_retry_after(retry_after_ms)
        .with_status(status_code)
        .with_context_exceeded(context_exceeded);
    classify(&input)
}
