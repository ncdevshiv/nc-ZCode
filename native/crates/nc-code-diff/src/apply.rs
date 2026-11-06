//! jsdiff fuzz-0 accurate application port of `patch/apply.js` + `util/distance-iterator.js`.
//!
//! TS fixed for all callers `fuzzFactor: 0`, `autoConvertLineEndings: false`,
//! Therefore error correction branches (context substitution/insertion/deletion tolerance) are not reachable, only exact matches remain
//! Path and all failure conditions; automatic conversion of end-of-line newlines is not implemented (see crate README).
//!
//! `undefined` semantic mirror: JS `lines[i]` out of bounds is `undefined` (constant and unequal comparison,
//! `join` time-varying empty string), the Rust side uses `None` to compare and `""` to splice, corresponding point by point.

use crate::patch::Hunk;

/// jsdiff `DistanceIterator`: Staggered traversal by distance around start [min, max].
/// Example: start=2, [0,4] → 3, 1, 4, 0 in sequence (the initial start is tried by the caller first).
struct DistanceIterator {
    start: i64,
    min_line: i64,
    max_line: i64,
    want_forward: bool,
    backward_exhausted: bool,
    forward_exhausted: bool,
    local_offset: i64,
}

impl DistanceIterator {
    fn new(start: i64, min_line: i64, max_line: i64) -> Self {
        Self {
            start,
            min_line,
            max_line,
            want_forward: true,
            backward_exhausted: false,
            forward_exhausted: false,
            local_offset: 1,
        }
    }

    fn next(&mut self) -> Option<i64> {
        // JS tail recursion `return iterator()` uses loop reentrancy equivalent expansion.
        loop {
            if self.want_forward && !self.forward_exhausted {
                if self.backward_exhausted {
                    self.local_offset += 1;
                } else {
                    self.want_forward = false;
                }
                if self.start + self.local_offset <= self.max_line {
                    return Some(self.start + self.local_offset);
                }
                self.forward_exhausted = true;
            }
            if !self.backward_exhausted {
                if !self.forward_exhausted {
                    self.want_forward = true;
                }
                if self.min_line <= self.start - self.local_offset {
                    let hit = self.start - self.local_offset;
                    self.local_offset += 1;
                    return Some(hit);
                }
                self.backward_exhausted = true;
                continue;
            }
            return None;
        }
    }
}

/// Take the `i`th row; out-of-bounds/negative numbers are processed according to jsdiff `undefined` (comparison is not equal, splicing is empty).
fn line_or_empty<'a>(lines: &[&'a str], index: i64) -> &'a str {
    if index < 0 {
        return "";
    }
    lines.get(index as usize).copied().unwrap_or("")
}

/// Comparison semantics: jsdiff default `compareLine` = strict string equality; `undefined` not equal.
fn line_at<'a>(lines: &[&'a str], index: i64) -> Option<&'a str> {
    if index < 0 {
        return None;
    }
    lines.get(index as usize).copied()
}

/// Apply exactly one hunk at `to_pos` (fuzz 0: fail if any mismatch).
/// Return successfully (output line, ending position `oldLineLastI`).
fn apply_hunk<'a>(
    hunk_lines: &'a [String],
    lines: &[&'a str],
    to_pos: i64,
) -> Option<(Vec<&'a str>, i64)> {
    let mut patched: Vec<&'a str> = Vec::new();
    let mut pos = to_pos;
    // The number of consecutive context lines since the last edit: removed from both output and position at the end.
    let mut trailing_context: i64 = 0;
    // jsdiff's nextContextLineMustMatch/lastContextLineMatched only serves
    // Error correction branch for fuzz>0; under fuzz 0, the former is never triggered and the latter is always true.
    for hunk_line in hunk_lines {
        let operation = hunk_line.chars().next().unwrap_or(' ');
        // When the operation is ASCII ('-'/'+'/' '), the first character must be a single byte, and slicing is safe.
        let content = hunk_line.get(1..).unwrap_or("");
        match operation {
            '-' => {
                if line_at(lines, pos) == Some(content) {
                    pos += 1;
                    trailing_context = 0;
                } else {
                    // Deleted lines must exist exactly line by line (fuzz 0 has no fault-tolerant branch).
                    return None;
                }
            }
            '+' => {
                patched.push(content);
                trailing_context = 0;
            }
            ' ' => {
                trailing_context += 1;
                match line_at(lines, pos) {
                    Some(line) if line == content => {
                        patched.push(line);
                        pos += 1;
                    }
                    _ => return None,
                }
            }
            //Other prefixes: jsdiff If none of the three ifs are hit, the entire line is silently skipped.
            _ => {}
        }
    }
    // Consistent with jsdiff: remove the unmodified context at the end (no output, position fallback).
    let trimmed = (patched.len() as i64).saturating_sub(trailing_context) as usize;
    patched.truncate(trimmed);
    pos -= trailing_context;
    Some((patched, pos - 1))
}

