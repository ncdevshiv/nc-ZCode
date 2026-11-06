//! The **pure** half of the git.* world reads (port of `workflow-git-world-read.ts`): argument validation,
//! fixed argv construction, and parsing of porcelain-v2 / log / path lists.
//!
//! Read-only is obtained **by construction**: this module can only assemble the argv of the five permitted subcommands, and no path lets a script-supplied
//! string become a subcommand or an option (a base ref passes a strict charset, and a path must be workspace-relative and placed after `--`).
//! The side effects (actually spawning a git) live in `world_read.rs`.
//!
//! Two output contracts that run throughout:
//! 1. `-z`: NUL-separated, paths byte-for-byte verbatim (without `-z` git C-quote-escapes non-ASCII paths,
//!    and a file name containing a newline also breaks line-based splitting).
//! 2. Paths are always handed out **workspace-relative**: the wire always yields repo-root-relative (`ls-files` adds `--full-name`),
//!    which is then stripped with the prefix from `rev-parse --show-prefix`. The patch text of `git.diff` cannot have its prefix stripped afterwards,
//!    so that one command aligns through git's own `--relative`.

use std::sync::OnceLock;

use nc_code_workflow::types::{WorkflowError, WorkflowErrorCode};
use regex::Regex;
use serde_json::{json, Value};

/// The return shape of `git.status()` (the facade's `GitStatus`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitStatusResult {
    pub branch: Option<String>,
    pub clean: bool,
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub untracked: Vec<String>,
}

impl GitStatusResult {
    pub fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        if let Some(branch) = &self.branch {
            object.insert("branch".to_string(), json!(branch));
        }
        object.insert("clean".to_string(), json!(self.clean));
        object.insert("staged".to_string(), json!(self.staged));
        object.insert("unstaged".to_string(), json!(self.unstaged));
        object.insert("untracked".to_string(), json!(self.untracked));
        Value::Object(object)
    }
}

/// One record of `git.log()` (the facade's `GitCommit`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCommitResult {
    pub hash: String,
    pub subject: String,
    pub author: String,
    pub date: String,
}

impl GitCommitResult {
    pub fn to_json(&self) -> Value {
        json!({
            "hash": self.hash,
            "subject": self.subject,
            "author": self.author,
            "date": self.date,
        })
    }
}

/// The prefix of the workspace relative to the repo root (an empty string at the repo root).
pub const GIT_SHOW_PREFIX_ARGV: &[&str] = &["rev-parse", "--show-prefix"];

/// The fixed argv of `git status`; `-- .` keeps the observation scope inside the workspace.
pub const GIT_STATUS_ARGV: &[&str] = &["status", "--porcelain=v2", "-z", "--branch", "--", "."];

/// The argv for untracked files: `--exclude-standard` respects .gitignore, `--full-name` pulls paths to repo-root-relative.
const GIT_UNTRACKED_ARGV: &[&str] = &[
    "ls-files",
    "--others",
    "--exclude-standard",
    "-z",
    "--full-name",
    "--",
    ".",
];

/// The workspace pathspec: git.* and files.* observe the same world (otherwise `changedFiles() → files.read()`,
/// the most obvious two-line combination, would become a trap).
const WORKSPACE_PATHSPEC: &str = ".";

/// The field format of `git log`: NUL-separated hash / subject / author name / author date (ISO-8601).
const GIT_LOG_PRETTY: &str = "--pretty=format:%H%x00%s%x00%an%x00%aI";

/// The strict charset of a base ref: the first character must be a letter/digit. Forbidding a leading `-` is the only security-relevant part
/// (`-foo` would be taken by git as an option); forbidding `.` incidentally blocks `.` / `..`.
fn git_ref_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._/@^~-]*$").expect("static git ref pattern")
    })
}

fn driver_error(message: impl Into<String>) -> WorkflowError {
    WorkflowError::new(WorkflowErrorCode::DriverError, message)
}

