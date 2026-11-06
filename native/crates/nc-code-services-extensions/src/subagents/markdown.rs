//! Subagent Markdown parsing/serialization —— the Rust counterpart of TS "subagents/subagentMarkdown.ts".
//!
//! The runtime uses loose frontmatter semantics (loose values override YAML, mcpServers keeps the YAML array shape),
//! and Settings reads with the same semantics, so a profile already loaded by the CLI does not disappear from the GUI because its YAML is not strict.

use serde_json::{Map, Value};

use crate::model_selection::{format_subagent_markdown_model, parse_subagent_markdown_selection};
use crate::yaml_lite;

const BACKTICK: char = '\u{60}';
const VALID_COLORS: [&str; 8] = [
    "red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan",
];
const VALID_PERMISSION_MODES: [&str; 2] = ["auto", "plan"];

/// createAgentStateId：source:scope:name.trim().toLowerCase()
pub fn create_agent_state_id(name: &str, scope: &str, source: &str) -> String {
    format!("{source}:{scope}:{}", name.trim().to_lowercase())
}

/// createPluginAgentStateId: plugin:pluginId:lowercased bare name
pub fn create_plugin_agent_state_id(plugin_id: &str, agent_name: &str) -> String {
    format!("plugin:{plugin_id}:{}", agent_name.trim().to_lowercase())
}

/// The parse result: agent (success) or diagnostic (failure), structurally the same as TS ParseSubagentMarkdownResult.
pub struct ParsedMarkdown {
    pub agent: Option<Value>,
    pub diagnostic: Option<Value>,
}

/// parseSubagentMarkdown: frontmatter → AgentSummary JSON; a missing name/description yields a diagnostic.
pub fn parse_subagent_markdown(content: &str, path: &str, scope: &str) -> ParsedMarkdown {
    let Some((frontmatter_text, body)) = split_markdown_frontmatter(content) else {
        return diagnostic(
            "agent_missing_frontmatter",
            &format!("Agent Markdown must include frontmatter: {path}"),
            path,
        );
    };
    let frontmatter = parse_runtime_compatible_frontmatter(&frontmatter_text);

    let name = scalar_string(frontmatter.get("name"));
    let description = scalar_string(frontmatter.get("description")).map(|d| d.replace("\\n", "\n"));
    let Some(name) = name else {
        return diagnostic(
            "agent_missing_required_frontmatter",
            &format!("Agent frontmatter must include name: {path}"),
            path,
        );
    };
    let Some(description) = description else {
        return diagnostic(
            "agent_missing_required_frontmatter",
            &format!("Agent frontmatter must include description: {path}"),
            path,
        );
    };

    let source = if scope == "built-in" {
        "built-in"
    } else {
        "user"
    };
    let mut agent = Map::new();
    agent.insert(
        "id".into(),
        Value::String(create_agent_state_id(&name, scope, source)),
    );
    agent.insert("name".into(), Value::String(name));
    agent.insert("description".into(), Value::String(description));
    agent.insert(
        "systemPrompt".into(),
        Value::String(body.trim().to_string()),
    );
    if let Some(color) = normalize_enum(frontmatter.get("color"), &VALID_COLORS) {
        agent.insert("color".into(), Value::String(color.to_string()));
    }
    if let Some(selection) = parse_subagent_markdown_selection(&frontmatter) {
        agent.insert("modelSelection".into(), selection.to_json());
    }
    if let Some(tools) = parse_tool_spec_list(frontmatter.get("tools")) {
        agent.insert("tools".into(), tools);
    }
    if let Some(tools) = parse_tool_spec_list(frontmatter.get("disallowedTools")) {
        agent.insert("disallowedTools".into(), tools);
    }
    if let Some(skills) = parse_string_list(frontmatter.get("skills")) {
        agent.insert("skills".into(), skills);
    }
    if let Some(mode) = normalize_enum(frontmatter.get("permissionMode"), &VALID_PERMISSION_MODES) {
        agent.insert("permissionMode".into(), Value::String(mode.to_string()));
    }
    if let Some(max_turns) = normalize_positive_integer(frontmatter.get("maxTurns")) {
        agent.insert("maxTurns".into(), Value::Number(max_turns.into()));
    }
    if let Some(background) = parse_optional_boolean(frontmatter.get("background")) {
        agent.insert("background".into(), Value::Bool(background));
    }
    if let Some(inject) = parse_optional_boolean(frontmatter.get("injectAgentsMd")) {
        agent.insert("injectAgentsMd".into(), Value::Bool(inject));
    }
    if let Some(servers) = frontmatter.get("mcpServers").and_then(Value::as_array) {
        agent.insert("mcpServers".into(), Value::Array(servers.clone()));
    }
    agent.insert("path".into(), Value::String(path.to_string()));
    agent.insert("scope".into(), Value::String(scope.to_string()));
    agent.insert("source".into(), Value::String(source.to_string()));
    agent.insert("enabled".into(), Value::Bool(true));
    // Only built-in agents are truly non-editable; plugin agents are explicitly overridden to read-only by discoverPluginAgents.
    agent.insert("readOnly".into(), Value::Bool(scope == "built-in"));

    ParsedMarkdown {
        agent: Some(Value::Object(agent)),
        diagnostic: None,
    }
}

