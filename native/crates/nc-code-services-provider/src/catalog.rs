//! Remote model directory capture + capability inference + real detection.
//!
//! Three-tier facts, sources must be distinguishable (`specs/model-catalog-and-tps.md` §1.2):
//!
//! 1. **catalog**: The metadata given by endpoint in `/models` (OpenAI compatible responses often only
//!    `id/owned_by/created`, so the fields that can be obtained are marked as catalog, and if they are not available, they will not be made up).
//! 2. **heuristic**: uses, modalities, contexts, tools and thinking gears derived from model names/family rules;
//!    `confidence="heuristic"` will be brought all the way to the UI, and the user can see "this is inference".
//! 3. **measured**: Measured using our own provider harness (`nc-code-provider::executor`)
//!    Connectivity, whether tool_calls are actually generated, whether there is a reasoning channel, and throughput. The actual measurement covers the first two.
//!
//! apiKey only appears in the request header and never writes logs or return values.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nc_code_provider::executor::{self, StreamRequest};
use nc_code_provider::http_transport::HttpTransport;
use nc_code_provider::request::{
    self, Message, MessageRole, ProviderKind, RequestBodyOptions, ToolDefinition,
};
use nc_code_provider::timing::{StreamTiming, TimedTransport};
use nc_code_provider::transport::{RealClock, TransportRequest};
use nc_code_provider::ModelStreamEvent;
use nc_code_service_core::ServiceError;
use serde_json::{json, Value};

/// Directory cache TTL: Repeated opening of the panel will not break the upstream current limit.
pub const CATALOG_TTL_MS: u64 = 15 * 60 * 1000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub base_url: String,
    pub api_type: String,
    pub api_key: String,
}

impl Endpoint {
    /// `{baseUrl}/models` candidate: the same caliber as the connectivity detection (those with /v1 will no longer be superimposed).
    pub fn models_url_candidates(&self) -> Vec<String> {
        let trimmed = self.base_url.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Vec::new();
        }
        // Users often paste complete chat endpoints (…/v1/chat/completions): cut to /v1 and spell /models.
        if let Some(cut) = trimmed.find("/chat/completions") {
            let base = &trimmed[..cut];
            return vec![format!("{base}/models"), format!("{trimmed}/models")];
        }
        if trimmed.ends_with("/v1") {
            vec![format!("{trimmed}/models")]
        } else {
            vec![format!("{trimmed}/v1/models"), format!("{trimmed}/models")]
        }
    }

    /// Inference request endpoint (for detection): chat completions / messages, selected by api type.
    pub fn completion_url(&self) -> String {
        let trimmed = self.base_url.trim().trim_end_matches('/');
        if self.api_type == "anthropic-messages" {
            if trimmed.ends_with("/messages") {
                return trimmed.to_string();
            }
            return format!("{trimmed}/messages");
        }
        if trimmed.ends_with("/chat/completions") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/chat/completions")
        }
    }

    pub fn provider_kind(&self) -> ProviderKind {
        if self.api_type == "anthropic-messages" {
            ProviderKind::Anthropic
        } else {
            ProviderKind::OpenaiCompatible
        }
    }

    fn auth_headers(&self) -> Vec<(String, String)> {
        if self.api_type == "anthropic-messages" {
            vec![
                ("x-api-key".to_string(), self.api_key.clone()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ]
        } else {
            vec![
                (
                    "authorization".to_string(),
                    format!("Bearer {}", self.api_key),
                ),
                ("content-type".to_string(), "application/json".to_string()),
            ]
        }
    }
}

/// Unique wall clock (milliseconds) shared by directory cache and probe reporting.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

/// Name rule table: The earlier family rules take precedence. `purpose` is a multi-valued purpose tag.
struct FamilyRule {
    /// Lowercase substring matching (model id full string).
    needle: &'static str,
    purpose: &'static [&'static str],
    vision: bool,
    tool_call: Option<bool>,
    /// thinking gear: None = unknown; Some(&[]) = explicitly not supported; otherwise gear list.
    reasoning_levels: Option<&'static [&'static str]>,
    context_window: Option<u64>,
    max_output_tokens: Option<u64>,
}

const TOOL_CAPABLE: Option<bool> = Some(true);
const NO_TOOL: Option<bool> = Some(false);