/// Validate a base ref and return the original string. The error message explains **why** it was rejected (the side writing the script must rewrite accordingly).
pub fn validate_git_ref(op: &str, value: &str) -> Result<String, WorkflowError> {
    if value.is_empty() {
        return Err(driver_error(format!(
            "world-read {op}: base must not be empty. Pass a ref such as \"main\" or omit it."
        )));
    }
    if value.starts_with('-') {
        return Err(driver_error(format!(
            "world-read {op}: base '{value}' starts with '-', which git would read as an option, \
             not a ref. Pass a plain ref."
        )));
    }
    if value.contains("..") {
        return Err(driver_error(format!(
            "world-read {op}: base '{value}' is a '..' range; only a single ref is accepted. Pass \
             one ref."
        )));
    }
    if !git_ref_pattern().is_match(value) {
        return Err(driver_error(format!(
            "world-read {op}: base '{value}' is not a valid ref. A ref starts with a letter or \
             digit, followed by letters, digits and . _ / @ ^ ~ - (no whitespace or syntax \
             characters such as {{}} :)."
        )));
    }
    Ok(value.to_string())
}

/// Absolute-path detection that **does not depend on the current platform** (the posix version cannot recognize `C:\x`).
fn looks_absolute(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with('\\')
        || (value.len() >= 2
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':')
}

/// Validate a workspace-relative path and return the normalized string (separators unified to `/`).
/// All three rules are lexical convergence: no absolute paths, nothing starting with `-`, no `..` segments.
pub fn validate_git_path(op: &str, value: &str) -> Result<String, WorkflowError> {
    if value.is_empty() {
        return Err(driver_error(format!(
            "world-read {op}: path must not be empty. Pass a workspace-relative path or omit it."
        )));
    }
    if value.starts_with('-') {
        return Err(driver_error(format!(
            "world-read {op}: path '{value}' starts with '-', which git could read as an option. \
             Pass a path that does not start with '-'."
        )));
    }
    if looks_absolute(value) {
        return Err(driver_error(format!(
            "world-read {op}: path '{value}' is absolute; only workspace-relative paths are \
             accepted. Pass the path relative to the workspace root."
        )));
    }
    // Normalization must come **before** the `..` check, otherwise `..\x` slips through the segment split.
    let normalized = value.replace('\\', "/");
    if normalized.split('/').any(|segment| segment == "..") {
        return Err(driver_error(format!(
            "world-read {op}: path '{value}' contains a '..' segment and would leave the \
             workspace. Pass a path inside the workspace."
        )));
    }
    Ok(normalized)
}

/// The commands `git.changedFiles(base?)` has to run. Without a base it is the union of the "tracked diff" and the "untracked list";
/// with a base only the tracked history is compared.
pub fn git_changed_files_plan(base: Option<&str>) -> Result<Vec<Vec<String>>, WorkflowError> {
    let reference = match base {
        None => "HEAD".to_string(),
        Some(base) => validate_git_ref("git-changed-files", base)?,
    };
    let tracked = vec![
        "diff".to_string(),
        "--name-only".to_string(),
        "-z".to_string(),
        reference,
        "--".to_string(),
        WORKSPACE_PATHSPEC.to_string(),
    ];
    if base.is_some() {
        return Ok(vec![tracked]);
    }
    Ok(vec![
        tracked,
        GIT_UNTRACKED_ARGV
            .iter()
            .map(|arg| arg.to_string())
            .collect(),
    ])
}

/// The argv of `git.diff(base?, path?)`. The path always sits after `--`; `--relative` aligns the patch baseline
/// with the workspace (the patch headers are written by git and cannot have prefixes stripped afterwards).
pub fn git_diff_argv(base: Option<&str>, path: Option<&str>) -> Result<Vec<String>, WorkflowError> {
    let reference = match base {
        None => "HEAD".to_string(),
        Some(base) => validate_git_ref("git-diff", base)?,
    };
    let pathspec = match path {
        None => WORKSPACE_PATHSPEC.to_string(),
        Some(path) => validate_git_path("git-diff", path)?,
    };
    Ok(vec![
        "diff".to_string(),
        "--relative".to_string(),
        reference,
        "--".to_string(),
        pathspec,
    ])
}

/// The argv of `git.log(count)`. Deliberately without a pathspec: a commit is a repository-level object.
pub fn git_log_argv(count: u32) -> Vec<String> {
    vec![
        "log".to_string(),
        format!("-n{count}"),
        GIT_LOG_PRETTY.to_string(),
    ]
}

/// The output of `rev-parse --show-prefix` → the prefix string (an empty string at the repo root).
pub fn parse_git_show_prefix(stdout: &str) -> String {
    stdout.trim_end_matches(['\r', '\n']).to_string()
}

