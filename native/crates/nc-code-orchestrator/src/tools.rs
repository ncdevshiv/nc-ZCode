//! Tool surface for NC-AG agents. Every call is workspace-scoped, permission-checked,
//! audited and bounded. Tools are plain Rust functions: no shelling out for file IO.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

use crate::error::{NcagError, Result};

/// Permissions an agent can hold (spec: specs/nc-ag-orchestration.md section 7).
pub const PERM_READ: &str = "fs.read";
pub const PERM_WRITE: &str = "fs.write";
pub const PERM_SHELL: &str = "shell.exec";
pub const PERM_GIT: &str = "git.merge";

#[derive(Debug, Clone, Default)]
pub struct PermissionSet {
    pub allowed: Vec<String>,
}

impl PermissionSet {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }

    pub fn allows(&self, permission: &str) -> bool {
        self.allowed.iter().any(|item| item == permission)
    }
}

#[derive(Debug, Clone)]
pub struct ToolDefinitionSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub fn definitions() -> Vec<ToolDefinitionSpec> {
    vec![
        ToolDefinitionSpec {
            name: "read_file",
            description: "Read a UTF-8 text file from the task worktree. Bounded to 200KB; returns numbered lines.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative path" },
                    "offset": { "type": "number", "description": "1-based first line" },
                    "limit": { "type": "number", "description": "Max lines (default 400)" }
                },
                "required": ["path"]
            }),
        },
        ToolDefinitionSpec {
            name: "list_dir",
            description: "List a directory inside the task worktree (non-recursive, sorted).",
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Workspace-relative directory; empty = root" } },
                "required": []
            }),
        },
        ToolDefinitionSpec {
            name: "grep",
            description: "Regex search file contents inside the worktree; returns path:line:text matches.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "glob": { "type": "string", "description": "Optional filename suffix filter, e.g. .rs" },
                    "max_results": { "type": "number" }
                },
                "required": ["pattern"]
            }),
        },
        ToolDefinitionSpec {
            name: "write_file",
            description: "Create or replace a file inside the worktree with the given UTF-8 content.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDefinitionSpec {
            name: "edit_file",
            description: "Replace exactly one occurrence of old_text with new_text in a file.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        },
        ToolDefinitionSpec {
            name: "run_command",
            description: "Run an allowlisted build/test command inside the worktree. Returns exit code, stdout and stderr (truncated).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command line, e.g. `cargo test -p app`" },
                    "timeout_ms": { "type": "number" }
                },
                "required": ["command"]
            }),
        },
        ToolDefinitionSpec {
            name: "submit_result",
            description: "Finish the current task. Reports what was done and which commands were run (the verifier re-runs them).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string" },
                    "files_changed": { "type": "array", "items": { "type": "string" } },
                    "commands_run": { "type": "array", "items": { "type": "string" } },
                    "limitations": { "type": "array", "items": { "type": "string" } },
                    "acceptance_notes": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["summary"]
            }),
        },
        ToolDefinitionSpec {
            name: "request_help",
            description: "Ask the orchestrator for a helper agent with a capability you lack.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "problem": { "type": "string" },
                    "capability": { "type": "string" },
                    "urgency": { "type": "string", "enum": ["low", "normal", "high"] }
                },
                "required": ["problem", "capability"]
            }),
        },
    ]
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub permissions: PermissionSet,
    pub allowed_commands: Vec<String>,
    pub agent_id: String,
    pub task_id: String,
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub text: String,
    pub changed_files: Vec<String>,
    /// Set when the agent called `submit_result`.
    pub submission: Option<crate::types::AgentSubmission>,
    /// Set when the agent called `request_help`.
    pub help_request: Option<Value>,
}

/// Stream events for an in-flight command so the monitor can show progress.
pub type ProgressFn = Arc<dyn Fn(&str) + Send + Sync>;

pub fn invoke(
    ctx: &ToolContext,
    name: &str,
    input: &Value,
    progress: Option<&ProgressFn>,
) -> Result<ToolOutcome> {
    match name {
        "read_file" => read_file(ctx, input),
        "list_dir" => list_dir(ctx, input),
        "grep" => grep(ctx, input),
        "write_file" => write_file(ctx, input),
        "edit_file" => edit_file(ctx, input),
        "run_command" => run_command(ctx, input, progress),
        "submit_result" => submit_result(input),
        "request_help" => request_help(input),
        other => Err(NcagError::invalid(format!("unknown tool: {other}"))),
    }
}

