//! attempt 重试循环集成测试：脚本化 MockTransport + 确定性 MockClock（零网络、零真实等待）。
//!
//! 退避断言使用注入 jitter=0.5 的确定值：`round(capped * 0.5 + 0.5 * 0.5 * capped)`，
//! 即 base11/factor2 时 attempt1=8ms、attempt2=17ms；off-peak 排队固定 60_000ms。

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use zcode_provider::executor::{execute, StreamExecution, StreamRequest};
use zcode_provider::request::ProviderKind;
use zcode_provider::retry::RetryOptions;
use zcode_provider::transport::{
    AttemptClock, ProviderTransport, TransportFailure, TransportRequest, TransportResponseHead,
};
use zcode_provider::{AdapterError, ModelErrorCode, ModelStreamEvent};

// ============================================================
// 脚本传输
// ============================================================

enum ChunkStep {
    Data(&'static str),
    Fail(TransportFailure),
}

enum Step {
    /// open 直接失败。
    OpenFail(TransportFailure),
    /// open 成功：状态 + 头 + 错误体/非 SSE 体 + SSE 分块序列。
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        chunks: Vec<ChunkStep>,
    },
}

struct MockTransport {
    steps: VecDeque<Step>,
    active: Option<(Vec<u8>, VecDeque<ChunkStep>)>,
    open_count: usize,
}

impl MockTransport {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: steps.into(),
            active: None,
            open_count: 0,
        }
    }
}

impl ProviderTransport for MockTransport {
    fn open(
        &mut self,
        _request: TransportRequest,
        _idle_timeout_ms: Option<u64>,
    ) -> impl Future<Output = Result<TransportResponseHead, TransportFailure>> + Send {
        let step = self.steps.pop_front();
        async move {
            self.open_count += 1;
            match step {
                Some(Step::OpenFail(failure)) => Err(failure),
                Some(Step::Respond {
                    status,
                    headers,
                    body,
                    chunks,
                }) => {
                    self.active = Some((body, chunks.into()));
                    Ok(TransportResponseHead { status, headers })
                }
                None => Err(TransportFailure::network(None, "script exhausted")),
            }
        }
    }

    fn read_body(&mut self) -> impl Future<Output = Result<Vec<u8>, TransportFailure>> + Send {
        let body = self
            .active
            .as_mut()
            .map(|(body, _)| std::mem::take(body))
            .unwrap_or_default();
        async move { Ok(body) }
    }

    fn next_chunk(
        &mut self,
        _idle_timeout_ms: Option<u64>,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, TransportFailure>> + Send {
        let outcome = match self.active.as_mut() {
            Some((_, chunks)) => match chunks.pop_front() {
                Some(ChunkStep::Data(text)) => Ok(Some(text.as_bytes().to_vec())),
                Some(ChunkStep::Fail(failure)) => Err(failure),
                None => Ok(None),
            },
            None => Ok(None),
        };
        async move { outcome }
    }
}

// ============================================================
// 确定性时钟
// ============================================================

#[derive(Default)]
struct MockClock {
    sleeps: Arc<Mutex<Vec<u64>>>,
}

impl AttemptClock for MockClock {
    fn now_ms(&self) -> u64 {
        0
    }

    fn sleep_ms(&self, delay_ms: u64) -> impl Future<Output = ()> + Send {
        self.sleeps.lock().expect("clock lock").push(delay_ms);
        std::future::ready(())
    }