/// Split `-z` output into segments: split on NUL and drop empty segments (the trailing terminator leaves one).
fn nul_segments(stdout: &str) -> Vec<&str> {
    stdout
        .split('\0')
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Strip the workspace prefix. `-- .` already guaranteed the scope, so still being outside the prefix here means the assumption collapsed -- fail loudly,
/// rather than handing the script a path with `../` for it to `files.read`.
fn strip_workspace_prefix(prefix: &str, path: &str) -> Result<String, WorkflowError> {
    if prefix.is_empty() {
        return Ok(path.to_string());
    }
    if let Some(stripped) = path.strip_prefix(prefix) {
        return Ok(stripped.to_string());
    }
    Err(driver_error(format!(
        "git reported path '{path}' outside the workspace prefix '{prefix}'. The pathspec should \
         have scoped the read to the workspace, so the scoping is broken; report this as a bug."
    )))
}

/// Parse `git status --porcelain=v2 -z --branch -- .`.
///
/// The first character of a segment is the record type: `#` header info, `1` ordinary change, `2` rename/copy, `u` unmerged, `?` untracked.
/// The two XY digits of `1`/`2` are the staging-area and workspace states respectively, `.` meaning unchanged. A rename/copy entry occupies two segments
/// (the new path + the original path), and the original path is not handed out.
pub fn parse_git_status_porcelain_v2(
    stdout: &str,
    prefix: &str,
) -> Result<GitStatusResult, WorkflowError> {
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    let mut branch = None;

    let segments = nul_segments(stdout);
    let mut index = 0usize;
    while index < segments.len() {
        let segment = segments[index];
        if let Some(head) = segment.strip_prefix("# branch.head ") {
            // At a detached HEAD git writes the literal "(detached)": branch is **absent**, not present with a placeholder string.
            if head != "(detached)" {
                branch = Some(head.to_string());
            }
            index += 1;
            continue;
        }
        if segment.starts_with('#') {
            index += 1;
            continue;
        }
        let kind = segment.chars().next();
        match kind {
            Some('1') | Some('2') => {
                let xy: Vec<char> = segment.split(' ').nth(1).unwrap_or("..").chars().collect();
                let path = field_tail(segment, if kind == Some('1') { 8 } else { 9 });
                if kind == Some('2') {
                    index += 1; // The original path occupies the whole segment immediately following.
                }
                index += 1;
                if path.is_empty() {
                    continue;
                }
                let relative = strip_workspace_prefix(prefix, path)?;
                if xy.first().is_some_and(|state| *state != '.') {
                    staged.push(relative.clone());
                }
                if xy.get(1).is_some_and(|state| *state != '.') {
                    unstaged.push(relative);
                }
            }
            Some('u') => {
                let path = field_tail(segment, 10);
                if !path.is_empty() {
                    unstaged.push(strip_workspace_prefix(prefix, path)?);
                }
                index += 1;
            }
            Some('?') => {
                let path = field_tail(segment, 1);
                if !path.is_empty() {
                    untracked.push(strip_workspace_prefix(prefix, path)?);
                }
                index += 1;
            }
            // `!` and unrecognized segments: skip.
            _ => index += 1,
        }
    }

    let clean = staged.is_empty() && unstaged.is_empty() && untracked.is_empty();
    Ok(GitStatusResult {
        branch,
        clean,
        staged: sorted_unique(staged),
        unstaged: sorted_unique(unstaged),
        untracked: sorted_unique(untracked),
    })
}

/// Parse the output of `--pretty=format:%H%x00%s%x00%an%x00%aI`: one record per line, fields NUL-separated.
/// `%s` by definition contains no newline, so splitting records by newline is reliable here.
pub fn parse_git_log(stdout: &str) -> Result<Vec<GitCommitResult>, WorkflowError> {
    let mut commits = Vec::new();
    for raw in stdout.split('\n') {
        let record = raw.strip_suffix('\r').unwrap_or(raw);
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split('\0').collect();
        if fields.len() != 4 {
            return Err(driver_error(format!(
                "world-read git-log: cannot parse the output; a record should have 4 NUL-separated \
                 fields, got {}. Retry, and report this as a bug if it persists.",
                fields.len()
            )));
        }
        commits.push(GitCommitResult {
            hash: fields[0].to_string(),
            subject: fields[1].to_string(),
            author: fields[2].to_string(),
            date: fields[3].to_string(),
        });
    }
    Ok(commits)
}

/// Split the output of `--name-only -z` / `ls-files -z` into a list of **workspace-relative** paths.
pub fn parse_git_path_list(stdout: &str, prefix: &str) -> Result<Vec<String>, WorkflowError> {
    nul_segments(stdout)
        .into_iter()
        .map(|path| strip_workspace_prefix(prefix, path))
        .collect()
}

/// The whole remainder starting at the `index`-th space-separated field (in porcelain v2 the path is the last segment and may contain spaces).
fn field_tail(line: &str, index: usize) -> &str {
    let mut at = 0usize;
    for _ in 0..index {
        match line[at..].find(' ') {
            Some(next) => at += next + 1,
            None => return "",
        }
    }
    &line[at..]
}

/// Deduplicate and sort: this is the value the journal stores, and it must depend only on content, not on git's output order.
fn sorted_unique(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_reject_options_ranges_and_syntax() {
        assert!(validate_git_ref("git-diff", "main").is_ok());
        assert!(validate_git_ref("git-diff", "origin/main").is_ok());
        assert!(validate_git_ref("git-diff", "v1.2.3~1").is_ok());
        assert!(validate_git_ref("git-diff", "").is_err());
        assert!(validate_git_ref("git-diff", "-foo").is_err());
        assert!(validate_git_ref("git-diff", "a..b").is_err());
        assert!(validate_git_ref("git-diff", "main^{}").is_err());
        assert!(validate_git_ref("git-diff", "has space").is_err());
    }

    #[test]
    fn paths_stay_inside_the_workspace() {
        assert_eq!(
            validate_git_path("git-diff", "src\\a.ts").unwrap(),
            "src/a.ts"
        );
        assert!(validate_git_path("git-diff", "../x").is_err());
        assert!(validate_git_path("git-diff", "..\\x").is_err());
        assert!(validate_git_path("git-diff", "C:/x").is_err());
        assert!(validate_git_path("git-diff", "/x").is_err());
        assert!(validate_git_path("git-diff", "-x").is_err());
    }

    #[test]
    fn argv_shapes_are_fixed() {
        assert_eq!(
            git_changed_files_plan(None).unwrap(),
            vec![
                vec!["diff", "--name-only", "-z", "HEAD", "--", "."],
                vec![
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "-z",
                    "--full-name",
                    "--",
                    "."
                ],
            ]
        );
        assert_eq!(
            git_changed_files_plan(Some("main")).unwrap(),
            vec![vec!["diff", "--name-only", "-z", "main", "--", "."]]
        );
        assert_eq!(
            git_diff_argv(None, Some("src/a.ts")).unwrap(),
            vec!["diff", "--relative", "HEAD", "--", "src/a.ts"]
        );
        assert_eq!(
            git_log_argv(20),
            vec!["log", "-n20", "--pretty=format:%H%x00%s%x00%an%x00%aI"]
        );
    }

    #[test]
    fn status_porcelain_v2_parses_renames_and_prefixes() {
        let stdout = concat!(
            "# branch.oid 1234\0",
            "# branch.head feature/x\0",
            "1 .M N... 100644 100644 100644 aaa bbb sub/dir/changed.txt\0",
            "2 R. N... 100644 100644 100644 aaa bbb R100 sub/dir/new name.txt\0",
            "sub/dir/old name.txt\0",
            "? sub/dir/untracked.txt\0",
        );
        let status = parse_git_status_porcelain_v2(stdout, "sub/dir/").unwrap();
        assert_eq!(status.branch.as_deref(), Some("feature/x"));
        assert!(!status.clean);
        assert_eq!(status.staged, vec!["new name.txt".to_string()]);
        assert_eq!(status.unstaged, vec!["changed.txt".to_string()]);
        assert_eq!(status.untracked, vec!["untracked.txt".to_string()]);
    }

    #[test]
    fn detached_head_has_no_branch() {
        let stdout = "# branch.head (detached)\0";
        let status = parse_git_status_porcelain_v2(stdout, "").unwrap();
        assert!(status.branch.is_none());
        assert!(status.clean);
    }

    #[test]
    fn log_and_path_lists_parse() {
        let log = "aaaa\x00subject one\x00Alice\x002026-01-01T00:00:00+00:00\n\
                   bbbb\x00subject two\x00Bob\x002026-01-02T00:00:00+00:00\n";
        let commits = parse_git_log(log).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[1].subject, "subject two");
        assert!(parse_git_log("only\0two\n").is_err());

        let paths = parse_git_path_list("w/a.txt\0w/b c.txt\0", "w/").unwrap();
        assert_eq!(paths, vec!["a.txt".to_string(), "b c.txt".to_string()]);
        assert!(parse_git_path_list("other/a.txt\0", "w/").is_err());
    }
}
