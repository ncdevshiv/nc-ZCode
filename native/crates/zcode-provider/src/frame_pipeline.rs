//! 单 attempt 帧流水线：SSE → 双层业务拦截 → provider 解析 → 归一事件 → 边界缓冲/空完成 hold。
//!
//! 每帧顺序逐条对齐 TS 两个调用点：
//!
//! ```text
//! wire chunk ─▶ ① fetch 层浅业务   detectProviderBusinessSseFrameFailure（model-execution.ts:815，
//!                                   TransformStream 每帧无条件浅读 + `event: error` 兜底）
//!            ─▶ ② 诊断记录 note + preserve 原始帧观察/边界投影（runner-stream.ts:1022 → 1026-1029）
//!            ─▶ ③ runner 层深业务   detectProviderBusinessFinishError（runner-stream.ts:1030，
//!                                   finish 形状门控 + 候选记录深扫）
//!            ─▶ ④ provider 解析    anthropic.handle / openai.on_chunk（TS: provider 变换层）
//!            ─▶ ⑤ 归一 + 拼装 + 缓冲（TS:1041 toModelStreamEvent → 1042 error 拦截
//!                                   → 1066 assembler.handle → applyStreamEventsToRetryBoundary）
//! ```
//!
//! 中途失败前已产生的事件写入 `visible` 并随失败一起返回（TS yield-then-throw 语义）：
//! 失败输出中保留该 attempt 已经可见的事件，retry 路径下这些事件已保证不会再被重放。

use serde_json::Value;

use crate::anthropic_stream::AnthropicStreamParser;
use crate::assemble::{RetryBoundaryBuffer, SdkPart, StreamingToolCallAssembler};
use crate::business::{business_failure_from_json_frame, ProviderBusinessFailure};
use crate::business_finish::{detect_business_failure_deep, frame_carries_business_signal};
use crate::classify::{context_exceeded_from_value, is_context_exceeded, FailureKind};
use crate::executor::AttemptFailure;
use crate::openai_stream::OpenAiStreamTransform;
use crate::request::ProviderKind;
use crate::sse::{JsonSseFrame, JsonSseStream, SseError};
use crate::{
    is_zero_output_model_completion, AdapterError, ModelErrorCode, ModelStreamEvent, StreamBoundary,
};

/// `provider_success_false`：AI SDK 把 success:false 提升成 finish 原因的字面量。
const PROVIDER_SUCCESS_FALSE: &str = "provider_success_false";

// ============================================================
// 诊断（对齐 runner-diagnostics.ts StreamDiagnostics 的可判定子集）
// ============================================================

/// 零产出/空补全判定所需的计数集合。
///
/// 只统计语义判定用得上的字段：文本/推理字符数、tool-call 数、finish 原因与用量。
/// TS 侧的 statusSink/日志字段（lastChunk 时间戳等）不属于 D2/D3 职责，未移植。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StreamDiagnostics {
    pub text_delta_chars: usize,
    pub reasoning_delta_chars: usize,
    pub tool_call_count: usize,
    pub finish_reason: Option<String>,
    /// 原始 finish 原因（wire `rawFinishReason`/`finish_reason`/`providerMetadata.rawFinishReason`），
    /// 驱动 TS runner-stream.ts:478 的 EOF 合成业务错误探测。
    pub raw_finish_reason: Option<String>,
    pub usage: Option<crate::ModelUsage>,
}

impl StreamDiagnostics {
    fn record(&mut self, event: &ModelStreamEvent) {
        match event {
            ModelStreamEvent::TextDelta { text, .. } => {
                self.text_delta_chars += text.chars().count();
            }
            ModelStreamEvent::ReasoningDelta { text, .. } => {
                self.reasoning_delta_chars += text.chars().count();
            }
            ModelStreamEvent::ToolCall { .. } => {
                self.tool_call_count += 1;
            }
            ModelStreamEvent::Finish {
                finish_reason,
                usage,
                ..
            } => {
                self.finish_reason = Some(finish_reason.clone());
                self.usage = Some(usage.clone());
            }
            _ => {}
        }
    }

