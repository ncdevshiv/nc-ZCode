//! Glob / Grepï¼šworkspace æ£€ç´¢ï¼ŒåŸºäºŽ S1 `zcode-scan` çš„ walkerï¼ˆin-processï¼Œ
//! æ—  sidecar è·³è½¬ï¼‰ã€‚å¿½ç•¥ç›®å½•ä¸Ž TS ä¾§ä¸€è‡´çš„æœ€å°é›†ï¼šnode_modules/.git/target/
//! dist/.turboã€‚Grep ä¸º regex-onlyï¼ˆå¯¹é½ TS Grep/ripgrep è¯­ä¹‰ï¼Œéžæ³• pattern ç›´æŽ¥
//! æŠ¥é”™ï¼Œä¸åšé™é»˜å­—é¢é‡å›žé€€ï¼‰ã€‚
use super::{
    optional_semantic_bool, optional_string, optional_u64, require_string, string_schema,
    truncate_to_limit, Tool, ToolContext, ToolContract, ToolError, ToolErrorCode,
};
use nc_code_scan::{walk_storage_root, WalkOptions};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
const SKIP_DIRS: [&str; 5] = ["node_modules", ".git", "target", "dist", ".turbo"];
const MAX_GLOB_RESULTS: usize = 1000;
const MAX_GREP_MATCHES: usize = 200;
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
/// æ£€ç´¢ä¸æä¾›å–æ¶ˆæŽ¢é’ˆï¼ˆæœ¬åœ°åªè¯»éåŽ†ï¼Œè¶…æ—¶ä¸Šé™å³è¾¹ç•Œï¼‰ï¼›æ¯æ¬¡è°ƒç”¨ä¸€ä¸ªæœ¬åœ°å¼€å…³ï¼Œ
/// ä¸ç”¨ constï¼ˆinterior mutability çš„ const æ˜¯ clippy æ˜Žä»¤ç¦æ­¢çš„å‘ï¼‰ã€‚
fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}
fn should_skip(relative_path: &str) -> bool {
    relative_path
        .split('/')
        .any(|segment| SKIP_DIRS.contains(&segment))
}
/// ç®€åŒ– glob åŒ¹é…ï¼š`**` è·¨ç›®å½•ã€`*` åŒå±‚ä»»æ„ã€`?` å•å­—ç¬¦ã€‚æŒ‰ `/` åˆ†æ®µåŒ¹é…ã€‚
fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern_segments: Vec<&str> = pattern.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();
    fn segment_matches(pattern: &str, text: &str) -> bool {
        // å•æ®µå†… * / ? åŒ¹é…ï¼ˆä¸å« /ï¼‰ã€‚
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
            description: "Fast file pattern matching. Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\". Returns matching file paths.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": string_schema("The glob pattern to match files against"),
                    "path": string_schema("The directory to search in. If not specified, the current working directory is used."),
                },
                "required": ["pattern"],
                "additionalProperties": false,
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: super::SCOPE_NONE,
            timeout_ms: 30_000,
            max_output_bytes: 100_000,
            max_model_bytes: 100_000,
            preview_direction: "head",
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
            description: "Content search (ripgrep-style regex). output_mode: files_with_matches (default), content, or count.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": string_schema("The regular expression pattern to search for in file contents"),
                    "path": string_schema("File or directory to search in. Defaults to current working directory."),
                    "glob": string_schema("Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\")"),
                    "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content, files_with_matches (default), or count"},
                    "-B": {"type": "number", "description": "Lines before each match (content mode)"},
                    "-A": {"type": "number", "description": "Lines after each match (content mode)"},
                    "-C": {"type": "number", "description": "Lines of context before/after (content mode)"},
                    "context": {"type": "number", "description": "Alias for -C"},
                    "-n": {"type": "boolean", "description": "Show line numbers in content mode (default true)"},
                    "-i": {"type": "boolean", "description": "Case insensitive search"},
                    "-o": {"type": "boolean", "description": "Print only matched parts (content mode)"},
                    "type": {"type": "string", "description": "File type hint (e.g. js, py, rust, go)"},
                    "head_limit": {"type": "number", "description": "Limit output entries/lines"},
                    "offset": {"type": "number", "description": "Skip first N entries/lines before head_limit"},
                    "multiline": {"type": "boolean", "description": "Enable multiline mode (dot matches newline)"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: super::SCOPE_NONE,
            timeout_ms: 30_000,
            max_output_bytes: 20_000,
            max_model_bytes: 20_000,
            preview_direction: "head",
        }
    }
    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let pattern = require_string(input, "pattern")?;
        let case_insensitive = optional_semantic_bool(input, "-i").unwrap_or(false);
        let multiline = optional_semantic_bool(input, "multiline").unwrap_or(false);
        let mut regex_pattern = String::new();
        if multiline {
            regex_pattern.push_str("(?s)");
        }
        if case_insensitive {
            regex_pattern.push_str("(?i)");
        }
        regex_pattern.push_str(pattern);
        let regex = Regex::new(&regex_pattern).map_err(|error| {
            ToolError::new(
                ToolErrorCode::InvalidInput,
                format!("invalid regex: {error}"),
            )
        })?;
        let output_mode = optional_string(input, "output_mode").unwrap_or("files_with_matches");
        let file_glob = optional_string(input, "glob");
        let file_type = optional_string(input, "type");
        let subdir = optional_string(input, "path").unwrap_or(".");
        let context_n = optional_u64(input, "-C")
            .or_else(|| optional_u64(input, "context"))
            .unwrap_or(0) as usize;
        let before = optional_u64(input, "-B")
            .map(|v| v as usize)
            .unwrap_or(context_n);
        let after = optional_u64(input, "-A")
            .map(|v| v as usize)
            .unwrap_or(context_n);
        let show_line_numbers = optional_semantic_bool(input, "-n").unwrap_or(true);
        let only_matching = optional_semantic_bool(input, "-o").unwrap_or(false);
        let head_limit = optional_u64(input, "head_limit").map(|v| v as usize);
        let offset = optional_u64(input, "offset").unwrap_or(0) as usize;
        let type_filter = file_type.map(|s| s.to_string());
        let root = if subdir == "." {
            ctx.workspace_root.clone()
        } else {
            super::fs_tools::resolve_workspace_path(ctx, subdir)?
        };
        let single_file = if subdir == "." {
            None
        } else {
            super::fs_tools::resolve_workspace_path(ctx, subdir)
                .ok()
                .filter(|p| p.is_file())
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
                    let file_name = candidate.rsplit('/').next().unwrap_or(&candidate);
                    if let Some(glob) = file_glob {
                        if !glob_matches(glob, file_name) && !glob_matches(glob, &candidate) {
                            return;
                        }
                    }
                    if let Some(ref tf) = type_filter {
                        if !matches_file_type(tf, file_name) {
                            return;
                        }
                    }
                    files.push(candidate);
                },
                |_error| {},
            );
            files.sort();
        }
        let mut content_lines: Vec<String> = Vec::new();
        let mut counts: Vec<(String, usize)> = Vec::new();
        let mut total_matches = 0usize;
        'outer: for relative in &files {
            let absolute = root.join(relative);
            let metadata = match std::fs::metadata(&absolute) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.len() > MAX_GREP_FILE_BYTES {
                continue;
            }
            let content = match std::fs::read_to_string(&absolute) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut file_matches = 0usize;
            let lines: Vec<&str> = content.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                file_matches += 1;
                total_matches += 1;
                match output_mode {
                    "files_with_matches" | "count" => {}
                    _ => {
                        if only_matching {
                            for m in regex.find_iter(line) {
                                let text = m.as_str();
                                content_lines.push(if show_line_numbers {
                                    format!("{relative}:{}:{text}", index + 1)
                                } else {
                                    format!("{relative}:{text}")
                                });
                            }
                        } else if before == 0 && after == 0 {
                            content_lines.push(if show_line_numbers {
                                format!("{relative}:{}:{line}", index + 1)
                            } else {
                                format!("{relative}:{line}")
                            });
                        } else {
                            let s = index.saturating_sub(before);
                            let e = (index + after + 1).min(lines.len());
                            for (ci, cl) in lines.iter().enumerate().take(e).skip(s) {
                                content_lines.push(if show_line_numbers {
                                    format!("{relative}:{}:{cl}", ci + 1)
                                } else {
                                    format!("{relative}:{cl}")
                                });
                            }
                        }
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
        let mut entries: Vec<String> = match output_mode {
            "files_with_matches" => counts.into_iter().map(|(p, _)| p).collect(),
            "count" => counts
                .into_iter()
                .map(|(p, c)| format!("{p}:{c}"))
                .collect(),
            _ => content_lines,
        };
        if offset > 0 {
            entries = entries.into_iter().skip(offset).collect();
        }
        if let Some(limit) = head_limit {
            entries.truncate(limit);
        }
        let out = entries.join(
            "
",
        );
        let (out, _) = truncate_to_limit(out, 20_000);
        Ok(out)
    }
}