fn diagnostic(code: &str, message: &str, path: &str) -> ParsedMarkdown {
    ParsedMarkdown {
        agent: None,
        diagnostic: Some(serde_json::json!({
            "code": code,
            "message": message,
            "path": path,
        })),
    }
}

/// splitMarkdownFrontmatter: frontmatter wrapped in --- plus the body; returns None when there is no frontmatter.
fn split_markdown_frontmatter(content: &str) -> Option<(String, String)> {
    let normalized = content.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return None;
    }
    let end_index = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.trim() == "---")
        .map(|(i, _)| i)?;
    let frontmatter = lines[1..end_index].join("\n");
    let body = lines[end_index + 1..].join("\n");
    Some((frontmatter, body))
}

/// parseRuntimeCompatibleFrontmatter: YAML values plus loose values (loose overrides), with mcpServers taken from the YAML array.
fn parse_runtime_compatible_frontmatter(frontmatter: &str) -> Map<String, Value> {
    let yaml_values = yaml_lite::parse(frontmatter)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let loose_values = parse_loose_frontmatter(frontmatter);
    let mut merged = yaml_values.clone();
    for (key, value) in loose_values {
        merged.insert(key, value);
    }
    if let Some(servers) = yaml_values.get("mcpServers").and_then(Value::as_array) {
        merged.insert("mcpServers".into(), Value::Array(servers.clone()));
    }
    merged
}

/// parseLooseFrontmatter: `key: value` scalars plus arrays of `- item` following `key:`; `#` comments are skipped.
fn parse_loose_frontmatter(frontmatter: &str) -> Map<String, Value> {
    let mut values = Map::new();
    let mut pending_list_key: Option<String> = None;
    for raw_line in frontmatter.split('\n') {
        let line = raw_line.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // List item: "- item" (attached to the preceding key).
        if let Some(item) = trimmed.strip_prefix("- ") {
            if let Some(key) = pending_list_key.clone() {
                let existing = values
                    .get(&key)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut list = existing;
                list.push(parse_scalar_value(item));
                values.insert(key, Value::Array(list));
                continue;
            }
        }
        pending_list_key = None;
        if let Some(colon) = trimmed.find(':') {
            let key = trimmed[..colon].trim();
            let raw_value = trimmed[colon + 1..].trim();
            if !is_plain_key(key) {
                continue;
            }
            if raw_value.is_empty() {
                values.insert(key.to_string(), Value::Array(Vec::new()));
                pending_list_key = Some(key.to_string());
                continue;
            }
            values.insert(key.to_string(), parse_scalar_value(raw_value));
        }
    }
    values
}

