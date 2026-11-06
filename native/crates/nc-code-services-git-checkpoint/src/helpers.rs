//! The Rust counterpart of `gitCheckpointHelpers.ts`: parse git's `-z` output and synthesize the checkpoint diff.
//!
//! Each function states its TS source at the top; `to_workspace_relative_git_path` / `repo_relative_to_absolute`
//! directly reuse the path utilities from `nc-code-services-git` (the repository has only one set of path rules).

use std::collections::HashMap;

use nc_code_services_git::path::{repo_relative_to_absolute, to_workspace_relative_git_path};
use serde_json::{json, Value};

/// `getWorkspacePathspec` (gitCheckpointHelpers.ts:23).
pub fn workspace_pathspec(workspace_in_repo_path: &str) -> &str {
    workspace_in_repo_path
}

/// `getCheckpointRefName` (gitCheckpointHelpers.ts:27): the workspace hash is computed by the caller
/// as `sha256(key)[..12]` and passed in (AppPaths::workspace_hash uses the same algorithm as TS getWorkspaceHash).
pub fn checkpoint_ref_name(workspace_hash: &str, checkpoint_id: &str) -> String {
    format!("refs/zcode/checkpoints/{workspace_hash}/{checkpoint_id}")
}

pub struct NameStatusEntry {
    pub kind: &'static str,
    pub path: String,
    pub original_path: Option<String>,
}

/// `mapNameStatusKind` (gitCheckpointHelpers.ts:33): A→added, D→deleted, R/C→renamed, everything else modified.
fn map_name_status_kind(status: &str) -> &'static str {
    let normalized = status.chars().next().unwrap_or('M');
    match normalized {
        'A' => "added",
        'D' => "deleted",
        'R' | 'C' => "renamed",
        _ => "modified",
    }
}

/// `parseNameStatus` (gitCheckpointHelpers.ts:43): `-z` records are separated by NUL;
/// a renamed record occupies three segments (status / old path / new path).
pub fn parse_name_status(stdout: &str) -> Vec<NameStatusEntry> {
    let records: Vec<&str> = stdout
        .split('\0')
        .filter(|record| !record.is_empty())
        .collect();
    let mut entries: Vec<NameStatusEntry> = Vec::new();
    let mut index = 0usize;
    while index < records.len() {
        let status = records[index];
        let kind = map_name_status_kind(status);
        if kind == "renamed" {
            let original = records.get(index + 1).copied();
            let path = records.get(index + 2).copied();
            index += 2;
            match (original, path) {
                (Some(original), Some(path)) => {
                    entries.push(NameStatusEntry {
                        kind,
                        path: normalized(path),
                        original_path: Some(normalized(original)),
                    });
                }
                _ => {
                    // TS: drop this record if any segment is missing (`continue`).
                }
            }
        } else {
            let path = records.get(index + 1).copied();
            index += 1;
            if let Some(path) = path {
                entries.push(NameStatusEntry {
                    kind,
                    path: normalized(path),
                    original_path: None,
                });
            }
        }
        index += 1;
    }
    entries
}

fn normalized(path: &str) -> String {
    nc_code_services_git::path::normalize_git_path(path)
}

/// `parseNumstat` (gitCheckpointHelpers.ts:73): a `-z` record is `added\tremoved\tpath`;
/// for a rename record the path segment is empty and the following two segments are the old/new paths (both sides are written into the stats).
pub fn parse_numstat(stdout: &str) -> HashMap<String, (i64, i64)> {
    let records: Vec<&str> = stdout
        .split('\0')
        .filter(|record| !record.is_empty())
        .collect();
    let mut stats: HashMap<String, (i64, i64)> = HashMap::new();
    let mut index = 0usize;
    while index < records.len() {
        let record = records[index];
        let fields: Vec<&str> = record.split('\t').collect();
        if fields.len() < 3 {
            index += 1;
            continue;
        }
        let parse_count = |raw: &str| -> i64 {
            if raw == "-" {
                0
            } else {
                raw.parse::<i64>().unwrap_or(0)
            }
        };
        let added = parse_count(fields[0]);
        let removed = parse_count(fields[1]);
        let path_field = fields[2..].join("\t");
        if !path_field.is_empty() {
            stats.insert(normalized(&path_field), (added, removed));
            index += 1;
            continue;
        }
        let original = records.get(index + 1).copied().unwrap_or("");
        let renamed = records.get(index + 2).copied().unwrap_or("");
        index += 2;
        if renamed.is_empty() {
            index += 1;
            continue;
        }
        stats.insert(normalized(renamed), (added, removed));
        if !original.is_empty() {
            stats.insert(normalized(original), (added, removed));
        }
        index += 1;
    }
    stats
}

pub struct TreeEntry {
    pub mode: String,
    pub object_id: String,
}

