//! attempt 重试循环（对齐 TS `runner-stream.ts` runStream 的 `for(;;)` 主体）。
//!
//! ```text
//! execute() ── while retryAttemptLoopContinues(budget, attempt, max)
//!   ├─ run_attempt()
//!   │    ├─ transport.open ── 非2xx：≤64KB body 浅业务 / 状态码失败
//!   │    │                   2xx+SSE：逐 chunk → FramePipeline（双层业务拦截/归一/缓冲）
//!   │    │                   2xx+非SSE：整体读 body → 浅业务 → 原始字节喂 SSE 管道
//!   │    ├─ 自然 EOF：finish_stream → 释放前奏 → EOF 合成业务错误(psf) → 空补全 hold
//!   │    └─ Ok(Completed | EmptyRetry) | Err(AttemptFailure)
//!   ├─ force_terminal：TerminalStreamChunkError 语义（TS:655）→ 直接失败
//!   └─ evaluate_failure()：classify → off-peak → 闸门 → 退避 → Retry/Terminal
//!        排队等待（off-peak queued）冻结 attempt（TS:857-860 `attempt -= 1`）
//! ```
//!
//! D2/D3：状态 sink、日志与 status 发布由调用方持有，本文件不产生回调；
//! 每个 attempt 的可见事件追加进同一 `events`，中途失败前已发出的事件保留
//! （TS yield-then-throw：消费者先看到事件、随后收到 terminal error）。

use std::sync::Arc;

use serde_json::Value;

use crate::business::read_provider_business_failure_from_body;
use crate::classify::{
    classify, context_exceeded_from_value, is_context_exceeded, parse_retry_after_ms,
    ClassifiedFailure, FailureInput, FailureKind,
};
use crate::frame_pipeline::FramePipeline;
use crate::request::ProviderKind;
use crate::retry::{
    adapter_error_from_failure, calculate_retry_delay, can_retry_empty_completion,
    can_retry_stream_failure, is_compact_stale_stream_failure, off_peak_ticket_expired_message,
    resolve_off_peak_failure_decision, resolve_stream_idle_timeout_ms,
    retry_attempt_loop_continues, OffPeakDecision, RetryOptions, StreamFailurePhase,
    StreamRetryInput,
};
use crate::transport::{AttemptClock, ProviderTransport, TransportFailure, TransportRequest};
use crate::{
    AdapterError, ModelErrorCode, ModelFailureReason, ModelRetryBudget, ModelRetryReason,
    ModelStreamEvent,
};

/// 非 2xx / 非 SSE body 的业务浅读上限（TS `MAX_BUSINESS_ERROR_BODY_CHARS`，JS 按字符切片）。
const MAX_BUSINESS_ERROR_BODY_CHARS: usize = 64_000;
/// 空闲超时文案（TS `ModelStreamIdleTimeoutError`）。
const IDLE_STALL_MESSAGE_PREFIX: &str = "Model stream stalled: no event received for ";
const IDLE_STALL_MESSAGE_SUFFIX: &str = "ms.";
const SSE_CONTENT_TYPE_MARKER: &str = "text/event-stream";

// ============================================================
// attempt 失败事实（classify 的结构化输入）
// ============================================================

/// 单 attempt 的失败事实；executor 据此分类并做重试决策。
///
/// `http_status` 是 compact stale 判定用的 transport 硬证据
/// （非 2xx = 实际状态；2xx 帧级业务 = 200；传输/解析失败 = None），
/// 与分类输入 `status`（仅非 2xx）分开，避免业务码派生状态污染 stale 判定。
#[derive(Debug, Clone)]
pub struct AttemptFailure {
    pub kind: FailureKind,
    pub message: Option<String>,
    /// 分类输入状态：仅非 2xx 置值（对齐 FailureInput.status 语义）。
    pub status: Option<u16>,
    pub retry_after_ms: Option<u64>,
    pub context_exceeded: bool,
    pub http_status: Option<u16>,
    /// EOF 合成业务错误：TS TerminalStreamChunkError 直接失败，不经重试闸门。
    pub force_terminal: bool,
    /// 失败时刻的边界观察状态（TS emittedRetryBoundaryEvent）。
    pub emitted_boundary: bool,
}

// ============================================================
// 对外入口
// ============================================================

