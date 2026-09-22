//! jsdiff `patch/create.js` 的移植：结构化补丁构建（`structuredPatch`）
//! 与 unified 文本渲染（`formatPatch`），含 Git 风格文件名 C 引号转义。
//!
//! 关键怪癖一并镜像（详见 crate README）：
//! - 哨兵片段 `{value:"", lines:[]}`：显式空行数组是真值，不走 `split_lines`；
//! - `oldRangeStart` 用真值判断（0 = 未开启区间），因此 `oldStart` 可为 ≤0；
//! - 行号计数用 `i64`，渲染阶段 `oldLines==0` 时起点减一（unified 格式怪癖）。

use crate::diff::{diff_pieces, Piece};
use crate::tokenize::split_lines;

/// unified 补丁的一个 hunk（契约字段与 `@zcode/contracts` DiffHunk 一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub old_start: i64,
    pub old_lines: i64,
    pub new_start: i64,
    pub new_lines: i64,
    pub lines: Vec<String>,
}

impl Hunk {
    /// 序列化为契约 DiffHunk 形状：`{oldStart,oldLines,newStart,newLines,lines}`。
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

/// 结构化补丁（jsdiff `StructuredPatch`）；`None` 字段 = jsdiff 的 `undefined`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructuredPatch {
    pub old_file_name: Option<String>,
    pub new_file_name: Option<String>,
    pub old_header: Option<String>,
    pub new_header: Option<String>,
    pub hunks: Vec<Hunk>,
}

