//! `generateCommitMessage` — prompt assembly, output validation and the real provider call.
//!
//! Port of `packages/services/src/git/gitCommitMessageGenerator.ts`. TS wires this to the
//! live model-selection view plus the agent's one-shot text generation
//! (`packages/services/src/node.ts:2299-2322`); the Rust side resolves the very same
//! `preferredSelection` from the personal provider store and issues a real completion
//! through `nc-code-provider`'s executor. Nothing here fabricates a message: when no model is
//! configured, or the model answers with something that is not a Conventional Commit
//! subject, the channel fails with the TS error text.

use std::sync::Arc;

use async_trait::async_trait;
use nc_code_provider::executor::{self, StreamRequest};
use nc_code_provider::http_transport::HttpTransport;
use nc_code_provider::request::{self, Message, MessageRole, RequestBodyOptions};
use nc_code_provider::transport::{RealClock, TransportRequest};
use nc_code_provider::ModelStreamEvent;
use nc_code_service_core::ServiceError;
use nc_code_services_provider::selection::build_view;
use nc_code_services_provider::store::{provider_effective_config, ProviderStore};
use serde_json::{json, Value};

const MAX_PROMPT_FILES: usize = 20;
const MAX_DIFF_FILES: usize = 8;
const MAX_DIFF_CHARS: usize = 12_000;
const MAX_DIFF_CHARS_PER_FILE: usize = 2_000;
const MAX_CONVERSATION_CONTEXT_MESSAGES: usize = 12;
const MAX_CONVERSATION_CONTEXT_CHARS: usize = 4_000;
const MAX_CONVERSATION_CONTEXT_CHARS_PER_MESSAGE: usize = 600;
const MAX_COMMIT_MESSAGE_CHARS: usize = 1_000;
/// A commit message is one subject plus a short body; TS lets the agent resolve the model
/// default, the native path pins an explicit budget for the same one-shot call.
const COMPLETION_MAX_TOKENS: u32 = 1_024;
const CONVENTIONAL_TYPES: [&str; 11] = [
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore", "revert",
];
const ERROR_NAME: &str = "GitCommitMessageGenerationError";
const MODEL_UNAVAILABLE: &str = "The current model was not read.";
const REQUEST_FAILED: &str = "Model request failed.";
const INVALID_OUTPUT: &str = "The model did not return a usable Conventional Commit message.";

pub struct GenerateParams<'a> {
    pub workspace_path: &'a str,
    pub workspace_identity: Option<&'a str>,
    pub locale: Option<&'a str>,
    pub branch_name: Option<&'a str>,
    pub files: &'a [Value],
    pub diffs: &'a [Value],
    pub conversation_context: Option<&'a Value>,
}