    /// 对齐 recordStreamChunkDiagnostic 对 rawFinishReason 的记录面：
    /// 只有携带这些键的帧才可能更新（顶层、providerMetadata、choices[0]）。
    /// 值为 null 时不更新（JS 侧 `stringProperty` 同样跳过）。
    fn note_raw_finish_reason(&mut self, value: &Value) {
        let Some(record) = value.as_object() else {
            return;
        };
        let raw = record
            .get("rawFinishReason")
            .and_then(Value::as_str)
            .or_else(|| record.get("finish_reason").and_then(Value::as_str))
            .or_else(|| {
                record
                    .get("providerMetadata")
                    .and_then(Value::as_object)
                    .and_then(|metadata| metadata.get("rawFinishReason"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                record
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|choices| choices.first())
                    .and_then(|choice| choice.get("finish_reason"))
                    .and_then(Value::as_str)
            });
        if let Some(raw) = raw {
            self.raw_finish_reason = Some(raw.to_string());
        }
    }

    /// 对齐 isZeroOutputModelCompletion：finish 已给出、reasoning 为 0、文本/工具/用量全空。
    pub fn zero_output(&self) -> bool {
        is_zero_output_model_completion(
            self.finish_reason.as_deref(),
            self.reasoning_delta_chars,
            self.text_delta_chars,
            self.tool_call_count,
            self.usage.as_ref(),
        )
    }
}

// ============================================================
// 流水线本体
// ============================================================

/// 一个 attempt 的帧流水线：持有 SSE 缓冲、双 provider 解析器、拼装器与边界缓冲。
///
/// 两个解析器同时持有、按 `kind` 激活——未激活的解析器不接收帧、无状态成本，
/// 避免为每个 attempt 分配枚举包装（行为与 TS 按 providerConfig.kind 建厂一致）。
pub struct FramePipeline {
    kind: ProviderKind,
    preserve: bool,
    /// 进入本 attempt 时的空补全预算判定（`can_retry_empty_completion`），见 executor。
    empty_retry_allowed: bool,
    /// 本 attempt 响应头状态（2xx）：帧级业务失败挂 responseStatus + Retry-After。
    head_status: u16,
    head_retry_after_ms: Option<u64>,
    sse: JsonSseStream,
    anthropic: AnthropicStreamParser,
    openai: OpenAiStreamTransform,
    assembler: StreamingToolCallAssembler,
    buffer: RetryBoundaryBuffer,
    diagnostics: StreamDiagnostics,
    finish_held: bool,
}

impl FramePipeline {
    pub fn new(
        kind: ProviderKind,
        preserve: bool,
        empty_retry_allowed: bool,
        head_status: u16,
        head_retry_after_ms: Option<u64>,
    ) -> Self {
        Self {
            kind,
            preserve,
            empty_retry_allowed,
            head_status,
            head_retry_after_ms,
            sse: JsonSseStream::new(),
            anthropic: AnthropicStreamParser::new(),
            openai: OpenAiStreamTransform::new(),
            assembler: StreamingToolCallAssembler::new(),
            buffer: RetryBoundaryBuffer::new(),
            diagnostics: StreamDiagnostics::default(),
            finish_held: false,
        }
    }

    /// 本地合成 `start`（TS: AI SDK 在读取 provider 流前合成 stream-start chunk）：
    /// 走前奏缓冲——preserve 下只有本地 `start` 算可暂存前奏（buffer.push 同款判定）。
    pub fn start(&mut self, visible: &mut Vec<ModelStreamEvent>) {
        visible.extend(self.buffer.push(ModelStreamEvent::Start, self.preserve));
    }

    /// 已观察到真实 provider 事件（重试边界已提交）。
    pub fn emitted_boundary(&self) -> bool {
        self.buffer.emitted()
    }

    /// 空补全 hold 是否生效（本批 finish + 前奏已按 TS 语义丢弃）。
    pub fn finish_held(&self) -> bool {
        self.finish_held
    }

    pub fn diagnostics(&self) -> &StreamDiagnostics {
        &self.diagnostics
    }

    /// 自然收尾：释放仍暂存的前奏（仅成功路径调用；失败路径前奏随 attempt 丢弃）。
    pub fn take_released(&mut self) -> Vec<ModelStreamEvent> {
        self.buffer.take_committed()
    }

