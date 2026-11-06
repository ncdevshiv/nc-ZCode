//! `git` channel dispatch — port of `packages/services/src/git/gitService.ts`.
//!
//! Does only what the TS facade layer does: carve the summary / staged / unstaged
//! datasets out of a single `git status` snapshot (the comment at `gitService.ts:330`: avoid the
//! renderer running status three times for three RPCs), and keeps server-side scope pruning
//! (monorepo subdirectory workspace) in this layer; `repo` still
//! only parses raw Git state. The commit-message session file filter is ported
//! entry by entry from `commitMessageFileScope.ts`.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::{ServiceArgs, ServiceError};

use crate::commit_message::CommitMessageGenerator;
use crate::path::{
    is_path_in_workspace_scope, repo_relative_to_absolute, to_workspace_relative_git_path,
};
use crate::repo::{BranchComparisonSnapshot, ComparisonChange, GitRepo, StatusSnapshot};

/// `COMMIT_MESSAGE_DIFF_FILE_LIMIT`（`gitService.ts:143`）。
const COMMIT_MESSAGE_DIFF_FILE_LIMIT: usize = 8;

pub struct GitChannelService {
    repo: Arc<GitRepo>,
    messages: Arc<dyn CommitMessageGenerator>,
}

impl GitChannelService {
    pub fn new(repo: Arc<GitRepo>, messages: Arc<dyn CommitMessageGenerator>) -> Arc<Self> {
        Arc::new(Self { repo, messages })
    }

