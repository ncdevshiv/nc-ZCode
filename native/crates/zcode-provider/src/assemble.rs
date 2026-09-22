//! 流事件归一、tool-call 拼装与重试边界缓冲。
//!
//! 对齐 TS `runner-normalization.ts`（toModelStreamEvent）、`streaming-tool-call-assembler.ts`
//! 与 `stream-retry-boundary.ts`（前奏暂存 / 提交边界后释放 / 失败即丢弃）。

use crate::schema::{normalize_model_tool_input, normalize_model_tool_name, ToolNameContext};
use crate::{AdapterError, ModelStreamEvent, ModelToolCall};
use serde_json::Value;

/// AI SDK `TextStreamPart` 的最小等价物（只保留 toModelStreamEvent 消费的变体）。
#[derive(Debug, Clone, PartialEq)]
pub enum SdkPart {
    Start,
    TextStart {
        id: String,
    },
    TextDelta {
        id: Option<String>,
        text: String,
    },
    TextEnd {
        id: String,
    },
    ReasoningStart {
        id: String,
        provider_metadata: Option<Value>,
    },
    ReasoningDelta {
        id: Option<String>,
        delta: String,
        provider_metadata: Option<Value>,
    },
    ReasoningEnd {
        id: String,
        provider_metadata: Option<Value>,
    },
    ToolInputStart {
        id: String,
        tool_name: String,
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
        tool_call_id: String,
        tool_name: String,
        /// 执行参数；流式来源常为 JSON 字符串，assembler 统一归一。
        input: Value,
        provider_executed: Option<bool>,
    },
    Finish {
        finish_reason: String,
        usage: crate::ModelUsage,
        provider_metadata: Option<Value>,
    },
    Error {
        message: String,
    },
}

/// provider part → ModelStreamEvent；工具名在此与 assembler 双重点检（与 TS 分层一致）。
pub fn to_model_stream_event(part: SdkPart) -> Result<Option<ModelStreamEvent>, AdapterError> {
    let event = match part {
        SdkPart::Start => ModelStreamEvent::Start,
        SdkPart::TextStart { id } => ModelStreamEvent::TextStart { id },
        SdkPart::TextDelta { id, text } => ModelStreamEvent::TextDelta { id, text },
        SdkPart::TextEnd { id } => ModelStreamEvent::TextEnd { id },
        SdkPart::ReasoningStart {
            id,
            provider_metadata,
        } => ModelStreamEvent::ReasoningStart {
            id,
            provider_metadata,
        },
        SdkPart::ReasoningDelta {
            id,
            delta,
            provider_metadata,
        } => ModelStreamEvent::ReasoningDelta {
            id,
            text: delta,
            provider_metadata,
        },
        SdkPart::ReasoningEnd {
            id,
            provider_metadata,
        } => ModelStreamEvent::ReasoningEnd {
            id,
            provider_metadata,
        },
        SdkPart::ToolInputStart {
            id,
            tool_name,
            provider_executed,
        } => {
            let context = ToolNameContext {
                provider_executed,
                tool_call_id: Some(id.clone()),
            };
            let tool_name = normalize_model_tool_name(Some(&tool_name), &context)?;
            ModelStreamEvent::ToolInputStart {
                id,
                tool_name,
                provider_executed,
            }
        }
        SdkPart::ToolInputDelta { id, delta } => ModelStreamEvent::ToolInputDelta { id, delta },
        SdkPart::ToolInputEnd { id } => ModelStreamEvent::ToolInputEnd { id },
        SdkPart::ToolCall {
            tool_call_id,
            tool_name,
            input,
            provider_executed,
        } => ModelStreamEvent::ToolCall {
            tool_call: ModelToolCall {
                id: tool_call_id,
                // name/input 的最终校验归一由 assembler 统一执行。
                name: tool_name,
                input,
                provider_executed,
            },
        },
        SdkPart::Finish {
            finish_reason,
            usage,
            provider_metadata,
        } => ModelStreamEvent::Finish {
            finish_reason,
            provider_metadata,
            usage,
        },
        SdkPart::Error { message } => ModelStreamEvent::Error {
            error: Value::String(message),
        },
    };
    Ok(Some(event))
}