/// 单次模型流执行：所有 attempt 的可见事件 + 最终失败（None = 自然完成）。
#[derive(Debug)]
pub struct StreamExecution {
    pub events: Vec<ModelStreamEvent>,
    pub failure: Option<AdapterError>,
}

/// 执行配置（attempt 预算、off-peak、compact 边界、取消探针）。
#[derive(Clone)]
pub struct StreamRequest {
    pub transport_request: TransportRequest,
    pub provider_kind: ProviderKind,
    pub retry: RetryOptions,
    pub retry_budget: Option<ModelRetryBudget>,
    pub off_peak: bool,
    pub preserve_provider_stream_boundaries: bool,
    /// TS `input.streamIdleTimeoutMs`（None → 默认窗口；<=0 → 禁用）。
    pub stream_idle_timeout_base_ms: Option<i64>,
    /// TS `input.request.streamIdleTimeoutRetryNumber`（core recovery 已发生的重试次数）。
    pub stream_idle_timeout_retry_number: Option<u32>,
    /// 协作式取消探针（调用方持有 token；在 attempt/读块/退避检查点读取）。
    pub is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl StreamRequest {
    fn cancelled(&self) -> bool {
        (self.is_cancelled)()
    }
}

/// attempt 循环。终态失败以 `events + failure` 返回；预算耗尽的结构性兜底
/// （TS for 条件失败 = 隐式自然返回）同样表现为 `failure: None`。
pub async fn execute<T: ProviderTransport, C: AttemptClock>(
    transport: &mut T,
    clock: &C,
    request: &StreamRequest,
) -> StreamExecution {
    let mut visible: Vec<ModelStreamEvent> = Vec::new();
    let mut attempt: u32 = 1;
    let mut empty_retry_count: u32 = 0;
    let mut failure_out: Option<AdapterError> = None;

    while retry_attempt_loop_continues(request.retry_budget, attempt, request.retry.max_attempts) {
        let retry_number = request
            .stream_idle_timeout_retry_number
            .unwrap_or(0)
            .saturating_add(attempt.saturating_sub(1));
        let idle =
            resolve_stream_idle_timeout_ms(request.stream_idle_timeout_base_ms, Some(retry_number));
        let idle = u64::try_from(idle).ok();
        let can_retry_empty = can_retry_empty_completion(
            empty_retry_count,
            attempt,
            request.retry.max_attempts,
            request.cancelled(),
        );

        match run_attempt(
            transport,
            clock,
            request,
            attempt,
            idle,
            can_retry_empty,
            &mut visible,
        )
        .await
        {
            Ok(AttemptOutcome::Completed) => break,
            Ok(AttemptOutcome::EmptyRetry { delay_ms }) => {
                // TS scheduleEmptyCompletionRetry：退避后消耗 attempt 重试（最多 1 次）。
                if let Some(cancelled) = sleep_backoff(clock, request, delay_ms, attempt).await {
                    failure_out = Some(cancelled);
                    break;
                }
                empty_retry_count += 1;
                attempt += 1;
            }
            Err(err_failure) => {
                if err_failure.force_terminal {
                    // TS line478→655 TerminalStreamChunkError：EOF 合成业务错误直接失败。
                    let classified = classify_attempt(&err_failure);
                    failure_out = Some(adapter_error_from_failure(&classified, attempt));
                    break;
                }
                match evaluate_failure(clock, request, attempt, &err_failure) {
                    AttemptDecision::Terminal(error) => {
                        failure_out = Some(error);
                        break;
                    }
                    AttemptDecision::Retry {
                        delay_ms,
                        freeze_attempt,
                    } => {
                        if let Some(cancelled) =
                            sleep_backoff(clock, request, delay_ms, attempt).await
                        {
                            failure_out = Some(cancelled);
                            break;
                        }
                        if !freeze_attempt {
                            attempt += 1;
                        }
                    }
                }
            }
        }
    }

    StreamExecution {
        events: visible,
        failure: failure_out,
    }
}

// ============================================================
// 单 attempt
// ============================================================

/// 单次物理请求的产出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptOutcome {
    /// 自然完成（含空补全被拒后的空产出完成）。
    Completed,
    /// 空补全 hold 成立 → 按 TS scheduleEmptyCompletionRetry 调度重试（消耗 attempt）。
    EmptyRetry { delay_ms: u64 },
}

