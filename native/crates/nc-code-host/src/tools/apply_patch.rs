//! `ApplyPatch`: apply a structured patch document (OpenAI-style `*** Begin Patch`
//! envelope) inside the workspace.
//!
//! Contract alignment (`contracts/src/tools/apply-patch.ts`): input `{ patch_text }`,
//! output `{ files, structuredPatch, summary }`. Error codes are mapped to stable
//! `tool.invalidInput` / `tool.executionFailed` messages that name the contract's
//! `ApplyPatchErrorCode` identifiers so callers can distinguish failure kinds.
//!
//! Supported patch directives:
//! - `*** Add File: <path>` followed by `+` lines
//! - `*** Delete File: <path>`
//! - `*** Update File: <path>` with optional `*** Move to: <path>` and `@@` hunks
//!
//! All resolved paths are workspace-confined (same policy as Read/Write/Edit).

use super::fs_tools::resolve_workspace_path;
use super::{
    require_string, string_schema, Tool, ToolContext, ToolContract, ToolError, ToolErrorCode,
};
use serde_json::{json, Value};
use std::path::PathBuf;

const APPLY_PATCH_MAX_BYTES: usize = 8 * 1024 * 1024;

fn invalid(code: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(
        ToolErrorCode::InvalidInput,
        format!("[{code}] {}", message.into()),
    )
}
fn exec_fail(code: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(
        ToolErrorCode::ExecutionFailed,
        format!("[{code}] {}", message.into()),
    )
}

#[derive(Debug, Clone)]
struct Hunk {
    old_start: usize,
    old_lines: usize,
    new_start: usize,
    new_lines: usize,
    lines: Vec<String>,
}

#[derive(Debug, Clone)]
struct FileChange {
    file_path: String,
    change_type: String,
    move_path: Option<String>,
    structured_patch: Vec<Value>,
    additions: usize,
    deletions: usize,
}

pub struct ApplyPatchTool;

impl Tool for ApplyPatchTool {
    fn contract(&self) -> ToolContract {
        ToolContract {
            name: "ApplyPatch".to_string(),
            description: "Apply a structured patch (*** Begin Patch ... *** End Patch) to files inside the workspace. Supports Add/Update/Delete/Move file operations with @@ hunks.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patch_text": string_schema("The full structured patch text to apply")
                },
                "required": ["patch_text"],
                "additionalProperties": false
            }),
            read_only: false,
            destructive: false, // may add/update; deletes are possible but treated as workspace mutation
            concurrent_safe: false,
            side_effect_scope: super::SCOPE_WORKSPACE,
            timeout_ms: 30_000,
            max_output_bytes: 256 * 1024,
            max_model_bytes: 64 * 1024,
            preview_direction: "head",
        }
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String, ToolError> {
        if ctx.is_cancelled() {
            return Err(ToolError::new(ToolErrorCode::Cancelled, "turn cancelled"));
        }
        let patch_text = require_string(input, "patch_text")?;
        if patch_text.trim().is_empty() {
            return Err(invalid("apply_patch_empty_patch", "patch_text is empty"));
        }
        if patch_text.len() > APPLY_PATCH_MAX_BYTES {
            return Err(invalid(
                "apply_patch_invalid_patch",
                format!("patch_text exceeds {} byte cap", APPLY_PATCH_MAX_BYTES),
            ));
        }
        let changes = parse_and_apply(patch_text, ctx)?;
        if changes.is_empty() {
            return Err(invalid(
                "apply_patch_empty_patch",
                "patch contains no file operations",
            ));
        }
        let mut all_hunks: Vec<Value> = Vec::new();
        let mut summary_parts: Vec<String> = Vec::new();
        for change in &changes {
            all_hunks.extend(change.structured_patch.clone());
            let verb = match change.change_type.as_str() {
                "add" => "A",
                "delete" => "D",
                "move" => "M",
                _ => "U",
            };
            if let Some(mv) = &change.move_path {
                summary_parts.push(format!("{verb} {} -> {}", change.file_path, mv));
            } else {
                summary_parts.push(format!("{verb} {}", change.file_path));
            }
        }
        let files: Vec<Value> = changes
            .into_iter()
            .map(|c| {
                json!({
                    "filePath": c.file_path,
                    "type": c.change_type,
                    "movePath": c.move_path,
                    "structuredPatch": c.structured_patch,
                    "additions": c.additions,
                    "deletions": c.deletions,
                })
            })
            .collect();
        let output = json!({
            "files": files,
            "structuredPatch": all_hunks,
            "summary": summary_parts.join("; "),
        });
        Ok(output.to_string())
    }
}

