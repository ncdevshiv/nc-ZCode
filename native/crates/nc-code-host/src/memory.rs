//! 项目记忆索引：把 `MEMORY.md` 摘要注入 context（对齐 TS
//! `core/src/memory/index-content.ts` + `memory-file-path.ts`）。
//!
//! 契约（逐条对齐 TS）：
//!   - 文件名 `MEMORY.md`，位于 host 的 memory root（默认 `<data-dir>/memory`）；
//!   - 上限 **200 行 / 25_000 字符**（MEMORY_INDEX_LINE_LIMIT /
//!     MEMORY_INDEX_CHARACTER_LIMIT），超限截断并追加与 TS 逐字一致的 WARNING；
//!   - 缺失文件**不是错误**（无记忆即无 block，不占 context）；
//!   - 读失败也不阻断（与 TS 的「目录预创建失败不阻断执行」同纪律）。
//!
//! 与 TS 的差异（README 记录）：TS 的记忆**抽取**（memory-agent-loop 从会话提炼
//! 记忆）与主题文件写入未移植——本模块只做**读取与注入**；写入侧仍由 zcode 侧
//! 负责，两边共享同一个 MEMORY.md 格式约定。

use std::path::Path;

const MEMORY_INDEX_LINE_LIMIT: usize = 200;
const MEMORY_INDEX_CHARACTER_LIMIT: usize = 25_000;
pub const MEMORY_FILE_NAME: &str = "MEMORY.md";

/// 读取并按 TS 规则截断记忆索引正文；文件缺失返回 None。
pub fn read_memory_index(memory_root: &Path) -> Option<String> {
    let path = memory_root.join(MEMORY_FILE_NAME);
    let raw = std::fs::read_to_string(path).ok()?;
    Some(truncate_index(&raw))
}

/// 逐字对齐 TS `index-content.ts` 的截断语义。
pub fn truncate_index(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let line_count = lines.len();
    let character_count = trimmed.chars().count();
    let line_truncated = line_count > MEMORY_INDEX_LINE_LIMIT;
    let character_truncated = character_count > MEMORY_INDEX_CHARACTER_LIMIT;
    if !line_truncated && !character_truncated {
        return trimmed.to_string();
    }

    let mut truncated: String = if line_truncated {
        lines[..MEMORY_INDEX_LINE_LIMIT].join("\n")
    } else {
        trimmed.to_string()
    };
    if character_truncated {
        // 字符上限：按 char 迭代截断（不能用 bytes，否则可能切在多字节中间）。
        truncated = truncated
            .chars()
            .take(MEMORY_INDEX_CHARACTER_LIMIT)
            .collect();
    }
    let size_description = if line_truncated && character_truncated {
        format!(
            "over {MEMORY_INDEX_LINE_LIMIT} lines and {MEMORY_INDEX_CHARACTER_LIMIT} characters"
        )
    } else if line_truncated {
        format!("over {MEMORY_INDEX_LINE_LIMIT} lines")
    } else {
        format!("over {MEMORY_INDEX_CHARACTER_LIMIT} characters")
    };
    format!(
        "{truncated}\n\n> WARNING: {MEMORY_FILE_NAME} is {size_description}. Only part of it was \
         loaded. Keep index entries to one line under ~200 chars; move detail into topic files."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_index_is_returned_trimmed() {
        assert_eq!(truncate_index("  a\nb  "), "a\nb");
        assert_eq!(truncate_index("   "), "");
    }

    #[test]
    fn line_limit_truncates_with_ts_warning() {
        let content = (0..300)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = truncate_index(&content);
        assert!(output.contains("line 199"));
        assert!(!output.contains("line 200"));
        assert!(output.contains("over 200 lines"));
        assert!(output.contains("move detail into topic files"));
    }

    #[test]
    fn character_limit_never_splits_a_multibyte_char() {
        let content = "é".repeat(MEMORY_INDEX_CHARACTER_LIMIT + 500);
        let output = truncate_index(&content);
        assert!(output.contains("over 25000 characters"));
        // 输出必须是合法 UTF-8（能安全 String 化即证明没切在字符中间）。
        assert_eq!(
            output.chars().filter(|ch| *ch == 'é').count(),
            MEMORY_INDEX_CHARACTER_LIMIT
        );
    }

    #[test]
    fn missing_memory_is_not_an_error() {
        let missing = std::env::temp_dir().join("zcode-memory-missing-dir-xyz");
        let _ = std::fs::remove_dir_all(&missing);
        assert_eq!(read_memory_index(&missing), None);
    }

    #[test]
    fn existing_memory_is_read() {
        let dir = std::env::temp_dir().join(format!("zcode-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join(MEMORY_FILE_NAME), "remember: the port is staged").expect("write");
        let index = read_memory_index(&dir).expect("memory must load");
        assert!(index.contains("the port is staged"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
