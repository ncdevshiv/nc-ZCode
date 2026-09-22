//! 严格工具 schema 折叠 + tool 参数归一。
//!
//! 对齐 TS `strict-tool-schema.ts`（可折叠约束进 description、不可表达形状 fail-closed 返回
//! None）、`tool-call-validation.ts`（空名工具调用判定）、`tool-input-normalization.ts`
//! （参数 JSON 解析失败降级空对象）。

use crate::{AdapterError, ModelErrorCode};
use serde_json::{Map, Value};

/// strict 模式认识的 string format；其余 format 折进 description。
const STRICT_STRING_FORMATS: [&str; 10] = [
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "uri",
    "ipv4",
    "ipv6",
    "uuid",
];

/// 判断 modelId 是否为 Anthropic 首方直连模型（裸 `claude-` 前缀）。
pub fn is_anthropic_first_party_model_id(model_id: Option<&str>) -> bool {
    model_id.is_some_and(|id| id.starts_with("claude-"))
}

/// 把 schema 转成 strict 子集里的等价物；形状不可表达时返回 None（调用方原样发送）。
pub fn to_strict_tool_schema(schema: &Value) -> Option<Value> {
    strict_node(schema)
}

/// 折进 description 的关键字及其措辞（对齐 FOLDED_KEYWORDS）。
fn folded_note(key: &str, value: &Value) -> Option<String> {
    let text = |prefix: &str| Some(format!("{prefix} {}", value_as_js_string(value)));
    match key {
        "minimum" => text("minimum"),
        "maximum" => text("maximum"),
        "exclusiveMinimum" => text("greater than"),
        "exclusiveMaximum" => text("less than"),
        "multipleOf" => text("multiple of"),
        "minLength" => Some(format!("at least {} characters", value_as_js_string(value))),
        "maxLength" => Some(format!("at most {} characters", value_as_js_string(value))),
        "pattern" => Some(format!("must match /{}/", value_as_js_string(value))),
        "minItems" => Some(format!("at least {} items", value_as_js_string(value))),
        "maxItems" => Some(format!("at most {} items", value_as_js_string(value))),
        "default" => Some(format!("default {}", serde_json::to_string(value).ok()?)),
        _ => None,
    }
}

/// 近似 JS `String(v)`：数字/布尔按字面量，字符串原样，其它退化为 JSON。
fn value_as_js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// None = INELIGIBLE（$ref/$defs/prefixItems、additionalProperties 是 schema、空 schema 等）。
fn strict_node(node: &Value) -> Option<Value> {
    let obj = node.as_object()?;

    // 入口判定：$ref/$defs（递归）与 tuple（prefixItems）strict 子集表达不了。
    if obj.contains_key("$ref") || obj.contains_key("$defs") || obj.contains_key("prefixItems") {
        return None;
    }

    let mut notes: Vec<String> = Vec::new();
    let mut out = Map::new();
    for (key, value) in obj {
        if let Some(note) = folded_note(key, value) {
            notes.push(note);
            continue;
        }
        if key == "format" {
            if let Some(format) = value.as_str() {
                if !STRICT_STRING_FORMATS.contains(&format) {
                    notes.push(format!("format {format}"));
                    continue;
                }
            }
        }
        out.insert(key.clone(), value.clone());
    }

    // 递归：properties / items / anyOf / allOf。
    if let Some(properties) = out.get("properties").and_then(Value::as_object) {
        let mut folded_properties = Map::new();
        for (name, child) in properties {
            folded_properties.insert(name.clone(), strict_node(child)?);
        }
        out.insert("properties".to_string(), Value::Object(folded_properties));
    }
    if let Some(items) = out.get("items") {
        // 数组形态的 items（遗留 tuple）TS 走 isRecord=false 分支，不递归。
        if items.is_object() {
            out.insert("items".to_string(), strict_node(items)?);
        }
    }
    for combinator in ["anyOf", "allOf"] {
        let Some(branches) = out.get(combinator).and_then(Value::as_array) else {
            continue;
        };
        let mut folded_branches = Vec::with_capacity(branches.len());
        for branch in branches {
            if !branch.is_object() {
                return None;
            }
            folded_branches.push(strict_node(branch)?);
        }
        out.insert(combinator.to_string(), Value::Array(folded_branches));
    }

    let types: Vec<&str> = match out.get("type") {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(single)) => vec![single.as_str()],
        _ => Vec::new(),
    };
    if types.contains(&"object") {
        // Record<string, T>：additionalProperties 是 schema → 严格子集表达不了。
        if out
            .get("additionalProperties")
            .is_some_and(Value::is_object)
        {
            return None;
        }
        if !out
            .get("additionalProperties")
            .is_some_and(|value| value == &Value::Bool(false))
        {
            out.insert("additionalProperties".to_string(), Value::Bool(false));
        }
        if !out.get("properties").is_some_and(Value::is_object) {
            out.insert("properties".to_string(), Value::Object(Map::new()));
        }
    }
    // 空 schema（unknown）：没有任何可约束的东西，strict 不接受。
    if out.is_empty() && notes.is_empty() {
        return None;
    }

    if !notes.is_empty() {
        let existing = out
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let folded = notes.join("; ");
        let description = if existing.is_empty() {
            folded
        } else {
            format!("{existing} ({folded})")
        };
        out.insert("description".to_string(), Value::String(description));
    }
    Some(Value::Object(out))
}

// ============================================================
// tool 名称与参数归一（对齐 tool-call-validation / tool-input-normalization）
// ============================================================

// tool_call_id 含 String，不能 Copy。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolNameContext {
    pub provider_executed: Option<bool>,
    pub tool_call_id: Option<String>,
}

/// 归一化工具名：trim 后非空用 trim 结果；空名时 client-executed 且有 id 可用原值闭合。
pub fn normalize_model_tool_name(
    value: Option<&str>,
    context: &ToolNameContext,
) -> Result<String, AdapterError> {
    if let Some(raw) = value {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        let has_closable_id = context
            .tool_call_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty());
        if context.provider_executed != Some(true) && has_closable_id {
            // client-executed 空名调用仍可用原 id 闭合，避免整个 turn 停止。
            return Ok(raw.to_string());
        }
    }
    Err(AdapterError::new(
        ModelErrorCode::InvalidModelResponse,
        "Model returned an invalid tool call: tool name is empty.",
    ))
}

/// 归一化 tool 参数：undefined/null/空串/坏 JSON 一律降级 `{}`，对象原样。
pub fn normalize_model_tool_input(input: &Value) -> Value {
    match input {
        Value::Null => Value::Object(Map::new()),
        Value::String(raw) => {
            if raw.is_empty() {
                return Value::Object(Map::new());
            }
            let stripped = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
            match serde_json::from_str::<Value>(stripped) {
                Ok(Value::Null) | Err(_) => Value::Object(Map::new()),
                Ok(other) => other,
            }
        }
        other => other.clone(),
    }
}