#[async_trait]
pub trait CommitMessageGenerator: Send + Sync {
    async fn generate(&self, params: &GenerateParams<'_>) -> Result<Value, ServiceError>;
}

pub struct ProviderCommitMessageGenerator {
    store: Arc<ProviderStore>,
}

impl ProviderCommitMessageGenerator {
    // clippy::new_ret_no_self: returns Arc<dyn Trait>, so it is named from_store per its semantics.
    pub fn from_store(store: Arc<ProviderStore>) -> Arc<dyn CommitMessageGenerator> {
        Arc::new(Self { store })
    }
}

struct ResolvedModel {
    provider_id: String,
    model_id: String,
    base_url: String,
    api_type: String,
    api_key: String,
}

#[async_trait]
impl CommitMessageGenerator for ProviderCommitMessageGenerator {
    async fn generate(&self, params: &GenerateParams<'_>) -> Result<Value, ServiceError> {
        let model = self.resolve_model(params).await?;
        let prompt = build_prompt(params);
        let raw = complete(&model, &prompt).await?;
        let message = validate_generated_message(&raw)?;
        Ok(json!({
            "message": message,
            "providerId": model.provider_id,
            "model": model.model_id,        }))
    }
}

impl ProviderCommitMessageGenerator {
    /// `resolveCurrentModel` — the same `modelSelection.getView().preferredSelection` the TS
    /// side reads; an unusable selection is `model-unavailable`, never a guessed default.
    async fn resolve_model(
        &self,
        params: &GenerateParams<'_>,
    ) -> Result<ResolvedModel, ServiceError> {
        let snapshot = self.store.snapshot().await?;
        let view = build_view(&snapshot, None);
        let preferred = view
            .get("preferredSelection")
            .filter(|value| !value.is_null());
        let provider_id = preferred
            .and_then(|value| value.get("providerId"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let model_id = preferred
            .and_then(|value| value.get("modelId"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if provider_id.is_empty() || model_id.is_empty() {
            return Err(ServiceError::named(ERROR_NAME, MODEL_UNAVAILABLE));
        }
        let provider = match snapshot.provider(provider_id) {
            Some(provider) if provider.enabled => provider,
            _ => return Err(ServiceError::named(ERROR_NAME, MODEL_UNAVAILABLE)),
        };
        let config = provider_effective_config(provider, &snapshot);
        let read = |sections: [&str; 2]| {
            config
                .get(sections[0])
                .and_then(|section| section.get(sections[1]))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let base_url = match read(["api", "baseUrl"]) {
            Some(value) => value,
            None => return Err(ServiceError::named(ERROR_NAME, MODEL_UNAVAILABLE)),
        };
        let api_type = read(["api", "type"]).unwrap_or_else(|| "openai-chat-completions".into());
        let api_key = match read(["access", "apiKey"]) {
            Some(value) => value,
            None => return Err(ServiceError::named(ERROR_NAME, MODEL_UNAVAILABLE)),
        };
        let _ = params;
        Ok(ResolvedModel {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            base_url,
            api_type,
            api_key,
        })
    }
}

/// `complete` + `normalizeModelText` — one real request, text only, no tools.
async fn complete(model: &ResolvedModel, prompt: &str) -> Result<String, ServiceError> {
    let messages = vec![Message::text(MessageRole::User, prompt.to_string())];
    let options = RequestBodyOptions {
        model: &model.model_id,
        system: &[],
        messages: &messages,
        tools: &[],
        max_tokens: COMPLETION_MAX_TOKENS,
        stream: true,
    };
    let body = if model.api_type == "anthropic-messages" {
        request::build_anthropic_messages_body(&options)
    } else {
        request::build_openai_compatible_body(&options)
    };
    let body = match body {
        Ok(body) => body,
        Err(error) => return Err(request_failed(&error.message)),
    };
    let body_json = match serde_json::to_string(&body) {
        Ok(text) => text,
        Err(error) => return Err(request_failed(&error.to_string())),
    };
    let transport_request = TransportRequest {
        url: completion_url(&model.base_url, &model.api_type),
        headers: auth_headers(&model.api_type, &model.api_key),
        body: body_json,
    };
    let stream_request = StreamRequest {
        transport_request,
        provider_kind: if model.api_type == "anthropic-messages" {
            request::ProviderKind::Anthropic
        } else {
            request::ProviderKind::OpenaiCompatible
        },
        retry: nc_code_provider::retry::RetryOptions::default(),
        retry_budget: None,
        off_peak: false,
        preserve_provider_stream_boundaries: false,
        stream_idle_timeout_base_ms: None,
        stream_idle_timeout_retry_number: None,
        is_cancelled: Arc::new(|| false),
    };
    let mut transport = HttpTransport::new();
    let execution = executor::execute(&mut transport, &RealClock, &stream_request).await;
    if let Some(failure) = execution.failure {
        return Err(request_failed(&failure.message));
    }
    let mut text = String::new();
    for event in &execution.events {
        if let ModelStreamEvent::TextDelta { text: delta, .. } = event {
            text.push_str(delta);
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(request_failed(
            "The model response is missing text content.",
        ));
    }
    Ok(trimmed.to_string())
}

fn request_failed(detail: &str) -> ServiceError {
    ServiceError::named(ERROR_NAME, REQUEST_FAILED).with_data(json!({ "detail": detail }))
}

fn completion_url(base_url: &str, api_type: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if api_type == "anthropic-messages" {
        if trimmed.ends_with("/messages") {
            return trimmed.to_string();
        }
        return format!("{trimmed}/messages");
    }
    if trimmed.ends_with("/chat/completions") {
        return trimmed.to_string();
    }
    format!("{trimmed}/chat/completions")
}

/// Same header shape as the provider-settings probe (`catalog.rs:81`).
fn auth_headers(api_type: &str, api_key: &str) -> Vec<(String, String)> {
    if api_type == "anthropic-messages" {
        return vec![
            ("x-api-key".to_string(), api_key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
    }
    vec![
        ("authorization".to_string(), format!("Bearer {api_key}")),
        ("content-type".to_string(), "application/json".to_string()),
    ]
}

/// `buildGitCommitMessageGenerationPrompt` (`gitCommitMessageGenerator.ts:175`). Lines that
/// are empty are dropped before joining, and the `""` placeholders keep the blank separators
/// — the filter only removes entries that are exactly empty.
pub fn build_prompt(params: &GenerateParams<'_>) -> String {
    let normalized_branch_name = params
        .branch_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("(detached or unknown)");
    let language = resolve_language(params.locale);
    let visible: &[Value] = &params.files[..params.files.len().min(MAX_PROMPT_FILES)];
    let omitted_file_count = params.files.len().saturating_sub(visible.len());
    let file_summary = visible
        .iter()
        .map(|file| {
            format!(
                "- {} {} (+{}/-{})",
                file["kind"].as_str().unwrap_or_default(),
                file["repoRelativePath"].as_str().unwrap_or_default(),
                file["added"].as_i64().unwrap_or(0),
                file["removed"].as_i64().unwrap_or(0),
            )
        })
        .collect::<Vec<String>>()
        .join("\n");
    let diff_summary = build_diff_summary(params.diffs);
    let conversation_summary = build_conversation_context_summary(params.conversation_context);

    let lines: Vec<Option<String>> = vec![
        Some("Write exactly one Git commit message for the workspace changes below.".into()),
        Some("Return only the commit message text.".into()),
        None,
        Some("Hard requirements:".into()),
        Some("- The first line must be a valid Conventional Commit subject.".into()),
        Some(
            "- Use one of: feat, fix, docs, style, refactor, perf, test, build, ci, chore, revert."
                .into(),
        ),
        Some("- Keep the Conventional Commit type and optional scope in English.".into()),
        Some("- Write the subject and any body in the current language.".into()),
        Some("- Keep the subject under 72 characters.".into()),
        Some("- Use the current session conversation context only to infer user intent.".into()),
        Some("- Do not mention the conversation, chat, prompt, or user request explicitly.".into()),
        Some("- Do not explain your reasoning.".into()),
        Some("- Do not repeat these instructions.".into()),
        None,
        Some(format!("Current branch: {normalized_branch_name}")),
        Some(format!("Current language: {language}")),
        None,
        Some("Current session conversation context:".into()),
        Some(if conversation_summary.is_empty() {
            "(not provided)".to_string()
        } else {
            conversation_summary
        }),
        None,
        Some("Changed files:".into()),
        Some(if file_summary.is_empty() {
            "- (no file summary available)".to_string()
        } else {
            file_summary
        }),
        (omitted_file_count > 0).then(|| format!("- ... {omitted_file_count} more files")),
        None,
        Some("Diff excerpts:".into()),
        Some(if diff_summary.is_empty() {
            "(diff unavailable; infer only from the changed file summary)".to_string()
        } else {
            diff_summary
        }),
    ];
    lines
        .into_iter()
        .flatten()
        .collect::<Vec<String>>()
        .join("\n")
}

/// `resolveCommitMessageLanguage` (`gitCommitMessageGenerator.ts:224`). Only `zh*` counts as
/// Chinese; anything else, including an unreadable runtime locale, stays English. TS reads
/// `Intl.DateTimeFormat().resolvedOptions().locale`, whose native equivalent is the process
/// locale variables.
fn resolve_language(locale: Option<&str>) -> &'static str {
    let candidate = locale
        .map(str::to_string)
        .or_else(|| std::env::var("LC_ALL").ok())
        .or_else(|| std::env::var("LANG").ok())
        .unwrap_or_default();
    if candidate.to_lowercase().starts_with("zh") {
        "Chinese"
    } else {
        "English"
    }
}

/// `buildDiffSummary` (`gitCommitMessageGenerator.ts:257`).
pub fn build_diff_summary(diffs: &[Value]) -> String {
    let mut total_chars = 0usize;
    let mut chunks: Vec<String> = Vec::new();
    for diff in diffs.iter().take(MAX_DIFF_FILES) {
        let header = format!("--- {}", diff["path"].as_str().unwrap_or_default());
        let patch = diff["patch"].as_str().unwrap_or_default().trim();
        let summary = diff["summary"].as_str().unwrap_or_default().trim();
        let body = if !patch.is_empty() {
            patch.to_string()
        } else if !summary.is_empty() {
            summary.to_string()
        } else {
            "(diff unavailable)".to_string()
        };
        let remaining = MAX_DIFF_CHARS.saturating_sub(total_chars);
        if remaining == 0 {
            break;
        }
        let clipped = clip_text(
            &body,
            MAX_DIFF_CHARS_PER_FILE.min(remaining),
            "...diff truncated...",
        );
        let chunk = format!("{header}\n{clipped}");
        total_chars += chunk.chars().count();
        chunks.push(chunk);
    }
    chunks.join("\n\n")
}

/// `buildConversationContextSummary` (`gitCommitMessageGenerator.ts:279`).
pub fn build_conversation_context_summary(context: Option<&Value>) -> String {
    let messages: Vec<&Value> = context
        .and_then(|value| value.get("messages"))
        .and_then(Value::as_array)
        .map(|items| items.iter().collect())
        .unwrap_or_default();
    if messages.is_empty() {
        return String::new();
    }
    let omitted_configured = context
        .and_then(|value| value.get("omittedMessageCount"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as usize;
    let omitted = omitted_configured
        + messages
            .len()
            .saturating_sub(MAX_CONVERSATION_CONTEXT_MESSAGES);
    let mut chunks: Vec<String> = Vec::new();
    if omitted > 0 {
        chunks.push(format!("- {omitted} earlier messages omitted"));
    }
    let mut total_chars = 0usize;
    for message in messages
        .iter()
        .rev()
        .take(MAX_CONVERSATION_CONTEXT_MESSAGES)
        .rev()
    {
        let content = normalize_conversation_context_text(
            message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        if content.is_empty() {
            continue;
        }
        let role = if message.get("role").and_then(Value::as_str) == Some("assistant") {
            "Assistant"
        } else {
            "User"
        };
        let clipped_content = clip_text(
            &content,
            MAX_CONVERSATION_CONTEXT_CHARS_PER_MESSAGE,
            "...message truncated...",
        );
        let next_chunk = format!("{role}: {clipped_content}");
        let remaining = MAX_CONVERSATION_CONTEXT_CHARS.saturating_sub(total_chars);
        if remaining == 0 {
            break;
        }
        let clipped_chunk = clip_text(&next_chunk, remaining, "...conversation truncated...");
        total_chars += clipped_chunk.chars().count() + 1;
        chunks.push(clipped_chunk);
    }
    chunks.join("\n")
}

fn normalize_conversation_context_text(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalized
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']).to_string())
        .collect();
    // Collapse three or more consecutive newlines into two (`/\n{3,}/gu → "\n\n"`).
    let mut collapsed: Vec<String> = Vec::with_capacity(lines.len());
    let mut run = 0usize;
    for line in lines.drain(..) {
        if line.is_empty() {
            run += 1;
            // Keep at most 1 empty string between texts = 2 newlines (TS: /\n{3,}/gu → "\n\n").
            if run <= 1 {
                collapsed.push(String::new());
            }
        } else {
            run = 0;
            collapsed.push(line);
        }
    }
    lines = collapsed;
    let joined = lines.join("\n");
    joined.trim().to_string()
}

/// `clipText` (`gitCommitMessageGenerator.ts:330`).
fn clip_text(value: &str, max_chars: usize, marker: &str) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(marker.chars().count() + 1);
    let head: String = value.chars().take(keep).collect();
    format!("{}\n{marker}", head.trim_end())
}

/// `stripGeneratedCommitMessageDecorations` (`gitCommitMessageGenerator.ts:337`).
fn strip_generated_commit_message_decorations(value: &str) -> String {
    let trimmed = value.trim();
    let without_fence = match fenced_block(trimmed) {
        Some(inner) => inner,
        None => trimmed.to_string(),
    };
    let prefix = "commit message:";
    // TS does a regex replace with /^commit message:\s*/i; slicing directly with ..prefix.len() would panic when the
    // text is shorter than the prefix or splits off a character boundary, so using get(..) and getting None falls back safely.
    let without_prefix = if without_fence
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    {
        without_fence[prefix.len()..].trim_start()
    } else {
        without_fence.as_str()
    }
    .trim();
    let quoted = (without_prefix.starts_with('"') && without_prefix.ends_with('"'))
        || (without_prefix.starts_with('\'') && without_prefix.ends_with('\''));
    if quoted && without_prefix.chars().count() >= 2 {
        return without_prefix[1..without_prefix.len() - 1].to_string();
    }
    without_prefix.to_string()
}

/// `/^```(?:[a-zA-Z0-9_-]+)?\s*([\s\S]*?)\s*```$/` — the lazy inner group means the first
/// fence info string is dropped and everything up to the final fence is kept.
fn fenced_block(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("```")?;
    let body = rest.strip_suffix("```")?;
    let info_end = body
        .char_indices()
        .find(|(_, character)| {
            !character.is_alphanumeric() && *character != '_' && *character != '-'
        })
        .map(|(index, _)| index)
        .unwrap_or(body.len());
    let info = &body[..info_end];
    if !info.is_empty()
        && !info
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_' || character == '-')
    {
        return None;
    }
    let inner = body[info_end..].trim();
    if body[..info_end].is_empty() && inner.is_empty() && !body.is_empty() {
        // ```x``` with no whitespace stays a fence with empty content in JS only when the
        // info string is followed by whitespace; treat it as no fence.
        return None;
    }
    Some(inner.to_string())
}

/// `validateGeneratedGitCommitMessage` (`gitCommitMessageGenerator.ts:239`).
pub fn validate_generated_message(raw: &str) -> Result<String, ServiceError> {
    let stripped = strip_generated_commit_message_decorations(raw);
    let message: String = stripped
        .trim()
        .chars()
        .take(MAX_COMMIT_MESSAGE_CHARS)
        .collect::<String>()
        .trim()
        .to_string();
    if message.is_empty() {
        return Err(ServiceError::named(ERROR_NAME, INVALID_OUTPUT));
    }
    let subject = message
        .split_once(['\n', '\r'])
        .map_or(message.as_str(), |(head, _)| head)
        .trim()
        .to_string();
    if !is_conventional_commit_subject(&subject) {
        return Err(ServiceError::named(ERROR_NAME, INVALID_OUTPUT).with_data(json!({
            "preview": if subject.is_empty() { message.chars().take(120).collect::<String>() } else { subject },
        })));
    }
    Ok(message)
}

/// `/^(feat|fix|…)(\([^)]+\))?!?: .{1,100}$/` (`gitCommitMessageGenerator.ts:20`).
fn is_conventional_commit_subject(subject: &str) -> bool {
    let matched = CONVENTIONAL_TYPES
        .iter()
        .find(|kind| subject.starts_with(**kind));
    let Some(kind) = matched else {
        return false;
    };
    let mut rest = &subject[kind.len()..];
    if let Some(after_paren) = rest.strip_prefix('(') {
        let Some(close) = after_paren.find(')') else {
            return false;
        };
        if close == 0 {
            return false;
        }
        rest = &after_paren[close + 1..];
    }
    if let Some(after_bang) = rest.strip_prefix('!') {
        rest = after_bang;
    }
    let Some(body) = rest.strip_prefix(": ") else {
        return false;
    };
    let length = body.chars().count();
    (1..=100).contains(&length) && !body.contains('\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params<'a>(
        files: &'a [Value],
        diffs: &'a [Value],
        context: Option<&'a Value>,
    ) -> GenerateParams<'a> {
        GenerateParams {
            workspace_path: "G:/repo",
            workspace_identity: None,
            locale: None,
            branch_name: Some("main"),
            files,
            diffs,
            conversation_context: context,
        }
    }

    #[test]
    fn prompt_matches_the_ts_layout() {
        let files = vec![json!({
            "kind": "modified",
            "repoRelativePath": "src/a.ts",
            "added": 3,
            "removed": 1,
        })];
        let prompt = build_prompt(&params(&files, &[], None));
        assert!(prompt.starts_with("Write exactly one Git commit message for the workspace changes below.\nReturn only the commit message text.\nHard requirements:\n"));
        assert!(prompt.contains("Current branch: main"));
        assert!(prompt.contains("Current language: English"));
        assert!(prompt.contains("(not provided)"));
        assert!(prompt.contains("- modified src/a.ts (+3/-1)"));
        assert!(prompt.contains("(diff unavailable; infer only from the changed file summary)"));
        assert!(!prompt.contains("\n\n\n"));
    }

    #[test]
    fn prompt_lists_only_the_file_budget_and_reports_the_omission() {
        let files: Vec<Value> = (0..25)
            .map(|index| {
                json!({"kind": "added", "repoRelativePath": format!("f{index}.txt"), "added": 1, "removed": 0})
            })
            .collect();
        let prompt = build_prompt(&params(&files, &[], None));
        assert!(prompt.contains("- ... 5 more files"));
        assert!(!prompt.contains("f20.txt"));
        assert!(prompt.contains("f19.txt"));
    }

    #[test]
    fn prompt_uses_chinese_only_for_zh_locales() {
        let files: Vec<Value> = Vec::new();
        let chinese = build_prompt(&GenerateParams {
            workspace_path: "p",
            workspace_identity: None,
            locale: Some("zh-CN"),
            branch_name: None,
            files: &files,
            diffs: &files,
            conversation_context: None,
        });
        assert!(chinese.contains("Current language: Chinese"));
        assert!(chinese.contains("Current branch: (detached or unknown)"));
        assert!(chinese.contains("- (no file summary available)"));
    }

    #[test]
    fn diff_summary_is_clipped_per_file_and_total() {
        let long = "x".repeat(3_000);
        let diffs: Vec<Value> = (0..3)
            .map(|index| json!({"path": format!("f{index}"), "patch": long.clone(), "summary": Value::Null}))
            .collect();
        let summary = build_diff_summary(&diffs);
        assert!(summary.contains("--- f0"));
        assert!(summary.contains("...diff truncated..."));
        assert!(!summary.contains("--- f3"));
        assert!(summary.chars().count() < 3_000 * 3);
    }

    #[test]
    fn conversation_summary_keeps_the_latest_messages() {
        let context = json!({
            "omittedMessageCount": 2,
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "second\n\n\n\n"},
            ],
        });
        let summary = build_conversation_context_summary(Some(&context));
        assert!(summary.starts_with("- 2 earlier messages omitted\n"));
        assert!(summary.contains("User: first"));
        assert!(summary.contains("Assistant: second"));
        assert_eq!(build_conversation_context_summary(None), "");
        assert_eq!(
            build_conversation_context_summary(Some(&json!({"messages": []}))),
            ""
        );
    }

    #[test]
    fn normalizes_conversation_whitespace() {
        assert_eq!(
            normalize_conversation_context_text("a\n\n\n\nb  \n"),
            "a\n\nb"
        );
        assert_eq!(normalize_conversation_context_text("  x\r\ny  "), "x\ny");
    }

    #[test]
    fn validates_conventional_subjects() {
        assert_eq!(
            validate_generated_message("feat: add thing").unwrap(),
            "feat: add thing"
        );
        assert_eq!(
            validate_generated_message("  fix(scope)!: subject\n\nbody line  \n").unwrap(),
            "fix(scope)!: subject\n\nbody line"
        );
        assert_eq!(
            validate_generated_message("```\nchore: x\n```").unwrap(),
            "chore: x"
        );
        assert_eq!(
            validate_generated_message("Commit message: docs: update readme").unwrap(),
            "docs: update readme"
        );
        assert_eq!(
            validate_generated_message("\"test: quoted\"").unwrap(),
            "test: quoted"
        );
        assert_eq!(
            validate_generated_message("").unwrap_err().message,
            INVALID_OUTPUT
        );
        let error = validate_generated_message("not a conventional subject").unwrap_err();
        assert_eq!(error.name, ERROR_NAME);
        assert_eq!(error.message, INVALID_OUTPUT);
        assert_eq!(
            error.data,
            Some(Box::new(json!({"preview": "not a conventional subject"})))
        );
        assert_eq!(
            validate_generated_message("feat: ").unwrap_err().name,
            ERROR_NAME
        );
        assert!(validate_generated_message(&format!("feat: {}", "y".repeat(101))).is_err());
        assert!(validate_generated_message(&format!("feat: {}", "y".repeat(100))).is_ok());
    }

    #[test]
    fn clips_text_like_ts() {
        assert_eq!(clip_text("abc", 10, "..."), "abc");
        assert_eq!(clip_text("abcdefghij", 6, "…"), "abcd\n…");
        assert_eq!(clip_text("abc def", 6, "…"), "abc\n…");
    }

    #[test]
    fn completion_url_follows_api_type() {
        assert_eq!(
            completion_url("https://api.example.com/v1/", "openai-chat-completions"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            completion_url(
                "https://api.example.com/v1/chat/completions",
                "openai-chat-completions"
            ),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            completion_url("https://api.example.com/v1", "anthropic-messages"),
            "https://api.example.com/v1/messages"
        );
        assert_eq!(
            auth_headers("anthropic-messages", "k"),
            vec![
                ("x-api-key".to_string(), "k".to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ]
        );
        assert_eq!(
            auth_headers("openai-chat-completions", "k")[0],
            ("authorization".to_string(), "Bearer k".to_string())
        );
    }
}
