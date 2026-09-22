//! FramePipeline 单元测试：compact 边界投影、前奏缓冲交互、双层业务拦截、
//! 空补全 hold 与 EOF 合成业务错误。

use zcode_provider::classify::FailureKind;
use zcode_provider::frame_pipeline::FramePipeline;
use zcode_provider::request::ProviderKind;
use zcode_provider::{ModelStreamEvent, StreamBoundary};

const MSG_START: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":5}}}\n\n";
const BLOCK_START_TEXT: &str = "event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
const BLOCK_DELTA_TEXT: &str = "event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n";
const PING: &str = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
const MSG_DELTA_END_TURN: &str = "event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n";
const CONTENT_HI: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n";
const ERROR_EVENT: &str = "data: {\"error\":{\"message\":\"boom\"}}\n\n";
const FINISH_ZERO: &str = "data: {\"choices\":[{\"delta\":{}}],\"usage\":{}}\n\n";
const PSF_FRAME: &str = "data: {\"choices\":[{\"finish_reason\":\"provider_success_false\"}]}\n\n";
const DEEP_PSF_FRAME: &str = "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\
\"rawFinishReason\":\"provider_success_false\"}\n\n";

fn pipeline(kind: ProviderKind, preserve: bool, empty_retry_allowed: bool) -> FramePipeline {
    FramePipeline::new(kind, preserve, empty_retry_allowed, 200, None)
}

fn boundary_index(events: &[ModelStreamEvent], boundary: StreamBoundary) -> Option<usize> {
    events.iter().position(|event| {
        matches!(
            event,
            ModelStreamEvent::CompactStreamBoundary { boundary: found, .. } if *found == boundary
        )
    })
}

fn find_boundary(
    events: &[ModelStreamEvent],
    boundary: StreamBoundary,
) -> Option<&ModelStreamEvent> {
    events.iter().find(|event| {
        matches!(
            event,
            ModelStreamEvent::CompactStreamBoundary { boundary: found, .. } if *found == boundary
        )
    })
}

#[test]
fn preserve_projects_anthropic_boundaries_and_releases_prelude() {
    let mut pipeline = pipeline(ProviderKind::Anthropic, true, false);
    let mut visible = Vec::new();
    pipeline.start(&mut visible);
    assert!(visible.is_empty(), "start 在首个边界前保持暂存");

    pipeline
        .feed(MSG_START.as_bytes(), &mut visible)
        .expect("message_start");
    let event = find_boundary(&visible, StreamBoundary::ProviderResponseStart)
        .expect("message_start → ProviderResponseStart 投影");
    if let ModelStreamEvent::CompactStreamBoundary {
        block_type,
        index,
        delta_type,
        present,
        ..
    } = event
    {
        assert_eq!(*block_type, None, "start 投影不携带 block_type");
        assert_eq!(*index, None, "start 投影不携带 index");
        assert_eq!(*delta_type, None);
        assert_eq!(*present, None);
    }
    assert!(
        matches!(visible.first(), Some(ModelStreamEvent::Start)),
        "投影事件必须先释放暂存的 start（TS: 先 drain 再 push）"
    );

    pipeline
        .feed(BLOCK_START_TEXT.as_bytes(), &mut visible)
        .expect("content_block_start");
    let event = find_boundary(&visible, StreamBoundary::ProviderContentBlockStart)
        .expect("content_block_start 投影");
    if let ModelStreamEvent::CompactStreamBoundary {
        block_type,
        index,
        delta_type,
        present,
        ..
    } = event
    {
        assert_eq!(
            block_type.as_deref(),
            Some("text"),
            "block_type 取 content_block.type"
        );
        assert_eq!(*index, Some(0), "index 取 wire 事件 index");
        assert_eq!(*delta_type, None);
        assert_eq!(*present, None);
    }

    pipeline
        .feed(BLOCK_DELTA_TEXT.as_bytes(), &mut visible)
        .expect("content_block_delta");
    let event = find_boundary(&visible, StreamBoundary::ProviderContentBlockDelta)
        .expect("content_block_delta 投影");
    if let ModelStreamEvent::CompactStreamBoundary {
        block_type,
        index,
        delta_type,
        present,
        ..
    } = event
    {
        assert_eq!(
            delta_type.as_deref(),
            Some("text_delta"),
            "delta_type 取 delta.type"
        );
        assert_eq!(*index, Some(0));
        assert_eq!(
            *block_type, None,
            "delta 投影不携带 block_type（TS 无此字段）"
        );
        assert_eq!(*present, None);
    }

    let start = boundary_index(&visible, StreamBoundary::ProviderResponseStart).expect("start");
    let block_start =
        boundary_index(&visible, StreamBoundary::ProviderContentBlockStart).expect("block start");
    let block_delta =
        boundary_index(&visible, StreamBoundary::ProviderContentBlockDelta).expect("block delta");
    assert!(
        start < block_start && block_start < block_delta,
        "投影顺序须与 wire 顺序一致"
    );

    assert!(
        visible.iter().any(|event| matches!(
            event,
            ModelStreamEvent::TextDelta { text, .. } if text == "Hi"
        )),
        "正文事件不得被投影吞掉"
    );
    assert!(pipeline.emitted_boundary(), "首个真实帧后边界视为已提交");
}

