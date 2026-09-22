//! 重试预算、退避曲线、off-peak、空补全、空闲超时与流重试闸门。
//!
//! 对齐 TS `retry-policy/retry-budget/runner-retry/offpeak-retry/empty-completion-retry/
//! stream-idle-timeout/workflow-model-failure-policy` 与 `canRetryStreamFailure`。

use crate::classify::is_retryable_failure;
use crate::classify::ClassifiedFailure;
use crate::{AdapterError, ModelFailureReason, ModelRetryBudget};
use serde_json::json;

// ---- 默认值与环境变量键（对齐 retry-policy.ts）----
pub const DEFAULT_MAX_RETRIES: u32 = 10;
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 2_000;
pub const DEFAULT_RETRY_BACKOFF_FACTOR: f64 = 2.0;
pub const DEFAULT_RETRY_MAX_DELAY_MS: u64 = 60_000;
pub const ENV_MODEL_RETRY_MAX_RETRIES: &str = "ZCODE_MODEL_RETRY_MAX_RETRIES";
pub const ENV_MODEL_RETRY_BASE_DELAY_MS: &str = "ZCODE_MODEL_RETRY_BASE_DELAY_MS";
pub const ENV_MODEL_RETRY_BACKOFF_FACTOR: &str = "ZCODE_MODEL_RETRY_BACKOFF_FACTOR";
pub const ENV_MODEL_RETRY_MAX_DELAY_MS: &str = "ZCODE_MODEL_RETRY_MAX_DELAY_MS";

/// Retry-After 合理上界（5 分钟）；超过后仅当小于指数退避才采纳。
pub const MAX_REASONABLE_RETRY_AFTER_MS: u64 = 5 * 60_000;
/// 状态事件里表示「无上限」的 maxAttempts 哨兵。
pub const UNBOUNDED_RETRY_MAX_ATTEMPTS: u32 = 0;
pub const EMPTY_COMPLETION_MAX_RETRIES: u32 = 1;
pub const EMPTY_COMPLETION_MESSAGE: &str =
    "Model returned no text, no tool calls, and no usage before completing the turn.";
pub const MODEL_STREAM_IDLE_TIMEOUT_RETRY_INCREMENT_MS: u64 = 30_000;
pub const DEFAULT_MODEL_STREAM_IDLE_TIMEOUT_MS: u64 = 600_000;
/// off-peak 单次排队等待钳制与无 Retry-After 的保守探测间隔。
pub const OFF_PEAK_QUEUE_WAIT_CAP_MS: u64 = 5 * 60_000;
pub const OFF_PEAK_QUEUE_WAIT_DEFAULT_MS: u64 = 60_000;
/// 与 desktop `@zcode/shared/off-peak-types.ts` 同值的 wire 标记。
pub const OFF_PEAK_TICKET_EXPIRED_MARKER: &str = "off-peak-ticket-expired";

#[derive(Debug, Clone, PartialEq)]
pub struct RetryOptions {
    /// 含首次请求的总尝试数。
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub backoff_factor: f64,
    pub max_delay_ms: u64,
    pub jitter: bool,
}

impl Default for RetryOptions {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_RETRIES + 1,
            base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
            backoff_factor: DEFAULT_RETRY_BACKOFF_FACTOR,
            max_delay_ms: DEFAULT_RETRY_MAX_DELAY_MS,
            jitter: true,
        }
    }
}

/// 解析重试配置：显式值 > 环境变量 > 默认值；`jitter` 只接受显式值。
/// `env` 为注入的只读环境视图（I/O 留在边缘层，测试零进程依赖）。
pub fn resolve_retry_options(
    explicit: Option<RetryOptions>,
    env: &dyn Fn(&str) -> Option<String>,
) -> RetryOptions {
    let defaults = RetryOptions::default();
    let env_max_retries = env(ENV_MODEL_RETRY_MAX_RETRIES)
        .and_then(|value| parse_non_negative_integer(&value))
        .map(|retries| retries.saturating_add(1))
        .map(saturating_to_u32);
    let env_base_delay =
        env(ENV_MODEL_RETRY_BASE_DELAY_MS).and_then(|value| parse_non_negative_integer(&value));
    let env_factor = env(ENV_MODEL_RETRY_BACKOFF_FACTOR)
        .as_deref()
        .and_then(parse_positive_number);
    let env_max_delay =
        env(ENV_MODEL_RETRY_MAX_DELAY_MS).and_then(|value| parse_non_negative_integer(&value));

    let max_attempts = normalize_positive_integer(
        explicit.as_ref().map(|options| options.max_attempts),
        env_max_retries.unwrap_or(defaults.max_attempts),
    );
    let base_delay_ms = normalize_non_negative_integer(
        explicit.as_ref().map(|options| options.base_delay_ms),
        env_base_delay.unwrap_or(defaults.base_delay_ms),
    );
    let backoff_factor = normalize_positive_number(
        explicit.as_ref().map(|options| options.backoff_factor),
        env_factor.unwrap_or(defaults.backoff_factor),
    );
    let max_delay_ms = normalize_non_negative_integer(
        explicit.as_ref().map(|options| options.max_delay_ms),
        env_max_delay.unwrap_or(defaults.max_delay_ms),
    );
    RetryOptions {
        max_attempts,
        base_delay_ms,
        backoff_factor,
        max_delay_ms,
        jitter: explicit
            .map(|options| options.jitter)
            .unwrap_or(defaults.jitter),
    }
}