    /// 失败路径：丢弃暂存前奏（消费者不会看到半截输入）。
    pub fn discard_prelude(&mut self) {
        self.buffer.discard();
    }

    // AttemptFailure 内嵌业务失败结构（~160B，clippy 阈值 128B）；错误路径每流至多
    // 触发一次，Box 化需改动全部调用点（executor/测试）且无行为收益 —— 不采纳。
    #[allow(clippy::result_large_err)]
    pub fn feed(
        &mut self,
        chunk: &[u8],
        visible: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), AttemptFailure> {
        let frames = self
            .sse
            .feed(chunk)
            .map_err(|err| self.attach(sse_failure(err)))?;
        for frame in &frames {
            self.process_frame(frame, visible)?;
        }
        Ok(())
    }

    /// 流自然结束：残帧 + provider flush（openai 闭合活动并产 Finish；anthropic 守卫防重复）+
    /// 拼装集合清理（TS runner-stream.ts:447 assembler.flush 只清集合）。
    // AttemptFailure ~160B（见 feed 上的说明）；低频错误路径，不 Box 化。
    #[allow(clippy::result_large_err)]
    pub fn finish_stream(
        &mut self,
        visible: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), AttemptFailure> {
        let frames = self
            .sse
            .finish()
            .map_err(|err| self.attach(sse_failure(err)))?;
        for frame in &frames {
            self.process_frame(frame, visible)?;
        }
        let parts = match self.kind {
            ProviderKind::Anthropic => self.anthropic.flush(),
            ProviderKind::OpenaiCompatible => self.openai.flush(),
        }
        .map_err(|err| self.attach(failure_from_adapter_error(err)))?;
        self.process_parts(parts, visible)?;
        self.assembler.flush();
        Ok(())
    }

    /// EOF 合成业务错误（对齐 runner-stream.ts:473-503：
    /// `detectProviderBusinessFinishError({source: {type:"finish", finishReason,
    /// rawFinishReason}})`）。合成记录无嵌套码 → 仅 `provider_success_false` 信号 +
    /// 默认文案；TS 对该错误直接 `TerminalStreamChunkError` 抛出（不经重试闸门），
    /// 故 `force_terminal = true`。
    ///
    /// 注：wire 扫描下 step② 的 raw 记录与 step③ 深扫信号源同源，feed 期通常已先
    /// 命中（force=false，但 classify 判不可重试 → 观测同为终态，executor 测试覆盖）；
    /// 本路径兜底“raw 已记录但该帧深扫未命中”的分叉形状。
    pub fn eof_business_failure(&self) -> Option<AttemptFailure> {
        if self.diagnostics.raw_finish_reason.as_deref() != Some(PROVIDER_SUCCESS_FALSE) {
            return None;
        }
        Some(AttemptFailure {
            kind: FailureKind::Business(ProviderBusinessFailure {
                status_code: None,
                provider_code: None,
                provider_message: None,
                provider_request_id: None,
                body_error_type: None,
            }),
            message: None,
            status: None,
            retry_after_ms: None,
            context_exceeded: false,
            http_status: Some(self.head_status),
            force_terminal: true,
            emitted_boundary: self.buffer.emitted(),
        })
    }

    // AttemptFailure ~160B（见 feed 上的说明）；低频错误路径，不 Box 化。
    #[allow(clippy::result_large_err)]
    fn process_frame(
        &mut self,
        frame: &JsonSseFrame,
        visible: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), AttemptFailure> {
        let JsonSseFrame { event, value } = frame;

        // ① fetch 层浅业务：每帧无条件（`data` 空与 `[DONE]` 已在 JsonSseStream 丢弃）。
        if let Some(failure) = business_failure_from_json_frame(event.as_deref(), value) {
            return Err(self.business_attempt_failure(failure, value));
        }

        // ② 诊断记录 + preserve 原始帧观察/投影（先于深扫，与 TS 1022→1029→1030 同序）。
        self.diagnostics.note_raw_finish_reason(value);
        if self.preserve && frame_observed(value) {
            match project_provider_boundary(value) {
                Some(boundary) => visible.extend(self.buffer.push(boundary, self.preserve)),
                // 无投影帧（message_stop / 非 anthropic 形状）：只提交观察到的边界，
                // 刷新暂存 synthetic start（TS applyStreamEventsToRetryBoundary 步骤①）。
                None => visible.extend(self.buffer.commit()),
            }
        }

        // ③ runner 层深业务：finish 形状门控 + 候选记录深扫。
        if frame_carries_business_signal(value, event.as_deref()) {
            if let Some(failure) = detect_business_failure_deep(value) {
                return Err(self.business_attempt_failure(failure, value));
            }
        }

        // ④ provider 解析。
        let parts = match self.kind {
            ProviderKind::Anthropic => self.anthropic.handle(value),
            ProviderKind::OpenaiCompatible => self.openai.on_chunk(value),
        }
        .map_err(|err| self.attach(failure_from_adapter_error(err)))?;

        // ⑤ 归一 → 拼装 → 缓冲/hold。
        self.process_parts(parts, visible)
    }

