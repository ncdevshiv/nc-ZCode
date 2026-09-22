//! openai-compatible 流式变换（对齐 `@ai-sdk/openai-compatible@2.0.60` doStream）：
//! index 累积 tool 拼装、name 门控 `tool-input-start`、flush 闭合 `tool-input-end` + `tool-call`。

use crate::assemble::SdkPart;
use crate::{AdapterError, ModelErrorCode, ModelUsage};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const REASONING_ID: &str = "reasoning-0";
const TEXT_ID: &str = "txt-0";

#[derive(Debug)]
struct PendingToolCall {
    id: Option<String>,
    buffered_arguments: String,
}

#[derive(Debug)]
struct ActiveToolCall {
    id: String,
    /// 上游 start 事件后 name 不再更新；flush 产出最终 tool-call 时读取。
    name: String,
    arguments: String,
    /// 上游 hasFinished 语义：flush 时过滤已闭合调用（line169）与增量转发（line307）。
    has_finished: bool,
}

#[derive(Debug, Default)]
pub struct OpenAiStreamTransform {
    tool_calls: BTreeMap<usize, ActiveToolCall>,
    pending_tool_calls: BTreeMap<usize, PendingToolCall>,
    /// JS `toolCalls.length` 语义：无 index 的增量按此追加。
    next_index: usize,
    finish_reason: Option<String>,
    usage: Option<ModelUsage>,
    is_first_chunk: bool,
    is_active_reasoning: bool,
    is_active_text: bool,
}

impl OpenAiStreamTransform {
    pub fn new() -> Self {
        Self {
            is_first_chunk: true,
            ..Self::default()
        }
    }

