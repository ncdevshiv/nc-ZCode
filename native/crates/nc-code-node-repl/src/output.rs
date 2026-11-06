//! The output surface and model budget of the `js` tool.
//!
//! Mirrors two TS sources of truth:
//! - Output schema: `contracts/src/tools/node-repl.ts:45-63 JsOutputSchema` (strict) —
//!   result?/logs/error?/images?/responseMeta?; `code` never appears here (see error.rs),
//!   and neither does `structuredResults` (the host's formatModelContent builds content blocks from it,
//!   the core handler node-repl.ts:265 never serializes it into the tool JSON).
//! - Model budget: `core/src/tool/handlers/node-repl.ts:285,392-397` —
//!   maxInlineBytes=1MB, maxModelBytes=64KiB (browser.documentation()'s full document
//!   would lose APIs past the old 30KB cap, while 64KiB stays bounded and covers the whole text), strategy=artifact,
//!   preview direction tail; the truncation notice is mirrored byte for byte from
//!   `core/src/tool/executor/result-serialization.ts:200-207`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The tool's default timeout (`node-repl-host/src/tool-contract.ts:3`; the executor layer and the tools/list
/// wording share it, so the real timeout and the model-visible description cannot drift).
pub const NODE_REPL_DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// The version the node_repl server advertises (`tool-contract.ts:19`; since 0.6.0 the tool surface narrowed to
/// `js` only — a host protocol change must be recognizable from it).
pub const NODE_REPL_SERVER_VERSION: &str = "0.6.0";
/// The upper bound of the `timeout_ms` argument, and the cap of the vm synchronous budget (`server.ts:39,54,228`:
/// MAX_SYNC_TIMEOUT_MS=120_000, so the input schema max and the syncTimeoutMs min share one source).
pub const TOOL_MAX_TIMEOUT_MS: u64 = 120_000;
/// The cap of the vm synchronous execution budget handed to the kernel (server.ts:228 `Math.min(timeoutMs,
/// MAX_SYNC_TIMEOUT_MS)`; once the argument reaches the schema ceiling the two are equal, and the constant names keep the roles distinct:
/// one is the tool-surface contract, the other a kernel frame field).
pub const TOOL_SYNC_TIMEOUT_CAP_MS: u64 = 120_000;
/// The byte budget of model-visible output (node-repl.ts:285 JS_MAX_MODEL_OUTPUT_BYTES).
pub const MAX_MODEL_OUTPUT_BYTES: usize = 64 * 1024;
/// The inline ceiling (node-repl.ts:393 maxInlineBytes); this island has no artifact persistence surface
/// (the host wiring provides it), so anything over the limit goes through the uniform truncation.
pub const MAX_INLINE_OUTPUT_BYTES: usize = 1_000_000;

/// One image collected by nodeRepl.emitImage (`{base64, mimeType}`;
/// kernel_driver.mjs:196 / schema node-repl.ts:57).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JsImage {
    pub base64: String,
    pub mime_type: String,
}

/// The JS error inside the tool output (strict `{name, message, stack?}`, schema node-repl.ts:48-53).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JsError {
    pub name: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

/// The strict output of the `js` tool (JsOutputSchema). The serialized field order matches the TS schema declaration order
/// (result → logs → error → images → responseMeta), and an omitted field drops the whole key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JsOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    pub logs: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<JsImage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_meta: Option<Value>,
}

impl JsOutput {
    pub fn logs_only(logs: impl Into<String>) -> Self {
        Self {
            result: None,
            logs: logs.into(),
            error: None,
            images: None,
            response_meta: None,
        }
    }

    /// Whether this is an empty success output (no result/error/images/meta and logs is empty).
    pub fn is_empty(&self) -> bool {
        self.result.is_none()
            && self.error.is_none()
            && self.images.as_ref().is_none_or(Vec::is_empty)
            && self.response_meta.is_none()
            && self.logs.is_empty()
    }
}