#[test]
fn ping_does_not_commit_boundary() {
    let mut pipeline = pipeline(ProviderKind::Anthropic, true, false);
    let mut visible = Vec::new();
    pipeline.start(&mut visible);

    pipeline.feed(PING.as_bytes(), &mut visible).expect("ping");
    assert!(
        visible.is_empty(),
        "ping 不投影也不提交边界（TS isRawProviderRetryBoundaryEvent 排除 ping）"
    );
    assert!(!pipeline.emitted_boundary(), "ping 不得把 start 释放出去");

    pipeline
        .feed(MSG_START.as_bytes(), &mut visible)
        .expect("message_start");
    assert_eq!(
        visible.len(),
        2,
        "真实边界才释放 [Start, ProviderResponseStart]"
    );
    assert!(pipeline.emitted_boundary());
}

#[test]
fn message_delta_projects_stop_reason_present_flag() {
    let mut pipeline = pipeline(ProviderKind::Anthropic, true, false);
    let mut visible = Vec::new();
    pipeline.start(&mut visible);
    pipeline
        .feed(MSG_START.as_bytes(), &mut visible)
        .expect("message_start");
    pipeline
        .feed(MSG_DELTA_END_TURN.as_bytes(), &mut visible)
        .expect("message_delta");

    let event = find_boundary(&visible, StreamBoundary::ProviderStopReason)
        .expect("message_delta → ProviderStopReason");
    if let ModelStreamEvent::CompactStreamBoundary { present, .. } = event {
        assert_eq!(*present, Some(true), "stop_reason 非空 → present=true");
    } else {
        panic!("应为 CompactStreamBoundary");
    }
}

#[test]
fn message_delta_without_stop_reason_projects_present_false() {
    let mut pipeline = pipeline(ProviderKind::Anthropic, true, false);
    let mut visible = Vec::new();
    pipeline.start(&mut visible);
    pipeline
        .feed(MSG_START.as_bytes(), &mut visible)
        .expect("message_start");
    let frame =
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{}}\n\n";
    pipeline
        .feed(frame.as_bytes(), &mut visible)
        .expect("message_delta");

    let event = find_boundary(&visible, StreamBoundary::ProviderStopReason)
        .expect("ProviderStopReason 投影");
    if let ModelStreamEvent::CompactStreamBoundary { present, .. } = event {
        assert_eq!(
            *present,
            Some(false),
            "stop_reason 缺失/null/空串 → present=false"
        );
    } else {
        panic!("应为 CompactStreamBoundary");
    }
}

