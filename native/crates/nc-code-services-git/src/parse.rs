//! Pure parsers for Git's machine-readable output plus the TS error-message shapes.
//!
//! Every function here mirrors a named helper in
//! `packages/services/src/git/repo/gitCliHelpers.ts`; the file:line references below are
//! the parity source, because the frontend asserts on these exact strings, field names and
//! orderings.

use nc_code_service_core::ServiceError;
use serde_json::{json, Value};

use crate::cmd::GitCommandResult;
use crate::path::normalize_git_path;

pub const KIND_ADDED: &str = "added";
pub const KIND_DELETED: &str = "deleted";
pub const KIND_RENAMED: &str = "renamed";
pub const KIND_MODIFIED: &str = "modified";

/// `GitLineStat` (`repo/gitCliTypes.ts:14`).
#[derive(Debug, Clone, PartialEq)]
pub struct LineStat {
    pub added: i64,
    pub removed: i64,
    pub kind: Option<&'static str>,
    pub original_path: Option<String>,
}

/// `parseNumstat` returns a JS `Map`, so the *insertion order* is part of the observable
/// result: `getBranchComparison` maps `Array.from(map.entries())` straight into the
/// response (`gitCliRepo.ts:1402`). Replacing a key keeps its original position, which is
/// what `OrderedStats` reproduces.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrderedStats {
    entries: Vec<(String, LineStat)>,
}

impl OrderedStats {
    pub fn set(&mut self, path: String, stat: LineStat) {
        if let Some(existing) = self.entries.iter_mut().find(|(key, _)| *key == path) {
            existing.1 = stat;
        } else {
            self.entries.push((path, stat));
        }
    }

    pub fn get(&self, path: &str) -> Option<&LineStat> {
        self.entries
            .iter()
            .find(|(key, _)| key == path)
            .map(|(_, stat)| stat)
    }

    pub fn paths(&self) -> impl Iterator<Item = &String> {
        self.entries.iter().map(|(path, _)| path)
    }

    /// Iteration order is the JS `Map` insertion order (Git output order).
    pub fn entries(&self) -> &[(String, LineStat)] {
        &self.entries
    }
}

/// `GitStatusEntry` (`repo/gitCliTypes.ts:21`).
#[derive(Debug, Clone, PartialEq)]
pub struct StatusEntry {
    pub path: String,
    pub original_path: Option<String>,
    pub kind: &'static str,
    pub x: Option<String>,
    pub y: Option<String>,
    pub is_untracked: bool,
    pub is_conflicted: bool,
}

/// `parseStatusPorcelain` return shape (`gitCliHelpers.ts:244`).
#[derive(Debug, Clone, Default)]
pub struct ParsedStatus {
    pub branch_name: Option<String>,
    pub tracking_branch_name: Option<String>,
    pub head_ref_type: &'static str,
    pub ahead: i64,
    pub behind: i64,
    pub entries: Vec<StatusEntry>,
}

/// `toResultMessage` (`gitCliHelpers.ts:19`).
pub fn to_result_message(result: &GitCommandResult) -> String {
    let stderr = result.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = result.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }
    format!("exitCode={}", display_exit_code(result.exit_code))
}

fn display_exit_code(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => code.to_string(),
        None => "null".to_string(),
    }
}

/// `ensureGitCommandSucceeded` (`gitCliHelpers.ts:169`) — throws the exact TS text.
pub fn ensure_git_command_succeeded(
    label: &str,
    result: &GitCommandResult,
    allowed_exit_codes: &[i32],
) -> Result<(), ServiceError> {
    if result.timed_out {
        let timeout_ms = result.timeout_ms;
        let mut details = vec![format!("elapsed={}ms", result.duration_ms)];
        if let Some(kill_at) = result.timeout_elapsed_ms {
            details.push(format!("killAt={kill_at}ms"));
        }
        return Err(ServiceError::message(format!(
            "{label} timed out after {timeout_ms}ms ({})",
            details.join(", ")
        )));
    }
    if result.output_truncated {
        return Err(ServiceError::message(format!(
            "{label} output exceeded limit"
        )));
    }
    if result
        .exit_code
        .is_some_and(|code| allowed_exit_codes.contains(&code))
    {
        return Ok(());
    }
    Err(ServiceError::message(format!(
        "{label} failed: {}",
        to_result_message(result)
    )))
}

