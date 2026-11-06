//! The execution side of world reads (port of `workflow-world-read.ts` + `workflow-git-world-read.ts`).
//!
//! Only three things live here: the arity and argument validation of each op, the enforcement of the caps (over the cap **rejects, never truncates**), and the
//! genuinely on-disk read-only observation. The pure `git.*` logic (fixed argv and parsing) is in `git.rs`.
//!
//! The porting differences from TS (recorded honestly):
//! - TS injects via `FileSystemPort` / `ExecutionPort`; the host has neither port, so this lands directly on
//!   `std::fs` and `std::process` (this module is the host's I/O boundary), using `spawn_blocking` for async waits.
//! - `files.glob` / `files.grep` use workspace traversal + in-process matching (TS uses a ripgrep port). The semantics follow
//!   the facade's promise: glob supports `*` / `?` / `**` / `[...]`, grep uses Rust `regex` syntax
//!   (ripgrep's core syntax; lookaround / backreference are unsupported). Symlinks are never followed --
//!   neither a soft link leaving the workspace nor a cycle should bring bytes into the journal.
//! - `files.read` has no TS counterpart with an explicit cap; this host adds a 4 MiB text read cap (reject rather than
//!   truncate), so that one read cannot blow up the journal and the model context.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nc_code_workflow::types::{WorkflowError, WorkflowErrorCode, WORLD_READ_CAPS};
use regex::Regex;
use serde_json::{json, Value};

use crate::workflow::git;

/// The text read cap for `files.read` (no explicit cap on the TS side; an explicit choice of this host, see the module comment).
pub const READ_MAX_BYTES: usize = 4 * 1024 * 1024;
/// The per-file read cap for `files.grep`: a larger file is usually generated or binary, so skip it instead of reading the whole thing into memory.
const GREP_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// The inline cap for `git.*` text output (TS's `GIT_TEXT_OUTPUT_BYTES`).
const GIT_TEXT_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// The safety timeout of the git commands themselves (these subcommands do not touch the network; 120s is only an anti-hang guard).
const GIT_TIMEOUT_MS: u64 = 120_000;

/// The base directory and the authorization surface a world read needs.
#[derive(Clone, Debug)]
pub struct WorldReadScope {
    /// The workspace root: path resolution, relativization and the git cwd all use it.
    pub workspace: PathBuf,
    /// The approved command set for `world.run`. **Absent means every world.run is rejected** (fail-closed).
    pub declared_run_commands: Option<BTreeSet<String>>,
}

impl WorldReadScope {
    pub fn new(
        workspace: impl Into<PathBuf>,
        declared_run_commands: Option<BTreeSet<String>>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            declared_run_commands,
        }
    }
}

/// Take the arguments by position from the positional argument array according to op, and execute one world read.
///
/// Declaring it `async` is deliberate: a throw from argument validation must become **that node's promise rejection**, instead of
/// escaping through the engine's admission path and leaving a node stuck in running forever.
pub async fn execute(
    scope: &WorldReadScope,
    op: &str,
    args: &[Value],
) -> Result<Value, WorkflowError> {
    match op {
        "glob" => {
            let pattern = string_args(op, args, &["pattern"])?.remove(0);
            let workspace = scope.workspace.clone();
            blocking(move || world_glob(&workspace, &pattern)).await
        }
        "read" => {
            let path = string_args(op, args, &["path"])?.remove(0);
            let workspace = scope.workspace.clone();
            blocking(move || world_read(&workspace, &path)).await
        }
        "grep" => {
            let values = optional_string_args(op, args, &["pattern"], &["glob"])?;
            let pattern = values[0].clone().expect("required pattern");
            let glob = values[1].clone();
            let workspace = scope.workspace.clone();
            blocking(move || world_grep(&workspace, &pattern, glob.as_deref())).await
        }
        "git-status" => {
            // Argument-less ops: an empty `names` means "exactly 0 arguments", and passing one more still fails loudly.
            string_args(op, args, &[])?;
            let workspace = scope.workspace.clone();
            blocking(move || git_status(&workspace)).await
        }
        "git-changed-files" => {
            let values = optional_string_args(op, args, &[], &["base"])?;
            let base = values[0].clone();
            let workspace = scope.workspace.clone();
            blocking(move || git_changed_files(&workspace, base.as_deref())).await
        }
        "git-diff" => {
            let values = optional_string_args(op, args, &[], &["base", "path"])?;
            let base = values[0].clone();
            let path = values[1].clone();
            let workspace = scope.workspace.clone();
            blocking(move || git_diff(&workspace, base.as_deref(), path.as_deref())).await
        }
        "git-log" => {
            let count = optional_count(op, args, "count")?;
            let workspace = scope.workspace.clone();
            blocking(move || git_log(&workspace, count)).await
        }
        "run" => {
            let request = world_run_args(args)?;
            let workspace = scope.workspace.clone();
            let declared = scope.declared_run_commands.clone();
            blocking(move || world_run(&workspace, declared.as_ref(), request)).await
        }
        other => Err(driver_error(format!(
            "Unsupported world-read op \"{other}\"."
        ))),
    }
}

