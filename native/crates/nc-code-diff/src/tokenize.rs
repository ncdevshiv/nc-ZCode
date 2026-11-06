//! Line splitting tools: mirror jsdiff `diff/line.js#tokenize` and `patch/create.js#splitLines`.
//!
//! The token of line-level diff is "line with end-of-line break", and CRLF's `\r` is part of the line content.
//! (jsdiff only uses `\n` / `\r\n` as separators, and merges the separators into the previous token).

/// Press `\n` / `\r\n` to split and merge the separated characters into the previous line (`newlineIsToken=false` semantics),
/// Press jsdiff `removeEmpty` again to remove the empty token.
pub(crate) fn tokenize(value: &str) -> Vec<String> {
    let bytes = value.as_bytes();
    // Alternate output content / separator / content ... (index parity determines ownership).
    let mut parts: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            // `\r\n` is used as a delimiter as a whole; `\r` only belongs to this delimiter if it immediately follows the current content.
            let sep_start = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            parts.push(&value[start..sep_start]);
            parts.push(&value[sep_start..i + 1]);
            start = i + 1;
        }
        i += 1;
    }
    parts.push(&value[start..]);
    // jsdiff: Discard the empty token at the end (the empty string split when the string ends with a newline).
    if parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }

    let mut tokens: Vec<String> = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if index % 2 == 1 {
            if let Some(last) = tokens.last_mut() {
                last.push_str(part);
            }
        } else {
            tokens.push((*part).to_string());
        }
    }
    tokens.retain(|token| !token.is_empty());
    tokens
}

/// Used for patch construction: split by `\n`, and each line except the last line of the file retains the newline at the end of the line.
/// (Mirror jsdiff `splitLines`: strip off the last newly added `\n` when there is no trailing newline).
pub(crate) fn split_lines(text: &str) -> Vec<String> {
    let has_trailing = text.ends_with('\n');
    let mut lines: Vec<String> = text.split('\n').map(|line| format!("{line}\n")).collect();
    if has_trailing {
        lines.pop();
    } else if let Some(mut last) = lines.pop() {
        // Remove the newline at the end of the line just added, and retain the original "no newline at the end of the line" form.
        last.pop();
        lines.push(last);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_merges_separators_and_drops_trailing_empty() {
        assert_eq!(tokenize("a\nb\n"), vec!["a\n", "b\n"]);
        assert_eq!(tokenize("a\nb"), vec!["a\n", "b"]);
        assert_eq!(tokenize(""), Vec::<String>::new());
        assert_eq!(tokenize("\n"), vec!["\n"]);
        // CRLF: `\r` falls into delimiters and merges forward.
        assert_eq!(tokenize("a\r\nb"), vec!["a\r\n", "b"]);
        // Isolated `\r` is not a delimiter and remains within the line content.
        assert_eq!(tokenize("a\rb\n"), vec!["a\rb\n"]);
        //Continuous empty lines: there is no empty token after the delimiter is merged.
        assert_eq!(tokenize("a\n\n\nb"), vec!["a\n", "\n", "\n", "b"]);
        assert_eq!(tokenize("\r\n\r\n"), vec!["\r\n", "\r\n"]);
    }

    #[test]
    fn split_lines_keeps_newlines_except_bare_tail() {
        assert_eq!(split_lines("a\nb\n"), vec!["a\n", "b\n"]);
        assert_eq!(split_lines("a\nb"), vec!["a\n", "b"]);
        assert_eq!(split_lines(""), vec![""]);
        assert_eq!(split_lines("\n"), vec!["\n"]);
        // CRLF line content with `\r`.
        assert_eq!(split_lines("a\r\nb\r\n"), vec!["a\r\n", "b\r\n"]);
    }
}
