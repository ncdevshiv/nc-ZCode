//! Glob / Grep：workspace 检索，基于 S1 `zcode-scan` 的 walker（in-process，
//! 无 sidecar 跳转）。忽略目录与 TS 侧一致的最小集：node_modules/.git/target/
//! dist/.turbo。Grep 为 regex-only（对齐 TS Grep/ripgrep 语义，非法 pattern 直接
//! 报错，不做静默字面量回退）。

use std::sync::atomic::AtomicBool;

use regex::Regex;
use serde_json::{json, Value};
use zcode_scan::{walk_storage_root, WalkOptions};

use super::{
    optional_string, require_string, string_schema, truncate_to_limit, Tool, ToolContext,
    ToolContract, ToolError, ToolErrorCode,
};

const SKIP_DIRS: [&str; 5] = ["node_modules", ".git", "target", "dist", ".turbo"];
const MAX_GLOB_RESULTS: usize = 1000;
const MAX_GREP_MATCHES: usize = 200;
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;

/// 检索不提供取消探针（本地只读遍历，超时上限即边界）；每次调用一个本地开关，
/// 不用 const（interior mutability 的 const 是 clippy 明令禁止的坑）。
fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn should_skip(relative_path: &str) -> bool {
    relative_path
        .split('/')
        .any(|segment| SKIP_DIRS.contains(&segment))
}

/// 简化 glob 匹配：`**` 跨目录、`*` 同层任意、`?` 单字符。按 `/` 分段匹配。
fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern_segments: Vec<&str> = pattern.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();
    fn segment_matches(pattern: &str, text: &str) -> bool {
        // 单段内 * / ? 匹配（不含 /）。
        let pattern_chars: Vec<char> = pattern.chars().collect();
        let text_chars: Vec<char> = text.chars().collect();
        let (mut p, mut t) = (0usize, 0usize);
        let mut star: Option<(usize, usize)> = None;
        while t < text_chars.len() {
            if p < pattern_chars.len()
                && (pattern_chars[p] == '?' || pattern_chars[p] == text_chars[t])
            {
                p += 1;
                t += 1;
            } else if p < pattern_chars.len() && pattern_chars[p] == '*' {
                star = Some((p, t));
                p += 1;
            } else if let Some((star_p, star_t)) = star {
                p = star_p + 1;
                t = star_t + 1;
                star = Some((star_p, t));
            } else {
                return false;
            }
        }
        while p < pattern_chars.len() && pattern_chars[p] == '*' {
            p += 1;
        }
        p == pattern_chars.len()
    }

    fn walk(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.first() {
            None => path.is_empty(),
            Some(&"**") => {
                for skip in 0..=path.len() {
                    if walk(&pattern[1..], &path[skip..]) {
                        return true;
                    }
                }
                false
            }
            Some(&segment) => match path.first() {
                Some(&head) if segment_matches(segment, head) => walk(&pattern[1..], &path[1..]),
                _ => false,
            },
        }
    }

    walk(&pattern_segments, &path_segments)
}

pub struct GlobTool;

impl Tool for GlobTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "Glob".to_string(),
            description: "Find files by glob pattern inside the workspace (supports *, **, ?). Returns paths sorted, capped at 1000.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": string_schema("glob pattern relative to the workspace root, e.g. src/**/*.rs"),
                    "path": string_schema("subdirectory to search in (default: workspace root)"),
                },
                "required": ["pattern"],
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: super::SCOPE_NONE,
            timeout_ms: 15_000,
            max_output_bytes: 128 * 1024,
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let pattern = require_string(input, "pattern")?;
        let subdir = optional_string(input, "path").unwrap_or(".");
        let root = if subdir == "." {
            ctx.workspace_root.clone()
        } else {
            super::fs_tools::resolve_workspace_path(ctx, subdir)?
        };
        let mut matches: Vec<String> = Vec::new();
        let options = WalkOptions::default();
        walk_storage_root(
            &root,
            &options,
            &no_cancel(),
            |entry| {
                if should_skip(&entry.relative_path) {
                    return;
                }
                let candidate = if subdir == "." {
                    entry.relative_path.clone()
                } else {
                    format!("{}/{}", subdir.trim_end_matches('/'), entry.relative_path)
                };
                if glob_matches(pattern, &candidate) {
                    matches.push(candidate);
                }
            },
            |_error| {},
        );
        matches.sort();
        matches.dedup();
        let total = matches.len();
        matches.truncate(MAX_GLOB_RESULTS);
        let mut out = matches.join("\n");
        if total > matches.len() {
            out.push_str(&format!(
                "\n[capped at {MAX_GLOB_RESULTS} of {total} matches]"
            ));
        }
        Ok(out)
    }
}