/// `isNotRepositoryResult` (`gitCliHelpers.ts:205`).
pub fn is_not_repository_result(result: &GitCommandResult) -> bool {
    let stderr = result.stderr.to_lowercase();
    stderr.contains("not a git repository") || stderr.contains("outside repository")
}

/// `isMissingWorkingDirectoryResult` (`gitCliHelpers.ts:210`).
pub fn is_missing_working_directory_result(result: &GitCommandResult) -> bool {
    let stderr = result.stderr.to_lowercase();
    (result.exit_code == Some(-2) && stderr.contains("enoent"))
        || stderr.contains("unable to read current working directory")
        || stderr.contains("no such file or directory")
}

/// `inferKindFromStatusCode` (`gitCliHelpers.ts:219`).
pub fn infer_kind_from_status_code(status_code: &str) -> &'static str {
    match status_code {
        "A" | "?" => KIND_ADDED,
        "D" => KIND_DELETED,
        "R" | "C" => KIND_RENAMED,
        _ => KIND_MODIFIED,
    }
}

/// `inferKindFromNumstat` (`gitCliHelpers.ts:370`).
pub fn infer_kind_from_numstat(stat: &LineStat) -> &'static str {
    if let Some(kind) = stat.kind {
        return kind;
    }
    if stat.added > 0 && stat.removed == 0 {
        return KIND_ADDED;
    }
    if stat.removed > 0 && stat.added == 0 {
        return KIND_DELETED;
    }
    KIND_MODIFIED
}

fn parse_branch_ahead_behind(value: &str) -> (i64, i64) {
    (
        first_number_after(value, '+').unwrap_or(0),
        first_number_after(value, '-').unwrap_or(0),
    )
}

/// JS `/\+(\d+)/` — the first marker that is immediately followed by digits.
fn first_number_after(value: &str, marker: char) -> Option<i64> {
    let mut search_from = 0usize;
    loop {
        let relative = value[search_from..].find(marker)?;
        let start = search_from + relative + marker.len_utf8();
        let digits: String = value[start..]
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            search_from = start;
            continue;
        }
        return digits.parse().ok();
    }
}

/// Splits `record` into exactly `fixed` single-space-delimited tokens plus the remainder,
/// requiring every fixed token to be non-empty and the first one to be exactly two
/// characters — the same constraint as TS's `/^1 ([^ ]{2}) [^ ]+ … (.+)$/`
/// (`gitCliHelpers.ts:298`).
fn split_fixed_tokens(
    record: &str,
    prefix_len: usize,
    fixed: usize,
) -> Option<(char, char, String)> {
    let body = record.get(prefix_len..)?;
    let mut rest = body;
    let mut first_token: &str = "";
    for position in 0..fixed {
        let (token, tail) = rest.split_once(' ')?;
        if token.is_empty() {
            return None;
        }
        if position == 0 {
            first_token = token;
        }
        rest = tail;
    }
    let mut characters = first_token.chars();
    let (Some(x), Some(y), None) = (characters.next(), characters.next(), characters.next()) else {
        return None;
    };
    if rest.is_empty() {
        return None;
    }
    Some((x, y, rest.to_string()))
}

