//! 非流式完整响应 → 等价 SSE 事件序列合成。
//!
//! 为什么存在：部分网关/ Provider 只提供非流式 JSON（或 stream 请求被网关翻译成
//! OpenAI chunk 形状，与 anthropic 事件协议不符）。executor 的 2xx-非-SSE 分支
//! 原本把原始字节喂 SSE 管道——对完整 message JSON 那等价于空事件序列。本模块把
//! 完整响应翻译成**与真实流式协议逐事件同构**的 SSE 字节，再喂同一条管道：
//! 解析、拼装、usage、守卫逻辑全部复用，不引入第二条代码路径。
//!
//! 事件形状按真实 Provider 的流式协议构造（anthropic message_start/block_*/
//! message_delta/message_stop；openai chat.completion.chunk + [DONE]）。

use serde_json::{json, Value};

use crate::request::ProviderKind;

fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn sse_data_only(data: &Value) -> String {
    format!("data: {data}\n\n")
}

/// 尝试把 2xx 非 SSE body 合成为等价 SSE 字节。无法识别为完整响应时返回 None
/// （调用方保持既有行为：原始字节喂管道）。
pub fn synthesize_sse_from_complete_body(
    provider_kind: ProviderKind,
    body: &[u8],
) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let value: Value = serde_json::from_str(&text).ok()?;
    match provider_kind {
        ProviderKind::Anthropic => synthesize_anthropic(&value),
        ProviderKind::OpenaiCompatible => synthesize_openai(&value),
    }
}

/// anthropic 完整 message → anthropic 流式事件序列。
fn synthesize_anthropic(message: &Value) -> Option<String> {
    if message.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let mut out = String::new();
    let mut start_message = message.clone();
    if let Some(object) = start_message.as_object_mut() {
        // 真实 message_start 的 content 为空数组；块由 content_block_* 逐块给出。
        object.insert("content".to_string(), json!([]));
    }
    out.push_str(&sse(
        "message_start",
        &json!({"type": "message_start", "message": start_message}),
    ));

    let content = message.get("content").and_then(Value::as_array)?;
    for (index, block) in content.iter().enumerate() {
        // content_block_start 用流式形状：tool_use 的 input 必须是 {}（真实协议里
        // 起始块不带参数，参数由 input_json_delta 增量给出）；带完整 input 会走解析器
        // 的一次性补齐快路，之后我们再发 input_json_delta 就会报「缺少未决 tool_use」。
        let start_block = if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            let mut block = block.clone();
            if let Some(object) = block.as_object_mut() {
                object.insert("input".to_string(), json!({}));
            }
            block
        } else {
            block.clone()
        };
        out.push_str(&sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index, "content_block": start_block}),
        ));
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    out.push_str(&sse(
                        "content_block_delta",
                        &json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": text}}),
                    ));
                }
            }
            Some("tool_use") => {
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let partial = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                out.push_str(&sse(
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": partial}}),
                ));
            }
            _ => {}
        }
        out.push_str(&sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
    }

    out.push_str(&sse(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": message.get("stop_reason").cloned().unwrap_or(Value::Null)},
            "usage": message.get("usage").cloned().unwrap_or_else(|| json!({})),
        }),
    ));
    out.push_str(&sse("message_stop", &json!({"type": "message_stop"})));
    Some(out)
}