impl StructuredPatch {
    /// 按 `JSON.stringify` 语义序列化：undefined（`None`）键整体省略，
    /// 键序与 jsdiff 对象字面量一致（oldFileName → newFileName → oldHeader
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

/// 文件名是否需要 C 风格引号（控制字符 / 非可打印 ASCII / 引号 / 反斜杠）。
fn needs_quoting(name: &str) -> bool {
    name.chars()
        .any(|ch| !('\x20'..='\x7e').contains(&ch) || ch == '"' || ch == '\\')
}

/// C 风格引号文件名：非 ASCII 按 UTF-8 字节整体八进制转义
/// （镜像 jsdiff TextEncoder 路径，Git/GNU diffutils 同款行为）。
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

/// jsdiff `diffLinesResultToPatch` 第 1 步：把片段序列折叠为 hunks。
fn pieces_to_hunks(pieces: &[Piece], context: usize) -> Vec<Hunk> {
    // jsdiff 会 push 空值哨兵让收尾逻辑统一；其 `lines:[]` 是真值，不重算行。
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
                // 变更区间开启：带回上一片段尾部的 context。
                old_range_start = old_line;
                new_range_start = new_line;
                if index > 0 {
                    let prev = &work[index - 1];
                    let prev_lines = match &prev.lines {
                        Some(lines) => lines.clone(),
                        None => split_lines(&prev.value),
                    };
                    if context > 0 {
                        // JS slice(-context)：context 超长时取整段。
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
                    // 相邻变更间距 ≤ 2*context：合并进同一 hunk。
                    cur_range.extend(lines.iter().map(|line| format!(" {line}")));
                } else {
                    // 否则封口输出当前 hunk。
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

/// jsdiff `diffLinesResultToPatch` 第 2 步：剥行尾换行、补
/// `\ No newline at end of file` 标记。
fn strip_trailing_newlines(hunks: &mut [Hunk]) {
    for hunk in hunks {
        let mut index = 0;
        while index < hunk.lines.len() {
            if hunk.lines[index].ends_with('\n') {
                hunk.lines[index].pop();
            } else {
                // 无行尾换行：其后插入标记并跳过标记行（与 JS splice+i++ 一致）。
                hunk.lines
                    .insert(index + 1, "\\ No newline at end of file".into());
                index += 1;
            }
            index += 1;
        }
    }
}

/// `structuredPatch`：行差分 → 结构化补丁；`None` = 超时（jsdiff undefined）。
// 参数与 jsdiff structuredPatch 签名一一对应，8 个参数为有意保留。
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

/// `formatPatch`（非 Git、`INCLUDE_HEADERS`）：Index/下划线/文件头/hunks。
pub(crate) fn format_patch(patch: &StructuredPatch) -> String {
    let mut out: Vec<String> = Vec::new();
    // Index 行要求两侧文件名同值且已定义（jsdiff `==` 对字符串即等值）。
    if let (Some(old_name), Some(new_name)) = (&patch.old_file_name, &patch.new_file_name) {
        if old_name == new_name {
            out.push(format!("Index: {old_name}"));
        }
    }
    out.push("=".repeat(67));
    if let (Some(old_name), Some(new_name)) = (&patch.old_file_name, &patch.new_file_name) {
        // header 为真值（非空字符串）才追加 `\t` 后缀。
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
        // unified 格式怪癖：范围为 0 时起点比预期小 1（artima 引用的 quirk）。
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
        // 控制字符与保留字符按 C 转义。
        assert_eq!(quote_file_name_if_needed("a\tb"), "\"a\\tb\"");
        assert_eq!(quote_file_name_if_needed("a\"b\\c"), "\"a\\\"b\\\\c\"");
        // 非 ASCII 整体 UTF-8 八进制（中文 3 字节/字）。
        assert_eq!(
            quote_file_name_if_needed("中文 文件.txt"),
            "\"\\344\\270\\255\\346\\226\\207 \\346\\226\\207\\344\\273\\266.txt\""
        );
    }

    #[test]
    fn identical_inputs_yield_index_headers_without_hunks() {
        let built = patch("a\n", "a\n", Some("f.txt"), 4).expect("非超时");
        assert!(built.hunks.is_empty(), "相同输入没有 hunk");
        let text = format_patch(&built);
        assert!(text.starts_with("Index: f.txt\n"), "{text}");
        assert!(text.contains("--- f.txt\n+++ f.txt\n"), "{text}");
        assert!(!text.contains("@@"), "{text}");
        // 无文件名 → 只剩下划线，无 Index/文件头。
        let bare = patch("a\n", "a\n", None, 4).expect("非超时");
        let text = format_patch(&bare);
        assert_eq!(text, format!("{}\n", "=".repeat(67)), "{text}");
    }

    #[test]
    fn context_window_and_hunk_header_numbers() {
        // 前后各 3 行 context（context=3）→ jsdiff 实测 `@@ -2,6 +2,6 @@`。
        let old = "1\n2\n3\n4\n5\n6\n7\n";
        let new = "1\n2\n3\n4\nX\n6\n7\n";
        let built = patch(old, new, Some("f.txt"), 3).expect("非超时");
        assert_eq!(built.hunks.len(), 1);
        assert_eq!(built.hunks[0].old_start, 2);
        assert_eq!(built.hunks[0].old_lines, 6);
        let text = format_patch(&built);
        assert!(text.contains("@@ -2,6 +2,6 @@"), "{text}");
        assert!(text.contains("-5\n"), "{text}");
        assert!(text.contains("+X\n"), "{text}");

        // 默认 context=4：间距 ≤ 2*context 的两处变更合并进同一 hunk。
        let old = "a\nb\nc\nd\ne\nf\n";
        let new = "A\nb\nc\nd\ne\nF\n";
        let merged = patch(old, new, Some("f.txt"), 4).expect("非超时");
        assert_eq!(merged.hunks.len(), 1, "间距 ≤ 2*context 应合并: {merged:?}");
        assert_eq!(merged.hunks[0].old_start, 1, "文件首变更无前置 context");

        // 相距远 → 拆成两个 hunk。
        let old: String = (0..40).map(|i| format!("{i}\n")).collect();
        let mut lines: Vec<String> = (0..40).map(|i| i.to_string()).collect();
        lines[0] = "zero".into();
        lines[39] = "thirty-nine".into();
        let new: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let split = patch(&old, &new, Some("f.txt"), 4).expect("非超时");
        assert_eq!(
            split.hunks.len(),
            2,
            "远距变更必须拆分: {:?}",
            split.hunks.len()
        );
    }

    #[test]
    fn no_newline_at_eof_markers_and_start_quirk() {
        // 末行无换行 → 删除/新增侧都会带 `\ No newline at end of file`。
        let built = patch("a\nb", "a\nB", Some("f.txt"), 4).expect("非超时");
        let text = format_patch(&built);
        assert!(
            text.contains("-b\n\\ No newline at end of file\n"),
            "{text}"
        );
        assert!(
            text.contains("+B\n\\ No newline at end of file\n"),
            "{text}"
        );

        // 旧侧为空（oldLines=0）→ 渲染起点减一（`-0,0`）。
        let built = structured_patch(Some("f"), Some("f"), "", "x\n", None, None, 4, u128::MAX)
            .expect("非超时");
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
        .expect("非超时");
        let text = format_patch(&built);
        assert!(text.contains("--- f\tbefore\n"), "{text}");
        assert!(text.contains("+++ f\n"), "{text}");
        // 文件名含非 ASCII → `---`/`+++` 里八进制转义，Index 行保持原样。
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
        .expect("非超时");
        let text = format_patch(&built);
        assert!(text.starts_with("Index: 中文.txt\n"), "{text}");
        assert!(text.contains("--- \"\\344"), "{text}");
    }

    #[test]
    fn structured_json_omits_undefined_keys_and_keeps_contract_order() {
        let built =
            structured_patch(None, None, "", "x\n", None, None, 4, u128::MAX).expect("非超时");
        let text = built.to_json().to_string();
        // undefined 键省略：只剩 hunks（镜像 JSON.stringify）。
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
        .expect("非超时");
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
