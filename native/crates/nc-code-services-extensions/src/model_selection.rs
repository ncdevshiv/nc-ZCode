//! `ModelSelection` validation and the Markdown/picker string boundary.
//!
//! Ports of `packages/shared/src/model-selection.ts`, `custom-model-value.ts`,
//! `subagent-markdown-selection.ts` and — for the one-way upgrade entry —
//! `legacy-model-provider-identity.ts` plus `official-glm-model-id.ts`.
//!
//! The legacy provider identity table below is the **only** copy inside the native service
//! crates; it is pinned by `legacy_identity_matches_the_typescript_tables`, whose expected
//! values were generated from the real TypeScript module rather than transcribed. The proper
//! home for that table is a shared native identity module, and the subagent state/markdown
//! migration is the last consumer keeping it here.

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning_level: Option<String>,
}

impl ModelSelection {
    /// `modelSelectionSchema.parse` output as JSON: `{providerId, modelId, options?}`.
    pub fn to_json(&self) -> Value {
        let mut selection = Map::new();
        selection.insert(
            "providerId".to_string(),
            Value::String(self.provider_id.clone()),
        );
        selection.insert("modelId".to_string(), Value::String(self.model_id.clone()));
        if let Some(reasoning_level) = &self.reasoning_level {
            let mut options = Map::new();
            options.insert(
                "reasoningLevel".to_string(),
                Value::String(reasoning_level.clone()),
            );
            selection.insert("options".to_string(), Value::Object(options));
        }
        Value::Object(selection)
    }
}

/// `modelSelectionSchema.safeParse`: strict object, trimmed non-empty strings.
pub fn parse_model_selection(value: Option<&Value>) -> Option<ModelSelection> {
    let object = value?.as_object()?;
    for key in object.keys() {
        if !matches!(key.as_str(), "providerId" | "modelId" | "options") {
            return None;
        }
    }
    let provider_id = object.get("providerId").and_then(Value::as_str)?.trim();
    let model_id = object.get("modelId").and_then(Value::as_str)?.trim();
    if provider_id.is_empty() || model_id.is_empty() {
        return None;
    }
    let mut reasoning_level = None;
    match object.get("options") {
        None | Some(Value::Null) => {}
        Some(Value::Object(options)) => {
            for key in options.keys() {
                if key != "reasoningLevel" {
                    return None;
                }
            }
            match options.get("reasoningLevel") {
                // TS: reasoningLevel is .optional() —— a missing key and an explicit null are synonymous (no level).
                None | Some(Value::Null) => {}
                Some(value) => {
                    let text = value.as_str()?.trim();
                    if text.is_empty() {
                        return None;
                    }
                    reasoning_level = Some(text.to_string());
                }
            }
        }
        Some(_) => return None,
    }
    Some(ModelSelection {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        reasoning_level,
    })
}

/// `normalizeSubagentModelSelection` (`subagents/subagentModelSelection.ts`): keep only the
/// structured fields and re-validate them.
pub fn normalize_subagent_model_selection(selection: Option<&Value>) -> Option<ModelSelection> {
    let object = selection?.as_object()?;
    let rebuilt = {
        let mut candidate = Map::new();
        candidate.insert("providerId".to_string(), object.get("providerId")?.clone());
        candidate.insert("modelId".to_string(), object.get("modelId")?.clone());
        if let Some(options) = object.get("options").and_then(Value::as_object) {
            if let Some(level) = options.get("reasoningLevel") {
                let mut rebuilt_options = Map::new();
                rebuilt_options.insert("reasoningLevel".to_string(), level.clone());
                candidate.insert("options".to_string(), Value::Object(rebuilt_options));
            }
        }
        Value::Object(candidate)
    };
    parse_model_selection(Some(&rebuilt))
}

pub const ZCODE_MODEL_REASONING_SEPARATOR: &str = "$";

