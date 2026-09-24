//! Read / Write / Edit：workspace 路径策略 + read-before-edit 状态。
//!
//! 对齐 TS 行为子集：Read 行号输出 + 截断；Write 创建/覆盖 + 体积上限；
//! Edit 精确串替换且要求 old_string 唯一命中；三者都不得逃逸 workspace 根
//! （路径穿越是外部输入边界，必须挡）。readFileState 登记使 Edit 前必须读过
//! 该文件（防盲改）。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{
    optional_u64, require_string, string_schema, Tool, ToolContext, ToolContract, ToolError,
    ToolErrorCode,
};

const DEFAULT_READ_LINES: usize = 2000;
const DEFAULT_READ_LINE_CHARS: usize = 2000;
const MAX_WRITE_BYTES: usize = 8 * 1024 * 1024;
/// TS `contracts/tools/read.ts` 的体积上限：256 KiB（对齐后 Rust Read 与 TS 同界）。
const READ_MAX_FILE_SIZE_BYTES: u64 = 256 * 1024;

/// 解析并校验 workspace 内的路径：拒绝 `..` 逃逸与非 workspace 绝对路径。
/// canonicalize 前的词法校验 + canonicalize 后的前缀校验双闸。
pub fn resolve_workspace_path(ctx: &ToolContext, raw: &str) -> Result<PathBuf, ToolError> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(ToolError::new(
            ToolErrorCode::PathEscapesWorkspace,
            format!("absolute paths are not allowed: {raw}"),
        ));
    }
    let mut current = ctx.workspace_root.clone();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => current.push(part),
            std::path::Component::CurDir => {}
            _ => {
                return Err(ToolError::new(
                    ToolErrorCode::PathEscapesWorkspace,
                    format!("path escapes workspace: {raw}"),
                ))
            }
        }
    }
    let canonical_root = ctx
        .workspace_root
        .canonicalize()
        .unwrap_or_else(|_| ctx.workspace_root.clone());
    // 目标可能尚不存在（Write/Edit 新建）：校验最长存在前缀。
    let mut probe = current.clone();
    loop {
        if probe.exists() {
            let canonical = probe.canonicalize().unwrap_or(probe.clone());
            if !canonical.starts_with(&canonical_root) {
                return Err(ToolError::new(
                    ToolErrorCode::PathEscapesWorkspace,
                    format!("path resolves outside workspace: {raw}"),
                ));
            }
            break;
        }
        if !probe.pop() {
            break;
        }
    }
    Ok(current)
}

fn check_cancelled(ctx: &ToolContext) -> Result<(), ToolError> {
    if ctx.is_cancelled() {
        return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
    }
    Ok(())
}

pub struct ReadTool;

impl Tool for ReadTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "Read".to_string(),
            description: "Read a text file with line numbers. Max 2000 lines / 2000 chars per line by default; use offset/limit for long files.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": string_schema("path relative to the workspace root"),
                    "offset": {"type": "integer", "description": "1-based start line"},
                    "limit": {"type": "integer", "description": "max lines to read"},
                },
                "required": ["file_path"],
            }),
            read_only: true,
            destructive: false,
            concurrent_safe: true,
            side_effect_scope: super::SCOPE_NONE,
            timeout_ms: 10_000,
            max_output_bytes: 256 * 1024,
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        check_cancelled(ctx)?;
        let raw = require_string(input, "file_path")?;
        let path = resolve_workspace_path(ctx, raw)?;
        // 体积上限与 TS Read 一致（256 KiB）：超限直接拒绝，交给模型换方式
        // （offset/limit）而不是先读进内存再截断。
        let metadata = std::fs::metadata(&path).map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("stat {raw}: {error}"),
            )
        })?;
        if metadata.len() > READ_MAX_FILE_SIZE_BYTES {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                format!("{raw} exceeds the {READ_MAX_FILE_SIZE_BYTES} byte read limit"),
            ));
        }
        let content = std::fs::read_to_string(&path).map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("read {raw}: {error}"),
            )
        })?;
        let offset = optional_u64(input, "offset")
            .map(|value| value as usize)
            .unwrap_or(1)
            .max(1);
        let limit = optional_u64(input, "limit")
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_READ_LINES);
        let mut out = String::new();
        for (index, line) in content.lines().enumerate().skip(offset - 1).take(limit) {
            let shown: String = line.chars().take(DEFAULT_READ_LINE_CHARS).collect();
            out.push_str(&format!("{:>6}\t{}\n", index + 1, shown));
        }
        if let Ok(mut read_state) = ctx.read_state.lock() {
            read_state.insert(path);
        }
        Ok(out)
    }
}

pub struct WriteTool;

impl Tool for WriteTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "Write".to_string(),
            description: "Create or overwrite a file inside the workspace. Creates parent directories. Max 8 MiB.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": string_schema("path relative to the workspace root"),
                    "content": string_schema("full file content"),
                },
                "required": ["file_path", "content"],
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: super::SCOPE_WORKSPACE,
            timeout_ms: 10_000,
            max_output_bytes: 4 * 1024,
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        check_cancelled(ctx)?;
        let raw = require_string(input, "file_path")?;
        let content = require_string(input, "content")?;
        if content.len() > MAX_WRITE_BYTES {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                format!("content exceeds {MAX_WRITE_BYTES} byte cap"),
            ));
        }
        let path = resolve_workspace_path(ctx, raw)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ToolError::new(
                    ToolErrorCode::ExecutionFailed,
                    format!("mkdir {raw}: {error}"),
                )
            })?;
        }
        std::fs::write(&path, content).map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("write {raw}: {error}"),
            )
        })?;
        Ok(format!("wrote {} bytes to {raw}", content.len()))
    }
}