fn matches_file_type(file_type: &str, file_name: &str) -> bool {
    let exts: &[&str] = match file_type {
        "js" | "javascript" => &["js", "jsx", "mjs", "cjs"],
        "ts" | "typescript" => &["ts", "tsx", "mts", "cts"],
        "py" | "python" => &["py"],
        "rust" | "rs" => &["rs"],
        "go" => &["go"],
        "java" => &["java"],
        "c" => &["c", "h"],
        "cpp" | "cxx" | "cc" => &["cpp", "cxx", "cc", "hpp", "h", "hh"],
        "md" | "markdown" => &["md", "markdown"],
        "json" => &["json"],
        "yaml" | "yml" => &["yaml", "yml"],
        "toml" => &["toml"],
        "html" => &["html", "htm"],
        "css" => &["css"],
        "sh" | "bash" | "shell" => &["sh", "bash", "zsh"],
        "txt" | "text" => &["txt"],
        "xml" => &["xml"],
        "sql" => &["sql"],
        other => {
            return file_name
                .rsplit('.')
                .next()
                .is_some_and(|e| e.eq_ignore_ascii_case(other));
        }
    };
    let ext = file_name.rsplit('.').next().unwrap_or("");
    exts.iter().any(|e| ext.eq_ignore_ascii_case(e))
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
            interactive: false,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
            session_id: "test-session".to_string(),
            model_ref: None,
            tool_call_id: None,
            plan_mode: false,
            ask_user: None,
            plan: None,
            subagent: None,
            background: None,
            sessions: None,
            workflow: None,
            automation: None,
            browser: None,
            computer: None,
            offpeak: None,
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
            .execute(
                &json!({"pattern": "println", "output_mode": "content"}),
                &ctx,
            )
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
            .execute(
                &json!({"pattern": "PRINTLN", "-i": true, "output_mode": "content"}),
                &ctx,
            )
            .expect("case-insensitive");
        assert!(ci.contains("src/main.rs:2:"));
        let bad = GrepTool.execute(&json!({"pattern": "fn("}), &ctx);
        assert!(
            bad.is_err(),
            "invalid regex must error, not silently fall back"
        );
        let globbed = GrepTool
            .execute(
                &json!({"pattern": "fn", "glob": "lib.rs", "output_mode": "content"}),
                &ctx,
            )
            .expect("glob filter");
        assert!(globbed.contains("src/lib.rs:1"));
        assert!(!globbed.contains("src/main.rs"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_type_limit_offset_and_no_match() {
        let root = temp_workspace("grep2");
        let ctx = test_ctx(&root);
        let typed = GrepTool
            .execute(
                &json!({"pattern": "fn", "type": "rust", "output_mode": "files_with_matches"}),
                &ctx,
            )
            .expect("type rust");
        assert_eq!(typed.lines().count(), 2);

        let limited = GrepTool
            .execute(
                &json!({"pattern": "fn", "output_mode": "content", "head_limit": 1}),
                &ctx,
            )
            .expect("head limit");
        assert!(limited.lines().count() <= 1, "limited: {limited}");

        let none = GrepTool
            .execute(
                &json!({"pattern": "ZZZ_NO_SUCH_TOKEN_999", "output_mode": "files_with_matches"}),
                &ctx,
            )
            .expect("no match");
        assert!(none.trim().is_empty(), "expected no matches: {none}");

        let offset = GrepTool
            .execute(
                &json!({"pattern": "fn", "output_mode": "files_with_matches", "offset": 1}),
                &ctx,
            )
            .expect("offset");
        assert!(offset.lines().count() <= 1, "offset: {offset}");
        let _ = fs::remove_dir_all(&root);
    }
}