    fn jitter_random(&self) -> f64 {
        0.5
    }
}

impl MockClock {
    fn sleeps(&self) -> Vec<u64> {
        self.sleeps.lock().expect("clock lock").clone()
    }
}

// ============================================================
// 请求与断言辅助
// ============================================================

fn base_request() -> StreamRequest {
    StreamRequest {
        transport_request: TransportRequest {
            url: "https://example.test/v1/chat/completions".to_string(),
            headers: vec![],
            body: "{}".to_string(),
        },
        provider_kind: ProviderKind::OpenaiCompatible,
        retry: RetryOptions {
            max_attempts: 3,
            base_delay_ms: 11,
            backoff_factor: 2.0,
            max_delay_ms: 2000,
            jitter: true,
        },
        retry_budget: None,
        off_peak: false,
        preserve_provider_stream_boundaries: false,
        stream_idle_timeout_base_ms: None,
        stream_idle_timeout_retry_number: None,
        is_cancelled: Arc::new(|| false),
    }
}

fn header(name: &str, value: &str) -> (String, String) {
    (name.to_string(), value.to_string())
}

fn sse_ok(chunks: Vec<ChunkStep>) -> Step {
    Step::Respond {
        status: 200,
        headers: vec![header("content-type", "text/event-stream")],
        body: Vec::new(),
        chunks,
    }
}

fn status_body(status: u16, body: &'static str) -> Step {
    Step::Respond {
        status,
        headers: vec![header("content-type", "application/json")],
        body: body.as_bytes().to_vec(),
        chunks: Vec::new(),
    }
}

const CONTENT_HI: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n";
const CONTENT_OK: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Ok\"}}]}\n\n";
const FINISH_NONZERO: &str = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\
\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n";
const FINISH_ZERO: &str = "data: {\"choices\":[{\"delta\":{}}],\"usage\":{}}\n\n";
const PSF_FRAME: &str = "data: {\"choices\":[{\"finish_reason\":\"provider_success_false\"}]}\n\n";
const ERROR_EVENT_3105: &str =
    "event: error\ndata: {\"error\":{\"code\":\"3105\",\"message\":\"queued\"}}\n\n";

async fn run(request: &StreamRequest, steps: Vec<Step>) -> (StreamExecution, usize, Vec<u64>) {
    let mut transport = MockTransport::new(steps);
    let clock = MockClock::default();
    let execution = execute(&mut transport, &clock, request).await;
    let open_count = transport.open_count;
    (execution, open_count, clock.sleeps())
}

fn text_concat(events: &[ModelStreamEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn count_where(
    events: &[ModelStreamEvent],
    predicate: impl Fn(&ModelStreamEvent) -> bool,
) -> usize {
    events.iter().filter(|event| predicate(event)).count()
}

fn start_count(events: &[ModelStreamEvent]) -> usize {
    count_where(events, |event| matches!(event, ModelStreamEvent::Start))
}

fn finish_count(events: &[ModelStreamEvent]) -> usize {
    count_where(events, |event| {
        matches!(event, ModelStreamEvent::Finish { .. })
    })
}

// ============================================================
// 用例
// ============================================================

#[tokio::test]
async fn natural_stream_completes_with_text_and_finish() {
    let request = base_request();
    let (execution, open_count, sleeps) = run(
        &request,
        vec![sse_ok(vec![
            ChunkStep::Data(CONTENT_HI),
            ChunkStep::Data(FINISH_NONZERO),
        ])],
    )
    .await;

    assert!(
        execution.failure.is_none(),
        "自然完成不应有失败: {:?}",
        execution.failure.as_ref().map(|error| &error.message)
    );
    assert_eq!(open_count, 1);
    assert!(sleeps.is_empty());
    assert_eq!(text_concat(&execution.events), "Hi");
    assert_eq!(start_count(&execution.events), 1);
    assert_eq!(finish_count(&execution.events), 1);
}

#[tokio::test]
async fn non_ok_500_retries_then_succeeds() {
    let request = base_request();
    let (execution, open_count, sleeps) = run(
        &request,
        vec![
            status_body(500, "{\"error\":{\"message\":\"boom\"}}"),
            sse_ok(vec![
                ChunkStep::Data(CONTENT_HI),
                ChunkStep::Data(FINISH_NONZERO),
            ]),
        ],
    )
    .await;

    assert!(execution.failure.is_none(), "第二次尝试成功后应无失败");
    assert_eq!(open_count, 2);
    assert_eq!(sleeps, vec![8], "attempt1 退避 = round(11 * 0.75)");
    assert_eq!(text_concat(&execution.events), "Hi");
}

#[tokio::test]
async fn non_ok_400_terminal_without_retry() {
    let request = base_request();
    let (execution, open_count, sleeps) = run(
        &request,
        vec![status_body(
            400,
            "{\"error\":{\"message\":\"bad request\"}}",
        )],
    )
    .await;

    let failure = execution.failure.expect("400 应终态失败");
    assert_eq!(failure.code, ModelErrorCode::InvalidModelRequest);
    assert_eq!(open_count, 1, "不可重试失败不得发起第二次物理请求");
    assert!(sleeps.is_empty());
}

#[tokio::test]
async fn non_ok_500_exhausts_attempt_budget() {
    let request = base_request();
    let (execution, open_count, sleeps) = run(
        &request,
        vec![
            status_body(500, "{\"error\":{\"message\":\"boom\"}}"),
            status_body(500, "{\"error\":{\"message\":\"boom\"}}"),
            status_body(500, "{\"error\":{\"message\":\"boom\"}}"),
        ],
    )
    .await;

    assert!(execution.failure.is_some(), "预算耗尽应终态失败");
    assert_eq!(open_count, 3, "max_attempts=3 恰好三次物理请求");
    assert_eq!(sleeps, vec![8, 17], "attempt2 退避 = round(22 * 0.75)");
}

#[tokio::test]
async fn open_network_failure_retries_then_succeeds() {
    let request = base_request();
    let (execution, open_count, sleeps) = run(
        &request,
        vec![
            Step::OpenFail(TransportFailure::network(
                Some("ECONNREFUSED"),
                "connection refused",
            )),
            sse_ok(vec![
                ChunkStep::Data(CONTENT_HI),
                ChunkStep::Data(FINISH_NONZERO),
            ]),
        ],
    )
    .await;

    assert!(execution.failure.is_none());
    assert_eq!(open_count, 2);
    assert_eq!(sleeps, vec![8]);
    assert_eq!(text_concat(&execution.events), "Hi");
}

#[tokio::test]
async fn idle_timeout_before_first_frame_retries() {
    // 空闲超时发生在任何已提交边界之前（emitted=false）→ 分类可重试 → 走退避。
    let request = base_request();
    let (execution, open_count, sleeps) = run(
        &request,
        vec![
            sse_ok(vec![ChunkStep::Fail(TransportFailure::IdleTimeout {
                timeout_ms: 30_000,
            })]),
            sse_ok(vec![
                ChunkStep::Data(CONTENT_HI),
                ChunkStep::Data(FINISH_NONZERO),
            ]),
        ],
    )
    .await;

    assert!(execution.failure.is_none(), "首个帧前的空闲超时应可重试");
    assert_eq!(open_count, 2);
    assert_eq!(sleeps, vec![8]);
    assert_eq!(text_concat(&execution.events), "Hi");
}

#[tokio::test]
async fn idle_timeout_after_output_is_terminal_and_keeps_events() {
    // TS canRetryStreamFailure 首闸：emittedRetryBoundaryEvent=true 后绝不重放
    // （交给 core 的 stream recovery）——与 preserve 无关。
    let request = base_request();
    let (execution, open_count, sleeps) = run(
        &request,
        vec![sse_ok(vec![
            ChunkStep::Data(CONTENT_HI),
            ChunkStep::Fail(TransportFailure::IdleTimeout { timeout_ms: 30_000 }),
        ])],
    )
    .await;

    let failure = execution.failure.expect("可见输出后的空闲超时必须终态");
    drop(failure);
    assert_eq!(open_count, 1, "emitted 后不得发起第二次物理请求");
    assert!(sleeps.is_empty(), "终态失败不得退避");
    assert_eq!(
        text_concat(&execution.events),
        "Hi",
        "失败前已发出的事件保留（yield-then-throw）"
    );
}

#[tokio::test]
async fn preserve_emitted_blocks_mid_stream_retry() {
    let mut request = base_request();
    request.preserve_provider_stream_boundaries = true;
    let (execution, open_count, sleeps) = run(
        &request,
        vec![sse_ok(vec![
            ChunkStep::Data(CONTENT_HI),
            ChunkStep::Fail(TransportFailure::network(
                Some("ECONNRESET"),
                "connection reset",
            )),
        ])],
    )
    .await;

    let failure = execution.failure.expect("可见输出后失败必须终态");
    assert_eq!(failure.code, ModelErrorCode::ModelRequestFailed);
    assert_eq!(open_count, 1, "emitted 边界后不得重放");
    assert!(sleeps.is_empty(), "终态失败不得发生退避");
    assert_eq!(text_concat(&execution.events), "Hi");
}

#[tokio::test]
async fn preserve_non_ok_setup_failure_is_retryable() {
    let mut request = base_request();
    request.preserve_provider_stream_boundaries = true;
    let (execution, open_count, sleeps) = run(
        &request,
        vec![
            status_body(500, "{\"error\":{\"message\":\"boom\"}}"),
            sse_ok(vec![
                ChunkStep::Data(CONTENT_HI),
                ChunkStep::Data(FINISH_NONZERO),
            ]),
        ],
    )
    .await;

    assert!(
        execution.failure.is_none(),
        "request_setup 阶段失败允许重试"
    );
    assert_eq!(open_count, 2);
    assert_eq!(sleeps, vec![8]);
}

#[tokio::test]
async fn offpeak_queued_429_freezes_attempt_and_forces_retry() {
    let mut request = base_request();
    request.off_peak = true;
    request.retry.max_attempts = 1; // 预算只有 1：靠 queued 强制 + attempt 冻结才能再试
    let (execution, open_count, sleeps) = run(
        &request,
        vec![
            status_body(429, "queue"),
            sse_ok(vec![
                ChunkStep::Data(CONTENT_HI),
                ChunkStep::Data(FINISH_NONZERO),
            ]),
        ],
    )
    .await;

    assert!(execution.failure.is_none(), "queued 应绕过预算强制重试");
    assert_eq!(
        open_count, 2,
        "attempt 冻结才可能在 max_attempts=1 下发起第二次请求"
    );
    assert_eq!(
        sleeps,
        vec![60_000],
        "排队等待 = 无 Retry-After 的保守探测间隔 60s（非指数退避）"
    );
}

#[tokio::test]
async fn offpeak_ticket_expired_terminal_with_marker() {
    let mut request = base_request();
    request.off_peak = true;
    let (execution, open_count, sleeps) = run(
        &request,
        vec![status_body(
            503,
            "{\"error\":{\"code\":\"3102\",\"message\":\"ticket gone\"}}",
        )],
    )
    .await;

    let failure: AdapterError = execution.failure.expect("ticketExpired 必须终态");
    assert!(
        failure.message.contains("off-peak-ticket-expired"),
        "终态消息需携带 wire 标记，当前: {:?}",
        failure.message
    );
    assert_eq!(open_count, 1);
    assert!(sleeps.is_empty());
}

#[tokio::test]
async fn empty_completion_hold_retries_once() {
    let request = base_request();
    let (execution, open_count, sleeps) = run(
        &request,
        vec![
            sse_ok(vec![ChunkStep::Data(FINISH_ZERO)]),
            sse_ok(vec![
                ChunkStep::Data(CONTENT_HI),
                ChunkStep::Data(FINISH_NONZERO),
            ]),
        ],
    )
    .await;

    assert!(execution.failure.is_none(), "空补全应在预算内重试一次");
    assert_eq!(open_count, 2);
    assert_eq!(sleeps, vec![8], "空补全重试走同一退避曲线");
    assert_eq!(
        finish_count(&execution.events),
        1,
        "被 hold 的 attempt 不得泄漏 Finish"
    );
    assert_eq!(
        start_count(&execution.events),
        1,
        "attempt1 的本地 start 随 hold 丢弃，只保留 attempt2 的"
    );
    assert_eq!(text_concat(&execution.events), "Hi");
}

#[tokio::test]
async fn empty_completion_budget_out_completes_empty() {
    let mut request = base_request();
    request.retry.max_attempts = 1; // attempt < max 不成立 → 不允许 hold
    let (execution, open_count, sleeps) =
        run(&request, vec![sse_ok(vec![ChunkStep::Data(FINISH_ZERO)])]).await;

    assert!(execution.failure.is_none(), "预算外空补全按自然完成返回");
    assert_eq!(open_count, 1);
    assert!(sleeps.is_empty());
    assert_eq!(
        finish_count(&execution.events),
        1,
        "未 hold 时 Finish 正常可见"
    );
    assert_eq!(text_concat(&execution.events), "");
}

#[tokio::test]
async fn frame_error_event_offpeak_queued_retries() {
    let mut request = base_request();
    request.off_peak = true;
    let (execution, open_count, sleeps) = run(
        &request,
        vec![
            sse_ok(vec![ChunkStep::Data(ERROR_EVENT_3105)]),
            sse_ok(vec![
                ChunkStep::Data(CONTENT_OK),
                ChunkStep::Data(FINISH_NONZERO),
            ]),
        ],
    )
    .await;

    assert!(
        execution.failure.is_none(),
        "帧级 3105 在 off-peak 下按排队重试"
    );
    assert_eq!(open_count, 2);
    assert_eq!(sleeps, vec![60_000]);
    assert_eq!(
        start_count(&execution.events),
        1,
        "失败 attempt 的前奏被丢弃，消费者只见一个 start"
    );
    assert_eq!(text_concat(&execution.events), "Ok");
}

#[tokio::test]
async fn psf_finish_frame_terminal_business_failure_without_retry() {
    // 路径无关断言：feed 期深扫命中（force=false + classify 不可重试）与 EOF force 合成
    // 观测一致 —— 单次物理请求 + 默认业务文案终态。max_attempts=1 使两条分叉等价。
    let mut request = base_request();
    request.retry.max_attempts = 1;
    let (execution, open_count, sleeps) =
        run(&request, vec![sse_ok(vec![ChunkStep::Data(PSF_FRAME)])]).await;

    let failure = execution
        .failure
        .expect("provider_success_false 必须终态失败");
    assert!(
        failure.message.contains("business error"),
        "应为默认业务失败文案，当前: {:?}",
        failure.message
    );
    assert_eq!(open_count, 1);
    assert!(sleeps.is_empty());
}

#[tokio::test]
async fn non_sse_2xx_json_business_offpeak_queued() {
    let mut request = base_request();
    request.off_peak = true;
    let (execution, open_count, sleeps) = run(
        &request,
        vec![
            Step::Respond {
                status: 200,
                headers: vec![header("content-type", "application/json")],
                body: b"{\"error\":{\"code\":\"3105\"}}".to_vec(),
                chunks: Vec::new(),
            },
            sse_ok(vec![
                ChunkStep::Data(CONTENT_HI),
                ChunkStep::Data(FINISH_NONZERO),
            ]),
        ],
    )
    .await;

    assert!(
        execution.failure.is_none(),
        "2xx JSON 体业务错误同样进入分类与重试"
    );
    assert_eq!(open_count, 2);
    assert_eq!(sleeps, vec![60_000]);
}

#[tokio::test]
async fn cancelled_before_open_terminal_without_request() {
    let mut request = base_request();
    request.is_cancelled = Arc::new(|| true);
    let (execution, open_count, sleeps) =
        run(&request, vec![sse_ok(vec![ChunkStep::Data(CONTENT_HI)])]).await;

    let failure = execution.failure.expect("取消必须终态失败");
    assert_eq!(failure.code, ModelErrorCode::ModelRequestCancelled);
    assert_eq!(open_count, 0, "已取消的请求不得发起物理调用");
    assert!(sleeps.is_empty());
}

#[tokio::test]
async fn cancel_checkpoint_between_failure_and_backoff() {
    // 探针调用顺序：#1 execute 的 empty-retry 预判定、#2 run_attempt 的 attempt 前
    // 检查（均返回 false 放行）；失败后退避前的检查点 #3 捕获取消 —— 不等待、不重发。
    let probes = Arc::new(AtomicUsize::new(0));
    let mut request = base_request();
    request.is_cancelled = {
        let probes = probes.clone();
        Arc::new(move || probes.fetch_add(1, Ordering::SeqCst) > 1)
    };
    let (execution, open_count, sleeps) = run(
        &request,
        vec![status_body(500, "{\"error\":{\"message\":\"boom\"}}")],
    )
    .await;

    let failure = execution.failure.expect("退避前取消应终态失败");
    assert_eq!(failure.code, ModelErrorCode::ModelRequestCancelled);
    assert_eq!(open_count, 1);
    assert!(
        sleeps.is_empty(),
        "取消发生在 sleep 检查点，不应产生真实等待"
    );
}