/// `parseStatusPorcelain` (`gitCliHelpers.ts:244`). `--porcelain=v2 -z` is locale stable,
/// which is why the TS side parses it instead of the human output.
pub fn parse_status_porcelain(stdout: &str) -> ParsedStatus {
    let records: Vec<&str> = stdout
        .split('\0')
        .filter(|record| !record.is_empty())
        .collect();
    let mut parsed = ParsedStatus {
        head_ref_type: "branch",
        ..ParsedStatus::default()
    };
    let mut index = 0usize;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.starts_with("# ") {
            if let Some(head) = record.strip_prefix("# branch.head ") {
                if head == "(detached)" {
                    parsed.branch_name = None;
                    parsed.head_ref_type = "detached";
                } else {
                    parsed.branch_name = Some(head.to_string());
                    parsed.head_ref_type = "branch";
                }
            } else if let Some(upstream) = record.strip_prefix("# branch.upstream ") {
                parsed.tracking_branch_name = Some(upstream.to_string());
            } else if let Some(ab) = record.strip_prefix("# branch.ab ") {
                let (ahead, behind) = parse_branch_ahead_behind(ab);
                parsed.ahead = ahead;
                parsed.behind = behind;
            }
            continue;
        }

        if let Some(path) = record.strip_prefix("? ") {
            parsed.entries.push(StatusEntry {
                path: normalize_git_path(path),
                original_path: None,
                kind: KIND_ADDED,
                x: None,
                y: Some("?".to_string()),
                is_untracked: true,
                is_conflicted: false,
            });
            continue;
        }

        if record.starts_with("1 ") {
            let Some((x, y, path)) = split_fixed_tokens(record, 2, 7) else {
                continue;
            };
            let active = if x == '.' { y } else { x };
            parsed.entries.push(StatusEntry {
                path: normalize_git_path(&path),
                original_path: None,
                kind: infer_kind_from_status_code(&active.to_string()),
                x: Some(x.to_string()),
                y: Some(y.to_string()),
                is_untracked: false,
                is_conflicted: false,
            });
            continue;
        }

        if record.starts_with("2 ") {
            let Some((x, y, path)) = split_fixed_tokens(record, 2, 8) else {
                continue;
            };
            let original_path = records.get(index).map(|value| normalize_git_path(value));
            index += 1;
            parsed.entries.push(StatusEntry {
                path: normalize_git_path(&path),
                original_path,
                kind: KIND_RENAMED,
                x: Some(x.to_string()),
                y: Some(y.to_string()),
                is_untracked: false,
                is_conflicted: false,
            });
            continue;
        }

        if !record.starts_with("u ") {
            continue;
        }
        let Some((x, y, path)) = split_fixed_tokens(record, 2, 9) else {
            continue;
        };
        parsed.entries.push(StatusEntry {
            path: normalize_git_path(&path),
            original_path: None,
            kind: KIND_MODIFIED,
            x: Some(x.to_string()),
            y: Some(y.to_string()),
            is_untracked: false,
            is_conflicted: true,
        });
    }
    parsed
}

fn parse_numstat_value(value: &str) -> i64 {
    if value == "-" {
        return 0;
    }
    // JS `parseInt` stops at the first non-digit instead of failing.
    let digits: String = value
        .chars()
        .skip_while(|character| *character == '+' || character.is_whitespace())
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(0)
}

/// `parseNumstat` (`gitCliHelpers.ts:386`).
pub fn parse_numstat(stdout: &str) -> OrderedStats {
    let records: Vec<&str> = stdout.split('\0').collect();
    let mut stats = OrderedStats::default();
    let mut index = 0usize;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split('\t').collect();
        if fields.len() < 3 {
            continue;
        }
        let added = parse_numstat_value(fields[0]);
        let removed = parse_numstat_value(fields[1]);
        let path_field = fields[2..].join("\t");
        if !path_field.is_empty() {
            stats.set(
                normalize_git_path(&path_field),
                LineStat {
                    added,
                    removed,
                    kind: None,
                    original_path: None,
                },
            );
            continue;
        }
        // Rename records put the paths in the two following NUL-separated fields:
        // `<added>\t<removed>\t\0<original>\0<renamed>\0`.
        let original_path = records.get(index).copied().unwrap_or_default();
        let renamed_path = records.get(index + 1).copied().unwrap_or_default();
        index += 2;
        if renamed_path.is_empty() {
            continue;
        }
        stats.set(
            normalize_git_path(renamed_path),
            LineStat {
                added,
                removed,
                kind: Some(KIND_RENAMED),
                original_path: Some(normalize_git_path(original_path)),
            },
        );
    }
    stats
}

