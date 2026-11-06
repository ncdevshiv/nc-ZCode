//! Path helpers that mirror the Node `path` semantics the TS Git service relies on.
//!
//! The TS implementation builds every absolute path with `node:path`'s `resolve` and
//! compares paths with `relative` (`packages/services/src/git/repo/gitCliHelpers.ts:683`,
//! `gitService.ts:16`). Those functions are platform specific: on Windows `resolve`
//! rewrites separators to `\` and `relative` compares case-insensitively, while Git
//! itself always prints forward slashes. Matching that exactly keeps `GitFileChange.path`
//! byte-identical to what the current frontend already receives.

use serde_json::{json, Value};

/// `path.sep` for the running platform.
pub const SEP: char = if cfg!(windows) { '\\' } else { '/' };

pub fn is_separator(character: char) -> bool {
    character == '/' || (cfg!(windows) && character == '\\')
}

/// `normalizeGitPath` (`config.ts:89`).
pub fn normalize_git_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// `normalizeWorkspaceInRepoPath` (`config.ts:93`).
pub fn normalize_workspace_in_repo_path(path: &str) -> String {
    let without_prefix = strip_leading_scope(&normalize_git_path(path));
    let without_suffix = without_prefix.trim_end_matches('/');
    if without_suffix.is_empty() {
        ".".to_string()
    } else {
        without_suffix.to_string()
    }
}

/// TS `/^\.?\//`: drop a leading `./` or `/` only.
fn strip_leading_scope(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("./") {
        return rest.to_string();
    }
    if let Some(rest) = path.strip_prefix('/') {
        return rest.to_string();
    }
    path.to_string()
}

/// `isPathInWorkspaceScope` (`config.ts:100`).
pub fn is_path_in_workspace_scope(repo_relative_path: &str, workspace_in_repo_path: &str) -> bool {
    let normalized_path = strip_leading_scope(&normalize_git_path(repo_relative_path));
    let normalized_workspace = normalize_workspace_in_repo_path(workspace_in_repo_path);
    if normalized_workspace == "." {
        return true;
    }
    normalized_path == normalized_workspace
        || normalized_path.starts_with(&format!("{normalized_workspace}/"))
}

/// `toWorkspaceRelativeGitPath` (`config.ts:115`).
pub fn to_workspace_relative_git_path(
    repo_relative_path: &str,
    workspace_in_repo_path: &str,
) -> String {
    let normalized_path = strip_leading_scope(&normalize_git_path(repo_relative_path));
    let normalized_workspace = normalize_workspace_in_repo_path(workspace_in_repo_path);
    if normalized_workspace == "." {
        return normalized_path;
    }
    if normalized_path == normalized_workspace {
        return ".".to_string();
    }
    match normalized_path.strip_prefix(&format!("{normalized_workspace}/")) {
        Some(stripped) => stripped.to_string(),
        None => normalized_path,
    }
}

/// `path.isAbsolute`.
pub fn is_absolute(path: &str) -> bool {
    if cfg!(windows) {
        let bytes = path.as_bytes();
        if bytes
            .first()
            .is_some_and(|byte| *byte == b'/' || *byte == b'\\')
        {
            return true;
        }
        // `C:\` / `C:/`; a bare drive prefix (`C:foo`) is drive-relative, not absolute.
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
        {
            return true;
        }
        return bytes.len() >= 4 && bytes[0] == b'\\' && bytes[1] == b'\\'; // UNC
    }
    path.starts_with('/')
}

/// The absolute root of `path` plus its relative remainder (`path.resolve` internals).
fn split_root(path: &str) -> (String, String) {
    if cfg!(windows) {
        let bytes = path.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return (format!("{}{}", &path[..2], SEP), path[2..].to_string());
        }
        if path.starts_with("\\\\") || path.starts_with("//") {
            let normalized = path.replace('/', "\\");
            let mut segments = normalized[2..].splitn(3, '\\');
            let server = segments.next().unwrap_or_default();
            let share = segments.next().unwrap_or_default();
            let rest = segments.next().unwrap_or_default();
            return (format!("\\\\{server}\\{share}"), rest.to_string());
        }
        if path.starts_with('\\') || path.starts_with('/') {
            return (SEP.to_string(), path[1..].to_string());
        }
        return (String::new(), path.to_string());
    }
    if let Some(rest) = path.strip_prefix('/') {
        return ("/".to_string(), rest.to_string());
    }
    (String::new(), path.to_string())
}