// ============================================================
// StreamingToolCallAssembler（对齐同名 TS 类）
// ============================================================

#[derive(Debug, Default)]
pub struct StreamingToolCallAssembler {
    completed_streaming_input_ids: std::collections::HashSet<String>,
    streaming_input_ids: std::collections::HashSet<String>,
    provider_executed_by_id: std::collections::HashMap<String, bool>,
    /// 保持插入序（完成顺序），供执行侧读取。
    normalized_tool_calls: Vec<ModelToolCall>,
}

impl StreamingToolCallAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 适配器统一入口：SdkPart → 归一事件。等价于 runner-stream 的
    /// `toModelStreamEvent(chunk)` + `toolCallAssembler.handle(event)` 两步（见
    /// handleStreamChunk），provider 变换层只产 SdkPart，拼装状态只此一处。
    pub fn apply(&mut self, part: SdkPart) -> Result<Vec<ModelStreamEvent>, AdapterError> {
        let Some(event) = to_model_stream_event(part)? else {
            return Ok(Vec::new());
        };
        self.handle(event)
    }

    pub fn snapshot_normalized_tool_calls(&self) -> Vec<ModelToolCall> {
        self.normalized_tool_calls.clone()
    }

    pub fn handle(
        &mut self,
        event: ModelStreamEvent,
    ) -> Result<Vec<ModelStreamEvent>, AdapterError> {
        match event {
            ModelStreamEvent::ToolInputStart {
                id,
                tool_name,
                provider_executed,
            } => self.handle_tool_input_start(id, tool_name, provider_executed),
            ModelStreamEvent::ToolInputDelta { id, delta } => {
                if self.has_normalized(&id) {
                    return Ok(vec![]);
                }
                Ok(vec![ModelStreamEvent::ToolInputDelta { id, delta }])
            }
            ModelStreamEvent::ToolInputEnd { id } => {
                if self.has_normalized(&id) {
                    return Ok(vec![]);
                }
                self.completed_streaming_input_ids.insert(id.clone());
                Ok(vec![ModelStreamEvent::ToolInputEnd { id }])
            }
            ModelStreamEvent::ToolCall { tool_call } => self.handle_tool_call(tool_call),
            ModelStreamEvent::Finish {
                finish_reason,
                provider_metadata,
                usage,
            } => {
                self.flush();
                Ok(vec![ModelStreamEvent::Finish {
                    finish_reason,
                    provider_metadata,
                    usage,
                }])
            }
            other => Ok(vec![other]),
        }
    }

    /// 生命周期收口：清流式追踪集，保留已归一化结果（对齐 TS flush）。
    pub fn flush(&mut self) {
        self.completed_streaming_input_ids.clear();
        self.provider_executed_by_id.clear();
        self.streaming_input_ids.clear();
    }

    fn has_normalized(&self, id: &str) -> bool {
        self.normalized_tool_calls
            .iter()
            .any(|tool_call| tool_call.id == id)
    }

    fn handle_tool_input_start(
        &mut self,
        id: String,
        tool_name: String,
        provider_executed: Option<bool>,
    ) -> Result<Vec<ModelStreamEvent>, AdapterError> {
        if self.has_normalized(&id) {
            return Ok(vec![]);
        }
        let context = ToolNameContext {
            provider_executed,
            tool_call_id: Some(id.clone()),
        };
        let tool_name = normalize_model_tool_name(Some(&tool_name), &context)?;
        if let Some(provider_executed) = provider_executed {
            self.provider_executed_by_id
                .insert(id.clone(), provider_executed);
        }
        self.streaming_input_ids.insert(id.clone());
        Ok(vec![ModelStreamEvent::ToolInputStart {
            id,
            tool_name,
            provider_executed,
        }])
    }

    fn handle_tool_call(
        &mut self,
        tool_call: ModelToolCall,
    ) -> Result<Vec<ModelStreamEvent>, AdapterError> {
        if self.has_normalized(&tool_call.id) {
            return Ok(vec![]);
        }
        if self.streaming_input_ids.contains(&tool_call.id)
            && !self.completed_streaming_input_ids.contains(&tool_call.id)
        {
            // 已进入流式 input 生命周期时，final call 不能替代缺失的 tool-input-end。
            return Ok(vec![]);
        }
        let provider_executed = tool_call
            .provider_executed
            .or_else(|| self.provider_executed_by_id.get(&tool_call.id).copied());
        let context = ToolNameContext {
            provider_executed,
            tool_call_id: Some(tool_call.id.clone()),
        };
        let tool_name = normalize_model_tool_name(Some(&tool_call.name), &context)?;
        let normalized = ModelToolCall {
            id: tool_call.id.clone(),
            name: tool_name,
            input: normalize_model_tool_input(&tool_call.input),
            // 流式 start 可能携带 final call 省略的 provider 执行标记，这里补回。
            provider_executed,
        };
        self.completed_streaming_input_ids.remove(&tool_call.id);
        self.provider_executed_by_id.remove(&tool_call.id);
        self.streaming_input_ids.remove(&tool_call.id);
        self.normalized_tool_calls.push(normalized.clone());
        Ok(vec![ModelStreamEvent::ToolCall {
            tool_call: normalized,
        }])
    }
}