pub struct EditTool;

impl Tool for EditTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "Edit".to_string(),
            description: "Replace an exact string in a file. old_string must be unique unless replace_all is set. The file must be Read first.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": string_schema("path relative to the workspace root"),
                    "old_string": string_schema("exact text to replace"),
                    "new_string": string_schema("replacement text"),
                    "replace_all": {"type": "boolean", "description": "replace every occurrence"},
                },
                "required": ["file_path", "old_string", "new_string"],
            }),
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            side_effect_scope: super::SCOPE_WORKSPACE,
            timeout_ms: 10_000,
            max_output_bytes: 4 * 1024,
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        check_cancelled(ctx)?;
        let raw = require_string(input, "file_path")?;
        let old_string = require_string(input, "old_string")?;
        let new_string = require_string(input, "new_string")?;
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = resolve_workspace_path(ctx, raw)?;

        let read_first = ctx
            .read_state
            .lock()
            .map(|state| state.contains(&path))
            .unwrap_or(false);
        if !read_first {
            return Err(ToolError::new(
                ToolErrorCode::PermissionDenied,
                format!("Read {raw} before editing it (read-before-edit is enforced)"),
            ));
        }

        let content = std::fs::read_to_string(&path).map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("read {raw}: {error}"),
            )
        })?;
        let occurrences = content.matches(old_string).count();
        if occurrences == 0 {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                "old_string not found in file".to_string(),
            ));
        }
        if occurrences > 1 && !replace_all {
            return Err(ToolError::new(
                ToolErrorCode::InvalidInput,
                format!(
                    "old_string matches {occurrences} times; make it unique or set replace_all"
                ),
            ));
        }
        let updated = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };
        std::fs::write(&path, &updated).map_err(|error| {
            ToolError::new(
                ToolErrorCode::ExecutionFailed,
                format!("write {raw}: {error}"),
            )
        })?;
        Ok(format!(
            "edited {raw} ({occurrences} replacement{})",
            if occurrences == 1 { "" } else { "s" }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    fn test_ctx(root: &Path, full_access: bool) -> ToolContext {
        ToolContext {
            workspace_root: root.to_path_buf(),
            full_access,
            trace_id: "t".to_string(),
            turn_id: "turn".to_string(),
            read_state: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
        }
    }

    fn temp_workspace(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zcode-host-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("workspace");
        dir
    }

    #[test]
    fn path_policy_rejects_escape_and_absolute() {
        let root = temp_workspace("paths");
        let ctx = test_ctx(&root, false);
        assert!(resolve_workspace_path(&ctx, "../outside.txt").is_err());
        assert!(resolve_workspace_path(&ctx, "/etc/passwd").is_err());
        assert!(resolve_workspace_path(&ctx, "a/b/c.txt").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_then_read_then_edit_round_trip() {
        let root = temp_workspace("roundtrip");
        let ctx = test_ctx(&root, true);
        let write = WriteTool;
        let read = ReadTool;
        let edit = EditTool;

        write
            .execute(
                &json!({"file_path": "notes/a.txt", "content": "hello\nworld\n"}),
                &ctx,
            )
            .expect("write");
        let output = read
            .execute(&json!({"file_path": "notes/a.txt"}), &ctx)
            .expect("read");
        assert!(output.contains("hello"));

        // edit 前未读 → 拒绝（换一个文件验证 read-before-edit）
        let denied = edit.execute(
            &json!({"file_path": "notes/a.txt", "old_string": "hello", "new_string": "hi"}),
            &test_ctx(&root, true),
        );
        assert!(denied.is_err(), "read-before-edit must gate Edit");

        edit.execute(
            &json!({"file_path": "notes/a.txt", "old_string": "world", "new_string": "rust"}),
            &ctx,
        )
        .expect("edit after read");
        let after = std::fs::read_to_string(root.join("notes/a.txt")).expect("read back");
        assert_eq!(after, "hello\nrust\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn edit_requires_unique_match() {
        let root = temp_workspace("unique");
        let ctx = test_ctx(&root, true);
        WriteTool
            .execute(&json!({"file_path": "dup.txt", "content": "x\nx\n"}), &ctx)
            .expect("write");
        ReadTool
            .execute(&json!({"file_path": "dup.txt"}), &ctx)
            .expect("read");
        let ambiguous = EditTool.execute(
            &json!({"file_path": "dup.txt", "old_string": "x", "new_string": "y"}),
            &ctx,
        );
        assert!(ambiguous.is_err());
        EditTool
            .execute(
                &json!({"file_path": "dup.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
                &ctx,
            )
            .expect("replace_all");
        assert_eq!(
            std::fs::read_to_string(root.join("dup.txt")).expect("read"),
            "y\ny\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancelled_context_blocks_execution() {
        let root = temp_workspace("cancel");
        let ctx = test_ctx(&root, true);
        ctx.cancel.store(true, Ordering::SeqCst);
        let result = WriteTool.execute(&json!({"file_path": "nope.txt", "content": "x"}), &ctx);
        assert!(matches!(
            result,
            Err(error) if matches!(error.code, ToolErrorCode::Cancelled)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}
