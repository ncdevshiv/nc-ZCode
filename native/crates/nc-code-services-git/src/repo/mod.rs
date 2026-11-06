//! Repository reads: resolution, status snapshot, refs, graph, diff, identity.
//!
//! Port of `packages/services/src/git/repo/gitCliRepo.ts`. TS coalesces in-flight requests
//! per workspace (`reuseInFlightRequest`); that is a subprocess-count optimisation, not an
//! observable result, so the Rust side recomputes instead of keeping a shared promise map.
//! The `collapsedUntrackedRepoRoots` set *is* observable (it changes which Git command
//! runs) and is ported.

pub mod mutation;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use nc_code_service_core::ServiceError;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::cmd::{GitCommand, GitCommandProvider, GitCommandResult, DIFF_BYTES, DIFF_TIMEOUT};
use crate::parse::{
    ensure_git_command_succeeded, infer_kind_from_numstat, is_missing_working_directory_result,
    is_not_repository_result, parse_git_config_value, parse_numstat, parse_status_porcelain,
    to_diff_result, unavailable_diff, LineStat, OrderedStats, StatusEntry,
};
use crate::path::{
    build_auto_refresh_watch_paths, is_absolute, normalize_git_path,
    normalize_workspace_in_repo_path, relative_path, resolve_path, strip_windows_verbatim,
};
use crate::refs::{
    normalize_graph_max_count, normalize_graph_skip, parse_branch_ref_records, parse_graph_records,
};
use crate::worktree::{
    build_untracked_stats, build_untracked_text_diff_result, file_exists, null_device_path,
    read_git_blob_preview_content, read_working_tree_preview_content, to_complete_diff_contents,
    with_diff_contents,
};

const GIT_OPERATION_MARKERS: [&str; 7] = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "REBASE_HEAD",
    "rebase-merge",
    "rebase-apply",
    "BISECT_LOG",
];

#[derive(Debug, Clone)]
pub struct ResolvedRepository {
    pub workspace_path: String,
    pub repo_root: String,
    pub workspace_in_repo_path: String,
    pub auto_refresh_watch_paths: Vec<Value>,
    pub is_git_available: bool,
    pub is_repository: bool,
}

impl ResolvedRepository {
    pub fn unavailable(workspace_path: &str, is_git_available: bool) -> Self {
        Self {
            workspace_path: workspace_path.to_string(),
            repo_root: workspace_path.to_string(),
            workspace_in_repo_path: ".".to_string(),
            auto_refresh_watch_paths: Vec::new(),
            is_git_available,
            is_repository: false,
        }
    }

    pub fn root(&self) -> &Path {
        Path::new(&self.repo_root)
    }
}

/// `createEmptySummary` (`gitCliTypes.ts:104`) — the key order is part of the response.
pub fn create_empty_summary(resolution: &ResolvedRepository) -> Value {
    json!({
        "workspacePath": resolution.workspace_path,
        "repoRoot": resolution.repo_root,
        "workspaceInRepoPath": resolution.workspace_in_repo_path,
        "autoRefreshWatchPaths": resolution.auto_refresh_watch_paths.clone(),
        "branchName": Value::Null,
        "trackingBranchName": Value::Null,
        "headRefType": "branch",
        "ahead": 0,
        "behind": 0,
        "isDirty": false,
        "isGitAvailable": resolution.is_git_available,
        "isRepository": resolution.is_repository,
    })
}

pub struct StatusSnapshot {
    pub resolution: ResolvedRepository,
    pub summary: Value,
    pub entries: Vec<StatusEntry>,
    pub staged_stats: OrderedStats,
    pub unstaged_stats: OrderedStats,
    pub untracked_stats: OrderedStats,
}

pub struct ComparisonChange {
    pub path: String,
    pub original_path: Option<String>,
    pub kind: &'static str,
    pub added: i64,
    pub removed: i64,
}