fn is_plain_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_scalar_value(raw_value: &str) -> Value {
    let value = strip_inline_comment(raw_value.trim());
    match value.as_str() {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return value
            .parse::<u64>()
            .map(|number| Value::Number(number.into()))
            .unwrap_or(Value::String(value));
    }
    if value.starts_with('[') && value.ends_with(']') {
        let inner = &value[1..value.len() - 1];
        let items = split_top_level_list(inner)
            .into_iter()
            .map(|item| Value::String(unquote_scalar(&strip_inline_comment(item.trim()))))
            .collect();
        return Value::Array(items);
    }
    if value.starts_with('{') && value.ends_with('}') {
        if let Ok(parsed) = serde_json::from_str::<Value>(&value) {
            return parsed;
        }
        // An invalid inline mapping keeps the original string; the downstream strict schema rejects it instead of letting the whole agent file disappear.
        return Value::String(value);
    }
    Value::String(unquote_scalar(&value))
}

fn strip_inline_comment(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut quote: Option<u8> = None;
    for (index, &byte) in bytes.iter().enumerate() {
        let char = byte as char;
        if (char == '"' || char == '\'') && index > 0 && bytes[index - 1] != b'\\' {
            quote = if quote == Some(byte) {
                None
            } else {
                quote.or(Some(byte))
            };
        }
        if quote.is_none() && char == '#' && index > 0 && (bytes[index - 1] as char).is_whitespace()
        {
            return value[..index].trim_end().to_string();
        }
    }
    value.to_string()
}

fn split_top_level_list(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut paren_depth = 0usize;
    for char in value.chars() {
        if (char == '"' || char == '\'') && quote.is_none() {
            quote = Some(char);
            current.push(char);
            continue;
        }
        if quote == Some(char) {
            quote = None;
            current.push(char);
            continue;
        }
        if quote.is_none() && char == '(' {
            paren_depth += 1;
        }
        if quote.is_none() && char == ')' {
            paren_depth = paren_depth.saturating_sub(1);
        }
        if quote.is_none() && paren_depth == 0 && char == ',' {
            if !current.trim().is_empty() {
                items.push(current.trim().to_string());
            }
            current = String::new();
            continue;
        }
        current.push(char);
    }
    if !current.trim().is_empty() {
        items.push(current.trim().to_string());
    }
    items
}

fn unquote_scalar(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn normalize_enum<'a>(value: Option<&Value>, valid: &[&'a str]) -> Option<&'a str> {
    let raw = scalar_string(value)?;
    valid.iter().copied().find(|candidate| *candidate == raw)
}

fn parse_optional_boolean(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(flag)) => Some(*flag),
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn normalize_positive_integer(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(number)) => {
            let int = number.as_u64()?;
            (int > 0).then_some(int)
        }
        Some(Value::String(text)) => {
            let text = text.trim();
            if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
                let parsed: u64 = text.parse().ok()?;
                (parsed > 0).then_some(parsed)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_string_list(value: Option<&Value>) -> Option<Value> {
    match value {
        Some(Value::Array(items)) => non_empty_list(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| Value::String(item.trim().to_string()))
                .collect(),
        ),
        Some(Value::String(text)) => non_empty_list(
            text.split(|c: char| c == ',' || c.is_whitespace())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| Value::String(item.to_string()))
                .collect(),
        ),
        _ => None,
    }
}

fn parse_tool_spec_list(value: Option<&Value>) -> Option<Value> {
    match value {
        Some(Value::Array(items)) => non_empty_list(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| Value::String(item.trim().to_string()))
                .collect(),
        ),
        Some(Value::String(text)) => {
            let mut result: Vec<Value> = Vec::new();
            let mut current = String::new();
            let mut paren_depth = 0usize;
            for char in text.chars() {
                if char == '(' {
                    paren_depth += 1;
                }
                if char == ')' {
                    paren_depth = paren_depth.saturating_sub(1);
                }
                if paren_depth == 0 && (char == ',' || char.is_whitespace()) {
                    if !current.trim().is_empty() {
                        result.push(Value::String(current.trim().to_string()));
                    }
                    current = String::new();
                    continue;
                }
                current.push(char);
            }
            if !current.trim().is_empty() {
                result.push(Value::String(current.trim().to_string()));
            }
            non_empty_list(result)
        }
        _ => None,
    }
}