// ——————————————————————————————— files.* ———————————————————————————————

/// `files.glob(pattern)`: workspace-relativized, lexicographic order, over the cap is rejected.
fn world_glob(workspace: &Path, pattern: &str) -> Result<Value, WorkflowError> {
    let matcher = compile_glob(pattern)?;
    let cap = WORLD_READ_CAPS.glob_max_files;
    let mut matches: Vec<String> = Vec::new();
    walk_files(workspace, &mut |relative| {
        if matcher.is_match(relative) {
            matches.push(relative.to_string());
            if matches.len() > cap {
                return Err(cap_exceeded(format!(
                    "files.glob: over {cap} files match (the cap). Narrow the pattern."
                )));
            }
        }
        Ok(())
    })?;
    // The journal stores this value: replaying the same commit must yield the same ordering, so the sort must not depend on directory traversal order.
    matches.sort();
    Ok(Value::Array(
        matches.into_iter().map(Value::String).collect(),
    ))
}

/// `files.read(path)`: first the lexical escape check, then a text read with a cap+1 probe.
fn world_read(workspace: &Path, arg: &str) -> Result<Value, WorkflowError> {
    let path = assert_within_workspace("read", workspace, arg)?;
    let metadata = std::fs::metadata(&path).map_err(|error| {
        driver_error(format!(
            "files.read: cannot read '{}': {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(driver_error(format!(
            "files.read: '{}' is not a regular file.",
            path.display()
        )));
    }
    let bytes = read_text_file_capped(&path, READ_MAX_BYTES)?;
    let content = String::from_utf8(bytes).map_err(|_| {
        driver_error(format!(
            "files.read: '{arg}' is not valid UTF-8 text. Read the bytes with a command \
             (world.run), or pick a text file."
        ))
    })?;
    Ok(Value::String(content))
}