fn require_perm(ctx: &ToolContext, permission: &str) -> Result<()> {
    if ctx.permissions.allows(permission) {
        Ok(())
    } else {
        Err(NcagError::new(
            "permission_denied",
            format!(
                "agent {} is not granted {} (task {})",
                ctx.agent_id, permission, ctx.task_id
            ),
        ))
    }
}

pub fn resolve_path(ctx: &ToolContext, raw: &str) -> Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.contains('\0') {
        return Err(NcagError::invalid("path contains a NUL byte"));
    }
    let relative = Path::new(trimmed);
    if relative.is_absolute() {
        return Err(NcagError::new(
            "path_escapes_workspace",
            format!("absolute path rejected: {trimmed}"),
        ));
    }
    let mut full = ctx.workspace.clone();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => full.push(part),
            std::path::Component::CurDir => {}
            _ => {
                return Err(NcagError::new(
                    "path_escapes_workspace",
                    format!("path escapes the worktree: {trimmed}"),
                ))
            }
        }
    }
    let root = ctx
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| ctx.workspace.clone());
    let mut probe = full.clone();
    loop {
        if probe.exists() {
            let canonical = probe.canonicalize().unwrap_or(probe.clone());
            if !canonical.starts_with(&root) {
                return Err(NcagError::new(
                    "path_escapes_workspace",
                    format!("path resolves outside the worktree: {trimmed}"),
                ));
            }
            break;
        }
        if !probe.pop() {
            break;
        }
    }
    Ok(full)
}

fn string_input(input: &Value, key: &str) -> Result<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| NcagError::invalid(format!("tool input {key} must be a string")))
}

fn number_input(input: &Value, key: &str) -> Option<u64> {
    input.get(key).and_then(Value::as_u64)
}

fn read_file(ctx: &ToolContext, input: &Value) -> Result<ToolOutcome> {
    require_perm(ctx, PERM_READ)?;
    let path = resolve_path(ctx, &string_input(input, "path")?)?;
    let text = read_text_limited(&path, 200 * 1024)?;
    let offset = number_input(input, "offset").unwrap_or(1).max(1) as usize;
    let limit = number_input(input, "limit").unwrap_or(400).clamp(1, 2000) as usize;
    let lines: Vec<&str> = text.lines().collect();
    let mut body = String::new();
    for (index, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
        body.push_str(&format!("{:>5}\t{line}\n", index + 1));
    }
    if body.is_empty() {
        body.push_str("(empty file or range beyond end)\n");
    }
    Ok(ToolOutcome {
        text: body,
        changed_files: Vec::new(),
        submission: None,
        help_request: None,
    })
}

pub fn read_text_limited(path: &Path, max_bytes: u64) -> Result<String> {
    let meta = std::fs::metadata(path)
        .map_err(|error| NcagError::invalid(format!("{}: {error}", path.display())))?;
    if !meta.is_file() {
        return Err(NcagError::invalid(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > max_bytes {
        return Err(NcagError::new(
            "file_too_large",
            format!(
                "{} is {} bytes, over the {} byte read limit",
                path.display(),
                meta.len(),
                max_bytes
            ),
        ));
    }
    std::fs::read_to_string(path)
        .map_err(|error| NcagError::invalid(format!("{}: {error}", path.display())))
}

fn list_dir(ctx: &ToolContext, input: &Value) -> Result<ToolOutcome> {
    require_perm(ctx, PERM_READ)?;
    let raw = input.get("path").and_then(Value::as_str).unwrap_or("");
    let path = if raw.trim().is_empty() {
        ctx.workspace.clone()
    } else {
        resolve_path(ctx, raw)?
    };
    let mut entries: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&path)
        .map_err(|error| NcagError::invalid(format!("{}: {error}", path.display())))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata()?;
        entries.push(if meta.is_dir() {
            format!("{name}/")
        } else {
            format!("{name} ({} bytes)", meta.len())
        });
    }
    entries.sort();
    Ok(ToolOutcome {
        text: entries.join("\n"),
        changed_files: Vec::new(),
        submission: None,
        help_request: None,
    })
}

fn grep(ctx: &ToolContext, input: &Value) -> Result<ToolOutcome> {
    require_perm(ctx, PERM_READ)?;
    let pattern = string_input(input, "pattern")?;
    let regex = Regex::new(&pattern)
        .map_err(|error| NcagError::invalid(format!("invalid regex {pattern}: {error}")))?;
    let suffix = input
        .get("glob")
        .and_then(Value::as_str)
        .map(str::to_string);
    let max_results = number_input(input, "max_results")
        .unwrap_or(60)
        .clamp(1, 500) as usize;
    let mut hits: Vec<String> = Vec::new();
    walk(&ctx.workspace, &mut |path| {
        if hits.len() >= max_results {
            return;
        }
        if let Some(suffix) = &suffix {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !name.ends_with(suffix.as_str()) {
                return;
            }
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for (line_no, line) in text.lines().enumerate() {
            if hits.len() >= max_results {
                return;
            }
            if regex.is_match(line) {
                let relative = path
                    .strip_prefix(&ctx.workspace)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| path.display().to_string());
                let trimmed: String = line.chars().take(300).collect();
                hits.push(format!("{relative}:{}:{}", line_no + 1, trimmed));
            }
        }
    });
    if hits.is_empty() {
        hits.push(format!("no matches for /{pattern}/"));
    }
    Ok(ToolOutcome {
        text: hits.join("\n"),
        changed_files: Vec::new(),
        submission: None,
        help_request: None,
    })
}

const SKIP_DIRS: [&str; 5] = ["target", "node_modules", ".git", "dist", "build"];

fn walk(root: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(&path, visit);
        } else if path.is_file() {
            visit(&path);
        }
    }
}