fn non_empty_list(values: Vec<Value>) -> Option<Value> {
    let filtered: Vec<Value> = values
        .into_iter()
        .filter(|value| value.as_str().is_none_or(|s| !s.is_empty()))
        .collect();
    (!filtered.is_empty()).then_some(Value::Array(filtered))
}

/// serializeSubagentMarkdown: AgentConfig → Markdown (frontmatter + body).
/// A "*" and any pattern containing ": " must be quoted, otherwise the saved file cannot be read back by the parser.
pub fn serialize_subagent_markdown(config: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    let name = config.get("name").and_then(Value::as_str).unwrap_or("");
    let description = config
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    lines.push(format!("name: \"{}\"", escape_yaml_string(name)));
    lines.push(format!(
        "description: \"{}\"",
        escape_yaml_string(description)
    ));
    if let Some(color) = config.get("color").and_then(Value::as_str) {
        lines.push(format!("color: {}", format_yaml_scalar(color)));
    }
    if let Some(selection) = config.get("modelSelection") {
        if selection.get("providerId").is_some() && selection.get("modelId").is_some() {
            let formatted = format_model_selection(selection);
            lines.push(format!("model: {}", format_yaml_scalar(&formatted)));
            if let Some(level) = selection
                .get("options")
                .and_then(|o| o.get("reasoningLevel"))
                .and_then(Value::as_str)
            {
                lines.push(format!("thoughtLevel: {}", format_yaml_scalar(level)));
            }
        }
    }
    append_list(&mut lines, "tools", config.get("tools"));
    append_list(&mut lines, "disallowedTools", config.get("disallowedTools"));
    append_list(&mut lines, "skills", config.get("skills"));
    if let Some(mode) = config.get("permissionMode").and_then(Value::as_str) {
        lines.push(format!("permissionMode: {}", format_yaml_scalar(mode)));
    }
    if let Some(max_turns) = config.get("maxTurns").and_then(Value::as_u64) {
        lines.push(format!("maxTurns: {max_turns}"));
    }
    if let Some(background) = config.get("background").and_then(Value::as_bool) {
        lines.push(format!("background: {background}"));
    }
    if let Some(inject) = config.get("injectAgentsMd").and_then(Value::as_bool) {
        lines.push(format!("injectAgentsMd: {inject}"));
    }
    if let Some(servers) = config.get("mcpServers").and_then(Value::as_array) {
        if !servers.is_empty() && servers.iter().all(Value::is_string) {
            append_list(&mut lines, "mcpServers", config.get("mcpServers"));
        }
    }
    let body = config
        .get("systemPrompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let formatted_body = if body.is_empty() {
        "\n".to_string()
    } else {
        format!("\n{body}\n")
    };
    format!("---\n{}\n---\n{}", lines.join("\n"), formatted_body)
}

fn format_model_selection(selection: &Value) -> String {
    let provider = selection
        .get("providerId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let model = selection
        .get("modelId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    format_subagent_markdown_model(&crate::model_selection::ModelSelection {
        provider_id: provider,
        model_id: model,
        reasoning_level: None,
    })
}

fn append_list(lines: &mut Vec<String>, key: &str, values: Option<&Value>) {
    let Some(Value::Array(items)) = values else {
        return;
    };
    if items.is_empty() {
        return;
    }
    lines.push(format!("{key}:"));
    for item in items {
        if let Some(text) = item.as_str() {
            lines.push(format!("  - {}", format_yaml_scalar(text)));
        }
    }
}