/// `files.grep(pattern, glob?)`: match file by file, reject as soon as the hits exceed the cap, reject when the serialization exceeds 256KB.
fn world_grep(workspace: &Path, pattern: &str, glob: Option<&str>) -> Result<Value, WorkflowError> {
    let regex = Regex::new(pattern).map_err(|error| {
        driver_error(format!("files.grep: invalid pattern '{pattern}': {error}"))
    })?;
    let glob_matcher = match glob {
        None => None,
        Some("") => None,
        Some(glob) => Some(compile_glob(glob)?),
    };
    let cap = WORLD_READ_CAPS.grep_max_matches;
    let mut matches: Vec<Value> = Vec::new();
    let mut files = Vec::new();
    walk_files(workspace, &mut |relative| {
        files.push(relative.to_string());
        Ok(())
    })?;
    files.sort();
    for relative in files {
        if let Some(matcher) = &glob_matcher {
            if !matcher.is_match(&relative) {
                continue;
            }
        }
        let path = workspace.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > GREP_MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        // Invalid UTF-8 = binary: ripgrep skips those by default, and we skip them here too (no garbage lines emitted).
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if !regex.is_match(line) {
                continue;
            }
            matches.push(json!({
                "path": relative,
                "line": index + 1,
                "text": line,
            }));
            if matches.len() > cap {
                return Err(cap_exceeded(format!(
                    "files.grep: over {cap} matches (the cap). Narrow the pattern or add a glob."
                )));
            }
        }
    }
    let serialized = serde_json::to_string(&matches).unwrap_or_default();
    if serialized.len() > WORLD_READ_CAPS.grep_max_serialized_bytes {
        return Err(cap_exceeded(format!(
            "files.grep: result is {} bytes, over the {}-byte cap. Narrow the pattern or add a glob.",
            serialized.len(),
            WORLD_READ_CAPS.grep_max_serialized_bytes
        )));
    }
    Ok(Value::Array(matches))
}

// ——————————————————————————————— git.* ———————————————————————————————

fn run_git(
    workspace: &Path,
    op: &str,
    argv: &[String],
    max_bytes: usize,
) -> Result<Vec<u8>, WorkflowError> {
    let outcome = run_command(
        "git",
        argv,
        workspace,
        Some(GIT_TIMEOUT_MS),
        max_bytes,
        max_bytes,
    );
    if let Some(error) = outcome.spawn_error {
        return Err(driver_error(format!(
            "world-read {op}: cannot run git: {error}"
        )));
    }
    if outcome.timed_out {
        return Err(driver_error(format!(
            "world-read {op}: git {} timed out after {GIT_TIMEOUT_MS}ms.",
            argv.join(" ")
        )));
    }
    let stderr = String::from_utf8_lossy(&outcome.stderr.bytes);
    let stdout = String::from_utf8_lossy(&outcome.stdout.bytes);
    if outcome.exit_code != Some(0) {
        let detail = first_line(&stderr);
        let detail = if detail.is_empty() {
            first_line(&stdout)
        } else {
            detail
        };
        return Err(driver_error(format!(
            "git {} failed ({}, exit={}): {detail}",
            argv.join(" "),
            if outcome.timed_out {
                "timed_out"
            } else {
                "failed"
            },
            outcome
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "n/a".to_string())
        )));
    }
    // A truncated porcelain / path list parses into a false observation of "no error but a few rows missing"; better to reject.
    if outcome.stdout.total > max_bytes {
        return Err(cap_exceeded(format!(
            "world-read {op}: git output is {} bytes, over the {max_bytes}-byte cap. Narrow the \
             request.",
            outcome.stdout.total
        )));
    }
    Ok(outcome.stdout.bytes)
}

fn git_workspace_prefix(workspace: &Path, op: &str) -> Result<String, WorkflowError> {
    let argv: Vec<String> = git::GIT_SHOW_PREFIX_ARGV
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let stdout = run_git(workspace, op, &argv, GIT_TEXT_OUTPUT_BYTES)?;
    Ok(git::parse_git_show_prefix(&String::from_utf8_lossy(
        &stdout,
    )))
}

fn git_status(workspace: &Path) -> Result<Value, WorkflowError> {
    let prefix = git_workspace_prefix(workspace, "git-status")?;
    let argv: Vec<String> = git::GIT_STATUS_ARGV
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let stdout = run_git(workspace, "git-status", &argv, GIT_TEXT_OUTPUT_BYTES)?;
    let status = git::parse_git_status_porcelain_v2(&String::from_utf8_lossy(&stdout), &prefix)?;
    Ok(status.to_json())
}