fn segments_of(rest: &str) -> Vec<&str> {
    rest.split(is_separator)
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Collapse `.`/`..`/repeated separators and emit the platform separator, like Node's
/// `normalize` on an absolute path.
fn normalize_absolute(path: &str) -> String {
    let (root, rest) = split_root(path);
    let mut stack: Vec<&str> = Vec::new();
    let collected = segments_of(&rest);
    for segment in collected {
        match segment {
            "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    if root.is_empty() {
        return if stack.is_empty() {
            ".".to_string()
        } else {
            stack.join(&SEP.to_string())
        };
    }
    if stack.is_empty() {
        return root;
    }
    let sep = SEP.to_string();
    let between = if root.ends_with(SEP) { "" } else { &sep };
    format!("{}{}{}", root, between, stack.join(&sep))
}

/// `path.resolve(base, ...segments)` for an absolute `base` (every Git service caller
/// passes a workspace path or a repo root).
pub fn resolve_path(base: &str, segments: &[&str]) -> String {
    let mut current = base.to_string();
    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        if is_absolute(segment) {
            current = segment.to_string();
            continue;
        }
        let trimmed = strip_leading_separators(segment);
        if trimmed.is_empty() {
            continue;
        }
        if !current.is_empty() && !current.ends_with(['/', '\\']) {
            current.push(SEP);
        }
        current.push_str(trimmed);
    }
    normalize_absolute(&current)
}

fn strip_leading_separators(path: &str) -> &str {
    for (position, character) in path.char_indices() {
        if !is_separator(character) {
            return &path[position..];
        }
    }
    ""
}

/// `toAbsolutePath(repoRoot, repoRelativePath)` (`gitService.ts:16`).
pub fn repo_relative_to_absolute(repo_root: &str, repo_relative_path: &str) -> String {
    let normalized = normalize_git_path(repo_relative_path);
    let segments: Vec<&str> = normalized.split('/').collect();
    resolve_path(repo_root, &segments)
}

/// `path.relative(from, to)` where both arguments are absolute.
pub fn relative_path(from: &str, to: &str) -> String {
    let from = normalize_absolute(from);
    let to = normalize_absolute(to);
    let (from_root, from_rest) = split_root(&from);
    let (to_root, to_rest) = split_root(&to);
    if !same_root(&from_root, &to_root) {
        // Node returns the absolute `to` when the roots differ.
        return to;
    }
    let from_segments = segments_of(&from_rest);
    let to_segments = segments_of(&to_rest);
    let mut common = 0;
    while common < from_segments.len()
        && common < to_segments.len()
        && segment_eq(from_segments[common], to_segments[common])
    {
        common += 1;
    }
    let mut out: Vec<String> = vec!["..".to_string(); from_segments.len() - common];
    out.extend(
        to_segments[common..]
            .iter()
            .map(|segment| (*segment).to_string()),
    );
    if out.is_empty() {
        return String::new();
    }
    out.join(&SEP.to_string())
}

fn same_root(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        return left.eq_ignore_ascii_case(right);
    }
    left == right
}

fn segment_eq(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        return left.to_lowercase() == right.to_lowercase();
    }
    left == right
}

/// `normalizeWatchPath` (`gitCliRepo.ts:117`).
pub fn normalize_watch_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed == "/" {
        return trimmed.to_string();
    }
    if is_bare_drive_root(trimmed) {
        return trimmed.to_string();
    }
    let mut end = trimmed.len();
    while end > 0 && is_separator(trimmed.as_bytes()[end - 1] as char) {
        end -= 1;
    }
    trimmed[..end].to_string()
}

/// `/^[A-Za-z]:[\\/]?$/.test(trimmed)` — only meaningful on Windows.
fn is_bare_drive_root(path: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let bytes = path.as_bytes();
    if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return false;
    }
    bytes.len() == 2 || (bytes.len() == 3 && (bytes[2] == b'\\' || bytes[2] == b'/'))
}

pub fn add_auto_refresh_watch_path(paths: &mut Vec<Value>, path: &str, recursive: bool) {
    let normalized = normalize_watch_path(path);
    if normalized.is_empty() || paths.iter().any(|entry| entry["path"] == json!(normalized)) {
        return;
    }
    paths.push(json!({"path": normalized, "recursive": recursive}));
}