// ============================================================
// 重试边界缓冲（对齐 applyStreamEventsToRetryBoundary + isRetrySafePrelude）
// ============================================================

#[derive(Debug, Default)]
pub struct RetryBoundaryBuffer {
    buffered: Vec<ModelStreamEvent>,
    emitted: bool,
}

impl RetryBoundaryBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已提交可见边界（含正文 token / tool-call / finish / error）。
    pub fn emitted(&self) -> bool {
        self.emitted
    }

    /// 前奏事件在首个提交边界前暂存；提交时先释放暂存再输出当前事件。
    /// `preserve_provider_stream_boundaries`（compact 请求）下只有本地合成的 `start` 算前奏。
    pub fn push(
        &mut self,
        event: ModelStreamEvent,
        preserve_provider_stream_boundaries: bool,
    ) -> Vec<ModelStreamEvent> {
        let retry_safe_prelude = is_retry_safe_prelude_event(&event)
            && (!preserve_provider_stream_boundaries || matches!(event, ModelStreamEvent::Start));
        if retry_safe_prelude && !self.emitted {
            self.buffered.push(event);
            return Vec::new();
        }
        let mut visible = Vec::new();
        if !self.emitted {
            visible.append(&mut self.buffered);
        }
        visible.push(event);
        self.emitted = true;
        visible
    }

    /// 尝试成功收尾：按原序释放残余前奏（签名类空 delta 随前奏保留原位）。
    pub fn take_committed(&mut self) -> Vec<ModelStreamEvent> {
        self.buffered.drain(..).collect()
    }

    /// providerEventObserved 分支：事件已算"观察到"，强制释放前奏并标记已提交。
    /// 对齐 TS `emittedRetryBoundaryEvent = true` + 持续 drain pending 的路径——
    /// 该帧自身的事件不在这里输出（调用方随后按正常 push 输出当前事件）。
    pub fn commit(&mut self) -> Vec<ModelStreamEvent> {
        let mut visible = Vec::new();
        if !self.emitted {
            visible.append(&mut self.buffered);
        }
        self.emitted = true;
        visible
    }

    /// 尝试失败且未提交边界：丢弃前奏，消费者不会看到半截输入。
    pub fn discard(&mut self) {
        self.buffered.clear();
    }
}