fn git_changed_files(workspace: &Path, base: Option<&str>) -> Result<Value, WorkflowError> {
    // Build the argv first: an invalid base should be rejected **before running any git**.
    let plans = git::git_changed_files_plan(base)?;
    let prefix = git_workspace_prefix(workspace, "git-changed-files")?;
    let mut paths: Vec<String> = Vec::new();
    for plan in plans {
        let stdout = run_git(workspace, "git-changed-files", &plan, GIT_TEXT_OUTPUT_BYTES)?;
        paths.extend(git::parse_git_path_list(
            &String::from_utf8_lossy(&stdout),
            &prefix,
        )?);
    }
    // Union with dedup and sort: the journal stores this value, so it cannot depend on which of the two commands returns first.
    let unique: BTreeSet<String> = paths.into_iter().collect();
    Ok(Value::Array(
        unique.into_iter().map(Value::String).collect(),
    ))
}

fn git_diff(
    workspace: &Path,
    base: Option<&str>,
    path: Option<&str>,
) -> Result<Value, WorkflowError> {
    let cap = WORLD_READ_CAPS.git_diff_max_bytes;
    let argv = git::git_diff_argv(base, path)?;
    // cap + 1: the same idiom as grep -- "exactly cap bytes" and "truncated" must stay distinguishable.
    let stdout = run_git(workspace, "git-diff", &argv, cap + 1)?;
    if stdout.len() > cap {
        return Err(cap_exceeded(format!(
            "git.diff: output is {} bytes, over the {cap}-byte cap. Pass a path to narrow.",
            stdout.len()
        )));
    }
    Ok(Value::String(String::from_utf8_lossy(&stdout).to_string()))
}

fn git_log(workspace: &Path, count: u32) -> Result<Value, WorkflowError> {
    let argv = git::git_log_argv(count);
    let stdout = run_git(workspace, "git-log", &argv, GIT_TEXT_OUTPUT_BYTES)?;
    let commits = git::parse_git_log(&String::from_utf8_lossy(&stdout))?;
    Ok(Value::Array(
        commits.iter().map(git::GitCommitResult::to_json).collect(),
    ))
}

// ——————————————————————————————— world.run ———————————————————————————————

struct WorldRunRequest {
    cmd: String,
    argv: Vec<String>,
    timeout_ms: u64,
}

/// `world.run(cmd, args?, opts?)`: journal-ized command execution.
///
/// Three deliberate differences from `git.*`: a non-zero exit is a **value** (the normal path of a gated loop should not travel through exceptional control flow);
/// cmd is re-validated fail-closed; the timeout is specified by the script (no upper clamp, cancel remains the last control).
fn world_run(
    workspace: &Path,
    declared: Option<&BTreeSet<String>>,
    request: WorldRunRequest,
) -> Result<Value, WorkflowError> {
    match declared {
        Some(declared) if declared.contains(&request.cmd) => {}
        _ => {
            // Any cmd reaching here should already have passed compile-time literal collection and confirmation; this only guards against wiring errors.
            return Err(driver_error(format!(
                "world.run: command '{}' is not in the declared set of commands (a wiring error).",
                request.cmd
            )));
        }
    }
    let stdout_cap = WORLD_READ_CAPS.run_stdout_max_bytes;
    let stderr_cap = WORLD_READ_CAPS.run_stderr_max_bytes;
    let outcome = run_command(
        &request.cmd,
        &request.argv,
        workspace,
        Some(request.timeout_ms),
        stdout_cap,
        stderr_cap,
    );
    if let Some(error) = outcome.spawn_error {
        return Err(driver_error(format!(
            "world.run '{}' did not run to completion (spawn_error): {error}",
            request.cmd
        )));
    }
    if outcome.timed_out {
        return Err(driver_error(format!(
            "world.run '{}' timed out after {}ms. Raise opts.timeoutMs or narrow the work.",
            request.cmd, request.timeout_ms
        )));
    }
    // Killed by a signal (Unix) leaves exitCode absent: that means "did not finish", not exit 0.
    let Some(exit_code) = outcome.exit_code else {
        return Err(driver_error(format!(
            "world.run '{}' did not run to completion (terminated before an exit code was \
             reported).",
            request.cmd
        )));
    };
    if outcome.stdout.total > stdout_cap {
        return Err(cap_exceeded(format!(
            "world.run '{}': stdout is {} bytes, over the {stdout_cap}-byte cap. Quiet the output \
             (e.g. --quiet), or write a file and files.read a summary.",
            request.cmd, outcome.stdout.total
        )));
    }
    if outcome.stderr.total > stderr_cap {
        return Err(cap_exceeded(format!(
            "world.run '{}': stderr is {} bytes, over the {stderr_cap}-byte cap. Reduce the \
             diagnostics, or write a file and files.read a summary.",
            request.cmd, outcome.stderr.total
        )));
    }
    Ok(json!({
        "exitCode": exit_code,
        "stdout": String::from_utf8_lossy(&outcome.stdout.bytes),
        "stderr": String::from_utf8_lossy(&outcome.stderr.bytes),
    }))
}

