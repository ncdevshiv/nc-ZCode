//! A deliberately small YAML reader for frontmatter blocks.
//!
//! The TypeScript `skillsService.ts` and `subagentMarkdown.ts` call `parse()` from the `yaml`
//! package. No YAML crate is vendored in `native/Cargo.lock` and this workspace builds offline,
//! so the subset that frontmatter actually uses is implemented here instead of adding a
//! dependency. What is supported:
//!
//! - block mappings, nested by indentation;
//! - block sequences (`- item`), including sequences of mappings (`- key: value` with
//!   indented continuation lines);
//! - plain scalars (single-line and multi-line, folded with a space, blank lines become `\n`);
//! - single- and double-quoted scalars with escapes, spanning lines;
//! - literal (`|`) and folded (`>`) block scalars with `-`/`+` chomping;
//! - flow collections on one line (`[a, b]`, `{a: b}`);
//! - comments (`#` at line start or after whitespace) and `null`/`~`, `true`/`false`, integers.
//!
//! Anything else (anchors `&x`, aliases `*x`, explicit tags, multiple documents, complex `?`
//! keys) is reported as unsupported rather than guessed at. Both callers already have a
//! documented fallback for a YAML failure — `readFrontmatter` in skills and
//! `parseRuntimeCompatibleFrontmatter` in subagents fall back to their line-oriented loose
//! reader — so an unsupported construct degrades to the same behaviour the TypeScript code has
//! for invalid YAML, instead of silently producing different data.

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct YamlError(pub String);

impl YamlError {
    fn unsupported(construct: &str) -> Self {
        YamlError(format!("unsupported YAML construct: {construct}"))
    }
}

/// Parse a YAML document into a JSON value. Only object/array/scalar results are produced;
/// anything requiring a construct outside the supported subset is an error.
pub fn parse(text: &str) -> Result<Value, YamlError> {
    let lines = scan_lines(text);
    if lines.iter().all(|line| line.content.is_empty()) {
        return Ok(Value::Null);
    }
    for line in &lines {
        if let Some(marker) = unsupported_marker(&line.content) {
            return Err(YamlError::unsupported(marker));
        }
    }
    let mut index = 0usize;
    let value = parse_node(
        &lines,
        &mut index,
        lines
            .iter()
            .find(|line| !line.content.is_empty())
            .map(|line| line.indent)
            .unwrap_or(0),
    )?;
    if index < lines.len() && !lines[index..].iter().all(|line| line.content.is_empty()) {
        return Err(YamlError(
            "trailing content after the YAML document root".to_string(),
        ));
    }
    Ok(value)
}

fn unsupported_marker(content: &str) -> Option<&'static str> {
    let trimmed = content.trim();
    if trimmed.starts_with("--- ") || trimmed == "..." {
        return Some("multiple documents");
    }
    if trimmed.starts_with("? ") {
        return Some("complex key");
    }
    if let Some(rest) = trimmed.strip_prefix(':') {
        let _ = rest;
    }
    // `key: &anchor` / `key: *alias` / `key: !!tag`
    if let Some((_, value)) = split_key_and_value(trimmed) {
        let value = value.trim();
        if value.starts_with('&') || value.starts_with('*') || value.starts_with("!!") {
            return Some(if value.starts_with('&') {
                "anchor"
            } else if value.starts_with('*') {
                "alias"
            } else {
                "tag"
            });
        }
    }
    None
}

struct Line {
    indent: usize,
    /// Content after the indentation with trailing comments removed; empty for blank lines.
    content: String,
    /// Raw line including block-scalar indentation, used by the block-scalar reader.
    raw: String,
}

fn scan_lines(text: &str) -> Vec<Line> {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|raw| {
            let indent = raw.chars().take_while(|ch| *ch == ' ').count();
            let body = &raw[indent..];
            let content = strip_comment(body).trim_end().to_string();
            Line {
                indent,
                content,
                raw: raw.to_string(),
            }
        })
        .collect()
}