fn parse_non_negative_integer(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed: f64 = trimmed.parse().ok()?;
    if !parsed.is_finite() || parsed < 0.0 {
        return None;
    }
    // TS 数字为 f64；越界 `as` 转换饱和，保持不 panic。
    Some(parsed.floor() as u64)
}

fn parse_positive_number(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed: f64 = trimmed.parse().ok()?;
    if !parsed.is_finite() || parsed <= 0.0 {
        return None;
    }
    Some(parsed)
}

fn normalize_positive_integer(value: Option<u32>, fallback: u32) -> u32 {
    value.map(|value| value.max(1)).unwrap_or(fallback)
}

/// TS 侧 attempts 是 f64；Rust 用 u32 承载，超界饱和而非 panic。
fn saturating_to_u32(value: u64) -> u32 {
    value.min(u32::MAX as u64) as u32
}

fn normalize_positive_number(value: Option<f64>, fallback: f64) -> f64 {
    value
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(fallback)
}

fn normalize_non_negative_integer(value: Option<u64>, fallback: u64) -> u64 {
    value.unwrap_or(fallback)
}

/// 退避延迟：`base * factor^(attempt-1)` 封顶后，优先采纳合理的 Retry-After，
/// 否则按 `round(capped * (0.5 + rand * 0.5))` 抖动（rand 由调用方注入）。
pub fn calculate_retry_delay(
    options: &RetryOptions,
    attempt: u32,
    retry_after_ms: Option<u64>,
    jitter_random: f64,
) -> u64 {
    let exponent = f64::from(attempt.saturating_sub(1));
    // u64 → f64 用 `as`（f64::from 不支持 u64）；延迟毫秒在 JS 侧本就是 f64。
    let uncapped = options.base_delay_ms as f64 * options.backoff_factor.powf(exponent);
    let capped = uncapped.min(options.max_delay_ms as f64);
    if is_reasonable_retry_after(retry_after_ms, uncapped) {
        return retry_after_ms.unwrap_or_default();
    }
    if !options.jitter || capped == 0.0 {
        return capped as u64;
    }
    (capped * (0.5 + jitter_random * 0.5)).round() as u64
}

fn is_reasonable_retry_after(value: Option<u64>, exponential_delay_ms: f64) -> bool {
    value.is_some_and(|value| {
        (value as f64) <= MAX_REASONABLE_RETRY_AFTER_MS as f64
            || (value as f64) < exponential_delay_ms
    })
}

// ============================================================
// 重试预算（对齐 retry-budget.ts）
// ============================================================

pub fn is_unbounded_retry_budget(budget: Option<ModelRetryBudget>) -> bool {
    budget == Some(ModelRetryBudget::Unbounded)
}

/// 失败之后还允许再试一次吗（unbounded 恒真）。
pub fn retry_budget_allows(
    budget: Option<ModelRetryBudget>,
    attempt: u32,
    max_attempts: u32,
) -> bool {
    is_unbounded_retry_budget(budget) || attempt < max_attempts
}

/// attempt 循环的继续条件（unbounded 恒真）。
pub fn retry_attempt_loop_continues(
    budget: Option<ModelRetryBudget>,
    attempt: u32,
    loop_max_attempts: u32,
) -> bool {
    is_unbounded_retry_budget(budget) || attempt <= loop_max_attempts
}

/// 写进状态事件的 maxAttempts：unbounded 下是哨兵 0。
pub fn retry_budget_max_attempts(budget: Option<ModelRetryBudget>, max_attempts: u32) -> u32 {
    if is_unbounded_retry_budget(budget) {
        UNBOUNDED_RETRY_MAX_ATTEMPTS
    } else {
        max_attempts
    }
}

// ============================================================
// off-peak（对齐 offpeak-retry.ts；仅 idle plan provider 生效）
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffPeakDecision {
    Queued { delay_ms: u64 },
    TicketExpired,
}