    // AttemptFailure ~160B（见 feed 上的说明）；低频错误路径，不 Box 化。
    #[allow(clippy::result_large_err)]
    fn process_parts(
        &mut self,
        parts: Vec<SdkPart>,
        visible: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), AttemptFailure> {
        for part in parts {
            // ⑤a error 拦截（TS:1042 error chunk → handleStreamErrorEvent；深扫已在③先行）。
            if let SdkPart::Error { message } = part {
                return Err(AttemptFailure {
                    kind: FailureKind::Unknown,
                    message: None,
                    status: None,
                    retry_after_ms: None,
                    context_exceeded: is_context_exceeded(None, Some(&message)),
                    http_status: None,
                    force_terminal: false,
                    emitted_boundary: self.buffer.emitted(),
                });
            }

            let direct_tool = matches!(&part, SdkPart::ToolCall { .. });
            let events = match self.assembler.apply(part) {
                Ok(events) => events,
                Err(err) => {
                    if self.preserve && direct_tool {
                        // TS runner-stream.ts:373-388 compactDirectToolCallCommit：
                        // 完整 direct tool-call 已是 provider 事件——提交前奏 + 投影
                        // inferred block stop 后抛出原始归一化错误。
                        visible.extend(self.buffer.commit());
                        visible.push(inferred_block_stop_event());
                    }
                    return Err(self.attach(failure_from_adapter_error(err)));
                }
            };

            for event in events {
                self.diagnostics.record(&event);
                // ⑤b 空完成 hold（TS:390-411）：finish 批次 + 零产出 + 预算内 → 整批丢弃，
                // 避免第一次 attempt 的 finish/start 泄漏到 UI；emitted 标记不回滚。
                let hold = matches!(event, ModelStreamEvent::Finish { .. })
                    && !self.preserve
                    && self.empty_retry_allowed
                    && self.diagnostics.zero_output();
                let pushed = self.buffer.push(event, self.preserve);
                if hold {
                    self.finish_held = true;
                } else {
                    visible.extend(pushed);
                }
            }
        }
        Ok(())
    }

    /// 帧级业务失败 → attempt 失败：
    /// - `retry_after` 取响应头（TS fetch 层把 responseHeaders 挂到帧级业务错误）；
    /// - `http_status` = 响应头状态（2xx 帧级业务失败的 responseStatus）；
    /// - `status`（分类输入）保持 None——TS business 分类的 effective_status 来自
    ///   providerCode 派生状态（`failure.status_code`），2xx 不参与 4xx/5xx 判定。
    pub(crate) fn business_attempt_failure(
        &self,
        failure: ProviderBusinessFailure,
        value: &Value,
    ) -> AttemptFailure {
        AttemptFailure {
            kind: FailureKind::Business(failure),
            message: None,
            status: None,
            retry_after_ms: self.head_retry_after_ms,
            context_exceeded: context_exceeded_from_value(value),
            http_status: Some(self.head_status),
            force_terminal: false,
            emitted_boundary: self.buffer.emitted(),
        }
    }