fn parse_and_apply(patch_text: &str, ctx: &ToolContext) -> Result<Vec<FileChange>, ToolError> {
    let lines: Vec<&str> = patch_text.lines().collect();
    let mut i = 0usize;
    // Accept with or without envelope markers for robustness.
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i < lines.len() && lines[i].trim() == "*** Begin Patch" {
        i += 1;
    }
    let mut changes: Vec<FileChange> = Vec::new();
    while i < lines.len() {
        let line = lines[i].trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if trimmed == "*** End Patch" {
            break;
        }
        if let Some(path) = trimmed.strip_prefix("*** Add File: ") {
            let path = path.trim().to_string();
            i += 1;
            let (content, next) = collect_prefixed_lines(&lines, i, '+')?;
            i = next;
            let change = apply_add(&path, &content, ctx)?;
            changes.push(change);
            continue;
        }
        if let Some(path) = trimmed.strip_prefix("*** Delete File: ") {
            let path = path.trim().to_string();
            i += 1;
            let change = apply_delete(&path, ctx)?;
            changes.push(change);
            continue;
        }
        if let Some(path) = trimmed.strip_prefix("*** Update File: ") {
            let path = path.trim().to_string();
            i += 1;
            let mut move_to: Option<String> = None;
            if i < lines.len() {
                if let Some(target) = lines[i].trim().strip_prefix("*** Move to: ") {
                    move_to = Some(target.trim().to_string());
                    i += 1;
                }
            }
            let (hunks, next) = parse_hunks(&lines, i)?;
            i = next;
            let change = apply_update(&path, move_to.as_deref(), &hunks, ctx)?;
            changes.push(change);
            continue;
        }
        return Err(invalid(
            "apply_patch_invalid_patch",
            format!("unexpected patch directive: {}", trimmed),
        ));
    }
    Ok(changes)
}

fn collect_prefixed_lines(
    lines: &[&str],
    mut i: usize,
    prefix: char,
) -> Result<(String, usize), ToolError> {
    let mut out = String::new();
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_end();
        if trimmed.starts_with("*** ") {
            break;
        }
        if let Some(rest) = line.strip_prefix(prefix) {
            out.push_str(rest);
            out.push('\n');
            i += 1;
        } else if line.is_empty() && prefix == '+' {
            // tolerate blank line inside add block? break to be safe
            break;
        } else {
            break;
        }
    }
    Ok((out, i))
}

fn parse_hunks(lines: &[&str], mut i: usize) -> Result<(Vec<Hunk>, usize), ToolError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    while i < lines.len() {
        let trimmed = lines[i].trim_end();
        if trimmed.starts_with("*** ") {
            break;
        }
        if trimmed.starts_with("@@") {
            let (hunk, next) = parse_one_hunk(lines, i)?;
            hunks.push(hunk);
            i = next;
            continue;
        }
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        // Some patches may have context without @@; reject to avoid silent misapply.
        return Err(invalid(
            "apply_patch_invalid_patch",
            format!("expected @@ hunk header, got: {}", trimmed),
        ));
    }
    if hunks.is_empty() {
        return Err(invalid(
            "apply_patch_invalid_patch",
            "Update File requires at least one @@ hunk",
        ));
    }
    Ok((hunks, i))
}

fn parse_one_hunk(lines: &[&str], start: usize) -> Result<(Hunk, usize), ToolError> {
    let header = lines[start].trim();
    // Parse `@@ -a,b +c,d @@` or `@@` (best-effort)
    let (old_start, old_lines, new_start, new_lines) =
        parse_hunk_header(header).unwrap_or((1, 0, 1, 0));
    let mut i = start + 1;
    let mut hunk_lines: Vec<String> = Vec::new();
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_end();
        if trimmed.starts_with("@@") || trimmed.starts_with("*** ") {
            break;
        }
        // `\ No newline at end of file` marker
        if raw.starts_with("\\ No newline") {
            i += 1;
            continue;
        }
        let first = raw.chars().next();
        match first {
            Some(' ') | Some('+') | Some('-') => {
                hunk_lines.push(raw.to_string());
                i += 1;
            }
            Some(_) => {
                // Treat as context for lenient parsing
                hunk_lines.push(format!(" {}", raw));
                i += 1;
            }
            None => {
                hunk_lines.push(" ".to_string());
                i += 1;
            }
        }
    }
    Ok((
        Hunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            lines: hunk_lines,
        },
        i,
    ))
}

