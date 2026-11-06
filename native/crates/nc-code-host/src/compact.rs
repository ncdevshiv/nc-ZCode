//! Compaction：长会话摘要压缩（对齐 TS `core/src/compact/*`）。
//!
//! 两层，与 TS 同：
//!   1. **机械 microcompact**（turn_context）：阈值内直接裁剪旧工具结果，无需模型；
//!   2. **LLM 摘要压缩**（本模块）：历史超过阈值时，用**同一个模型**生成结构化
//!      摘要，替换旧历史。摘要提示词逐字移植 TS `core/src/compact/prompt.ts`
//!      （含「安全约束必须逐字保留」这条硬要求——压缩不能让安全约束失效）。
//!
//! 纪律：压缩**不静默**。摘要失败（模型错/取消/超限）→ 返回 typed error 让 turn
//! 按原路径继续（宁可带着长历史，也不要悄悄丢上下文）。

use std::sync::Arc;

use nc_code_provider::executor::{self, StreamRequest};
use nc_code_provider::http_transport::HttpTransport;
use nc_code_provider::request::{
    build_anthropic_messages_body, build_openai_compatible_body, Message, MessageRole,
    RequestBodyOptions,
};
use nc_code_provider::transport::{RealClock, TransportRequest};
use nc_code_provider::{AdapterError, ModelErrorCode, ModelStreamEvent};
use serde_json::json;

use crate::provider_config::ProviderConfig;
use crate::turn_context::ContextFacts;

const NO_TOOLS_PREAMBLE: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n\
- Do NOT use Read, Bash, Grep, Glob, Edit, Write, or ANY other tool.\n\
- You already have all the context you need in the conversation above.\n\
- Tool calls will be REJECTED and will waste your only turn — you will fail the task.\n\
- Your entire response must be plain text: an <analysis> block followed by a <summary> block.\n\n";

const NO_TOOLS_TRAILER: &str = "\n\nREMINDER: Do NOT call any tools. Respond with plain text only — \
an <analysis> block followed by a <summary> block. Tool calls will be rejected and you will fail the task.";

/// TS `BASE_COMPACT_PROMPT` 的移植（结构化九节 + 安全约束逐字保留的硬要求）。
const BASE_COMPACT_PROMPT: &str = "Your task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests and your previous actions.\nThis summary should be thorough in capturing technical details, code patterns, and architectural decisions that would be essential for continuing development work without losing context.\n\nBefore providing your final summary, wrap your analysis in <analysis> tags to organize your thoughts and ensure you've covered all necessary points. In your analysis process:\n\n1. Chronologically analyze each message and section of the conversation. For each section thoroughly identify:\n   - The user's explicit requests and intents\n   - Your approach to addressing the user's requests\n   - Key decisions, technical concepts and code patterns\n   - Specific details like:\n     - file names\n     - full code snippets\n     - function signatures\n     - file edits\n   - Errors that you ran into and how you fixed them\n   - Pay special attention to specific user feedback that you received, especially if the user told you to do something differently.\n   - Note any security-relevant instructions or constraints the user stated (e.g., sensitive files or data to avoid, operations that must not be performed, credential or secret handling rules). These MUST be preserved verbatim in the summary so they continue to apply after compaction.\n2. Double-check for technical accuracy and completeness, addressing each required element thoroughly.\n\nYour summary should include the following sections:\n\n1. Primary Request and Intent: Capture all of the user's explicit requests and intents in detail\n2. Key Technical Concepts: List all important technical concepts, technologies, and frameworks discussed.\n3. Files and Code Sections: Enumerate specific files and code sections examined, modified, or created. Pay special attention to the most recent messages and include full code snippets where applicable and include a summary of why this file read or edit is important.\n4. Errors and fixes: List all errors that you ran into, and how you fixed them. Pay special attention to specific user feedback that you received, especially if the user told you to do something differently.\n5. Problem Solving: Document problems solved and any ongoing troubleshooting efforts.\n6. All user messages: List ALL user messages that are not tool results. These are critical for understanding the users' feedback and changing intent. Preserve any security-relevant instructions or constraints verbatim so they remain in effect after compaction.\n7. Pending Tasks: Outline any pending tasks that you have explicitly been asked to work on.\n8. Current Work: Describe in detail precisely what was being worked on immediately before this summary request, paying special attention to the most recent messages from both user and assistant. Include file names and code snippets where applicable.\n9. Optional Next Step: List the next step that you will take that is related to the most recent work you were doing. IMPORTANT: ensure that this step is DIRECTLY in line with the user's most recent explicit requests, and the task you were working on immediately before this summary request. If your last task was concluded, then only list next steps if they are explicitly in line with the users request. Do not start on tangential requests or really old requests that were already completed without confirming with the user first.\n                       If there is a next step, include direct quotes from the most recent conversation showing exactly what task you were working on and where you left off. This should be verbatim to ensure there's no drift in task interpretation.";

/// 摘要提示词（系统块用 preamble + base；user 块用 trailer 收口）。
pub fn compact_prompt() -> String {
    format!("{NO_TOOLS_PREAMBLE}{BASE_COMPACT_PROMPT}{NO_TOOLS_TRAILER}")
}

/// 压缩后保留的尾部消息条数（最近上下文不动，只被摘要覆盖更早的部分）。
const KEEP_TAIL_MESSAGES: usize = 4;