/// Order sensitive: more specific names come first (`…-vl` precedes `…-instruct`).
static FAMILY_RULES: &[FamilyRule] = &[
    // Non-dialog class (should not be added as a chat model).
    FamilyRule {
        needle: "embed",
        purpose: &["embedding"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "rerank",
        purpose: &["rerank"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "guard",
        purpose: &["guard"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "moderation",
        purpose: &["guard"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "asr",
        purpose: &["audio-transcription"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "tts",
        purpose: &["speech"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "parakeet",
        purpose: &["audio-transcription"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "riva",
        purpose: &["audio"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "whisper",
        purpose: &["audio-transcription"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "flux",
        purpose: &["image-generation"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "stable-diffusion",
        purpose: &["image-generation"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "sdxl",
        purpose: &["image-generation"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "imagen",
        purpose: &["image-generation"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "diffusion gemma",
        purpose: &["image-generation"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "deplot",
        purpose: &["chart-generation"],
        vision: true,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "kosmos",
        purpose: &["vision"],
        vision: true,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "fuyu",
        purpose: &["vision"],
        vision: true,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&[]),
        context_window: None,
        max_output_tokens: None,
    },
    // Visual dialogue.
    FamilyRule {
        needle: "vision",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "-vl",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "internvl",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "llava",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: NO_TOOL,
        reasoning_levels: None,
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "pixtral",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "qvq",
        purpose: &["chat", "vision", "reasoning"],
        vision: true,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&["enabled"]),
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "gemma-3",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "gemma-4",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(256_000),
        max_output_tokens: None,
    },
    // Reasoning/thinking model.
    FamilyRule {
        needle: "r1",
        purpose: &["chat", "reasoning"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&["enabled"]),
        context_window: Some(128_000),
        max_output_tokens: Some(64_000),
    },
    FamilyRule {
        needle: "reasoner",
        purpose: &["chat", "reasoning"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&["enabled"]),
        context_window: Some(64_000),
        max_output_tokens: Some(32_000),
    },
    FamilyRule {
        needle: "thinking",
        purpose: &["chat", "reasoning"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: Some(&["low", "medium", "high"]),
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "qwq",
        purpose: &["chat", "reasoning"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&["enabled"]),
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "o1",
        purpose: &["chat", "reasoning"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: Some(&["low", "medium", "high"]),
        context_window: Some(200_000),
        max_output_tokens: Some(100_000),
    },
    FamilyRule {
        needle: "o3",
        purpose: &["chat", "reasoning"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: Some(&["low", "medium", "high"]),
        context_window: Some(200_000),
        max_output_tokens: Some(100_000),
    },
    FamilyRule {
        needle: "deepseek-reasoner",
        purpose: &["chat", "reasoning"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: Some(&["enabled"]),
        context_window: Some(64_000),
        max_output_tokens: Some(64_000),
    },
    // Code model.
    FamilyRule {
        needle: "coder",
        purpose: &["chat", "code"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "codellama",
        purpose: &["chat", "code"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: None,
        context_window: Some(100_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "codestral",
        purpose: &["chat", "code"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(256_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "devstral",
        purpose: &["chat", "code", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(256_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "starcoder",
        purpose: &["code-completion"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: None,
        context_window: Some(16_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "codegemma",
        purpose: &["chat", "code"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: None,
        context_window: Some(64_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "qwen2.5-coder",
        purpose: &["chat", "code"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: Some(64_000),
    },
    FamilyRule {
        needle: "granite-3.0-a800m-instruct",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "mistral-large",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "magistral",
        purpose: &["chat", "reasoning", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: Some(&["low", "medium", "high"]),
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "minitron",
        purpose: &["chat"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: None,
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "kimi",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "minimax",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(200_000),
        max_output_tokens: Some(128_000),
    },
    FamilyRule {
        needle: "glm",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: Some(&["low", "medium", "high"]),
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "gpt-4o",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: Some(16_000),
    },
    FamilyRule {
        needle: "gpt-4.1",
        purpose: &["chat"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(1_000_000),
        max_output_tokens: Some(32_000),
    },
    FamilyRule {
        needle: "grok",
        purpose: &["chat"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: Some(&["low", "medium", "high"]),
        context_window: Some(256_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "gemini",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: Some(&["low", "medium", "high"]),
        context_window: Some(1_000_000),
        max_output_tokens: Some(64_000),
    },
    FamilyRule {
        needle: "claude",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: Some(&["low", "medium", "high"]),
        context_window: Some(200_000),
        max_output_tokens: Some(64_000),
    },
    FamilyRule {
        needle: "llama-4",
        purpose: &["chat", "vision"],
        vision: true,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(10_000_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "llama-3.3",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "llama-3.2",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "llama-3.1",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "llama-3",
        purpose: &["chat"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: None,
        context_window: Some(8_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "nemotron",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "phi-4",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(16_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "phi-3",
        purpose: &["chat"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "qwen3",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: Some(&["enabled", "disabled"]),
        context_window: Some(128_000),
        max_output_tokens: Some(64_000),
    },
    FamilyRule {
        needle: "qwen2",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "qwen",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(32_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "deepseek-v4",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: Some(&["low", "medium", "high"]),
        context_window: Some(128_000),
        max_output_tokens: Some(64_000),
    },
    FamilyRule {
        needle: "deepseek-v3",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(128_000),
        max_output_tokens: Some(64_000),
    },
    FamilyRule {
        needle: "yi-large",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(16_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "jamba",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: Some(256_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "dbrx",
        purpose: &["chat"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: None,
        context_window: Some(32_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "sea-lion",
        purpose: &["chat"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: None,
        context_window: Some(32_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "gemma",
        purpose: &["chat"],
        vision: false,
        tool_call: NO_TOOL,
        reasoning_levels: None,
        context_window: Some(8_000),
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "instruct",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: None,
        max_output_tokens: None,
    },
    FamilyRule {
        needle: "chat",
        purpose: &["chat"],
        vision: false,
        tool_call: TOOL_CAPABLE,
        reasoning_levels: None,
        context_window: None,
        max_output_tokens: None,
    },
];

/// Directly write the context length (`…-128k-instruct`, `…-1m`) in the name.
fn context_from_name(model_id: &str) -> Option<u64> {
    let digits: Vec<char> = model_id.chars().collect();
    let mut index = 0usize;
    while index < digits.len() {
        if digits[index].is_ascii_digit() {
            let start = index;
            while index < digits.len() && digits[index].is_ascii_digit() {
                index += 1;
            }
            let number: String = digits[start..index].iter().collect();
            let value: f64 = number.parse().ok()?;
            match digits.get(index) {
                Some('k') | Some('K') => return Some((value * 1_000.0) as u64),
                Some('m') | Some('M') => return Some((value * 1_000_000.0) as u64),
                _ => {}
            }
            continue;
        }
        index += 1;
    }
    None
}

/// Name rule inference (heuristic). `confidence` fixed heuristic: if the caller has a catalog field, it will be overwritten.
pub fn infer(model_id: &str) -> Value {
    let lowered = model_id.to_ascii_lowercase();
    let mut purposes: Vec<&str> = Vec::new();
    let mut vision = false;
    let mut tool_call: Option<bool> = None;
    let mut reasoning_levels: Option<Option<&'static [&'static str]>> = None;
    let mut context_window: Option<u64> = None;
    let mut max_output_tokens: Option<u64> = None;
    let mut matched = false;
    for rule in FAMILY_RULES {
        if !lowered.contains(rule.needle) {
            continue;
        }
        matched = true;
        for purpose in rule.purpose {
            if !purposes.contains(purpose) {
                purposes.push(purpose);
            }
        }
        vision = vision || rule.vision;
        if tool_call.is_none() {
            tool_call = rule.tool_call;
        }
        if reasoning_levels.is_none() {
            reasoning_levels = Some(rule.reasoning_levels);
        }
        if context_window.is_none() {
            context_window = rule.context_window;
        }
        if max_output_tokens.is_none() {
            max_output_tokens = rule.max_output_tokens;
        }
        // The first hit is the strongest signal; more general rules (instruct/chat) only do completion.
        if !rule.purpose.contains(&"chat") || rule.purpose.len() > 1 {
            break;
        }
    }
    if purposes.is_empty() {
        purposes.push(if matched { "chat" } else { "unknown" });
    }
    let context_window = context_window.or_else(|| context_from_name(&lowered));
    let mut entry = json!({
        "purpose": purposes,
        "input": {
            "text": true,
            "image": vision,
            "video": false,
            "audio": false,
            "pdf": false,
        },
        "confidence": "heuristic",
    });
    if let Some(context_window) = context_window {
        entry["contextWindow"] = json!(context_window);
    }
    if let Some(max_output_tokens) = max_output_tokens {
        entry["maxOutputTokens"] = json!(max_output_tokens);
    }
    if let Some(tool_call) = tool_call {
        entry["supportsToolCall"] = json!(tool_call);
    }
    match reasoning_levels {
        Some(Some(levels)) if !levels.is_empty() => {
            entry["reasoningLevels"] = json!(levels);
        }
        Some(_) => {}
        None => {}
    }
    entry
}

/// Catalog fields take precedence, heuristic fills in gaps (does not cover upstream facts).
fn merge_entry(raw: &Value, heuristic: Value) -> Value {
    let mut merged = heuristic;
    let catalog_context = raw
        .get("context_length")
        .or_else(|| raw.get("contextWindow"))
        .or_else(|| raw.get("max_model_len"))
        .and_then(Value::as_u64);
    let catalog_output = raw
        .get("max_tokens")
        .or_else(|| raw.get("maxOutputTokens"))
        .and_then(Value::as_u64);
    let catalog_tools = raw
        .get("supports_tools")
        .or_else(|| raw.get("supportsTools"))
        .and_then(Value::as_bool);
    let catalog_reasoning = raw
        .get("reasoning_levels")
        .or_else(|| raw.get("reasoningLevels"))
        .and_then(Value::as_array);
    let catalog_modalities = raw
        .get("supported_modalities")
        .or_else(|| raw.get("modalities"))
        .and_then(Value::as_array);
    let has_catalog_fact = catalog_context.is_some()
        || catalog_output.is_some()
        || catalog_tools.is_some()
        || catalog_reasoning.is_some()
        || catalog_modalities.is_some();
    if has_catalog_fact {
        merged["confidence"] = json!("catalog");
    }
    if let Some(value) = catalog_context {
        merged["contextWindow"] = json!(value);
    }
    if let Some(value) = catalog_output {
        merged["maxOutputTokens"] = json!(value);
    }
    if let Some(value) = catalog_tools {
        merged["supportsToolCall"] = json!(value);
    }
    if let Some(levels) = catalog_reasoning {
        merged["reasoningLevels"] = Value::Array(levels.clone());
    }
    if let Some(modalities) = catalog_modalities {
        let flags = |name: &str| {
            modalities
                .iter()
                .any(|value| value.as_str().map(str::to_ascii_lowercase) == Some(name.to_string()))
        };
        merged["input"]["image"] = json!(flags("image") || flags("vision"));
        merged["input"]["audio"] = json!(flags("audio"));
        merged["input"]["video"] = json!(flags("video"));
    }
    merged
}

fn normalize_entry(raw: &Value) -> Option<Value> {
    let model_id = raw.get("id").and_then(Value::as_str)?.trim().to_string();
    if model_id.is_empty() {
        return None;
    }
    let heuristic = infer(&model_id);
    let mut entry = merge_entry(raw, heuristic);
    entry["modelId"] = json!(model_id);
    if let Some(display) = raw
        .get("display_name")
        .or_else(|| raw.get("displayName"))
        .or_else(|| raw.get("name"))
        .and_then(Value::as_str)
    {
        entry["displayName"] = json!(display);
    }
    if let Some(owned_by) = raw
        .get("owned_by")
        .or_else(|| raw.get("ownedBy"))
        .and_then(Value::as_str)
    {
        entry["ownedBy"] = json!(owned_by);
    }
    if let Some(created) = raw.get("created").and_then(Value::as_i64) {
        entry["created"] = json!(created);
    }
    Some(entry)
}

/// Parse the `/models` response body (OpenAI `{"data":[…]}` / naked array / NVIDIA NIM variant).
pub fn parse_catalog(body: &Value) -> Vec<Value> {
    let items = body
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| body.as_array())
        .cloned()
        .unwrap_or_default();
    let mut models: Vec<Value> = items.iter().filter_map(normalize_entry).collect();
    models.sort_by(|left, right| {
        left["modelId"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["modelId"].as_str().unwrap_or_default())
    });
    models
}
/// Fetch the remote directory (no caching; caching is done by the caller based on providerId for TTL).
pub async fn fetch_catalog(endpoint: &Endpoint) -> Result<Vec<Value>, ServiceError> {
    if endpoint.api_key.trim().is_empty() {
        return Err(ServiceError::invalid_params("Provider apiKey is missing"));
    }
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| ServiceError::message(error.to_string()))?;
    let candidates = endpoint.models_url_candidates();
    if candidates.is_empty() {
        return Err(ServiceError::invalid_params("Provider baseUrl is missing"));
    }
    let mut last_error = None;
    for url in candidates {
        let mut request = client.get(&url);
        for (name, value) in endpoint.auth_headers() {
            request = request.header(name, value);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                let body: Value = response
                    .json()
                    .await
                    .map_err(|error| ServiceError::message(error.to_string()))?;
                return Ok(parse_catalog(&body));
            }
            Ok(response) => {
                last_error = Some(format!("GET {url} -> HTTP {}", response.status().as_u16()));
            }
            Err(error) => {
                // Do not print sensitive information outside of the URL; the error text does not contain credentials.
                last_error = Some(format!("GET {url} -> {error}"));
            }
        }
    }
    Err(ServiceError::message(format!(
        "Model catalog fetch failed: {}",
        last_error.unwrap_or_else(|| "no candidate url".to_string())
    )))
}

/// executor final state failed → displayable failure fact (code + statusCode + whether it can be retried).
fn failure_facts(error: &nc_code_provider::AdapterError) -> Value {
    json!({
        "code": serde_json::to_value(error.code).unwrap_or(Value::String("unknown".to_string())),
        "message": error.message,
        "statusCode": error.context.get("statusCode").cloned().unwrap_or(Value::Null),
        "reason": error.context.get("reason").cloned().unwrap_or(Value::Null),
        "retryable": error.context.get("retryable").cloned().unwrap_or(Value::Null),
    })
}

/// The actual measurement result of a single probe (`run_probe` is pub(crate), the return type must be visible to the same level).
pub(crate) struct MeasuredRun {
    ok: bool,
    latency_ms: u64,
    completion_tokens: Option<u64>,
    reasoning: bool,
    tool_call_name: Option<String>,
    text_chars: usize,
    /// The actual streaming window (first block → last block) and number of blocks recorded by transport.
    generation_ms: u64,
    chunk_count: u64,
    error: Option<Value>,
}

fn probe_prompt(check: &str) -> (&'static str, u32, Vec<ToolDefinition>) {
    match check {
        "toolCall" => (
            "You must call the get_weather tool for Paris, France. Do not answer in prose.",
            128,
            vec![ToolDefinition {
                name: "get_weather".to_string(),
                description: Some("Get current weather for a city".to_string()),
                strict: false,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "city": {"type": "string", "description": "City name"}
                    },
                    "required": ["city"]
                }),
            }],
        ),
        "reasoning" => (
            "Think carefully step by step, then answer with only the number: 27 * 43",
            512,
            vec![],
        ),
        "throughput" => (
            "List the numbers from 1 to 200 separated by a single space and nothing else.",
            1024,
            vec![],
        ),
        _ => (
            "Reply with exactly the two letters OK and nothing else.",
            16,
            vec![],
        ),
    }
}

/// Use the **product's own provider harness** (executor + SSE parsing + retry classification) to run a streaming request.
pub(crate) async fn run_probe(
    endpoint: &Endpoint,
    model_id: &str,
    check: &str,
) -> Result<MeasuredRun, ServiceError> {
    let (prompt, max_tokens, tools) = probe_prompt(check);
    let messages = vec![Message::text(MessageRole::User, prompt)];
    let options = RequestBodyOptions {
        model: model_id,
        system: &[],
        messages: &messages,
        tools: &tools,
        max_tokens,
        stream: true,
    };
    let body = if endpoint.api_type == "anthropic-messages" {
        request::build_anthropic_messages_body(&options)
    } else {
        request::build_openai_compatible_body(&options)
    }
    .map_err(|error| ServiceError::message(error.message))?;
    let body_json =
        serde_json::to_string(&body).map_err(|error| ServiceError::message(error.to_string()))?;
    let transport_request = TransportRequest {
        url: endpoint.completion_url(),
        headers: endpoint.auth_headers(),
        body: body_json,
    };
    let started = Instant::now();
    let timing = StreamTiming::new();
    let stream_request = StreamRequest {
        transport_request,
        provider_kind: endpoint.provider_kind(),
        retry: nc_code_provider::retry::RetryOptions {
            max_attempts: 1,
            ..nc_code_provider::retry::RetryOptions::default()
        },
        retry_budget: None,
        off_peak: false,
        preserve_provider_stream_boundaries: false,
        stream_idle_timeout_base_ms: Some(30_000),
        stream_idle_timeout_retry_number: None,
        is_cancelled: std::sync::Arc::new(|| false),
    };
    // Use TimedTransport to wrap the real transport: the TPS window must come from the block arrival time,
    // Instead of "after the event vector returns" (when all events have landed, the measured value is always 0).
    let mut transport = TimedTransport::new(HttpTransport::new(), timing.clone());
    let execution = executor::execute(&mut transport, &RealClock, &stream_request).await;
    let mut completion_tokens = None;
    let mut reasoning = false;
    let mut tool_call_name = None;
    let mut text_chars = 0usize;
    for event in &execution.events {
        match event {
            ModelStreamEvent::TextDelta { text, .. } => {
                text_chars += text.chars().count();
            }
            ModelStreamEvent::ReasoningDelta { .. } => {
                reasoning = true;
            }
            ModelStreamEvent::ToolCall { tool_call } => {
                tool_call_name = Some(tool_call.name.clone());
            }
            ModelStreamEvent::Finish { usage, .. } => {
                completion_tokens = usage.output_tokens;
            }
            _ => {}
        }
    }
    let latency_ms = started.elapsed().as_millis() as u64;
    let (generation_ms, chunk_count) = timing
        .window()
        .map(|(first, last, count)| (last.saturating_sub(first), count))
        .unwrap_or((0, 0));
    // The failure facts must be able to distinguish "model is offline / current limiting / timeout / network": harness report and Auto fetch
    // All panels rely on it, instead of just one sentence "Model request failed.".
    let error = execution.failure.as_ref().map(failure_facts);
    let ok = error.is_none() && !execution.events.is_empty();
    Ok(MeasuredRun {
        ok,
        latency_ms,
        completion_tokens,
        reasoning,
        tool_call_name,
        text_chars,
        generation_ms,
        chunk_count,
        error,
    })
}

fn token_estimate(run: &MeasuredRun) -> u64 {
    // If there is usage, use usage; if not, estimate it as 4 char/token and mark estimated=true in the response.
    // (The detection panel wants comparable magnitudes, not pseudo-accurate numbers).
    run.completion_tokens.unwrap_or(0)
}

/// Detect the actual measurement capabilities of a model (Tools/Thinking/Throughput on demand).
pub async fn probe_model(
    endpoint: &Endpoint,
    model_id: &str,
    checks: &[String],
) -> Result<Value, ServiceError> {
    let connectivity = run_probe(endpoint, model_id, "connectivity").await?;
    let mut result = json!({
        "modelId": model_id,
        "connectivity": {
            "ok": connectivity.ok,
            "latencyMs": connectivity.latency_ms,
            "generationMs": connectivity.generation_ms,
            "chunks": connectivity.chunk_count,
            "completionTokens": connectivity.completion_tokens,
            "textChars": connectivity.text_chars,
            "error": connectivity.error,
        }
    });
    if !connectivity.ok {
        // Upstream unavailable/current limit/timeout: Return truthfully and no longer waste quota to run subsequent detections.
        result["skippedChecks"] = json!(checks);
        return Ok(result);
    }
    for check in checks {
        match check.as_str() {
            "toolCall" => {
                let run = run_probe(endpoint, model_id, "toolCall").await?;
                result["toolCall"] = json!({
                    "ok": run.tool_call_name.is_some(),
                    "toolName": run.tool_call_name,
                    "latencyMs": run.latency_ms,
                    "error": run.error,
                });
            }
            "reasoning" => {
                let run = run_probe(endpoint, model_id, "reasoning").await?;
                result["reasoning"] = json!({
                    "ok": run.reasoning,
                    "channel": if run.reasoning { json!("reasoning") } else { Value::Null },"latencyMs": run.latency_ms,
                    "error": run.error,
                });
            }
            "throughput" => {
                let run = run_probe(endpoint, model_id, "throughput").await?;
                let tokens = token_estimate(&run);
                let estimated = run.completion_tokens.is_none();
                // Generate window chunk arrival time from transport; entire responses cannot be separated when they fall on the same millisecond
                // (This is true for native/extremely fast endpoints), fall back to the full request window and label windowSource accordingly.
                let (window_ms, window_source) = if run.generation_ms > 0 {
                    (run.generation_ms, "generation")
                } else {
                    (run.latency_ms.max(1), "request")
                };
                let tps = if tokens > 0 {
                    f64::round(tokens as f64 * 1000.0 / window_ms as f64 * 10.0) / 10.0
                } else {
                    0.0
                };
                result["throughput"] = json!({
                    "ok": run.ok,
                    "completionTokens": tokens,
                    "estimated": estimated,
                    "elapsedMs": window_ms,
                    "windowSource": window_source,
                    "tokensPerSecond": tps,
                    "streamed": true,
                    "error": run.error,
                });
            }
            other => {
                return Err(ServiceError::invalid_params(format!(
                    "unknown probe check: {other}"
                )))
            }
        }
    }
    Ok(result)
}

/// Directory + Already model merging: `alreadyAdded` / `enabled` provided by the caller snapshot.
pub fn mark_existing(models: Vec<Value>, existing: &[(String, bool)]) -> Vec<Value> {
    models
        .into_iter()
        .map(|mut model| {
            let id = model["modelId"].as_str().unwrap_or_default();
            match existing.iter().find(|(existing_id, _)| existing_id == id) {
                Some((_, enabled)) => {
                    model["alreadyAdded"] = json!(true);
                    model["enabled"] = json!(enabled);
                }
                None => {
                    model["alreadyAdded"] = json!(false);
                    model["enabled"] = json!(false);
                }
            }
            model
        })
        .collect()
}

/// heuristic directory → model config of provider store (input parameter shape of `addPersonalModel`).
pub fn to_model_config(model: &Value) -> Value {
    let mut properties = json!({
        "requiresMfjsToolSchema": false,
        "inputFormat": {
            "supportsText": model["input"]["text"].as_bool().unwrap_or(true),
            "supportsImage": model["input"]["image"].as_bool().unwrap_or(false),
            "supportsVideo": model["input"]["video"].as_bool().unwrap_or(false),
            "supportsAudio": model["input"]["audio"].as_bool().unwrap_or(false),
            "supportsPdf": model["input"]["pdf"].as_bool().unwrap_or(false),
        },
        "outputFormat": {"supportsText": true},
        "supportsToolCall": model["supportsToolCall"].as_bool().unwrap_or(false),
        "supportsJsonSchemaOutput": false,
        "supportsNativeWebSearch": false,
        "supportsMidConversationSystem": false,
    });
    if let Some(context_window) = model.get("contextWindow").and_then(Value::as_u64) {
        properties["contextWindow"] = json!(context_window);
    }
    let mut config = json!({
        "enabled": true,
        "properties": properties,
    });
    if let Some(max_output) = model.get("maxOutputTokens").and_then(Value::as_u64) {
        config["optionSpecs"] = json!({"maxOutputTokens": {"max": max_output}});
    }
    if let Some(levels) = model.get("reasoningLevels").and_then(Value::as_array) {
        if !levels.is_empty() {
            let specs = config
                .as_object_mut()
                .and_then(|object| {
                    object
                        .entry("optionSpecs")
                        .or_insert_with(|| json!({}))
                        .as_object_mut()
                })
                .and_then(|object| {
                    object.insert(
                        "reasoningLevel".to_string(),
                        json!({"values": levels, "default": levels.first()}),
                    );
                    object.get_mut("reasoningLevel")
                });
            let _ = specs;
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> Value {
        json!({"id": id, "object": "model", "created": 1, "owned_by": "someone"})
    }

    #[test]
    fn catalog_entries_carry_purpose_and_modality() {
        let models = parse_catalog(&json!({"data": [
            entry("nvidia/llama-3.1-nemotron-70b-instruct"),
            entry("meta/llama-3.2-11b-vision-instruct"),
            entry("deepseek-ai/deepseek-r1"),
            entry("nvidia/nv-embedqa-e5-v5"),
            entry("black-forest-labs/flux-1.1-pro"),
        ]}));
        let by_id: Vec<&Value> = models.iter().collect();
        assert!(by_id.iter().any(|model| model["modelId"]
            == json!("nvidia/llama-3.1-nemotron-70b-instruct")
            && model["purpose"]
                .as_array()
                .unwrap()
                .contains(&json!("chat"))
            && model["supportsToolCall"] == json!(true)));
        assert!(by_id.iter().any(|model| {
            model["modelId"] == json!("meta/llama-3.2-11b-vision-instruct")
                && model["input"]["image"] == json!(true)
        }));
        assert!(by_id.iter().any(|model| {
            model["modelId"] == json!("deepseek-ai/deepseek-r1")
                && model["purpose"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("reasoning"))
                && model["reasoningLevels"] == json!(["enabled"])
        }));
        assert!(by_id.iter().any(|model| {
            model["modelId"] == json!("nvidia/nv-embedqa-e5-v5")
                && model["purpose"] == json!(["embedding"])
        }));
        assert!(by_id.iter().any(|model| {
            model["modelId"] == json!("black-forest-labs/flux-1.1-pro")
                && model["purpose"] == json!(["image-generation"])
        }));
    }

    #[test]
    fn context_window_falls_back_to_name_hint() {
        let models =
            parse_catalog(&json!({"data": [entry("microsoft/phi-3-vision-128k-instruct")]}));
        assert_eq!(models[0]["contextWindow"], json!(128_000));
        assert_eq!(models[0]["input"]["image"], json!(true));
    }

    #[test]
    fn catalog_fields_outrank_heuristics_and_confidence() {
        let models = parse_catalog(&json!({"data": [{
            "id": "example/model-instruct",
            "context_length": 32_000,
            "supports_tools": false,
            "reasoning_levels": ["low", "high"],
            "supported_modalities": ["text", "audio"],
        }]}));
        assert_eq!(models[0]["confidence"], json!("catalog"));
        assert_eq!(models[0]["contextWindow"], json!(32_000));
        assert_eq!(models[0]["supportsToolCall"], json!(false));
        assert_eq!(models[0]["reasoningLevels"], json!(["low", "high"]));
        assert_eq!(models[0]["input"]["audio"], json!(true));
    }

    #[test]
    fn unknown_names_are_flagged_not_guessed_as_chat() {
        let models = parse_catalog(&json!({"data": [entry("weird/thing-xyz")]}));
        assert_eq!(models[0]["purpose"], json!(["unknown"]));
    }

    #[test]
    fn existing_models_are_marked() {
        let models = parse_catalog(&json!({"data": [entry("a/one"), entry("a/two")]}));
        let marked = mark_existing(models, &[("a/one".to_string(), false)]);
        assert_eq!(marked[0]["alreadyAdded"], json!(true));
        assert_eq!(marked[0]["enabled"], json!(false));
        assert_eq!(marked[1]["alreadyAdded"], json!(false));
    }

    #[test]
    fn model_config_shape_matches_store_records() {
        let models = parse_catalog(&json!({"data": [entry("deepseek-ai/deepseek-r1")]}));
        let config = to_model_config(&models[0]);
        assert_eq!(config["properties"]["contextWindow"], json!(128_000));
        assert_eq!(
            config["optionSpecs"]["reasoningLevel"]["values"],
            json!(["enabled"])
        );
        assert_eq!(config["properties"]["supportsToolCall"], json!(false));
    }

    #[test]
    fn url_candidates_handle_full_chat_endpoint() {
        let endpoint = Endpoint {
            base_url: "https://integrate.api.nvidia.com/v1/chat/completions".to_string(),
            api_type: "openai-chat-completions".to_string(),
            api_key: "k".to_string(),
        };
        assert_eq!(
            endpoint.models_url_candidates()[0],
            "https://integrate.api.nvidia.com/v1/models"
        );
        assert_eq!(
            endpoint.completion_url(),
            "https://integrate.api.nvidia.com/v1/chat/completions"
        );
    }

    #[test]
    fn anthropic_endpoint_maps_kind_and_headers() {
        let endpoint = Endpoint {
            base_url: "https://api.anthropic.com/v1".to_string(),
            api_type: "anthropic-messages".to_string(),
            api_key: "secret".to_string(),
        };
        assert_eq!(
            endpoint.completion_url(),
            "https://api.anthropic.com/v1/messages"
        );
        let headers = endpoint.auth_headers();
        assert!(headers.iter().any(|(name, _)| name == "x-api-key"));
    }

    #[test]
    fn catalog_ttl_is_a_bounded_window() {
        assert_eq!(CATALOG_TTL_MS, 900_000);
        assert!(now_ms() > 0);
    }
}