pub struct BranchComparisonSnapshot {
    pub resolution: ResolvedRepository,
    pub base_ref: Option<String>,
    pub head_ref: Option<String>,
    pub comparison_label: Option<String>,
    pub changes: Vec<ComparisonChange>,
}

pub struct GitRepo {
    provider: Arc<GitCommandProvider>,
    collapsed_untracked_roots: Mutex<HashSet<String>>,
}

impl GitRepo {
    pub fn new(provider: Arc<GitCommandProvider>) -> Arc<Self> {
        Arc::new(Self {
            provider,
            collapsed_untracked_roots: Mutex::new(HashSet::new()),
        })
    }

    pub fn provider(&self) -> &Arc<GitCommandProvider> {
        &self.provider
    }

    /// `resolveRepository` (`gitCliRepo.ts:712`).
    pub async fn resolve_repository(&self, workspace_path: &str) -> ResolvedRepository {
        let git_binary = self.provider.resolve_git_binary().await;
        if git_binary.is_none() {
            return ResolvedRepository::unavailable(workspace_path, false);
        }
        let result = match self
            .provider
            .run(GitCommand::new(
                Path::new(workspace_path),
                &[
                    "rev-parse",
                    "--show-toplevel",
                    "--show-prefix",
                    "--absolute-git-dir",
                    "--git-common-dir",
                ],
            ))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                // Only reachable if the binary vanished between the probe and this call;
                // TS's provider throws here, the port degrades like a failed probe.
                let _ = error;
                return ResolvedRepository::unavailable(workspace_path, true);
            }
        };

        if result.exit_code != Some(0) {
            // A workspace directory removed while the request is in flight is not a Git
            // protocol failure; TS degrades it to “no usable repository” instead of
            // throwing, so a fire-and-forget caller cannot crash the host
            // (`gitCliRepo.ts:739-766`).
            if is_missing_working_directory_result(&result) || is_not_repository_result(&result) {
                return ResolvedRepository::unavailable(workspace_path, true);
            }
            let error = ensure_git_command_succeeded("git rev-parse", &result, &[0])
                .err()
                .unwrap_or_else(|| ServiceError::message("Failed to resolve Git repository root"));
            return ResolvedRepository {
                repo_root: error.message.clone(),
                ..ResolvedRepository::unavailable(workspace_path, true)
            };
        }

        let normalized_stdout = result.stdout.replace("\r\n", "\n");
        let lines: Vec<&str> = normalized_stdout.split('\n').collect();
        let repo_root = lines
            .first()
            .copied()
            .unwrap_or_default()
            .trim()
            .to_string();
        if repo_root.is_empty() {
            // `gitCliRepo.ts:771` throws; the observable client-facing behaviour is the
            // same degradation used for any other unusable repository.
            return ResolvedRepository::unavailable(workspace_path, true);
        }
        ResolvedRepository {
            workspace_path: workspace_path.to_string(),
            repo_root,
            workspace_in_repo_path: normalize_workspace_in_repo_path(
                lines.get(1).copied().unwrap_or_default(),
            ),
            auto_refresh_watch_paths: build_auto_refresh_watch_paths(
                workspace_path,
                lines.get(2).copied().unwrap_or_default(),
                lines.get(3).copied().unwrap_or_default(),
            ),
            is_git_available: true,
            is_repository: true,
        }
    }

    /// `ensureRepositoryAvailable` (`gitCliHelpers.ts:704`).
    pub fn ensure_repository_available(
        resolution: &ResolvedRepository,
        label: &str,
    ) -> Result<(), ServiceError> {
        if !resolution.is_git_available {
            return Err(ServiceError::message(format!(
                "Cannot {label}: Git binary is not available"
            )));
        }
        if !resolution.is_repository {
            return Err(ServiceError::message(format!(
                "Cannot {label}: workspace is not inside a Git repository"
            )));
        }
        Ok(())
    }

    /// `getWorkspaceRepositoryInfo` (`gitCliRepo.ts:789`). The `.git` file form only
    /// classifies linked worktrees; anything unusual falls through as `main-tree` because
    /// the caller filters migration candidates and must not lose valid records.
    pub async fn get_workspace_repository_info(&self, workspace_path: &str) -> Value {
        let resolution = self.resolve_repository(workspace_path).await;
        if !resolution.is_git_available || !resolution.is_repository {
            return json!({
                "workspacePath": workspace_path,
                "kind": "not-repository",
                "isGitAvailable": resolution.is_git_available,
            });
        }
        let main_tree = json!({
            "workspacePath": workspace_path,
            "kind": "main-tree",
            "isGitAvailable": true,
        });
        let git_entry = resolution.root().join(".git");
        let Ok(metadata) = tokio::fs::metadata(&git_entry).await else {
            return main_tree;
        };
        if metadata.is_dir() {
            return main_tree;
        }
        let Ok(content) = tokio::fs::read_to_string(&git_entry).await else {
            return main_tree;
        };
        let first_line = content.replace("\r\n", "\n");
        let first_line = first_line
            .split('\n')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let Some(raw_git_dir) = first_line.strip_prefix("gitdir:") else {
            return main_tree;
        };
        let raw_git_dir = raw_git_dir.trim();
        if raw_git_dir.is_empty() {
            return main_tree;
        }
        let resolved = if is_absolute(raw_git_dir) {
            raw_git_dir.to_string()
        } else {
            resolve_path(&resolution.repo_root, &[raw_git_dir])
        };
        if normalize_git_path(&resolved).contains("/.git/worktrees/") {
            return json!({
                "workspacePath": workspace_path,
                "kind": "linked-worktree",
                "isGitAvailable": true,
            });
        }
        main_tree
    }

    /// `runGitStatus` (`gitCliRepo.ts:555`): a huge repository can push per-file untracked
    /// output past the budget; after the first overflow that repoRoot stays in collapsed
    /// mode so auto-refresh does not repeat a command guaranteed to fail.
    async fn run_git_status(
        &self,
        resolution: &ResolvedRepository,
    ) -> Result<GitCommandResult, ServiceError> {
        let collapsed = self
            .collapsed_untracked_roots
            .lock()
            .await
            .contains(&resolution.repo_root);
        let untracked_flag = if collapsed {
            "--untracked-files=normal"
        } else {
            "--untracked-files=all"
        };
        let first = self
            .provider
            .run(GitCommand::new(
                resolution.root(),
                &["status", "--porcelain=v2", "--branch", untracked_flag, "-z"],
            ))
            .await?;
        if collapsed || !first.output_truncated {
            return Ok(first);
        }
        self.collapsed_untracked_roots
            .lock()
            .await
            .insert(resolution.repo_root.clone());
        self.provider
            .run(GitCommand::new(
                resolution.root(),
                &[
                    "status",
                    "--porcelain=v2",
                    "--branch",
                    "--untracked-files=normal",
                    "-z",
                ],
            ))
            .await
    }

    /// `getStatus` (`gitCliRepo.ts:855`).
    pub async fn get_status(&self, workspace_path: &str) -> Result<StatusSnapshot, ServiceError> {
        let resolution = self.resolve_repository(workspace_path).await;
        if !resolution.is_git_available || !resolution.is_repository {
            return Ok(StatusSnapshot {
                summary: create_empty_summary(&resolution),
                resolution,
                entries: Vec::new(),
                staged_stats: OrderedStats::default(),
                unstaged_stats: OrderedStats::default(),
                untracked_stats: OrderedStats::default(),
            });
        }
        let repo_root = resolution.root().to_path_buf();
        let status_result = self.run_git_status(&resolution).await?;
        let staged_stats_result = self
            .provider
            .run(GitCommand::new(
                &repo_root,
                &[
                    "diff",
                    "--cached",
                    "--numstat",
                    "-z",
                    "--find-renames",
                    "--",
                ],
            ))
            .await?;
        let unstaged_stats_result = self
            .provider
            .run(GitCommand::new(
                &repo_root,
                &["diff", "--numstat", "-z", "--find-renames", "--"],
            ))
            .await?;
        ensure_git_command_succeeded("git status", &status_result, &[0])?;
        ensure_git_command_succeeded("git diff --cached --numstat", &staged_stats_result, &[0])?;
        ensure_git_command_succeeded("git diff --numstat", &unstaged_stats_result, &[0])?;

        let parsed_status = parse_status_porcelain(&status_result.stdout);
        let untracked_stats = {
            let mut stats = OrderedStats::default();
            for (path, added) in build_untracked_stats(&repo_root, &parsed_status.entries).await {
                stats.set(
                    path,
                    LineStat {
                        added,
                        removed: 0,
                        kind: None,
                        original_path: None,
                    },
                );
            }
            stats
        };

        let summary = json!({
            "workspacePath": resolution.workspace_path,
            "repoRoot": resolution.repo_root,
            "workspaceInRepoPath": resolution.workspace_in_repo_path,
            "autoRefreshWatchPaths": resolution.auto_refresh_watch_paths,
            "branchName": parsed_status.branch_name,
            "trackingBranchName": parsed_status.tracking_branch_name,
            "headRefType": parsed_status.head_ref_type,
            "ahead": parsed_status.ahead,
            "behind": parsed_status.behind,
            "isDirty": !parsed_status.entries.is_empty(),
            "isGitAvailable": true,
            "isRepository": true,
        });

        Ok(StatusSnapshot {
            resolution,
            summary,
            entries: parsed_status.entries,
            staged_stats: parse_numstat(&staged_stats_result.stdout),
            unstaged_stats: parse_numstat(&unstaged_stats_result.stdout),
            untracked_stats,
        })
    }

    /// `getIgnoredPaths` (`gitCliRepo.ts:919`).
    pub async fn get_ignored_paths(
        &self,
        workspace_path: &str,
        paths: &[String],
    ) -> Result<Vec<String>, ServiceError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let resolution = self.resolve_repository(workspace_path).await;
        if !resolution.is_git_available || !resolution.is_repository {
            return Ok(Vec::new());
        }
        let mut valid: Vec<(String, String)> = Vec::new();
        for path in paths {
            let absolute = absolute_from_workspace(&resolution.workspace_path, path);
            // Paths the caller cannot resolve are dropped, exactly like the TS
            // `Promise.all(...).catch(() => null)`.
            if let Ok(repo_relative) = self.normalize_input_path(&resolution, path).await {
                valid.push((absolute, repo_relative));
            }
        }
        if valid.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<&str> = vec!["check-ignore", "--"];
        args.extend(
            valid
                .iter()
                .map(|(_, repo_relative)| repo_relative.as_str()),
        );
        let result = self
            .provider
            .run(GitCommand::new(resolution.root(), &args))
            .await?;
        if result.exit_code == Some(1) {
            return Ok(Vec::new());
        }
        ensure_git_command_succeeded("git check-ignore", &result, &[0])?;
        // `check-ignore -z` only works together with `--stdin`; the paths are passed as
        // argv here, so the plain newline output is what gets parsed (`gitCliRepo.ts:965`).
        let ignored: HashSet<String> = result
            .stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(normalize_git_path)
            .collect();
        Ok(valid
            .into_iter()
            .filter(|(_, repo_relative)| ignored.contains(repo_relative))
            .map(|(absolute, _)| absolute)
            .collect())
    }

    /// `listLocalBranches` (`gitCliRepo.ts:977`).
    pub async fn list_local_branches(&self, workspace_path: &str) -> Result<Value, ServiceError> {
        let status = self.get_status(workspace_path).await?;
        if !status.resolution.is_git_available || !status.resolution.is_repository {
            return Ok(json!({
                "headRefType": status.summary["headRefType"],
                "currentBranchName": status.summary["branchName"],
                "branches": [],
            }));
        }
        let result = self
            .provider
            .run(GitCommand::new(
                status.resolution.root(),
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--format=%(refname:short)%00%(upstream:short)%00%(objectname)%00%(committerdate:unix)",
                ],
            ))
            .await?;
        ensure_git_command_succeeded("git for-each-ref refs/heads", &result, &[0])?;
        // `isCurrent` only compares against a branch name, never against a detached HEAD.
        let current_branch = if status.summary["headRefType"] == json!("branch") {
            status.summary["branchName"].as_str()
        } else {
            None
        };
        Ok(json!({
            "headRefType": status.summary["headRefType"],
            "currentBranchName": status.summary["branchName"],
            "branches": parse_branch_ref_records(&result.stdout, current_branch),
        }))
    }

    /// `getCommitGraph` (`gitCliRepo.ts:1009`). `--branches --tags --remotes` limits the
    /// graph to user-visible history so internal hidden refs stay out of it.
    pub async fn get_commit_graph(
        &self,
        workspace_path: &str,
        max_count: Option<i64>,
        skip: Option<i64>,
    ) -> Result<Value, ServiceError> {
        let resolution = self.resolve_repository(workspace_path).await;
        if !resolution.is_git_available || !resolution.is_repository {
            return Ok(json!({"commits": [], "hasMore": false}));
        }
        let normalized_max_count = normalize_graph_max_count(max_count);
        let normalized_skip = normalize_graph_skip(skip);
        let max_count_argument = format!("--max-count={}", normalized_max_count + 1);
        let skip_argument = format!("--skip={normalized_skip}");
        let result = self
            .provider
            .run(GitCommand::new(
                resolution.root(),
                &[
                    "log",
                    "HEAD",
                    "--branches",
                    "--tags",
                    "--remotes",
                    "--date-order",
                    "--topo-order",
                    &skip_argument,
                    &max_count_argument,
                    "--format=%H%x00%P%x00%an%x00%at%x00%s%x00%D%x1e",
                ],
            ))
            .await?;
        if result.exit_code != Some(0) {
            let stderr = result.stderr.to_lowercase();
            let empty_repository = [
                "does not have any commits yet",
                "your current branch",
                "bad default revision",
                "ambiguous argument 'head'",
            ]
            .iter()
            .any(|marker| stderr.contains(marker));
            if empty_repository {
                return Ok(json!({"commits": [], "hasMore": false}));
            }
            ensure_git_command_succeeded("git log visible refs", &result, &[0])?;
        }
        let parsed = parse_graph_records(&result.stdout);
        let has_more = (parsed.len() as i64) > normalized_max_count;
        let commits: Vec<Value> = parsed
            .into_iter()
            .take(normalized_max_count as usize)
            .collect();
        Ok(json!({"commits": commits, "hasMore": has_more}))
    }

    /// `normalizeInputPath` (`gitCliHelpers.ts:683`).
    pub async fn normalize_input_path(
        &self,
        resolution: &ResolvedRepository,
        path: &str,
    ) -> Result<String, ServiceError> {
        let raw_absolute = absolute_from_workspace(&resolution.workspace_path, path);
        // `fs.realpath` failures fall back to the raw path in TS; symlinks therefore only
        // change the answer when they actually resolve.
        let absolute = tokio::fs::canonicalize(&raw_absolute)
            .await
            .map(|path| strip_windows_verbatim(&path.to_string_lossy()))
            .unwrap_or(raw_absolute);
        let repo_relative = normalize_git_path(&relative_path(&resolution.repo_root, &absolute));
        if repo_relative.is_empty()
            || repo_relative == "."
            || repo_relative == ".."
            || repo_relative.starts_with("../")
        {
            return Err(ServiceError::message(format!(
                "Path is outside repository scope: {path}"
            )));
        }
        Ok(repo_relative)
    }

    /// `getDiff` (`gitCliRepo.ts:1252`).
    pub async fn get_diff(&self, params: &Value) -> Result<Value, ServiceError> {
        let workspace_path = required_str(params, "workspacePath")?;
        let path = required_str(params, "path")?;
        let resolution = self.resolve_repository(&workspace_path).await;
        let absolute_path = absolute_from_workspace(&workspace_path, &path);
        if !resolution.is_git_available {
            return Ok(unavailable_diff(
                &absolute_path,
                "Git binary is not available in the current environment.",
            ));
        }
        if !resolution.is_repository {
            return Ok(unavailable_diff(
                &absolute_path,
                "Workspace is not inside a Git repository.",
            ));
        }
        let repo_root = resolution.root().to_path_buf();
        let repo_relative_path = self.normalize_input_path(&resolution, &path).await?;
        let source_id = params
            .get("sourceId")
            .and_then(Value::as_str)
            .map(str::to_string);

        if source_id.as_deref() == Some("branch") {
            let status = self.get_status(&workspace_path).await?;
            let Some(tracking_branch_name) = status.summary["trackingBranchName"]
                .as_str()
                .map(str::to_string)
            else {
                return Ok(unavailable_diff(
                    &absolute_path,
                    "Current branch does not have an upstream branch.",
                ));
            };
            let range = format!("{tracking_branch_name}...HEAD");
            let result = self
                .provider
                .run(
                    GitCommand::new(
                        &repo_root,
                        &[
                            "diff",
                            "--no-ext-diff",
                            "--no-color",
                            "--binary",
                            "--find-renames",
                            &range,
                            "--",
                            &repo_relative_path,
                        ],
                    )
                    .timeout(DIFF_TIMEOUT)
                    .max_output_bytes(DIFF_BYTES),
                )
                .await?;
            let parsed = to_diff_result(
                &absolute_path,
                &result,
                &[0],
                "No branch comparison diff is available for this file.",
                "Binary diff is not previewable.",
            );
            // The comparison base is the merge base when Git can find one
            // (`gitCliRepo.ts:227-247`).
            let merge_base = self
                .provider
                .run(GitCommand::new(
                    &repo_root,
                    &["merge-base", &tracking_branch_name, "HEAD"],
                ))
                .await?;
            let base_ref = if merge_base.exit_code == Some(0) {
                let trimmed = merge_base.stdout.trim().to_string();
                if trimmed.is_empty() {
                    tracking_branch_name.clone()
                } else {
                    trimmed
                }
            } else {
                tracking_branch_name.clone()
            };
            let before = read_git_blob_preview_content(
                &self.provider,
                &repo_root,
                &base_ref,
                &repo_relative_path,
            )
            .await;
            let after = read_git_blob_preview_content(
                &self.provider,
                &repo_root,
                "HEAD",
                &repo_relative_path,
            )
            .await;
            return Ok(with_diff_contents(
                parsed,
                to_complete_diff_contents(before, after),
            ));
        }

        let staged = match params.get("staged").and_then(Value::as_bool) {
            Some(value) => value,
            None => source_id.as_deref() == Some("staged"),
        };
        let mut args: Vec<&str> = vec!["diff", "--no-ext-diff", "--no-color", "--binary"];
        if staged {
            args.insert(1, "--cached");
        }
        args.extend(["--", &repo_relative_path]);
        let result = self
            .provider
            .run(
                GitCommand::new(&repo_root, &args)
                    .timeout(DIFF_TIMEOUT)
                    .max_output_bytes(DIFF_BYTES),
            )
            .await?;
        let parsed = to_diff_result(
            &absolute_path,
            &result,
            &[0],
            "No Git diff is available for this file.",
            "Binary diff is not previewable.",
        );
        if parsed["availability"] != json!("unavailable") || staged {
            let contents = if staged {
                let before = read_git_blob_preview_content(
                    &self.provider,
                    &repo_root,
                    "HEAD",
                    &repo_relative_path,
                )
                .await;
                // An empty ref means “the index”, which is where the staged side lives.
                let after = read_git_blob_preview_content(
                    &self.provider,
                    &repo_root,
                    "",
                    &repo_relative_path,
                )
                .await;
                to_complete_diff_contents(before, after)
            } else {
                let before = read_git_blob_preview_content(
                    &self.provider,
                    &repo_root,
                    "",
                    &repo_relative_path,
                )
                .await;
                let after = read_working_tree_preview_content(Path::new(&absolute_path)).await;
                to_complete_diff_contents(before, after)
            };
            return Ok(with_diff_contents(parsed, contents));
        }

        // Untracked files never appear in `git diff`, so a single-file patch is built from
        // the file itself; `--no-index` is only the binary/unreadable fallback
        // (`gitCliRepo.ts:1336-1368`).
        if !file_exists(Path::new(&absolute_path)).await {
            return Ok(parsed);
        }
        if let Some(diff) = build_untracked_text_diff_result(
            Path::new(&absolute_path),
            &repo_relative_path,
            DIFF_BYTES,
        )
        .await
        {
            return Ok(diff);
        }
        let no_index = self
            .provider
            .run(
                GitCommand::new(
                    &repo_root,
                    &[
                        "diff",
                        "--no-index",
                        "--no-ext-diff",
                        "--no-color",
                        "--binary",
                        null_device_path(),
                        &absolute_path,
                    ],
                )
                .timeout(DIFF_TIMEOUT)
                .max_output_bytes(DIFF_BYTES),
            )
            .await?;
        Ok(to_diff_result(
            &absolute_path,
            &no_index,
            &[0, 1],
            "No previewable diff is available for this file.",
            "Binary diff is not previewable.",
        ))
    }

    /// `getBranchComparison` (`gitCliRepo.ts:1371`).
    pub async fn get_branch_comparison(
        &self,
        workspace_path: &str,
    ) -> Result<BranchComparisonSnapshot, ServiceError> {
        let status = self.get_status(workspace_path).await?;
        let tracking_branch_name = status.summary["trackingBranchName"]
            .as_str()
            .map(str::to_string);
        let head_ref = status.summary["branchName"]
            .as_str()
            .map(str::to_string)
            .or_else(|| Some("HEAD".to_string()));
        if !status.resolution.is_git_available
            || !status.resolution.is_repository
            || tracking_branch_name.is_none()
        {
            return Ok(BranchComparisonSnapshot {
                base_ref: tracking_branch_name,
                head_ref,
                comparison_label: None,
                changes: Vec::new(),
                resolution: status.resolution,
            });
        }
        let tracking_branch_name = tracking_branch_name.expect("checked above");
        let range = format!("{tracking_branch_name}...HEAD");
        let result = self
            .provider
            .run(GitCommand::new(
                status.resolution.root(),
                &["diff", "--numstat", "-z", "--find-renames", &range, "--"],
            ))
            .await?;
        ensure_git_command_succeeded("git diff --numstat upstream...HEAD", &result, &[0])?;
        let stats = parse_numstat(&result.stdout);
        // `Array.from(map.entries())` — Git's numstat order is the response order.
        let changes = stats
            .entries()
            .iter()
            .map(|(path, stat)| ComparisonChange {
                path: path.clone(),
                original_path: stat.original_path.clone(),
                kind: infer_kind_from_numstat(stat),
                added: stat.added,
                removed: stat.removed,
            })
            .collect();
        let comparison_label = match status.summary["branchName"].as_str() {
            Some(branch_name) => Some(format!("{branch_name} -> {tracking_branch_name}")),
            None => Some(format!("HEAD -> {tracking_branch_name}")),
        };
        Ok(BranchComparisonSnapshot {
            resolution: status.resolution,
            base_ref: Some(tracking_branch_name),
            head_ref,
            comparison_label,
            changes,
        })
    }

    /// `getIdentity` (`gitCliRepo.ts:1675`).
    pub async fn get_identity(&self, workspace_path: &str) -> Result<Value, ServiceError> {
        let resolution = self.resolve_repository(workspace_path).await;
        if !resolution.is_git_available || !resolution.is_repository {
            return Ok(json!({
                "userName": Value::Null,
                "userEmail": Value::Null,
                "nameSource": Value::Null,
                "emailSource": Value::Null,
                "scopeLabel": Value::Null,
            }));
        }
        let repo_root = resolution.root().to_path_buf();
        let name_result = self
            .provider
            .run(GitCommand::new(
                &repo_root,
                &[
                    "config",
                    "--show-scope",
                    "--show-origin",
                    "--get",
                    "user.name",
                ],
            ))
            .await?;
        let email_result = self
            .provider
            .run(GitCommand::new(
                &repo_root,
                &[
                    "config",
                    "--show-scope",
                    "--show-origin",
                    "--get",
                    "user.email",
                ],
            ))
            .await?;
        let (name_scope, name_source, name_value) = parse_git_config_value(&name_result)?;
        let (email_scope, email_source, email_value) = parse_git_config_value(&email_result)?;
        Ok(json!({
            "userName": name_value,
            "userEmail": email_value,
            "nameSource": name_source,
            "emailSource": email_source,
            "scopeLabel": name_scope.or(email_scope),
        }))
    }

    /// `hasOperationInProgress` (`gitCliRepo.ts:616`): probe the git-dir markers first so
    /// the UI gets a stable blocking reason instead of a version-dependent Git message.
    pub(crate) async fn has_operation_in_progress(
        &self,
        resolution: &ResolvedRepository,
    ) -> Result<bool, ServiceError> {
        let mut argv: Vec<&str> = vec!["rev-parse"];
        for marker in GIT_OPERATION_MARKERS {
            argv.push("--git-path");
            argv.push(marker);
        }
        let result = self
            .provider
            .run(GitCommand::new(resolution.root(), &argv))
            .await?;
        ensure_git_command_succeeded("git rev-parse --git-path", &result, &[0])?;
        for line in result.stdout.replace("\r\n", "\n").lines() {
            let candidate = line.trim();
            if candidate.is_empty() {
                continue;
            }
            let absolute = if is_absolute(candidate) {
                candidate.to_string()
            } else {
                resolve_path(&resolution.repo_root, &[candidate])
            };
            if file_exists(Path::new(&absolute)).await {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `readOptionalGitConfig` (`gitCliRepo.ts:637`).
    pub(crate) async fn read_optional_git_config(
        &self,
        resolution: &ResolvedRepository,
        key: &str,
    ) -> Result<Option<String>, ServiceError> {
        let result = self
            .provider
            .run(GitCommand::new(
                resolution.root(),
                &["config", "--get", key],
            ))
            .await?;
        if result.timed_out || result.output_truncated {
            ensure_git_command_succeeded(&format!("git config --get {key}"), &result, &[0])?;
        }
        if result.exit_code == Some(0) {
            let value = result.stdout.trim();
            return Ok((!value.is_empty()).then(|| value.to_string()));
        }
        if result.exit_code == Some(1) {
            return Ok(None);
        }
        ensure_git_command_succeeded(&format!("git config --get {key}"), &result, &[0])?;
        Ok(None)
    }

    /// `listRemotes` (`gitCliRepo.ts:663`).
    pub(crate) async fn list_remotes(
        &self,
        resolution: &ResolvedRepository,
    ) -> Result<Vec<String>, ServiceError> {
        let result = self
            .provider
            .run(GitCommand::new(resolution.root(), &["remote"]))
            .await?;
        ensure_git_command_succeeded("git remote", &result, &[0])?;
        Ok(result
            .stdout
            .replace("\r\n", "\n")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }
}

/// `isAbsolute(path) ? path : resolve(workspacePath, path.split("/").join(sep))`, the shape
/// used by `getDiff`, `getIgnoredPaths` and `normalizeInputPath`.
pub fn absolute_from_workspace(workspace_path: &str, path: &str) -> String {
    if is_absolute(path) {
        return path.to_string();
    }
    resolve_path(workspace_path, &path.split('/').collect::<Vec<_>>())
}

pub(crate) fn required_str(params: &Value, key: &str) -> Result<String, ServiceError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ServiceError::invalid_params(format!("params.{key} is required")))
}