/// 对历史做 LLM 摘要压缩。返回压缩后的历史。
///
/// 语义：旧消息（`..len-KEEP_TAIL`）被一条 user 摘要消息取代；尾部 KEEP_TAIL 条
/// 原样保留。摘要请求**不带工具**（tools: []）并用强约束提示词——工具调用会被
/// 拒绝、白白消耗唯一一轮（TS 同纪律）。
pub async fn compact_history(
    config: &ProviderConfig,
    context: &ContextFacts,
    history: &[Message],
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<Vec<Message>, AdapterError> {
    if history.len() <= KEEP_TAIL_MESSAGES {
        return Ok(history.to_vec());
    }
    let split = history.len() - KEEP_TAIL_MESSAGES;
    let older = &history[..split];
    let summary = generate_summary(config, context, older, is_cancelled).await?;

    let mut compacted = vec![Message::text(
        MessageRole::User,
        format!(
            "[conversation summary — the earlier {} messages were compacted]\n\n{summary}",
            older.len()
        ),
    )];
    compacted.extend_from_slice(&history[split..]);
    Ok(compacted)
}

async fn generate_summary(
    config: &ProviderConfig,
    context: &ContextFacts,
    older: &[Message],
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<String, AdapterError> {
    let provider_kind = config.provider_kind().map_err(|error| {
        AdapterError::new(ModelErrorCode::ProviderNotConfigured, error.to_string())
    })?;
    let mut messages = vec![Message::text(MessageRole::User, compact_prompt())];
    messages.extend_from_slice(older);

    // 摘要请求不带工具：模型只产出文本。
    let options = RequestBodyOptions {
        model: &config.model,
        system: &context.system_blocks,
        messages: &messages,
        tools: &[],
        max_tokens: config.max_tokens.max(4096),
        stream: config.stream,
    };
    let body = match provider_kind {
        nc_code_provider::request::ProviderKind::Anthropic => {
            build_anthropic_messages_body(&options)
        }
        nc_code_provider::request::ProviderKind::OpenaiCompatible => {
            build_openai_compatible_body(&options)
        }
    }?;
    let body_json = serde_json::to_string(&body).map_err(|error| {
        AdapterError::new(ModelErrorCode::InvalidModelRequest, error.to_string())
    })?;

    let transport_request = TransportRequest {
        url: config.endpoint_url(),
        headers: match provider_kind {
            nc_code_provider::request::ProviderKind::Anthropic => vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-api-key".to_string(), config.api_key.clone()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ],
            nc_code_provider::request::ProviderKind::OpenaiCompatible => vec![
                ("content-type".to_string(), "application/json".to_string()),
                (
                    "authorization".to_string(),
                    format!("Bearer {}", config.api_key),
                ),
            ],
        },
        body: body_json,
    };
    let stream_request = StreamRequest {
        transport_request,
        provider_kind,
        retry: nc_code_provider::retry::RetryOptions::default(),
        retry_budget: None,
        off_peak: false,
        preserve_provider_stream_boundaries: false,
        stream_idle_timeout_base_ms: None,
        stream_idle_timeout_retry_number: None,
        is_cancelled,
    };
    let mut transport = HttpTransport::new();
    let execution = executor::execute(&mut transport, &RealClock, &stream_request).await;
    if let Some(failure) = execution.failure {
        return Err(failure);
    }
    let mut summary = String::new();
    for event in &execution.events {
        match event {
            ModelStreamEvent::TextDelta { text, .. } => summary.push_str(text),
            ModelStreamEvent::Error { error } => {
                return Err(AdapterError::new(
                    ModelErrorCode::InvalidModelResponse,
                    error.to_string(),
                ));
            }
            _ => {}
        }
    }
    if summary.trim().is_empty() {
        return Err(AdapterError::new(
            ModelErrorCode::InvalidModelResponse,
            "compaction produced an empty summary",
        ));
    }
    Ok(summary)
}

/// 压缩事件投影（供 conversation 行展示）。
pub fn compact_summary_event(summary: &str, message_count: usize) -> serde_json::Value {
    json!({
        "type": "compact_summary",
        "messageCount": message_count,
        "summary": summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_provider::request::ContentPart;

    fn text_of(message: &Message) -> String {
        message
            .parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn prompt_carries_the_security_preservation_clause() {
        let prompt = compact_prompt();
        assert!(prompt.contains("TEXT ONLY"));
        assert!(
            prompt.contains("must NOT call any tools") || prompt.contains("Do NOT call any tools")
        );
        // 两条安全硬要求都在（TS 的硬约束，压缩不得让安全约束失效）。
        assert!(prompt.contains("MUST be preserved verbatim"));
        assert!(prompt.contains("remain in effect after compaction"));
    }

    #[test]
    fn prompt_lists_all_nine_sections() {
        let prompt = compact_prompt();
        for section in [
            "Primary Request and Intent",
            "Key Technical Concepts",
            "Files and Code Sections",
            "Errors and fixes",
            "Problem Solving",
            "All user messages",
            "Pending Tasks",
            "Current Work",
            "Optional Next Step",
        ] {
            assert!(
                prompt.contains(section),
                "missing compact section: {section}"
            );
        }
    }

    #[tokio::test]
    async fn short_history_is_returned_unchanged_without_a_model_call() {
        let config = ProviderConfig {
            provider_id: None,
            kind: "openai-compatible".to_string(),
            base_url: "http://127.0.0.1:1/v1".to_string(),
            api_key: "k".to_string(),
            model: "m".to_string(),
            max_tokens: 1024,
            auth_style: None,
            stream: true,
            extra_models: Vec::new(),
            reasoning_levels: Vec::new(),
            headers: Vec::new(),
        };
        let context = crate::turn_context::assemble_context(std::path::Path::new("."));
        let history = vec![Message::text(MessageRole::User, "short")];
        let cancelled: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| false);
        // provider 指向不可达端口也不报错：短历史根本不发请求。
        let result = compact_history(&config, &context, &history, Arc::clone(&cancelled))
            .await
            .expect("short history must pass through");
        assert_eq!(result.len(), 1);
        assert_eq!(text_of(&result[0]), "short");
    }
}