/// Remove a trailing `#` comment: a `#` at the start of the token or preceded by whitespace,
/// outside of quotes.
fn strip_comment(body: &str) -> String {
    let bytes: Vec<char> = body.chars().collect();
    let mut quote: Option<char> = None;
    let mut out = String::new();
    for (index, ch) in bytes.iter().enumerate() {
        match quote {
            None => {
                if *ch == '"' || *ch == '\'' {
                    quote = Some(*ch);
                } else if *ch == '#' && (index == 0 || bytes[index - 1].is_whitespace()) {
                    break;
                }
                out.push(*ch);
            }
            Some(marker) => {
                out.push(*ch);
                if *ch == marker {
                    quote = None;
                }
            }
        }
    }
    out
}

fn parse_node(lines: &[Line], index: &mut usize, indent: usize) -> Result<Value, YamlError> {
    while *index < lines.len() && lines[*index].content.is_empty() {
        *index += 1;
    }
    if *index >= lines.len() {
        return Ok(Value::Null);
    }
    let line = &lines[*index];
    if line.indent < indent {
        return Ok(Value::Null);
    }
    if line.content == "-" || line.content.starts_with("- ") {
        parse_sequence(lines, index, line.indent)
    } else {
        Ok(Value::Object(parse_mapping(lines, index, line.indent)?))
    }
}

fn parse_sequence(lines: &[Line], index: &mut usize, indent: usize) -> Result<Value, YamlError> {
    let mut items = Vec::new();
    while *index < lines.len() {
        let line = &lines[*index];
        if line.content.is_empty() {
            *index += 1;
            continue;
        }
        if line.indent < indent {
            break;
        }
        if line.indent > indent || !(line.content == "-" || line.content.starts_with("- ")) {
            return Err(YamlError(format!(
                "expected a block sequence entry at line {}",
                *index + 1
            )));
        }
        let rest = if line.content == "-" {
            String::new()
        } else {
            line.content[2..].trim().to_string()
        };
        *index += 1;
        if rest.is_empty() {
            // Nested block on the following, deeper-indented lines.
            let next_indent = lines
                .iter()
                .skip(*index)
                .find(|candidate| !candidate.content.is_empty())
                .map(|candidate| candidate.indent)
                .unwrap_or(0);
            if next_indent > indent {
                items.push(parse_node(lines, index, next_indent)?);
            } else {
                items.push(Value::Null);
            }
            continue;
        }
        if let Some(object) = split_key_and_value(&rest) {
            // `- key: value` starts a mapping whose remaining keys are indented to the dash.
            let mut map = Map::new();
            let (key, value_text) = object;
            let value_indent = indent + 2;
            if value_text.trim().is_empty() {
                let next_indent = lines
                    .iter()
                    .skip(*index)
                    .find(|candidate| !candidate.content.is_empty())
                    .map(|candidate| candidate.indent)
                    .unwrap_or(0);
                if next_indent > value_indent {
                    map.insert(key, parse_node(lines, index, next_indent)?);
                } else {
                    map.insert(key, Value::Null);
                }
            } else {
                map.insert(
                    key,
                    parse_value_text(lines, index, value_text.trim(), value_indent)?,
                );
            }
            // Continuation keys of the same mapping.
            while *index < lines.len() {
                let candidate = &lines[*index];
                if candidate.content.is_empty() {
                    *index += 1;
                    continue;
                }
                if candidate.indent >= value_indent && candidate.indent > indent {
                    if let Some((next_key, next_value)) = split_key_and_value(&candidate.content) {
                        *index += 1;
                        if next_value.trim().is_empty() {
                            let nested_indent = lines
                                .iter()
                                .skip(*index)
                                .find(|inner| !inner.content.is_empty())
                                .map(|inner| inner.indent)
                                .unwrap_or(0);
                            if nested_indent > value_indent {
                                map.insert(next_key, parse_node(lines, index, nested_indent)?);
                            } else {
                                map.insert(next_key, Value::Null);
                            }
                        } else {
                            map.insert(
                                next_key,
                                parse_value_text(
                                    lines,
                                    index,
                                    next_value.trim(),
                                    candidate.indent,
                                )?,
                            );
                        }
                        continue;
                    }
                }
                break;
            }
            items.push(Value::Object(map));
            continue;
        }
        items.push(parse_value_text(lines, index, &rest, indent)?);
    }
    Ok(Value::Array(items))
}

