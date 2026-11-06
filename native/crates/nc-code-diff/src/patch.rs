//! Porting of jsdiff `patch/create.js`: structured patch construction (`structuredPatch`)
//! With unified text rendering (`formatPatch`), with Git-style filename C quote escaping.
//!
//! Key quirks are mirrored together (see crate README for details):
//! - Sentinel fragment `{value:"", lines:[]}`: explicit empty line array is a true value, no `split_lines` is used;
//! - `oldRangeStart` uses truth value (0 = not open range), so `oldStart` can be ≤0;
//! - Line number counting uses `i64`, and the starting point is decremented by one when `oldLines==0` in the rendering phase (unified format quirk).

use crate::diff::{diff_pieces, Piece};
use crate::tokenize::split_lines;

/// A hunk for the unified patch (contract fields consistent with `@zcode/contracts` DiffHunk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub old_start: i64,
    pub old_lines: i64,
    pub new_start: i64,
    pub new_lines: i64,
    pub lines: Vec<String>,
}

impl Hunk {
    /// Serialize to contract DiffHunk shape: `{oldStart,oldLines,newStart,newLines,lines}`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "oldStart": self.old_start,
            "oldLines": self.old_lines,
            "newStart": self.new_start,
            "newLines": self.new_lines,
            "lines": self.lines,
        })
    }
}

/// StructuredPatch (jsdiff `StructuredPatch`); `None` field = `undefined` for jsdiff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructuredPatch {
    pub old_file_name: Option<String>,
    pub new_file_name: Option<String>,
    pub old_header: Option<String>,
    pub new_header: Option<String>,
    pub hunks: Vec<Hunk>,
}

impl StructuredPatch {
    /// Semantic serialization by `JSON.stringify`: undefined (`None`) keys are omitted entirely,
    /// The key sequence is consistent with the jsdiff object literal (oldFileName → newFileName → oldHeader
    /// → newHeader → hunks）。
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if let Some(name) = &self.old_file_name {
            map.insert("oldFileName".into(), serde_json::Value::from(name.as_str()));
        }
        if let Some(name) = &self.new_file_name {
            map.insert("newFileName".into(), serde_json::Value::from(name.as_str()));
        }
        if let Some(header) = &self.old_header {
            map.insert("oldHeader".into(), serde_json::Value::from(header.as_str()));
        }
        if let Some(header) = &self.new_header {
            map.insert("newHeader".into(), serde_json::Value::from(header.as_str()));
        }
        map.insert(
            "hunks".into(),
            serde_json::Value::Array(self.hunks.iter().map(Hunk::to_json).collect()),
        );
        serde_json::Value::Object(map)
    }
}

/// Whether the filename requires C-style quotes (control characters / non-printable ASCII / quotes / backslashes).
fn needs_quoting(name: &str) -> bool {
    name.chars()
        .any(|ch| !('\x20'..='\x7e').contains(&ch) || ch == '"' || ch == '\\')
}

/// C-style quoted filename: non-ASCII escaped in UTF-8 byte whole octal format
/// (Mirror jsdiff TextEncoder path, same behavior as Git/GNU diffutils).
pub(crate) fn quote_file_name_if_needed(name: &str) -> String {
    if !needs_quoting(name) {
        return name.to_string();
    }
    let mut quoted = String::from('"');
    for &byte in name.as_bytes() {
        match byte {
            0x07 => quoted.push_str("\\a"),
            0x08 => quoted.push_str("\\b"),
            0x09 => quoted.push_str("\\t"),
            0x0a => quoted.push_str("\\n"),
            0x0b => quoted.push_str("\\v"),
            0x0c => quoted.push_str("\\f"),
            0x0d => quoted.push_str("\\r"),
            0x22 => quoted.push_str("\\\""),
            0x5c => quoted.push_str("\\\\"),
            0x20..=0x7e => quoted.push(byte as char),
            _ => quoted.push_str(&format!("\\{:03o}", byte)),
        }
    }
    quoted.push('"');
    quoted
}