/// off-peak 特有失败语义；非 off-peak 请求恒 None（零影响）。
/// 业务码按**字符串**严格相等判定（与 TS 一致）。
pub fn resolve_off_peak_failure_decision(
    off_peak: bool,
    provider_code: Option<&str>,
    status_code: Option<u16>,
    retry_after_ms: Option<u64>,
) -> Option<OffPeakDecision> {
    if !off_peak {
        return None;
    }
    // 服务端最新契约 3102；3001 兼容滚动发布期间的旧网关。
    if matches!(provider_code, Some("3102") | Some("3001")) {
        return Some(OffPeakDecision::TicketExpired);
    }
    // 排队真实信号 = HTTP 429（3105 带 Retry-After）；裸 429 同样按排队处理。
    if provider_code == Some("3105") || status_code == Some(429) {
        return Some(OffPeakDecision::Queued {
            delay_ms: retry_after_ms
                .unwrap_or(OFF_PEAK_QUEUE_WAIT_DEFAULT_MS)
                .min(OFF_PEAK_QUEUE_WAIT_CAP_MS),
        });
    }
    None
}

pub fn off_peak_ticket_expired_message(original: &str) -> String {
    format!("{OFF_PEAK_TICKET_EXPIRED_MARKER}: {original}")
}

// ============================================================
// 空补全 / 空闲超时（对齐 empty-completion-retry / stream-idle-timeout）
// ============================================================

/// 空补全最多补 1 次，且仍受 attempt 预算与取消约束。
pub fn can_retry_empty_completion(
    retry_count: u32,
    attempt: u32,
    max_attempts: u32,
    cancelled: bool,
) -> bool {
    !cancelled && retry_count < EMPTY_COMPLETION_MAX_RETRIES && attempt < max_attempts
}

/// `base + retryNumber * 30_000`；base <= 0 表示禁用（原样返回，可能为负/零）。
pub fn resolve_stream_idle_timeout_ms(
    base_timeout_ms: Option<i64>,
    retry_number: Option<u32>,
) -> i64 {
    let base = base_timeout_ms.unwrap_or(DEFAULT_MODEL_STREAM_IDLE_TIMEOUT_MS as i64);
    if base <= 0 {
        return base;
    }
    let retry_number = i64::from(retry_number.unwrap_or(0));
    base + retry_number * MODEL_STREAM_IDLE_TIMEOUT_RETRY_INCREMENT_MS as i64
}

// ============================================================
// workflow 策略表（对齐 workflow-model-failure-policy.ts）
// ============================================================

/// 配额类业务码（Stop 集）；与主分类表分开维护、语义相反属正常。
pub const WORKFLOW_QUOTA_PROVIDER_CODES: [&str; 18] = [
    "1005",
    "1308",
    "1310",
    "1313",
    "1316",
    "1317",
    "1318",
    "1319",
    "1320",
    "1321",
    "2056",
    "20097",
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
    "exceeded_current_quota_error",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStopKind {
    Auth,
    NotConfigured,
    ModelUnavailable,
    InvalidRequest,
    Quota,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowModelFailurePolicy {
    Retry,
    Stop(WorkflowStopKind),
    ContextExceeded,
    Cancelled,
}

/// 「provider 没配好 / 选型无效」一族的 ModelErrorCode。
fn is_not_configured_code(code: crate::ModelErrorCode) -> bool {
    matches!(
        code,
        crate::ModelErrorCode::ProviderNotFound
            | crate::ModelErrorCode::ProviderNotConfigured
            | crate::ModelErrorCode::ModelConfigMissing
            | crate::ModelErrorCode::InvalidModelSelection
            | crate::ModelErrorCode::ModelRequestAuthMissing
    )
}

/// 策略表本体；顺序有意义：取消 → 配额按码 → 鉴权 → 未配置 → 模型不可用 → 超窗 → 请求无效 → 表外 retry。
pub fn resolve_workflow_model_failure_policy(
    failure: &ClassifiedFailure,
    provider_code: Option<&str>,
) -> WorkflowModelFailurePolicy {
    if failure.reason == ModelFailureReason::Cancelled {
        return WorkflowModelFailurePolicy::Cancelled;
    }
    if provider_code.is_some_and(|code| WORKFLOW_QUOTA_PROVIDER_CODES.contains(&code)) {
        return WorkflowModelFailurePolicy::Stop(WorkflowStopKind::Quota);
    }
    if failure.reason == ModelFailureReason::AuthFailed {
        return WorkflowModelFailurePolicy::Stop(WorkflowStopKind::Auth);
    }
    if failure.reason == ModelFailureReason::ProviderNotConfigured
        || is_not_configured_code(failure.code)
    {
        return WorkflowModelFailurePolicy::Stop(WorkflowStopKind::NotConfigured);
    }
    if failure.code == crate::ModelErrorCode::ModelNotFound {
        return WorkflowModelFailurePolicy::Stop(WorkflowStopKind::ModelUnavailable);
    }
    if failure.reason == ModelFailureReason::ContextExceeded {
        return WorkflowModelFailurePolicy::ContextExceeded;
    }
    // invalid_model_response 虽被标成 invalid_request，但是响应解析失败，重问一次很可能就好。
    if failure.reason == ModelFailureReason::InvalidRequest
        && failure.code != crate::ModelErrorCode::InvalidModelResponse
    {
        return WorkflowModelFailurePolicy::Stop(WorkflowStopKind::InvalidRequest);
    }
    WorkflowModelFailurePolicy::Retry
}

/// 重试闸门的策略替换点：有界预算读分类器 retryable；无上限预算读策略表。
pub fn retry_allowed_by_failure_policy(
    failure: &ClassifiedFailure,
    budget: Option<ModelRetryBudget>,
    provider_code: Option<&str>,
) -> bool {
    if !is_unbounded_retry_budget(budget) {
        return is_retryable_failure(failure);
    }
    resolve_workflow_model_failure_policy(failure, provider_code)
        == WorkflowModelFailurePolicy::Retry
}

// ============================================================
// 流失败重试闸门（对齐 canRetryStreamFailure）
// ============================================================

/// 流失败阶段（对齐 TS `type StreamFailurePhase = "request_setup" | "response_body"`，
/// 见 runner-stream.ts classifyStreamFailurePhase 的两个返回值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFailurePhase {
    /// `request_setup`：拿到 2xx 响应之前的失败（请求构造期 / 3xx-5xx 状态）。
    Setup,
    /// `response_body`：2xx 响应已到达后的失败（帧级业务错、流内传输错、流已观察到
    /// 错误/finish chunk）。compact（preserve）请求下该阶段否决重放（不可回退到 non-stream）。
    ResponseBody,
}

#[derive(Debug, Clone, Copy)]
pub struct StreamRetryInput<'a> {
    pub attempt: u32,
    pub max_attempts: u32,
    pub emitted_retry_boundary_event: bool,
    pub failure: &'a ClassifiedFailure,
    pub retry_budget: Option<ModelRetryBudget>,
    pub provider_code: Option<&'a str>,
    pub preserve_provider_stream_boundaries: bool,
    pub stream_failure_phase: Option<StreamFailurePhase>,
    /// `isCompactStaleStreamFailure` 的判定结果（idle timeout / ECONNRESET 族且无 HTTP 状态）。
    pub compact_stale: bool,
}