    /// 处理一条已解析的 chunk JSON；输出顺序即事件顺序。
    pub fn on_chunk(&mut self, value: &Value) -> Result<Vec<SdkPart>, AdapterError> {
        let mut parts = Vec::new();
        if let Some(error) = value.get("error") {
            self.finish_reason = Some("error".to_string());
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string());
            parts.push(SdkPart::Error { message });
            return Ok(parts);
        }
        self.is_first_chunk = false; // response-metadata 无对应 ModelStreamEvent，直接越过
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = Some(convert_usage(usage));
        }
        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        if let Some(finish_reason) = choice
            .and_then(|choice| choice.get("finish_reason"))
            .filter(|value| !value.is_null())
            .and_then(Value::as_str)
        {
            self.finish_reason = Some(map_finish_reason(finish_reason).to_string());
        }
        let Some(delta) = choice
            .and_then(|choice| choice.get("delta"))
            .filter(|value| !value.is_null())
        else {
            return Ok(parts);
        };

        let reasoning = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .or_else(|| delta.get("reasoning").and_then(Value::as_str))
            .filter(|text| !text.is_empty());
        if let Some(reasoning) = reasoning {
            if !self.is_active_reasoning {
                parts.push(SdkPart::ReasoningStart {
                    id: REASONING_ID.to_string(),
                    provider_metadata: None,
                });
                self.is_active_reasoning = true;
            }
            parts.push(SdkPart::ReasoningDelta {
                id: Some(REASONING_ID.to_string()),
                delta: reasoning.to_string(),
                provider_metadata: None,
            });
        }

        if let Some(content) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            if self.is_active_reasoning {
                parts.push(SdkPart::ReasoningEnd {
                    id: REASONING_ID.to_string(),
                    provider_metadata: None,
                });
                self.is_active_reasoning = false;
            }
            if !self.is_active_text {
                parts.push(SdkPart::TextStart {
                    id: TEXT_ID.to_string(),
                });
                self.is_active_text = true;
            }
            parts.push(SdkPart::TextDelta {
                id: Some(TEXT_ID.to_string()),
                text: content.to_string(),
            });
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            if self.is_active_reasoning {
                parts.push(SdkPart::ReasoningEnd {
                    id: REASONING_ID.to_string(),
                    provider_metadata: None,
                });
                self.is_active_reasoning = false;
            }
            for item in tool_calls {
                self.handle_tool_call_delta(item, &mut parts)?;
            }
        }
        Ok(parts)
    }

    /// 流结束：闭合活动文本/推理与所有未完成 tool-call，最后发射 finish。
    pub fn flush(&mut self) -> Result<Vec<SdkPart>, AdapterError> {
        let mut parts = Vec::new();
        if self.is_active_reasoning {
            parts.push(SdkPart::ReasoningEnd {
                id: REASONING_ID.to_string(),
                provider_metadata: None,
            });
            self.is_active_reasoning = false;
        }
        if self.is_active_text {
            parts.push(SdkPart::TextEnd {
                id: TEXT_ID.to_string(),
            });
            self.is_active_text = false;
        }
        if let Some((index, pending)) = self.pending_tool_calls.iter().next() {
            // 有参数累积但 name 始终未到 → 终态失败（对齐上游 InvalidResponseDataError）。
            return Err(AdapterError::new(
                ModelErrorCode::InvalidModelResponse,
                "Expected 'function.name' to be a string.",
            )
            .with_context("index", json!(index))
            .with_context("arguments", json!(pending.buffered_arguments)));
        }
        for tool_call in self.tool_calls.values() {
            if tool_call.has_finished {
                continue;
            }
            parts.push(SdkPart::ToolInputEnd {
                id: tool_call.id.clone(),
            });
            // 上游把原始 arguments 字符串交给最终 tool-call，解析归一在 assembler。
            parts.push(SdkPart::ToolCall {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                input: Value::String(tool_call.arguments.clone()),
                provider_executed: None,
            });
        }
        parts.push(SdkPart::Finish {
            finish_reason: self
                .finish_reason
                .clone()
                .unwrap_or_else(|| "other".to_string()),
            usage: self.usage.clone().unwrap_or_default(),
            provider_metadata: None,
        });
        Ok(parts)
    }

    fn handle_tool_call_delta(
        &mut self,
        item: &Value,
        parts: &mut Vec<SdkPart>,
    ) -> Result<(), AdapterError> {
        let explicit_index = item
            .get("index")
            .filter(|value| !value.is_null())
            .and_then(Value::as_u64)
            .map(|index| index as usize);
        let index = explicit_index.unwrap_or(self.next_index);
        let function = item.get("function");
        let id = item
            .get("id")
            .filter(|value| !value.is_null())
            .and_then(Value::as_str);
        let name = function
            .and_then(|function| function.get("name"))
            .filter(|value| !value.is_null())
            .and_then(Value::as_str);
        let arguments = function
            .and_then(|function| function.get("arguments"))
            .filter(|value| !value.is_null())
            .and_then(Value::as_str);

        if !self.tool_calls.contains_key(&index) {
            if explicit_index.is_some() {
                // 有 index 但 name 未到：先进 pending 累积，name 到达才发 start。
                let pending =
                    self.pending_tool_calls
                        .entry(index)
                        .or_insert_with(|| PendingToolCall {
                            id: id.map(str::to_string),
                            buffered_arguments: String::new(),
                        });
                if pending.id.is_none() {
                    if let Some(id) = id {
                        pending.id = Some(id.to_string());
                    }
                }
                if let Some(arguments) = arguments {
                    pending.buffered_arguments.push_str(arguments);
                }
                let Some(name) = name else {
                    return Ok(()); // name 未到，继续等待
                };
                let pending = self
                    .pending_tool_calls
                    .remove(&index)
                    .expect("pending entry was inserted above");
                let Some(tool_call_id) = pending.id else {
                    return Err(AdapterError::new(
                        ModelErrorCode::InvalidModelResponse,
                        "Expected 'id' to be a string.",
                    )
                    .with_context("index", json!(index)));
                };
                parts.push(SdkPart::ToolInputStart {
                    id: tool_call_id.clone(),
                    tool_name: name.to_string(),
                    provider_executed: None,
                });
                self.tool_calls.insert(
                    index,
                    ActiveToolCall {
                        id: tool_call_id,
                        name: name.to_string(),
                        arguments: pending.buffered_arguments,
                        has_finished: false,
                    },
                );
            } else {
                // 无 index：start 必须同时携带 id 与 name。
                let Some(tool_call_id) = id else {
                    return Err(AdapterError::new(
                        ModelErrorCode::InvalidModelResponse,
                        "Expected 'id' to be a string.",
                    )
                    .with_context("index", json!(index)));
                };
                let Some(name) = name else {
                    return Err(AdapterError::new(
                        ModelErrorCode::InvalidModelResponse,
                        "Expected 'function.name' to be a string.",
                    )
                    .with_context("index", json!(index)));
                };
                parts.push(SdkPart::ToolInputStart {
                    id: tool_call_id.to_string(),
                    tool_name: name.to_string(),
                    provider_executed: None,
                });
                self.tool_calls.insert(
                    index,
                    ActiveToolCall {
                        id: tool_call_id.to_string(),
                        name: name.to_string(),
                        arguments: arguments.unwrap_or_default().to_string(),
                        has_finished: false,
                    },
                );
            }
            self.next_index = self.next_index.max(index + 1);
            // 统一尾巴：新激活的调用若已有累积参数，立刻补发一次 delta。
            let active = &self.tool_calls[&index];
            if !active.arguments.is_empty() {
                parts.push(SdkPart::ToolInputDelta {
                    id: active.id.clone(),
                    delta: active.arguments.clone(),
                });
            }
            return Ok(());
        }

        // 已激活调用：delta 无条件转发（空串也发，与上游一致）。
        let active = self.tool_calls.get_mut(&index).expect("checked above");
        if active.has_finished {
            return Ok(());
        }
        if let Some(arguments) = arguments {
            active.arguments.push_str(arguments);
        }
        parts.push(SdkPart::ToolInputDelta {
            id: active.id.clone(),
            delta: arguments.unwrap_or_default().to_string(),
        });
        Ok(())
    }
}

/// mapOpenAICompatibleFinishReason。
fn map_finish_reason(finish_reason: &str) -> &'static str {
    match finish_reason {
        "stop" => "stop",
        "length" => "length",
        "content_filter" => "content-filter",
        "function_call" | "tool_calls" => "tool-calls",
        _ => "other",
    }
}

/// openai token usage → ModelUsage（prompt/completion/details 字段直读）。
fn convert_usage(usage: &Value) -> ModelUsage {
    let read = |path: &str| usage.pointer(path).and_then(Value::as_u64);
    let server_tool_use = usage
        .get("server_tool_use")
        .filter(|value| value.is_object())
        .map(|value| crate::ServerToolUse {
            web_search_requests: value.get("web_search_requests").and_then(Value::as_u64),
            web_fetch_requests: value.get("web_fetch_requests").and_then(Value::as_u64),
        });
    ModelUsage {
        input_tokens: read("/prompt_tokens"),
        output_tokens: read("/completion_tokens"),
        total_tokens: read("/total_tokens"),
        cache_read_tokens: read("/prompt_tokens_details/cached_tokens"),
        cache_write_tokens: None,
        reasoning_tokens: read("/completion_tokens_details/reasoning_tokens"),
        server_tool_use,
    }
}
