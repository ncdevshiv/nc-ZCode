//! Markdown command files — the port of
//! `packages/services/src/commands/commandFileParser.ts`.
//!
//! A command file is optional frontmatter plus the prompt body:
//!
//! ```markdown
//! ---
//! description: what it does
//! argument-hint: <file>
//! ---
//!
//! the prompt
//! ```
//!
//! Frontmatter reading is deliberately line-oriented (not YAML): it matches the TypeScript
//! parser's `key:`-plus-indented-continuation rules, joins continuations with a single space,
//! and lowercases the key so `Description:` also resolves.

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCommandFile {
    pub name: String,
    pub prompt: String,
    pub content: String,
    pub file_path: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// `disable-noninteractive: true` removes the command from the protocol directory
    /// (TS `listProtocolSlashCommands` filter).
    pub disable_non_interactive: bool,
}

impl ParsedCommandFile {
    /// `{ name, prompt, content, filePath, description?, argumentHint? }` — the field order of
    /// the TypeScript object literal, which decides the JSON key order on the wire.
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("name".to_string(), Value::String(self.name.clone()));
        object.insert("prompt".to_string(), Value::String(self.prompt.clone()));
        object.insert("content".to_string(), Value::String(self.content.clone()));
        object.insert(
            "filePath".to_string(),
            Value::String(self.file_path.clone()),
        );
        insert_optional(&mut object, "description", self.description.as_deref());
        insert_optional(&mut object, "argumentHint", self.argument_hint.as_deref());
        if self.disable_non_interactive {
            object.insert("disableNonInteractive".to_string(), Value::Bool(true));
        }
        Value::Object(object)
    }
}

fn insert_optional(object: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        object.insert(key.to_string(), Value::String(value.to_string()));
    }
}

/// `CommandConfig` fields the writer may emit.
#[derive(Debug, Clone, Default)]
pub struct CommandConfig {
    pub name: String,
    pub prompt: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
}

pub fn parse_command_file(content: &str, file_path: &str) -> ParsedCommandFile {
    let (frontmatter_lines, content_lines) = split_markdown_command_content(content);
    let file_name = cross_platform_basename(file_path);
    let name = format!("/{}", strip_markdown_extension(&file_name));
    let joined = content_lines.join("\n");
    ParsedCommandFile {
        description: read_frontmatter_multiline_value(&frontmatter_lines, "description"),
        argument_hint: read_frontmatter_multiline_value(&frontmatter_lines, "argument-hint"),
        disable_non_interactive: read_frontmatter_multiline_value(
            &frontmatter_lines,
            "disable-noninteractive",
        )
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            normalized == "true" || normalized == "1" || normalized == "yes"
        })
        .unwrap_or(false),
        name,
        prompt: joined.trim().to_string(),
        content: joined,
        file_path: file_path.to_string(),
    }
}

/// `generateMarkdownCommandFileContent`. Keys replaced by this writer drop their old value and
/// its continuation lines; every other frontmatter line is preserved verbatim.
pub fn generate_command_file_content(
    config: &CommandConfig,
    existing_content: Option<&str>,
) -> String {
    let replaced = ["description", "argument-hint"];
    let mut frontmatter_lines = preserve_frontmatter_lines(existing_content, &replaced);
    if let Some(description) = config.description.as_deref().map(str::trim) {
        if !description.is_empty() {
            frontmatter_lines.push(format!("description: {description}"));
        }
    }
    if let Some(argument_hint) = config.argument_hint.as_deref().map(str::trim) {
        if !argument_hint.is_empty() {
            frontmatter_lines.push(format!("argument-hint: {argument_hint}"));
        }
    }
    if frontmatter_lines.is_empty() {
        return config.prompt.clone();
    }
    format!(
        "---\n{}\n---\n\n{}",
        frontmatter_lines.join("\n"),
        config.prompt
    )
}

fn split_markdown_command_content(content: &str) -> (Vec<String>, Vec<String>) {
    let lines: Vec<&str> = content.split('\n').collect();
    let start = lines.iter().position(|line| line.trim() == "---");
    let end = start.and_then(|start| {
        lines
            .iter()
            .enumerate()
            .find(|(index, line)| *index > start && line.trim() == "---")
            .map(|(index, _)| index)
    });
    let (Some(start), Some(end)) = (start, end) else {
        return (
            Vec::new(),
            lines.iter().map(|line| (*line).to_string()).collect(),
        );
    };
    let frontmatter = lines[start + 1..end]
        .iter()
        .map(|line| (*line).to_string())
        .collect();
    let body = lines[end + 1..]
        .iter()
        .map(|line| (*line).to_string())
        .collect();
    (frontmatter, body)
}

fn read_frontmatter_key(line: &str) -> Option<String> {
    if line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    let trimmed = line.trim();
    let colon = trimmed.find(':')?;
    let key = &trimmed[..colon];
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    // `^([A-Za-z0-9_-]+)\s*:` — only whitespace may sit between the key and the colon.
    if !trimmed[colon..].starts_with(':') || trimmed[..colon].chars().any(char::is_whitespace) {
        return None;
    }
    Some(key.to_lowercase())
}