fn parse_hunk_header(header: &str) -> Option<(usize, usize, usize, usize)> {
    // @@ -1,3 +1,4 @@ optional
    let h = header.trim_start_matches('@').trim();
    let h = h.split("@@").next()?.trim();
    let mut parts = h.split_whitespace();
    let old = parts.next()?; // -1,3
    let new = parts.next()?; // +1,4
    let (os, ol) = parse_range(old.strip_prefix('-')?)?;
    let (ns, nl) = parse_range(new.strip_prefix('+')?)?;
    Some((os, ol, ns, nl))
}
fn parse_range(s: &str) -> Option<(usize, usize)> {
    if let Some((a, b)) = s.split_once(',') {
        Some((a.parse().ok()?, b.parse().ok()?))
    } else {
        Some((s.parse().ok()?, 1))
    }
}

fn apply_add(path: &str, content: &str, ctx: &ToolContext) -> Result<FileChange, ToolError> {
    let resolved = resolve_workspace_path(ctx, path)?;
    if resolved.exists() {
        return Err(invalid(
            "apply_patch_file_exists",
            format!("{path}: file already exists"),
        ));
    }
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| exec_fail("apply_patch_io_error", format!("mkdir for {path}: {e}")))?;
    }
    std::fs::write(&resolved, content)
        .map_err(|e| exec_fail("apply_patch_io_error", format!("write {path}: {e}")))?;
    let additions = content.lines().count();
    let diff = make_structured_hunk(
        0,
        0,
        1,
        additions,
        content.lines().map(|l| format!("+{l}")).collect(),
    );
    Ok(FileChange {
        file_path: path.to_string(),
        change_type: "add".to_string(),
        move_path: None,
        structured_patch: vec![diff],
        additions,
        deletions: 0,
    })
}

fn apply_delete(path: &str, ctx: &ToolContext) -> Result<FileChange, ToolError> {
    let resolved = resolve_workspace_path(ctx, path)?;
    if !resolved.exists() {
        return Err(invalid(
            "apply_patch_file_not_exist",
            format!("{path}: file does not exist"),
        ));
    }
    let old = std::fs::read_to_string(&resolved)
        .map_err(|e| exec_fail("apply_patch_io_error", format!("read {path}: {e}")))?;
    std::fs::remove_file(&resolved)
        .map_err(|e| exec_fail("apply_patch_io_error", format!("delete {path}: {e}")))?;
    let deletions = old.lines().count();
    let diff = make_structured_hunk(
        1,
        deletions,
        0,
        0,
        old.lines().map(|l| format!("-{l}")).collect(),
    );
    Ok(FileChange {
        file_path: path.to_string(),
        change_type: "delete".to_string(),
        move_path: None,
        structured_patch: vec![diff],
        additions: 0,
        deletions,
    })
}

fn apply_update(
    path: &str,
    move_to: Option<&str>,
    hunks: &[Hunk],
    ctx: &ToolContext,
) -> Result<FileChange, ToolError> {
    let resolved = resolve_workspace_path(ctx, path)?;
    if !resolved.exists() {
        return Err(invalid(
            "apply_patch_file_not_exist",
            format!("{path}: file does not exist"),
        ));
    }
    let original = std::fs::read_to_string(&resolved)
        .map_err(|e| exec_fail("apply_patch_io_error", format!("read {path}: {e}")))?;
    let (updated, structured, adds, dels) = apply_hunks(&original, hunks, path)?;
    let target_path: PathBuf = if let Some(mv) = move_to {
        resolve_workspace_path(ctx, mv)?
    } else {
        resolved.clone()
    };
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| exec_fail("apply_patch_io_error", format!("mkdir for target: {e}")))?;
    }
    std::fs::write(&target_path, &updated)
        .map_err(|e| exec_fail("apply_patch_io_error", format!("write {path}: {e}")))?;
    if move_to.is_some() && target_path != resolved && resolved.exists() {
        let _ = std::fs::remove_file(&resolved);
    }
    Ok(FileChange {
        file_path: path.to_string(),
        change_type: if move_to.is_some() { "move" } else { "update" }.to_string(),
        move_path: move_to.map(|s| s.to_string()),
        structured_patch: structured,
        additions: adds,
        deletions: dels,
    })
}