pub struct GrepTool;

impl Tool for GrepTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "Grep".to_string(),
            description: "Search file contents with a regex (ripgrep-style). output_mode: content (default), files_with_matches, or count.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": string_schema("regular expression"),
                    "path": string_schema("file or subdirectory to search (default: workspace root)"),
                    "glob": string_schema("filter files by glob, e.g. *.rs"),
                    "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"]},
                    "-i": {"type": "boolean", "description": "case-insensitive"},
                },
                "required": ["pattern"],
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: super::SCOPE_NONE,
            timeout_ms: 20_000,
            max_output_bytes: 256 * 1024,
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let pattern = require_string(input, "pattern")?;
        let case_insensitive = input.get("-i").and_then(Value::as_bool).unwrap_or(false);
        let regex = Regex::new(&if case_insensitive {
            format!("(?i){pattern}")
        } else {
            pattern.to_string()
        })
        .map_err(|error| {
            ToolError::new(
                ToolErrorCode::InvalidInput,
                format!("invalid regex: {error}"),
            )
        })?;
        let output_mode = optional_string(input, "output_mode").unwrap_or("content");
        let file_glob = optional_string(input, "glob");
        let subdir = optional_string(input, "path").unwrap_or(".");

        let root = if subdir == "." {
            ctx.workspace_root.clone()
        } else {
            super::fs_tools::resolve_workspace_path(ctx, subdir)?
        };

        // path 可以是单文件（TS 同款）：走文件直读路径。
        let single_file = if subdir == "." {
            None
        } else {
            let candidate = super::fs_tools::resolve_workspace_path(ctx, subdir).ok();
            candidate.filter(|path| path.is_file())
        };

        let mut files: Vec<String> = Vec::new();
        if let Some(file) = single_file {
            files.push(file.to_string_lossy().to_string());
        } else {
            let options = WalkOptions::default();
            walk_storage_root(
                &root,
                &options,
                &no_cancel(),
                |entry| {
                    if should_skip(&entry.relative_path) {
                        return;
                    }
                    let candidate = if subdir == "." {
                        entry.relative_path.clone()
                    } else {
                        format!("{}/{}", subdir.trim_end_matches('/'), entry.relative_path)
                    };
                    if let Some(glob) = file_glob {
                        let file_name = candidate.rsplit('/').next().unwrap_or(&candidate);
                        if !glob_matches(glob, file_name) {
                            return;
                        }
                    }
                    files.push(candidate);
                },
                |_error| {},
            );
            files.sort();
        }

        let mut content_out = String::new();
        let mut counts: Vec<(String, usize)> = Vec::new();
        let mut total_matches = 0usize;
        'outer: for relative in &files {
            let absolute = root.join(relative);
            let metadata = match std::fs::metadata(&absolute) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.len() > MAX_GREP_FILE_BYTES {
                continue;
            }
            let content = match std::fs::read_to_string(&absolute) {
                Ok(content) => content,
                Err(_) => continue, // 非 UTF8 / 二进制：跳过（对齐 ripgrep 默认）
            };
            let mut file_matches = 0usize;
            for (index, line) in content.lines().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                file_matches += 1;
                total_matches += 1;
                match output_mode {
                    "files_with_matches" => {}
                    "count" => {}
                    _ => {
                        content_out.push_str(&format!("{relative}:{}:{line}\n", index + 1));
                    }
                }
                if total_matches >= MAX_GREP_MATCHES {
                    break 'outer;
                }
            }
            if file_matches > 0 {
                counts.push((relative.clone(), file_matches));
            }
        }

        let out = match output_mode {
            "files_with_matches" => counts
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>()
                .join("\n"),
            "count" => counts
                .into_iter()
                .map(|(path, count)| format!("{path}:{count}"))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => content_out,
        };
        let (out, _) = truncate_to_limit(out, 256 * 1024);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn test_ctx(root: &Path) -> ToolContext {
        ToolContext {
            workspace_root: root.to_path_buf(),
            full_access: true,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn temp_workspace(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zcode-host-search-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).expect("workspace");
        fs::write(
            dir.join("src/main.rs"),
            "fn main() {\n    println!(\"hello native\");\n}\n",
        )
        .expect("write");
        fs::write(
            dir.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .expect("write");
        fs::create_dir_all(dir.join("node_modules/pkg")).expect("nm");
        fs::write(
            dir.join("node_modules/pkg/index.js"),
            "println!(\"ignored\")\n",
        )
        .expect("write");
        dir
    }

    #[test]
    fn glob_matches_recursive_patterns() {
        assert!(glob_matches("src/**/*.rs", "src/main.rs"));
        assert!(glob_matches("src/*.rs", "src/lib.rs"));
        assert!(!glob_matches("src/*.rs", "src/nested/deep.rs"));
        assert!(glob_matches("**/*.rs", "a/b/c.rs"));
        assert!(!glob_matches("*.ts", "src/main.rs"));
        assert!(glob_matches("src/mai?.rs", "src/main.rs"));
    }

    #[test]
    fn glob_tool_skips_ignored_dirs() {
        let root = temp_workspace("glob");
        let out = GlobTool
            .execute(&json!({"pattern": "**/*.js"}), &test_ctx(&root))
            .expect("glob");
        assert!(out.is_empty(), "node_modules must be skipped: {out}");
        let out = GlobTool
            .execute(&json!({"pattern": "**/*.rs"}), &test_ctx(&root))
            .expect("glob");
        assert_eq!(out.lines().count(), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_content_files_and_count_modes() {
        let root = temp_workspace("grep");
        let ctx = test_ctx(&root);
        let content = GrepTool
            .execute(&json!({"pattern": "println"}), &ctx)
            .expect("grep content");
        assert!(content.contains("src/main.rs:2:"));

        let files = GrepTool
            .execute(
                &json!({"pattern": "fn", "output_mode": "files_with_matches"}),
                &ctx,
            )
            .expect("grep files");
        assert_eq!(files.lines().count(), 2);

        let counts = GrepTool
            .execute(&json!({"pattern": "fn", "output_mode": "count"}), &ctx)
            .expect("grep count");
        assert!(counts.contains("src/lib.rs:1"));

        let ci = GrepTool
            .execute(&json!({"pattern": "PRINTLN", "-i": true}), &ctx)
            .expect("case-insensitive");
        assert!(ci.contains("src/main.rs:2:"));

        let bad = GrepTool.execute(&json!({"pattern": "fn("}), &ctx);
        assert!(
            bad.is_err(),
            "invalid regex must error, not silently fall back"
        );

        let globbed = GrepTool
            .execute(&json!({"pattern": "fn", "glob": "lib.rs"}), &ctx)
            .expect("glob filter");
        assert!(globbed.contains("src/lib.rs:1"));
        assert!(!globbed.contains("src/main.rs"));
        let _ = fs::remove_dir_all(&root);
    }
}