fn parse_mapping(
    lines: &[Line],
    index: &mut usize,
    indent: usize,
) -> Result<Map<String, Value>, YamlError> {
    let mut map = Map::new();
    while *index < lines.len() {
        let line = &lines[*index];
        if line.content.is_empty() {
            *index += 1;
            continue;
        }
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(YamlError(format!(
                "unexpected indentation at line {}",
                *index + 1
            )));
        }
        let Some((key, value_text)) = split_key_and_value(&line.content) else {
            break;
        };
        *index += 1;
        if value_text.trim().is_empty() {
            let next = lines
                .iter()
                .skip(*index)
                .find(|candidate| !candidate.content.is_empty());
            match next {
                Some(candidate) if candidate.indent > indent => {
                    map.insert(key, parse_node(lines, index, candidate.indent)?);
                }
                Some(candidate)
                    if candidate.indent == indent
                        && split_key_and_value(&candidate.content).is_none() =>
                {
                    // Multi-line plain scalar continuation of an empty inline value.
                    let text = collect_plain_continuation(lines, index, indent);
                    map.insert(key, scalar_or_folded(&text));
                }
                _ => {
                    map.insert(key, Value::Null);
                }
            }
            continue;
        }
        map.insert(
            key,
            parse_value_text(lines, index, value_text.trim(), indent)?,
        );
    }
    Ok(map)
}

fn parse_value_text(
    lines: &[Line],
    index: &mut usize,
    text: &str,
    indent: usize,
) -> Result<Value, YamlError> {
    if text.starts_with('|') || text.starts_with('>') {
        return parse_block_scalar(lines, index, text, indent);
    }
    if text.starts_with('[') || text.starts_with('{') {
        return parse_flow(text);
    }
    if text.starts_with('"') || text.starts_with('\'') {
        let marker = text.as_bytes()[0] as char;
        if quoted_is_closed(text, marker) {
            return Ok(Value::String(unquote(text, marker)));
        }
        // Multi-line quoted scalar: fold the continuation lines into one string.
        let mut joined = text.to_string();
        while *index < lines.len() {
            let next = lines[*index].content.clone();
            *index += 1;
            joined.push(' ');
            joined.push_str(&next);
            if quoted_is_closed(&joined, marker) {
                return Ok(Value::String(unquote(&joined, marker)));
            }
        }
        return Err(YamlError("unterminated quoted scalar".to_string()));
    }
    let mut single = text.to_string();
    // A plain scalar keeps folding following, more-indented non-key lines.
    let mut continuation: Vec<String> = Vec::new();
    while *index < lines.len() {
        let candidate = &lines[*index];
        if candidate.content.is_empty() {
            // A blank line only continues the scalar if more indented text follows.
            let mut probe = *index;
            while probe < lines.len() && lines[probe].content.is_empty() {
                probe += 1;
            }
            if probe >= lines.len() || lines[probe].indent <= indent {
                break;
            }
            continuation.push(String::new());
            *index = probe;
            continue;
        }
        if candidate.indent <= indent {
            break;
        }
        if split_key_and_value(&candidate.content).is_some() || candidate.content.starts_with("- ")
        {
            break;
        }
        continuation.push(candidate.content.clone());
        *index += 1;
    }
    if continuation.is_empty() {
        return Ok(scalar_or_folded(&single));
    }
    let folded = fold_plain_lines(&continuation);
    if !single.is_empty() && !folded.is_empty() && !single.ends_with(' ') && !single.ends_with('\n')
    {
        // TS plain-scalar folding: insert a single space between continuation lines (otherwise the two segments run together).
        single.push(' ');
    }
    single.push_str(&folded);
    Ok(scalar_or_folded(&single))
}

fn collect_plain_continuation(lines: &[Line], index: &mut usize, indent: usize) -> String {
    let mut collected: Vec<String> = Vec::new();
    while *index < lines.len() {
        let candidate = &lines[*index];
        if candidate.content.is_empty() {
            let mut probe = *index;
            while probe < lines.len() && lines[probe].content.is_empty() {
                probe += 1;
            }
            if probe >= lines.len() || lines[probe].indent <= indent {
                break;
            }
            collected.push(String::new());
            *index = probe;
            continue;
        }
        if candidate.indent <= indent
            || split_key_and_value(&candidate.content).is_some()
            || candidate.content.starts_with("- ")
        {
            break;
        }
        collected.push(candidate.content.clone());
        *index += 1;
    }
    fold_plain_lines(&collected).trim().to_string()
}