/// `toDiffResult` (`gitCliHelpers.ts:585`).
pub fn to_diff_result(
    path: &str,
    result: &GitCommandResult,
    allowed_exit_codes: &[i32],
    empty_summary: &str,
    binary_summary: &str,
) -> Value {
    if result.timed_out {
        return unavailable_diff(path, "Git diff command timed out.");
    }
    if result.output_truncated {
        return unavailable_diff(path, "Git diff output exceeded the preview limit.");
    }
    if !result
        .exit_code
        .is_some_and(|code| allowed_exit_codes.contains(&code))
    {
        return unavailable_diff(path, &to_result_message(result));
    }
    if result.stdout.trim().is_empty() {
        return unavailable_diff(path, empty_summary);
    }
    if is_binary_diff(&result.stdout) {
        return json!({
            "path": path,
            "availability": "binary",
            "patch": Value::Null,
            "beforeContent": Value::Null,
            "afterContent": Value::Null,
            "summary": binary_summary,
        });
    }
    json!({
        "path": path,
        "availability": "patch",
        "patch": result.stdout.clone(),
        "beforeContent": Value::Null,
        "afterContent": Value::Null,
        "summary": Value::Null,
    })
}

pub fn unavailable_diff(path: &str, summary: &str) -> Value {
    json!({
        "path": path,
        "availability": "unavailable",
        "patch": Value::Null,
        "beforeContent": Value::Null,
        "afterContent": Value::Null,
        "summary": summary,
    })
}

/// `isBinaryDiff` (`gitCliHelpers.ts:581`).
fn is_binary_diff(stdout: &str) -> bool {
    stdout.contains("GIT binary patch") || stdout.contains("Binary files ")
}

/// `parseGitConfigValue` (`gitCliHelpers.ts:660`) for
/// the triple returned for `git config --get`: (value, origin, scope).
pub type GitConfigGet = (Option<String>, Option<String>, Option<String>);

/// `git config --show-scope --show-origin --get <key>`.
pub fn parse_git_config_value(result: &GitCommandResult) -> Result<GitConfigGet, ServiceError> {
    if result.exit_code == Some(1) {
        return Ok((None, None, None));
    }
    ensure_git_command_succeeded("git config", result, &[0])?;
    let line = result.stdout.trim_end_matches('\n').trim_end_matches('\r');
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 3 {
        return Ok((
            None,
            None,
            if line.is_empty() {
                None
            } else {
                Some(line.to_string())
            },
        ));
    }
    let value = parts[2..].join("\t");
    Ok((
        Some(parts[0].to_string()),
        Some(parts[1].to_string()),
        if value.is_empty() { None } else { Some(value) },
    ))
}

/// `toNormalizedLines` (`gitCliHelpers.ts:23`).
fn to_normalized_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .split('\n')
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// `extractIndentedPaths` (`gitCliHelpers.ts:30`).
fn extract_indented_paths(lines: &[String], header: &str) -> Vec<String> {
    let Some(header_index) = lines
        .iter()
        .position(|line| line.to_lowercase().contains(header))
    else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for line in lines.iter().skip(header_index + 1) {
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            break;
        }
        let value = line.trim();
        if !value.is_empty() {
            paths.push(normalize_git_path(value));
        }
    }
    paths
}

/// `toInvalidBranchNameIssue` (`gitCliHelpers.ts:56`).
pub fn to_invalid_branch_name_issue(detail: Option<&str>) -> Value {
    let trimmed = detail.map(str::trim).filter(|value| !value.is_empty());
    json!({
        "code": "invalid-branch-name",
        "message": "Branch name is invalid.",
        "detail": trimmed.map(str::to_string),
    })
}