/// openai 完整 chat.completion → openai chunk 序列（含 [DONE]）。
fn synthesize_openai(completion: &Value) -> Option<String> {
    if completion.get("object").and_then(Value::as_str) != Some("chat.completion") {
        return None;
    }
    let id = completion.get("id").cloned().unwrap_or_else(|| json!(""));
    let model = completion
        .get("model")
        .cloned()
        .unwrap_or_else(|| json!(""));
    let created = completion
        .get("created")
        .cloned()
        .unwrap_or_else(|| json!(0));
    let choice = completion
        .get("choices")
        .and_then(Value::as_array)?
        .first()?
        .clone();
    let message = choice.get("message")?.clone();

    let mut out = String::new();
    out.push_str(&sse_data_only(&json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
        "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": Value::Null}]
    })));

    if let Some(content) = message.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            out.push_str(&sse_data_only(&json!({
                "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
                "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": Value::Null}]
            })));
        }
    }

    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in tool_calls.iter().enumerate() {
            let function = call.get("function").cloned().unwrap_or_else(|| json!({}));
            out.push_str(&sse_data_only(&json!({
                "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": index,
                    "id": call.get("id").cloned().unwrap_or_else(|| json!("")),
                    "type": "function",
                    "function": {
                        "name": function.get("name").cloned().unwrap_or_else(|| json!("")),
                        "arguments": function.get("arguments").cloned().unwrap_or_else(|| json!("{}")),
                    }
                }]}, "finish_reason": Value::Null}]
            })));
        }
    }

    out.push_str(&sse_data_only(&json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": choice.get("finish_reason").cloned().unwrap_or_else(|| json!("stop"))}]
    })));

    if let Some(usage) = completion.get("usage") {
        out.push_str(&sse_data_only(&json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [], "usage": usage
        })));
    }
    out.push_str("data: [DONE]\n\n");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_pipeline::FramePipeline;
    use crate::ModelStreamEvent;

    // 真实网关响应（2026-09-22 抓取）：anthropic 形状、tool_use 块、stop_reason=tool_use。
    const LIVE_ANTHROPIC_TOOL_USE: &str = r#"{"type":"message","role":"assistant","content":[{"type":"tool_use","id":"call_function_5qleawh42t9v_1","name":"Write","input":{"file_path":"probe.txt","content":"hi\n"}}],"model":"space-bunny-free","id":"07035f922210201866ed506ac4028b6e","stop_reason":"tool_use","usage":{"input_tokens":1257,"output_tokens":90}}"#;

    /// 与 executor 相同的路径：合成 SSE → FramePipeline → 事件序列。
    fn run_pipeline(provider_kind: ProviderKind, sse: &str) -> Vec<ModelStreamEvent> {
        let mut pipeline = FramePipeline::new(provider_kind, false, false, 200, None);
        let mut visible = Vec::new();
        pipeline.start(&mut visible);
        pipeline
            .feed(sse.as_bytes(), &mut visible)
            .expect("pipeline feed");
        pipeline.finish_stream(&mut visible).expect("finish_stream");
        visible
    }

    #[test]
    fn anthropic_tool_use_body_round_trips_to_tool_call_event() {
        let sse = synthesize_sse_from_complete_body(
            ProviderKind::Anthropic,
            LIVE_ANTHROPIC_TOOL_USE.as_bytes(),
        )
        .expect("must synthesize");
        assert!(sse.contains("event: message_start"));
        assert!(sse.contains("event: content_block_start"));
        assert!(sse.contains("input_json_delta"));
        assert!(sse.contains("event: message_delta"));
        assert!(sse.contains("event: message_stop"));

        let events = run_pipeline(ProviderKind::Anthropic, &sse);
        let tool_call = events
            .iter()
            .find_map(|event| match event {
                ModelStreamEvent::ToolCall { tool_call } => Some(tool_call.clone()),
                _ => None,
            })
            .expect("tool call event");
        assert_eq!(tool_call.name, "Write");
        assert_eq!(tool_call.input["file_path"], serde_json::json!("probe.txt"));
        // stop_reason 经 map_anthropic_stop_reason 归一化：tool_use → tool-calls。
        assert!(events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::Finish { finish_reason, .. } if finish_reason == "tool-calls"
        )));
    }

    #[test]
    fn anthropic_text_body_round_trips_to_text_and_usage() {
        let body = br#"{"type":"message","role":"assistant","content":[{"type":"text","text":"hello native"}],"model":"m","id":"msg_x","stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":3}}"#;
        let sse =
            synthesize_sse_from_complete_body(ProviderKind::Anthropic, body).expect("synthesize");
        let events = run_pipeline(ProviderKind::Anthropic, &sse);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                ModelStreamEvent::TextDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello native");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::Finish { usage, .. } if usage.input_tokens == Some(11) && usage.output_tokens == Some(3)
        )));
    }

    #[test]
    fn openai_tool_call_body_round_trips_through_openai_pipeline() {
        let body = br#"{"id":"cc_1","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"Write","arguments":"{\"file_path\":\"a.txt\"}"}}]}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#;
        let sse = synthesize_sse_from_complete_body(ProviderKind::OpenaiCompatible, body)
            .expect("synthesize");
        assert!(sse.contains("[DONE]"));
        let events = run_pipeline(ProviderKind::OpenaiCompatible, &sse);
        assert!(events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ToolCall { tool_call } if tool_call.name == "Write"
        )));
    }

    #[test]
    fn unrecognized_bodies_return_none() {
        assert!(synthesize_sse_from_complete_body(ProviderKind::Anthropic, b"{}").is_none());
        assert!(synthesize_sse_from_complete_body(ProviderKind::OpenaiCompatible, b"{}").is_none());
        assert!(synthesize_sse_from_complete_body(ProviderKind::Anthropic, b"not json").is_none());
    }
}