/// The argument shape of `world.run`: `[cmd: string, args?: string[], opts?: { timeoutMs?: number }]`.
fn world_run_args(args: &[Value]) -> Result<WorldRunRequest, WorkflowError> {
    if args.is_empty() || args.len() > 3 {
        return Err(driver_error(format!(
            "world.run takes 1 to 3 arguments (cmd, args?, opts?), got {}.",
            args.len()
        )));
    }
    let cmd = require_string_arg("run", &args[0], "cmd", 0)?;
    let mut argv = Vec::new();
    if let Some(raw) = args.get(1) {
        let Some(items) = raw.as_array() else {
            return Err(driver_error(format!(
                "world.run: argument 2 (args) must be an array of strings, got {}.",
                describe_arg(raw)
            )));
        };
        for (index, item) in items.iter().enumerate() {
            let Some(value) = item.as_str() else {
                return Err(driver_error(format!(
                    "world.run: args[{index}] must be a string, got {}. Stringify values.",
                    describe_arg(item)
                )));
            };
            argv.push(value.to_string());
        }
    }
    let mut timeout_ms = WORLD_READ_CAPS.run_default_timeout_ms;
    if let Some(raw) = args.get(2) {
        if !raw.is_object() {
            return Err(driver_error(format!(
                "world.run: arg 3 (opts) must be an options object or omitted, got {}.",
                describe_arg(raw)
            )));
        }
        if let Some(raw_timeout) = raw.get("timeoutMs") {
            let Some(parsed) = integer_at_least_one(raw_timeout) else {
                return Err(driver_error(format!(
                    "world.run: opts.timeoutMs must be an integer >= 1, got {}.",
                    describe_arg(raw_timeout)
                )));
            };
            // Deliberately no upper clamp: designed for genuinely long-running tests; cancel is the last control.
            timeout_ms = parsed;
        }
    }
    Ok(WorldRunRequest {
        cmd,
        argv,
        timeout_ms,
    })
}

// ——————————————————————————————— argument validation ———————————————————————————————

fn world_read_string_args(
    op: &str,
    args: &[Value],
    names: &[&str],
) -> Result<Vec<String>, WorkflowError> {
    if args.len() != names.len() {
        return Err(driver_error(format!(
            "world-read {op} takes {} arguments ({}), got {}.",
            names.len(),
            names.join(", "),
            args.len()
        )));
    }
    names
        .iter()
        .enumerate()
        .map(|(index, name)| require_string_arg(op, &args[index], name, index))
        .collect()
}

fn string_args(op: &str, args: &[Value], names: &[&str]) -> Result<Vec<String>, WorkflowError> {
    world_read_string_args(op, args, names)
}

