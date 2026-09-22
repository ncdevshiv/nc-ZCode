//! 行切分工具：镜像 jsdiff `diff/line.js#tokenize` 与 `patch/create.js#splitLines`。
//!
//! 行级 diff 的 token 即“带行尾换行的行”，CRLF 的 `\r` 属于行内容的一部分
//! （jsdiff 只把 `\n` / `\r\n` 当分隔符，且把分隔符合并进前一个 token）。

/// 按 `\n` / `\r\n` 切分并把分隔符合并进前一行（`newlineIsToken=false` 语义），
/// 再按 jsdiff `removeEmpty` 去掉空 token。
pub(crate) fn tokenize(value: &str) -> Vec<String> {
    let bytes = value.as_bytes();
    // 交替产出 内容 / 分隔符 / 内容 ……（索引奇偶决定归属）。
    let mut parts: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            // `\r\n` 整体作为分隔符；`\r` 只有紧跟当前内容才归属本分隔符。
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
    // jsdiff：丢弃末尾空 token（字符串以换行结尾时 split 出的空串）。
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

/// patch 构建用：按 `\n` 切分，除文件末行外每行保留行尾换行
/// （镜像 jsdiff `splitLines`：无尾换行时剥掉最后一个刚补的 `\n`）。
pub(crate) fn split_lines(text: &str) -> Vec<String> {
    let has_trailing = text.ends_with('\n');
    let mut lines: Vec<String> = text.split('\n').map(|line| format!("{line}\n")).collect();
    if has_trailing {
        lines.pop();
    } else if let Some(mut last) = lines.pop() {
        // 去掉刚补上的行尾换行，保留原始“末行无换行”形态。
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
        // CRLF：`\r` 归入分隔符并合并进前行。
        assert_eq!(tokenize("a\r\nb"), vec!["a\r\n", "b"]);
        // 孤立 `\r` 不是分隔符，留在行内容里。
        assert_eq!(tokenize("a\rb\n"), vec!["a\rb\n"]);
        // 连续空行：分隔符合并后不存在空 token。
        assert_eq!(tokenize("a\n\n\nb"), vec!["a\n", "\n", "\n", "b"]);
        assert_eq!(tokenize("\r\n\r\n"), vec!["\r\n", "\r\n"]);
    }

    #[test]
    fn split_lines_keeps_newlines_except_bare_tail() {
        assert_eq!(split_lines("a\nb\n"), vec!["a\n", "b\n"]);
        assert_eq!(split_lines("a\nb"), vec!["a\n", "b"]);
        assert_eq!(split_lines(""), vec![""]);
        assert_eq!(split_lines("\n"), vec!["\n"]);
        // CRLF 行内容带 `\r`。
        assert_eq!(split_lines("a\r\nb\r\n"), vec!["a\r\n", "b\r\n"]);
    }
}