/// `buildAutoRefreshWatchPaths` (`gitCliRepo.ts:142`). Git metadata can live outside
/// `repoRoot` for linked worktrees, so the refresh boundary comes from Git itself.
pub fn build_auto_refresh_watch_paths(
    workspace_path: &str,
    absolute_git_dir: &str,
    git_common_dir: &str,
) -> Vec<Value> {
    let mut paths = Vec::new();
    let absolute_git_dir = absolute_git_dir.trim();
    add_auto_refresh_watch_path(&mut paths, absolute_git_dir, true);
    let common_dir = git_common_dir.trim();
    let resolved_common_dir = if common_dir.is_empty() {
        absolute_git_dir.to_string()
    } else if is_absolute(common_dir) {
        common_dir.to_string()
    } else {
        // `--git-common-dir` is relative to the command cwd, not to repoRoot.
        resolve_path(workspace_path, &[common_dir])
    };
    add_auto_refresh_watch_path(&mut paths, &resolved_common_dir, true);
    paths
}

/// Node strips the `\\?\` prefix from `fs.realpath`, but `canonicalize` keeps it on
/// Windows; leaving it in place would make every `relative` call report the path as
/// outside the repository.
pub fn strip_windows_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{}", rest.replace('/', "\\"));
    }
    if let Some(rest) = path.strip_prefix("\\\\?\\") {
        return rest.to_string();
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_scope_helpers_match_ts() {
        assert_eq!(normalize_workspace_in_repo_path(""), ".");
        assert_eq!(normalize_workspace_in_repo_path("apps/"), "apps");
        assert_eq!(normalize_workspace_in_repo_path("./apps"), "apps");
        assert_eq!(normalize_workspace_in_repo_path("/apps"), "apps");
        assert_eq!(normalize_workspace_in_repo_path("."), ".");
        assert!(is_path_in_workspace_scope("a/b.txt", "."));
        assert!(is_path_in_workspace_scope("apps/a/b.txt", "apps"));
        assert!(!is_path_in_workspace_scope("packages/a/b.txt", "apps"));
        assert_eq!(
            to_workspace_relative_git_path("apps/a.txt", "apps"),
            "a.txt"
        );
        assert_eq!(to_workspace_relative_git_path("apps", "apps"), ".");
        assert_eq!(to_workspace_relative_git_path("other", "apps"), "other");
    }

    #[test]
    fn watch_paths_keep_git_dir_and_resolved_common_dir() {
        let paths = build_auto_refresh_watch_paths("/repo", "/repo/.git", ".git");
        assert_eq!(paths.len(), 2, "TS keeps both string forms it produces");
        assert_eq!(paths[0]["path"], json!("/repo/.git"));
        assert_eq!(paths[0]["recursive"], json!(true));
        let external =
            build_auto_refresh_watch_paths("/repo", "/main/.git/worktrees/r", "/main/.git");
        assert_eq!(external[1]["path"], json!("/main/.git"));
    }

    #[test]
    fn normalize_watch_path_keeps_root_shapes() {
        assert_eq!(normalize_watch_path("/var/"), "/var");
        assert_eq!(normalize_watch_path("/"), "/");
        if cfg!(windows) {
            assert_eq!(normalize_watch_path("C:/"), "C:/");
            assert_eq!(normalize_watch_path("D:"), "D:");
            assert_eq!(normalize_watch_path("C:\\repo\\"), "C:\\repo");
        }
    }

    #[test]
    fn relative_path_walks_up_and_down() {
        if cfg!(windows) {
            assert_eq!(
                relative_path("C:\\repo\\sub", "C:\\repo\\a\\b.txt"),
                "..\\a\\b.txt"
            );
            assert_eq!(relative_path("C:\\REPO", "C:\\repo\\a"), "a");
            assert_eq!(relative_path("C:\\repo", "C:\\repo"), "");
            assert_eq!(relative_path("C:\\repo\\a", "C:\\repo"), "..");
            assert_eq!(resolve_path("C:/repo", &["a/b.txt"]), "C:\\repo\\a\\b.txt");
            assert_eq!(
                repo_relative_to_absolute("C:/repo", "sub/a.txt"),
                "C:\\repo\\sub\\a.txt"
            );
            assert_eq!(normalize_absolute("C:/a/./b/../c/"), "C:\\a\\c");
        } else {
            assert_eq!(relative_path("/repo/sub", "/repo/a/b.txt"), "../a/b.txt");
            assert_eq!(relative_path("/repo", "/repo"), "");
            assert_eq!(resolve_path("/repo", &["a/b.txt"]), "/repo/a/b.txt");
            assert_eq!(normalize_absolute("/a/./b/../c/"), "/a/c");
        }
    }

    #[test]
    fn is_absolute_covers_platform_shapes() {
        assert!(is_absolute(if cfg!(windows) { "C:\\x" } else { "/x" }));
        assert!(!is_absolute("relative/x"));
        if cfg!(windows) {
            assert!(is_absolute("/posix-like-root"));
            assert!(!is_absolute("C:relative"));
        }
    }
}