pub fn can_retry_stream_failure(input: &StreamRetryInput<'_>) -> bool {
    // 可见输出已发出后绝不重放（交给 core 的 stream recovery）；预算门 unbounded 恒开。
    if input.emitted_retry_boundary_event
        || !retry_budget_allows(input.retry_budget, input.attempt, input.max_attempts)
    {
        return false;
    }
    if input.failure.reason == ModelFailureReason::Cancelled {
        return false;
    }
    if input.preserve_provider_stream_boundaries && input.compact_stale {
        // compact 请求允许重试 EPIPE/ConnectionClosed；先于通用 gate 判定。
        return true;
    }
    if !retry_allowed_by_failure_policy(input.failure, input.retry_budget, input.provider_code) {
        return false;
    }
    // compact 的 SSE protocol/business body error 不再重放。
    !(input.preserve_provider_stream_boundaries
        && input.stream_failure_phase == Some(StreamFailurePhase::ResponseBody))
}

/// isCompactStaleStreamFailure：idle 超时恒真；传输码族精确匹配；有 HTTP 状态即否决码回退。
pub fn is_compact_stale_stream_failure(
    idle_timeout: bool,
    error_code: Option<&str>,
    provider_code: Option<&str>,
    http_response_status: Option<u16>,
) -> bool {
    if idle_timeout {
        return true;
    }
    if is_stale_code(error_code) {
        return true;
    }
    if http_response_status.is_some() {
        return false;
    }
    is_stale_code(provider_code)
}

fn is_stale_code(value: Option<&str>) -> bool {
    let normalized = value.map(|value| value.trim().to_ascii_uppercase());
    matches!(
        normalized.as_deref(),
        Some("ECONNRESET") | Some("EPIPE") | Some("CONNECTIONCLOSED")
    )
}

/// 分类结果 → 对外归一化错误（对齐 toAdapterError 的稳定上下文字段子集）。
pub fn adapter_error_from_failure(failure: &ClassifiedFailure, attempt: u32) -> AdapterError {
    let mut error = AdapterError::new(failure.code, failure.message.clone())
        .with_context("attempt", json!(attempt))
        .with_context("reason", json!(failure.reason))
        .with_context("retryReason", json!(failure.retry_reason))
        .with_context("retryable", json!(failure.retryable));
    if let Some(status) = failure.status_code {
        error = error.with_context("statusCode", json!(status));
    }
    if let Some(retry_after_ms) = failure.retry_after_ms {
        error = error.with_context("retryAfterMs", json!(retry_after_ms));
    }
    error
}