/// `parseModelPickerValue`: `provider/model`, optionally `$reasoningLevel`.
fn parse_model_picker_value(value: &str) -> Option<ModelSelection> {
    let normalized = value.trim();
    let separator = normalized.find('/')?;
    if separator == 0 {
        return None;
    }
    let provider_id = &normalized[..separator];
    let raw_model_id = &normalized[separator + 1..];
    let reasoning_index = raw_model_id.find(ZCODE_MODEL_REASONING_SEPARATOR);
    let split_ok = reasoning_index
        .map(|index| index > 0 && index < raw_model_id.len() - 1)
        .unwrap_or(false);
    let rebuilt = if split_ok {
        let index = reasoning_index.unwrap_or(0);
        let mut candidate = Map::new();
        candidate.insert(
            "providerId".to_string(),
            Value::String(provider_id.to_string()),
        );
        candidate.insert(
            "modelId".to_string(),
            Value::String(raw_model_id[..index].to_string()),
        );
        let mut options = Map::new();
        options.insert(
            "reasoningLevel".to_string(),
            Value::String(raw_model_id[index + 1..].to_string()),
        );
        candidate.insert("options".to_string(), Value::Object(options));
        Value::Object(candidate)
    } else {
        let mut candidate = Map::new();
        candidate.insert(
            "providerId".to_string(),
            Value::String(provider_id.to_string()),
        );
        candidate.insert(
            "modelId".to_string(),
            Value::String(raw_model_id.to_string()),
        );
        Value::Object(candidate)
    };
    parse_model_selection(Some(&rebuilt))
}

pub const CUSTOM_MODEL_VALUE_PREFIX: &str = "custom:";

pub struct DecodedCustomModelValue {
    pub provider_id: String,
    pub model_name: Option<String>,
}

/// `safeDecodeUriComponent`: `decodeURIComponent`, falling back to the raw text when the
/// percent escapes are malformed.
fn safe_decode_uri_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                // A truncated escape: Node throws, and the caller keeps the original string.
                return value.to_string();
            }
            let hex = &value[index + 1..index + 3];
            match u8::from_str_radix(hex, 16) {
                Ok(byte) => {
                    out.push(byte);
                    index += 3;
                }
                Err(_) => return value.to_string(),
            }
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    // Percent-encoded UTF-8 arrives as separate byte escapes, so decode the whole buffer.
    String::from_utf8(out)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// `encodeCustomModelValue`: percent-encode both halves (JS `encodeURIComponent` set).
pub fn encode_custom_model_value(provider_id: &str, model_name: Option<&str>) -> String {
    let encoded_provider = encode_uri_component(provider_id);
    match model_name {
        None => format!("{CUSTOM_MODEL_VALUE_PREFIX}{encoded_provider}"),
        Some(model_name) => {
            format!(
                "{CUSTOM_MODEL_VALUE_PREFIX}{encoded_provider}:{}",
                encode_uri_component(model_name)
            )
        }
    }
}