fn write_file(ctx: &ToolContext, input: &Value) -> Result<ToolOutcome> {
    require_perm(ctx, PERM_WRITE)?;
    let path = resolve_path(ctx, &string_input(input, "path")?)?;
    let content = string_input(input, "content")?;
    if content.len() > 2 * 1024 * 1024 {
        return Err(NcagError::invalid("write exceeds the 2MB content limit"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    let relative = path
        .strip_prefix(&ctx.workspace)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.display().to_string());
    Ok(ToolOutcome {
        text: format!(
            "wrote {} bytes to {relative}",
            path.metadata().map(|m| m.len()).unwrap_or(0)
        ),
        changed_files: vec![relative],
        submission: None,
        help_request: None,
    })
}

fn edit_file(ctx: &ToolContext, input: &Value) -> Result<ToolOutcome> {
    require_perm(ctx, PERM_WRITE)?;
    let path = resolve_path(ctx, &string_input(input, "path")?)?;
    let old_text = string_input(input, "old_text")?;
    let new_text = string_input(input, "new_text")?;
    if old_text.is_empty() {
        return Err(NcagError::invalid("old_text must not be empty"));
    }
    let content = read_text_limited(&path, 2 * 1024 * 1024)?;
    let occurrences = content.matches(&old_text).count();
    if occurrences != 1 {
        return Err(NcagError::invalid(format!(
            "old_text must match exactly once in {} (found {occurrences})",
            path.display()
        )));
    }
    let updated = content.replacen(&old_text, &new_text, 1);
    std::fs::write(&path, updated)?;
    let relative = path
        .strip_prefix(&ctx.workspace)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.display().to_string());
    Ok(ToolOutcome {
        text: format!("edited {relative}"),
        changed_files: vec![relative],
        submission: None,
        help_request: None,
    })
}

fn command_allowed(ctx: &ToolContext, command: &str) -> bool {
    let normalized = command.trim().replace('\\', "/");
    let lowercase = normalized.to_ascii_lowercase();
    ctx.allowed_commands.iter().any(|allowed| {
        lowercase == allowed.to_ascii_lowercase()
            || lowercase.starts_with(&format!("{} ", allowed.to_ascii_lowercase()))
    })
}

/// Split a command line into (program, args) using double-quote aware tokenisation.
/// Resolve a program name to a real executable through `PATH`/`PATHEXT`.
///
/// On Windows `npm`, `npx`, `pnpm` and `yarn` are `.cmd` shims; `Command::new("npm")`
/// fails with "program not found" unless the extension is resolved. Everywhere else the
/// name is returned unchanged (the OS does the lookup).
pub fn resolve_program(program: &str) -> String {
    if std::path::Path::new(program).extension().is_some() {
        return program.to_string();
    }
    #[cfg(windows)]
    {
        let path = std::env::var("PATH").unwrap_or_default();
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        for dir in std::env::split_paths(&path) {
            for extension in extensions.split(';') {
                if extension.is_empty() {
                    continue;
                }
                let candidate = dir.join(format!("{program}{extension}"));
                if candidate.is_file() {
                    return candidate.to_string_lossy().to_string();
                }
            }
        }
    }
    program.to_string()
}

/// True when the command line needs a shell (pipes, redirects, chaining, variables).
pub fn needs_shell(tokens: &[String]) -> bool {
    tokens
        .iter()
        .any(|token| token.contains(['|', '>', '<', '&', ';', '$', '`', '%', '(', ')']))
}

/// A platform shell invocation for one command line (argv-level, never string-spliced).
pub fn shell_command(line: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("cmd");
        command.arg("/D").arg("/C").arg(line);
        #[allow(unused_imports)]
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(line);
        command
    }
}