fn apply_hunks(
    original: &str,
    hunks: &[Hunk],
    path: &str,
) -> Result<(String, Vec<Value>, usize, usize), ToolError> {
    let had_trailing_newline = original.ends_with('\n');
    let mut src: Vec<String> = original.lines().map(|s| s.to_string()).collect();
    // Apply from bottom to top to keep line numbers stable.
    let mut indexed: Vec<(usize, &Hunk)> = hunks.iter().enumerate().collect();
    indexed.sort_by(|a, b| b.1.old_start.cmp(&a.1.old_start).then(b.0.cmp(&a.0)));
    let mut all_structured: Vec<(usize, Value)> = Vec::new();

    for (orig_idx, hunk) in indexed {
        let (old_seq, new_seq) = split_hunk_lines(&hunk.lines);
        let pos = find_hunk_position(&src, &old_seq, hunk.old_start)?;
        let end = pos + old_seq.len();
        if pos > src.len() || end > src.len() + usize::from(old_seq.is_empty()) {
            return Err(invalid(
                "apply_patch_hunk_not_found",
                format!("{path}: hunk does not apply at expected location"),
            ));
        }
        let bounded_end = end.min(src.len());
        src.splice(pos..bounded_end, new_seq.iter().cloned());
        all_structured.push((
            orig_idx,
            make_structured_hunk(
                hunk.old_start,
                old_seq.len(),
                hunk.new_start,
                new_seq.len(),
                hunk.lines.clone(),
            ),
        ));
    }
    all_structured.sort_by_key(|(i, _)| *i);
    let structured: Vec<Value> = all_structured.into_iter().map(|(_, v)| v).collect();
    // recompute totals from markers
    let mut adds = 0usize;
    let mut dels = 0usize;
    for h in hunks {
        for l in &h.lines {
            if l.starts_with('+') && !l.starts_with("+++") {
                adds += 1;
            } else if l.starts_with('-') && !l.starts_with("---") {
                dels += 1;
            }
        }
    }
    let mut out = src.join("\n");
    if (had_trailing_newline || !out.is_empty()) && !out.ends_with('\n') {
        out.push('\n');
    }
    if original.is_empty() {
        out = new_from_hunks(hunks);
    }
    Ok((out, structured, adds, dels))
}