/// EOFNL preprocessing: scan the `\ No newline at end of file` mark of the last hunk.
/// Returning `None` indicates a hard failure under fuzz 0 (the newline shape of the source file is inconsistent with the patch).
fn prepare_eofnl(source_lines: &mut Vec<&str>, last_hunk: &Hunk) -> Option<()> {
    let mut prev_line = "";
    let mut remove_eofnl = false;
    let mut add_eofnl = false;
    for line in &last_hunk.lines {
        if line.starts_with('\\') {
            if prev_line.starts_with('+') {
                remove_eofnl = true;
            } else if prev_line.starts_with('-') {
                add_eofnl = true;
            }
        }
        prev_line = line.as_str();
    }
    if remove_eofnl {
        if add_eofnl {
            // 末行两侧都无换行：fuzz 0 只校验源文件末行确实无换行。
            if source_lines.last().copied() == Some("") {
                return None;
            }
        } else if source_lines.last().copied() == Some("") {
            // 补丁要求去掉末尾换行。
            source_lines.pop();
        } else {
            // 源文件本就没有末尾换行可去 → 矛盾。
            return None;
        }
    } else if add_eofnl {
        if source_lines.last().copied() != Some("") {
            // 补丁要求补上末尾换行。
            source_lines.push("");
        } else {
            // 换行已存在 → 矛盾。
            return None;
        }
    }
    Some(())
}