/// Trailing-optional variant: `required` is the mandatory prefix and `optional` the optional tail. An explicit `undefined`/`null`
/// is writable in the script (`git.diff(undefined, "a.ts")`) and is read as "absent".
fn optional_string_args(
    op: &str,
    args: &[Value],
    required: &[&str],
    optional: &[&str],
) -> Result<Vec<Option<String>>, WorkflowError> {
    let max = required.len() + optional.len();
    if args.len() < required.len() || args.len() > max {
        let range = if required.len() == max {
            max.to_string()
        } else {
            format!("{}~{}", required.len(), max)
        };
        let rendered: Vec<String> = required
            .iter()
            .map(|name| (*name).to_string())
            .chain(optional.iter().map(|name| format!("{name}?")))
            .collect();
        return Err(driver_error(format!(
            "world-read {op} takes {range} arguments ({}), got {}.",
            rendered.join(", "),
            args.len()
        )));
    }
    let mut out = Vec::with_capacity(max);
    for (index, name) in required.iter().enumerate() {
        out.push(Some(require_string_arg(op, &args[index], name, index)?));
    }
    for (offset, name) in optional.iter().enumerate() {
        let index = required.len() + offset;
        match args.get(index) {
            None | Some(Value::Null) => out.push(None),
            Some(value) => out.push(Some(require_string_arg(op, value, name, index)?)),
        }
    }
    Ok(out)
}

/// `git.log(count?)`: 0 or 1 positive integer, not above the cap. Over the cap **rejects** instead of silently clamping to it.
fn optional_count(op: &str, args: &[Value], name: &str) -> Result<u32, WorkflowError> {
    let max = WORLD_READ_CAPS.git_log_max_count;
    if args.len() > 1 {
        return Err(driver_error(format!(
            "world-read {op} takes at most 1 argument ({name}?), got {}.",
            args.len()
        )));
    }
    let Some(raw) = args.first() else {
        return Ok(WORLD_READ_CAPS.git_log_default_count);
    };
    if raw.is_null() {
        return Ok(WORLD_READ_CAPS.git_log_default_count);
    }
    let Some(parsed) = integer_at_least_one(raw) else {
        return Err(driver_error(format!(
            "world-read {op}: argument '{name}' must be an integer >= 1, got {}.",
            describe_arg(raw)
        )));
    };
    if parsed > u64::from(max) {
        return Err(driver_error(format!(
            "world-read {op}: argument '{name}'={parsed} is over the cap of {max}; pass at most {max}."
        )));
    }
    Ok(parsed as u32)
}

fn require_string_arg(
    op: &str,
    value: &Value,
    name: &str,
    index: usize,
) -> Result<String, WorkflowError> {
    value.as_str().map(str::to_string).ok_or_else(|| {
        driver_error(format!(
            "world-read {op}: arg {} ({name}) must be a string, got {}.",
            index + 1,
            describe_arg(value)
        ))
    })
}

/// An integer JSON argument: TS's `Number.isInteger` accepts `1` and also `1.0`, and here both spellings are accepted too.
fn integer_at_least_one(value: &Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return (value >= 1).then_some(value);
    }
    let float = value.as_f64()?;
    if float.fract() == 0.0 && float >= 1.0 && float <= u64::MAX as f64 {
        Some(float as u64)
    } else {
        None
    }
}