fn fold_plain_lines(lines: &[String]) -> String {
    let mut out = String::new();
    for (position, line) in lines.iter().enumerate() {
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        if position > 0 && !out.ends_with('\n') && !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line.trim());
    }
    out
}

fn parse_block_scalar(
    lines: &[Line],
    index: &mut usize,
    header: &str,
    indent: usize,
) -> Result<Value, YamlError> {
    let folded = header.starts_with('>');
    let mut chomp = 'c'; // clip
    let mut explicit_indent: Option<usize> = None;
    for ch in header[1..].chars() {
        match ch {
            '-' | '+' => chomp = ch,
            digit if digit.is_ascii_digit() => {
                explicit_indent = Some(indent + digit.to_digit(10).unwrap_or(1) as usize)
            }
            _ => return Err(YamlError(format!("invalid block scalar header: {header}"))),
        }
    }
    let mut body: Vec<String> = Vec::new();
    let mut block_indent = explicit_indent.unwrap_or(usize::MAX);
    while *index < lines.len() {
        let raw = &lines[*index].raw;
        if raw.trim().is_empty() {
            body.push(String::new());
            *index += 1;
            continue;
        }
        let line_indent = raw.chars().take_while(|ch| *ch == ' ').count();
        if line_indent <= indent {
            break;
        }
        if block_indent == usize::MAX {
            block_indent = line_indent;
        }
        if line_indent < block_indent {
            break;
        }
        body.push(raw[block_indent..].to_string());
        *index += 1;
    }
    while body.last().map(|line| line.is_empty()).unwrap_or(false) {
        body.pop();
        if chomp == '-' {
            body.push(String::new());
            break;
        }
    }
    let text = if folded {
        let mut out = String::new();
        for (position, line) in body.iter().enumerate() {
            if position > 0 && !out.is_empty() {
                if line.is_empty() {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                } else if !out.ends_with('\n') {
                    out.push(' ');
                }
                // When out already ends with \n (the previous blank line emitted the newline), do not add another separator,
                // otherwise the single blank line inside a folded scalar would be output as two newlines.
            }
            out.push_str(line.trim_end_matches(' '));
        }
        out
    } else {
        body.join("\n")
    };
    let mut text = text;
    match chomp {
        '-' => text = text.trim_end_matches('\n').to_string(),
        '+' => {
            if !text.is_empty() {
                text.push('\n');
            }
        }
        _ => {
            if !text.is_empty() {
                text.push('\n');
            }
        }
    }
    Ok(Value::String(text))
}

fn parse_flow(text: &str) -> Result<Value, YamlError> {
    let chars: Vec<char> = text.chars().collect();
    let mut position = 0usize;
    let value = parse_flow_item(&chars, &mut position)?;
    while position < chars.len() && chars[position].is_whitespace() {
        position += 1;
    }
    if position != chars.len() {
        return Err(YamlError(format!("trailing content in flow value: {text}")));
    }
    Ok(value)
}