pub fn tokenize_command(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut has = false;
    for ch in command.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has || !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                    has = false;
                }
            }
            c => {
                current.push(c);
                has = true;
            }
        }
    }
    if has || !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn run_command(
    ctx: &ToolContext,
    input: &Value,
    progress: Option<&ProgressFn>,
) -> Result<ToolOutcome> {
    require_perm(ctx, PERM_SHELL)?;
    let command = string_input(input, "command")?;
    if !command_allowed(ctx, &command) {
        return Err(NcagError::new(
            "command_not_allowlisted",
            format!("command is not in the project allowlist: {command}"),
        ));
    }
    let tokens = tokenize_command(&command);
    if tokens.is_empty() {
        return Err(NcagError::invalid("empty command"));
    }
    let timeout_ms = number_input(input, "timeout_ms")
        .unwrap_or(300_000)
        .clamp(1_000, 900_000);
    if let Some(progress) = progress {
        progress(&format!("$ {command}"));
    }
    // A command without shell metacharacters runs as a real argv pair: the platform shell
    // would otherwise consume the quotes a model legitimately puts inside `node -e ""`
    // (verified live: `node -e "require('x')"` reached node with the outer quotes intact but
    // the inner expression mangled). Pipelines/redirects keep the shell so semantics match
    // the developer's terminal; the allowlist already decided whether this command may run.
    // A bare shell builtin (echo, dir, cd, ...) has no executable image; those must still
    // go through the shell even without metacharacters.
    let is_builtin = matches!(
        tokens[0].to_ascii_lowercase().as_str(),
        "echo" | "dir" | "cd" | "type" | "set" | "cls" | "copy" | "move" | "del" | "echo."
    );
    let program = resolve_program(&tokens[0]);
    let mut process = if is_builtin || needs_shell(&tokens) {
        shell_command(&command)
    } else {
        let mut direct = std::process::Command::new(&program);
        direct.args(&tokens[1..]);
        #[cfg(windows)]
        {
            #[allow(unused_imports)]
            use std::os::windows::process::CommandExt;
            direct.creation_flags(0x0800_0000);
        }
        direct
    };
    process.current_dir(&ctx.workspace);
    process
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        // Hide the console window; the output still arrives through the pipes.
        #[allow(unused_imports)]
        use std::os::windows::process::CommandExt;
        process.creation_flags(0x0800_0000);
    }
    let mut child = process
        .spawn()
        .map_err(|error| NcagError::new("command_failed", format!("{command}: {error}")))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {}
            Err(error) => {
                return Err(NcagError::new(
                    "command_failed",
                    format!("{command}: {error}"),
                ))
            }
        }
        if started.elapsed() > Duration::from_millis(timeout_ms) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(NcagError::new(
                "command_timeout",
                format!("{command} exceeded {timeout_ms}ms"),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = child
        .wait_with_output()
        .map_err(|error| NcagError::new("command_failed", error.to_string()))?;
    let stdout = truncate_text(&decode_output(&output.stdout), 20_000);
    let stderr = truncate_text(&decode_output(&output.stderr), 10_000);
    let code = output.status.code().unwrap_or(-1);
    Ok(ToolOutcome {
        text: format!("exit={code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"),
        changed_files: Vec::new(),
        submission: None,
        help_request: None,
    })
}

pub fn decode_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\r', "")
}

pub fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let tail: String = text
        .chars()
        .skip(text.chars().count().saturating_sub(max_chars))
        .collect();
    format!("...(truncated, showing the tail)\n{tail}")
}

