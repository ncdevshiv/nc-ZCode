//! Lightweight Markdown frontmatter reader — port of
//! `adapters/src/plugins/markdown-frontmatter.ts`.
//!
//! Only `name` / `description` are consumed (component names and descriptions in
//! `plugins/describe`), including YAML block scalars (`>` folded / `|` literal with
//! optional chomping) so a multi-line description is not replaced by the `>` marker.

use std::collections::HashMap;
use std::path::Path;

pub fn read_plugin_markdown_frontmatter(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    parse_markdown_frontmatter(&text)
}

fn parse_markdown_frontmatter(content: &str) -> (Option<String>, Option<String>) {
    let Some(frontmatter) = extract_frontmatter(content) else {
        return (None, None);
    };
    let values = parse_flat_yaml_scalars(&frontmatter);
    (
        values.get("name").and_then(|value| parse_scalar(value)),
        values
            .get("description")
            .and_then(|value| parse_scalar(value)),
    )
}

fn extract_frontmatter(content: &str) -> Option<String> {
    let normalized = content.strip_prefix('\u{feff}').unwrap_or(content);
    if !normalized.starts_with("---") {
        return None;
    }
    let lines: Vec<&str> = normalized.lines().collect();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return None;
    }
    let end_index = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.trim() == "---")
        .map(|(index, _)| index)?;
    if end_index == 0 {
        return None;
    }
    Some(lines[1..end_index].join("\n"))
}

fn parse_flat_yaml_scalars(frontmatter: &str) -> HashMap<String, String> {
    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut values: HashMap<String, String> = HashMap::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            index += 1;
            continue;
        }
        // Indented lines belong to a preceding block scalar.
        if line.starts_with(' ') || line.starts_with('\t') {
            index += 1;
            continue;
        }
        let Some(separator) = line.find(':') else {
            index += 1;
            continue;
        };
        if separator == 0 {
            index += 1;
            continue;
        }
        let key = line[..separator].trim().to_string();
        let value = line[separator + 1..].trim().to_string();
        if values.contains_key(&key) {
            index += 1;
            continue;
        }
        if let Some(style) = parse_block_scalar_style(&value) {
            let (block, next_index) = read_block_scalar(&lines, index + 1, style);
            values.insert(key, block);
            index = next_index;
        } else {
            values.insert(key, value);
            index += 1;
        }
    }
    values
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockStyle {
    Folded,
    Literal,
}

fn parse_block_scalar_style(value: &str) -> Option<BlockStyle> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let marker = bytes[0];
    if marker != b'>' && marker != b'|' {
        return None;
    }
    let suffix = &value[1..];
    if !suffix.is_empty() && suffix != "+" && suffix != "-" {
        return None;
    }
    Some(if marker == b'>' {
        BlockStyle::Folded
    } else {
        BlockStyle::Literal
    })
}

fn read_block_scalar(lines: &[&str], start_index: usize, style: BlockStyle) -> (String, usize) {
    let mut raw_lines: Vec<&str> = Vec::new();
    let mut index = start_index;
    while index < lines.len() {
        let line = lines[index];
        if !line.trim().is_empty() && !line.starts_with(' ') && !line.starts_with('\t') {
            break;
        }
        raw_lines.push(line);
        index += 1;
    }
    let indent = raw_lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min();
    let content_lines: Vec<String> = raw_lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                line.chars().skip(indent.unwrap_or(0)).collect()
            }
        })
        .collect();
    let value = match style {
        BlockStyle::Folded => fold_block_scalar_lines(&content_lines),
        BlockStyle::Literal => content_lines.join("\n").trim().to_string(),
    };
    (value, index)
}

fn fold_block_scalar_lines(lines: &[String]) -> String {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join(" "));
                current.clear();
            }
            continue;
        }
        current.push(trimmed.to_string());
    }
    if !current.is_empty() {
        paragraphs.push(current.join(" "));
    }
    paragraphs.join("\n").trim().to_string()
}

fn parse_scalar(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            let inner = &trimmed[1..trimmed.len() - 1];
            let inner = inner.trim();
            return (!inner.is_empty()).then(|| inner.to_string());
        }
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_scalars_are_folded_or_literal() {
        let content = "---\nname: demo\ndescription: >\n  first line\n  second line\n\n  next para\n---\nbody";
        let (name, description) = parse_markdown_frontmatter(content);
        assert_eq!(name.as_deref(), Some("demo"));
        assert_eq!(
            description.as_deref(),
            Some("first line second line\nnext para")
        );
    }

    #[test]
    fn literal_block_keeps_newlines() {
        let content = "---\ndescription: |\n  line one\n  line two\n---\n";
        let (_, description) = parse_markdown_frontmatter(content);
        assert_eq!(description.as_deref(), Some("line one\nline two"));
    }

    #[test]
    fn quoted_scalars_are_unwrapped_and_missing_frontmatter_is_empty() {
        let (name, description) = parse_markdown_frontmatter("---\nname: \"quoted\"\n---\n");
        assert_eq!(name.as_deref(), Some("quoted"));
        assert!(description.is_none());
        let (name, description) = parse_markdown_frontmatter("no frontmatter");
        assert!(name.is_none() && description.is_none());
    }
}