fn parse_flow_item(chars: &[char], position: &mut usize) -> Result<Value, YamlError> {
    while *position < chars.len() && chars[*position].is_whitespace() {
        *position += 1;
    }
    let Some(first) = chars.get(*position).copied() else {
        return Ok(Value::Null);
    };
    match first {
        '[' => {
            *position += 1;
            let mut items = Vec::new();
            loop {
                skip_flow_space(chars, position);
                match chars.get(*position).copied() {
                    Some(']') => {
                        *position += 1;
                        break;
                    }
                    Some(',') => *position += 1,
                    None => return Err(YamlError("unterminated flow sequence".to_string())),
                    _ => {}
                }
                items.push(parse_flow_item(chars, position)?);
            }
            Ok(Value::Array(items))
        }
        '{' => {
            *position += 1;
            let mut map = Map::new();
            loop {
                skip_flow_space(chars, position);
                match chars.get(*position).copied() {
                    Some('}') => {
                        *position += 1;
                        break;
                    }
                    Some(',') => *position += 1,
                    None => return Err(YamlError("unterminated flow mapping".to_string())),
                    _ => {}
                }
                let key = parse_flow_key(chars, position)?;
                skip_flow_space(chars, position);
                let value = if chars.get(*position) == Some(&':') {
                    *position += 1;
                    parse_flow_item(chars, position)?
                } else {
                    Value::Null
                };
                map.insert(key, value);
            }
            Ok(Value::Object(map))
        }
        '"' | '\'' => {
            let marker = first;
            *position += 1;
            let mut raw = String::new();
            let mut closed = false;
            while *position < chars.len() {
                let ch = chars[*position];
                if ch == marker {
                    if marker == '\'' && chars.get(*position + 1) == Some(&'\'') {
                        raw.push('\'');
                        *position += 2;
                        continue;
                    }
                    *position += 1;
                    closed = true;
                    break;
                }
                if marker == '"' && ch == '\\' {
                    if let Some(escaped) = chars.get(*position + 1) {
                        raw.push(escape_char(*escaped));
                        *position += 2;
                        continue;
                    }
                }
                raw.push(ch);
                *position += 1;
            }
            if !closed {
                return Err(YamlError("unterminated flow scalar".to_string()));
            }
            Ok(Value::String(raw))
        }
        _ => {
            let mut token = String::new();
            while let Some(ch) = chars.get(*position).copied() {
                if matches!(ch, ',' | ']' | '}' | ':') {
                    break;
                }
                token.push(ch);
                *position += 1;
            }
            Ok(scalar_or_folded(token.trim()))
        }
    }
}

fn parse_flow_key(chars: &[char], position: &mut usize) -> Result<String, YamlError> {
    match parse_flow_item(chars, position)? {
        Value::String(text) => Ok(text),
        Value::Null => Ok(String::new()),
        other => Err(YamlError(format!("unsupported flow key: {other}"))),
    }
}

fn skip_flow_space(chars: &[char], position: &mut usize) {
    while *position < chars.len() && chars[*position].is_whitespace() {
        *position += 1;
    }
}

fn escape_char(ch: char) -> char {
    match ch {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        other => other,
    }
}

fn quoted_is_closed(text: &str, marker: char) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 1usize;
    while index < chars.len() {
        if chars[index] == marker {
            if marker == '\'' && chars.get(index + 1) == Some(&'\'') {
                index += 2;
                continue;
            }
            if marker == '"'
                && chars.get(index - 1) == Some(&'\\')
                && !is_double_escaped(&chars, index - 1)
            {
                index += 1;
                continue;
            }
            return true;
        }
        index += 1;
    }
    false
}

fn is_double_escaped(chars: &[char], backslash: usize) -> bool {
    let mut count = 0usize;
    let mut index = backslash + 1;
    while index < chars.len() {
        index += 1;
    }
    let mut cursor = backslash;
    while cursor > 0 && chars[cursor - 1] == '\\' {
        count += 1;
        cursor -= 1;
    }
    count % 2 == 1
}