fn strings(input: &Value, key: &str) -> Vec<String> {
    input
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn submit_result(input: &Value) -> Result<ToolOutcome> {
    let submission = crate::types::AgentSubmission {
        summary: string_input(input, "summary")?,
        files_changed: strings(input, "files_changed"),
        commands_run: strings(input, "commands_run"),
        limitations: strings(input, "limitations"),
        acceptance_notes: strings(input, "acceptance_notes"),
    };
    Ok(ToolOutcome {
        text: "result recorded; the orchestrator will verify acceptance criteria".to_string(),
        changed_files: submission.files_changed.clone(),
        submission: Some(submission),
        help_request: None,
    })
}

fn request_help(input: &Value) -> Result<ToolOutcome> {
    let payload = json!({
        "problem": string_input(input, "problem")?,
        "capability": string_input(input, "capability")?,
        "urgency": input.get("urgency").and_then(Value::as_str).unwrap_or("normal"),
    });
    Ok(ToolOutcome {
        text: "help request forwarded to the orchestrator".to_string(),
        changed_files: Vec::new(),
        submission: None,
        help_request: Some(payload),
    })
}

/// Convert NC-AG tool specs to the provider's tool definition shape.
pub fn provider_definitions() -> Vec<nc_code_provider::request::ToolDefinition> {
    definitions()
        .into_iter()
        .map(|spec| nc_code_provider::request::ToolDefinition {
            name: spec.name.to_string(),
            description: Some(spec.description.to_string()),
            input_schema: spec.input_schema,
            strict: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn context(dir: &Path) -> ToolContext {
        ToolContext {
            workspace: dir.to_path_buf(),
            permissions: PermissionSet::new(vec![
                PERM_READ.into(),
                PERM_WRITE.into(),
                PERM_SHELL.into(),
            ]),
            allowed_commands: vec![
                "echo".into(),
                "cargo test".into(),
                "cmd /C echo".into(),
                "cmd".into(),
            ],
            agent_id: "a1".into(),
            task_id: "t1".into(),
        }
    }

    #[test]
    fn absolute_and_parent_paths_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let error = resolve_path(&ctx, "../escape").expect_err("parent must fail");
        assert_eq!(error.code, "path_escapes_workspace");
        let error = resolve_path(&ctx, "C:/Windows").expect_err("absolute must fail");
        assert_eq!(error.code, "path_escapes_workspace");
    }

    #[test]
    fn writes_reads_and_edits_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let out = invoke(
            &ctx,
            "write_file",
            &json!({"path": "src/main.rs", "content": "fn main() {}\n"}),
            None,
        )
        .unwrap();
        assert_eq!(out.changed_files, vec!["src/main.rs"]);
        let read = invoke(&ctx, "read_file", &json!({"path": "src/main.rs"}), None).unwrap();
        assert!(read.text.contains("fn main()"));
        let edit = invoke(
            &ctx,
            "edit_file",
            &json!({"path": "src/main.rs", "old_text": "fn main()", "new_text": "fn run()"}),
            None,
        )
        .unwrap();
        assert!(edit.text.contains("src/main.rs"));
    }

    #[test]
    fn permission_and_allowlist_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.permissions = PermissionSet::new(vec![PERM_READ.into()]);
        let denied = invoke(
            &ctx,
            "write_file",
            &json!({"path": "a.txt", "content": "x"}),
            None,
        )
        .unwrap_err();
        assert_eq!(denied.code, "permission_denied");

        ctx.permissions = PermissionSet::new(vec![PERM_SHELL.into()]);
        let blocked =
            invoke(&ctx, "run_command", &json!({"command": "rm -rf ."}), None).unwrap_err();
        assert_eq!(blocked.code, "command_not_allowlisted");
    }

    #[test]
    fn program_resolution_finds_windows_shims() {
        // `node` is a real .exe on every developer machine with this toolchain; the
        // resolver must not mangle a plain name.
        let resolved = resolve_program("node");
        assert!(!resolved.is_empty());
        assert!(
            std::path::Path::new(&resolved).is_file(),
            "resolved {resolved}"
        );
    }

    #[test]
    fn allowlisted_command_runs_in_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let out = invoke(
            &ctx,
            "run_command",
            &json!({"command": "echo hello", "timeout_ms": 10000}),
            None,
        )
        .unwrap();
        assert!(out.text.contains("hello"), "{}", out.text);
    }

    #[test]
    fn grep_walks_and_skips_heavy_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        let mut target = std::fs::File::create(dir.path().join("target/x.rs")).unwrap();
        writeln!(target, "needle").unwrap();
        std::fs::write(dir.path().join("src.rs"), "let needle = 1;\n").unwrap();
        let ctx = context(dir.path());
        let out = invoke(&ctx, "grep", &json!({"pattern": "needle"}), None).unwrap();
        assert!(out.text.contains("src.rs:1:"));
        assert!(!out.text.contains("target/x.rs"));
    }
}