fn encode_uri_component(value: &str) -> String {
    // `encodeURIComponent` keeps A-Za-z0-9 - _ . ! ~ * ' ( ) and percent-encodes everything
    // else, UTF-8 byte by byte.
    const UNRESERVED: &str =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
    let mut out = String::new();
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if UNRESERVED.contains(ch) {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `decodeCustomModelValue`; `None` when the value is not a `custom:` payload.
pub fn decode_custom_model_value(value: &str) -> Option<DecodedCustomModelValue> {
    let body = value.strip_prefix(CUSTOM_MODEL_VALUE_PREFIX)?;
    let separator = body.find(':');
    let Some(separator) = separator else {
        return Some(DecodedCustomModelValue {
            provider_id: safe_decode_uri_component(body),
            model_name: None,
        });
    };
    let legacy_parts: Vec<&str> = body.split(':').collect();
    if legacy_parts.len() >= 3 && legacy_parts[0] == "builtin" {
        return Some(DecodedCustomModelValue {
            provider_id: format!("{}:{}", legacy_parts[0], legacy_parts[1]),
            model_name: Some(safe_decode_uri_component(&legacy_parts[2..].join(":"))),
        });
    }
    Some(DecodedCustomModelValue {
        provider_id: safe_decode_uri_component(&body[..separator]),
        model_name: Some(safe_decode_uri_component(&body[separator + 1..])),
    })
}

/// `parseSubagentMarkdownSelection`: Markdown stores `model` (+ `thoughtLevel`) only.
pub fn parse_subagent_markdown_selection(
    frontmatter: &Map<String, Value>,
) -> Option<ModelSelection> {
    let raw = frontmatter.get("model")?.as_str()?;
    let value = raw.trim();
    if value.is_empty() || matches!(value, "inherit" | "main" | "sonnet" | "opus" | "haiku") {
        return None;
    }
    let selection = match decode_custom_model_value(value) {
        Some(decoded) => {
            let provider_id = decoded.provider_id.trim();
            let model_name = decoded.model_name.unwrap_or_default();
            if provider_id.is_empty() || model_name.trim().is_empty() {
                return None;
            }
            ModelSelection {
                provider_id: provider_id.to_string(),
                model_id: model_name.trim().to_string(),
                reasoning_level: None,
            }
        }
        None => parse_model_picker_value(value)?,
    };
    let reasoning_level = frontmatter
        .get("thoughtLevel")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Some(ModelSelection {
        reasoning_level,
        ..selection
    })
}

/// `formatSubagentMarkdownModel`: plain ids stay readable; ids carrying the separators are
/// stored through the lossless `custom:` encoding.
pub fn format_subagent_markdown_model(selection: &ModelSelection) -> String {
    if selection.provider_id.starts_with(CUSTOM_MODEL_VALUE_PREFIX)
        || selection.provider_id.contains('/')
        || selection.model_id.contains(ZCODE_MODEL_REASONING_SEPARATOR)
    {
        return encode_custom_model_value(&selection.provider_id, Some(&selection.model_id));
    }
    format!("{}/{}", selection.provider_id, selection.model_id)
}

// ---------------------------------------------------------------- legacy identity

/// `BUILTIN_PROVIDER_TEMPLATE_IDS`.
const TEMPLATE_ZAI: &str = "zai-api";
const TEMPLATE_BIGMODEL: &str = "bigmodel-api";
/// `migrateLegacyModelProviderId`: the six published renames, plus "an unknown `builtin:*` id
/// is not evidence of an old format".
pub fn migrate_legacy_model_provider_id(provider_id: &str) -> Option<String> {
    match provider_id {
        "builtin:bigmodel" => Some(TEMPLATE_BIGMODEL.to_string()),
        "builtin:zai" => Some(TEMPLATE_ZAI.to_string()),
        "builtin:bigmodel-start-plan" => Some("account:bigmodel-start-plan".to_string()),
        "builtin:zai-start-plan" => Some("account:zai-start-plan".to_string()),
        "builtin:bigmodel-coding-plan" => {
            Some("account:bigmodel-individual-coding-plan".to_string())
        }
        "builtin:zai-coding-plan" => Some("account:zai-individual-coding-plan".to_string()),
        other => {
            if other.starts_with("builtin:") {
                None
            } else {
                Some(other.to_string())
            }
        }
    }
}

/// The provider ids `migrateLegacyOfficialGlmModelId` rewrites (its inline regex, enumerated).
const OFFICIAL_GLM_PROVIDER_IDS: [&str; 12] = [
    "builtin:zai",
    "builtin:bigmodel",
    "builtin:zai-start-plan",
    "builtin:bigmodel-start-plan",
    "builtin:zai-coding-plan",
    "builtin:bigmodel-coding-plan",
    "account:zai-start-plan",
    "account:bigmodel-start-plan",
    "account:zai-individual-coding-plan",
    "account:bigmodel-individual-coding-plan",
    "account:zai-team-coding-plan",
    "account:bigmodel-team-coding-plan",
];

pub fn migrate_legacy_official_glm_model_id(provider_id: &str, model_id: &str) -> String {
    if OFFICIAL_GLM_PROVIDER_IDS.contains(&provider_id) {
        normalize_official_glm_model_id(model_id)
    } else {
        model_id.to_string()
    }
}

/// `OFFICIAL_GLM_MODEL_IDS` canonical spellings.
const OFFICIAL_GLM_MODEL_IDS: [&str; 22] = [
    "GLM-5.3",
    "GLM-5.3-Flash",
    "GLM-5V-Turbo",
    "GLM-5.2",
    "GLM-5.1",
    "GLM-5.1-Highspeed",
    "GLM-5",
    "GLM-5-Turbo",
    "GLM-4.7",
    "GLM-4.7-FlashX",
    "GLM-4.7-Flash",
    "GLM-4.6",
    "GLM-4.5-Air",
    "GLM-4.5",
    "GLM-4.6V",
    "GLM-4.6V-Flash",
    "GLM-4.6V-FlashX",
    "GLM-4.1V-Thinking-FlashX",
    "GLM-4.1V-Thinking-Flash",
    "GLM-4-FlashX-250414",
    "GLM-4-Flash-250414",
    "GLM-4V-Flash",
];

pub fn normalize_official_glm_model_id(model_id: &str) -> String {
    let lowered = model_id.to_lowercase();
    for candidate in OFFICIAL_GLM_MODEL_IDS {
        if candidate.to_lowercase() == lowered {
            return candidate.to_string();
        }
    }
    model_id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn selection(json: Value) -> Option<ModelSelection> {
        parse_model_selection(Some(&json))
    }

    #[test]
    fn model_selection_is_strict_and_trims() {
        assert_eq!(
            selection(json!({"providerId": " zai-api ", "modelId": "GLM-4.6"})),
            Some(ModelSelection {
                provider_id: "zai-api".to_string(),
                model_id: "GLM-4.6".to_string(),
                reasoning_level: None
            })
        );
        assert_eq!(
            selection(json!({"providerId": "a", "modelId": "b", "extra": 1})),
            None
        );
        assert_eq!(selection(json!({"providerId": "a", "modelId": ""})), None);
        // In TS `modelSelectionSchema`, options.reasoningLevel is `.optional()`
        // (packages/shared/src/model-selection.ts:10): empty options are valid = no level.
        assert_eq!(
            selection(json!({"providerId": "a", "modelId": "b", "options": {}}))
                .map(|value| value.reasoning_level),
            Some(None)
        );
        let level = selection(
            json!({"providerId": "a", "modelId": "b", "options": {"reasoningLevel": " high "}}),
        );
        assert_eq!(level.unwrap().reasoning_level.as_deref(), Some("high"));
    }

    #[test]
    fn picker_and_custom_encodings_round_trip() {
        let parsed = parse_model_picker_value("zai-api/GLM-4.6$high").unwrap();
        assert_eq!(parsed.model_id, "GLM-4.6");
        assert_eq!(parsed.reasoning_level.as_deref(), Some("high"));
        assert!(parse_model_picker_value("no-provider").is_none());
        let encoded = encode_custom_model_value("custom:tenant", Some("glm$4.6"));
        assert_eq!(
            encoded,
            "custom:custom%3Atenant%2Ftenant2:glm%244.6".replace("%2Ftenant2", "")
        );
        let decoded = decode_custom_model_value("custom:custom%3Ax:glm%244.6").unwrap();
        assert_eq!(decoded.provider_id, "custom:x");
        assert_eq!(decoded.model_name.as_deref(), Some("glm$4.6"));
        let legacy = decode_custom_model_value("custom:builtin:zai:GLM-4.6").unwrap();
        assert_eq!(legacy.provider_id, "builtin:zai");
        assert_eq!(legacy.model_name.as_deref(), Some("GLM-4.6"));
    }

    #[test]
    fn markdown_selection_ignores_inherit_aliases() {
        let mut frontmatter = Map::new();
        for alias in ["inherit", "main", "sonnet", "opus", "haiku"] {
            frontmatter.insert("model".to_string(), json!(alias));
            assert_eq!(parse_subagent_markdown_selection(&frontmatter), None);
        }
        frontmatter.insert("model".to_string(), json!("zai-api/GLM-4.6"));
        frontmatter.insert("thoughtLevel".to_string(), json!("high"));
        let parsed = parse_subagent_markdown_selection(&frontmatter).unwrap();
        assert_eq!(parsed.reasoning_level.as_deref(), Some("high"));
    }

    #[test]
    fn legacy_identity_matches_the_typescript_tables() {
        // Expected values minted from the real TS module (`node --experimental-strip-types`),
        // not transcribed by hand.
        let cases = [
            ("builtin:bigmodel", "bigmodel-api"),
            ("builtin:zai", "zai-api"),
            ("builtin:bigmodel-start-plan", "account:bigmodel-start-plan"),
            ("builtin:zai-start-plan", "account:zai-start-plan"),
            (
                "builtin:bigmodel-coding-plan",
                "account:bigmodel-individual-coding-plan",
            ),
            (
                "builtin:zai-coding-plan",
                "account:zai-individual-coding-plan",
            ),
            ("custom:zai", "custom:zai"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                migrate_legacy_model_provider_id(input).as_deref(),
                Some(expected),
                "{input}"
            );
        }
        assert_eq!(migrate_legacy_model_provider_id("builtin:unknown"), None);
        assert_eq!(
            migrate_legacy_official_glm_model_id("builtin:zai", "glm-4.6"),
            "GLM-4.6"
        );
        assert_eq!(
            migrate_legacy_official_glm_model_id("zai-api", "glm-4.6"),
            "glm-4.6"
        );
        assert_eq!(
            migrate_legacy_official_glm_model_id(
                "account:bigmodel-team-coding-plan",
                "GLM-4.7-FLASHX"
            ),
            "GLM-4.7-FlashX"
        );
    }
}