fn format_yaml_scalar(value: &str) -> String {
    if is_safe_plain_yaml_scalar(value) {
        value.to_string()
    } else {
        format!("\"{}\"", escape_yaml_string(value))
    }
}

fn is_safe_plain_yaml_scalar(value: &str) -> bool {
    if value.is_empty() || value.trim() != value {
        return false;
    }
    let lowered = value.to_ascii_lowercase();
    if matches!(lowered.as_str(), "false" | "null" | "true" | "~") {
        return false;
    }
    let first = value.chars().next().unwrap_or('a');
    if matches!(
        first,
        '-' | '+'
            | '*'
            | '?'
            | ':'
            | ','
            | '['
            | ']'
            | '{'
            | '}'
            | '&'
            | '!'
            | '|'
            | '>'
            | '\''
            | '"'
            | '%'
            | '@'
            | BACKTICK
    ) {
        return false;
    }
    if value.contains('#') || value.contains(": ") {
        return false;
    }
    value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '.' | '/' | '@' | '*' | ' ' | '(' | ')' | '-')
    })
}

fn escape_yaml_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_agent_markdown_with_frontmatter_and_body() {
        let content = "---\nname: \"my-agent\"\ndescription: \"A helper\"\ncolor: blue\ntools:\n  - Bash\n  - Read\nmaxTurns: 5\n---\nYou are helpful.\n";
        let parsed = parse_subagent_markdown(content, "/agents/my-agent.md", "user");
        let agent = parsed.agent.expect("agent");
        assert_eq!(agent["id"], json!("user:user:my-agent"));
        assert_eq!(agent["name"], json!("my-agent"));
        assert_eq!(agent["description"], json!("A helper"));
        assert_eq!(agent["systemPrompt"], json!("You are helpful."));
        assert_eq!(agent["color"], json!("blue"));
        assert_eq!(agent["tools"], json!(["Bash", "Read"]));
        assert_eq!(agent["maxTurns"], json!(5));
        assert_eq!(agent["scope"], json!("user"));
        assert_eq!(agent["readOnly"], json!(false));
    }

    #[test]
    fn builtin_scope_is_read_only_and_missing_description_diagnoses() {
        let built_in = parse_subagent_markdown(
            "---\nname: Explore\ndescription: d\n---\nbody",
            "/x.md",
            "built-in",
        );
        assert_eq!(built_in.agent.expect("agent")["readOnly"], json!(true));
        let missing = parse_subagent_markdown("---\nname: only-name\n---\nbody", "/x.md", "user");
        assert!(missing.agent.is_none());
        assert_eq!(
            missing.diagnostic.expect("diagnostic")["code"],
            json!("agent_missing_required_frontmatter")
        );
    }

    #[test]
    fn agent_state_ids_are_stable() {
        assert_eq!(
            create_agent_state_id(" My-Agent ", "user", "user"),
            "user:user:my-agent"
        );
        assert_eq!(
            create_plugin_agent_state_id("ext@zcode", "My-Agent"),
            "plugin:ext@zcode:my-agent"
        );
    }

    #[test]
    fn serialize_round_trips_through_parser() {
        let config = json!({
            "name": "my-agent",
            "description": "A helper",
            "systemPrompt": "You are helpful.",
            "color": "blue",
            "tools": ["Bash", "Read"],
            "maxTurns": 5,
        });
        let content = serialize_subagent_markdown(&config);
        let parsed = parse_subagent_markdown(&content, "/a.md", "user");
        let agent = parsed.agent.expect("round-trip agent");
        assert_eq!(agent["name"], json!("my-agent"));
        assert_eq!(agent["description"], json!("A helper"));
        assert_eq!(agent["systemPrompt"], json!("You are helpful."));
        assert_eq!(agent["tools"], json!(["Bash", "Read"]));
        assert_eq!(agent["maxTurns"], json!(5));
    }
}