/// jsdiff `diffLinesResultToPatch` Step 1: Fold sequence of fragments into hunks.
fn pieces_to_hunks(pieces: &[Piece], context: usize) -> Vec<Hunk> {
    // jsdiff will push the null value sentinel to unify the closing logic; its `lines:[]` is a true value and does not recalculate the lines.
    let sentinel = Piece {
        count: 0,
        added: false,
        removed: false,
        value: String::new(),
        lines: Some(Vec::new()),
    };
    let mut work = pieces.to_vec();
    work.push(sentinel);

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut old_range_start: i64 = 0;
    let mut new_range_start: i64 = 0;
    let mut cur_range: Vec<String> = Vec::new();
    let mut old_line: i64 = 1;
    let mut new_line: i64 = 1;

    for (index, piece) in work.iter().enumerate() {
        let lines = match &piece.lines {
            Some(lines) => lines.clone(),
            None => split_lines(&piece.value),
        };
        if piece.added || piece.removed {
            if old_range_start == 0 {
                // Change interval on: bring back the context at the end of the previous fragment.
                old_range_start = old_line;
                new_range_start = new_line;
                if index > 0 {
                    let prev = &work[index - 1];
                    let prev_lines = match &prev.lines {
                        Some(lines) => lines.clone(),
                        None => split_lines(&prev.value),
                    };
                    if context > 0 {
                        // JS slice(-context): round off the whole segment when the context is too long.
                        let tail = &prev_lines[prev_lines.len().saturating_sub(context)..];
                        cur_range = tail.iter().map(|line| format!(" {line}")).collect();
                    } else {
                        cur_range = Vec::new();
                    }
                    old_range_start -= cur_range.len() as i64;
                    new_range_start -= cur_range.len() as i64;
                }
            }
            let prefix = if piece.added { '+' } else { '-' };
            cur_range.extend(lines.iter().map(|line| format!("{prefix}{line}")));
            if piece.added {
                new_line += lines.len() as i64;
            } else {
                old_line += lines.len() as i64;
            }
        } else {
            if old_range_start != 0 {
                if lines.len() <= context.saturating_mul(2) && index + 2 < work.len() {
                    // Adjacent change distance ≤ 2*context: merge into the same hunk.
                    cur_range.extend(lines.iter().map(|line| format!(" {line}")));
                } else {
                    // Otherwise, seal and output the current hunk.
                    let context_size = lines.len().min(context);
                    cur_range.extend(lines[..context_size].iter().map(|line| format!(" {line}")));
                    hunks.push(Hunk {
                        old_start: old_range_start,
                        old_lines: old_line - old_range_start + context_size as i64,
                        new_start: new_range_start,
                        new_lines: new_line - new_range_start + context_size as i64,
                        lines: std::mem::take(&mut cur_range),
                    });
                    old_range_start = 0;
                    new_range_start = 0;
                }
            }
            old_line += lines.len() as i64;
            new_line += lines.len() as i64;
        }
    }
    hunks
}

/// jsdiff `diffLinesResultToPatch` Step 2: Strip the end of the line, wrap it, and fill it
/// `\ No newline at end of file` tag.
fn strip_trailing_newlines(hunks: &mut [Hunk]) {
    for hunk in hunks {
        let mut index = 0;
        while index < hunk.lines.len() {
            if hunk.lines[index].ends_with('\n') {
                hunk.lines[index].pop();
            } else {
                // No end-of-line wrap: insert marker after and skip marker line (consistent with JS splice+i++).
                hunk.lines
                    .insert(index + 1, "\\ No newline at end of file".into());
                index += 1;
            }
            index += 1;
        }
    }
}

/// `structuredPatch`: row diff → structured patch; `None` = timeout (jsdiff undefined).
// The parameters correspond to the jsdiff structuredPatch signature one-to-one, and the 8 parameters are intentionally reserved.
#[allow(clippy::too_many_arguments)]
pub(crate) fn structured_patch(
    old_file_name: Option<&str>,
    new_file_name: Option<&str>,
    old: &str,
    new: &str,
    old_header: Option<&str>,
    new_header: Option<&str>,
    context: usize,
    timeout_ms: u128,
) -> Option<StructuredPatch> {
    let pieces = diff_pieces(old, new, timeout_ms)?;
    let mut hunks = pieces_to_hunks(&pieces, context);
    strip_trailing_newlines(&mut hunks);
    Some(StructuredPatch {
        old_file_name: old_file_name.map(str::to_string),
        new_file_name: new_file_name.map(str::to_string),
        old_header: old_header.map(str::to_string),
        new_header: new_header.map(str::to_string),
        hunks,
    })
}