/// `parseGitBranchMutationIssues` (`gitCliHelpers.ts:64`) — Git's native stderr is the
/// fact source; this normalises it into the stable issue codes the UI renders.
pub fn parse_git_branch_mutation_issues(result: &GitCommandResult) -> Vec<Value> {
    let detail = {
        let stderr = result.stderr.trim();
        let stdout = result.stdout.trim();
        if !stderr.is_empty() {
            Some(stderr.to_string())
        } else if !stdout.is_empty() {
            Some(stdout.to_string())
        } else {
            None
        }
    };
    let lines = to_normalized_lines(detail.as_deref().unwrap_or_default());
    let normalized_detail = detail.clone().unwrap_or_default().to_lowercase();

    let overwrite = ["checkout", "switch"];
    for (header, code, message) in [
        (
            "your local changes to the following files would be overwritten by",
            "tracked-changes-would-be-overwritten",
            "Tracked changes would be overwritten by switching branches.",
        ),
        (
            "the following untracked working tree files would be overwritten by",
            "untracked-changes-would-be-overwritten",
            "Untracked files would be overwritten by switching branches.",
        ),
    ] {
        for verb in overwrite {
            let paths = extract_indented_paths(&lines, &format!("{header} {verb}"));
            if !paths.is_empty() {
                return vec![json!({
                    "code": code,
                    "message": message,
                    "paths": paths,
                    "detail": detail,
                })];
            }
        }
    }

    if normalized_detail.contains("already exists") {
        return vec![issue(
            "branch-already-exists",
            "Branch already exists.",
            &detail,
        )];
    }
    if normalized_detail.contains("invalid reference:") {
        return vec![issue(
            "target-branch-not-found",
            "Target branch was not found.",
            &detail,
        )];
    }
    if normalized_detail.contains("is already used by worktree at") {
        return vec![issue(
            "branch-in-other-worktree",
            "Branch is already checked out in another worktree.",
            &detail,
        )];
    }
    if normalized_detail.contains("resolve your current index first") {
        return vec![issue(
            "conflicts-present",
            "Repository still has unresolved conflicts.",
            &detail,
        )];
    }
    let operation_in_progress = [
        "merging",
        "rebasing",
        "cherry-picking",
        "reverting",
        "bisecting",
    ]
    .iter()
    .any(|verb| normalized_detail.contains(&format!("cannot switch branch while {verb}")))
        || normalized_detail.contains("you have not concluded your merge")
        || normalized_detail.contains("rebase in progress");
    if operation_in_progress {
        return vec![issue(
            "operation-in-progress",
            "Another Git operation is still in progress.",
            &detail,
        )];
    }
    vec![issue(
        "unknown",
        "Git could not complete the branch operation.",
        &detail,
    )]
}