#[test]
fn error_event_fails_attempt_and_keeps_prior_events() {
    let mut pipeline = pipeline(ProviderKind::OpenaiCompatible, false, false);
    let mut visible = Vec::new();
    pipeline.start(&mut visible);
    pipeline
        .feed(CONTENT_HI.as_bytes(), &mut visible)
        .expect("content");

    let failure = pipeline
        .feed(ERROR_EVENT.as_bytes(), &mut visible)
        .expect_err("error 帧必须转成 attempt 失败");
    assert!(
        matches!(failure.kind, FailureKind::Unknown),
        "无业务码的 error 帧 → Unknown 分类"
    );
    assert!(!failure.context_exceeded, "普通错误文案不得误判为超窗");
    assert!(
        failure.emitted_boundary,
        "失败时刻的边界观察状态必须随失败上抛"
    );
    assert!(
        visible.iter().any(|event| matches!(
            event,
            ModelStreamEvent::TextDelta { text, .. } if text == "Hi"
        )),
        "失败前已发出的事件保留在 visible（yield-then-throw）"
    );
}

#[test]
fn zero_output_finish_holds_batch_when_empty_retry_allowed() {
    let mut pipeline = pipeline(ProviderKind::OpenaiCompatible, false, true);
    let mut visible = Vec::new();
    pipeline.start(&mut visible);
    pipeline
        .feed(FINISH_ZERO.as_bytes(), &mut visible)
        .expect("zero finish frame");
    pipeline.finish_stream(&mut visible).expect("flush");

    assert!(
        pipeline.diagnostics().zero_output(),
        "finish/other + 零用量 + 零正文 → 空补全"
    );
    assert!(pipeline.finish_held(), "预算允许且零产出 → hold 本批事件");
    assert!(
        visible.is_empty(),
        "hold 后消费者看不到任何事件（本地 start 随批丢弃）"
    );
    assert!(pipeline.take_released().is_empty(), "hold 已消费前奏缓冲");
    assert!(
        pipeline.eof_business_failure().is_none(),
        "无 rawFinishReason 不触发合成错误"
    );
}

#[test]
fn psf_finish_frame_fails_at_feed_time_with_raw_recorded() {
    // 同一帧的 note_raw（step②）与深扫 psf 信号源完全同源，feed 期必先命中：
    // 分类为不可重试业务错误 → 终态（与 TS EOF force 终态观测一致）。
    // raw 记录已落诊断，防御性的 eof_business_failure 只在“记录了 psf 但 feed 未拦截”
    // 的分叉形状下补刀（见 README 已知差异）。
    let mut pipeline = pipeline(ProviderKind::OpenaiCompatible, false, false);
    let mut visible = Vec::new();
    pipeline.start(&mut visible);
    let failure = pipeline
        .feed(PSF_FRAME.as_bytes(), &mut visible)
        .expect_err("psf finish 帧必须在 feed 期转成业务失败");
    assert!(matches!(failure.kind, FailureKind::Business(_)));
    assert!(
        !failure.force_terminal,
        "feed 期失败走分类/闸门路径（只有 EOF 合成才是 force）"
    );
    assert!(
        failure.message.is_none(),
        "无嵌套码 → 走 classify 的默认业务文案"
    );
    assert_eq!(failure.http_status, Some(200), "帧级业务挂 2xx 响应状态");
    assert_eq!(
        pipeline.diagnostics().raw_finish_reason.as_deref(),
        Some("provider_success_false"),
        "raw finish 原因仍须记录（TS diagnostics 面）"
    );
}

#[test]
fn deep_scan_detects_psf_on_finish_shaped_frame() {
    // 深扫门 = finish/error 形状（frame_carries_business_signal），与 preserve/预算无关；
    // 浅扫只认顶层 code/success —— rawFinishReason 由深扫捕获。
    let mut pipeline = pipeline(ProviderKind::OpenaiCompatible, false, false);
    let mut visible = Vec::new();
    pipeline.start(&mut visible);
    let failure = pipeline
        .feed(DEEP_PSF_FRAME.as_bytes(), &mut visible)
        .expect_err("深扫必须在 feed 期拦截 psf finish 帧");
    assert!(matches!(failure.kind, FailureKind::Business(_)));
    let FailureKind::Business(business) = &failure.kind else {
        panic!("上行已断言 Business");
    };
    assert_eq!(business.provider_code, None, "合成 psf 无嵌套码");
    assert!(
        !failure.force_terminal,
        "feed 期业务失败仍走重试闸门（只有 EOF 合成才是 force）"
    );
}