async fn run_attempt<T: ProviderTransport, C: AttemptClock>(
    transport: &mut T,
    clock: &C,
    request: &StreamRequest,
    attempt: u32,
    idle_timeout_ms: Option<u64>,
    can_retry_empty: bool,
    visible: &mut Vec<ModelStreamEvent>,
) -> Result<AttemptOutcome, AttemptFailure> {
    if request.cancelled() {
        return Err(cancelled_failure());
    }

    let head = transport
        .open(request.transport_request.clone(), idle_timeout_ms)
        .await
        .map_err(transport_attempt_failure)?;
    let head_retry_after_ms = parse_retry_after_ms(&head.headers);

    // 非 2xx：TS createProviderBusinessErrorFetch 步骤②——读 ≤64KB body 浅业务；
    // 非业务则以状态码失败（响应头 Retry-After 进入退避）。
    if !(200..300).contains(&head.status) {
        let body = transport
            .read_body()
            .await
            .map_err(transport_attempt_failure)?;
        return Err(non_ok_attempt_failure(
            head.status,
            head_retry_after_ms,
            &body,
        ));
    }

    let content_type = head
        .header("content-type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let is_sse = content_type.contains(SSE_CONTENT_TYPE_MARKER);

    let mut pipeline = FramePipeline::new(
        request.provider_kind,
        request.preserve_provider_stream_boundaries,
        can_retry_empty,
        head.status,
        head_retry_after_ms,
    );
    // 本地合成 start：进前奏缓冲（preserve 下只有本地 start 算可暂存前奏）。
    pipeline.start(visible);

    if is_sse {
        loop {
            if request.cancelled() {
                return Err(attach_emitted(&mut pipeline, cancelled_failure()));
            }
            match transport.next_chunk(idle_timeout_ms).await {
                Ok(None) => break,
                Ok(Some(chunk)) => pipeline.feed(&chunk, visible)?,
                Err(failure) => {
                    return Err(attach_emitted(
                        &mut pipeline,
                        transport_attempt_failure(failure),
                    ));
                }
            }
        }
    } else {
        // 2xx 非 SSE：TS 步骤④ detectProviderBusinessError 读 body；非业务时把原始字节
        // 喂进 SSE 管道——无 `data:` 帧等价于 AI SDK 对非 SSE 响应的空事件序列，
        // 收尾由 provider flush 决定（openai flush → Finish；anthropic → 守卫）。
        if request.cancelled() {
            return Err(attach_emitted(&mut pipeline, cancelled_failure()));
        }
        let body = transport
            .read_body()
            .await
            .map_err(|failure| attach_emitted(&mut pipeline, transport_attempt_failure(failure)))?;
        let parsed = truncate_lossy_chars(&body, MAX_BUSINESS_ERROR_BODY_CHARS);
        let value: Option<Value> = serde_json::from_str(&parsed).ok();
        if let Some(parsed_value) = &value {
            if let Some(business) = read_provider_business_failure_from_body(parsed_value) {
                return Err(pipeline.business_attempt_failure(business, parsed_value));
            }
        }
        // body 是完整响应（anthropic message / openai chat.completion）时，合成与
        // 真实流式协议逐事件同构的 SSE 再喂管道——非流式 Provider 与网关也能跑同一条
        // 解析/拼装/usage 路径（complete_body.rs）。
        let synthesized =
            crate::complete_body::synthesize_sse_from_complete_body(request.provider_kind, &body);
        match synthesized {
            Some(sse) => pipeline.feed(sse.as_bytes(), visible)?,
            None => pipeline.feed(&body, visible)?,
        }
    }

    // 自然 EOF：残帧 + provider flush（openai 闭合活动块并产 Finish；anthropic 防重复守卫）。
    pipeline.finish_stream(visible)?;

    // TS line465：自然完成先释放已暂存前奏（hold 场景缓冲已空，此处为空操作）。
    visible.extend(pipeline.take_released());

    // TS line478：EOF 合成业务错误（provider_success_false）→ force_terminal。
    if let Some(failure) = pipeline.eof_business_failure() {
        return Err(failure);
    }

    // TS line505-529：空补全 hold 成立 → 本 attempt 无可见输出，调度空补全重试。
    if pipeline.finish_held() {
        let delay_ms = calculate_retry_delay(&request.retry, attempt, None, clock.jitter_random());
        return Ok(AttemptOutcome::EmptyRetry { delay_ms });
    }

    Ok(AttemptOutcome::Completed)
}

/// 非 2xx attempt 失败（对齐 detectProviderBusinessError + 非业务状态分支）。
fn non_ok_attempt_failure(status: u16, retry_after_ms: Option<u64>, body: &[u8]) -> AttemptFailure {
    let text = truncate_lossy_chars(body, MAX_BUSINESS_ERROR_BODY_CHARS);
    let value: Option<Value> = serde_json::from_str(&text).ok();
    let context_exceeded = value
        .as_ref()
        .map(context_exceeded_from_value)
        .unwrap_or(false);
    if let Some(parsed) = &value {
        if let Some(business) = read_provider_business_failure_from_body(parsed) {
            return AttemptFailure {
                kind: FailureKind::Business(business),
                message: None,
                status: Some(status),
                retry_after_ms,
                context_exceeded,
                http_status: Some(status),
                force_terminal: false,
                emitted_boundary: false,
            };
        }
    }
    AttemptFailure {
        kind: FailureKind::HttpStatus(status),
        message: None,
        status: Some(status),
        retry_after_ms,
        context_exceeded,
        http_status: Some(status),
        force_terminal: false,
        emitted_boundary: false,
    }
}

fn transport_attempt_failure(failure: TransportFailure) -> AttemptFailure {
    match failure {
        TransportFailure::Timeout => AttemptFailure {
            kind: FailureKind::Timeout,
            message: None,
            status: None,
            retry_after_ms: None,
            context_exceeded: false,
            http_status: None,
            force_terminal: false,
            emitted_boundary: false,
        },
        TransportFailure::IdleTimeout { timeout_ms } => {
            let message =
                format!("{IDLE_STALL_MESSAGE_PREFIX}{timeout_ms}{IDLE_STALL_MESSAGE_SUFFIX}");
            AttemptFailure {
                kind: FailureKind::StreamIdleTimeout {
                    message: message.clone(),
                },
                message: None,
                status: None,
                retry_after_ms: None,
                context_exceeded: is_context_exceeded(None, Some(&message)),
                http_status: None,
                force_terminal: false,
                emitted_boundary: false,
            }
        }
        TransportFailure::Network { code, message } => {
            let context_exceeded = is_context_exceeded(None, Some(&message));
            AttemptFailure {
                kind: FailureKind::Transport { code, message },
                message: None,
                status: None,
                retry_after_ms: None,
                context_exceeded,
                http_status: None,
                force_terminal: false,
                emitted_boundary: false,
            }
        }
    }
}

fn cancelled_failure() -> AttemptFailure {
    AttemptFailure {
        kind: FailureKind::Cancelled,
        message: None,
        status: None,
        retry_after_ms: None,
        context_exceeded: false,
        http_status: None,
        force_terminal: false,
        emitted_boundary: false,
    }
}

/// 传输失败发生在流中时，保留该 attempt 已观察到的边界状态。
fn attach_emitted(pipeline: &mut FramePipeline, mut failure: AttemptFailure) -> AttemptFailure {
    failure.emitted_boundary = pipeline.emitted_boundary();
    failure
}

/// body 字节 → ≤64_000 字符的 lossy 文本（JS `slice(0, 64_000)` 按字符截断的对齐）。
fn truncate_lossy_chars(body: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(body);
    text.chars().take(max_chars).collect()
}

// ============================================================
// 失败评估
// ============================================================

#[derive(Debug)]
enum AttemptDecision {
    Retry {
        delay_ms: u64,
        /// off-peak 排队：attempt 冻结原地重试（TS line857-860）。
        freeze_attempt: bool,
    },
    Terminal(AdapterError),
}

fn classify_attempt(failure: &AttemptFailure) -> ClassifiedFailure {
    let mut input = FailureInput::new(failure.kind.clone());
    if let Some(message) = &failure.message {
        input = input.with_message(message.clone());
    }
    input = input
        .with_retry_after(failure.retry_after_ms)
        .with_status(failure.status)
        .with_context_exceeded(failure.context_exceeded);
    classify(&input)
}

fn evaluate_failure<C: AttemptClock>(
    clock: &C,
    request: &StreamRequest,
    attempt: u32,
    failure: &AttemptFailure,
) -> AttemptDecision {
    let mut classified = classify_attempt(failure);
    let provider_code: Option<String> = match &failure.kind {
        FailureKind::Business(business) => business.provider_code.clone(),
        _ => None,
    };

    // off-peak：3102/3001 → ticketExpired（终止 + 标记文案）；
    // 3105/429 → queued（retryable + 固定排队等待 + 预算豁免）。
    let off_peak = resolve_off_peak_failure_decision(
        request.off_peak,
        provider_code.as_deref(),
        classified.status_code,
        classified.retry_after_ms,
    );
    match off_peak {
        Some(OffPeakDecision::TicketExpired) => {
            classified.retryable = false;
            classified.message = off_peak_ticket_expired_message(&classified.message);
        }
        Some(OffPeakDecision::Queued { .. }) => {
            classified.retryable = true;
            classified.retry_reason = ModelRetryReason::OffpeakQueued;
        }
        None => {}
    }

    // compact phase（结构等价：2xx→response_body；3xx-5xx→request_setup；无状态→未定）。
    let phase = request
        .preserve_provider_stream_boundaries
        .then(|| structural_phase(failure))
        .flatten();
    let compact_stale = is_compact_stale_stream_failure(
        matches!(failure.kind, FailureKind::StreamIdleTimeout { .. }),
        match &failure.kind {
            FailureKind::Transport { code, .. } => code.as_deref(),
            _ => None,
        },
        provider_code.as_deref(),
        failure.http_status,
    );

    let mut can_retry = can_retry_stream_failure(&StreamRetryInput {
        attempt,
        max_attempts: request.retry.max_attempts,
        emitted_retry_boundary_event: failure.emitted_boundary,
        failure: &classified,
        retry_budget: request.retry_budget,
        provider_code: provider_code.as_deref(),
        preserve_provider_stream_boundaries: request.preserve_provider_stream_boundaries,
        stream_failure_phase: phase,
        compact_stale,
    });
    // off-peak 排队 429 豁免预算（TS line722-725 / 1244-1246）：未见可见输出即强制可重试。
    if matches!(off_peak, Some(OffPeakDecision::Queued { .. })) && !failure.emitted_boundary {
        can_retry = true;
    }

    if !can_retry {
        return AttemptDecision::Terminal(adapter_error_from_failure(&classified, attempt));
    }

    let queued_delay = match off_peak {
        Some(OffPeakDecision::Queued { delay_ms }) => Some(delay_ms),
        _ => None,
    };
    let delay_ms = queued_delay.unwrap_or_else(|| {
        calculate_retry_delay(
            &request.retry,
            attempt,
            classified.retry_after_ms,
            clock.jitter_random(),
        )
    });
    AttemptDecision::Retry {
        delay_ms,
        freeze_attempt: queued_delay.is_some(),
    }
}

/// 结构 phase（仅 preserve 消费；emitted 闸门先于 phase，二者在决策面等价：
/// emitted=true 时 can_retry 首闸已否决，phase 仅喂给 compact 上下文字段）。
fn structural_phase(failure: &AttemptFailure) -> Option<StreamFailurePhase> {
    match failure.http_status {
        Some(status) if (200..300).contains(&status) => Some(StreamFailurePhase::ResponseBody),
        Some(status) if (300..600).contains(&status) => Some(StreamFailurePhase::Setup),
        _ => None,
    }
}

/// 退避等待 + 取消检查点（TS `sleep(delay, abortSignal)`：取消抛 Cancelled 直接终态）。
/// 中断精度：取消在 sleep 前/后检查点生效；等待期内的即时中断依赖下一 checkpoint
/// 捕获（宿主 token 直连属 host wiring OPEN 范围，见 README 已知差异）。
async fn sleep_backoff<C: AttemptClock>(
    clock: &C,
    request: &StreamRequest,
    delay_ms: u64,
    attempt: u32,
) -> Option<AdapterError> {
    if request.cancelled() {
        return Some(cancelled_adapter_error(attempt));
    }
    clock.sleep_ms(delay_ms).await;
    request
        .cancelled()
        .then(|| cancelled_adapter_error(attempt))
}

fn cancelled_adapter_error(attempt: u32) -> AdapterError {
    let classified = classify(&FailureInput::new(FailureKind::Cancelled));
    debug_assert_eq!(
        classified.code,
        ModelErrorCode::ModelRequestCancelled,
        "FailureKind::Cancelled 必须分类为 ModelRequestCancelled"
    );
    debug_assert_eq!(classified.reason, ModelFailureReason::Cancelled);
    adapter_error_from_failure(&classified, attempt)
}