fn unquote(text: &str, marker: char) -> String {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix(marker)
        .and_then(|value| value.strip_suffix(marker))
        .unwrap_or(trimmed);
    if marker == '\'' {
        return inner.replace("''", "'");
    }
    let chars: Vec<char> = inner.chars().collect();
    let mut out = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '\\' && index + 1 < chars.len() {
            out.push(escape_char(chars[index + 1]));
            index += 2;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// `key: value` split at the first top-level colon. Returns `None` when the line is not a
/// mapping entry.
fn split_key_and_value(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("- ") || trimmed == "-" {
        return None;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    for (index, ch) in chars.iter().enumerate() {
        match quote {
            None => {
                if *ch == '"' || *ch == '\'' {
                    quote = Some(*ch);
                } else if matches!(*ch, '[' | '{') {
                    depth += 1;
                } else if matches!(*ch, ']' | '}') {
                    depth = depth.saturating_sub(1);
                } else if *ch == ':' && depth == 0 {
                    let after = chars.get(index + 1);
                    if after
                        .map(|next| next.is_whitespace() || *next == ',')
                        .unwrap_or(true)
                    {
                        let key = chars[..index].iter().collect::<String>();
                        let value = chars[index + 1..].iter().collect::<String>();
                        return Some((unquote_key(key.trim()), value));
                    }
                }
            }
            Some(marker) => {
                if *ch == marker {
                    quote = None;
                }
            }
        }
    }
    None
}

fn unquote_key(key: &str) -> String {
    let bytes = key.as_bytes();
    if bytes.len() >= 2
        && ((key.starts_with('"') && key.ends_with('"'))
            || (key.starts_with('\'') && key.ends_with('\'')))
    {
        unquote(key, bytes[0] as char)
    } else {
        key.to_string()
    }
}

fn scalar_or_folded(text: &str) -> Value {
    let value = text.trim();
    if value.is_empty() {
        return Value::Null;
    }
    if value.starts_with('"') || value.starts_with('\'') {
        let marker = value.as_bytes()[0] as char;
        if quoted_is_closed(value, marker) {
            return Value::String(unquote(value, marker));
        }
    }
    match value {
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        "null" | "Null" | "NULL" | "~" => return Value::Null,
        _ => {}
    }
    if value.eq_ignore_ascii_case("nan") || value.eq_ignore_ascii_case(".nan") {
        return Value::Null;
    }
    if let Ok(integer) = value.parse::<i64>() {
        return Value::from(integer);
    }
    if let Ok(float) = value.parse::<f64>() {
        if float.is_finite() {
            return Value::from(float);
        }
    }
    Value::String(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_block_mappings_and_sequences() {
        let parsed =
            parse("name: demo\ndescription: hello\nallowed-tools:\n  - Read\n  - Write\n").unwrap();
        assert_eq!(
            parsed,
            json!({"name": "demo", "description": "hello", "allowed-tools": ["Read", "Write"]})
        );
    }

    #[test]
    fn folds_multiline_plain_scalars() {
        let parsed = parse("description: first line\n  second line\nname: x").unwrap();
        assert_eq!(parsed["description"], json!("first line second line"));
        assert_eq!(parsed["name"], json!("x"));
    }

    #[test]
    fn reads_block_scalars_with_chomping() {
        let literal = parse("text: |\n  one\n  two\nname: x").unwrap();
        assert_eq!(literal["text"], json!("one\ntwo\n"));
        let stripped = parse("text: |-\n  one\n  two\nname: x").unwrap();
        assert_eq!(stripped["text"], json!("one\ntwo"));
        let folded = parse("text: >\n  one\n  two\n\n  three\nname: x").unwrap();
        assert_eq!(folded["text"], json!("one two\nthree\n"));
    }

    #[test]
    fn reads_quoted_and_flow_values_and_comments() {
        let parsed = parse("name: \"a: b\" # trailing\ndescription: 'it''s here'\ntools: [Read, \"Write\"]\nflag: true\nempty:\n").unwrap();
        assert_eq!(parsed["name"], json!("a: b"));
        assert_eq!(parsed["description"], json!("it's here"));
        assert_eq!(parsed["tools"], json!(["Read", "Write"]));
        assert_eq!(parsed["flag"], json!(true));
        assert_eq!(parsed["empty"], Value::Null);
    }

    #[test]
    fn parses_sequences_of_mappings() {
        let parsed = parse(
            "mcpServers:\n  - name: demo\n    command: node\n    args:\n      - a\n      - b\n  - name: two\n    command: python\n",
        )
        .unwrap();
        assert_eq!(
            parsed["mcpServers"],
            json!([
                {"name": "demo", "command": "node", "args": ["a", "b"]},
                {"name": "two", "command": "python"}
            ])
        );
    }

    #[test]
    fn rejects_anchors_and_aliases_instead_of_guessing() {
        assert!(parse("name: &anchor\n").is_err() || parse("name: *anchor\n").is_err());
        let error = parse("ref: *other\n").unwrap_err();
        assert!(error.0.contains("alias"), "{error:?}");
    }

    #[test]
    fn values_keep_json_types() {
        let parsed = parse("count: 12\nratio: 1.5\ntext: keep as-is\n").unwrap();
        assert_eq!(parsed["count"], json!(12));
        assert_eq!(parsed["ratio"], json!(1.5));
        assert_eq!(parsed["text"], json!("keep as-is"));
    }
}