fn issue(code: &str, message: &str, detail: &Option<String>) -> Value {
    json!({
        "code": code,
        "message": message,
        "detail": detail.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(stdout: &str, stderr: &str, exit_code: Option<i32>) -> GitCommandResult {
        GitCommandResult {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit_code,
            timed_out: false,
            output_truncated: false,
            duration_ms: 1,
            timeout_ms: 15_000,
            timeout_elapsed_ms: None,
        }
    }

    #[test]
    fn parses_status_records_and_branch_headers() {
        let stdout = "# branch.oid 111\0# branch.head master\0# branch.upstream origin/master\0\
             # branch.ab +2 -1\x001 .M N... 100644 100644 100644 a b a.txt\0\
             ? untracked.txt\x002 RM N... 100644 100644 100644 a b R100 new.txt\0old.txt\0\
             u UU N... 100644 100644 100644 100644 a b c conflict.txt\0";
        let parsed = parse_status_porcelain(stdout);
        assert_eq!(parsed.branch_name.as_deref(), Some("master"));
        assert_eq!(
            parsed.tracking_branch_name.as_deref(),
            Some("origin/master")
        );
        assert_eq!((parsed.ahead, parsed.behind), (2, 1));
        assert_eq!(parsed.entries.len(), 4);
        assert_eq!(parsed.entries[0].path, "a.txt");
        assert_eq!(parsed.entries[0].kind, KIND_MODIFIED);
        assert_eq!(parsed.entries[1].kind, KIND_ADDED);
        assert_eq!(parsed.entries[1].y.as_deref(), Some("?"));
        assert!(parsed.entries[1].is_untracked);
        assert_eq!(parsed.entries[2].original_path.as_deref(), Some("old.txt"));
        assert!(parsed.entries[3].is_conflicted);
    }

    #[test]
    fn detached_head_and_ahead_without_upstream() {
        let parsed = parse_status_porcelain("# branch.oid 1\0# branch.head (detached)\0");
        assert_eq!(parsed.head_ref_type, "detached");
        assert_eq!(parsed.branch_name, None);
        let ab = parse_branch_ahead_behind("+0 -0");
        assert_eq!(ab, (0, 0));
    }

    #[test]
    fn malformed_status_records_are_skipped_like_ts() {
        // `1` records need seven fixed tokens before the path.
        let parsed = parse_status_porcelain("1 .M N... a.txt\0");
        assert!(parsed.entries.is_empty());
    }

    #[test]
    fn numstat_keeps_insertion_order_and_handles_renames() {
        let stats = parse_numstat("2\t1\tb.txt\x005\t0\ta.txt\0");
        assert_eq!(
            stats.paths().cloned().collect::<Vec<_>>(),
            vec!["b.txt".to_string(), "a.txt".to_string()]
        );
        assert_eq!(stats.get("b.txt").map(|stat| stat.added), Some(2));
        assert_eq!(stats.get("b.txt").map(|stat| stat.removed), Some(1));

        let renamed = parse_numstat("3\t3\t\0old.txt\0new.txt\0");
        assert_eq!(
            renamed.get("new.txt").map(|stat| stat.kind),
            Some(Some(KIND_RENAMED))
        );
        assert_eq!(
            renamed
                .get("new.txt")
                .and_then(|stat| stat.original_path.as_deref()),
            Some("old.txt")
        );

        // Binary files report `-` for both sides, which TS maps to 0.
        let binary = parse_numstat("-\t-\timage.png\0");
        assert_eq!(binary.get("image.png").map(|stat| stat.added), Some(0));
        assert_eq!(
            infer_kind_from_numstat(binary.get("image.png").unwrap()),
            KIND_MODIFIED
        );
    }

    #[test]
    fn ordered_stats_replace_in_place() {
        let mut stats = OrderedStats::default();
        stats.set(
            "a".to_string(),
            LineStat {
                added: 1,
                removed: 0,
                kind: None,
                original_path: None,
            },
        );
        stats.set(
            "b".to_string(),
            LineStat {
                added: 2,
                removed: 0,
                kind: None,
                original_path: None,
            },
        );
        stats.set(
            "a".to_string(),
            LineStat {
                added: 9,
                removed: 0,
                kind: None,
                original_path: None,
            },
        );
        assert_eq!(
            stats.paths().cloned().collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(stats.get("a").map(|stat| stat.added), Some(9));
    }

    #[test]
    fn diff_result_states_match_ts_strings() {
        let patch = to_diff_result(
            "C:/r/a.txt",
            &result("diff --git a\n", "", Some(0)),
            &[0],
            "empty",
            "binary",
        );
        assert_eq!(patch["availability"], json!("patch"));
        assert_eq!(patch["summary"], Value::Null);
        let empty = to_diff_result(
            "p",
            &result("  \n", "", Some(0)),
            &[0],
            "No diff output available.",
            "b",
        );
        assert_eq!(empty["availability"], json!("unavailable"));
        assert_eq!(empty["summary"], json!("No diff output available."));
        let binary = to_diff_result(
            "p",
            &result("Binary files a and b differ", "", Some(0)),
            &[0],
            "e",
            "Binary diff is not previewable.",
        );
        assert_eq!(binary["availability"], json!("binary"));
        let failed = to_diff_result("p", &result("", "boom", Some(128)), &[0], "e", "b");
        assert_eq!(failed["summary"], json!("boom"));
        let truncated = GitCommandResult {
            output_truncated: true,
            ..result("x", "", Some(0))
        };
        assert_eq!(
            to_diff_result("p", &truncated, &[0], "e", "b")["summary"],
            json!("Git diff output exceeded the preview limit.")
        );
    }

    #[test]
    fn command_failure_messages_match_ts_text() {
        let error =
            ensure_git_command_succeeded("git status", &result("", "fatal: bad", Some(128)), &[0])
                .unwrap_err();
        assert_eq!(error.message, "git status failed: fatal: bad");
        let truncated = GitCommandResult {
            output_truncated: true,
            ..result("", "", Some(0))
        };
        assert_eq!(
            ensure_git_command_succeeded("git status", &truncated, &[0])
                .unwrap_err()
                .message,
            "git status output exceeded limit"
        );
        let timed_out = GitCommandResult {
            timed_out: true,
            timeout_elapsed_ms: Some(15_000),
            ..result("", "", None)
        };
        assert_eq!(
            ensure_git_command_succeeded("git log", &timed_out, &[0])
                .unwrap_err()
                .message,
            "git log timed out after 15000ms (elapsed=1ms, killAt=15000ms)"
        );
        assert_eq!(
            ensure_git_command_succeeded("git commit", &result("", "", None), &[0])
                .unwrap_err()
                .message,
            "git commit failed: exitCode=null"
        );
        assert!(
            ensure_git_command_succeeded("git diff", &result("", "", Some(1)), &[0, 1]).is_ok()
        );
    }

    #[test]
    fn repository_and_missing_directory_detection() {
        assert!(is_not_repository_result(&result(
            "",
            "fatal: not a git repository (or any of the parent directories): .git",
            Some(128)
        )));
        assert!(is_missing_working_directory_result(&result(
            "",
            "spawn git ENOENT",
            Some(-2)
        )));
        assert!(!is_missing_working_directory_result(&result(
            "",
            "fatal: not a git repository",
            Some(128)
        )));
    }

    #[test]
    fn config_value_parses_scoped_origin_lines() {
        let (scope, source, value) = parse_git_config_value(&result(
            "local\tC:/repo/.git/gitconfig\tTest User\n",
            "",
            Some(0),
        ))
        .expect("ok");
        assert_eq!(scope.as_deref(), Some("local"));
        assert_eq!(source.as_deref(), Some("C:/repo/.git/gitconfig"));
        assert_eq!(value.as_deref(), Some("Test User"));
        let missing = parse_git_config_value(&result("", "", Some(1))).expect("unset");
        assert_eq!(missing, (None, None, None));
        let unscoped = parse_git_config_value(&result("Plain Value\n", "", Some(0))).expect("ok");
        assert_eq!(unscoped, (None, None, Some("Plain Value".to_string())));
    }

    #[test]
    fn branch_mutation_issues_are_normalised() {
        let tracked = parse_git_branch_mutation_issues(&result(
            "",
            "error: Your local changes to the following files would be overwritten by checkout:\n\ta.txt\n\tb.txt\nPlease commit your changes or stash them before you switch branches.",
            Some(1),
        ));
        assert_eq!(
            tracked[0]["code"],
            json!("tracked-changes-would-be-overwritten")
        );
        assert_eq!(tracked[0]["paths"], json!(["a.txt", "b.txt"]));

        let untracked = parse_git_branch_mutation_issues(&result(
            "",
            "error: The following untracked working tree files would be overwritten by switch:\n\tc.txt",
            Some(1),
        ));
        assert_eq!(
            untracked[0]["code"],
            json!("untracked-changes-would-be-overwritten")
        );
        assert_eq!(untracked[0]["paths"], json!(["c.txt"]));

        for (stderr, code) in [
            (
                "fatal: a branch named 'x' already exists",
                "branch-already-exists",
            ),
            ("fatal: invalid reference: nope", "target-branch-not-found"),
            (
                "fatal: 'x' is already used by worktree at '/w'",
                "branch-in-other-worktree",
            ),
            (
                "error: please resolve your current index first",
                "conflicts-present",
            ),
            (
                "fatal: you have not concluded your merge (MERGE_HEAD exists)",
                "operation-in-progress",
            ),
            (
                "error: cannot switch branch while rebasing",
                "operation-in-progress",
            ),
            ("fatal: something else", "unknown"),
        ] {
            let issues = parse_git_branch_mutation_issues(&result("", stderr, Some(128)));
            assert_eq!(issues[0]["code"], json!(code), "stderr: {stderr}");
            assert_eq!(issues.len(), 1);
        }
    }

    #[test]
    fn invalid_branch_name_issue_drops_blank_detail() {
        assert_eq!(
            to_invalid_branch_name_issue(Some("  "))["detail"],
            Value::Null
        );
        assert_eq!(
            to_invalid_branch_name_issue(Some("fatal: bad"))["detail"],
            json!("fatal: bad")
        );
    }
}