/// `formatPatch` (non-Git, `INCLUDE_HEADERS`): Index/underscore/fileheaders/hunks.
pub(crate) fn format_patch(patch: &StructuredPatch) -> String {
    let mut out: Vec<String> = Vec::new();
    // The Index line requires that the file names on both sides have the same value and are defined (jsdiff `==` is equivalent to a string).
    if let (Some(old_name), Some(new_name)) = (&patch.old_file_name, &patch.new_file_name) {
        if old_name == new_name {
            out.push(format!("Index: {old_name}"));
        }
    }
    out.push("=".repeat(67));
    if let (Some(old_name), Some(new_name)) = (&patch.old_file_name, &patch.new_file_name) {
        // The `\t` suffix is ​​appended only when the header is a true value (non-empty string).
        let old_suffix = patch
            .old_header
            .as_deref()
            .filter(|header| !header.is_empty())
            .map(|header| format!("\t{header}"))
            .unwrap_or_default();
        let new_suffix = patch
            .new_header
            .as_deref()
            .filter(|header| !header.is_empty())
            .map(|header| format!("\t{header}"))
            .unwrap_or_default();
        out.push(format!(
            "--- {}{old_suffix}",
            quote_file_name_if_needed(old_name)
        ));
        out.push(format!(
            "+++ {}{new_suffix}",
            quote_file_name_if_needed(new_name)
        ));
    }
    for hunk in &patch.hunks {
        // unified format quirk: range 0 starts at 1 smaller than expected (a quirk cited by artima).
        let old_start = if hunk.old_lines == 0 {
            hunk.old_start - 1
        } else {
            hunk.old_start
        };
        let new_start = if hunk.new_lines == 0 {
            hunk.new_start - 1
        } else {
            hunk.new_start
        };
        out.push(format!(
            "@@ -{old_start},{} +{new_start},{} @@",
            hunk.old_lines, hunk.new_lines
        ));
        out.extend(hunk.lines.iter().cloned());
    }
    format!("{}\n", out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(
        old: &str,
        new: &str,
        old_name: Option<&str>,
        context: usize,
    ) -> Option<StructuredPatch> {
        structured_patch(old_name, old_name, old, new, None, None, context, u128::MAX)
    }

    #[test]
    fn quote_escapes_control_quotes_backslash_and_utf8_octals() {
        assert_eq!(quote_file_name_if_needed("plain.txt"), "plain.txt");
        assert_eq!(quote_file_name_if_needed(""), "");
        //Control characters and reserved characters are escaped as C.
        assert_eq!(quote_file_name_if_needed("a\tb"), "\"a\\tb\"");
        assert_eq!(quote_file_name_if_needed("a\"b\\c"), "\"a\\\"b\\\\c\"");
        // Non-ASCII whole UTF-8 octal (Chinese 3 bytes/word).
        assert_eq!(
            quote_file_name_if_needed("中文 文件.txt"),
            "\"\\344\\270\\255\\346\\226\\207 \\346\\226\\207\\344\\273\\266.txt\""
        );
    }

    #[test]
    fn identical_inputs_yield_index_headers_without_hunks() {
        let built = patch("a\n", "a\n", Some("f.txt"), 4).expect("non-timeout");
        assert!(built.hunks.is_empty(), "Same input without hunk");
        let text = format_patch(&built);
        assert!(text.starts_with("Index: f.txt\n"), "{text}");
        assert!(text.contains("--- f.txt\n+++ f.txt\n"), "{text}");
        assert!(!text.contains("@@"), "{text}");
        // No file name → only dashes, no Index/file header.
        let bare = patch("a\n", "a\n", None, 4).expect("non-timeout");
        let text = format_patch(&bare);
        assert_eq!(text, format!("{}\n", "=".repeat(67)), "{text}");
    }

    #[test]
    fn context_window_and_hunk_header_numbers() {
        // 3 lines before and after context (context=3) → jsdiff actual measurement `@@ -2,6 +2,6 @@`.
        let old = "1\n2\n3\n4\n5\n6\n7\n";
        let new = "1\n2\n3\n4\nX\n6\n7\n";
        let built = patch(old, new, Some("f.txt"), 3).expect("non-timeout");
        assert_eq!(built.hunks.len(), 1);
        assert_eq!(built.hunks[0].old_start, 2);
        assert_eq!(built.hunks[0].old_lines, 6);
        let text = format_patch(&built);
        assert!(text.contains("@@ -2,6 +2,6 @@"), "{text}");
        assert!(text.contains("-5\n"), "{text}");
        assert!(text.contains("+X\n"), "{text}");

        //Default context=4: Two changes with a spacing ≤ 2*context are merged into the same hunk.
        let old = "a\nb\nc\nd\ne\nf\n";
        let new = "A\nb\nc\nd\ne\nF\n";
        let merged = patch(old, new, Some("f.txt"), 4).expect("non-timeout");
        assert_eq!(
            merged.hunks.len(),
            1,
            "Spacing ≤ 2*context should be merged: {merged:?}"
        );
        assert_eq!(
            merged.hunks[0].old_start, 1,
            "The first change of the file has no preceding context"
        );

        // Far apart → Split into two hunks.
        let old: String = (0..40).map(|i| format!("{i}\n")).collect();
        let mut lines: Vec<String> = (0..40).map(|i| i.to_string()).collect();
        lines[0] = "zero".into();
        lines[39] = "thirty-nine".into();
        let new: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let split = patch(&old, &new, Some("f.txt"), 4).expect("non-timeout");
        assert_eq!(
            split.hunks.len(),
            2,
            "Remote changes must be split: {:?}",
            split.hunks.len()
        );
    }

    #[test]
    fn no_newline_at_eof_markers_and_start_quirk() {
        // No newline at the end of the line → The delete/add side will have `\ No newline at end of file`.
        let built = patch("a\nb", "a\nB", Some("f.txt"), 4).expect("non-timeout");
        let text = format_patch(&built);
        assert!(
            text.contains("-b\n\\ No newline at end of file\n"),
            "{text}"
        );
        assert!(
            text.contains("+B\n\\ No newline at end of file\n"),
            "{text}"
        );

        // The old side is empty (oldLines=0) → the rendering starting point is minus one (`-0,0`).
        let built = structured_patch(Some("f"), Some("f"), "", "x\n", None, None, 4, u128::MAX)
            .expect("non-timeout");
        assert_eq!(built.hunks.len(), 1);
        assert_eq!(built.hunks[0].old_lines, 0);
        assert_eq!(built.hunks[0].lines, vec!["+x".to_string()]);
        let text = format_patch(&built);
        assert!(text.contains("@@ -0,0 +1,1 @@"), "{text}");
    }

    #[test]
    fn headers_append_tab_suffix_only_when_non_empty() {
        let built = structured_patch(
            Some("f"),
            Some("f"),
            "a\n",
            "b\n",
            Some("before"),
            Some(""),
            4,
            u128::MAX,
        )
        .expect("non-timeout");
        let text = format_patch(&built);
        assert!(text.contains("--- f\tbefore\n"), "{text}");
        assert!(text.contains("+++ f\n"), "{text}");
        // File names containing non-ASCII → octal escapes in `---`/`+++`, and the Index line remains intact.
        let built = structured_patch(
            Some("中文.txt"),
            Some("中文.txt"),
            "a\n",
            "b\n",
            None,
            None,
            4,
            u128::MAX,
        )
        .expect("non-timeout");
        let text = format_patch(&built);
        assert!(text.starts_with("Index: 中文.txt\n"), "{text}");
        assert!(text.contains("--- \"\\344"), "{text}");
    }

    #[test]
    fn structured_json_omits_undefined_keys_and_keeps_contract_order() {
        let built =
            structured_patch(None, None, "", "x\n", None, None, 4, u128::MAX).expect("non-timeout");
        let text = built.to_json().to_string();
        // undefined keys omitted: only hunks remain (mirroring JSON.stringify).
        assert!(text.starts_with("{\"hunks\":[{\"oldStart\":"), "{text}");
        assert!(!text.contains("oldFileName"), "{text}");

        let built = structured_patch(
            Some("a.txt"),
            Some("b.txt"),
            "a\n",
            "b\n",
            Some("h1"),
            Some("h2"),
            4,
            u128::MAX,
        )
        .expect("non-timeout");
        let text = built.to_json().to_string();
        let head: String = text.chars().take(60).collect();
        assert!(
            head.starts_with("{\"oldFileName\":\"a.txt\",\"newFileName\":\"b.txt\""),
            "{text}"
        );
        assert!(
            text.contains("\"oldHeader\":\"h1\",\"newHeader\":\"h2\",\"hunks\":["),
            "{text}"
        );
    }

    #[test]
    fn timeout_maps_to_none_like_jsdiff_undefined() {
        let old: String = (0..5000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..5000).map(|i| format!("new {i}\n")).collect();
        assert!(structured_patch(Some("f"), Some("f"), &old, &new, None, None, 4, 0).is_none());
    }
}