fn read_frontmatter_multiline_value(lines: &[String], target_key: &str) -> Option<String> {
    let mut collected: Vec<String> = Vec::new();
    let mut collecting = false;
    for line in lines {
        if let Some(key) = read_frontmatter_key(line) {
            if collecting {
                break;
            }
            if key == target_key {
                collecting = true;
                let value = split_on_colons_and_rejoin(line).trim();
                if !value.is_empty() {
                    collected.push(value.to_string());
                }
            }
            continue;
        }
        if collecting && (line.starts_with(' ') || line.starts_with('\t')) {
            collected.push(line.trim().to_string());
        }
    }
    let value = collected.join(" ").trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// `line.split(":").slice(1).join(":")`
fn split_on_colons_and_rejoin(line: &str) -> &str {
    match line.find(':') {
        Some(index) => &line[index + 1..],
        None => "",
    }
}

fn preserve_frontmatter_lines(
    existing_content: Option<&str>,
    replaced_keys: &[&str],
) -> Vec<String> {
    let mut preserved = Vec::new();
    let mut skip_until_next_key = false;
    let (frontmatter_lines, _) = split_markdown_command_content(existing_content.unwrap_or(""));
    for line in frontmatter_lines {
        if let Some(key) = read_frontmatter_key(&line) {
            skip_until_next_key = replaced_keys.contains(&key.as_str());
        }
        if !skip_until_next_key {
            preserved.push(line);
        }
    }
    preserved
}

/// `getCrossPlatformBasename`: `filePath.split(/[\\/]+/).pop() ?? ""`. Runs of separators count
/// as one, and a trailing separator still yields an empty final segment, exactly as in JS.
fn cross_platform_basename(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    let mut rest = path;
    loop {
        match rest.find(['\\', '/']) {
            None => {
                segments.push(rest);
                break;
            }
            Some(index) => {
                segments.push(&rest[..index]);
                let bytes = rest.as_bytes();
                let mut end = index;
                while end < bytes.len() && matches!(bytes[end], b'\\' | b'/') {
                    end += 1;
                }
                rest = &rest[end..];
                if rest.is_empty() {
                    segments.push("");
                    break;
                }
            }
        }
    }
    segments.pop().unwrap_or_default().to_string()
}

/// `relativePath.replace(/\.md$/i, "")` — the last three bytes are ASCII `.md`, so cutting
/// three bytes from the end is always a `char` boundary. Also used by `getCommandName`, which
/// strips the command file extension from the relative path.
pub fn strip_markdown_extension(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 3 && bytes[bytes.len() - 3..].eq_ignore_ascii_case(b".md") {
        return value[..value.len() - 3].to_string();
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_body_and_multiline_values() {
        let content = "---\nname: keep\ndescription: first\n  second\nargument-hint: <a:b>\n---\n\nprompt body\n";
        let parsed = parse_command_file(content, "/root/dir/say hi.md");
        assert_eq!(parsed.name, "/say hi");
        assert_eq!(parsed.description.as_deref(), Some("first second"));
        assert_eq!(parsed.argument_hint.as_deref(), Some("<a:b>"));
        assert_eq!(parsed.prompt, "prompt body");
        assert_eq!(parsed.content, "\nprompt body\n");
    }

    #[test]
    fn no_frontmatter_means_the_whole_file_is_the_prompt() {
        let parsed = parse_command_file("plain text", "/x/y.md");
        assert_eq!(parsed.prompt, "plain text");
        assert!(parsed.description.is_none());
        assert!(parsed.argument_hint.is_none());
    }

    #[test]
    fn generated_content_replaces_only_its_own_keys() {
        let existing = "---\ndescription: stale\n  continuation\nuser-key: kept\n---\nold body";
        let generated = generate_command_file_content(
            &CommandConfig {
                name: "/x".to_string(),
                prompt: "new body".to_string(),
                description: Some("fresh".to_string()),
                argument_hint: None,
            },
            Some(existing),
        );
        assert_eq!(
            generated,
            "---\nuser-key: kept\ndescription: fresh\n---\n\nnew body"
        );
    }

    #[test]
    fn generated_content_without_frontmatter_is_the_prompt() {
        let generated = generate_command_file_content(
            &CommandConfig {
                name: "/x".to_string(),
                prompt: "body".to_string(),
                description: Some("   ".to_string()),
                argument_hint: None,
            },
            None,
        );
        assert_eq!(generated, "body");
    }

    #[test]
    fn keys_are_matched_case_insensitively_and_indented_lines_are_children() {
        let content = "---\nDescription: hello\n  world\nnested: ignored\n---\nbody";
        let parsed = parse_command_file(content, "/a/b.md");
        assert_eq!(parsed.description.as_deref(), Some("hello world"));
    }
}