fn common_prefix_count(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn new_from_hunks(hunks: &[Hunk]) -> String {
    let mut out = String::new();
    for h in hunks {
        for l in &h.lines {
            if let Some(rest) = l.strip_prefix('+') {
                out.push_str(rest);
                out.push('\n');
            } else if let Some(rest) = l.strip_prefix(' ') {
                out.push_str(rest);
                out.push('\n');
            }
        }
    }
    out
}

fn split_hunk_lines(lines: &[String]) -> (Vec<String>, Vec<String>) {
    let mut old_seq = Vec::new();
    let mut new_seq = Vec::new();
    for l in lines {
        if let Some(rest) = l.strip_prefix('-') {
            old_seq.push(rest.to_string());
        } else if let Some(rest) = l.strip_prefix('+') {
            new_seq.push(rest.to_string());
        } else if let Some(rest) = l.strip_prefix(' ') {
            old_seq.push(rest.to_string());
            new_seq.push(rest.to_string());
        } else {
            old_seq.push(l.clone());
            new_seq.push(l.clone());
        }
    }
    (old_seq, new_seq)
}

fn find_hunk_position(
    src: &[String],
    old_seq: &[String],
    hint_old_start: usize,
) -> Result<usize, ToolError> {
    if old_seq.is_empty() {
        let pos = hint_old_start.saturating_sub(1).min(src.len());
        return Ok(pos);
    }
    // Try exact match at hint, then search nearby, then global.
    let hint = hint_old_start.saturating_sub(1);
    if matches_at(src, old_seq, hint) {
        return Ok(hint);
    }
    for delta in 1..=50usize {
        if hint + delta <= src.len() && matches_at(src, old_seq, hint + delta) {
            return Ok(hint + delta);
        }
        if delta <= hint && matches_at(src, old_seq, hint - delta) {
            return Ok(hint - delta);
        }
    }
    for pos in 0..=src.len().saturating_sub(old_seq.len()) {
        if matches_at(src, old_seq, pos) {
            return Ok(pos);
        }
    }
    Err(invalid(
        "apply_patch_hunk_not_found",
        "hunk context not found in target file",
    ))
}

fn matches_at(src: &[String], seq: &[String], pos: usize) -> bool {
    if pos + seq.len() > src.len() {
        return false;
    }
    src[pos..pos + seq.len()]
        .iter()
        .zip(seq.iter())
        .all(|(a, b)| a == b)
}

fn make_structured_hunk(
    old_start: usize,
    old_lines: usize,
    new_start: usize,
    new_lines: usize,
    lines: Vec<String>,
) -> Value {
    json!({
        "oldStart": old_start,
        "oldLines": old_lines,
        "newStart": new_start,
        "newLines": new_lines,
        "lines": lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn test_ctx(root: &Path) -> ToolContext {
        ToolContext {
            workspace_root: root.to_path_buf(),
            full_access: true,
            interactive: false,
            trace_id: "t".into(),
            turn_id: "turn".into(),
            session_id: "s".into(),
            model_ref: None,
            tool_call_id: None,
            plan_mode: false,
            read_state: Arc::new(Mutex::new(Default::default())),
            cancel: Arc::new(AtomicBool::new(false)),
            todos: crate::tools::todo::new_todo_store(),
            provider_config: None,
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
    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zcode-applypatch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn contract_is_workspace_mutating() {
        let c = ApplyPatchTool.contract();
        assert_eq!(c.name, "ApplyPatch");
        assert!(!c.read_only);
        assert_eq!(c.side_effect_scope, crate::tools::SCOPE_WORKSPACE);
    }

    #[test]
    fn rejects_empty_patch() {
        let root = temp("empty");
        let ctx = test_ctx(&root);
        let err = ApplyPatchTool
            .execute(&json!({"patch_text": "  "}), &ctx)
            .unwrap_err();
        assert!(err.message.contains("apply_patch_empty_patch"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn add_file_creates_content() {
        let root = temp("add");
        let ctx = test_ctx(&root);
        let patch = "*** Begin Patch\n*** Add File: hello.txt\n+line1\n+line2\n*** End Patch\n";
        let out = ApplyPatchTool
            .execute(&json!({"patch_text": patch}), &ctx)
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["files"][0]["type"], "add");
        assert!(root.join("hello.txt").exists());
        let content = std::fs::read_to_string(root.join("hello.txt")).unwrap();
        assert!(content.contains("line1"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn update_file_applies_hunk() {
        let root = temp("update");
        std::fs::write(root.join("f.txt"), "a\nb\nc\n").unwrap();
        let ctx = test_ctx(&root);
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@ -1,3 +1,3 @@\n a\n-B\n+b2\n c\n*** End Patch\n";
        // hunk expects B but file has b - search should find? our matcher is exact; use matching case
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+b2\n c\n*** End Patch\n";
        let out = ApplyPatchTool
            .execute(&json!({"patch_text": patch}), &ctx)
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["files"][0]["type"], "update");
        let content = std::fs::read_to_string(root.join("f.txt")).unwrap();
        assert!(content.contains("b2"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn delete_file_removes() {
        let root = temp("del");
        std::fs::write(root.join("gone.txt"), "bye\n").unwrap();
        let ctx = test_ctx(&root);
        let patch = "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n";
        let out = ApplyPatchTool
            .execute(&json!({"patch_text": patch}), &ctx)
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["files"][0]["type"], "delete");
        assert!(!root.join("gone.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_path_escape() {
        let root = temp("escape");
        let ctx = test_ctx(&root);
        let patch = "*** Begin Patch\n*** Add File: ../evil.txt\n+nope\n*** End Patch\n";
        let err = ApplyPatchTool
            .execute(&json!({"patch_text": patch}), &ctx)
            .unwrap_err();
        assert!(matches!(
            err.code,
            ToolErrorCode::PathEscapesWorkspace
                | ToolErrorCode::InvalidInput
                | ToolErrorCode::ExecutionFailed
        ));
        let _ = std::fs::remove_dir_all(root);
    }
}