fn describe_arg(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn driver_error(message: impl Into<String>) -> WorkflowError {
    WorkflowError::new(WorkflowErrorCode::DriverError, message)
}

fn cap_exceeded(message: impl Into<String>) -> WorkflowError {
    WorkflowError::new(WorkflowErrorCode::WorldReadCapExceeded, message)
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

// ——————————————————————————————— paths ———————————————————————————————

/// Resolve a script-supplied relative path into an absolute one, and lexically verify it is still inside the workspace.
///
/// The test is "after absolutizing, the workspace is still a prefix" (compared component by component): `..` only leaves traces outside the prefix,
/// and a cross-drive join lands elsewhere outright. Lexical checks only, symlinks are not followed -- world reads accept this known boundary
/// (the bytes read are exactly the value the script itself saw); artifact publishing does a separate realpath recheck.
pub(crate) fn resolve_within_workspace(cwd: &Path, arg: &str) -> Option<PathBuf> {
    let root = std::path::absolute(cwd).ok()?;
    let resolved = std::path::absolute(root.join(arg)).ok()?;
    if resolved.starts_with(&root) {
        Some(resolved)
    } else {
        None
    }
}

fn assert_within_workspace(op: &str, cwd: &Path, arg: &str) -> Result<PathBuf, WorkflowError> {
    resolve_within_workspace(cwd, arg).ok_or_else(|| {
        driver_error(format!(
            "world-read {op}: path '{arg}' is outside the workspace; pass a path inside it."
        ))
    })
}

/// Normalize the paths the port returns into workspace-relative, forward-slash form (the journal and the model hints both use this shape).
pub(crate) fn to_workspace_relative(cwd: &Path, path: &Path) -> String {
    let root = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match absolute.strip_prefix(&root) {
        Ok(relative) => relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

// ——————————————————————————————— workspace traversal / glob ———————————————————————————————

/// Walk the regular files in the workspace; skip `.git` and symlinks (never followed, guarding against cycles and out-of-bounds reads).
fn walk_files(
    root: &Path,
    visit: &mut dyn FnMut(&str) -> Result<(), WorkflowError>,
) -> Result<(), WorkflowError> {
    fn walk(
        dir: &Path,
        root: &Path,
        visit: &mut dyn FnMut(&str) -> Result<(), WorkflowError>,
    ) -> Result<(), WorkflowError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(driver_error(format!(
                    "cannot list '{}': {error}",
                    dir.display()
                )))
            }
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if entry.file_name() == std::ffi::OsStr::new(".git") {
                continue;
            }
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                walk(&entry.path(), root, visit)?;
            } else if file_type.is_file() {
                visit(&to_workspace_relative(root, &entry.path()))?;
            }
        }
        Ok(())
    }
    walk(root, root, visit)
}

/// Compile a workspace-relative glob (`*` / `?` / `**` / `[...]`). An absolute pattern is rejected outright:
/// the facade promises workspace-relative paths, and an absolute pattern would make the journal value drift per machine.
fn compile_glob(pattern: &str) -> Result<Regex, WorkflowError> {
    if pattern.is_empty() {
        return Err(driver_error(
            "files.glob: pattern must not be empty. Pass a glob such as \"**/*.ts\".",
        ));
    }
    if pattern.starts_with('/') || pattern.starts_with('\\') || looks_absolute(pattern) {
        return Err(driver_error(format!(
            "files.glob: pattern '{pattern}' is absolute; pass a workspace-relative glob such as \
             \"**/*.ts\"."
        )));
    }
    // Unify separators to `/`: a model writing a windows-style relative path is common, and a glob should give the same answer on all three platforms.
    let pattern = pattern.replace('\\', "/");
    let pattern = pattern.strip_prefix("./").unwrap_or(&pattern);
    let mut regex = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        match chars[index] {
            '*' => {
                if chars.get(index + 1) == Some(&'*') {
                    index += 2;
                    if chars.get(index) == Some(&'/') {
                        regex.push_str("(?:.*/)?");
                        index += 1;
                    } else {
                        regex.push_str(".*");
                    }
                } else {
                    regex.push_str("[^/]*");
                    index += 1;
                }
            }
            '?' => {
                regex.push_str("[^/]");
                index += 1;
            }
            '[' => {
                // Character classes are handed to regex as-is; an unclosed `[` is treated as a literal.
                let mut end = index + 1;
                while end < chars.len() && chars[end] != ']' {
                    end += 1;
                }
                if end == chars.len() {
                    regex.push_str("\\[");
                    index += 1;
                } else {
                    let class: String = chars[index..=end].iter().collect();
                    // `[!a]` is glob's negation; regex spells it `[^a]`.
                    let class = class.replacen("[!", "[^", 1);
                    regex.push_str(&class);
                    index = end + 1;
                }
            }
            character => {
                regex.push_str(&regex::escape(&character.to_string()));
                index += 1;
            }
        }
    }
    regex.push('$');
    Regex::new(&regex).map_err(|error| {
        driver_error(format!(
            "files.glob: pattern '{pattern}' is not usable: {error}"
        ))
    })
}