/// The model budget (the constant form of node-repl.ts:392-397 resultBudget).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputBudget {
    pub max_inline_bytes: usize,
    pub max_model_bytes: usize,
    /// The TS strategy; node-repl is always "artifact" (over budget it should land in the artifact store,
    /// this island has no persistence surface, so the truncation notice carries that strategy name verbatim).
    pub strategy: &'static str,
    /// The preview direction: node-repl is tail (the end usually holds the evaluation result).
    pub preview_tail: bool,
}

impl Default for OutputBudget {
    fn default() -> Self {
        Self {
            max_inline_bytes: MAX_INLINE_OUTPUT_BYTES,
            max_model_bytes: MAX_MODEL_OUTPUT_BYTES,
            strategy: "artifact",
            preview_tail: true,
        }
    }
}

/// The output after the budget has been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CappedOutput {
    /// The truncated JSON text (the content that goes to the model).
    pub json: String,
    pub truncated: bool,
    /// The UTF-8 byte count before truncation (goes into the truncation notice).
    pub original_bytes: usize,
}

/// Converge the output JSON into model-visible text within the budget.
///
/// Over budget the TS semantics apply: the budget first pays for the truncation notice's own bytes, the rest
/// gives the content its window by the preview direction (node-repl=tail), then the notice is appended —
/// mirroring result-serialization.ts:200-207 + result-content-projection.ts:181-192 line by line.
pub fn cap_model_output(output: &JsOutput, budget: &OutputBudget) -> CappedOutput {
    let json = serde_json::to_string(output).expect("JsOutput serializes to JSON");
    let original_bytes = json.len();
    let max = budget.max_model_bytes.min(budget.max_inline_bytes);
    if original_bytes <= max {
        return CappedOutput {
            json,
            truncated: false,
            original_bytes,
        };
    }
    let suffix = format!(
        "\n\n[Tool output truncated by resultBudget: originalBytes={original_bytes}, maxModelBytes={max}, strategy={}]",
        budget.strategy,
    );
    let json = fit_content_with_suffix(&json, max, &suffix, budget.preview_tail);
    CappedOutput {
        json,
        truncated: true,
        original_bytes,
    }
}

/// TS `fitContentWithSuffix` (result-content-projection.ts:181-192):
/// the notice first self-truncates in the head direction to max, then the remaining bytes give the content its window by direction.
fn fit_content_with_suffix(content: &str, max_bytes: usize, suffix: &str, tail: bool) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let suffix_fit = fit_string_to_bytes(suffix, max_bytes, false);
    let remaining = max_bytes.saturating_sub(suffix_fit.len());
    if remaining == 0 {
        return suffix_fit;
    }
    format!(
        "{}{}",
        fit_string_to_bytes(content, remaining, tail),
        suffix_fit
    )
}