/// 应用补丁：成功返回拼接后的文本，失败返回 `None`（jsdiff 的 false）。
pub(crate) fn apply_patch(source: &str, hunks: &[Hunk]) -> Option<String> {
    let mut lines: Vec<&str> = source.split('\n').collect();
    // 空补丁：原样返回（jsdiff 在 EOFNL 处理之前就短路）。
    if hunks.is_empty() {
        return Some(source.to_string());
    }
    prepare_eofnl(&mut lines, hunks.last().expect("非空 hunks"))?;

    let mut result: Vec<&str> = Vec::with_capacity(lines.len());
    let mut min_line: i64 = 0;
    let mut prev_hunk_offset: i64 = 0;
    for hunk in hunks {
        let max_line = lines.len() as i64 - hunk.old_lines;
        let mut pos = hunk.old_start + prev_hunk_offset - 1;
        let initial = pos;
        let mut iterator = DistanceIterator::new(initial, min_line, max_line);

        // 先试声明位置，再按距离交错扫描（jsdiff 的 for 循环形态）。
        let mut placed = None;
        loop {
            if let Some(patched) = apply_hunk(&hunk.lines, &lines, pos) {
                placed = Some((patched, pos));
                break;
            }
            match iterator.next() {
                Some(next) => pos = next,
                None => break,
            }
        }
        let ((patched, old_line_last_i), to_pos) = placed?;

        // 复制上一个 hunk 结束到本 hunk 起点之间的原始行。
        for index in min_line..to_pos {
            result.push(line_or_empty(&lines, index));
        }
        result.extend(patched);
        // 后续 hunk 不得覆盖已打补丁区域，并按实际偏移校正声明行号。
        min_line = old_line_last_i + 1;
        prev_hunk_offset = to_pos + 1 - hunk.old_start;
    }
    for index in min_line..lines.len() as i64 {
        result.push(line_or_empty(&lines, index));
    }
    Some(result.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::structured_patch;

    /// 用真实 structuredPatch 产出 hunk（与 parity 同源）。
    fn hunks(old: &str, new: &str) -> Vec<Hunk> {
        structured_patch(Some("f"), Some("f"), old, new, None, None, 4, u128::MAX)
            .expect("非超时")
            .hunks
    }

    #[test]
    fn distance_iterator_matches_jsdiff_order() {
        // jsdiff 注释例：[0,4]、start=2 → 调用方先试 2，迭代器依次 3, 1, 4, 0。
        let mut iterator = DistanceIterator::new(2, 0, 4);
        assert_eq!(
            (0..4).map(|_| iterator.next()).collect::<Vec<_>>(),
            vec![Some(3), Some(1), Some(4), Some(0)]
        );
        assert_eq!(iterator.next(), None, "两侧穷尽后返回 undefined");
        // 负起点（hunk old_start=0 → pos=-1 的开头插入场景）：先试 -1，
        // 反向越界耗尽后转为单向前进，直到超出 max 才终止（node 实测 jsdiff 序列）。
        let mut iterator = DistanceIterator::new(-2, 0, 3);
        assert_eq!(
            (0..5).map(|_| iterator.next()).collect::<Vec<_>>(),
            vec![Some(-1), Some(0), Some(1), Some(2), Some(3)]
        );
        assert_eq!(iterator.next(), None, "越过 max 后返回 undefined");
    }

    #[test]
    fn replace_ok_and_drift_position_scan() {
        let hunks = hunks("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(
            apply_patch("a\nb\nc\n", &hunks).expect("可应用"),
            "a\nB\nc\n",
            "声明位置精确命中"
        );
        // 源文件首部多一行 → 声明位置偏移，DistanceIterator 扫描命中下一行。
        assert_eq!(
            apply_patch("z\na\nb\nc\n", &hunks).expect("可扫描应用"),
            "z\na\nB\nc\n",
            "向后偏移 1 行仍可套用"
        );
    }

    #[test]
    fn drift_fails_when_context_never_matches() {
        let hunks = hunks("a\nb\nc\n", "a\nB\nc\n");
        assert!(
            apply_patch("z\ny\nq\n", &hunks).is_none(),
            "None of the contexts match → jsdiff false"
        );
        //Context partially drifted but delete line missing → failed.
        assert!(
            apply_patch("a\nother\nc\n", &hunks).is_none(),
            "Delete target row is missing"
        );
    }

    #[test]
    fn empty_hunks_return_source_unchanged() {
        let hunks = hunks("a\n", "a\n");
        assert!(hunks.is_empty(), "Same input without hunk");
        assert_eq!(
            apply_patch("raw\ncontent", &hunks).expect("empty patch"),
            "raw\ncontent"
        );
        assert_eq!(apply_patch("", &hunks).expect("empty patch"), "");
    }

    #[test]
    fn eofnl_add_remove_and_contradiction_paths() {
        // Add a newline at the end: the source has no newline → add a new line and then apply.
        let add = hunks("a", "a\n");
        assert_eq!(
            add[0].lines,
            vec!["-a", "\\ No newline at end of file", "+a"]
        );
        assert_eq!(apply_patch("a", &add).expect("Applicable"), "a\n");
        // The source already has a newline but it is required to be added → Contradiction failure (fuzz 0).
        assert!(
            apply_patch("a\n", &add).is_none(),
            "Already have a newline → false"
        );

        //Remove newline at the end.
        let remove = hunks("a\n", "a");
        assert_eq!(
            remove[0].lines,
            vec!["-a", "+a", "\\ No newline at end of file"]
        );
        assert_eq!(apply_patch("a\n", &remove).expect("Applicable"), "a");
        // The source does not have newlines but requires removal → contradictory failure.
        assert!(
            apply_patch("a", &remove).is_none(),
            "There is no newline → false"
        );

        //The last line is rewritten without line breaks on both sides: only the last line of the source is verified without line breaks.
        let both = hunks("a\nb", "a\nB");
        assert_eq!(
            both[0].lines,
            vec![
                " a",
                "-b",
                "\\ No newline at end of file",
                "+B",
                "\\ No newline at end of file",
            ]
        );
        assert_eq!(apply_patch("a\nb", &both).expect("Applicable"), "a\nB");
        assert!(
            apply_patch("a\nb\n", &both).is_none(),
            "There is a line break in the last line of the source (the last element is an empty string) → false"
        );
    }

    #[test]
    fn insert_at_start_and_delete_all_and_crlf() {
        let insert = hunks("b\n", "a\nb\n");
        assert_eq!(apply_patch("b\n", &insert).expect("Applicable"), "a\nb\n");

        let delete = hunks("a\nb\n", "");
        assert_eq!(apply_patch("a\nb\n", &delete).expect("Can delete all"), "");

        // CRLF: `\r` belongs to the line content, and the patch line must be compared with `\r` before comparison.
        let crlf = hunks("a\r\nb\r\n", "a\r\nB\r\n");
        assert_eq!(crlf[0].lines, vec![" a\r", "-b\r", "+B\r"]);
        assert_eq!(
            apply_patch("a\r\nb\r\n", &crlf).expect("Applicable"),
            "a\r\nB\r\n",
            "CRLF source must be left intact"
        );
        // LF source to CRLF patch → lines not equal → fails (fuzz 0 no end-of-line conversion).
        assert!(
            apply_patch("a\nb\n", &crlf).is_none(),
            "None autoConvertLineEndings"
        );
    }

    #[test]
    fn later_hunks_do_not_overlap_earlier_regions() {
        // Two remote changes in the same patch: the second hunk should be applied at the offset-corrected position.
        let old: String = (0..20).map(|i| format!("{i}\n")).collect();
        let mut lines: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        lines[1] = "one-changed".into();
        lines[18] = "eighteen-changed".into();
        let new: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let hunks = hunks(&old, &new);
        assert_eq!(hunks.len(), 2, "Two ranged changes: {hunks:?}");
        let applied = apply_patch(&old, &hunks).expect("Double hunk available");
        assert_eq!(
            applied, new,
            "The result of the application must equal the target text"
        );
    }
}