/// isRetrySafePreludeStreamEvent：空 text/reasoning delta 视为前奏（签名转空 delta 的场景）。
pub fn is_retry_safe_prelude_event(event: &ModelStreamEvent) -> bool {
    match event {
        ModelStreamEvent::Start
        | ModelStreamEvent::TextStart { .. }
        | ModelStreamEvent::TextEnd { .. }
        | ModelStreamEvent::ReasoningStart { .. }
        | ModelStreamEvent::ReasoningEnd { .. }
        | ModelStreamEvent::ToolInputStart { .. }
        | ModelStreamEvent::ToolInputDelta { .. }
        | ModelStreamEvent::ToolInputEnd { .. } => true,
        ModelStreamEvent::TextDelta { text, .. }
        | ModelStreamEvent::ReasoningDelta { text, .. } => text.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_delta(text: &str) -> ModelStreamEvent {
        ModelStreamEvent::TextDelta {
            id: Some("txt-0".to_string()),
            text: text.to_string(),
        }
    }

    #[test]
    fn prelude_holds_until_first_committed_boundary() {
        let mut buffer = RetryBoundaryBuffer::new();
        assert!(buffer.push(ModelStreamEvent::Start, false).is_empty());
        assert!(!buffer.emitted(), "前奏未提交前不算已观察输出");

        let visible = buffer.push(text_delta("x"), false);
        assert_eq!(visible.len(), 2, "提交边界时先释放前奏再输出当前事件");
        assert!(matches!(visible[0], ModelStreamEvent::Start));
        assert!(matches!(&visible[1], ModelStreamEvent::TextDelta { text, .. } if text == "x"));
        assert!(buffer.emitted());
    }

    #[test]
    fn preserve_mode_only_local_start_counts_as_prelude() {
        let mut buffer = RetryBoundaryBuffer::new();
        // compact 请求：provider TextStart 不是可暂存前奏，立即可见。
        let visible = buffer.push(ModelStreamEvent::TextStart { id: "txt-0".into() }, true);
        assert_eq!(visible.len(), 1);
        assert!(matches!(visible[0], ModelStreamEvent::TextStart { .. }));
        assert!(buffer.emitted());
    }

    #[test]
    fn commit_releases_buffered_prelude_exactly_once() {
        let mut buffer = RetryBoundaryBuffer::new();
        assert!(buffer.push(ModelStreamEvent::Start, true).is_empty());

        let first = buffer.commit();
        assert_eq!(first.len(), 1, "观察到边界 → 立即释放暂存前奏");
        assert!(matches!(first[0], ModelStreamEvent::Start));
        assert!(buffer.commit().is_empty(), "已提交后重复 commit 不再产出");
        assert!(buffer.emitted());
    }

    #[test]
    fn discard_drops_prelude_on_failure_path() {
        let mut buffer = RetryBoundaryBuffer::new();
        assert!(buffer.push(ModelStreamEvent::Start, false).is_empty());
        buffer.discard();
        assert!(
            buffer.take_committed().is_empty(),
            "失败路径消费者不得看到半截前奏"
        );
        assert!(!buffer.emitted(), "丢弃不改变提交状态判定");
    }

    #[test]
    fn empty_signature_delta_stays_with_prelude_in_order() {
        // SDK 把纯签名转成空 reasoning/text delta：随前奏暂存，成功时保留原序释放。
        let mut buffer = RetryBoundaryBuffer::new();
        assert!(buffer.push(ModelStreamEvent::Start, false).is_empty());
        assert!(buffer.push(text_delta(""), false).is_empty());
        let visible = buffer.push(text_delta("x"), false);
        assert_eq!(visible.len(), 3, "空 delta 必须留在原位");
        assert!(matches!(&visible[1], ModelStreamEvent::TextDelta { text, .. } if text.is_empty()));
        assert!(matches!(&visible[2], ModelStreamEvent::TextDelta { text, .. } if text == "x"));
    }

    #[test]
    fn take_committed_releases_in_original_order_on_success() {
        let mut buffer = RetryBoundaryBuffer::new();
        assert!(buffer.push(ModelStreamEvent::Start, false).is_empty());
        assert!(buffer
            .push(ModelStreamEvent::TextStart { id: "t".into() }, false)
            .is_empty());
        let released = buffer.take_committed();
        assert_eq!(released.len(), 2, "自然完成按原序释放残余前奏");
        assert!(matches!(released[0], ModelStreamEvent::Start));
        assert!(matches!(released[1], ModelStreamEvent::TextStart { .. }));
    }
}