    fn workspace_path(params: &Value) -> Result<String, ServiceError> {
        params
            .get("workspacePath")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ServiceError::invalid_params("workspacePath is required"))
    }

    fn string_list(params: &Value, key: &str) -> Vec<String> {
        params
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn string_array(params: &Value, key: &str) -> Vec<Value> {
        params
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// `getChangesForSource`（`gitService.ts:114`）。
    async fn changes_for_source(&self, status: &StatusSnapshot, source_id: &str) -> Vec<Value> {
        let _ = status;
        status
            .entries
            .iter()
            .filter_map(|entry| to_change_for_source(status, entry, source_id))
            .collect()
    }
}

/// `matchesWorkspaceScope`（`gitService.ts:16`）。
fn matches_workspace_scope(snapshot: &StatusSnapshot, entry: &crate::parse::StatusEntry) -> bool {
    let workspace_in_repo = workspace_in_repo_path(snapshot);
    if is_path_in_workspace_scope(&entry.path, &workspace_in_repo) {
        return true;
    }
    entry
        .original_path
        .as_deref()
        .is_some_and(|original| is_path_in_workspace_scope(original, &workspace_in_repo))
}

fn workspace_in_repo_path(snapshot: &StatusSnapshot) -> String {
    snapshot
        .summary
        .get("workspaceInRepoPath")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `buildFileChange` (`gitService.ts:28`). In TS `x`/`y` are `?? undefined`,
/// i.e. the key is absent when missing; here the key is likewise inserted only when it has a value.
fn build_file_change(
    snapshot: &StatusSnapshot,
    entry: &crate::parse::StatusEntry,
    section: &str,
    added: i64,
    removed: i64,
) -> Value {
    let workspace_in_repo = workspace_in_repo_path(snapshot);
    let mut object = Map::new();
    object.insert(
        "path".into(),
        json!(repo_relative_to_absolute(
            snapshot.resolution.root().to_string_lossy().as_ref(),
            &entry.path
        )),
    );
    object.insert("repoRelativePath".into(), json!(entry.path));
    object.insert(
        "workspaceRelativePath".into(),
        json!(to_workspace_relative_git_path(
            &entry.path,
            &workspace_in_repo
        )),
    );
    if let Some(x) = &entry.x {
        object.insert("x".into(), json!(x));
    }
    if let Some(y) = &entry.y {
        object.insert("y".into(), json!(y));
    }
    object.insert("kind".into(), json!(entry.kind));
    object.insert("section".into(), json!(section));
    object.insert("added".into(), json!(added));
    object.insert("removed".into(), json!(removed));
    object.insert("isStaged".into(), json!(section == "staged"));
    object.insert("isUntracked".into(), json!(section == "untracked"));
    object.insert("isConflicted".into(), json!(section == "conflicted"));
    Value::Object(object)
}

/// `shouldIncludeUntrackedChange` (`gitService.ts:56`): an empty 0/0 artifact does not enter Review,
/// but a directory entry ending in `/` must be kept as the degraded entry point for stage/discard.
fn should_include_untracked_change(path: &str, added: i64, removed: i64) -> bool {
    path.ends_with('/') || added > 0 || removed > 0
}

/// `toChangeForSource`（`gitService.ts:70`）。
fn to_change_for_source(
    snapshot: &StatusSnapshot,
    entry: &crate::parse::StatusEntry,
    source_id: &str,
) -> Option<Value> {
    if !matches_workspace_scope(snapshot, entry) {
        return None;
    }
    if source_id == "staged" {
        if entry.is_untracked || entry.is_conflicted || entry.x.is_none() {
            return None;
        }
        if entry.x.as_deref() == Some(".") {
            return None;
        }
        let stat = snapshot
            .staged_stats
            .get(&entry.path)
            .map(|stat| (stat.added, stat.removed))
            .unwrap_or((0, 0));
        return Some(build_file_change(snapshot, entry, "staged", stat.0, stat.1));
    }
    if entry.is_conflicted {
        return Some(build_file_change(snapshot, entry, "conflicted", 0, 0));
    }
    if entry.is_untracked {
        let (added, removed) = snapshot
            .untracked_stats
            .get(&entry.path)
            .map(|stat| (stat.added, stat.removed))
            .unwrap_or((0, 0));
        if !should_include_untracked_change(&entry.path, added, removed) {
            return None;
        }
        return Some(build_file_change(
            snapshot,
            entry,
            "untracked",
            added,
            removed,
        ));
    }
    if entry.y.is_none() || entry.y.as_deref() == Some(".") {
        return None;
    }
    let (added, removed) = snapshot
        .unstaged_stats
        .get(&entry.path)
        .map(|stat| (stat.added, stat.removed))
        .unwrap_or((0, 0));
    Some(build_file_change(
        snapshot, entry, "unstaged", added, removed,
    ))
}

/// `toBranchComparisonChange`（`gitService.ts:109`）。
fn to_branch_comparison_change(
    snapshot: &BranchComparisonSnapshot,
    change: &ComparisonChange,
) -> Option<Value> {
    let workspace_in_repo = snapshot.resolution.workspace_in_repo_path.clone();
    if !is_path_in_workspace_scope(&change.path, &workspace_in_repo) {
        return None;
    }
    let repo_root = snapshot.resolution.root().to_string_lossy().into_owned();
    Some(json!({
        "path": repo_relative_to_absolute(&repo_root, &change.path),
        "repoRelativePath": change.path,
        "workspaceRelativePath": to_workspace_relative_git_path(&change.path, &workspace_in_repo),
        "kind": change.kind,
        "section": "branch",
        "added": change.added,
        "removed": change.removed,
        "isStaged": false,
        "isUntracked": false,
        "isConflicted": false,
    }))
}

/// `normalizeCommitMessageScopePath`（`commitMessageFileScope.ts:5`）。
fn normalize_scope_path(path: &str) -> String {
    let trimmed = path.trim();
    let mut normalized = crate::path::normalize_git_path(trimmed);
    while let Some(rest) = normalized.strip_prefix("./") {
        normalized = rest.to_string();
    }
    if normalized.starts_with('/') {
        normalized.remove(0);
    }
    normalized.trim_end_matches('/').to_string()
}

/// `isRelativePathInsideScope`（`commitMessageFileScope.ts:11`）。
fn is_relative_path_inside_scope(path: &str) -> bool {
    let normalized = crate::path::normalize_git_path(path);
    !normalized.is_empty()
        && normalized != ".."
        && !normalized.starts_with("../")
        && !crate::path::is_absolute(&normalized)
}

/// `buildCommitMessageFileScope` (`commitMessageFileScope.ts:29`). Returning `None` means
/// "no restriction" (in TS this is `scope === null` -> everything passes).
fn build_commit_message_file_scope(
    workspace_path: &str,
    repo_root: &str,
    workspace_in_repo_path: &str,
    current_session_paths: &[Value],
) -> Option<HashSet<String>> {
    let source_paths: Vec<String> = current_session_paths
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    if source_paths.is_empty() {
        return None;
    }
    let mut scope: HashSet<String> = HashSet::new();
    let normalized_workspace =
        crate::path::normalize_workspace_in_repo_path(workspace_in_repo_path);
    let add = |scope: &mut HashSet<String>, path: &str| {
        let normalized = normalize_scope_path(path);
        if !normalized.is_empty() {
            scope.insert(normalized);
        }
    };
    for path in &source_paths {
        add(&mut scope, path);
        if crate::path::is_absolute(path) {
            let repo_relative = relative_path(repo_root, path);
            if is_relative_path_inside_scope(&repo_relative) {
                add(&mut scope, &repo_relative);
            }
            let workspace_relative = relative_path(workspace_path, path);
            if is_relative_path_inside_scope(&workspace_relative) {
                add(&mut scope, &workspace_relative);
            }
            continue;
        }
        let normalized = normalize_scope_path(path);
        if normalized_workspace != "."
            && !normalized.starts_with(&format!("{normalized_workspace}/"))
        {
            add(&mut scope, &format!("{normalized_workspace}/{normalized}"));
        }
    }
    (!scope.is_empty()).then_some(scope)
}

/// The relative path of `relative(base, path)` (node:path.relative): Windows-only semantics that split on the separator
/// (the repo's other path utilities use this same convention).
fn relative_path(base: &str, path: &str) -> String {
    // replace produces a temporary String: bind it first so split borrows a stable value (otherwise E0716).
    let base_text = base.replace('\\', "/");
    let path_text = path.replace('\\', "/");
    let base_parts: Vec<&str> = base_text
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let path_parts: Vec<&str> = path_text
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let mut shared = 0;
    while shared < base_parts.len()
        && shared < path_parts.len()
        && base_parts[shared].eq_ignore_ascii_case(path_parts[shared])
    {
        shared += 1;
    }
    let mut parts: Vec<String> = vec!["..".to_string(); base_parts.len() - shared];
    parts.extend(path_parts[shared..].iter().map(|part| part.to_string()));
    parts.join("/")
}

/// `isCommitMessageFileInScope` (`commitMessageFileScope.ts:73`) plus the filtering wrapper.
fn filter_commit_message_files_by_current_session(
    files: &[Value],
    workspace_path: &str,
    repo_root: &str,
    workspace_in_repo_path: &str,
    current_session_paths: &[Value],
) -> Vec<Value> {
    let scope = build_commit_message_file_scope(
        workspace_path,
        repo_root,
        workspace_in_repo_path,
        current_session_paths,
    );
    let Some(scope) = scope else {
        return files.to_vec();
    };
    files
        .iter()
        .filter(|file| {
            ["path", "repoRelativePath", "workspaceRelativePath"]
                .into_iter()
                .any(|key| {
                    file.get(key)
                        .and_then(Value::as_str)
                        .is_some_and(|value| scope.contains(&normalize_scope_path(value)))
                })
        })
        .cloned()
        .collect()
}

/// `getCommitMessageDiffQueries`（`gitService.ts:145`）。
fn commit_message_diff_queries(
    files: &[Value],
    include_unstaged: bool,
) -> Vec<(String, &'static str)> {
    let mut queries = Vec::new();
    for file in files.iter().take(COMMIT_MESSAGE_DIFF_FILE_LIMIT) {
        let Some(path) = file.get("path").and_then(Value::as_str) else {
            continue;
        };
        let section = file.get("section").and_then(Value::as_str).unwrap_or("");
        if section == "staged" {
            queries.push((path.to_string(), "staged"));
            continue;
        }
        if !include_unstaged {
            continue;
        }
        queries.push((path.to_string(), "unstaged"));
    }
    queries
}

#[async_trait::async_trait]
impl Channel for GitChannelService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "getRepositorySummary" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let status = self.repo.get_status(&workspace_path).await?;
                Ok(WireValue::Json(status.summary))
            }
            "getWorkspaceRepositoryInfo" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                Ok(WireValue::Json(
                    self.repo
                        .get_workspace_repository_info(&workspace_path)
                        .await,
                ))
            }
            "getLocalBranches" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                Ok(WireValue::Json(
                    self.repo.list_local_branches(&workspace_path).await?,
                ))
            }
            "getCommitGraph" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let max_count = params.get("maxCount").and_then(Value::as_i64);
                let skip = params.get("skip").and_then(Value::as_i64);
                Ok(WireValue::Json(
                    self.repo
                        .get_commit_graph(&workspace_path, max_count, skip)
                        .await?,
                ))
            }
            "switchBranch" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let target = params
                    .get("targetBranchName")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(WireValue::Json(
                    self.repo.switch_branch(&workspace_path, target).await?,
                ))
            }
            "createBranchAndSwitch" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let branch_name = params
                    .get("branchName")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let start_point = params.get("startPoint").and_then(Value::as_str);
                Ok(WireValue::Json(
                    self.repo
                        .create_branch_and_switch(&workspace_path, branch_name, start_point)
                        .await?,
                ))
            }
            "getChanges" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let source_id = params
                    .get("sourceId")
                    .and_then(Value::as_str)
                    .unwrap_or("unstaged");
                let status = self.repo.get_status(&workspace_path).await?;
                let changes = self.changes_for_source(&status, source_id).await;
                Ok(WireValue::Json(Value::Array(changes)))
            }
            "getIgnoredPaths" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let paths = Self::string_list(&params, "paths");
                Ok(WireValue::Json(json!(
                    self.repo.get_ignored_paths(&workspace_path, &paths).await?
                )))
            }
            "getDiff" => {
                let params = args.required_json(0, "params")?;
                Ok(WireValue::Json(self.repo.get_diff(&params).await?))
            }
            "getBranchComparison" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let snapshot = self.repo.get_branch_comparison(&workspace_path).await?;
                let changes = snapshot
                    .changes
                    .iter()
                    .filter_map(|change| to_branch_comparison_change(&snapshot, change))
                    .collect::<Vec<Value>>();
                Ok(WireValue::Json(json!({
                    "baseRef": snapshot.base_ref,
                    "headRef": snapshot.head_ref,
                    "comparisonLabel": snapshot.comparison_label,
                    "changes": changes,
                })))
            }
            "stagePaths" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let paths = Self::string_list(&params, "paths");
                self.repo.stage(&workspace_path, &paths).await?;
                Ok(WireValue::Json(Value::Null))
            }
            "unstagePaths" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let paths = Self::string_list(&params, "paths");
                self.repo.unstage(&workspace_path, &paths).await?;
                Ok(WireValue::Json(Value::Null))
            }
            "discardPaths" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let paths = Self::string_list(&params, "paths");
                let staged = params
                    .get("staged")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.repo.discard(&workspace_path, &paths, staged).await?;
                Ok(WireValue::Json(Value::Null))
            }
            "generateCommitMessage" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let include_unstaged = params
                    .get("includeUnstaged")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let status = self.repo.get_status(&workspace_path).await?;
                let workspace_in_repo = workspace_in_repo_path(&status);
                let unstaged = if include_unstaged {
                    self.changes_for_source(&status, "unstaged").await
                } else {
                    Vec::new()
                };
                let staged = self.changes_for_source(&status, "staged").await;
                let mut files: Vec<Value> = staged;
                files.extend(unstaged);
                let current_session_paths = Self::string_array(&params, "currentSessionFilePaths");
                let files = filter_commit_message_files_by_current_session(
                    &files,
                    &workspace_path,
                    status.resolution.root().to_string_lossy().as_ref(),
                    &workspace_in_repo,
                    &current_session_paths,
                );
                if files.is_empty() {
                    return Err(
                        ServiceError::message("There are no changes available to commit.").into(),
                    );
                }
                let _repo_root = status.resolution.root().to_string_lossy().into_owned();
                let queries = commit_message_diff_queries(&files, include_unstaged);
                let mut diffs: Vec<Value> = Vec::new();
                for (path, source_id) in queries {
                    let diff_params = json!({
                        "workspacePath": workspace_path,
                        "path": path,
                        "sourceId": source_id,
                    });
                    // Skip a single failing diff (TS uses Promise.allSettled and takes only the fulfilled items).
                    if let Ok(diff) = self.repo.get_diff(&diff_params).await {
                        let usable = diff
                            .get("patch")
                            .and_then(Value::as_str)
                            .is_some_and(|p| !p.is_empty())
                            || diff
                                .get("summary")
                                .and_then(Value::as_str)
                                .is_some_and(|s| !s.is_empty());
                        if usable {
                            diffs.push(diff);
                        }
                    }
                }
                let branch_name = status
                    .summary
                    .get("branchName")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let workspace_identity = params.get("workspaceIdentity").and_then(Value::as_str);
                let locale = params.get("locale").and_then(Value::as_str);
                let conversation_context = params.get("conversationContext");
                let message = self
                    .messages
                    .generate(&crate::commit_message::GenerateParams {
                        workspace_path: &workspace_path,
                        workspace_identity,
                        locale,
                        branch_name: branch_name.as_deref(),
                        files: &files,
                        diffs: &diffs,
                        conversation_context,
                    })
                    .await?;
                Ok(WireValue::Json(message))
            }
            "commit" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let paths = Self::string_list(&params, "paths");
                let staged_only = params
                    .get("stagedOnly")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let paths = (!paths.is_empty()).then_some(paths.as_slice());
                let result = self
                    .repo
                    .commit(&workspace_path, message, paths, staged_only)
                    .await?;
                let status = self.repo.get_status(&workspace_path).await?;
                let commit_hash = result.get("commitHash").cloned().unwrap_or(Value::Null);
                Ok(WireValue::Json(json!({
                    "commitHash": commit_hash,
                    "summary": status.summary,
                })))
            }
            "push" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                Ok(WireValue::Json(self.repo.push(&workspace_path).await?))
            }
            "getIdentity" => {
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                Ok(WireValue::Json(
                    self.repo.get_identity(&workspace_path).await?,
                ))
            }
            "refresh" => {
                // TS: the UI's auto-refresh originally ran the summary/staged/unstaged three RPCs concurrently (each
                // re-running git status); refresh carves all datasets out of a single snapshot (gitService.ts:327).
                let params = args.required_json(0, "params")?;
                let workspace_path = Self::workspace_path(&params)?;
                let include_identity = params
                    .get("includeIdentity")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let include_branch_comparison = params
                    .get("includeBranchComparison")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let status = self.repo.get_status(&workspace_path).await?;
                let identity = if include_identity {
                    self.repo.get_identity(&workspace_path).await?
                } else {
                    Value::Null
                };
                let branch_comparison = if include_branch_comparison {
                    let snapshot = self.repo.get_branch_comparison(&workspace_path).await?;
                    let changes = snapshot
                        .changes
                        .iter()
                        .filter_map(|change| to_branch_comparison_change(&snapshot, change))
                        .collect::<Vec<Value>>();
                    json!({
                        "baseRef": snapshot.base_ref,
                        "headRef": snapshot.head_ref,
                        "comparisonLabel": snapshot.comparison_label,
                        "changes": changes,
                    })
                } else {
                    Value::Null
                };
                let unstaged_changes = self.changes_for_source(&status, "unstaged").await;
                let staged_changes = self.changes_for_source(&status, "staged").await;
                Ok(WireValue::Json(json!({
                    "summary": status.summary,
                    "identity": identity,
                    "unstagedChanges": unstaged_changes,
                    "stagedChanges": staged_changes,
                    "branchComparison": branch_comparison,
                })))
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nc_code_channels::ClientMode;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::process::Command as TokioCommand;

    fn ctx() -> ChannelContext {
        ChannelContext::new("git", ClientMode::DesktopContinuous)
    }

    async fn setup_repo() -> (TempDir, Arc<GitChannelService>) {
        let tmp = TempDir::new().unwrap();
        let repo_path = tmp.path().to_path_buf();
        TokioCommand::new("git")
            .args(["init"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();
        TokioCommand::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();
        TokioCommand::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();
        std::fs::write(repo_path.join("README.md"), "hello\n").unwrap();
        TokioCommand::new("git")
            .args(["add", "README.md"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();
        TokioCommand::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();

        let repo = crate::repo::GitRepo::new(crate::cmd::GitCommandProvider::new());
        // Use a dummy generator that returns a fixed message
        struct DummyGen;
        #[async_trait::async_trait]
        impl crate::commit_message::CommitMessageGenerator for DummyGen {
            async fn generate(
                &self,
                _params: &crate::commit_message::GenerateParams<'_>,
            ) -> Result<serde_json::Value, nc_code_service_core::ServiceError> {
                Ok(serde_json::json!({"message": "chore: update"}))
            }
        }
        let service = GitChannelService::new(repo, Arc::new(DummyGen));
        (tmp, service)
    }

    #[tokio::test]
    async fn get_repository_summary_requires_workspace_path() {
        let repo = crate::repo::GitRepo::new(crate::cmd::GitCommandProvider::new());
        struct DummyGen;
        #[async_trait::async_trait]
        impl crate::commit_message::CommitMessageGenerator for DummyGen {
            async fn generate(
                &self,
                _params: &crate::commit_message::GenerateParams<'_>,
            ) -> Result<serde_json::Value, nc_code_service_core::ServiceError> {
                Ok(serde_json::json!({"message": "chore: update"}))
            }
        }
        let service = GitChannelService::new(repo, Arc::new(DummyGen));
        let res = service
            .call(&ctx(), "getRepositorySummary", vec![], CancelToken::new())
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn get_repository_summary_on_real_repo() {
        let (_tmp, service) = setup_repo().await;
        let res = service
            .call(
                &ctx(),
                "getRepositorySummary",
                vec![WireValue::Json(
                    serde_json::json!({"workspacePath": _tmp.path().to_string_lossy()}),
                )],
                CancelToken::new(),
            )
            .await
            .unwrap();
        if let WireValue::Json(v) = res {
            assert!(v.is_object());
        } else {
            panic!("bad response");
        }
    }

    #[tokio::test]
    async fn get_identity_on_real_repo() {
        let (_tmp, service) = setup_repo().await;
        let res = service
            .call(
                &ctx(),
                "getIdentity",
                vec![WireValue::Json(
                    serde_json::json!({"workspacePath": _tmp.path().to_string_lossy()}),
                )],
                CancelToken::new(),
            )
            .await
            .unwrap();
        if let WireValue::Json(v) = res {
            assert!(v.is_object());
        } else {
            panic!("bad response");
        }
    }

    #[tokio::test]
    async fn refresh_on_real_repo() {
        let (_tmp, service) = setup_repo().await;
        let res = service
            .call(
                &ctx(),
                "refresh",
                vec![WireValue::Json(
                    serde_json::json!({"workspacePath": _tmp.path().to_string_lossy()}),
                )],
                CancelToken::new(),
            )
            .await
            .unwrap();
        if let WireValue::Json(v) = res {
            assert!(v.get("summary").is_some());
        } else {
            panic!("bad response");
        }
    }
}