fn looks_absolute(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with('\\')
        || (value.len() >= 2
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':')
}

// ——————————————————————————————— process execution ———————————————————————————————

struct Captured {
    bytes: Vec<u8>,
    /// The total bytes actually read from the stream (the other half of the cap+1 probe: telling "exactly cap" from "over cap").
    total: usize,
}

struct CommandOutcome {
    spawn_error: Option<String>,
    timed_out: bool,
    exit_code: Option<i32>,
    stdout: Captured,
    stderr: Captured,
}

/// Run one argv command (never through a shell), capture both streams with a cap, and kill the process tree on timeout.
///
/// Memory-bounded: each stream keeps at most `cap + 1` bytes and only counts the rest; the decision is made upstream from `total`,
/// so "over the cap" is a rejection rather than a truncation.
fn run_command(
    file: &str,
    args: &[String],
    cwd: &Path,
    timeout_ms: Option<u64>,
    stdout_cap: usize,
    stderr_cap: usize,
) -> CommandOutcome {
    use std::process::{Command, Stdio};
    let mut command = Command::new(file);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return CommandOutcome {
                spawn_error: Some(error.to_string()),
                timed_out: false,
                exit_code: None,
                stdout: Captured {
                    bytes: Vec::new(),
                    total: 0,
                },
                stderr: Captured {
                    bytes: Vec::new(),
                    total: 0,
                },
            }
        }
    };
    let pid = child.id();
    let stdout = child.stdout.take().map(|pipe| {
        let cap = stdout_cap;
        std::thread::spawn(move || read_capped_stream(pipe, cap))
    });
    let stderr = child.stderr.take().map(|pipe| {
        let cap = stderr_cap;
        std::thread::spawn(move || read_capped_stream(pipe, cap))
    });
    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut timed_out = false;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(_) => break None,
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            timed_out = true;
            // The bash tool uses the same tree-kill knife: killing only the direct child would leave grandchildren behind.
            let _ = nc_code_proc::kill_tree(pid, 2_000);
            let _ = child.kill();
            break child.wait().ok().and_then(|status| status.code());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout
        .and_then(|handle| handle.join().ok())
        .unwrap_or(Captured {
            bytes: Vec::new(),
            total: 0,
        });
    let stderr = stderr
        .and_then(|handle| handle.join().ok())
        .unwrap_or(Captured {
            bytes: Vec::new(),
            total: 0,
        });
    CommandOutcome {
        spawn_error: None,
        timed_out,
        exit_code,
        stdout,
        stderr,
    }
}

fn read_capped_stream(mut reader: impl Read, cap: usize) -> Captured {
    let mut bytes = Vec::new();
    let mut total = 0usize;
    let mut buffer = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                total = total.saturating_add(read);
                let room = cap.saturating_add(1).saturating_sub(bytes.len());
                if room > 0 {
                    bytes.extend_from_slice(&buffer[..read.min(room)]);
                }
            }
            Err(_) => break,
        }
    }
    Captured { bytes, total }
}

/// Read a file with a cap (the cap+1 probe: above the cap yields `WorldReadCapExceeded`, never truncated).
fn read_file_capped(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref().take(cap as u64 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_text_file_capped(path: &Path, cap: usize) -> Result<Vec<u8>, WorkflowError> {
    let bytes = read_file_capped(path, cap).map_err(|error| {
        driver_error(format!(
            "files.read: cannot read '{}': {error}",
            path.display()
        ))
    })?;
    if bytes.len() > cap {
        return Err(cap_exceeded(format!(
            "files.read: '{}' is over the {cap}-byte cap. Read a slice with files.grep, or split \
             the file.",
            path.display()
        )));
    }
    Ok(bytes)
}

async fn blocking<T, F>(work: F) -> Result<T, WorkflowError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, WorkflowError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| driver_error(format!("world-read task failed to run: {error}")))?
}

#[cfg(test)]
mod tests;