/// TS `fitStringToBytes` (result-content-projection.ts:242-261): binary-searches over characters for the
/// largest UTF-8 byte prefix/suffix window — char-boundary safe, multibyte characters are never cut in half.
fn fit_string_to_bytes(value: &str, max_bytes: usize, tail: bool) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let chars: Vec<char> = value.chars().collect();
    let (mut low, mut high) = (0_usize, chars.len());
    while low < high {
        let mid = low.div_ceil(2).max(low + 1);
        let candidate: String = if tail {
            chars[chars.len() - mid..].iter().collect()
        } else {
            chars[..mid].iter().collect()
        };
        if candidate.len() <= max_bytes {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    if tail {
        chars[chars.len() - low..].iter().collect()
    } else {
        chars[..low].iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn output_serializes_strict_schema_shape() {
        let output = JsOutput {
            result: Some("2".into()),
            logs: "log line".into(),
            error: None,
            images: Some(vec![JsImage {
                base64: "AA==".into(),
                mime_type: "image/png".into(),
            }]),
            response_meta: Some(json!({"k": 1})),
        };
        let value = serde_json::to_value(&output).unwrap();
        // Field names match the TS schema key by key (camelCase), and strict carries no extra keys.
        let object = value.as_object().unwrap();
        let keys: Vec<&str> = object.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["result", "logs", "images", "responseMeta"]);
        assert_eq!(value["images"][0]["mimeType"], "image/png");
    }

    #[test]
    fn empty_fields_are_omitted_not_null() {
        let value = serde_json::to_value(JsOutput::logs_only("x")).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 1, "Only logs: {value}");
        assert_eq!(value["logs"], "x");
    }

    #[test]
    fn error_shape_excludes_code_field() {
        // code never enters the tool JSON (the design contract in error.rs: only the to_model_text prefix carries it).
        let error = JsError {
            name: "NodeReplUnavailableError".into(),
            message: "gone".into(),
            stack: None,
        };
        let value = serde_json::to_value(&error).unwrap();
        assert!(value.get("code").is_none());
        assert_eq!(value["name"], "NodeReplUnavailableError");
    }

    #[test]
    fn small_output_passes_through_uncapped() {
        let output = JsOutput::logs_only("hello");
        let capped = cap_model_output(&output, &OutputBudget::default());
        assert!(!capped.truncated);
        assert_eq!(capped.json, serde_json::to_string(&output).unwrap());
        assert_eq!(capped.original_bytes, capped.json.len());
    }

    #[test]
    fn oversized_output_keeps_tail_and_appends_ts_suffix() {
        let big = "a".repeat(MAX_MODEL_OUTPUT_BYTES * 2);
        let capped = cap_model_output(&JsOutput::logs_only(big), &OutputBudget::default());
        assert!(capped.truncated);
        assert!(capped.original_bytes > MAX_MODEL_OUTPUT_BYTES);
        // tail preview: the tail content survives, the head is dropped.
        assert!(!capped.json.starts_with("{\"logs\":\"a"));
        assert!(capped.json.contains("aaaa"));
        // The TS truncation notice field by field (result-serialization.ts:201).
        assert!(capped
            .json
            .contains("[Tool output truncated by resultBudget: "));
        assert!(capped
            .json
            .contains(&format!("originalBytes={}", capped.original_bytes)));
        assert!(capped
            .json
            .contains(&format!("maxModelBytes={MAX_MODEL_OUTPUT_BYTES}")));
        assert!(capped.json.contains("strategy=artifact"));
        // the total byte count stays inside the budget (notice + content).
        assert!(capped.json.len() <= MAX_MODEL_OUTPUT_BYTES);
    }

    #[test]
    fn truncation_is_char_boundary_safe_for_multibyte() {
        // When every character is 4 bytes the window must not cut a character in half (TS binary-searches over Array.from characters).
        let big = "Spicy".repeat(MAX_MODEL_OUTPUT_BYTES);
        let capped = cap_model_output(&JsOutput::logs_only(big), &OutputBudget::default());
        assert!(capped.truncated);
        assert!(std::str::from_utf8(capped.json.as_bytes()).is_ok());
        assert!(capped.json.len() <= MAX_MODEL_OUTPUT_BYTES);
    }

    #[test]
    fn head_preview_mode_keeps_prefix() {
        let budget = OutputBudget {
            preview_tail: false,
            ..OutputBudget::default()
        };
        let big = "b".repeat(MAX_MODEL_OUTPUT_BYTES * 2);
        let capped = cap_model_output(&JsOutput::logs_only(big), &budget);
        assert!(
            capped.json.contains("{\"logs\":\"bbb"),
            "head direction preserves prefix"
        );
    }

    #[test]
    fn constants_match_ts_sources() {
        assert_eq!(NODE_REPL_DEFAULT_TIMEOUT_MS, 60_000);
        assert_eq!(NODE_REPL_SERVER_VERSION, "0.6.0");
        assert_eq!(TOOL_MAX_TIMEOUT_MS, 120_000);
        assert_eq!(MAX_MODEL_OUTPUT_BYTES, 64 * 1024);
        assert_eq!(MAX_INLINE_OUTPUT_BYTES, 1_000_000);
    }
}