/// `parseLsTree` (gitCheckpointHelpers.ts:107): `<mode> <type> <oid>\t<path>` (NUL separated).
pub fn parse_ls_tree(stdout: &str) -> HashMap<String, TreeEntry> {
    let mut entries = HashMap::new();
    for record in stdout.split('\0').filter(|record| !record.is_empty()) {
        let Some(tab) = record.find('\t') else {
            continue;
        };
        let header: Vec<&str> = record[..tab].split(' ').collect();
        if header.len() < 3 {
            continue;
        }
        let path = normalized(&record[tab + 1..]);
        entries.insert(
            path.clone(),
            TreeEntry {
                mode: header.first().copied().unwrap_or("100644").to_string(),
                object_id: header.get(2).copied().unwrap_or("").to_string(),
            },
        );
    }
    entries
}

/// `mergeCheckpointDiff` (gitCheckpointHelpers.ts:134): name-status decides the file set,
/// numstat provides added/removed; both `path` and `originalPath` are absolute paths.
pub fn merge_checkpoint_diff(
    repo_root: &str,
    workspace_in_repo_path: &str,
    from_checkpoint_id: &str,
    to_checkpoint_id: &str,
    name_status_entries: Vec<NameStatusEntry>,
    numstat: &HashMap<String, (i64, i64)>,
) -> Value {
    let files: Vec<Value> = name_status_entries
        .iter()
        .map(|entry| {
            let (added, removed) = numstat
                .get(&entry.path)
                .or_else(|| entry.original_path.as_deref().and_then(|path| numstat.get(path)))
                .copied()
                .unwrap_or((0, 0));
            json!({
                "path": repo_relative_to_absolute(repo_root, &entry.path),
                "repoRelativePath": entry.path,
                "workspaceRelativePath": to_workspace_relative_git_path(&entry.path, workspace_in_repo_path),
                "originalPath": entry
                    .original_path
                    .as_deref()
                    .map(|path| repo_relative_to_absolute(repo_root, path)),
                "kind": entry.kind,
                "added": added,
                "removed": removed,
            })
        })
        .collect();
    json!({
        "fromCheckpointId": from_checkpoint_id,
        "toCheckpointId": to_checkpoint_id,
        "files": files,
    })
}

/// `buildAffectedRepoPaths` (gitCheckpointHelpers.ts:177) plus `normalizeAffectedRepoPath`
/// (:201): an absolute originalPath is first folded back into a repo-relative path, then deduped before being handed to conflict detection.
pub fn build_affected_repo_paths(repo_root: &str, files: &[Value]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for file in files {
        if let Some(path) = file.get("repoRelativePath").and_then(Value::as_str) {
            let path = normalize_affected_repo_path(repo_root, path);
            if !seen.contains(&path) {
                seen.push(path);
            }
        }
        if let Some(original) = file.get("originalPath").and_then(Value::as_str) {
            let path = normalize_affected_repo_path(repo_root, original);
            if !seen.contains(&path) {
                seen.push(path);
            }
        }
    }
    seen
}

/// TS `normalizeAffectedRepoPath`: for an absolute path, strip the repoRoot prefix then normalize.
pub fn normalize_affected_repo_path(repo_root: &str, path: &str) -> String {
    let looks_absolute = path.starts_with('/') || (path.len() > 2 && path.as_bytes()[1] == b':');
    if !looks_absolute {
        return normalized(path);
    }
    let root = normalized(repo_root);
    let full = normalized(path);
    let stripped = full
        .strip_prefix(&format!("{root}/"))
        .or_else(|| full.strip_prefix(&root))
        .unwrap_or(&full);
    normalized(stripped)
}

/// The absolute paths the restore phase must delete: the paths of deleted entries plus the old paths of renamed entries
/// (`git restore` only restores the new path, so the old path must be deleted explicitly — see gitCheckpointRepo.ts:437-451).
pub fn paths_to_remove_after_restore(files: &[Value]) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for file in files {
        let kind = file.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind == "deleted" {
            if let Some(path) = file.get("path").and_then(Value::as_str) {
                if !paths.iter().any(|existing| existing == path) {
                    paths.push(path.to_string());
                }
            }
            continue;
        }
        if kind == "renamed" {
            if let Some(original) = file.get("originalPath").and_then(Value::as_str) {
                if !paths.iter().any(|existing| existing == original) {
                    paths.push(original.to_string());
                }
            }
        }
    }
    paths
}

/// The repo-relative paths the restore phase must write back (`git restore --worktree`, with deleted excluded).
pub fn restore_repo_paths(files: &[Value]) -> Vec<String> {
    files
        .iter()
        .filter(|file| file.get("kind").and_then(Value::as_str) != Some("deleted"))
        .filter_map(|file| file.get("repoRelativePath").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}