    /// 归一化/解析错误附带失败时刻的边界观察状态（attempt 级 emittedRetryBoundaryEvent）。
    fn attach(&self, mut failure: AttemptFailure) -> AttemptFailure {
        failure.emitted_boundary = self.buffer.emitted();
        failure
    }
}

// ============================================================
// helper
// ============================================================

/// `isRawProviderRetryBoundaryEvent`：非 `type:"ping"` 的对象帧/原始值都算已观察；
/// ping 帧（anthropic 心跳）不结束 compact SSE retry。
fn frame_observed(value: &Value) -> bool {
    !value
        .as_object()
        .is_some_and(|record| record.get("type").and_then(Value::as_str) == Some("ping"))
}

/// `toProviderStreamBoundaryEvent` 的逐字段镜像：
/// 只投影 response/block/stop 五类语义边界；其余类型（message_stop 等）只提交不投影。
/// 字段可空性与 TS 一致（无值不带字段；TS 侧序列化为 null，Option 消费方等价读取）。
fn project_provider_boundary(value: &Value) -> Option<ModelStreamEvent> {
    let record = value.as_object()?;
    let kind = record.get("type").and_then(Value::as_str)?;
    let boundary = match kind {
        "message_start" => StreamBoundary::ProviderResponseStart,
        "content_block_start" => StreamBoundary::ProviderContentBlockStart,
        "content_block_delta" => StreamBoundary::ProviderContentBlockDelta,
        "content_block_stop" => StreamBoundary::ProviderContentBlockStop,
        "message_delta" => StreamBoundary::ProviderStopReason,
        _ => return None,
    };
    // block_type 仅 content_block_start 携带（TS: content_block.type）；
    // delta_type 仅 content_block_delta 携带；index 仅 block 三类携带；
    // present 仅 message_delta 携带（Boolean(delta.stop_reason)：null/"" 为 false）。
    let block_type = (boundary == StreamBoundary::ProviderContentBlockStart)
        .then(|| {
            record
                .get("content_block")
                .and_then(Value::as_object)
                .and_then(|block| block.get("type"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .flatten();
    let delta_type = (boundary == StreamBoundary::ProviderContentBlockDelta)
        .then(|| {
            record
                .get("delta")
                .and_then(Value::as_object)
                .and_then(|delta| delta.get("type"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .flatten();
    let index = matches!(
        boundary,
        StreamBoundary::ProviderContentBlockStart
            | StreamBoundary::ProviderContentBlockDelta
            | StreamBoundary::ProviderContentBlockStop
    )
    .then(|| record.get("index").and_then(Value::as_i64))
    .flatten();
    let present = (boundary == StreamBoundary::ProviderStopReason).then(|| {
        record
            .get("delta")
            .and_then(Value::as_object)
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str)
            .is_some_and(|stop| !stop.is_empty())
    });
    Some(ModelStreamEvent::CompactStreamBoundary {
        boundary,
        block_type,
        index,
        delta_type,
        present,
    })
}

fn inferred_block_stop_event() -> ModelStreamEvent {
    ModelStreamEvent::CompactStreamBoundary {
        boundary: StreamBoundary::InferredContentBlockStop,
        block_type: None,
        index: None,
        delta_type: None,
        present: None,
    }
}

fn sse_failure(error: SseError) -> AttemptFailure {
    let message = error.to_string();
    AttemptFailure {
        kind: FailureKind::InvalidModelResponse {
            message: Some(message.clone()),
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

/// 归一化错误 → attempt 失败（对齐 TS AI SDK error part 的分类输入面）。
pub(crate) fn failure_from_adapter_error(error: AdapterError) -> AttemptFailure {
    let context_exceeded = is_context_exceeded(None, Some(&error.message));
    let kind = match error.code {
        ModelErrorCode::InvalidModelResponse => FailureKind::InvalidModelResponse {
            message: Some(error.message),
        },
        ModelErrorCode::InvalidModelRequest => FailureKind::InvalidModelRequest {
            message: Some(error.message),
        },
        // 其余 code 走 TS 兜底分支：分类文案固定（"Model request failed."），
        // 原始 message 仅用于超窗探针，不覆盖分类文案。
        _ => FailureKind::Unknown,
    };
    AttemptFailure {
        kind,
        message: None,
        status: None,
        retry_after_ms: None,
        context_exceeded,
        http_status: None,
        force_terminal: false,
        // 调用方（`FramePipeline::attach` / executor `attach_emitted`）按失败时刻覆写。
        emitted_boundary: false,
    }
}
