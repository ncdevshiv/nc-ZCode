//! jsdiff `patch/apply.js` 的 fuzz-0 精确应用移植 + `util/distance-iterator.js`。
//!
//! TS 全部调用方固定 `fuzzFactor: 0`、`autoConvertLineEndings: false`，
//! 因此错误纠正分支（上下文替换/插入/删除容错）不可达，仅保留精确匹配
//! 路径与全部失败条件；行尾换行自动转换不实现（见 crate README）。
//!
//! `undefined` 语义镜像：JS `lines[i]` 越界为 `undefined`（比较恒不等，
//! `join` 时变空串），Rust 侧用 `None` 比较、`""` 拼接，逐点对应。

use crate::patch::Hunk;

/// jsdiff `DistanceIterator`：围绕 start 按距离交错遍历 [min, max]。
/// 例：start=2、[0,4] → 依次 3, 1, 4, 0（初始 start 由调用方先试）。
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
        // JS 尾递归 `return iterator()` 用循环重入等价展开。
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

/// 取第 `i` 行；越界/负数按 jsdiff `undefined` 处理（比较不等、拼接为空）。
fn line_or_empty<'a>(lines: &[&'a str], index: i64) -> &'a str {
    if index < 0 {
        return "";
    }
    lines.get(index as usize).copied().unwrap_or("")
}

/// 比较语义：jsdiff 默认 `compareLine` = 严格字符串相等；`undefined` 不等。
fn line_at<'a>(lines: &[&'a str], index: i64) -> Option<&'a str> {
    if index < 0 {
        return None;
    }
    lines.get(index as usize).copied()
}

/// 在 `to_pos` 处精确套用一个 hunk（fuzz 0：任一不匹配即失败）。
/// 成功返回（输出行, 收尾位置 `oldLineLastI`）。
fn apply_hunk<'a>(
    hunk_lines: &'a [String],
    lines: &[&'a str],
    to_pos: i64,
) -> Option<(Vec<&'a str>, i64)> {
    let mut patched: Vec<&'a str> = Vec::new();
    let mut pos = to_pos;
    // 自上次编辑以来的连续 context 行数：收尾时从输出与位置上双双剔除。
    let mut trailing_context: i64 = 0;
    // jsdiff 的 nextContextLineMustMatch/lastContextLineMatched 只服务
    // fuzz>0 的错误纠正分支；fuzz 0 下前者恒不触发、后者恒为 true。
    for hunk_line in hunk_lines {
        let operation = hunk_line.chars().next().unwrap_or(' ');
        // operation 为 ASCII（'-'/'+'/' '）时首字符必为单字节，切片安全。
        let content = hunk_line.get(1..).unwrap_or("");
        match operation {
            '-' => {
                if line_at(lines, pos) == Some(content) {
                    pos += 1;
                    trailing_context = 0;
                } else {
                    // 删除行必须逐行精确存在（fuzz 0 无容错分支）。
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
            // 其它前缀：jsdiff 三个 if 都不命中，整行静默跳过。
            _ => {}
        }
    }
    // 与 jsdiff 一致：剔除尾部未修改 context（不进输出、位置回退）。
    let trimmed = (patched.len() as i64).saturating_sub(trailing_context) as usize;
    patched.truncate(trimmed);
    pos -= trailing_context;
    Some((patched, pos - 1))
}

/// EOFNL 预处理：扫描最后 hunk 的 `\ No newline at end of file` 标记。
/// 返回 `None` 表示 fuzz 0 下的硬失败（源文件换行形态与补丁矛盾）。
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
            "上下文全不匹配 → jsdiff false"
        );
        // 上下文部分漂移但删除行缺失 → 失败。
        assert!(
            apply_patch("a\nother\nc\n", &hunks).is_none(),
            "删除目标行缺失"
        );
    }

    #[test]
    fn empty_hunks_return_source_unchanged() {
        let hunks = hunks("a\n", "a\n");
        assert!(hunks.is_empty(), "相同输入没有 hunk");
        assert_eq!(
            apply_patch("raw\ncontent", &hunks).expect("空补丁"),
            "raw\ncontent"
        );
        assert_eq!(apply_patch("", &hunks).expect("空补丁"), "");
    }

    #[test]
    fn eofnl_add_remove_and_contradiction_paths() {
        // 加末尾换行：源无换行 → 补一行再套用。
        let add = hunks("a", "a\n");
        assert_eq!(
            add[0].lines,
            vec!["-a", "\\ No newline at end of file", "+a"]
        );
        assert_eq!(apply_patch("a", &add).expect("可应用"), "a\n");
        // 源已有换行却要求添加 → 矛盾失败（fuzz 0）。
        assert!(apply_patch("a\n", &add).is_none(), "已有换行 → false");

        // 去末尾换行。
        let remove = hunks("a\n", "a");
        assert_eq!(
            remove[0].lines,
            vec!["-a", "+a", "\\ No newline at end of file"]
        );
        assert_eq!(apply_patch("a\n", &remove).expect("可应用"), "a");
        // 源无换行却要求去除 → 矛盾失败。
        assert!(apply_patch("a", &remove).is_none(), "本就无换行 → false");

        // 末行改写且两侧都无换行：只校验源末行无换行。
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
        assert_eq!(apply_patch("a\nb", &both).expect("可应用"), "a\nB");
        assert!(
            apply_patch("a\nb\n", &both).is_none(),
            "源末行有换行（末元素为空串）→ false"
        );
    }

    #[test]
    fn insert_at_start_and_delete_all_and_crlf() {
        let insert = hunks("b\n", "a\nb\n");
        assert_eq!(apply_patch("b\n", &insert).expect("可应用"), "a\nb\n");

        let delete = hunks("a\nb\n", "");
        assert_eq!(apply_patch("a\nb\n", &delete).expect("可删除全部"), "");

        // CRLF：`\r` 属行内容，补丁行带 `\r` 后比较才相等。
        let crlf = hunks("a\r\nb\r\n", "a\r\nB\r\n");
        assert_eq!(crlf[0].lines, vec![" a\r", "-b\r", "+B\r"]);
        assert_eq!(
            apply_patch("a\r\nb\r\n", &crlf).expect("可应用"),
            "a\r\nB\r\n",
            "CRLF 源必须原样保留"
        );
        // LF 源对 CRLF 补丁 → 行不等 → 失败（fuzz 0 无行尾转换）。
        assert!(
            apply_patch("a\nb\n", &crlf).is_none(),
            "无 autoConvertLineEndings"
        );
    }

    #[test]
    fn later_hunks_do_not_overlap_earlier_regions() {
        // 同一补丁两处远距变更：第二个 hunk 应套用在偏移校正后的位置。
        let old: String = (0..20).map(|i| format!("{i}\n")).collect();
        let mut lines: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        lines[1] = "one-changed".into();
        lines[18] = "eighteen-changed".into();
        let new: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let hunks = hunks(&old, &new);
        assert_eq!(hunks.len(), 2, "两处远距变更: {hunks:?}");
        let applied = apply_patch(&old, &hunks).expect("双 hunk 可应用");
        assert_eq!(applied, new, "应用结果必须等于目标文本");
    }
}
